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
    /// Hub node-facing address, host:port (e.g. 127.0.0.1:7000).
    #[arg(long)]
    pub hub: String,
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

pub async fn run_agent(args: RunArgs) -> Result<()> {
    let name = if args.name.is_empty() {
        hostname()
    } else {
        args.name.clone()
    };
    let connector = if args.tls {
        Some(tls::client_connector(
            args.hub_fingerprint.clone(),
            args.insecure,
        )?)
    } else {
        None
    };
    let key_path = args.key_file.clone().unwrap_or_else(default_key_file);
    let identity =
        Identity::load_or_create(&key_path).with_context(|| format!("node key {key_path}"))?;
    tracing::info!(code = %crate::identity::short_code(&identity.pubkey()), key = %key_path, "node identity ready");
    loop {
        match connect_once(&args, &name, &connector, &identity).await {
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
    args: &RunArgs,
    name: &str,
    connector: &Option<TlsConnector>,
    identity: &Identity,
) -> Result<()> {
    let mut conn = dial_conn(&args.hub, connector).await?;

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let hello = Hello {
        protocol_version: PROTOCOL_VERSION,
        pubkey: identity.pubkey(),
        token: args.token.clone(),
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
            tracing::info!(node = %node_id.0, hub = %args.hub, "enrolled")
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

    let hub_addr = args.hub.clone();
    let connector = connector.clone();
    let res: Result<()> = async {
        loop {
            let msg: HubToNode = read_msg(&mut rd).await?;
            match msg {
                HubToNode::Ping { nonce } => {
                    let _ = ntx.send(NodeToHub::Pong { nonce });
                }
                HubToNode::Drain { reason } => {
                    tracing::info!(%reason, "hub asked node to drain");
                    break;
                }
                HubToNode::Dial {
                    conn_id,
                    host,
                    port,
                } => {
                    let addr = hub_addr.clone();
                    let conn_tor = connector.clone();
                    let ntx2 = ntx.clone();
                    tokio::spawn(async move {
                        handle_dial(addr, conn_tor, conn_id, host, port, ntx2).await;
                    });
                }
            }
        }
        Ok(())
    }
    .await;

    writer.abort();
    res
}

async fn handle_dial(
    hub_addr: String,
    connector: Option<TlsConnector>,
    conn_id: u64,
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
    if let Err(e) = write_msg(&mut data, &Greeting::Data(DataHello { conn_id })).await {
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

/// Build the argv for `warren node run ...` from the install args.
fn node_run_argv(exe: &str, a: &RunArgs) -> Vec<String> {
    let mut v = vec![
        exe.to_string(),
        "node".into(),
        "run".into(),
        "--hub".into(),
        a.hub.clone(),
    ];
    if let Some(token) = &a.token {
        v.push("--token".into());
        v.push(token.clone());
    }
    if let Some(kf) = &a.key_file {
        v.push("--key-file".into());
        v.push(kf.clone());
    }
    if !a.name.is_empty() {
        v.push("--name".into());
        v.push(a.name.clone());
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
    Ok(())
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
