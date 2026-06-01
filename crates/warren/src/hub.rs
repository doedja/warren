//! Hub mode: client-facing HTTP CONNECT proxy + node control plane.
//!
//! Node-facing listener accepts two kinds of connection, distinguished by the
//! first framed [`Greeting`]:
//!   - Control: a node enrolls, then we keep the link for Dial/Ping/Pong.
//!   - Data: a fresh socket tagged with a conn_id, handed to the waiting
//!     client handler to splice.
//!
//! The node link is TLS by default (self-signed cert, fingerprint printed at
//! startup; `--no-tls` drops to plaintext for localhost). The client-facing
//! proxy auto-detects HTTP CONNECT, SOCKS5, and plain-HTTP on one port.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
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
use tokio::io::{copy_bidirectional, split, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::{timeout, Duration};
use tokio_rustls::TlsAcceptor;

use warren_proto::{DataHello, Greeting, Hello, HelloReply, HubToNode, NodeId, NodeToHub};

use crate::conn::Conn;
use crate::identity;
use crate::proxy;
use crate::socks5;
use crate::store::Store;
use crate::tls;
use crate::wire::{read_msg, write_msg};

/// A node is skipped (tried only as last resort) once its consecutive-failure
/// count reaches this.
const UNHEALTHY_AT: u32 = 3;

/// Cap on distinct hosts tracked per node before low-count entries are dropped.
const HOST_FAIL_CAP: usize = 256;

/// How long a sticky session keeps mapping to the same node after its last use.
const SESSION_TTL: Duration = Duration::from_secs(600);
/// Cap on tracked sessions before expired ones are swept.
const SESSION_CAP: usize = 10_000;

#[derive(Args, Debug)]
pub struct HubArgs {
    /// Node-facing listen address (control + data connections).
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
    fails: Arc<AtomicU32>,
    /// Per-target-host consecutive failures. Lets routing prefer nodes that are
    /// still "fresh" on a given host (transport-level reachability). App-level
    /// blocks like 429/403 are inside the TLS tunnel and not observable here.
    host_fails: Arc<StdMutex<HashMap<String, u32>>>,
    /// Public egress IP + geo, self-reported by the node (best-effort, may be
    /// empty until the first report arrives).
    info: Arc<StdMutex<NodeReport>>,
}

#[derive(Default, Clone)]
struct NodeReport {
    ip: Option<String>,
    country: Option<String>,
    city: Option<String>,
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
}

struct Hub {
    store: Arc<Store>,
    tls: Option<TlsAcceptor>,
    public_node_addr: Option<String>,
    public_proxy_addr: Option<String>,
    fingerprint: Option<String>,
    nodes: Mutex<Vec<NodeEntry>>,
    /// conn_id -> (expected data-conn nonce, waker for the client). The nonce
    /// authenticates the data connection: only the node we sent the Dial to
    /// knows it, so a guessed conn_id alone cannot hijack the splice.
    pending: Mutex<HashMap<u64, (u64, oneshot::Sender<Conn>)>>,
    rr: AtomicUsize,
    conn_seq: AtomicU64,
    /// Sticky sessions: session key -> (node name, last use). A `user-session-K`
    /// proxy username keeps requests on the same device while it stays healthy.
    sessions: Mutex<HashMap<String, (String, Instant)>>,
}

impl Hub {
    fn next_conn_id(&self) -> u64 {
        self.conn_seq.fetch_add(1, Ordering::Relaxed)
    }
    /// Snapshot of connected nodes (id, name, tx, fails, host_fails). When `sel`
    /// is `Some(name)`, only nodes with that exact device name are returned, so a
    /// client can route through one specific device (and gets nothing, not a
    /// fallback, if it is offline).
    // `map_or(true, ...)` keeps the MSRV at 1.74; `Option::is_none_or` is 1.82+.
    #[allow(clippy::type_complexity, clippy::unnecessary_map_or)]
    async fn snapshot(
        &self,
        sel: Option<&str>,
    ) -> Vec<(
        NodeId,
        String,
        mpsc::UnboundedSender<HubToNode>,
        Arc<AtomicU32>,
        Arc<StdMutex<HashMap<String, u32>>>,
    )> {
        self.nodes
            .lock()
            .await
            .iter()
            .filter(|n| sel.map_or(true, |s| n.name == s))
            .map(|n| {
                (
                    n.id.clone(),
                    n.name.clone(),
                    n.tx.clone(),
                    n.fails.clone(),
                    n.host_fails.clone(),
                )
            })
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
    async fn add_node(&self, e: NodeEntry) {
        self.nodes.lock().await.push(e);
    }
    async fn remove_node(&self, id: &NodeId) {
        self.nodes.lock().await.retain(|n| &n.id != id);
    }
    async fn insert_pending(&self, id: u64, nonce: u64, tx: oneshot::Sender<Conn>) {
        self.pending.lock().await.insert(id, (nonce, tx));
    }
    async fn take_pending(&self, id: u64) -> Option<(u64, oneshot::Sender<Conn>)> {
        self.pending.lock().await.remove(&id)
    }
    async fn list_node_info(&self) -> Vec<NodeInfo> {
        self.nodes
            .lock()
            .await
            .iter()
            .map(|n| {
                let r = n.info.lock().unwrap().clone();
                NodeInfo {
                    id: n.id.0.clone(),
                    name: n.name.clone(),
                    fails: n.fails.load(Ordering::Relaxed),
                    since: n.since,
                    ip: r.ip,
                    country: r.country,
                    city: r.city,
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

    let cfg = HubConfig {
        store,
        tls,
        admin,
        public_node_addr: args.public_node_addr,
        public_proxy_addr: args.public_proxy_addr,
        fingerprint,
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
    } = cfg;
    let hub = Arc::new(Hub {
        store,
        tls,
        public_node_addr,
        public_proxy_addr,
        fingerprint,
        nodes: Mutex::new(Vec::new()),
        pending: Mutex::new(HashMap::new()),
        rr: AtomicUsize::new(0),
        conn_seq: AtomicU64::new(1),
        sessions: Mutex::new(HashMap::new()),
    });

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
                    let h = h1.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_node_conn(stream, h).await {
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

async fn handle_node_conn(stream: TcpStream, hub: Arc<Hub>) -> Result<()> {
    let mut conn = match &hub.tls {
        Some(acceptor) => Conn::ServerTls(acceptor.accept(stream).await?),
        None => Conn::Plain(stream),
    };
    let greeting: Greeting = read_msg(&mut conn).await?;
    match greeting {
        Greeting::Control(hello) => handle_control(hello, conn, hub).await,
        Greeting::Data(dh) => handle_data(dh, conn, hub).await,
    }
}

async fn handle_control(hello: Hello, mut conn: Conn, hub: Arc<Hub>) -> Result<()> {
    // 0. Refuse a node speaking a different protocol version, with a clear
    //    reason instead of a confusing decode failure later.
    if hello.protocol_version != warren_proto::PROTOCOL_VERSION {
        let _ = write_msg(
            &mut conn,
            &HelloReply::Reject {
                reason: format!(
                    "protocol version mismatch: hub speaks {}, node speaks {}; update the node",
                    warren_proto::PROTOCOL_VERSION,
                    hello.protocol_version
                ),
            },
        )
        .await;
        return Ok(());
    }

    // 1. The node must own its key and present a fresh timestamp.
    if !identity::verify_auth(&hello.pubkey, hello.timestamp, &hello.signature) {
        let _ = write_msg(
            &mut conn,
            &HelloReply::Reject {
                reason: "bad signature".into(),
            },
        )
        .await;
        return Ok(());
    }
    let skew = (unix_now() - hello.timestamp as i64).abs();
    if skew > 120 {
        let _ = write_msg(
            &mut conn,
            &HelloReply::Reject {
                reason: "stale timestamp".into(),
            },
        )
        .await;
        return Ok(());
    }

    // 2. Enrollment: an already-approved key, or a valid token (auto-approve),
    //    otherwise record as pending and ask for admin approval.
    let pk_hex = identity::fingerprint(&hello.pubkey);
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
            let _ = write_msg(&mut conn, &HelloReply::Pending { code: code.clone() }).await;
            tracing::info!(%code, name = %hello.node_name, "node pending admin approval");
            return Ok(());
        }
    }

    let node_id = NodeId(format!("{}-{}", hello.node_name, code));
    write_msg(
        &mut conn,
        &HelloReply::Welcome {
            node_id: node_id.clone(),
        },
    )
    .await?;
    tracing::info!(node = %node_id.0, "node enrolled");

    let (tx, mut rx) = mpsc::unbounded_channel::<HubToNode>();
    let info = Arc::new(StdMutex::new(NodeReport::default()));
    hub.add_node(NodeEntry {
        id: node_id.clone(),
        name: hello.node_name.clone(),
        since: unix_now(),
        tx: tx.clone(),
        fails: Arc::new(AtomicU32::new(0)),
        host_fails: Arc::new(StdMutex::new(HashMap::new())),
        info: info.clone(),
    })
    .await;

    let (mut rd, mut wr) = split(conn);

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

    let res: Result<()> = async {
        loop {
            let msg: NodeToHub = read_msg(&mut rd).await?;
            match msg {
                NodeToHub::Pong { .. } => {}
                NodeToHub::DialFailed { conn_id, reason } => {
                    tracing::debug!(node = %node_id.0, conn_id, %reason, "node reported dial failed");
                    let _ = hub.take_pending(conn_id).await;
                }
                NodeToHub::Info {
                    public_ip,
                    country,
                    city,
                } => {
                    *info.lock().unwrap() = NodeReport {
                        ip: public_ip,
                        country,
                        city,
                    };
                }
            }
        }
    }
    .await;

    hub.remove_node(&node_id).await;
    pinger.abort();
    writer.abort();
    tracing::info!(node = %node_id.0, "node disconnected");
    res
}

async fn handle_data(dh: DataHello, conn: Conn, hub: Arc<Hub>) -> Result<()> {
    match hub.take_pending(dh.conn_id).await {
        Some((nonce, tx)) if nonce == dh.nonce => {
            let _ = tx.send(conn);
        }
        Some((nonce, tx)) => {
            // Right conn_id, wrong nonce: not the node we dialed. Drop it and
            // put the waker back so the real node's data conn can still arrive.
            tracing::warn!(conn_id = dh.conn_id, "data conn nonce mismatch; dropped");
            hub.pending.lock().await.insert(dh.conn_id, (nonce, tx));
        }
        None => tracing::debug!(conn_id = dh.conn_id, "data conn with no pending dial"),
    }
    Ok(())
}

fn host_fail_count(m: &StdMutex<HashMap<String, u32>>, host: &str) -> u32 {
    m.lock().unwrap().get(host).copied().unwrap_or(0)
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
}

/// Parse `(base_user, route)` from a proxy username. `-session-` is checked first
/// (and rejected at user creation, so it cannot collide), then `+`.
fn parse_route(user: &str) -> (&str, Route) {
    if let Some((base, key)) = user.split_once("-session-") {
        if !key.is_empty() {
            return (base, Route::Session(key.to_string()));
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
    if client.peek(&mut peek).await? == 0 {
        return Ok(());
    }

    let need_auth = hub.store.auth_required().unwrap_or(false);
    let store = hub.store.clone();
    // The proxy username encodes how to route (user / user+device / user-session-K).
    // Auth runs inside the SOCKS5/HTTP paths, so capture the route there and read
    // it back once the target is known. The password is checked against the base.
    let route_cell: Arc<StdMutex<Route>> = Arc::new(StdMutex::new(Route::Pool));
    let route_cap = route_cell.clone();
    let verify = move |u: &str, p: &str| {
        let (base, route) = parse_route(u);
        *route_cap.lock().unwrap() = route;
        store.check_user(base, p).unwrap_or(false)
    };

    let (mode, host, port) = if peek[0] == 0x05 {
        let verify_opt: Option<&socks5::Verifier> = if need_auth { Some(&verify) } else { None };
        match socks5::negotiate(&mut client, verify_opt).await? {
            Some((h, p)) => (Mode::Socks5, h, p),
            None => return Ok(()), // rejected, reply already sent
        }
    } else {
        let req = proxy::read_request(&mut client).await?;
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
            // target is host:port
            match req.target.rsplit_once(':') {
                Some((h, p)) => match p.parse::<u16>() {
                    Ok(port) => (Mode::Connect, h.to_string(), port),
                    Err(_) => {
                        proxy::write_bad_gateway(&mut client).await?;
                        return Ok(());
                    }
                },
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
    // Resolve candidate nodes. Device/sticky pin to one device; pool and
    // fresh-session use the whole pool. session_key is Some when we should record
    // which device served, so the next request in that session sticks to it.
    let (nodes, session_key) = match route {
        Route::Device(name) => (hub.snapshot(Some(&name)).await, None),
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
    order.sort_by_key(|&i| {
        let hf = host_fail_count(&nodes[i].4, &host);
        let global = nodes[i].3.load(Ordering::Relaxed);
        (hf >= UNHEALTHY_AT, hf, global)
    });

    for idx in order {
        let (node_id, node_name, node_tx, fails, host_fails) = &nodes[idx];
        let conn_id = hub.next_conn_id();
        let nonce = rand::random::<u64>();
        let (otx, orx) = oneshot::channel::<Conn>();
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
            continue;
        }

        match timeout(Duration::from_secs(15), orx).await {
            Ok(Ok(mut data)) => {
                fails.store(0, Ordering::Relaxed);
                host_fails.lock().unwrap().remove(&host);
                // This device served the request: stick the session to it.
                if let Some(key) = &session_key {
                    hub.set_session(key, node_name).await;
                }
                match &mode {
                    Mode::Connect => proxy::write_established(&mut client).await?,
                    Mode::Socks5 => socks5::write_reply(&mut client, socks5::REP_SUCCESS).await?,
                    Mode::Http(head) => data.write_all(head).await?,
                }
                let _ = copy_bidirectional(&mut client, &mut data).await;
                return Ok(());
            }
            _ => {
                hub.take_pending(conn_id).await;
                fails.fetch_add(1, Ordering::Relaxed);
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

/// HTTP Basic auth: any username, password must equal the admin token.
fn authed(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(crate::proxy::parse_basic)
        .map(|(_, pass)| pass == token)
        .unwrap_or(false)
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
        .route("/api/tokens/:token", delete(api_delete_token))
        .route("/api/users", get(api_list_users).post(api_create_user))
        .route("/api/users/:username", delete(api_delete_user))
        .route("/api/pending", get(api_list_pending))
        .route("/api/pending/:pubkey/approve", post(api_approve_pending))
        .route("/api/pending/:pubkey", delete(api_deny_pending))
        .route("/api/node-keys", get(api_list_node_keys))
        .route("/api/node-keys/:pubkey", delete(api_delete_node_key))
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
    if !authed(&headers, &ctx.token) {
        return Err(StatusCode::UNAUTHORIZED);
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
    if !authed(&headers, &ctx.token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    ctx.hub.store.delete_token(&token).map_err(ise)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn api_list_users(
    State(ctx): State<AdminCtx>,
    headers: HeaderMap,
) -> Result<Json<Vec<String>>, StatusCode> {
    if !authed(&headers, &ctx.token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(Json(ctx.hub.store.list_users().map_err(ise)?))
}

async fn api_create_user(
    State(ctx): State<AdminCtx>,
    headers: HeaderMap,
    Json(req): Json<UserReq>,
) -> Result<StatusCode, StatusCode> {
    if !authed(&headers, &ctx.token) {
        return Err(StatusCode::UNAUTHORIZED);
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
    if !authed(&headers, &ctx.token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
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
    if !authed(&headers, &ctx.token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
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
    if !authed(&headers, &ctx.token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
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
    if !authed(&headers, &ctx.token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    ctx.hub.store.delete_node(&pubkey).map_err(ise)?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::{parse_route, Route};

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
    }
}
