//! muxa daemon.
//!
//! Listens on a unix socket, ingests normalized `AgentEvent`s from adapters,
//! exposes query endpoints to the CLI and tmux status line.

use anyhow::{Context, Result};
use clap::Parser;
use muxa_core::config::NotifierBackend;
use muxa_core::{paths, Config, Store};
use muxa_runtime::ipc::{harden_permissions, Server};
use muxa_runtime::notify::Notifier;
use std::path::PathBuf;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::broadcast;

/// Inactivity window before a stopped agent is evicted from the in-memory store.
const STOPPED_AGENT_TTL_MINUTES: i64 = 60;
/// Cadence at which the GC task scans for evictable agents.
const GC_SWEEP_INTERVAL_SECONDS: u64 = 60;

#[derive(Debug, Parser)]
#[command(name = "muxad", version, about = "muxa daemon")]
struct Args {
    /// Unix socket path. Overrides config and XDG default.
    #[arg(long, env = "MUXA_SOCKET")]
    socket: Option<PathBuf>,

    /// Config file path. Defaults to `$XDG_CONFIG_HOME/muxa/config.toml`.
    #[arg(long, env = "MUXA_CONFIG")]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let config_path = args.config.or_else(paths::default_config_file);
    let cfg = Config::load_or_default(config_path.as_deref()).context("loading config")?;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "muxa=info".into()),
        )
        .init();

    let socket = args
        .socket
        .or(cfg.socket.clone())
        .unwrap_or_else(paths::default_socket);
    tracing::info!(socket = %socket.display(), "starting muxad");

    let store = Store::shared();

    // GC task: evict long-stopped agents.
    {
        let store = store.clone();
        let ttl = time::Duration::minutes(STOPPED_AGENT_TTL_MINUTES);
        let tick = std::time::Duration::from_secs(GC_SWEEP_INTERVAL_SECONDS);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tick);
            loop {
                interval.tick().await;
                let removed = store.gc(ttl).await;
                if removed > 0 {
                    tracing::debug!(removed, "gc swept stopped agents");
                }
            }
        });
    }

    // Desktop notifier: spawned only when opted in. We subscribe BEFORE
    // the server starts accepting events so no early transition is lost.
    if cfg.notifier.enabled && matches!(cfg.notifier.backend, NotifierBackend::Libnotify) {
        let rx = store.subscribe();
        tokio::spawn(async move {
            if let Err(e) = Notifier::new().run(rx).await {
                tracing::warn!(error = %e, "notifier task exited");
            }
        });
        tracing::info!("desktop notifier enabled");
    }

    let (shutdown_tx, _) = broadcast::channel::<()>(1);

    // Signal handler — translates SIGTERM/SIGINT into a broadcast.
    let shutdown_for_signals = shutdown_tx.clone();
    tokio::spawn(async move {
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        tokio::select! {
            _ = term.recv() => tracing::info!("SIGTERM received"),
            _ = int.recv()  => tracing::info!("SIGINT received"),
        }
        let _ = shutdown_for_signals.send(());
    });

    let server = Server::new(socket.clone(), store);
    let handle = tokio::spawn(server.run(shutdown_tx.subscribe()));

    // Harden socket permissions once the listener exists. We poll briefly
    // because bind is fire-and-forget vs. spawn timing.
    for _ in 0..50 {
        if socket.exists() {
            if let Err(e) = harden_permissions(&socket) {
                tracing::warn!(error = %e, "chmod 0600 on socket failed");
            }
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    handle.await??;
    Ok(())
}
