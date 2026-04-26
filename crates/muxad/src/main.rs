//! muxa daemon.
//!
//! Listens on a unix socket, ingests normalized `AgentEvent`s from adapters,
//! exposes query endpoints to the CLI and tmux status line.

use anyhow::{Context, Result};
use clap::Parser;
use muxa_core::config::NotifierBackend;
use muxa_core::{paths, Config, Store};
use muxa_runtime::discovery;
use muxa_runtime::ipc::{harden_permissions, Client, Server};
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
    let mut listener_ready = false;
    for _ in 0..50 {
        if socket.exists() {
            if let Err(e) = harden_permissions(&socket) {
                tracing::warn!(error = %e, "chmod 0600 on socket failed");
            }
            listener_ready = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // One-shot startup discovery: backfill agents that are still running in
    // tmux panes from before the daemon (re)started. Configurable; default
    // on. Does not block server readiness — spawned in the background.
    if listener_ready {
        spawn_startup_discovery(&cfg, socket.clone());
    }

    handle.await??;
    Ok(())
}

/// Decide whether to fire the one-shot discovery pass and, if so, spawn it
/// onto the current tokio runtime.
///
/// Returns `true` when a task was spawned. Extracted from `main` so tests
/// can drive both branches of the `discovery.enabled` flag without having
/// to spawn the real daemon.
fn spawn_startup_discovery(cfg: &Config, socket: PathBuf) -> bool {
    if !cfg.discovery.enabled {
        tracing::debug!("startup discovery disabled by config");
        return false;
    }
    tokio::spawn(async move {
        // Small grace so the listener's `accept` loop is actually running
        // by the time we connect. The 250 ms figure matches the design
        // doc — anything less is racy on slower hosts, anything more
        // delays the visible backfill needlessly.
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        let client = Client::new(socket);
        match discovery::run_discovery(&client).await {
            Ok(report) => {
                tracing::info!(
                    claude_code = report.claude_code,
                    codex = report.codex,
                    gemini_cli = report.gemini_cli,
                    skipped_known = report.skipped_known,
                    failed = report.failed,
                    "startup discovery complete",
                );
            }
            Err(e) => tracing::warn!(error = %e, "startup discovery failed"),
        }
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use muxa_core::config::DiscoveryConfig;

    #[tokio::test]
    async fn startup_discovery_runs_when_enabled() {
        let cfg = Config {
            discovery: DiscoveryConfig { enabled: true },
            ..Config::default()
        };
        let spawned = spawn_startup_discovery(&cfg, PathBuf::from("/tmp/never-bound.sock"));
        assert!(spawned, "discovery should spawn when enabled");
    }

    #[tokio::test]
    async fn startup_discovery_skipped_when_disabled() {
        let cfg = Config {
            discovery: DiscoveryConfig { enabled: false },
            ..Config::default()
        };
        let spawned = spawn_startup_discovery(&cfg, PathBuf::from("/tmp/never-bound.sock"));
        assert!(!spawned, "discovery must not spawn when disabled");
    }
}
