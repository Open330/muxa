//! muxa daemon.
//!
//! Run:     muxad
//! Socket:  $XDG_RUNTIME_DIR/muxa.sock (override with --socket)

use anyhow::Result;
use clap::Parser;
use muxa::{default_socket_path, ipc, state::Store};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "muxad", about = "muxa daemon")]
struct Args {
    /// Unix socket path.
    #[arg(long, env = "MUXA_SOCKET")]
    socket: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "muxa=info,muxad=info".into()),
        )
        .init();

    let args = Args::parse();
    let socket = args.socket.unwrap_or_else(default_socket_path);
    let store = Store::shared();

    ipc::serve(&socket, store).await
}
