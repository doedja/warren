//! Hub mode: client-facing HTTP CONNECT proxy + node control plane.
//!
//! Node-facing listener accepts two kinds of connection, distinguished by the
//! first framed [`Greeting`]:
//!   - Control: a node enrolls, then we keep the link for Dial/Ping/Pong.
//!   - Data: a fresh socket tagged with a conn_id, handed to the waiting
//!     client handler to splice.
//!
//! With `--tls` the node link is wrapped in TLS (self-signed cert, fingerprint
//! printed at startup). The client-facing proxy is always plain HTTP CONNECT.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::{Context, Result};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Html;
use axum::routing::{delete, get};
use axum::{Json, Router};
use clap::Args;
use serde::{Deserialize, Serialize};
use tokio::io::{copy_bidirectional, split};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::{timeout, Duration};
use tokio_rustls::TlsAcceptor;

use warren_proto::{DataHello, Greeting, Hello, HelloReply, HubToNode, NodeId, NodeToHub};

use crate::conn::Conn;
use crate::proxy;
use crate::socks5;
use crate::store::Store;
use crate::tls;
use crate::wire::{read_msg, write_msg};

/// A node is skipped (tried only as last resort) once its consecutive-failure
/// count reaches this.
const UNHEALTHY_AT: u32 = 3;

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
    /// Enable TLS on the node link (self-signed cert; fingerprint printed).
    #[arg(long, default_value_t = false)]
    pub tls: bool,
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
    tx: mpsc::UnboundedSender<HubToNode>,
    /// Consecutive dial failures (any target); reset to 0 on a successful dial.
    fails: Arc<AtomicU32>,
    /// Per-target-host consecutive failures. Lets routing prefer nodes that are
    /// still "fresh" on a given host (transport-level reachability). App-level
    /// blocks like 429/403 are inside the TLS tunnel and not observable here.
    host_fails: Arc<StdMutex<HashMap<String, u32>>>,
}

/// Runtime config independent of the CLI, so tests can drive the hub with
/// pre-bound listeners on ephemeral ports.
pub struct HubConfig {
    pub store: Arc<Store>,
    pub tls: Option<TlsAcceptor>,
    /// (listen addr, bearer token) for the admin API + dashboard.
    pub admin: Option<(String, String)>,
}

struct Hub {
    store: Arc<Store>,
    tls: Option<TlsAcceptor>,
    nodes: Mutex<Vec<NodeEntry>>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Conn>>>,
    rr: AtomicUsize,
    conn_seq: AtomicU64,
}

impl Hub {
    fn next_conn_id(&self) -> u64 {
        self.conn_seq.fetch_add(1, Ordering::Relaxed)
    }
    #[allow(clippy::type_complexity)]
    async fn snapshot(
        &self,
    ) -> Vec<(
        NodeId,
        mpsc::UnboundedSender<HubToNode>,
        Arc<AtomicU32>,
        Arc<StdMutex<HashMap<String, u32>>>,
    )> {
        self.nodes
            .lock()
            .await
            .iter()
            .map(|n| {
                (
                    n.id.clone(),
                    n.tx.clone(),
                    n.fails.clone(),
                    n.host_fails.clone(),
                )
            })
            .collect()
    }
    async fn add_node(&self, e: NodeEntry) {
        self.nodes.lock().await.push(e);
    }
    async fn remove_node(&self, id: &NodeId) {
        self.nodes.lock().await.retain(|n| &n.id != id);
    }
    async fn insert_pending(&self, id: u64, tx: oneshot::Sender<Conn>) {
        self.pending.lock().await.insert(id, tx);
    }
    async fn take_pending(&self, id: u64) -> Option<oneshot::Sender<Conn>> {
        self.pending.lock().await.remove(&id)
    }
    async fn list_node_info(&self) -> Vec<NodeInfo> {
        self.nodes
            .lock()
            .await
            .iter()
            .map(|n| NodeInfo {
                id: n.id.0.clone(),
                fails: n.fails.load(Ordering::Relaxed),
            })
            .collect()
    }
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

pub async fn run(args: HubArgs) -> Result<()> {
    let node_listener = TcpListener::bind(&args.listen)
        .await
        .with_context(|| format!("bind node listener {}", args.listen))?;
    let proxy_listener = TcpListener::bind(&args.proxy_listen)
        .await
        .with_context(|| format!("bind proxy listener {}", args.proxy_listen))?;

    let tls = if args.tls {
        let (acceptor, fingerprint) = match &args.tls_cert_dir {
            Some(dir) => tls::server_acceptor_from_dir(dir)?,
            None => tls::server_acceptor()?,
        };
        println!("warren hub TLS fingerprint: {fingerprint}");
        tracing::info!(%fingerprint,
            "TLS enabled on node link; join nodes with --tls --hub-fingerprint <fingerprint>");
        Some(acceptor)
    } else {
        tracing::warn!("node link is PLAINTEXT (no --tls); use only on localhost or a tailnet");
        None
    };

    let store = Arc::new(Store::open(&args.db).with_context(|| format!("open db {}", args.db))?);
    if let Some(tok) = &args.enroll_token {
        store.add_token(tok, "cli")?;
    }
    if let (Some(u), Some(p)) = (&args.proxy_user, &args.proxy_pass) {
        store.add_user(u, p)?;
    }

    let admin = match (args.admin_listen, args.admin_token) {
        (Some(l), Some(t)) => Some((l, t)),
        (Some(_), None) => anyhow::bail!("--admin-listen requires --admin-token"),
        _ => None,
    };

    let cfg = HubConfig { store, tls, admin };
    run_with_listeners(node_listener, proxy_listener, cfg).await
}

pub async fn run_with_listeners(
    node_listener: TcpListener,
    proxy_listener: TcpListener,
    cfg: HubConfig,
) -> Result<()> {
    let HubConfig { store, tls, admin } = cfg;
    let hub = Arc::new(Hub {
        store,
        tls,
        nodes: Mutex::new(Vec::new()),
        pending: Mutex::new(HashMap::new()),
        rr: AtomicUsize::new(0),
        conn_seq: AtomicU64::new(1),
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
    if !hub.store.token_valid(&hello.token).unwrap_or(false) {
        let _ = write_msg(
            &mut conn,
            &HelloReply::Reject {
                reason: "bad token".into(),
            },
        )
        .await;
        return Ok(());
    }
    let node_id = NodeId(format!("{}-{}", hello.node_name, hub.next_conn_id()));
    write_msg(
        &mut conn,
        &HelloReply::Welcome {
            node_id: node_id.clone(),
        },
    )
    .await?;
    tracing::info!(node = %node_id.0, "node enrolled");

    let (tx, mut rx) = mpsc::unbounded_channel::<HubToNode>();
    hub.add_node(NodeEntry {
        id: node_id.clone(),
        tx: tx.clone(),
        fails: Arc::new(AtomicU32::new(0)),
        host_fails: Arc::new(StdMutex::new(HashMap::new())),
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
        Some(tx) => {
            let _ = tx.send(conn);
        }
        None => tracing::debug!(conn_id = dh.conn_id, "data conn with no pending dial"),
    }
    Ok(())
}

fn host_fail_count(m: &StdMutex<HashMap<String, u32>>, host: &str) -> u32 {
    m.lock().unwrap().get(host).copied().unwrap_or(0)
}

#[derive(Clone, Copy)]
enum Proto {
    Http,
    Socks5,
}

async fn handle_client(mut client: TcpStream, hub: Arc<Hub>) -> Result<()> {
    // Detect the client protocol by peeking the first byte (0x05 = SOCKS5).
    let mut peek = [0u8; 1];
    if client.peek(&mut peek).await? == 0 {
        return Ok(());
    }

    let need_auth = hub.store.auth_required().unwrap_or(false);
    let store = hub.store.clone();
    let verify = move |u: &str, p: &str| store.check_user(u, p).unwrap_or(false);

    let (proto, host, port) = if peek[0] == 0x05 {
        let verify_opt: Option<&socks5::Verifier> = if need_auth { Some(&verify) } else { None };
        match socks5::negotiate(&mut client, verify_opt).await? {
            Some((h, p)) => (Proto::Socks5, h, p),
            None => return Ok(()), // rejected, reply already sent
        }
    } else {
        let req = proxy::read_connect_request(&mut client).await?;
        if need_auth {
            let ok = req
                .authorization
                .as_deref()
                .and_then(proxy::parse_basic)
                .map(|(u, p)| verify(&u, &p))
                .unwrap_or(false);
            if !ok {
                proxy::write_auth_required(&mut client).await?;
                return Ok(());
            }
        }
        (Proto::Http, req.host, req.port)
    };

    let nodes = hub.snapshot().await;
    if nodes.is_empty() {
        reject(&mut client, proto).await?;
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
        let hf = host_fail_count(&nodes[i].3, &host);
        let global = nodes[i].2.load(Ordering::Relaxed);
        (hf >= UNHEALTHY_AT, hf, global)
    });

    for idx in order {
        let (node_id, node_tx, fails, host_fails) = &nodes[idx];
        let conn_id = hub.next_conn_id();
        let (otx, orx) = oneshot::channel::<Conn>();
        hub.insert_pending(conn_id, otx).await;

        if node_tx
            .send(HubToNode::Dial {
                conn_id,
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
                established(&mut client, proto).await?;
                let _ = copy_bidirectional(&mut client, &mut data).await;
                return Ok(());
            }
            _ => {
                hub.take_pending(conn_id).await;
                fails.fetch_add(1, Ordering::Relaxed);
                *host_fails.lock().unwrap().entry(host.clone()).or_insert(0) += 1;
                tracing::debug!(node = %node_id.0, %host, "dial failed on host, failing over");
                continue;
            }
        }
    }

    reject(&mut client, proto).await?;
    Ok(())
}

async fn established(client: &mut TcpStream, proto: Proto) -> std::io::Result<()> {
    match proto {
        Proto::Http => proxy::write_established(client).await,
        Proto::Socks5 => socks5::write_reply(client, socks5::REP_SUCCESS).await,
    }
}

async fn reject(client: &mut TcpStream, proto: Proto) -> std::io::Result<()> {
    match proto {
        Proto::Http => proxy::write_bad_gateway(client).await,
        Proto::Socks5 => socks5::write_reply(client, socks5::REP_GENERAL_FAILURE).await,
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
    fails: u32,
}

#[derive(Serialize)]
struct TokenInfo {
    token: String,
    name: String,
    created: i64,
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

fn authed(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| t == token)
        .unwrap_or(false)
}

fn ise<E: std::fmt::Display>(e: E) -> StatusCode {
    tracing::warn!(error = %e, "admin api error");
    StatusCode::INTERNAL_SERVER_ERROR
}

async fn run_admin(listen: String, ctx: AdminCtx) -> Result<()> {
    let app = Router::new()
        .route("/", get(|| async { Html(crate::admin_ui::DASHBOARD) }))
        .route("/api/nodes", get(api_nodes))
        .route("/api/tokens", get(api_list_tokens).post(api_create_token))
        .route("/api/tokens/:token", delete(api_delete_token))
        .route("/api/users", get(api_list_users).post(api_create_user))
        .route("/api/users/:username", delete(api_delete_user))
        .with_state(ctx);
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("bind admin {listen}"))?;
    tracing::info!(%listen, "admin API + dashboard up");
    axum::serve(listener, app).await?;
    Ok(())
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
    Ok(Json(
        rows.into_iter()
            .map(|(token, name, created)| TokenInfo {
                token,
                name,
                created,
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
