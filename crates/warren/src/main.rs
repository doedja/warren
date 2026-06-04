//! warren: a residential proxy pool from devices you own.
//!
//! One binary, two modes:
//!   warren hub      run the control plane (client proxy + node registry)
//!   warren node     run a node agent (dials out to the hub, egresses locally)
//!   warren enroll   (hub side) mint an enrollment token for a new node

use anyhow::Result;
use clap::{Parser, Subcommand};
use warren::{hub, node};

#[derive(Parser, Debug)]
#[command(
    name = "warren",
    version,
    about = "Residential proxy pool from your own devices"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the hub (control plane + client-facing proxy).
    Hub(hub::HubArgs),
    /// Run a node agent on this device.
    Node(node::NodeArgs),
    /// (hub side) Mint an enrollment token for a new node.
    Enroll(hub::EnrollArgs),
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    match cli.command {
        Command::Hub(args) => hub::run(args).await,
        Command::Node(args) => node::run(args).await,
        Command::Enroll(args) => hub::enroll(args).await,
    }
}

fn init_tracing() {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};
    // rustls WARNs on every ClientHello whose SNI is a literal IP (RFC 6066: SNI
    // must be a hostname). Internet scanners hitting the exposed node port do this
    // constantly; the warning is harmless (rustls ignores the SNI, auth is the
    // app-layer enroll token) but floods the dashboard hub-log card. Drop just that
    // target below WARN. RUST_LOG still overrides the whole filter when set.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,rustls::msgs::handshake=error"));
    // stdout/journald as before, plus an in-memory ring the hub dashboard reads
    // (bounded in logbuf, so it never bloats).
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer())
        .with(
            fmt::layer()
                .with_ansi(false)
                .with_writer(warren::logbuf::RingWriter),
        )
        .init();
}
