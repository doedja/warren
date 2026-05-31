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
    /// Install warren as a boot service on this OS (later milestone).
    Install(RunArgs),
    /// Remove the boot service and leave the pool (later milestone).
    Uninstall,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    /// Hub node-facing address, host:port (e.g. 127.0.0.1:7000).
    #[arg(long)]
    pub hub: String,
    /// Enrollment token.
    #[arg(long)]
    pub token: String,
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
    loop {
        match connect_once(&args, &name, &connector).await {
            Ok(()) => tracing::warn!("control connection closed; reconnecting in 3s"),
            Err(e) => tracing::warn!(error = %e, "control connection error; reconnecting in 3s"),
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
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

async fn connect_once(args: &RunArgs, name: &str, connector: &Option<TlsConnector>) -> Result<()> {
    let mut conn = dial_conn(&args.hub, connector).await?;

    let hello = Hello {
        protocol_version: PROTOCOL_VERSION,
        token: args.token.clone(),
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

async fn install_service(_args: RunArgs) -> Result<()> {
    tracing::info!("install: per-OS boot service lands in a later milestone");
    Ok(())
}

async fn uninstall_service() -> Result<()> {
    tracing::info!("uninstall: lands in a later milestone");
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
