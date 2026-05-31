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
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).init();
}
