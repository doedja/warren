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
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Args;
use tokio::io::{copy_bidirectional, split};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::{timeout, Duration};
use tokio_rustls::TlsAcceptor;

use warren_proto::{DataHello, Greeting, Hello, HelloReply, HubToNode, NodeId, NodeToHub};

use crate::conn::Conn;
use crate::proxy;
use crate::tls;
use crate::wire::{read_msg, write_msg};

#[derive(Args, Debug)]
pub struct HubArgs {
    /// Node-facing listen address (control + data connections).
    #[arg(long, default_value = "0.0.0.0:7000")]
    pub listen: String,
    /// Client-facing proxy listen address (HTTP CONNECT).
    #[arg(long, default_value = "0.0.0.0:8000")]
    pub proxy_listen: String,
    /// Shared enrollment token a node must present to join.
    #[arg(long)]
    pub enroll_token: String,
    /// Optional Basic-auth username required of proxy clients.
    #[arg(long)]
    pub proxy_user: Option<String>,
    /// Optional Basic-auth password required of proxy clients.
    #[arg(long)]
    pub proxy_pass: Option<String>,
    /// Enable TLS on the node link (self-signed cert; fingerprint printed).
    #[arg(long, default_value_t = false)]
    pub tls: bool,
}

#[derive(Args, Debug)]
pub struct EnrollArgs {
    /// Friendly name for the node (informational).
    #[arg(long, default_value = "node")]
    pub name: String,
}

struct NodeEntry {
    id: NodeId,
    tx: mpsc::UnboundedSender<HubToNode>,
}

/// Runtime config independent of the CLI, so tests can drive the hub with
/// pre-bound listeners on ephemeral ports.
pub struct HubConfig {
    pub enroll_token: String,
    pub proxy_creds: Option<(String, String)>,
    pub tls: Option<TlsAcceptor>,
}

struct Hub {
    enroll_token: String,
    proxy_creds: Option<(String, String)>,
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
    async fn snapshot(&self) -> Vec<(NodeId, mpsc::UnboundedSender<HubToNode>)> {
        self.nodes
            .lock()
            .await
            .iter()
            .map(|n| (n.id.clone(), n.tx.clone()))
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
}

pub async fn run(args: HubArgs) -> Result<()> {
    let node_listener = TcpListener::bind(&args.listen)
        .await
        .with_context(|| format!("bind node listener {}", args.listen))?;
    let proxy_listener = TcpListener::bind(&args.proxy_listen)
        .await
        .with_context(|| format!("bind proxy listener {}", args.proxy_listen))?;

    let tls = if args.tls {
        let (acceptor, fingerprint) = tls::server_acceptor()?;
        println!("warren hub TLS fingerprint: {fingerprint}");
        tracing::info!(%fingerprint,
            "TLS enabled on node link; join nodes with --tls --hub-fingerprint <fingerprint>");
        Some(acceptor)
    } else {
        tracing::warn!("node link is PLAINTEXT (no --tls); use only on localhost or a tailnet");
        None
    };

    let cfg = HubConfig {
        enroll_token: args.enroll_token,
        proxy_creds: match (args.proxy_user, args.proxy_pass) {
            (Some(u), Some(p)) => Some((u, p)),
            _ => None,
        },
        tls,
    };
    run_with_listeners(node_listener, proxy_listener, cfg).await
}

pub async fn run_with_listeners(
    node_listener: TcpListener,
    proxy_listener: TcpListener,
    cfg: HubConfig,
) -> Result<()> {
    let hub = Arc::new(Hub {
        enroll_token: cfg.enroll_token,
        proxy_creds: cfg.proxy_creds,
        tls: cfg.tls,
        nodes: Mutex::new(Vec::new()),
        pending: Mutex::new(HashMap::new()),
        rr: AtomicUsize::new(0),
        conn_seq: AtomicU64::new(1),
    });

    tracing::info!(
        node_listen = ?node_listener.local_addr().ok(),
        proxy_listen = ?proxy_listener.local_addr().ok(),
        auth = hub.proxy_creds.is_some(),
        tls = hub.tls.is_some(),
        "warren hub up"
    );

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
    if hello.token != hub.enroll_token {
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

async fn handle_client(mut client: TcpStream, hub: Arc<Hub>) -> Result<()> {
    let req = proxy::read_connect_request(&mut client).await?;
    if !proxy::check_proxy_auth(&req, hub.proxy_creds.as_ref()) {
        proxy::write_auth_required(&mut client).await?;
        return Ok(());
    }

    let nodes = hub.snapshot().await;
    if nodes.is_empty() {
        proxy::write_bad_gateway(&mut client).await?;
        return Ok(());
    }

    let start = hub.rr.fetch_add(1, Ordering::Relaxed);
    for i in 0..nodes.len() {
        let (node_id, node_tx) = &nodes[(start + i) % nodes.len()];
        let conn_id = hub.next_conn_id();
        let (otx, orx) = oneshot::channel::<Conn>();
        hub.insert_pending(conn_id, otx).await;

        if node_tx
            .send(HubToNode::Dial {
                conn_id,
                host: req.host.clone(),
                port: req.port,
            })
            .is_err()
        {
            hub.take_pending(conn_id).await;
            continue;
        }

        match timeout(Duration::from_secs(15), orx).await {
            Ok(Ok(mut data)) => {
                proxy::write_established(&mut client).await?;
                let _ = copy_bidirectional(&mut client, &mut data).await;
                return Ok(());
            }
            _ => {
                hub.take_pending(conn_id).await;
                tracing::debug!(node = %node_id.0, "dial timed out or failed, failing over");
                continue;
            }
        }
    }

    proxy::write_bad_gateway(&mut client).await?;
    Ok(())
}

pub async fn enroll(args: EnrollArgs) -> Result<()> {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let token: String = (0..32)
        .map(|_| {
            let c: u8 = rng.gen_range(0u8..36);
            if c < 10 {
                (b'0' + c) as char
            } else {
                (b'a' + (c - 10)) as char
            }
        })
        .collect();
    println!("enroll token for '{}': {}", args.name, token);
    println!("start hub:  warren hub --enroll-token {token}");
    println!("join node:  warren node run --hub <hub-host:7000> --token {token}");
    Ok(())
}
