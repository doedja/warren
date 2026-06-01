//! Node mode: the outbound-only agent that runs on each device.
//!
//! Opens one control connection to the hub (Hello + token), then serves Dial
//! requests: for each, dial the target from THIS machine (residential egress),
//! open a fresh data connection to the hub tagged with the conn_id, and splice
//! the target to that data connection. Reconnects on drop.
//!
//! With `--tls` the node link is TLS; the hub cert is pinned by
//! `--hub-fingerprint` (or accepted blindly with `--insecure`, dev only).

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::{Args, Subcommand};
use tokio::io::{copy_bidirectional, split};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::TlsConnector;

use warren_proto::{
    DataHello, Greeting, Hello, HelloReply, HubToNode, NodeToHub, Platform, PROTOCOL_VERSION,
};

use crate::conn::Conn;
use crate::identity::Identity;
use crate::tls;
use crate::wire::{read_msg, write_msg};

#[derive(Args, Debug)]
pub struct NodeArgs {
    #[command(subcommand)]
    pub action: NodeAction,
}

#[derive(Subcommand, Debug)]
pub enum NodeAction {
    /// Connect to the hub and serve as a proxy node.
    Run(RunArgs),
    /// Install warren as a boot service (systemd / launchd / Windows task).
    Install(RunArgs),
    /// Remove the boot service and leave the pool.
    Uninstall,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    /// One-paste join code from the hub/dashboard. Fills in --hub, --token,
    /// --tls, and --hub-fingerprint, so you do not pass them separately.
    #[arg(long)]
    pub join: Option<String>,
    /// Hub node-facing address, host:port (e.g. 127.0.0.1:7000). Not needed with --join.
    #[arg(long)]
    pub hub: Option<String>,
    /// Enrollment token (Mode B). Omit to request admin approval (Mode A).
    #[arg(long)]
    pub token: Option<String>,
    /// File holding this node's ed25519 key (created on first run).
    #[arg(long)]
    pub key_file: Option<String>,
    /// Name this node shows up as in the hub.
    #[arg(long, default_value = "")]
    pub name: String,
    /// Use TLS to the hub.
    #[arg(long, default_value_t = false)]
    pub tls: bool,
    /// Pinned hub cert SHA256 (hex), required with --tls unless --insecure.
    #[arg(long)]
    pub hub_fingerprint: Option<String>,
    /// Accept any hub cert (dev only).
    #[arg(long, default_value_t = false)]
    pub insecure: bool,
}

pub async fn run(args: NodeArgs) -> Result<()> {
    match args.action {
        NodeAction::Run(r) => run_agent(r).await,
        NodeAction::Install(r) => install_service(r).await,
        NodeAction::Uninstall => uninstall_service().await,
    }
}

/// The connection settings after merging --join (if any) with the flags.
struct Resolved {
    hub: String,
    token: Option<String>,
    tls: bool,
    fingerprint: Option<String>,
    insecure: bool,
}

/// A --join code provides hub/token/tls/fingerprint in one string; otherwise
/// fall back to the individual flags. Either --join or --hub is required.
fn resolve(args: &RunArgs) -> Result<Resolved> {
    if let Some(code) = &args.join {
        let j = crate::joincode::decode(code)?;
        Ok(Resolved {
            hub: j.hub,
            token: j.token.or_else(|| args.token.clone()),
            tls: j.tls,
            fingerprint: j.fingerprint.or_else(|| args.hub_fingerprint.clone()),
            insecure: args.insecure,
        })
    } else {
        let hub = args
            .hub
            .clone()
            .ok_or_else(|| anyhow!("need --join <code> or --hub <host:port>"))?;
        Ok(Resolved {
            hub,
            token: args.token.clone(),
            tls: args.tls,
            fingerprint: args.hub_fingerprint.clone(),
            insecure: args.insecure,
        })
    }
}

pub async fn run_agent(args: RunArgs) -> Result<()> {
    let r = resolve(&args)?;
    let name = if args.name.is_empty() {
        hostname()
    } else {
        args.name.clone()
    };
    let connector = if r.tls {
        Some(tls::client_connector(r.fingerprint.clone(), r.insecure)?)
    } else {
        None
    };
    let key_path = args.key_file.clone().unwrap_or_else(default_key_file);
    let identity =
        Identity::load_or_create(&key_path).with_context(|| format!("node key {key_path}"))?;
    tracing::info!(code = %crate::identity::short_code(&identity.pubkey()), key = %key_path, "node identity ready");
    loop {
        match connect_once(&r, &name, &connector, &identity).await {
            Ok(()) => tracing::warn!("control connection closed; reconnecting in 3s"),
            Err(e) => tracing::warn!(error = %e, "control connection error; reconnecting in 3s"),
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

fn default_key_file() -> String {
    match std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        Ok(home) => format!("{home}/.warren/node.key"),
        Err(_) => "warren-node.key".to_string(),
    }
}

/// TCP-connect to `addr`, wrapping in TLS if a connector is configured.
async fn dial_conn(addr: &str, connector: &Option<TlsConnector>) -> Result<Conn> {
    let tcp = TcpStream::connect(addr)
        .await
        .with_context(|| format!("connect {addr}"))?;
    match connector {
        Some(c) => {
            // "warren" is a 'static str, so this ServerName is 'static. The
            // pinned/insecure verifier ignores the name anyway.
            let domain = ServerName::try_from("warren").map_err(|_| anyhow!("bad servername"))?;
            Ok(Conn::ClientTls(c.connect(domain, tcp).await?))
        }
        None => Ok(Conn::Plain(tcp)),
    }
}

async fn connect_once(
    r: &Resolved,
    name: &str,
    connector: &Option<TlsConnector>,
    identity: &Identity,
) -> Result<()> {
    let mut conn = dial_conn(&r.hub, connector).await?;

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let hello = Hello {
        protocol_version: PROTOCOL_VERSION,
        pubkey: identity.pubkey(),
        token: r.token.clone(),
        timestamp,
        signature: identity.sign_auth(timestamp),
        node_name: name.to_string(),
        platform: current_platform(),
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    write_msg(&mut conn, &Greeting::Control(hello)).await?;

    let reply: HelloReply = read_msg(&mut conn).await?;
    match reply {
        HelloReply::Welcome { node_id } => {
            tracing::info!(node = %node_id.0, hub = %r.hub, "enrolled")
        }
        HelloReply::Pending { code } => {
            anyhow::bail!("pending admin approval (code {code}); approve it in the hub dashboard")
        }
        HelloReply::Reject { reason } => anyhow::bail!("hub rejected node: {reason}"),
    }

    let (mut rd, mut wr) = split(conn);
    let (ntx, mut nrx) = mpsc::unbounded_channel::<NodeToHub>();

    let writer = tokio::spawn(async move {
        while let Some(msg) = nrx.recv().await {
            if write_msg(&mut wr, &msg).await.is_err() {
                break;
            }
        }
    });

    // Self-report public egress IP + geo to the hub, best-effort, on connect and
    // every 5 minutes. Old hubs ignore an unknown message; this hub shows it.
    let report_tx = ntx.clone();
    let reporter = tokio::spawn(async move {
        loop {
            if let Some((ip, country, city)) = fetch_public_info().await {
                let _ = report_tx.send(NodeToHub::Info {
                    public_ip: Some(ip),
                    country,
                    city,
                });
            }
            tokio::time::sleep(Duration::from_secs(300)).await;
        }
    });

    let hub_addr = r.hub.clone();
    let connector = connector.clone();
    let res: Result<()> = async {
        loop {
            let msg: HubToNode = read_msg(&mut rd).await?;
            match msg {
                HubToNode::Ping { nonce } => {
                    let _ = ntx.send(NodeToHub::Pong { nonce });
                }
                HubToNode::Dial {
                    conn_id,
                    nonce,
                    host,
                    port,
                } => {
                    let addr = hub_addr.clone();
                    let conn_tor = connector.clone();
                    let ntx2 = ntx.clone();
                    tokio::spawn(async move {
                        handle_dial(addr, conn_tor, conn_id, nonce, host, port, ntx2).await;
                    });
                }
            }
        }
    }
    .await;

    writer.abort();
    reporter.abort();
    res
}

/// Best-effort lookup of this node's public egress IP + geo via ip-api.com
/// (plain HTTP, no key, no extra dependency). Returns None on any failure.
async fn fetch_public_info() -> Option<(String, Option<String>, Option<String>)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let connect = TcpStream::connect("ip-api.com:80");
    let mut s = tokio::time::timeout(Duration::from_secs(8), connect)
        .await
        .ok()?
        .ok()?;
    let req = "GET /line/?fields=query,country,city HTTP/1.1\r\nHost: ip-api.com\r\nConnection: close\r\nUser-Agent: warren\r\n\r\n";
    s.write_all(req.as_bytes()).await.ok()?;
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(8), s.read_to_end(&mut buf))
        .await
        .ok()?
        .ok()?;
    let text = String::from_utf8_lossy(&buf);
    let body = text.split("\r\n\r\n").nth(1)?;
    let mut lines = body.lines().map(str::trim).filter(|l| !l.is_empty());
    let ip = lines.next()?.to_string();
    // /line returns "fail" as the first line on error; reject anything that is
    // not IP-shaped.
    if !ip.contains('.') && !ip.contains(':') {
        return None;
    }
    let country = lines.next().map(str::to_string);
    let city = lines.next().map(str::to_string);
    Some((ip, country, city))
}

async fn handle_dial(
    hub_addr: String,
    connector: Option<TlsConnector>,
    conn_id: u64,
    nonce: u64,
    host: String,
    port: u16,
    ntx: mpsc::UnboundedSender<NodeToHub>,
) {
    // Dial the target from this node: this is the residential egress.
    let mut target = match TcpStream::connect((host.as_str(), port)).await {
        Ok(t) => t,
        Err(e) => {
            let _ = ntx.send(NodeToHub::DialFailed {
                conn_id,
                reason: e.to_string(),
            });
            return;
        }
    };

    // Open a fresh data connection back to the hub, tagged with conn_id.
    let mut data = match dial_conn(&hub_addr, &connector).await {
        Ok(d) => d,
        Err(e) => {
            let _ = ntx.send(NodeToHub::DialFailed {
                conn_id,
                reason: format!("data dial: {e}"),
            });
            return;
        }
    };
    if let Err(e) = write_msg(&mut data, &Greeting::Data(DataHello { conn_id, nonce })).await {
        let _ = ntx.send(NodeToHub::DialFailed {
            conn_id,
            reason: format!("data hello: {e}"),
        });
        return;
    }

    let _ = copy_bidirectional(&mut data, &mut target).await;
}

const SYSTEMD_UNIT: &str = "/etc/systemd/system/warren-node.service";
const LAUNCHD_LABEL: &str = "com.warren.node";

/// Build the argv for `warren node run ...` from the install args. A join code
/// is passed through verbatim (it already encodes hub/token/tls/fingerprint);
/// otherwise the individual flags are emitted.
fn node_run_argv(exe: &str, a: &RunArgs) -> Vec<String> {
    let mut v = vec![exe.to_string(), "node".into(), "run".into()];
    if let Some(code) = &a.join {
        v.push("--join".into());
        v.push(code.clone());
    } else if let Some(hub) = &a.hub {
        v.push("--hub".into());
        v.push(hub.clone());
        if let Some(token) = &a.token {
            v.push("--token".into());
            v.push(token.clone());
        }
        if a.tls {
            v.push("--tls".into());
            if let Some(fp) = &a.hub_fingerprint {
                v.push("--hub-fingerprint".into());
                v.push(fp.clone());
            }
            if a.insecure {
                v.push("--insecure".into());
            }
        }
    }
    if let Some(kf) = &a.key_file {
        v.push("--key-file".into());
        v.push(kf.clone());
    }
    if !a.name.is_empty() {
        v.push("--name".into());
        v.push(a.name.clone());
    }
    v
}

fn run_cmd(cmd: &str, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new(cmd)
        .args(args)
        .status()
        .with_context(|| format!("run {cmd}"))?;
    if !status.success() {
        anyhow::bail!("{cmd} {args:?} exited with {status}");
    }
    Ok(())
}

async fn install_service(args: RunArgs) -> Result<()> {
    let exe = std::env::current_exe()?.to_string_lossy().to_string();
    let argv = node_run_argv(&exe, &args);
    match current_platform() {
        Platform::Linux => install_systemd(&argv),
        Platform::MacOs => install_launchd(&argv),
        Platform::Windows => install_windows(&argv),
        _ => {
            println!(
                "Automatic install is not supported on this platform. Run manually:\n  {}",
                argv.join(" ")
            );
            Ok(())
        }
    }
}

/// Windows: a Scheduled Task that runs the node at startup as SYSTEM.
fn install_windows(argv: &[String]) -> Result<()> {
    // /TR takes one string; quote the exe path, append the rest.
    let tr = format!("\"{}\" {}", argv[0], argv[1..].join(" "));
    run_cmd(
        "schtasks",
        &[
            "/Create",
            "/TN",
            "warren-node",
            "/TR",
            tr.as_str(),
            "/SC",
            "ONSTART",
            "/RU",
            "SYSTEM",
            "/RL",
            "HIGHEST",
            "/F",
        ],
    )?;
    let _ = run_cmd("schtasks", &["/Run", "/TN", "warren-node"]);
    println!("installed Windows scheduled task 'warren-node' (runs at startup)");
    Ok(())
}

fn install_systemd(argv: &[String]) -> Result<()> {
    let unit = format!(
        "[Unit]\n\
         Description=warren proxy node\n\
         After=network-online.target\n\
         Wants=network-online.target\n\n\
         [Service]\n\
         ExecStart={}\n\
         Restart=always\n\
         RestartSec=3\n\n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        argv.join(" ")
    );
    std::fs::write(SYSTEMD_UNIT, unit)
        .with_context(|| format!("write {SYSTEMD_UNIT} (need root? re-run with sudo)"))?;
    run_cmd("systemctl", &["daemon-reload"])?;
    run_cmd("systemctl", &["enable", "--now", "warren-node"])?;
    println!("installed and started systemd service: warren-node");
    Ok(())
}

fn install_launchd(argv: &[String]) -> Result<()> {
    let plist_path = launchd_path()?;
    let args_xml: String = argv
        .iter()
        .map(|a| format!("    <string>{a}</string>\n"))
        .collect();
    let plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\"><dict>\n\
         <key>Label</key><string>{LAUNCHD_LABEL}</string>\n\
         <key>ProgramArguments</key><array>\n{args_xml}</array>\n\
         <key>RunAtLoad</key><true/>\n\
         <key>KeepAlive</key><true/>\n\
         </dict></plist>\n"
    );
    if let Some(dir) = std::path::Path::new(&plist_path).parent() {
        std::fs::create_dir_all(dir).ok();
    }
    std::fs::write(&plist_path, plist).with_context(|| format!("write {plist_path}"))?;
    run_cmd("launchctl", &["load", "-w", &plist_path])?;
    println!("installed and loaded launchd agent: {plist_path}");
    Ok(())
}

fn launchd_path() -> Result<String> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(format!("{home}/Library/LaunchAgents/{LAUNCHD_LABEL}.plist"))
}

async fn uninstall_service() -> Result<()> {
    match current_platform() {
        Platform::Linux => {
            let _ = run_cmd("systemctl", &["disable", "--now", "warren-node"]);
            std::fs::remove_file(SYSTEMD_UNIT).ok();
            let _ = run_cmd("systemctl", &["daemon-reload"]);
            println!("removed systemd service: warren-node");
        }
        Platform::MacOs => {
            let plist_path = launchd_path()?;
            let _ = run_cmd("launchctl", &["unload", "-w", &plist_path]);
            std::fs::remove_file(&plist_path).ok();
            println!("removed launchd agent: {plist_path}");
        }
        Platform::Windows => {
            let _ = run_cmd("schtasks", &["/End", "/TN", "warren-node"]);
            run_cmd("schtasks", &["/Delete", "/TN", "warren-node", "/F"])?;
            println!("removed Windows scheduled task: warren-node");
        }
        _ => println!("nothing to uninstall on this platform"),
    }
    remove_node_state();
    Ok(())
}

/// Leave the device clean after the service is gone: delete the node identity
/// (and its dir if now empty), and best-effort remove the installed binary.
fn remove_node_state() {
    let key = default_key_file();
    if std::fs::remove_file(&key).is_ok() {
        println!("removed node key: {key}");
    }
    if let Some(dir) = std::path::Path::new(&key).parent() {
        // Only removes ~/.warren when it is empty; harmless otherwise.
        let _ = std::fs::remove_dir(dir);
    }
    match std::env::current_exe() {
        // A running .exe cannot delete itself on Windows; the PowerShell
        // uninstaller removes the install dir, so just point at it here.
        Ok(exe) if cfg!(windows) => println!("remove the binary to finish: {}", exe.display()),
        // On Unix, unlinking the running binary is fine (the inode lives until
        // the process exits). Announce it before the unlink.
        Ok(exe) => {
            println!("removing binary: {}", exe.display());
            if std::fs::remove_file(&exe).is_err() {
                println!("could not remove {}; delete it to finish", exe.display());
            }
        }
        Err(_) => {}
    }
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "warren-node".to_string())
}

fn current_platform() -> Platform {
    if cfg!(target_os = "linux") {
        Platform::Linux
    } else if cfg!(target_os = "windows") {
        Platform::Windows
    } else if cfg!(target_os = "macos") {
        Platform::MacOs
    } else {
        Platform::Other
    }
}
