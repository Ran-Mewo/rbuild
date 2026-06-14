//! `rbuildd` — the remote daemon. Launched on demand over SSH (`rbuildd
//! serve`), it speaks the frame protocol on stdin/stdout. It owns a single
//! root directory for all workspaces and runs every build inside a container,
//! so the host filesystem stays pristine and the whole install is removable.

mod backend;
mod build;
mod serve;
mod sync;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "rbuildd", version, about = "rbuild remote daemon")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Serve one client session over stdin/stdout (invoked by the client via SSH).
    Serve,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Logs must go to stderr: stdout is the protocol channel.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    match Cli::parse().command {
        Cmd::Serve => serve::serve().await,
    }
}
