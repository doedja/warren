//! Node mode: the outbound-only agent that runs on each device.
//!
//! Dials ONE yamux-multiplexed connection to the hub, opens a control stream
//! (Hello + token), then serves Dial requests: for each, dial the target from
//! THIS machine (residential egress), open a fresh logical stream to the hub
//! tagged with the conn_id, and splice the target to that stream. Opening a
//! stream is cheap (no new TCP/TLS handshake). Reconnects on drop.
//!
//! With `--tls` the node link is TLS; the hub cert is pinned by
//! `--hub-fingerprint` (or accepted blindly with `--insecure`, dev only).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::{Args, Subcommand};
use rand::Rng;
use tokio::io::{copy_bidirectional, split};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{mpsc, watch};
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::TlsConnector;

use warren_proto::{
    DataHello, Greeting, Hello, HelloReply, HubToNode, NodeToHub, Platform, UdpDatagram,
    PROTOCOL_VERSION,
};

use crate::conn::Conn;
use crate::identity::Identity;
use crate::mux;
use crate::tls;
use crate::wire::{read_msg, write_msg};

/// On SIGTERM/SIGINT the node stops taking new dials and waits up to this long
/// for in-flight splices to finish before exiting, so a redeploy does not cut
/// live requests.
const DRAIN_BUDGET: Duration = Duration::from_secs(20);
/// Bound on the whole connect + enroll handshake (TCP/TLS dial, stream open,
/// Hello, HelloReply). Without it, a connection that comes up at the transport
/// layer but never completes enrollment (hub mid-restart, half-open link) wedges
/// connect_once forever and the reconnect loop never runs. On timeout we return
/// an error and fall into the existing backoff/retry, like the steady-state read.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

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
    /// Auto-update: when the hub reports a newer version, re-run the installer to
    /// upgrade this node. Off by default (a notice is logged instead). Unix only;
    /// on Windows the running exe is locked, so reinstall manually.
    #[arg(long, default_value_t = false)]
    pub auto_update: bool,
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
    // On Termux (Android), hold a CPU wake lock so Doze does not suspend the node.
    // Best-effort and self-contained, so a plain `node run` works even without the
    // install script. Released on clean exit below.
    termux_wakelock(true);
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

    // Drain on shutdown: a watch channel flips to true on SIGTERM/SIGINT; the
    // control read loop breaks on it, the reconnect loop exits instead of
    // dialing again, and in-flight splices get DRAIN_BUDGET to finish.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let in_flight = Arc::new(AtomicU32::new(0));
    // On unix the signal task owns the sender. On non-unix there is no SIGTERM,
    // so keep the sender alive here (dropping it would make rx.changed() error
    // out immediately and spin the read loop).
    #[cfg(unix)]
    spawn_signal_handler(shutdown_tx);
    #[cfg(not(unix))]
    let _shutdown_tx_keepalive = shutdown_tx;

    let mut delay_ms: u64 = 0;
    loop {
        let res = connect_once(
            &r,
            &name,
            &connector,
            &identity,
            shutdown_rx.clone(),
            in_flight.clone(),
            args.auto_update,
        )
        .await;
        if *shutdown_rx.borrow() {
            break;
        }
        match res {
            Ok(()) => {
                tracing::warn!("control connection closed; reconnecting immediately");
                delay_ms = 0;
            }
            Err(e) => {
                let sleep = backoff_sleep(&mut delay_ms);
                tracing::warn!(error = %e, backoff_ms = sleep.as_millis() as u64, "control connection error; reconnecting");
                tokio::time::sleep(sleep).await;
            }
        }
    }

    // connect_once drains in-flight splices on shutdown (while its yamux driver
    // is still alive), so by the time we break out there is nothing left to wait
    // for here.
    termux_wakelock(false); // release on clean exit so we do not pin the CPU.
    Ok(())
}

/// Acquire (or release) a Termux CPU wake lock so Android Doze does not suspend
/// the node. No-op off Termux (gated on TERMUX_VERSION) and best-effort: needs
/// the termux-api package + the Termux:API app, but failure is harmless.
fn termux_wakelock(acquire: bool) {
    if std::env::var_os("TERMUX_VERSION").is_none() {
        return;
    }
    let cmd = if acquire {
        "termux-wake-lock"
    } else {
        "termux-wake-unlock"
    };
    let _ = std::process::Command::new(cmd).spawn();
}

/// Wait up to DRAIN_BUDGET for in-flight dials to finish, then return so the
/// process can exit. Logs how many were still running if the budget elapses.
async fn drain_in_flight(in_flight: &AtomicU32) {
    let n = in_flight.load(Ordering::Relaxed);
    if n == 0 {
        return;
    }
    tracing::info!(in_flight = n, "draining in-flight requests before exit");
    let deadline = tokio::time::Instant::now() + DRAIN_BUDGET;
    while in_flight.load(Ordering::Relaxed) > 0 {
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                remaining = in_flight.load(Ordering::Relaxed),
                "drain budget elapsed; exiting with requests still in flight"
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tracing::info!("drain complete; exiting");
}

/// True if dotted version `remote` is numerically greater than `local`.
fn version_gt(remote: &str, local: &str) -> bool {
    let parse = |s: &str| {
        s.split('.')
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect::<Vec<u64>>()
    };
    parse(remote) > parse(local)
}

/// On a newer hub version, log it and (with --auto-update, unix only) re-run the
/// installer detached: it stops this service, swaps the binary, and restarts it.
/// Default is notice-only, because a silent binary swap across a fleet is
/// dangerous; enable per node once confirmed. Version tolerance means an
/// unupdated node keeps serving meanwhile.
fn maybe_self_update(remote: &str, auto_update: bool, r: &Resolved) {
    let local = env!("CARGO_PKG_VERSION");
    if !version_gt(remote, local) {
        return;
    }
    tracing::warn!(node = local, hub = remote, "hub is newer than this node");
    if !auto_update {
        tracing::warn!("auto-update off (pass --auto-update to enable); reinstall to upgrade");
        return;
    }
    if cfg!(windows) {
        tracing::warn!("auto-update unsupported on Windows (locked exe); reinstall manually");
        return;
    }
    let url = "https://raw.githubusercontent.com/doedja/warren/main/install.sh";
    let mut cmd = format!("curl -fsSL {url} | sh -s -- --hub {}", r.hub);
    if let Some(t) = &r.token {
        cmd.push_str(&format!(" --token {t}"));
    }
    if r.tls {
        cmd.push_str(" --tls");
        if let Some(fp) = &r.fingerprint {
            cmd.push_str(&format!(" --hub-fingerprint {fp}"));
        }
    }
    if r.insecure {
        cmd.push_str(" --insecure");
    }
    tracing::warn!("auto-updating: re-running installer (service will restart)");
    let _ = std::process::Command::new("sh").arg("-c").arg(&cmd).spawn();
}

/// Flip the shutdown watch on the first SIGTERM/SIGINT. On non-unix there is no
/// SIGTERM, so drain is skipped (the process is killed directly).
#[cfg(unix)]
fn spawn_signal_handler(tx: watch::Sender<bool>) {
    tokio::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => return,
        };
        let mut intr = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(_) => return,
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = intr.recv() => {}
        }
        tracing::info!("shutdown signal received; starting graceful drain");
        let _ = tx.send(true);
    });
}

fn backoff_sleep(delay_ms: &mut u64) -> Duration {
    const BASE: u64 = 1_000;
    const CAP: u64 = 30_000;
    if *delay_ms == 0 {
        *delay_ms = BASE;
        return Duration::ZERO;
    }
    let wait = (*delay_ms).min(CAP);
    let j = rand::thread_rng().gen_range(0..=(wait / 4));
    *delay_ms = (*delay_ms * 2).min(CAP);
    Duration::from_millis(wait + j)
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
    let _ = tcp.set_nodelay(true); // proxied splice: no Nagle stalls
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
    mut shutdown_rx: watch::Receiver<bool>,
    in_flight: Arc<AtomicU32>,
    auto_update: bool,
) -> Result<()> {
    // Whole handshake is bounded by CONNECT_TIMEOUT so a transport-up but
    // enroll-stalled hub cannot wedge us forever; a timeout returns Err and the
    // caller (run_agent) backs off and retries.
    let conn = tokio::time::timeout(CONNECT_TIMEOUT, dial_conn(&r.hub, connector))
        .await
        .map_err(|_| anyhow!("connect to hub timed out"))??;
    // One yamux connection carries everything. The node opens streams: the
    // control stream first, then one per Dial. The driver task owns the yamux
    // Connection and serves these opens; its handle aborts the driver on drop,
    // so every return path below tears the connection down without a manual call.
    let (opener, _driver) = mux::client(conn);

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
    // Open the control stream + exchange Hello/HelloReply under the same bound.
    let (ctrl, reply) = tokio::time::timeout(CONNECT_TIMEOUT, async {
        let mut ctrl = mux::open(&opener).await?;
        write_msg(&mut ctrl, &Greeting::Control(hello)).await?;
        let reply: HelloReply = read_msg(&mut ctrl).await?;
        Ok::<_, anyhow::Error>((ctrl, reply))
    })
    .await
    .map_err(|_| anyhow!("hub enrollment handshake timed out"))??;
    match reply {
        HelloReply::Welcome { node_id } => {
            tracing::info!(node = %node_id.0, hub = %r.hub, "enrolled")
        }
        HelloReply::Pending { code } => {
            anyhow::bail!("pending admin approval (code {code}); approve it in the hub dashboard")
        }
        HelloReply::Reject { reason } => {
            anyhow::bail!("hub rejected node: {reason}")
        }
    }

    let (mut rd, mut wr) = split(ctrl);
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
            if let Some((ip, country, city, ms)) = fetch_public_info().await {
                let _ = report_tx.send(NodeToHub::Info {
                    public_ip: Some(ip),
                    country,
                    city,
                });
                let _ = report_tx.send(NodeToHub::Latency { ms });
            }
            tokio::time::sleep(Duration::from_secs(300)).await;
        }
    });

    let res: Result<()> = (async {
        'read: loop {
            // Already draining: stop taking new dials, let the loop exit cleanly.
            if *shutdown_rx.borrow() {
                break 'read Ok(());
            }
            tokio::select! {
                _ = shutdown_rx.changed() => break 'read Ok(()),
                read = tokio::time::timeout(mux::LINK_READ_TIMEOUT, read_msg(&mut rd)) => match read {
                    Ok(Ok(msg)) => match msg {
                        HubToNode::Ping { nonce } => {
                            let _ = ntx.send(NodeToHub::Pong { nonce });
                        }
                        HubToNode::Dial {
                            conn_id,
                            nonce,
                            host,
                            port,
                        } => {
                            let opener = opener.clone();
                            let ntx2 = ntx.clone();
                            // RAII guard so the count is decremented even if the
                            // dial task panics; drain accounting relies on it.
                            let guard = mux::InFlightGuard::new(in_flight.clone());
                            tokio::spawn(async move {
                                let _g = guard;
                                handle_dial(opener, conn_id, nonce, host, port, ntx2).await;
                            });
                        }
                        HubToNode::UdpOpen { conn_id, nonce } => {
                            let opener = opener.clone();
                            let ntx2 = ntx.clone();
                            let guard = mux::InFlightGuard::new(in_flight.clone());
                            tokio::spawn(async move {
                                let _g = guard;
                                handle_udp(opener, conn_id, nonce, ntx2).await;
                            });
                        }
                        HubToNode::HubVersion { version } => {
                            maybe_self_update(&version, auto_update, r);
                        }
                    },
                    Ok(Err(e)) => break 'read Err(e.into()),
                    Err(_) => break 'read Err(anyhow!("node<->hub read timed out (dead hub)")),
                },
            }
        }
    })
    .await;

    // Graceful drain: if we are shutting down, in-flight splices still ride the
    // yamux driver, so let them finish (up to the budget) BEFORE aborting it.
    // The read loop already exited, so no new dials are accepted meanwhile.
    if *shutdown_rx.borrow() {
        drain_in_flight(&in_flight).await;
    }

    writer.abort();
    reporter.abort();
    // _driver's DriverHandle aborts the yamux driver when it drops here (after
    // the drain above, so in-flight splices were not cut short on shutdown).
    res
}

/// Best-effort lookup of this node's public egress IP + geo via ip-api.com
/// (plain HTTP, no key, no extra dependency). Also times the round trip as a
/// rough latency signal. Returns (ip, country, city, latency_ms) or None.
async fn fetch_public_info() -> Option<(String, Option<String>, Option<String>, u32)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let start = std::time::Instant::now();
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
    let latency_ms = start.elapsed().as_millis().min(u128::from(u32::MAX)) as u32;
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
    Some((ip, country, city, latency_ms))
}

#[allow(clippy::too_many_arguments)]
async fn handle_dial(
    opener: mux::Opener,
    conn_id: u64,
    nonce: u64,
    host: String,
    port: u16,
    ntx: mpsc::UnboundedSender<NodeToHub>,
) {
    // Dial the target from this node: this is the residential egress.
    let mut target = match TcpStream::connect((host.as_str(), port)).await {
        Ok(t) => {
            let _ = t.set_nodelay(true); // proxied splice: no Nagle stalls
            t
        }
        Err(e) => {
            let _ = ntx.send(NodeToHub::DialFailed {
                conn_id,
                reason: e.to_string(),
            });
            return;
        }
    };

    // Open a fresh logical stream back to the hub, tagged with conn_id+nonce. No
    // TCP/TLS handshake: it is multiplexed over the existing connection.
    let mut data = match mux::open(&opener).await {
        Ok(s) => s,
        Err(e) => {
            let _ = ntx.send(NodeToHub::DialFailed {
                conn_id,
                reason: format!("open data stream: {e}"),
            });
            return;
        }
    };
    let dh = Greeting::Data(DataHello { conn_id, nonce });
    if let Err(e) = write_msg(&mut data, &dh).await {
        let _ = ntx.send(NodeToHub::DialFailed {
            conn_id,
            reason: format!("data hello: {e}"),
        });
        return;
    }

    let _ = copy_bidirectional(&mut data, &mut target).await;
}

/// Serve a UDP relay for a SOCKS5 UDP ASSOCIATE: open the relay stream, then
/// bridge `UdpDatagram` frames to/from a residential UDP egress socket. One
/// socket carries every target the client talks to on this association; replies
/// are framed back tagged with the responder's address. Ends when the stream or
/// the egress socket closes.
async fn handle_udp(
    opener: mux::Opener,
    conn_id: u64,
    nonce: u64,
    ntx: mpsc::UnboundedSender<NodeToHub>,
) {
    let mut stream = match mux::open(&opener).await {
        Ok(s) => s,
        Err(e) => {
            let _ = ntx.send(NodeToHub::DialFailed {
                conn_id,
                reason: format!("open udp stream: {e}"),
            });
            return;
        }
    };
    if let Err(e) = write_msg(&mut stream, &Greeting::Data(DataHello { conn_id, nonce })).await {
        let _ = ntx.send(NodeToHub::DialFailed {
            conn_id,
            reason: format!("udp hello: {e}"),
        });
        return;
    }
    // Egress sockets: IPv4 always, IPv6 best-effort. Targets are routed to the
    // socket matching their resolved address family, so v6-only destinations
    // (QUIC/DNS over IPv6) work too, not just v4.
    let udp4 = match UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)).await {
        Ok(u) => Arc::new(u),
        Err(e) => {
            let _ = ntx.send(NodeToHub::DialFailed {
                conn_id,
                reason: format!("udp bind: {e}"),
            });
            return;
        }
    };
    let udp6 = UdpSocket::bind((std::net::Ipv6Addr::UNSPECIFIED, 0))
        .await
        .ok()
        .map(Arc::new);

    let (mut srd, mut swr) = split(stream);

    // Both egress sockets funnel replies through one channel to a single writer,
    // so the stream's write half is never touched from two tasks at once.
    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<UdpDatagram>();
    let writer = tokio::spawn(async move {
        while let Some(dg) = reply_rx.recv().await {
            if write_msg(&mut swr, &dg).await.is_err() {
                break;
            }
        }
    });
    let r4 = spawn_udp_recv(udp4.clone(), reply_tx.clone());
    let r6 = udp6.clone().map(|s| spawn_udp_recv(s, reply_tx.clone()));
    drop(reply_tx); // writer ends once both recv tasks (the only senders) stop.

    // Client datagrams from the hub -> the target, out the family-matched socket.
    while let Ok(dg) = read_msg::<_, UdpDatagram>(&mut srd).await {
        if let Ok(mut addrs) = tokio::net::lookup_host((dg.host.as_str(), dg.port)).await {
            match addrs.next() {
                Some(a @ SocketAddr::V4(_)) => {
                    let _ = udp4.send_to(&dg.data, a).await;
                }
                Some(a @ SocketAddr::V6(_)) => {
                    if let Some(s6) = &udp6 {
                        let _ = s6.send_to(&dg.data, a).await;
                    }
                }
                None => {}
            }
        }
    }
    r4.abort();
    if let Some(r6) = r6 {
        r6.abort();
    }
    writer.abort();
}

/// Spawn a task that reads datagrams off `sock` and forwards each to `tx` tagged
/// with the responder's address (the node->hub reply direction of a UDP relay).
fn spawn_udp_recv(
    sock: Arc<UdpSocket>,
    tx: mpsc::UnboundedSender<UdpDatagram>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65535];
        while let Ok((n, src)) = sock.recv_from(&mut buf).await {
            let dg = UdpDatagram {
                host: src.ip().to_string(),
                port: src.port(),
                data: buf[..n].to_vec(),
            };
            if tx.send(dg).is_err() {
                break;
            }
        }
    })
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

#[cfg(test)]
mod tests {
    use super::{backoff_sleep, drain_in_flight, version_gt, DRAIN_BUDGET};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    // Zero in-flight: drain returns at once, no waiting.
    #[tokio::test(start_paused = true)]
    async fn drain_returns_immediately_when_idle() {
        let n = Arc::new(AtomicU32::new(0));
        let start = tokio::time::Instant::now();
        drain_in_flight(&n).await;
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    // In-flight count falls to zero before the budget: drain returns early.
    #[tokio::test(start_paused = true)]
    async fn drain_waits_until_in_flight_clears() {
        let n = Arc::new(AtomicU32::new(2));
        let n2 = n.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            n2.store(0, Ordering::Relaxed);
        });
        let start = tokio::time::Instant::now();
        drain_in_flight(&n).await;
        let waited = start.elapsed();
        // Cleared at ~1s, well under the budget, and not instant.
        assert!(waited >= Duration::from_secs(1));
        assert!(waited < DRAIN_BUDGET);
    }

    // In-flight never clears: drain gives up after the budget elapses.
    #[tokio::test(start_paused = true)]
    async fn drain_gives_up_after_budget() {
        let n = Arc::new(AtomicU32::new(1));
        let start = tokio::time::Instant::now();
        drain_in_flight(&n).await;
        let waited = start.elapsed();
        assert!(waited >= DRAIN_BUDGET);
        // It still exits (does not hang forever); cap the upper bound generously.
        assert!(waited < DRAIN_BUDGET + Duration::from_secs(1));
    }

    #[test]
    fn version_compare() {
        assert!(version_gt("0.4.0", "0.3.0"));
        assert!(version_gt("0.3.1", "0.3.0"));
        assert!(version_gt("1.0.0", "0.9.9"));
        assert!(!version_gt("0.3.0", "0.3.0"));
        assert!(!version_gt("0.2.0", "0.3.0"));
    }

    #[test]
    fn backoff_from_zero() {
        let mut delay_ms = 0;
        let d = backoff_sleep(&mut delay_ms);
        assert_eq!(d, Duration::ZERO);
        assert_eq!(delay_ms, 1000);
    }

    #[test]
    fn backoff_doubles_and_jitters() {
        let mut delay_ms = 1000;
        let d = backoff_sleep(&mut delay_ms);
        let ms = d.as_millis() as u64;
        assert!((1000..=1250).contains(&ms));
        assert_eq!(delay_ms, 2000);
    }

    #[test]
    fn backoff_caps() {
        let mut delay_ms = 30000;
        let d = backoff_sleep(&mut delay_ms);
        let ms = d.as_millis() as u64;
        assert!((30000..=37500).contains(&ms));
        assert_eq!(delay_ms, 30000);
    }
}
