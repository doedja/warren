//! Hub mode: client-facing HTTP CONNECT proxy + node control plane.
//!
//! Each node holds ONE yamux-multiplexed connection to the hub. The hub accepts
//! logical streams off it, distinguished by the first framed [`Greeting`]:
//!   - Control: the first stream; the node enrolls, then it carries Dial/Ping/Pong.
//!   - Data: one stream per proxied request, tagged with a conn_id, handed to the
//!     waiting client handler to splice.
//!
//! The node link is TLS by default (self-signed cert, fingerprint printed at
//! startup; `--no-tls` drops to plaintext for localhost). The client-facing
//! proxy auto-detects HTTP CONNECT, SOCKS5, and plain-HTTP on one port.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;

use anyhow::{Context, Result};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Html;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use clap::Args;
use serde::{Deserialize, Serialize};
use tokio::io::{copy_bidirectional, split, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, oneshot, Mutex, Notify};
use tokio::time::{timeout, Duration};
use tokio_rustls::TlsAcceptor;

use warren_proto::{
    DataHello, Greeting, Hello, HelloReply, HubToNode, NodeId, NodeToHub, UdpDatagram,
};

use crate::conn::Conn;
use crate::identity;
use crate::mux::{self, MuxStream};
use crate::proxy;
use crate::socks5;
use crate::store::Store;
use crate::tls;
use crate::wire::{read_msg, write_msg};

/// A node is skipped (tried only as last resort) once its consecutive-failure
/// count reaches this.
const UNHEALTHY_AT: u32 = 3;

/// How long a recorded failure keeps counting toward a node's health. A success
/// resets `fails` to 0 instantly; absent a success, recent failures expire after
/// this window so a node that simply stopped failing (or went idle) recovers its
/// health instead of sticking unhealthy until its next successful dial. Also
/// stops transient dead-target timeouts from pinning an otherwise-fine node red.
const HEALTH_WINDOW_SECS: i64 = 60;

/// Effective recent-failure count: the raw counter, or 0 once the last failure is
/// older than [`HEALTH_WINDOW_SECS`] (or there are no failures). Used everywhere
/// health is read (routing order, dashboard, metrics) so a stale counter neither
/// deprioritizes a recovered node nor shows it as unhealthy. Pure for testing.
fn effective_fails(raw: u32, last_fail: i64, now: i64) -> u32 {
    if raw == 0 || now.saturating_sub(last_fail) > HEALTH_WINDOW_SECS {
        0
    } else {
        raw
    }
}

/// Per-node cap on concurrent in-flight dials.
const MAX_INFLIGHT_PER_NODE: u32 = 64;

/// Cap on distinct hosts tracked per node before low-count entries are dropped.
const HOST_FAIL_CAP: usize = 256;

/// How long a sticky session keeps mapping to the same node after its last use.
const SESSION_TTL: Duration = Duration::from_secs(600);
/// Cap on tracked sessions before expired ones are swept.
const SESSION_CAP: usize = 10_000;

/// Bound on enrollment (control stream open + Hello) so a peer that finishes TLS
/// then stalls cannot pin a connection + driver task indefinitely.
const ENROLL_TIMEOUT: Duration = Duration::from_secs(20);

/// Minimum node protocol for UDP relay (UdpOpen landed in v5).
const UDP_MIN_VERSION: u16 = 5;
/// Minimum node protocol that understands the HubVersion self-update hint (v6).
const HUB_VERSION_MIN: u16 = 6;
/// Bound on a data stream sending its DataHello before we give up on it, so a
/// stream opened but never identified cannot leak a task + open stream.
const DATA_HELLO_TIMEOUT: Duration = Duration::from_secs(15);
/// Upper bound on a proxy client's pre-tunnel handshake (peek + SOCKS5/HTTP
/// negotiation). Stops a client that connects then stalls from pinning a task +
/// socket. Does not bound the data splice that follows.
const PROXY_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Args, Debug)]
pub struct HubArgs {
    /// Node-facing listen address (one multiplexed connection per node).
    #[arg(long, default_value = "0.0.0.0:7000")]
    pub listen: String,
    /// Client-facing proxy listen address (HTTP CONNECT).
    #[arg(long, default_value = "0.0.0.0:8000")]
    pub proxy_listen: String,
    /// SQLite database (enrollment tokens + proxy users). Created if absent.
    #[arg(long, env = "WARREN_DB", default_value = "warren.db")]
    pub db: String,
    /// Seed this enrollment token into the DB on startup (optional).
    #[arg(long, env = "WARREN_ENROLL_TOKEN")]
    pub enroll_token: Option<String>,
    /// Seed this proxy username into the DB on startup (with --proxy-pass).
    #[arg(long, env = "WARREN_PROXY_USER")]
    pub proxy_user: Option<String>,
    /// Optional Basic-auth password required of proxy clients.
    #[arg(long, env = "WARREN_PROXY_PASS")]
    pub proxy_pass: Option<String>,
    /// Deprecated: TLS is on by default now. Accepted for compatibility, no effect.
    #[arg(long, default_value_t = false, hide = true)]
    pub tls: bool,
    /// Turn OFF TLS on the node link (plaintext). Use only on localhost or a tailnet.
    #[arg(long, default_value_t = false)]
    pub no_tls: bool,
    /// Persist the TLS cert under this dir so the fingerprint survives restarts.
    /// Without it, a fresh cert is generated each boot.
    #[arg(long, env = "WARREN_TLS_CERT_DIR")]
    pub tls_cert_dir: Option<String>,
    /// Serve the admin API + dashboard on this address (e.g. 127.0.0.1:9000).
    #[arg(long, env = "WARREN_ADMIN_LISTEN")]
    pub admin_listen: Option<String>,
    /// Bearer token required by the admin API.
    #[arg(long, env = "WARREN_ADMIN_TOKEN")]
    pub admin_token: Option<String>,
    /// Public address nodes dial, shown in the dashboard (e.g. 1.2.3.4:7000).
    #[arg(long, env = "WARREN_PUBLIC_NODE_ADDR")]
    pub public_node_addr: Option<String>,
    /// Public proxy address, shown in the dashboard (e.g. 1.2.3.4:18080).
    #[arg(long, env = "WARREN_PUBLIC_PROXY_ADDR")]
    pub public_proxy_addr: Option<String>,
}

#[derive(Args, Debug)]
pub struct EnrollArgs {
    /// Friendly name for the node (informational).
    #[arg(long, default_value = "node")]
    pub name: String,
    /// SQLite database to add the token to (must match the hub's --db).
    #[arg(long, env = "WARREN_DB", default_value = "warren.db")]
    pub db: String,
}

struct NodeEntry {
    id: NodeId,
    /// Friendly device name (without the disambiguating code suffix). Clients
    /// select a single device by putting this after a `+` in the proxy
    /// username, e.g. `user+phone`.
    name: String,
    /// Unix seconds when this control connection enrolled (for "up since").
    since: i64,
    tx: mpsc::UnboundedSender<HubToNode>,
    /// Consecutive dial failures (any target); reset to 0 on a successful dial.
    /// Read through [`effective_fails`] so failures older than the health window
    /// stop counting (see `last_fail`).
    fails: Arc<AtomicU32>,
    /// Unix seconds of the most recent dial failure. Paired with `fails` so a run
    /// of failures expires after [`HEALTH_WINDOW_SECS`] with no new failure,
    /// letting an idle or recovered node return to healthy.
    last_fail: Arc<AtomicI64>,
    /// Per-target-host consecutive failures. Lets routing prefer nodes that are
    /// still "fresh" on a given host (transport-level reachability). App-level
    /// blocks like 429/403 are inside the TLS tunnel and not observable here.
    host_fails: Arc<StdMutex<HashMap<String, u32>>>,
    /// Number of in-flight dials currently being handled by this node.
    in_flight: Arc<AtomicU32>,
    /// The node's reported protocol version. Routing gates version-specific
    /// features (e.g. UDP relay needs >= 5) so older nodes still serve TCP.
    protocol_version: u16,
    /// The node's binary version (semver, e.g. "0.4.0"), shown on the dashboard
    /// so an operator can spot nodes that need updating.
    agent_version: String,
    /// Public egress IP + geo, self-reported by the node (best-effort, may be
    /// empty until the first report arrives).
    info: Arc<StdMutex<NodeReport>>,
    /// Group label: the name of the enroll token this node joined with. Lets a
    /// client target every node in a group with `user-group-NAME`. None for nodes
    /// that joined without a token (admin-approved) or with an unnamed token.
    group: Option<String>,
    /// Unique per-connection sequence (not the stable node id). A reconnecting
    /// node keeps the same id but gets a fresh conn_seq, so the dead-peer reap of
    /// a stale half-open connection removes only its own entry, never the live
    /// reconnected one that shares the id.
    conn_seq: u64,
    /// The node's key fingerprint (same value the store keys approvals on). Lets a
    /// key revocation find and tear down this live link immediately.
    pubkey: String,
    /// Fired to force this link down (e.g. on key revocation). The connection
    /// task selects on it alongside its read/accept loops.
    shutdown: Arc<Notify>,
}

#[derive(Default, Clone)]
struct NodeReport {
    ip: Option<String>,
    country: Option<String>,
    city: Option<String>,
    latency_ms: Option<u32>,
}

/// Runtime config independent of the CLI, so tests can drive the hub with
/// pre-bound listeners on ephemeral ports.
pub struct HubConfig {
    pub store: Arc<Store>,
    pub tls: Option<TlsAcceptor>,
    /// (listen addr, bearer token) for the admin API + dashboard.
    pub admin: Option<(String, String)>,
    pub public_node_addr: Option<String>,
    pub public_proxy_addr: Option<String>,
    pub fingerprint: Option<String>,
    /// Pre-bound UDP socket for the SOCKS5 UDP relay (shared across associations,
    /// demuxed by client IP). `None` disables UDP ASSOCIATE.
    pub udp_relay: Option<UdpSocket>,
}

/// A live UDP association: the channel that forwards client datagrams to the
/// node's relay stream, and the client's source address for replies.
struct UdpAssoc {
    /// Bounded so a client flooding faster than its node drains drops datagrams
    /// (UDP is lossy) instead of growing hub memory without limit.
    to_node: mpsc::Sender<UdpDatagram>,
    client_src: Arc<StdMutex<Option<SocketAddr>>>,
    /// Activity timestamp (idle close) + per-second rate counter.
    meter: Arc<StdMutex<AssocMeter>>,
}

/// Per-association queue depth from the demux to a node's relay stream.
const UDP_QUEUE_DEPTH: usize = 1024;
/// Close a UDP association after this long with no client datagrams (frees the
/// node stream + tasks for clients that vanish without closing their TCP conn).
const UDP_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// Per-association cap on client->target datagrams per second (amplification /
/// reflection guard). Excess datagrams are dropped (UDP is lossy).
const UDP_MAX_PPS: u32 = 2000;

/// Per-association activity + rate state, behind one lock.
struct AssocMeter {
    last_seen: Instant,
    window_start: Instant,
    count: u32,
}
impl AssocMeter {
    fn new(now: Instant) -> Self {
        AssocMeter {
            last_seen: now,
            window_start: now,
            count: 0,
        }
    }
    /// Record a datagram at `now`; returns false if it exceeds the per-second cap.
    fn allow(&mut self, now: Instant) -> bool {
        self.last_seen = now;
        if now.duration_since(self.window_start) >= Duration::from_secs(1) {
            self.window_start = now;
            self.count = 0;
        }
        self.count += 1;
        self.count <= UDP_MAX_PPS
    }
}

struct Hub {
    store: Arc<Store>,
    tls: Option<TlsAcceptor>,
    public_node_addr: Option<String>,
    public_proxy_addr: Option<String>,
    fingerprint: Option<String>,
    /// Shared UDP relay socket (SOCKS5 UDP ASSOCIATE) + live associations keyed
    /// by client IP. The relay is reachable at the proxy address on UDP.
    udp_relay: Option<Arc<UdpSocket>>,
    udp_assoc: Mutex<HashMap<IpAddr, UdpAssoc>>,
    nodes: Mutex<Vec<NodeEntry>>,
    /// conn_id -> (expected data-conn nonce, waker for the client). The nonce
    /// authenticates the data connection: only the node we sent the Dial to
    /// knows it, so a guessed conn_id alone cannot hijack the splice.
    pending: Mutex<HashMap<u64, (u64, oneshot::Sender<MuxStream>)>>,
    rr: AtomicUsize,
    conn_seq: AtomicU64,
    /// Sticky sessions: session key -> (node name, last use). A `user-session-K`
    /// proxy username keeps requests on the same device while it stays healthy.
    sessions: Mutex<HashMap<String, (String, Instant)>>,
    /// Cumulative bytes relayed per node id and per proxy user (in-memory).
    node_bytes: Mutex<HashMap<String, u64>>,
    user_bytes: Mutex<HashMap<String, u64>>,
    /// Per node id: (successful dials, total dials, last error reason). For the
    /// dashboard's success-rate + last-error display.
    node_stats: Mutex<NodeStats>,
    /// Concurrent node-link count per source IP. A node holds one link, so a flood
    /// of connections from one IP is abuse; capped to bound task/FD growth.
    node_conns: StdMutex<HashMap<IpAddr, u32>>,
    /// Highest enrollment timestamp accepted per node key fingerprint. A new
    /// Hello must be strictly newer, so a captured Hello cannot be replayed within
    /// the 120s skew window to displace the live link. In-memory: a hub restart
    /// reopens only the 120s window.
    last_auth: StdMutex<HashMap<String, u64>>,
}

/// Per node id -> (ok dials, total dials, last error). Aliased to keep the
/// `Hub` field readable (clippy::type_complexity).
type NodeStats = HashMap<String, (u32, u32, Option<String>)>;

/// A routing snapshot of one node: id, name, control tx, fails, per-host fails,
/// in-flight, protocol version, last-fail time. Cloned out so routing decisions
/// touch no locks.
type NodeSnap = (
    NodeId,
    String,
    mpsc::UnboundedSender<HubToNode>,
    Arc<AtomicU32>,
    Arc<StdMutex<HashMap<String, u32>>>,
    Arc<AtomicU32>,
    u16,
    Arc<AtomicI64>,
);

fn node_snap(n: &NodeEntry) -> NodeSnap {
    (
        n.id.clone(),
        n.name.clone(),
        n.tx.clone(),
        n.fails.clone(),
        n.host_fails.clone(),
        n.in_flight.clone(),
        n.protocol_version,
        n.last_fail.clone(),
    )
}

impl Hub {
    fn next_conn_id(&self) -> u64 {
        self.conn_seq.fetch_add(1, Ordering::Relaxed)
    }
    /// Force every live link whose key fingerprint is `pubkey` down. Returns how
    /// many were signalled. Called on key revocation so a captured session cannot
    /// keep proxying until the link drops on its own.
    async fn disconnect_node(&self, pubkey: &str) -> usize {
        let nodes = self.nodes.lock().await;
        let mut n = 0;
        for e in nodes.iter().filter(|e| e.pubkey == pubkey) {
            e.shutdown.notify_one();
            n += 1;
        }
        n
    }
    /// Snapshot of connected nodes (id, name, tx, fails, host_fails, in_flight). When `sel`
    /// is `Some(name)`, only nodes with that exact device name are returned, so a
    /// client can route through one specific device (and gets nothing, not a
    /// fallback, if it is offline).
    // `map_or(true, ...)` keeps the MSRV at 1.74; `Option::is_none_or` is 1.82+.
    #[allow(clippy::type_complexity, clippy::unnecessary_map_or)]
    async fn snapshot(&self, sel: Option<&str>) -> Vec<NodeSnap> {
        self.nodes
            .lock()
            .await
            .iter()
            .filter(|n| sel.map_or(true, |s| n.name == s))
            .map(node_snap)
            .collect()
    }

    /// Look up the sticky node name for a session key if it is still within TTL.
    async fn session_node(&self, key: &str) -> Option<String> {
        let map = self.sessions.lock().await;
        map.get(key)
            .and_then(|(name, seen)| (seen.elapsed() < SESSION_TTL).then(|| name.clone()))
    }

    /// Record (or refresh) the node a session is stuck to.
    async fn set_session(&self, key: &str, name: &str) {
        let mut map = self.sessions.lock().await;
        if map.len() > SESSION_CAP {
            map.retain(|_, (_, seen)| seen.elapsed() < SESSION_TTL);
        }
        map.insert(key.to_string(), (name.to_string(), Instant::now()));
    }

    /// Add relayed bytes to the per-node and per-user counters.
    async fn add_bytes(&self, node_id: &str, user: &str, n: u64) {
        if n == 0 {
            return;
        }
        *self
            .node_bytes
            .lock()
            .await
            .entry(node_id.to_string())
            .or_insert(0) += n;
        if !user.is_empty() {
            *self
                .user_bytes
                .lock()
                .await
                .entry(user.to_string())
                .or_insert(0) += n;
        }
    }
    /// Record a dial outcome for a node: bumps total, bumps ok on success, sets
    /// the last error reason on failure. Feeds the dashboard success-rate column.
    async fn record_dial(&self, node_id: &str, ok: bool, reason: Option<&str>) {
        let mut m = self.node_stats.lock().await;
        let e = m.entry(node_id.to_string()).or_insert((0, 0, None));
        e.1 += 1;
        if ok {
            e.0 += 1;
        } else {
            e.2 = reason.map(str::to_string);
        }
    }
    /// Snapshot of nodes whose reported geo country matches `country`
    /// (case-insensitive). Same shape as [`snapshot`].
    async fn snapshot_region(&self, country: &str) -> Vec<NodeSnap> {
        self.nodes
            .lock()
            .await
            .iter()
            .filter(|n| {
                n.info
                    .lock()
                    .unwrap()
                    .country
                    .as_deref()
                    .is_some_and(|c| c.eq_ignore_ascii_case(country))
            })
            .map(node_snap)
            .collect()
    }
    /// Snapshot of nodes in `group` (the enroll-token name they joined with),
    /// case-insensitive. Same shape as [`snapshot`]. No fallback if none match.
    async fn snapshot_group(&self, group: &str) -> Vec<NodeSnap> {
        self.nodes
            .lock()
            .await
            .iter()
            .filter(|n| {
                n.group
                    .as_deref()
                    .is_some_and(|g| g.eq_ignore_ascii_case(group))
            })
            .map(node_snap)
            .collect()
    }
    async fn add_node(&self, e: NodeEntry) {
        let mut nodes = self.nodes.lock().await;
        displace_and_push(&mut nodes, e);
    }
    async fn remove_node(&self, id: &NodeId, conn_seq: u64) {
        // Only purge the per-id byte/stat counters once the node is fully gone.
        // A stale half-open connection timing out must not wipe the live
        // reconnected connection's counters (they share the id).
        let id_still_present = {
            let mut nodes = self.nodes.lock().await;
            remove_conn(&mut nodes, id, conn_seq)
        };
        if !id_still_present {
            self.node_bytes.lock().await.remove(&id.0);
            self.node_stats.lock().await.remove(&id.0);
        }
    }
    async fn insert_pending(&self, id: u64, nonce: u64, tx: oneshot::Sender<MuxStream>) {
        self.pending.lock().await.insert(id, (nonce, tx));
    }
    async fn take_pending(&self, id: u64) -> Option<(u64, oneshot::Sender<MuxStream>)> {
        self.pending.lock().await.remove(&id)
    }
    async fn list_node_info(&self) -> Vec<NodeInfo> {
        let bytes = self.node_bytes.lock().await.clone();
        let stats = self.node_stats.lock().await.clone();
        self.nodes
            .lock()
            .await
            .iter()
            .map(|n| {
                let r = n.info.lock().unwrap().clone();
                let (ok, total, last_error) = stats.get(&n.id.0).cloned().unwrap_or((0, 0, None));
                let success_rate = success_pct(ok, total);
                NodeInfo {
                    id: n.id.0.clone(),
                    name: n.name.clone(),
                    fails: effective_fails(
                        n.fails.load(Ordering::Relaxed),
                        n.last_fail.load(Ordering::Relaxed),
                        unix_now(),
                    ),
                    since: n.since,
                    ip: r.ip,
                    country: r.country,
                    city: r.city,
                    latency_ms: r.latency_ms,
                    bytes: bytes.get(&n.id.0).copied().unwrap_or(0),
                    success_rate,
                    dials: total,
                    last_error,
                    version: n.agent_version.clone(),
                    group: n.group.clone(),
                }
            })
            .collect()
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Success rate as a percent, or None until at least one dial has been tried
/// (avoids divide-by-zero and shows a dash in the dashboard for fresh nodes).
fn success_pct(ok: u32, total: u32) -> Option<f64> {
    (total > 0).then(|| (ok as f64 / total as f64) * 100.0)
}

/// Random lowercase-alnum token (32 chars).
fn gen_token() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..32)
        .map(|_| {
            let c: u8 = rng.gen_range(0u8..36);
            if c < 10 {
                (b'0' + c) as char
            } else {
                (b'a' + (c - 10)) as char
            }
        })
        .collect()
}

/// Directory that holds the db, used as the default TLS cert location so the
/// fingerprint persists next to the rest of the hub's state.
fn db_parent_dir(db: &str) -> String {
    std::path::Path::new(db)
        .parent()
        .map(|p| {
            if p.as_os_str().is_empty() {
                ".".to_string()
            } else {
                p.to_string_lossy().into_owned()
            }
        })
        .unwrap_or_else(|| ".".to_string())
}

/// Print the secrets and a ready-to-paste join command once at startup, so a
/// fresh hub is usable without digging through flags or the dashboard.
fn print_startup(
    store: &Store,
    tls_on: bool,
    fingerprint: Option<&str>,
    public_node_addr: Option<&str>,
    login: Option<(String, String)>,
) -> Result<()> {
    let token = store.list_tokens()?.into_iter().next().map(|(t, _, _)| t);
    println!("\n  warren hub is up.");
    if let Some((u, p)) = &login {
        println!("  proxy login:   {u} / {p}");
    }
    if let Some(t) = &token {
        println!("  enroll token:  {t}");
    }
    if tls_on {
        if let Some(fp) = fingerprint {
            println!("  fingerprint:   {fp}");
        }
    }
    match (public_node_addr, &token) {
        (Some(addr), Some(t)) => {
            let code = crate::joincode::encode(addr, tls_on, fingerprint, Some(t));
            println!("  join a device: warren node run --join {code}");
        }
        _ => println!(
            "  (set --public-node-addr to print a one-paste join code, or copy it from the dashboard)"
        ),
    }
    println!();
    Ok(())
}

pub async fn run(args: HubArgs) -> Result<()> {
    let node_listener = TcpListener::bind(&args.listen)
        .await
        .with_context(|| format!("bind node listener {}", args.listen))?;
    let proxy_listener = TcpListener::bind(&args.proxy_listen)
        .await
        .with_context(|| format!("bind proxy listener {}", args.proxy_listen))?;

    // TLS is on by default; --no-tls drops to plaintext for localhost/tailnet.
    // The cert is persisted (next to the db unless --tls-cert-dir is given) so
    // the fingerprint stays stable across restarts and pinned nodes keep working.
    let tls_on = !args.no_tls;
    let (tls, fingerprint) = if tls_on {
        let cert_dir = args
            .tls_cert_dir
            .clone()
            .unwrap_or_else(|| db_parent_dir(&args.db));
        let (acceptor, fp) = tls::server_acceptor_from_dir(&cert_dir)?;
        tracing::info!(fingerprint = %fp, cert_dir = %cert_dir, "TLS on (node link)");
        (Some(acceptor), Some(fp))
    } else {
        tracing::warn!("node link is PLAINTEXT (--no-tls); use only on localhost or a tailnet");
        (None, None)
    };

    let store = Arc::new(Store::open(&args.db).with_context(|| format!("open db {}", args.db))?);
    if let Some(tok) = &args.enroll_token {
        store.add_token(tok, "cli")?;
    } else if store.list_tokens()?.is_empty() {
        // No token supplied and none stored yet: mint one so a fresh hub is
        // usable without inventing a secret. Rotate with `warren enroll`.
        store.add_token(&gen_token(), "auto")?;
    }
    // Seed a proxy login. If none is given and none exists yet, generate one so
    // a fresh hub is immediately usable; the operator can change it later.
    let mut printed_login: Option<(String, String)> = None;
    match (&args.proxy_user, &args.proxy_pass) {
        (Some(u), Some(p)) => store.add_user(u, p)?,
        _ if !store.auth_required().unwrap_or(false) => {
            let (u, p) = ("warren".to_string(), gen_token());
            store.add_user(&u, &p)?;
            printed_login = Some((u, p));
        }
        _ => {}
    }
    print_startup(
        &store,
        tls_on,
        fingerprint.as_deref(),
        args.public_node_addr.as_deref(),
        printed_login,
    )?;

    let admin = match (args.admin_listen, args.admin_token) {
        (Some(l), Some(t)) => Some((l, t)),
        (Some(_), None) => anyhow::bail!("--admin-listen requires --admin-token"),
        _ => None,
    };

    // UDP relay shares the proxy address (UDP namespace), so clients send UDP
    // datagrams to the same host:port they use for the proxy. Best-effort: if the
    // bind fails, UDP ASSOCIATE is simply unavailable.
    let udp_relay = match UdpSocket::bind(&args.proxy_listen).await {
        Ok(u) => Some(u),
        Err(e) => {
            tracing::warn!(error = %e, addr = %args.proxy_listen, "UDP relay bind failed; UDP ASSOCIATE disabled");
            None
        }
    };

    let cfg = HubConfig {
        store,
        tls,
        admin,
        public_node_addr: args.public_node_addr,
        public_proxy_addr: args.public_proxy_addr,
        fingerprint,
        udp_relay,
    };
    run_with_listeners(node_listener, proxy_listener, cfg).await
}

pub async fn run_with_listeners(
    node_listener: TcpListener,
    proxy_listener: TcpListener,
    cfg: HubConfig,
) -> Result<()> {
    let HubConfig {
        store,
        tls,
        admin,
        public_node_addr,
        public_proxy_addr,
        fingerprint,
        udp_relay,
    } = cfg;
    let hub = Arc::new(Hub {
        store,
        tls,
        public_node_addr,
        public_proxy_addr,
        fingerprint,
        udp_relay: udp_relay.map(Arc::new),
        udp_assoc: Mutex::new(HashMap::new()),
        nodes: Mutex::new(Vec::new()),
        pending: Mutex::new(HashMap::new()),
        rr: AtomicUsize::new(0),
        conn_seq: AtomicU64::new(1),
        sessions: Mutex::new(HashMap::new()),
        node_bytes: Mutex::new(HashMap::new()),
        user_bytes: Mutex::new(HashMap::new()),
        node_stats: Mutex::new(HashMap::new()),
        node_conns: StdMutex::new(HashMap::new()),
        last_auth: StdMutex::new(HashMap::new()),
    });

    // Central UDP demux: one socket, datagrams routed to an association by the
    // client's source IP (set when the association is created).
    if let Some(relay) = hub.udp_relay.clone() {
        let h = hub.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            loop {
                let (n, src) = match relay.recv_from(&mut buf).await {
                    Ok(v) => v,
                    // UDP recv errors are typically per-datagram (e.g. an ICMP
                    // port-unreachable surfaced on the next recv). Keep serving;
                    // never kill the relay for every association on one error.
                    Err(_) => continue,
                };
                // Look up by client IP, then drop the map lock before parsing or
                // copying so one association cannot stall the others.
                let assoc = {
                    let m = h.udp_assoc.lock().await;
                    m.get(&src.ip())
                        .map(|a| (a.to_node.clone(), a.client_src.clone(), a.meter.clone()))
                };
                let Some((to_node, client_src, meter)) = assoc else {
                    continue;
                };
                // Lock the association to the first client socket we hear from and
                // ignore datagrams from any other source (anti-injection: an
                // off-path spoof of the client IP cannot hijack the reply path).
                {
                    let mut cs = client_src.lock().unwrap();
                    match *cs {
                        None => *cs = Some(src),
                        Some(known) if known != src => continue,
                        _ => {}
                    }
                }
                // Mark activity (idle timer) and enforce the per-second rate cap.
                if !meter.lock().unwrap().allow(Instant::now()) {
                    continue;
                }
                if let Some((host, port, off)) = socks5::parse_udp_header(&buf[..n]) {
                    // Bounded queue: drop on overflow rather than grow hub memory.
                    let _ = to_node.try_send(UdpDatagram {
                        host,
                        port,
                        data: buf[off..n].to_vec(),
                    });
                }
            }
        });
    }

    tracing::info!(
        node_listen = ?node_listener.local_addr().ok(),
        proxy_listen = ?proxy_listener.local_addr().ok(),
        auth = hub.store.auth_required().unwrap_or(false),
        tls = hub.tls.is_some(),
        admin = admin.is_some(),
        "warren hub up"
    );

    if let Some((listen, token)) = admin {
        let ctx = AdminCtx {
            hub: hub.clone(),
            token,
        };
        tokio::spawn(async move {
            if let Err(e) = run_admin(listen, ctx).await {
                tracing::error!(error = %e, "admin server stopped");
            }
        });
    }

    let h1 = hub.clone();
    let node_task = tokio::spawn(async move {
        loop {
            match node_listener.accept().await {
                Ok((stream, peer)) => {
                    let _ = stream.set_nodelay(true); // proxied splice: no Nagle stalls
                    let h = h1.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_node_conn(stream, peer.ip(), h).await {
                            tracing::debug!(%peer, error = %e, "node conn ended");
                        }
                    });
                }
                Err(e) => tracing::warn!(error = %e, "node accept failed"),
            }
        }
    });

    let h2 = hub.clone();
    let proxy_task = tokio::spawn(async move {
        loop {
            match proxy_listener.accept().await {
                Ok((stream, peer)) => {
                    let _ = stream.set_nodelay(true); // proxied splice: no Nagle stalls
                    let h = h2.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_client(stream, h).await {
                            tracing::debug!(%peer, error = %e, "client conn ended");
                        }
                    });
                }
                Err(e) => tracing::warn!(error = %e, "client accept failed"),
            }
        }
    });

    tokio::try_join!(node_task, proxy_task)?;
    Ok(())
}

/// Max concurrent node links from a single source IP. A node holds exactly one
/// link, so anything beyond a small reconnect overlap is abuse.
const MAX_NODE_CONNS_PER_IP: u32 = 16;

/// RAII per-IP node-connection counter. Increments on acquire, decrements (and
/// prunes the map entry) on drop, so the count tracks live links exactly.
struct NodeConnGuard {
    hub: Arc<Hub>,
    ip: IpAddr,
}

impl NodeConnGuard {
    fn acquire(hub: &Arc<Hub>, ip: IpAddr) -> Option<Self> {
        let mut m = hub.node_conns.lock().unwrap();
        let c = m.entry(ip).or_insert(0);
        if *c >= MAX_NODE_CONNS_PER_IP {
            return None;
        }
        *c += 1;
        Some(Self {
            hub: hub.clone(),
            ip,
        })
    }
}

impl Drop for NodeConnGuard {
    fn drop(&mut self) {
        let mut m = self.hub.node_conns.lock().unwrap();
        if let Some(c) = m.get_mut(&self.ip) {
            *c -= 1;
            if *c == 0 {
                m.remove(&self.ip);
            }
        }
    }
}

async fn handle_node_conn(stream: TcpStream, peer_ip: IpAddr, hub: Arc<Hub>) -> Result<()> {
    let Some(_conn_guard) = NodeConnGuard::acquire(&hub, peer_ip) else {
        tracing::debug!(%peer_ip, "too many node connections from this IP; dropping");
        return Ok(());
    };
    let conn = match &hub.tls {
        Some(acceptor) => Conn::ServerTls(acceptor.accept(stream).await?),
        None => Conn::Plain(stream),
    };
    // The node<->hub link is one yamux connection. The hub only accepts streams;
    // the first is the control channel, each later one is a data stream for a
    // pending dial. The driver continuously polls the connection (yamux only
    // makes progress while polled) and forwards inbound streams over the channel,
    // so the connection stays live even while we read Hello / splice. Its handle
    // aborts the driver on drop, so every return below tears the link down.
    let (mut stream_rx, _driver) = mux::server(conn);

    // Enroll on the first inbound stream (the control channel), bounded so a
    // peer that finishes TLS then stalls cannot pin the driver task + socket.
    // Any early exit (timeout, decode error, reject, wrong first stream) drops
    // the driver handle and tears down; only a Welcome falls through.
    let enrolled = timeout(ENROLL_TIMEOUT, async {
        let mut ctrl = match stream_rx.recv().await {
            Some(s) => s,
            None => return Ok(None),
        };
        let hello = match read_msg::<_, Greeting>(&mut ctrl).await? {
            Greeting::Control(h) => h,
            Greeting::Data(_) => {
                tracing::warn!("node's first stream was not control; dropping connection");
                return Ok(None);
            }
        };
        // On reject/pending the reply is already sent inside enroll_node.
        match enroll_node(&hello, &mut ctrl, &hub).await? {
            Some(id) => Ok::<_, anyhow::Error>(Some((ctrl, hello, id))),
            None => Ok(None),
        }
    })
    .await;
    let (ctrl, hello, node_id) = match enrolled {
        Ok(Ok(Some(v))) => v,
        // timeout, decode error, reject/pending, or non-control first stream:
        // dropping _driver aborts the connection.
        _ => return Ok(()),
    };
    let pubkey = identity::fingerprint(&hello.pubkey);
    // Fired by a key revocation to force this link down (see Hub::disconnect_node).
    let shutdown = Arc::new(Notify::new());

    let (tx, mut rx) = mpsc::unbounded_channel::<HubToNode>();
    let info = Arc::new(StdMutex::new(NodeReport::default()));
    // The node dialed out from its residential connection, so the control-conn
    // source IP IS its public egress IP (same NAT as proxied traffic). Set it
    // directly: reliable and instant, no dependency on the node reaching an
    // external IP-echo service (the old node-side ip-api self-report failed on
    // networks that block its plaintext call). Geo is resolved hub-side below.
    info.lock().unwrap().ip = Some(peer_ip.to_string());
    // The node's group is the name of the enroll token it presents (the node
    // sends it on every connect from its join code), so a reconnect keeps the
    // group without persisting it separately. None if no/unknown/unnamed token.
    let group = hello
        .token
        .as_deref()
        .and_then(|t| hub.store.token_name(t).ok().flatten());
    // Unique per-connection sequence so a reconnect (same id) and the dead-peer
    // reap of its stale predecessor cannot evict each other.
    let conn_seq = hub.next_conn_id();
    hub.add_node(NodeEntry {
        id: node_id.clone(),
        name: hello.node_name.clone(),
        since: unix_now(),
        tx: tx.clone(),
        fails: Arc::new(AtomicU32::new(0)),
        last_fail: Arc::new(AtomicI64::new(0)),
        host_fails: Arc::new(StdMutex::new(HashMap::new())),
        in_flight: Arc::new(AtomicU32::new(0)),
        protocol_version: hello.protocol_version,
        agent_version: hello.agent_version.clone(),
        info: info.clone(),
        group,
        conn_seq,
        pubkey,
        shutdown: shutdown.clone(),
    })
    .await;
    tracing::info!(node = %node_id.0, ip = %peer_ip, ver = hello.protocol_version, "node enrolled");

    // Tell new-enough nodes our version so they can self-update. Gated: a node
    // older than the version that introduced HubVersion would fail to decode it.
    if hello.protocol_version >= HUB_VERSION_MIN {
        let _ = tx.send(HubToNode::HubVersion {
            version: env!("CARGO_PKG_VERSION").to_string(),
        });
    }

    // Geo-locate the node's IP from the hub (which has reliable connectivity),
    // rather than relying on the node to do it. Best-effort.
    let geo_info = info.clone();
    tokio::spawn(async move {
        if let Some((country, city)) = geo_lookup(&peer_ip.to_string()).await {
            let mut r = geo_info.lock().unwrap();
            r.country = country;
            r.city = city;
        }
    });

    let (mut rd, mut wr) = split(ctrl);

    let ping_tx = tx.clone();
    let pinger = tokio::spawn(async move {
        let mut n = 0u64;
        loop {
            tokio::time::sleep(Duration::from_secs(20)).await;
            n += 1;
            if ping_tx.send(HubToNode::Ping { nonce: n }).is_err() {
                break;
            }
        }
    });
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if write_msg(&mut wr, &msg).await.is_err() {
                break;
            }
        }
    });

    // Control receive loop runs as its own task: read_msg is not cancel-safe, so
    // it cannot share a tokio::select with the data-accept loop below. A dead peer
    // (no traffic for NODE_READ_TIMEOUT, ~3 missed pings) ends it and tears down.
    let ctrl_info = info.clone();
    let ctrl_hub = hub.clone();
    // Returns WHY it ended so the disconnect can be logged with a reason: a
    // dead-peer timeout (no NodeToHub traffic for LINK_READ_TIMEOUT, ~3 missed
    // pings) or a control read/decode error.
    let mut ctrl_loop = tokio::spawn(async move {
        loop {
            match timeout(mux::LINK_READ_TIMEOUT, read_msg::<_, NodeToHub>(&mut rd)).await {
                Ok(Ok(msg)) => match msg {
                    NodeToHub::Pong { .. } => {}
                    NodeToHub::DialFailed { conn_id, reason } => {
                        tracing::debug!(conn_id, %reason, "node reported dial failed");
                        let _ = ctrl_hub.take_pending(conn_id).await;
                    }
                    NodeToHub::Info { .. } => {
                        // Ignored: ip/country/city are hub-authoritative. The hub
                        // sets `ip` from the control-conn peer address (the node's
                        // real egress) and resolves geo itself. Trusting the
                        // node-reported values here would let a malicious node spoof
                        // its country and capture `user-region-*` traffic it should
                        // not serve.
                    }
                    NodeToHub::Latency { ms } => {
                        ctrl_info.lock().unwrap().latency_ms = Some(ms);
                    }
                },
                Ok(Err(_)) => return "control connection lost",
                Err(_) => return "stopped responding (~75s, no pong)",
            }
        }
    });

    // Data-accept loop: each later inbound stream is a data connection for a
    // pending dial. Streams arrive from the driver via the channel; recv returns
    // None when the driver stops (connection closed).
    let accept_hub = hub.clone();
    let accept = async {
        while let Some(stream) = stream_rx.recv().await {
            let h = accept_hub.clone();
            tokio::spawn(async move {
                let _ = handle_data_stream(stream, h).await;
            });
        }
    };

    // Whichever ends first (connection died, or control loop hit error/timeout)
    // tears the node down. The reason is logged so it shows in the hub log.
    let reason = tokio::select! {
        _ = accept => "connection closed",
        r = &mut ctrl_loop => r.unwrap_or("control task ended"),
        _ = shutdown.notified() => "key revoked",
    };

    hub.remove_node(&node_id, conn_seq).await;
    pinger.abort();
    writer.abort();
    ctrl_loop.abort();
    // _driver's DriverHandle aborts the yamux driver when it drops here.
    tracing::info!(node = %node_id.0, reason, "node disconnected");
    Ok(())
}

/// A data stream's first frame identifies which pending dial it serves. The read
/// is bounded so a stream opened but never identified cannot leak this task.
async fn handle_data_stream(mut s: MuxStream, hub: Arc<Hub>) -> Result<()> {
    match timeout(DATA_HELLO_TIMEOUT, read_msg::<_, Greeting>(&mut s)).await?? {
        Greeting::Data(dh) => handle_data(dh, s, hub).await,
        // A second control stream is unexpected; ignore it.
        Greeting::Control(_) => Ok(()),
    }
}

/// Validate a node's Hello on the control stream and reply. Returns the node's
/// id on success (Welcome sent); None if rejected or left pending (reply sent).
async fn enroll_node(
    hello: &Hello,
    conn: &mut MuxStream,
    hub: &Arc<Hub>,
) -> Result<Option<NodeId>> {
    // 0. Accept a RANGE of node versions [MIN..=CURRENT] so an additive bump does
    //    not force every node to reinstall at once. Newer hub messages are gated
    //    per node by the version recorded below. Only reject genuinely
    //    incompatible nodes (older than the shared-framing floor, or from the
    //    future), with a clear reason instead of a confusing decode failure.
    let nv = hello.protocol_version;
    if !(warren_proto::MIN_PROTOCOL_VERSION..=warren_proto::PROTOCOL_VERSION).contains(&nv) {
        let _ = write_msg(
            conn,
            &HelloReply::Reject {
                reason: format!(
                    "protocol version unsupported: hub speaks {} (min {}), node speaks {}; update the node",
                    warren_proto::PROTOCOL_VERSION,
                    warren_proto::MIN_PROTOCOL_VERSION,
                    nv
                ),
            },
        )
        .await;
        return Ok(None);
    }

    // 1. The node must own its key and present a fresh timestamp.
    if !identity::verify_auth(&hello.pubkey, hello.timestamp, &hello.signature) {
        let _ = write_msg(
            conn,
            &HelloReply::Reject {
                reason: "bad signature".into(),
            },
        )
        .await;
        return Ok(None);
    }
    let skew = (unix_now() - hello.timestamp as i64).abs();
    if skew > 120 {
        let _ = write_msg(
            conn,
            &HelloReply::Reject {
                reason: "stale timestamp".into(),
            },
        )
        .await;
        return Ok(None);
    }

    let pk_hex = identity::fingerprint(&hello.pubkey);

    // 1a. Replay guard: each new Hello must carry a strictly newer timestamp than
    //     the last one accepted for this key. A captured Hello (same timestamp)
    //     is therefore rejected, so it cannot be replayed inside the 120s skew
    //     window to displace the live link. (Lock released before the await.)
    let replayed = {
        let mut seen = hub.last_auth.lock().unwrap();
        match seen.get(&pk_hex) {
            Some(&prev) if hello.timestamp <= prev => true,
            _ => {
                seen.insert(pk_hex.clone(), hello.timestamp);
                false
            }
        }
    };
    if replayed {
        let _ = write_msg(
            conn,
            &HelloReply::Reject {
                reason: "stale or replayed timestamp".into(),
            },
        )
        .await;
        return Ok(None);
    }

    // 1b. Names disambiguate routing (`user+name`), so a device may not claim a
    //     name already approved for a DIFFERENT key (its own key is excluded, so
    //     reconnects are fine).
    if hub
        .store
        .node_name_taken_by_other(&hello.node_name, &pk_hex)
        .unwrap_or(false)
    {
        let _ = write_msg(
            conn,
            &HelloReply::Reject {
                reason: format!(
                    "node name '{}' is already in use by another device; pass a unique --name",
                    hello.node_name
                ),
            },
        )
        .await;
        return Ok(None);
    }

    // 2. Enrollment: an already-approved key, or a valid token (auto-approve),
    //    otherwise record as pending and ask for admin approval.
    let code = identity::short_code(&hello.pubkey);
    if !hub.store.is_node_approved(&pk_hex).unwrap_or(false) {
        let by_token = hello
            .token
            .as_deref()
            .map(|t| hub.store.token_valid(t).unwrap_or(false))
            .unwrap_or(false);
        if by_token {
            let _ = hub.store.approve_node(&pk_hex, &hello.node_name);
            tracing::info!(%code, name = %hello.node_name, "node approved via token");
        } else {
            let _ = hub.store.add_pending(&pk_hex, &hello.node_name, &code);
            let _ = write_msg(conn, &HelloReply::Pending { code: code.clone() }).await;
            tracing::info!(%code, name = %hello.node_name, "node pending admin approval");
            return Ok(None);
        }
    }

    let node_id = NodeId(format!("{}-{}", hello.node_name, code));
    write_msg(
        conn,
        &HelloReply::Welcome {
            node_id: node_id.clone(),
        },
    )
    .await?;
    Ok(Some(node_id))
}

async fn handle_data(dh: DataHello, conn: MuxStream, hub: Arc<Hub>) -> Result<()> {
    match hub.take_pending(dh.conn_id).await {
        Some((nonce, tx)) if nonce == dh.nonce => {
            let _ = tx.send(conn);
        }
        Some((nonce, tx)) => {
            // Right conn_id, wrong nonce: not the node we dialed. Drop it and
            // put the waker back so the real node's data stream can still arrive.
            tracing::warn!(conn_id = dh.conn_id, "data stream nonce mismatch; dropped");
            hub.pending.lock().await.insert(dh.conn_id, (nonce, tx));
        }
        None => tracing::debug!(conn_id = dh.conn_id, "data conn with no pending dial"),
    }
    Ok(())
}

fn host_fail_count(m: &StdMutex<HashMap<String, u32>>, host: &str) -> u32 {
    m.lock().unwrap().get(host).copied().unwrap_or(0)
}

/// Routing rank for a node on a given host; lower sorts first. Order: nodes
/// healthy globally before unhealthy, then healthy-on-this-host before not, then
/// least in-flight, then fewest host failures, then fewest global failures. So a
/// recovered/healthy idle node is preferred and an unhealthy one is tried only as
/// a last resort. Pure (the booleans gate the tiers) so the ordering is testable.
fn route_rank(eff_fails: u32, host_fails: u32, in_flight: u32) -> (bool, bool, u32, u32, u32) {
    (
        eff_fails >= UNHEALTHY_AT,
        host_fails >= UNHEALTHY_AT,
        in_flight,
        host_fails,
        eff_fails,
    )
}

/// How the client wants its request routed, parsed from the proxy username.
#[derive(Debug, PartialEq, Clone, Default)]
enum Route {
    /// `user`: any healthy device (round-robin + failover).
    #[default]
    Pool,
    /// `user+name`: pin one named device (no fallback if offline).
    Device(String),
    /// `user-session-KEY`: stick to one device for this session key.
    Session(String),
    /// `user-region-COUNTRY`: route through a device whose reported geo country
    /// matches (case-insensitive). No fallback to other regions if none match.
    Region(String),
    /// `user-group-NAME`: route through any node that joined with the enroll token
    /// named NAME (case-insensitive). No fallback if none match.
    Group(String),
}

/// Parse `(base_user, route)` from a proxy username. `-session-`, `-region-`, and
/// `-group-` are checked first (all rejected at user creation, so they cannot
/// collide), then `+`.
fn parse_route(user: &str) -> (&str, Route) {
    if let Some((base, key)) = user.split_once("-session-") {
        if !key.is_empty() {
            return (base, Route::Session(key.to_string()));
        }
    }
    if let Some((base, region)) = user.split_once("-region-") {
        if !region.is_empty() {
            return (base, Route::Region(region.to_string()));
        }
    }
    if let Some((base, group)) = user.split_once("-group-") {
        if !group.is_empty() {
            return (base, Route::Group(group.to_string()));
        }
    }
    match user.split_once('+') {
        Some((base, name)) if !name.is_empty() => (base, Route::Device(name.to_string())),
        _ => (user, Route::Pool),
    }
}

/// What the client spoke, and (for plain HTTP) the rewritten request to send
/// to the target once a data connection is open.
enum Mode {
    Connect,
    Socks5,
    Http(Vec<u8>),
}

async fn handle_client(mut client: TcpStream, hub: Arc<Hub>) -> Result<()> {
    // Detect the client protocol by peeking the first byte (0x05 = SOCKS5).
    let mut peek = [0u8; 1];
    let peeked = match timeout(PROXY_HANDSHAKE_TIMEOUT, client.peek(&mut peek)).await {
        Ok(r) => r?,
        Err(_) => return Ok(()), // connected but sent nothing in time
    };
    if peeked == 0 {
        tracing::debug!(peer = ?client.peer_addr().ok(), "client closed before sending any bytes");
        return Ok(());
    }

    let need_auth = hub.store.auth_required().unwrap_or(false);
    let store = hub.store.clone();
    // The proxy username encodes how to route (user / user+device / user-session-K).
    // Auth runs inside the SOCKS5/HTTP paths, so capture the route there and read
    // it back once the target is known. The password is checked against the base.
    let route_cell: Arc<StdMutex<Route>> = Arc::new(StdMutex::new(Route::Pool));
    let route_cap = route_cell.clone();
    let user_cell: Arc<StdMutex<String>> = Arc::new(StdMutex::new(String::new()));
    let user_cap = user_cell.clone();
    let verify = move |u: &str, p: &str| {
        let (base, route) = parse_route(u);
        *route_cap.lock().unwrap() = route;
        *user_cap.lock().unwrap() = base.to_string();
        store.check_user(base, p).unwrap_or(false)
    };

    let (mode, host, port) = if peek[0] == 0x05 {
        let verify_opt: Option<&socks5::Verifier> = if need_auth { Some(&verify) } else { None };
        let neg = match timeout(
            PROXY_HANDSHAKE_TIMEOUT,
            socks5::negotiate(&mut client, verify_opt),
        )
        .await
        {
            Ok(r) => r?,
            Err(_) => return Ok(()),
        };
        match neg {
            Some(socks5::Socks5Req::Connect { host, port }) => (Mode::Socks5, host, port),
            Some(socks5::Socks5Req::UdpAssociate) => {
                // UDP relay: a different shape from the TCP splice, so it gets its
                // own handler. Route/user were captured by the auth closure above.
                let route = std::mem::take(&mut *route_cell.lock().unwrap());
                let base_user = std::mem::take(&mut *user_cell.lock().unwrap());
                return handle_udp_associate(client, hub, route, base_user).await;
            }
            None => return Ok(()), // rejected, reply already sent
        }
    } else {
        let req = match timeout(PROXY_HANDSHAKE_TIMEOUT, proxy::read_request(&mut client)).await {
            Ok(r) => r?,
            Err(_) => return Ok(()),
        };
        if need_auth {
            let ok = req
                .authorization()
                .and_then(proxy::parse_basic)
                .map(|(u, p)| verify(&u, &p))
                .unwrap_or(false);
            if !ok {
                proxy::write_auth_required(&mut client).await?;
                return Ok(());
            }
        }
        if req.method.eq_ignore_ascii_case("CONNECT") {
            // host:port, bracketed IPv6, or bare host (defaults to port 443).
            match proxy::split_connect_target(&req.target) {
                Some((h, port)) => (Mode::Connect, h, port),
                None => {
                    proxy::write_bad_gateway(&mut client).await?;
                    return Ok(());
                }
            }
        } else {
            // plain HTTP: absolute-URI request, forwarded in origin form.
            match proxy::forward_request(&req) {
                Some((h, p, head)) => (Mode::Http(head), h, p),
                None => {
                    proxy::write_bad_gateway(&mut client).await?;
                    return Ok(());
                }
            }
        }
    };

    let route = std::mem::take(&mut *route_cell.lock().unwrap());
    let base_user = std::mem::take(&mut *user_cell.lock().unwrap());
    // Resolve candidate nodes. Device/sticky pin to one device; pool and
    // fresh-session use the whole pool. session_key is Some when we should record
    // which device served, so the next request in that session sticks to it.
    let (nodes, session_key) = match route {
        Route::Device(name) => (hub.snapshot(Some(&name)).await, None),
        Route::Region(country) => (hub.snapshot_region(&country).await, None),
        Route::Group(group) => (hub.snapshot_group(&group).await, None),
        Route::Session(key) => match hub.session_node(&key).await {
            Some(name) => {
                let pinned = hub.snapshot(Some(&name)).await;
                if pinned.is_empty() {
                    (hub.snapshot(None).await, Some(key)) // stuck node gone: re-pick
                } else {
                    (pinned, Some(key))
                }
            }
            None => (hub.snapshot(None).await, Some(key)),
        },
        Route::Pool => (hub.snapshot(None).await, None),
    };
    if nodes.is_empty() {
        // No devices connected, or a pinned device (user+name) is offline. Reject
        // rather than silently leaving from a different device than asked.
        reject(&mut client, &mode).await?;
        return Ok(());
    }

    // Round-robin order, but try healthy nodes before unhealthy ones. The sort
    // is stable, so rotation is preserved within each health tier.
    let start = hub.rr.fetch_add(1, Ordering::Relaxed);
    let mut order: Vec<usize> = (0..nodes.len())
        .map(|i| (start + i) % nodes.len())
        .collect();
    // Prefer nodes fresh on THIS host: fewest host-fails first, unhealthy-on-host
    // last, global fails as the final tie-break. Stable sort keeps round-robin
    // order within a tier.
    let now = unix_now();
    order.sort_by_key(|&i| {
        let node_fails = effective_fails(
            nodes[i].3.load(Ordering::Relaxed),
            nodes[i].7.load(Ordering::Relaxed),
            now,
        );
        let node_host_fails = host_fail_count(&nodes[i].4, &host);
        let node_in_flight = nodes[i].5.load(Ordering::Relaxed);
        route_rank(node_fails, node_host_fails, node_in_flight)
    });

    let mut uncapped_idx: Vec<usize> = Vec::new();
    let mut capped_idx: Vec<usize> = Vec::new();
    for &i in &order {
        if nodes[i].5.load(Ordering::Relaxed) >= MAX_INFLIGHT_PER_NODE {
            capped_idx.push(i);
        } else {
            uncapped_idx.push(i);
        }
    }

    for idx in uncapped_idx.iter().chain(capped_idx.iter()).copied() {
        let (node_id, node_name, node_tx, fails, host_fails, in_flight, _ver, last_fail) =
            &nodes[idx];
        let _g = mux::InFlightGuard::new(in_flight.clone());

        let conn_id = hub.next_conn_id();
        let nonce = rand::random::<u64>();
        let (otx, orx) = oneshot::channel::<MuxStream>();
        hub.insert_pending(conn_id, nonce, otx).await;

        if node_tx
            .send(HubToNode::Dial {
                conn_id,
                nonce,
                host: host.clone(),
                port,
            })
            .is_err()
        {
            hub.take_pending(conn_id).await;
            fails.fetch_add(1, Ordering::Relaxed);
            last_fail.store(unix_now(), Ordering::Relaxed);
            hub.record_dial(&node_id.0, false, Some("dial send failed"))
                .await;
            continue;
        }

        match timeout(Duration::from_secs(15), orx).await {
            Ok(Ok(mut data)) => {
                fails.store(0, Ordering::Relaxed);
                host_fails.lock().unwrap().remove(&host);
                hub.record_dial(&node_id.0, true, None).await;
                // This device served the request: stick the session to it.
                if let Some(key) = &session_key {
                    hub.set_session(key, node_name).await;
                }
                match &mode {
                    Mode::Connect => proxy::write_established(&mut client).await?,
                    Mode::Socks5 => socks5::write_reply(&mut client, socks5::REP_SUCCESS).await?,
                    Mode::Http(head) => data.write_all(head).await?,
                }
                if let Ok((a, b)) = copy_bidirectional(&mut client, &mut data).await {
                    hub.add_bytes(&node_id.0, &base_user, a + b).await;
                }
                return Ok(());
            }
            _ => {
                hub.take_pending(conn_id).await;
                fails.fetch_add(1, Ordering::Relaxed);
                last_fail.store(unix_now(), Ordering::Relaxed);
                hub.record_dial(&node_id.0, false, Some("dial timed out"))
                    .await;
                {
                    let mut hf = host_fails.lock().unwrap();
                    *hf.entry(host.clone()).or_insert(0) += 1;
                    // Bound the map: a node used for many one-off hosts would
                    // otherwise accumulate an entry per host. Only counts at or
                    // above the unhealthy threshold affect routing, so drop the
                    // rest once the map gets large.
                    if hf.len() > HOST_FAIL_CAP {
                        hf.retain(|_, &mut c| c >= UNHEALTHY_AT);
                    }
                }
                tracing::debug!(node = %node_id.0, %host, "dial failed on host, failing over");
                continue;
            }
        }
    }

    reject(&mut client, &mode).await?;
    Ok(())
}

async fn reject(client: &mut TcpStream, mode: &Mode) -> std::io::Result<()> {
    match mode {
        Mode::Connect | Mode::Http(_) => proxy::write_bad_gateway(client).await,
        Mode::Socks5 => socks5::write_reply(client, socks5::REP_GENERAL_FAILURE).await,
    }
}

/// SOCKS5 UDP ASSOCIATE: pick a node, open a UDP relay stream to it, register the
/// association (keyed by client IP), and bridge client datagrams to/from the
/// node's residential UDP egress. The association lives until the client's TCP
/// control connection closes. Datagrams ride the shared relay socket; the central
/// demux task routes inbound ones here by client IP.
async fn handle_udp_associate(
    mut client: TcpStream,
    hub: Arc<Hub>,
    route: Route,
    base_user: String,
) -> Result<()> {
    let _ = &base_user; // UDP byte metering not tracked per-user yet.
    let Some(relay) = hub.udp_relay.clone() else {
        socks5::write_reply(&mut client, socks5::REP_GENERAL_FAILURE).await?;
        return Ok(());
    };

    // Pick a node (route-aware), preferring healthy + least-loaded. UDP has no
    // per-datagram failover; one node carries the whole association. Only nodes
    // new enough to speak UDP (the 7th tuple field is the protocol version) are
    // eligible, so an older node in the pool is skipped for UDP but still serves TCP.
    // Sticky sessions over UDP pin to the device that served the session, same as
    // TCP, falling back to the full pool if that device is gone. Without this,
    // `user-session-KEY` would silently spread datagrams across the pool.
    let session_key = if let Route::Session(k) = &route {
        Some(k.clone())
    } else {
        None
    };
    let nodes: Vec<NodeSnap> = match &route {
        Route::Device(name) => hub.snapshot(Some(name)).await,
        Route::Region(country) => hub.snapshot_region(country).await,
        Route::Group(group) => hub.snapshot_group(group).await,
        Route::Session(key) => match hub.session_node(key).await {
            Some(name) => {
                let pinned = hub.snapshot(Some(&name)).await;
                if pinned.is_empty() {
                    hub.snapshot(None).await
                } else {
                    pinned
                }
            }
            None => hub.snapshot(None).await,
        },
        Route::Pool => hub.snapshot(None).await,
    };
    let udp: Vec<&NodeSnap> = nodes.iter().filter(|n| n.6 >= UDP_MIN_VERSION).collect();
    // Match TCP health semantics: failures older than the health window stop
    // counting (effective_fails), so a recovered node is not wrongly excluded.
    let now = unix_now();
    let chosen = udp
        .iter()
        .copied()
        .filter(|n| effective_fails(n.3.load(Ordering::Relaxed), n.7.load(Ordering::Relaxed), now) < UNHEALTHY_AT)
        .min_by_key(|n| n.5.load(Ordering::Relaxed))
        .or_else(|| {
            udp.iter()
                .copied()
                .min_by_key(|n| n.5.load(Ordering::Relaxed))
        });
    let Some((node_id, node_name, node_tx, _fails, _host_fails, in_flight, _ver, _last_fail)) =
        chosen
    else {
        socks5::write_reply(&mut client, socks5::REP_GENERAL_FAILURE).await?;
        return Ok(());
    };
    // Stick the session to the chosen device so later requests (TCP or UDP) on the
    // same session key land on it too.
    if let Some(key) = &session_key {
        hub.set_session(key, node_name).await;
    }
    let node_id = node_id.0.clone();
    let _g = mux::InFlightGuard::new(in_flight.clone());

    // Open the relay stream on the node (same conn_id/nonce handshake as a dial).
    let conn_id = hub.next_conn_id();
    let nonce = rand::random::<u64>();
    let (otx, orx) = oneshot::channel::<MuxStream>();
    hub.insert_pending(conn_id, nonce, otx).await;
    if node_tx.send(HubToNode::UdpOpen { conn_id, nonce }).is_err() {
        hub.take_pending(conn_id).await;
        socks5::write_reply(&mut client, socks5::REP_GENERAL_FAILURE).await?;
        return Ok(());
    }
    let stream = match timeout(Duration::from_secs(15), orx).await {
        Ok(Ok(s)) => s,
        _ => {
            hub.take_pending(conn_id).await;
            socks5::write_reply(&mut client, socks5::REP_GENERAL_FAILURE).await?;
            return Ok(());
        }
    };
    tracing::debug!(node = %node_id, "udp associate");

    // Tell the client where to send datagrams BEFORE registering the association,
    // so a failed reply write cannot leak a map entry (the client only sends
    // datagrams after this reply, a full round trip away).
    let client_ip = client.peer_addr()?.ip();
    let bnd = udp_bnd_addr(&hub, &relay, &client);
    socks5::write_reply_addr(&mut client, socks5::REP_SUCCESS, bnd).await?;

    let (to_node, mut node_rx) = mpsc::channel::<UdpDatagram>(UDP_QUEUE_DEPTH);
    let client_src = Arc::new(StdMutex::new(None::<SocketAddr>));
    let meter = Arc::new(StdMutex::new(AssocMeter::new(Instant::now())));
    hub.udp_assoc.lock().await.insert(
        client_ip,
        UdpAssoc {
            to_node,
            client_src: client_src.clone(),
            meter: meter.clone(),
        },
    );

    let (mut srd, mut swr) = split(stream);

    // client -> node: drain the channel (fed by the central demux) to the stream.
    let writer = tokio::spawn(async move {
        while let Some(dg) = node_rx.recv().await {
            if write_msg(&mut swr, &dg).await.is_err() {
                break;
            }
        }
    });

    // node -> client: read replies, wrap in the SOCKS5 UDP header, send via the
    // relay to the client's learned source address.
    let relay_tx = relay.clone();
    let csrc = client_src.clone();
    let mut reader = tokio::spawn(async move {
        while let Ok(dg) = read_msg::<_, UdpDatagram>(&mut srd).await {
            let dest = *csrc.lock().unwrap();
            if let Some(ca) = dest {
                if let Some(pkt) = socks5::wrap_udp(&dg.host, dg.port, &dg.data) {
                    let _ = relay_tx.send_to(&pkt, ca).await;
                }
            }
        }
    });

    // The association ends when ANY of: the client closes its TCP control conn,
    // the node relay stream dies (reader returns), or it goes idle past the
    // timeout (client vanished without closing). None of these leak the entry.
    let mut ctrl = [0u8; 1];
    tokio::select! {
        _ = async { while client.read(&mut ctrl).await.unwrap_or(0) > 0 {} } => {}
        _ = &mut reader => {}
        _ = async {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                if meter.lock().unwrap().last_seen.elapsed() > UDP_IDLE_TIMEOUT {
                    break;
                }
            }
        } => {}
    }

    hub.udp_assoc.lock().await.remove(&client_ip);
    writer.abort();
    reader.abort();
    Ok(())
}

/// BND address handed to the client for UDP datagrams: the configured public
/// proxy address if it parses (the relay is reachable there), else the local
/// address the client connected to with the relay socket's port.
fn udp_bnd_addr(hub: &Hub, relay: &UdpSocket, client: &TcpStream) -> SocketAddr {
    if let Some(pa) = &hub.public_proxy_addr {
        if let Ok(sa) = pa.parse::<SocketAddr>() {
            return sa;
        }
    }
    let port = relay.local_addr().map(|a| a.port()).unwrap_or(0);
    let ip = client
        .local_addr()
        .map(|a| a.ip())
        .unwrap_or(IpAddr::from([0u8, 0, 0, 0]));
    SocketAddr::new(ip, port)
}

/// Geo-locate a node IP from the hub (best-effort). Plaintext ip-api.com query;
/// the hub has reliable connectivity even when the node does not. Returns
/// (country, city); None on any failure.
async fn geo_lookup(ip: &str) -> Option<(Option<String>, Option<String>)> {
    let connect = TcpStream::connect("ip-api.com:80");
    let mut s = timeout(Duration::from_secs(8), connect).await.ok()?.ok()?;
    let req = format!(
        "GET /line/{ip}?fields=status,country,city HTTP/1.1\r\nHost: ip-api.com\r\nConnection: close\r\nUser-Agent: warren\r\n\r\n"
    );
    s.write_all(req.as_bytes()).await.ok()?;
    let mut buf = Vec::new();
    timeout(Duration::from_secs(8), s.read_to_end(&mut buf))
        .await
        .ok()?
        .ok()?;
    let text = String::from_utf8_lossy(&buf);
    let body = text.split("\r\n\r\n").nth(1)?;
    let mut lines = body.lines().map(str::trim).filter(|l| !l.is_empty());
    // fields=status,country,city -> first line is "success" or "fail".
    if lines.next()? != "success" {
        return None;
    }
    let country = lines.next().map(str::to_string).filter(|s| !s.is_empty());
    let city = lines.next().map(str::to_string).filter(|s| !s.is_empty());
    Some((country, city))
}

pub async fn enroll(args: EnrollArgs) -> Result<()> {
    let token = gen_token();
    let store = Store::open(&args.db).with_context(|| format!("open db {}", args.db))?;
    store.add_token(&token, &args.name)?;
    println!(
        "enroll token for '{}': {}  (added to {})",
        args.name, token, args.db
    );
    println!("join node:  warren node run --hub <hub-host:7000> --token {token}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Admin API + dashboard
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct AdminCtx {
    hub: Arc<Hub>,
    token: String,
}

#[derive(Serialize)]
struct NodeInfo {
    id: String,
    name: String,
    fails: u32,
    since: i64,
    ip: Option<String>,
    country: Option<String>,
    city: Option<String>,
    latency_ms: Option<u32>,
    bytes: u64,
    success_rate: Option<f64>,
    dials: u32,
    last_error: Option<String>,
    version: String,
    group: Option<String>,
}

#[derive(Serialize)]
struct TokenInfo {
    token: String,
    name: String,
    created: i64,
    /// One-paste join code for this token, or null if the hub does not know its
    /// public address yet (set --public-node-addr).
    join_code: Option<String>,
}

#[derive(Deserialize)]
struct NameReq {
    name: String,
}

#[derive(Serialize)]
struct TokenResp {
    token: String,
}

#[derive(Deserialize)]
struct UserReq {
    username: String,
    password: String,
}

/// Constant-time byte equality: ORs every byte difference so the comparison takes
/// the same time whether the mismatch is in the first byte or the last. A plain
/// `==` short-circuits on the first differing byte, leaking (via timing) how much
/// of a guessed admin token is correct. Length is not secret, so an early return
/// on length mismatch is fine.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// HTTP Basic auth: any username, password must equal the admin token. Compared
/// in constant time so the token cannot be recovered by timing.
fn authed(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(crate::proxy::parse_basic)
        .map(|(_, pass)| ct_eq(pass.as_bytes(), token.as_bytes()))
        .unwrap_or(false)
}

/// Guard for state-changing admin routes: valid Basic-auth token AND a custom
/// `X-Warren-Admin` header. The dashboard's own fetch() sets the header; a
/// cross-site page cannot (a custom header forces a CORS preflight the hub never
/// approves), so a malicious site holding the browser's cached Basic creds still
/// cannot drive a mutation. Non-browser clients (curl) just pass the header.
fn admin_mutate_ok(headers: &HeaderMap, token: &str) -> Result<(), StatusCode> {
    if !authed(headers, token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    if !headers.contains_key("x-warren-admin") {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(())
}

/// 401 with a Basic challenge, so a browser shows its login prompt.
fn unauthorized() -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        StatusCode::UNAUTHORIZED,
        [(
            axum::http::header::WWW_AUTHENTICATE,
            "Basic realm=\"warren admin\"",
        )],
    )
        .into_response()
}

/// Dashboard page, gated by Basic auth (nothing renders to the public).
async fn dashboard(State(ctx): State<AdminCtx>, headers: HeaderMap) -> axum::response::Response {
    use axum::response::IntoResponse;
    if !authed(&headers, &ctx.token) {
        return unauthorized();
    }
    Html(crate::admin_ui::DASHBOARD).into_response()
}

fn ise<E: std::fmt::Display>(e: E) -> StatusCode {
    tracing::warn!(error = %e, "admin api error");
    StatusCode::INTERNAL_SERVER_ERROR
}

async fn run_admin(listen: String, ctx: AdminCtx) -> Result<()> {
    let app = Router::new()
        .route("/", get(dashboard))
        .route("/api/info", get(api_info))
        .route("/api/nodes", get(api_nodes))
        .route("/api/tokens", get(api_list_tokens).post(api_create_token))
        .route(
            "/api/tokens/:token",
            delete(api_delete_token).patch(api_rename_token),
        )
        .route("/api/users", get(api_list_users).post(api_create_user))
        .route("/api/users/:username", delete(api_delete_user))
        .route("/api/pending", get(api_list_pending))
        .route("/api/pending/:pubkey/approve", post(api_approve_pending))
        .route("/api/pending/:pubkey", delete(api_deny_pending))
        .route("/api/node-keys", get(api_list_node_keys))
        .route("/api/node-keys/:pubkey", delete(api_delete_node_key))
        .route("/metrics", get(api_metrics))
        .route("/api/logs", get(api_logs))
        .with_state(ctx);
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("bind admin {listen}"))?;
    tracing::info!(%listen, "admin API + dashboard up");
    axum::serve(listener, app).await?;
    Ok(())
}

#[derive(Serialize)]
struct InfoJson {
    node_addr: Option<String>,
    proxy_addr: Option<String>,
    fingerprint: Option<String>,
    tls: bool,
    proxy_user: Option<String>,
    /// Password of the displayed proxy user, so the dashboard's copy commands are
    /// runnable as-is. The dashboard is admin-gated, so this is no more exposed
    /// than the enroll/admin tokens already shown.
    proxy_pass: Option<String>,
    install_url: String,
    /// Hub binary version, shown in the dashboard header.
    version: String,
}

async fn api_info(
    State(ctx): State<AdminCtx>,
    headers: HeaderMap,
) -> Result<Json<InfoJson>, StatusCode> {
    if !authed(&headers, &ctx.token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let (proxy_user, proxy_pass) = match ctx.hub.store.first_user().ok().flatten() {
        Some((u, p)) => (Some(u), Some(p)),
        None => (None, None),
    };
    Ok(Json(InfoJson {
        node_addr: ctx.hub.public_node_addr.clone(),
        proxy_addr: ctx.hub.public_proxy_addr.clone(),
        fingerprint: ctx.hub.fingerprint.clone(),
        tls: ctx.hub.tls.is_some(),
        proxy_user,
        proxy_pass,
        install_url: "https://raw.githubusercontent.com/doedja/warren/main/install.sh".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    }))
}

async fn api_nodes(
    State(ctx): State<AdminCtx>,
    headers: HeaderMap,
) -> Result<Json<Vec<NodeInfo>>, StatusCode> {
    if !authed(&headers, &ctx.token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(Json(ctx.hub.list_node_info().await))
}

/// Recent hub log lines (bounded ring) for the dashboard's log pane. Same admin
/// token as the rest of the admin server.
async fn api_logs(
    State(ctx): State<AdminCtx>,
    headers: HeaderMap,
) -> Result<Json<Vec<String>>, StatusCode> {
    if !authed(&headers, &ctx.token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(Json(crate::logbuf::recent()))
}

/// Prometheus text exposition of hub state (gated by the admin token, like the
/// rest of the admin server). Built from counters already tracked.
async fn api_metrics(
    State(ctx): State<AdminCtx>,
    headers: HeaderMap,
) -> Result<String, StatusCode> {
    if !authed(&headers, &ctx.token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let h = &ctx.hub;
    let nodes = h.list_node_info().await;
    let user_bytes = h.user_bytes.lock().await.clone();
    let udp_assocs = h.udp_assoc.lock().await.len();
    // Label values here are node ids / usernames (alnum + '-' + '_'); escape the
    // few metachars Prometheus cares about to stay well-formed regardless.
    let esc = |s: &str| {
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', " ")
    };
    let mut o = String::new();
    o.push_str("# HELP warren_nodes_online Connected nodes.\n# TYPE warren_nodes_online gauge\n");
    o.push_str(&format!("warren_nodes_online {}\n", nodes.len()));
    o.push_str("# HELP warren_udp_associations Active UDP associations.\n# TYPE warren_udp_associations gauge\n");
    o.push_str(&format!("warren_udp_associations {udp_assocs}\n"));
    o.push_str("# HELP warren_node_bytes_total Bytes relayed per node.\n# TYPE warren_node_bytes_total counter\n");
    for n in &nodes {
        o.push_str(&format!(
            "warren_node_bytes_total{{node=\"{}\"}} {}\n",
            esc(&n.id),
            n.bytes
        ));
    }
    o.push_str("# HELP warren_node_dials_total Dial attempts per node.\n# TYPE warren_node_dials_total counter\n");
    o.push_str("# HELP warren_node_fails Consecutive dial failures per node.\n# TYPE warren_node_fails gauge\n");
    for n in &nodes {
        o.push_str(&format!(
            "warren_node_dials_total{{node=\"{}\"}} {}\n",
            esc(&n.id),
            n.dials
        ));
        o.push_str(&format!(
            "warren_node_fails{{node=\"{}\"}} {}\n",
            esc(&n.id),
            n.fails
        ));
    }
    o.push_str("# HELP warren_user_bytes_total Bytes relayed per proxy user.\n# TYPE warren_user_bytes_total counter\n");
    for (user, bytes) in &user_bytes {
        o.push_str(&format!(
            "warren_user_bytes_total{{user=\"{}\"}} {}\n",
            esc(user),
            bytes
        ));
    }
    Ok(o)
}

async fn api_list_tokens(
    State(ctx): State<AdminCtx>,
    headers: HeaderMap,
) -> Result<Json<Vec<TokenInfo>>, StatusCode> {
    if !authed(&headers, &ctx.token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let rows = ctx.hub.store.list_tokens().map_err(ise)?;
    let node_addr = ctx.hub.public_node_addr.clone();
    let tls = ctx.hub.tls.is_some();
    let fp = ctx.hub.fingerprint.clone();
    Ok(Json(
        rows.into_iter()
            .map(|(token, name, created)| {
                let join_code = node_addr
                    .as_deref()
                    .map(|addr| crate::joincode::encode(addr, tls, fp.as_deref(), Some(&token)));
                TokenInfo {
                    token,
                    name,
                    created,
                    join_code,
                }
            })
            .collect(),
    ))
}

async fn api_create_token(
    State(ctx): State<AdminCtx>,
    headers: HeaderMap,
    Json(req): Json<NameReq>,
) -> Result<Json<TokenResp>, StatusCode> {
    admin_mutate_ok(&headers, &ctx.token)?;
    if Store::validate_token_name(&req.name).is_err() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let token = gen_token();
    ctx.hub.store.add_token(&token, &req.name).map_err(ise)?;
    Ok(Json(TokenResp { token }))
}

async fn api_delete_token(
    State(ctx): State<AdminCtx>,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> Result<StatusCode, StatusCode> {
    admin_mutate_ok(&headers, &ctx.token)?;
    ctx.hub.store.delete_token(&token).map_err(ise)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Rename a token, which relabels its group. The label is re-read from the token
/// name only when a node (re)connects, so already-connected nodes keep the old
/// group label until they reconnect; new joins and reconnects get the new one.
async fn api_rename_token(
    State(ctx): State<AdminCtx>,
    headers: HeaderMap,
    Path(token): Path<String>,
    Json(req): Json<NameReq>,
) -> Result<StatusCode, StatusCode> {
    admin_mutate_ok(&headers, &ctx.token)?;
    if Store::validate_token_name(&req.name).is_err() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let renamed = ctx.hub.store.rename_token(&token, &req.name).map_err(ise)?;
    if renamed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

#[derive(Serialize)]
struct UserInfo {
    username: String,
    bytes: u64,
}

async fn api_list_users(
    State(ctx): State<AdminCtx>,
    headers: HeaderMap,
) -> Result<Json<Vec<UserInfo>>, StatusCode> {
    if !authed(&headers, &ctx.token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let bytes = ctx.hub.user_bytes.lock().await.clone();
    Ok(Json(
        ctx.hub
            .store
            .list_users()
            .map_err(ise)?
            .into_iter()
            .map(|username| {
                let b = bytes.get(&username).copied().unwrap_or(0);
                UserInfo { username, bytes: b }
            })
            .collect(),
    ))
}

async fn api_create_user(
    State(ctx): State<AdminCtx>,
    headers: HeaderMap,
    Json(req): Json<UserReq>,
) -> Result<StatusCode, StatusCode> {
    admin_mutate_ok(&headers, &ctx.token)?;
    if req.username.is_empty() || req.password.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    ctx.hub
        .store
        .add_user(&req.username, &req.password)
        .map_err(ise)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn api_delete_user(
    State(ctx): State<AdminCtx>,
    headers: HeaderMap,
    Path(username): Path<String>,
) -> Result<StatusCode, StatusCode> {
    admin_mutate_ok(&headers, &ctx.token)?;
    ctx.hub.store.delete_user(&username).map_err(ise)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
struct PendingInfo {
    pubkey: String,
    name: String,
    code: String,
    first_seen: i64,
}

#[derive(Serialize)]
struct NodeKeyInfo {
    pubkey: String,
    name: String,
    approved_at: i64,
}

async fn api_list_pending(
    State(ctx): State<AdminCtx>,
    headers: HeaderMap,
) -> Result<Json<Vec<PendingInfo>>, StatusCode> {
    if !authed(&headers, &ctx.token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let rows = ctx.hub.store.list_pending().map_err(ise)?;
    Ok(Json(
        rows.into_iter()
            .map(|(pubkey, name, code, first_seen)| PendingInfo {
                pubkey,
                name,
                code,
                first_seen,
            })
            .collect(),
    ))
}

async fn api_approve_pending(
    State(ctx): State<AdminCtx>,
    headers: HeaderMap,
    Path(pubkey): Path<String>,
) -> Result<StatusCode, StatusCode> {
    admin_mutate_ok(&headers, &ctx.token)?;
    // Carry over the name from the pending entry if present.
    let name = ctx
        .hub
        .store
        .list_pending()
        .map_err(ise)?
        .into_iter()
        .find(|(pk, ..)| pk == &pubkey)
        .map(|(_, name, ..)| name)
        .unwrap_or_else(|| "node".to_string());
    ctx.hub.store.approve_node(&pubkey, &name).map_err(ise)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn api_deny_pending(
    State(ctx): State<AdminCtx>,
    headers: HeaderMap,
    Path(pubkey): Path<String>,
) -> Result<StatusCode, StatusCode> {
    admin_mutate_ok(&headers, &ctx.token)?;
    ctx.hub.store.delete_pending(&pubkey).map_err(ise)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn api_list_node_keys(
    State(ctx): State<AdminCtx>,
    headers: HeaderMap,
) -> Result<Json<Vec<NodeKeyInfo>>, StatusCode> {
    if !authed(&headers, &ctx.token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let rows = ctx.hub.store.list_nodes().map_err(ise)?;
    Ok(Json(
        rows.into_iter()
            .map(|(pubkey, name, approved_at)| NodeKeyInfo {
                pubkey,
                name,
                approved_at,
            })
            .collect(),
    ))
}

async fn api_delete_node_key(
    State(ctx): State<AdminCtx>,
    headers: HeaderMap,
    Path(pubkey): Path<String>,
) -> Result<StatusCode, StatusCode> {
    admin_mutate_ok(&headers, &ctx.token)?;
    ctx.hub.store.delete_node(&pubkey).map_err(ise)?;
    // Reconnection is already blocked by the deleted key; also tear down any link
    // still live so a captured session stops proxying immediately.
    let dropped = ctx.hub.disconnect_node(&pubkey).await;
    if dropped > 0 {
        tracing::info!(%pubkey, count = dropped, "revoked node key; tore down live link(s)");
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Add `e` to the node list, displacing any existing entry for the same node id.
/// A node holds exactly one live link; when it reconnects (same id, fresh
/// conn_seq) the new connection supersedes the old, so a half-open duplicate
/// never lingers in the routable set waiting to be reaped alongside the live one.
fn displace_and_push(nodes: &mut Vec<NodeEntry>, e: NodeEntry) {
    nodes.retain(|n| n.id != e.id);
    nodes.push(e);
}

/// Remove the entry matching exactly this (id, conn_seq). Returns true if another
/// entry for the same id is still present, so the caller keeps that node's per-id
/// counters. Matching on conn_seq (not id alone) is what stops a stale connection
/// reap from evicting a live reconnected connection that shares the id.
fn remove_conn(nodes: &mut Vec<NodeEntry>, id: &NodeId, conn_seq: u64) -> bool {
    nodes.retain(|n| !(n.id == *id && n.conn_seq == conn_seq));
    nodes.iter().any(|n| n.id == *id)
}

#[cfg(test)]
mod tests {
    use super::{
        admin_mutate_ok, ct_eq, displace_and_push, effective_fails, parse_route, remove_conn,
        route_rank, success_pct, NodeEntry, NodeId, NodeReport, Route, HEALTH_WINDOW_SECS,
        UNHEALTHY_AT,
    };
    use axum::http::{HeaderMap, HeaderValue, StatusCode};
    use base64::Engine as _;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicI64, AtomicU32};
    use std::sync::{Arc, Mutex};
    use tokio::sync::{mpsc, Notify};

    #[test]
    fn admin_mutate_guard_requires_auth_and_csrf_header() {
        let tok = "secret";
        let basic = base64::engine::general_purpose::STANDARD.encode("admin:secret");
        let auth = || HeaderValue::from_str(&format!("Basic {basic}")).unwrap();

        // Both present -> ok.
        let mut h = HeaderMap::new();
        h.insert("authorization", auth());
        h.insert("x-warren-admin", HeaderValue::from_static("1"));
        assert!(admin_mutate_ok(&h, tok).is_ok());

        // Authed but no CSRF header -> 403 (blocks cross-site requests carrying
        // the browser's cached Basic creds).
        let mut h = HeaderMap::new();
        h.insert("authorization", auth());
        assert_eq!(admin_mutate_ok(&h, tok).unwrap_err(), StatusCode::FORBIDDEN);

        // CSRF header but bad/no token -> 401.
        let mut h = HeaderMap::new();
        h.insert("x-warren-admin", HeaderValue::from_static("1"));
        assert_eq!(
            admin_mutate_ok(&h, tok).unwrap_err(),
            StatusCode::UNAUTHORIZED
        );
    }

    fn entry(id: &str, conn_seq: u64) -> NodeEntry {
        let (tx, _rx) = mpsc::unbounded_channel();
        NodeEntry {
            id: NodeId(id.to_string()),
            name: "n".into(),
            since: 0,
            tx,
            fails: Arc::new(AtomicU32::new(0)),
            last_fail: Arc::new(AtomicI64::new(0)),
            host_fails: Arc::new(Mutex::new(HashMap::new())),
            in_flight: Arc::new(AtomicU32::new(0)),
            protocol_version: 6,
            agent_version: "0.4.6".into(),
            info: Arc::new(Mutex::new(NodeReport::default())),
            group: None,
            conn_seq,
            pubkey: String::new(),
            shutdown: Arc::new(Notify::new()),
        }
    }

    #[test]
    fn route_rank_orders_healthy_least_loaded_first() {
        // (eff_fails, host_fails, in_flight)
        let healthy_idle = route_rank(0, 0, 0);
        let healthy_busy = route_rank(0, 0, 5);
        let host_unhealthy = route_rank(0, UNHEALTHY_AT, 0);
        let globally_unhealthy = route_rank(UNHEALTHY_AT, 0, 0);
        // Healthy idle beats healthy-but-busy (least in-flight wins within a tier).
        assert!(healthy_idle < healthy_busy);
        // Any healthy node beats one that is unhealthy on this host.
        assert!(healthy_busy < host_unhealthy);
        // Host-unhealthy (still globally ok) is tried before a globally-unhealthy node.
        assert!(host_unhealthy < globally_unhealthy);
        // A shuffled set sorts healthy-first, globally-unhealthy-last.
        let mut v = vec![
            globally_unhealthy,
            healthy_busy,
            host_unhealthy,
            healthy_idle,
        ];
        v.sort();
        assert_eq!(
            v,
            vec![
                healthy_idle,
                healthy_busy,
                host_unhealthy,
                globally_unhealthy
            ]
        );
    }

    #[test]
    fn ct_eq_matches_only_identical_bytes() {
        assert!(ct_eq(b"s3cret-token", b"s3cret-token"));
        assert!(!ct_eq(b"s3cret-token", b"s3cret-toke!")); // same length, last byte differs
        assert!(!ct_eq(b"short", b"longer-token")); // length mismatch
        assert!(ct_eq(b"", b"")); // empty equals empty
    }

    #[test]
    fn effective_fails_expires_and_resets() {
        let w = HEALTH_WINDOW_SECS;
        // No failures: always healthy, regardless of timestamps.
        assert_eq!(effective_fails(0, 0, 1_000), 0);
        // Recent failures (within the window) still count toward unhealthy.
        assert_eq!(effective_fails(4, 1_000, 1_000), 4);
        assert_eq!(effective_fails(4, 1_000, 1_000 + w), 4);
        // Once the last failure is older than the window, it stops counting, so a
        // node that simply stopped failing (or went idle) reads as healthy again.
        assert_eq!(effective_fails(4, 1_000, 1_000 + w + 1), 0);
        // A backwards clock (now < last_fail) must not underflow; keep counting.
        assert_eq!(effective_fails(4, 1_000, 500), 4);
    }

    #[test]
    fn add_displaces_stale_same_id() {
        // A node reconnecting (same id, new conn_seq) must not leave a stale
        // duplicate behind: the newest connection replaces the old entry.
        let mut nodes = Vec::new();
        displace_and_push(&mut nodes, entry("yuki", 1));
        displace_and_push(&mut nodes, entry("yuki", 2));
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].conn_seq, 2);
    }

    #[test]
    fn add_keeps_distinct_ids() {
        let mut nodes = Vec::new();
        displace_and_push(&mut nodes, entry("yuki", 1));
        displace_and_push(&mut nodes, entry("kukky", 1));
        assert_eq!(nodes.len(), 2);
    }

    #[test]
    fn reap_stale_keeps_live_reconnect() {
        // The core bug: two connections briefly overlap for one id. Reaping the
        // stale one (seq 1) must leave the live one (seq 2) routable, and report
        // that the id is still present so its byte/stat counters survive.
        let mut nodes = vec![entry("yuki", 1), entry("yuki", 2)];
        let id_still_present = remove_conn(&mut nodes, &NodeId("yuki".into()), 1);
        assert!(id_still_present);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].conn_seq, 2);
    }

    #[test]
    fn reap_after_displace_is_noop_for_live() {
        // Real flow: seq 2 already displaced seq 1 on enroll. When seq 1's dead
        // socket finally times out and reaps itself, it must touch nothing.
        let mut nodes = vec![entry("yuki", 2)];
        let id_still_present = remove_conn(&mut nodes, &NodeId("yuki".into()), 1);
        assert!(id_still_present);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].conn_seq, 2);
    }

    #[test]
    fn reap_last_connection_reports_gone() {
        // When the only connection for an id is removed, the id is fully gone so
        // the caller knows to purge its per-id byte/stat counters.
        let mut nodes = vec![entry("yuki", 1)];
        let id_still_present = remove_conn(&mut nodes, &NodeId("yuki".into()), 1);
        assert!(!id_still_present);
        assert!(nodes.is_empty());
    }

    #[test]
    fn success_pct_no_dials_is_none() {
        // Fresh node, no dials yet: dash in the dashboard, never a divide-by-zero.
        assert_eq!(success_pct(0, 0), None);
        assert_eq!(success_pct(5, 0), None); // total drives it, not ok
    }

    #[test]
    fn success_pct_math() {
        assert_eq!(success_pct(1, 1), Some(100.0));
        assert_eq!(success_pct(0, 1), Some(0.0));
        assert_eq!(success_pct(1, 2), Some(50.0));
        assert_eq!(success_pct(98, 100), Some(98.0));
    }

    #[test]
    fn route_parsing() {
        assert_eq!(parse_route("me"), ("me", Route::Pool));
        assert_eq!(
            parse_route("me+phone"),
            ("me", Route::Device("phone".into()))
        );
        // empty selector is ignored (whole pool)
        assert_eq!(parse_route("me+"), ("me+", Route::Pool));
        // only the first + splits; later ones are part of the device name
        assert_eq!(parse_route("me+a+b"), ("me", Route::Device("a+b".into())));
        // sticky session
        assert_eq!(
            parse_route("me-session-ab12"),
            ("me", Route::Session("ab12".into()))
        );
        // -session- takes priority over +
        assert_eq!(
            parse_route("me-session-k+x"),
            ("me", Route::Session("k+x".into()))
        );
        // region tag
        assert_eq!(
            parse_route("me-region-Indonesia"),
            ("me", Route::Region("Indonesia".into()))
        );
        // group tag (enroll-token name)
        assert_eq!(
            parse_route("me-group-residential"),
            ("me", Route::Group("residential".into()))
        );
        // empty group selector is ignored (whole pool)
        assert_eq!(parse_route("me-group-"), ("me-group-", Route::Pool));
    }
}
