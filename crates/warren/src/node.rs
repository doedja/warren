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
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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
/// Window over which an auto-update is randomly delayed. A hub redeploy announces
/// the new version to every auto-update node at once; without a stagger they would
/// all restart together and the whole pool would blink out. Each node waits a
/// random slice of this so updates roll across the fleet one at a time. Version
/// tolerance keeps the not-yet-updated nodes routable meanwhile.
const UPDATE_SPREAD: Duration = Duration::from_secs(300);
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
    /// upgrade this node. Off by default (a notice is logged instead). On Windows
    /// a detached helper swaps the locked exe after this process exits.
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
#[derive(Clone)]
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

    // Debounce slot for auto-update, shared across reconnects so a node that
    // reconnects mid-stagger does not schedule a second update.
    let update_pending = Arc::new(AtomicBool::new(false));
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
            update_pending.clone(),
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

/// Whether to schedule a self-update now: only when auto-update is on, the hub is
/// newer than this node, and no update is already pending (debounce, so a hub that
/// keeps re-announcing the version does not stack timers). Pure for testing.
fn should_schedule_update(
    remote: &str,
    local: &str,
    auto_update: bool,
    already_pending: bool,
) -> bool {
    auto_update && !already_pending && version_gt(remote, local)
}

/// On a newer hub version, log it and (with --auto-update) schedule the installer
/// to swap the binary and restart the service. Default is notice-only, because a
/// silent binary swap across a fleet is dangerous; enable per node once confirmed.
/// Version tolerance means an unupdated node keeps serving meanwhile.
///
/// The update is delayed by a random slice of [`UPDATE_SPREAD`] and runs in a
/// detached task, so a fleet-wide announce does not restart every node at once and
/// the read loop keeps answering pings while the timer counts down. `pending`
/// debounces: at most one update is scheduled per process.
fn maybe_self_update(remote: &str, auto_update: bool, r: &Resolved, pending: &Arc<AtomicBool>) {
    let local = env!("CARGO_PKG_VERSION");
    // Log the situation regardless of whether we act, so an operator always sees a
    // newer hub even with auto-update off.
    if version_gt(remote, local) {
        tracing::warn!(node = local, hub = remote, "hub is newer than this node");
        if !auto_update {
            tracing::warn!("auto-update off (pass --auto-update to enable); reinstall to upgrade");
        }
    }
    if !should_schedule_update(remote, local, auto_update, pending.load(Ordering::Acquire)) {
        return;
    }
    // Claim the single pending slot; if another HubVersion raced us to it, bail.
    if pending
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let delay = Duration::from_secs(rand::thread_rng().gen_range(0..=UPDATE_SPREAD.as_secs()));
    let r = r.clone();
    let pending = pending.clone();
    tracing::warn!(
        delay_s = delay.as_secs(),
        "auto-update scheduled (staggered)"
    );
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        if !self_update_now(&r) {
            // The updater could not launch (e.g. failed to spawn the detached
            // installer). Release the slot so a later hub announce retries instead
            // of being silently debounced for the rest of this process's life.
            pending.store(false, Ordering::Release);
        }
    });
}

/// Windows: the running .exe is locked, so a detached PowerShell helper waits for
/// THIS process to exit, then re-runs install.ps1 (stops the task, swaps the exe,
/// reinstalls the service, restarts it). We exit right after spawning so the
/// binary unlocks. `-AutoUpdate` persists the setting across the swap.
#[cfg(windows)]
fn self_update_now(r: &Resolved) -> bool {
    let ps_url = "https://raw.githubusercontent.com/doedja/warren/main/install.ps1";
    let mut inv = format!(
        "& ([scriptblock]::Create((irm {} -UseBasicParsing))) -Hub {}",
        ps_single_quote(ps_url),
        ps_single_quote(&r.hub)
    );
    if let Some(t) = &r.token {
        inv.push_str(&format!(" -Token {}", ps_single_quote(t)));
    }
    if r.tls {
        inv.push_str(" -Tls");
        if let Some(fp) = &r.fingerprint {
            inv.push_str(&format!(" -HubFingerprint {}", ps_single_quote(fp)));
        }
    }
    if r.insecure {
        inv.push_str(" -Insecure");
    }
    inv.push_str(" -AutoUpdate");
    let script = format!(
        "Wait-Process -Id {} -Timeout 300 -ErrorAction SilentlyContinue; {}",
        std::process::id(),
        inv
    );
    tracing::warn!("auto-updating: a helper will swap the binary after this node exits");
    if std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
            &script,
        ])
        .spawn()
        .is_ok()
    {
        // Release the locked exe so the helper can overwrite it. The helper
        // restarts the service (install.ps1 ends with schtasks /Run).
        std::process::exit(0);
    }
    tracing::warn!("auto-update: failed to spawn the Windows updater; reinstall manually");
    false
}

/// Unix: re-run install.sh to swap the binary and restart the service. The
/// installer STOPS the warren-node service mid-run (to release/replace the
/// binary), so it must run DETACHED from this process. Otherwise it is our child
/// and stopping the service kills it before the swap finishes, leaving the node
/// down (the v0.4.x bug). See `spawn_detached_unix` for how it survives.
#[cfg(unix)]
fn self_update_now(r: &Resolved) -> bool {
    let cmd = unix_update_cmd(r);
    if spawn_detached_unix(&cmd) {
        tracing::warn!("auto-updating: detached installer will swap the binary and restart");
        true
    } else {
        tracing::warn!("auto-update: failed to launch the updater; reinstall manually");
        false
    }
}

/// Build the `curl install.sh | sh -s -- ...` command that re-installs this node.
/// `--auto-update` is always appended so the setting survives the reinstall.
#[cfg(unix)]
fn unix_update_cmd(r: &Resolved) -> String {
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
    cmd.push_str(" --auto-update");
    cmd
}

/// Launch `script` detached enough to outlive stopping the warren-node service.
/// On systemd, a transient unit runs it in a cgroup that PID 1 owns, independent
/// of the warren-node unit's stop/restart. Elsewhere (launchd, non-systemd), a
/// new session (setsid) escapes the service's process-group teardown.
#[cfg(unix)]
fn spawn_detached_unix(script: &str) -> bool {
    use std::process::Command;
    if unix_command_exists("systemd-run") {
        return Command::new("systemd-run")
            .args(["--collect", "--quiet", "sh", "-c", script])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    }
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(script);
    // SAFETY: the only work in the child between fork and exec is setsid(), which
    // is async-signal-safe. It detaches the child into a new session so a service
    // manager signalling the old process group does not kill the updater.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().is_ok()
}

#[cfg(unix)]
fn unix_command_exists(name: &str) -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name} >/dev/null 2>&1"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(not(any(unix, windows)))]
fn self_update_now(_r: &Resolved) -> bool {
    tracing::warn!("auto-update unsupported on this platform; reinstall manually");
    false
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

#[allow(clippy::too_many_arguments)]
async fn connect_once(
    r: &Resolved,
    name: &str,
    connector: &Option<TlsConnector>,
    identity: &Identity,
    mut shutdown_rx: watch::Receiver<bool>,
    in_flight: Arc<AtomicU32>,
    auto_update: bool,
    update_pending: Arc<AtomicBool>,
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
    // A plaintext node against a TLS hub fails here (garbled handshake), so when
    // we are not using TLS, add a pointed hint instead of a bare decode/timeout.
    let tls_hint = "; if the hub uses TLS, re-run with the join code (or --tls)";
    let (ctrl, reply) = match tokio::time::timeout(CONNECT_TIMEOUT, async {
        let mut ctrl = mux::open(&opener).await?;
        write_msg(&mut ctrl, &Greeting::Control(hello)).await?;
        let reply: HelloReply = read_msg(&mut ctrl).await?;
        Ok::<_, anyhow::Error>((ctrl, reply))
    })
    .await
    {
        Ok(Ok(v)) => v,
        Ok(Err(e)) if connector.is_none() => {
            return Err(e.context(format!("enrollment handshake failed{tls_hint}")));
        }
        Ok(Err(e)) => return Err(e.context("enrollment handshake failed")),
        Err(_) if connector.is_none() => {
            anyhow::bail!("hub enrollment handshake timed out{tls_hint}");
        }
        Err(_) => anyhow::bail!("hub enrollment handshake timed out"),
    };
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
                            maybe_self_update(&version, auto_update, r, &update_pending);
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
    // Opt-in auto-update: bake it into the service command so it persists across
    // restarts (and across a self-update, which re-runs the installer).
    if a.auto_update {
        v.push("--auto-update".into());
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
///
/// Built with PowerShell's ScheduledTask cmdlets, which generate schema-valid
/// task XML for us. Hand-written XML is too fragile here: the Settings elements
/// must appear in a fixed order, and the restart Interval has a 1-minute minimum
/// (PT15S is rejected as "out of range"). The cmdlets give us the parity the
/// flat `schtasks /TR` form cannot: restart-on-failure (matches systemd
/// `Restart=always` / launchd `KeepAlive`), no execution time limit (the flat
/// form inherits the 72h default and would kill a long-running node), and
/// run-on-batteries. If PowerShell is unavailable or fails, fall back to the
/// flat form so the node still starts at boot (just without auto-restart).
fn install_windows(argv: &[String]) -> Result<()> {
    let exe = ps_single_quote(&argv[0]);
    let args = ps_single_quote(&argv[1..].join(" "));
    // RestartInterval is 1 minute (the Task Scheduler minimum); ExecutionTimeLimit
    // of zero means no limit; SYSTEM + Highest, triggered at boot.
    let ps = format!(
        "$ErrorActionPreference='Stop'; \
         $a=New-ScheduledTaskAction -Execute {exe} -Argument {args}; \
         $t=New-ScheduledTaskTrigger -AtStartup; \
         $p=New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest; \
         $s=New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartInterval (New-TimeSpan -Minutes 1) -RestartCount 999; \
         Register-ScheduledTask -TaskName 'warren-node' -Action $a -Trigger $t -Principal $p -Settings $s -Force | Out-Null"
    );
    let registered = run_cmd_quiet(
        "powershell",
        &["-NoProfile", "-NonInteractive", "-Command", &ps],
    );
    if !registered {
        // Fallback: the basic boot task without restart-on-failure, using the
        // flat schtasks form that has always worked. Better a node that starts
        // at boot than no service at all.
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
        )
        .context("schtasks /Create failed (run from an elevated/admin shell?)")?;
    }
    let _ = run_cmd_quiet("schtasks", &["/Run", "/TN", "warren-node"]);
    println!(
        "installed Windows scheduled task 'warren-node' (runs at startup, restarts on failure)"
    );
    Ok(())
}

/// Wrap a value as a PowerShell single-quoted literal (doubling embedded quotes),
/// so an exe path or argument cannot break out of the -Command string.
fn ps_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
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

/// Escape XML text so plist string values containing `&`, `<`, `>`, or quotes
/// (e.g. a binary path or argument) produce a well-formed plist.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn install_launchd(argv: &[String]) -> Result<()> {
    // A launchd LaunchAgent is per-user. It must be loaded into the target
    // user's GUI domain (gui/<uid>), not root's. If the installer runs as root
    // (curl | sudo sh, or `sudo warren node install`), a root `launchctl load`
    // lands the agent in the wrong domain and it silently never starts. Resolve
    // the real login user instead, and chown the plist to them so it stays
    // user-managed (an upgrade/uninstall later runs without sudo).
    let t = macos_target()?;
    let plist_path = format!("{}/Library/LaunchAgents/{LAUNCHD_LABEL}.plist", t.home);
    let args_xml: String = argv
        .iter()
        .map(|a| format!("    <string>{}</string>\n", xml_escape(a)))
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
        // If we ran as root, the LaunchAgents dir we may have just created is
        // root-owned; hand it back to the user so they can manage it.
        if current_euid() == 0 {
            chown_to(&dir.to_string_lossy(), t.uid, t.gid);
        }
    }
    std::fs::write(&plist_path, plist).with_context(|| format!("write {plist_path}"))?;
    if current_euid() == 0 {
        chown_to(&plist_path, t.uid, t.gid);
    }
    let domain = format!("gui/{}", t.uid);
    load_launchd(&domain, &plist_path)?;
    println!("installed and loaded launchd agent: {plist_path} (domain {domain})");
    Ok(())
}

/// (Re)load a LaunchAgent into a GUI domain. bootout + bootstrap is the modern
/// path; a KeepAlive service torn down by bootout settles asynchronously, so an
/// immediate bootstrap can fail with EIO (error 5). Retry a few times, then fall
/// back to the legacy `load -w` for pre-bootstrap macOS.
fn load_launchd(domain: &str, plist_path: &str) -> Result<()> {
    let service = format!("{domain}/{LAUNCHD_LABEL}");
    let _ = run_cmd_quiet("launchctl", &["bootout", &service]);
    for _ in 0..4 {
        if run_cmd_quiet("launchctl", &["bootstrap", domain, plist_path]) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(700));
        let _ = run_cmd_quiet("launchctl", &["bootout", &service]);
    }
    // Loud fallback: surfaces the real launchctl error if this also fails.
    run_cmd("launchctl", &["load", "-w", plist_path])
}

/// The user the agent should run as: uid, gid, and home directory.
struct MacTarget {
    uid: u32,
    gid: u32,
    home: String,
}

/// Resolve the login user even when the installer runs as root. Priority:
/// SUDO_USER (sudo), then the console (GUI session) owner (plain root), then the
/// current user. Loading an agent into root's own domain (gui/0) does not work,
/// so root installs must target the human user's domain.
fn macos_target() -> Result<MacTarget> {
    let user = std::env::var("SUDO_USER")
        .ok()
        .filter(|u| !u.trim().is_empty())
        .map(|u| u.trim().to_string())
        .or_else(|| {
            if current_euid() == 0 {
                console_user()
            } else {
                None
            }
        });
    if let Some(user) = user {
        let uid = id_num("-u", &user).with_context(|| format!("resolve uid for {user}"))?;
        let gid = id_num("-g", &user).with_context(|| format!("resolve gid for {user}"))?;
        return Ok(MacTarget {
            uid,
            gid,
            home: home_for_user(&user),
        });
    }
    Ok(MacTarget {
        uid: current_euid(),
        gid: current_egid(),
        home: std::env::var("HOME").context("HOME not set")?,
    })
}

/// The GUI/console session owner (`stat -f%Su /dev/console`), or None when it is
/// root/loginwindow (no human session to target).
fn console_user() -> Option<String> {
    let out = std::process::Command::new("stat")
        .args(["-f", "%Su", "/dev/console"])
        .output()
        .ok()?;
    let user = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if user.is_empty() || user == "root" || user == "loginwindow" {
        None
    } else {
        Some(user)
    }
}

/// `id <flag> <user>` as a number (e.g. `id -u alice`).
fn id_num(flag: &str, user: &str) -> Result<u32> {
    let out = std::process::Command::new("id")
        .args([flag, user])
        .output()?;
    String::from_utf8(out.stdout)?
        .trim()
        .parse::<u32>()
        .map_err(Into::into)
}

fn current_euid() -> u32 {
    id_self("-u")
}

fn current_egid() -> u32 {
    id_self("-g")
}

fn id_self(flag: &str) -> u32 {
    std::process::Command::new("id")
        .arg(flag)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0)
}

/// Look up a macOS user's home dir (dscl), falling back to the /Users convention.
fn home_for_user(user: &str) -> String {
    if let Ok(out) = std::process::Command::new("dscl")
        .args([".", "-read", &format!("/Users/{user}"), "NFSHomeDirectory"])
        .output()
    {
        if let Ok(s) = String::from_utf8(out.stdout) {
            if let Some(path) = s.split_whitespace().last() {
                if path.starts_with('/') {
                    return path.to_string();
                }
            }
        }
    }
    format!("/Users/{user}")
}

fn run_cmd_quiet(cmd: &str, args: &[&str]) -> bool {
    std::process::Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// chown a path to (uid, gid). No-op on non-unix (the crate also builds for
/// Windows, where std::os::unix is absent and this path never runs).
#[cfg(unix)]
fn chown_to(path: &str, uid: u32, gid: u32) {
    let _ = std::os::unix::fs::chown(path, Some(uid), Some(gid));
}
#[cfg(not(unix))]
fn chown_to(_path: &str, _uid: u32, _gid: u32) {}

async fn uninstall_service() -> Result<()> {
    match current_platform() {
        Platform::Linux => {
            let _ = run_cmd("systemctl", &["disable", "--now", "warren-node"]);
            std::fs::remove_file(SYSTEMD_UNIT).ok();
            let _ = run_cmd("systemctl", &["daemon-reload"]);
            println!("removed systemd service: warren-node");
        }
        Platform::MacOs => {
            let t = macos_target()?;
            let plist_path = format!("{}/Library/LaunchAgents/{LAUNCHD_LABEL}.plist", t.home);
            let service = format!("gui/{}/{LAUNCHD_LABEL}", t.uid);
            // bootout the modern way; fall back to legacy unload.
            if !run_cmd_quiet("launchctl", &["bootout", &service]) {
                let _ = run_cmd_quiet("launchctl", &["unload", "-w", &plist_path]);
            }
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
    use super::{
        backoff_sleep, drain_in_flight, node_run_argv, should_schedule_update, version_gt, RunArgs,
        DRAIN_BUDGET,
    };
    #[cfg(unix)]
    use super::{unix_update_cmd, Resolved};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn schedule_update_requires_newer_auto_and_not_pending() {
        // Newer hub + auto-update on + nothing pending: schedule.
        assert!(should_schedule_update("0.4.8", "0.4.7", true, false));
        // Auto-update off: never schedule (notice only).
        assert!(!should_schedule_update("0.4.8", "0.4.7", false, false));
        // Not newer (equal or older): never schedule.
        assert!(!should_schedule_update("0.4.7", "0.4.7", true, false));
        assert!(!should_schedule_update("0.4.6", "0.4.7", true, false));
        // Already pending: debounce, do not stack a second timer.
        assert!(!should_schedule_update("0.4.8", "0.4.7", true, true));
    }

    #[cfg(unix)]
    fn resolved(tls: bool) -> Resolved {
        Resolved {
            hub: "h:7000".into(),
            token: Some("tok".into()),
            tls,
            fingerprint: tls.then(|| "FP".into()),
            insecure: false,
        }
    }

    // The self-update installer command carries the connection flags and always
    // re-adds --auto-update so the opt-in survives the reinstall.
    #[cfg(unix)]
    #[test]
    fn unix_update_cmd_includes_flags_and_auto_update() {
        let c = unix_update_cmd(&resolved(true));
        assert!(c.contains("--hub h:7000"));
        assert!(c.contains("--token tok"));
        assert!(c.contains("--tls --hub-fingerprint FP"));
        assert!(c.trim_end().ends_with("--auto-update"));
    }

    // TLS flags are omitted when TLS is off; --auto-update is still appended.
    #[cfg(unix)]
    #[test]
    fn unix_update_cmd_omits_tls_when_off() {
        let c = unix_update_cmd(&resolved(false));
        assert!(!c.contains("--tls"));
        assert!(!c.contains("--hub-fingerprint"));
        assert!(c.contains("--auto-update"));
    }

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

    fn run_args(auto_update: bool) -> RunArgs {
        RunArgs {
            join: None,
            hub: Some("127.0.0.1:7000".into()),
            token: Some("tok".into()),
            key_file: None,
            name: String::new(),
            tls: false,
            hub_fingerprint: None,
            insecure: false,
            auto_update,
        }
    }

    // The service command must carry --auto-update only when it was requested, so
    // the opt-in survives restarts and self-updates (and stays off by default).
    #[test]
    fn argv_forwards_auto_update() {
        let on = node_run_argv("/usr/local/bin/warren", &run_args(true));
        assert!(on.iter().any(|a| a == "--auto-update"));
        let off = node_run_argv("/usr/local/bin/warren", &run_args(false));
        assert!(!off.iter().any(|a| a == "--auto-update"));
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
