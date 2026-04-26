//! muxa daemon.
//!
//! Listens on a unix socket, ingests normalized `AgentEvent`s from adapters,
//! exposes query endpoints to the CLI and tmux status line, and — when
//! opted in via `[dashboard] enabled = true` or `--dashboard` — serves a
//! read-only HTTP dashboard alongside the unix socket.

use anyhow::{Context, Result};
use clap::Parser;
use muxa::config::NotifierBackend;
use muxa::dashboard::{DashboardConfig, DashboardOverrides};
use muxa::ipc::{harden_permissions, Server};
use muxa::notify::Notifier;
use muxa::tmux::scanner::PaneCache;
use muxa::{paths, Config, Store};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
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

    /// Enable the HTTP dashboard. Equivalent to `[dashboard] enabled =
    /// true`.
    #[arg(long, conflicts_with = "no_dashboard")]
    dashboard: bool,

    /// Force the HTTP dashboard off, even if the config file enabled it.
    #[arg(long, conflicts_with = "dashboard")]
    no_dashboard: bool,

    /// Dashboard bind address as `ip:port`. Defaults to `127.0.0.1:7878`.
    #[arg(long, value_name = "ADDR", env = "MUXA_DASHBOARD_BIND")]
    dashboard_bind: Option<String>,

    /// Dashboard bearer token. Required for non-loopback binds.
    #[arg(long, value_name = "TOKEN", env = "MUXA_DASHBOARD_TOKEN")]
    dashboard_token: Option<String>,

    /// Confirm that you want to bind the dashboard to a non-loopback
    /// address. Required (with a token) when `dashboard.bind` is not
    /// `127.0.0.1` / `::1`.
    #[arg(long)]
    allow_public: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let config_path = args.config.clone().or_else(paths::default_config_file);
    let cfg = Config::load_or_default(config_path.as_deref()).context("loading config")?;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "muxa=info".into()),
        )
        // Daemon logs go to stderr — stdout is reserved for any future
        // tool-friendly output, and external tooling (systemd, e2e
        // tests) expects logs on stderr.
        .with_writer(std::io::stderr)
        // Disable ANSI escapes when stderr isn't a TTY. systemd-journald
        // captures the bytes verbatim; without this, `journalctl -u muxad`
        // is full of literal `\e[1m` debris. Same logic helps anything
        // grep-ing piped logs (the e2e tests included).
        .with_ansi(std::io::stderr().is_terminal())
        .init();

    let socket = args
        .socket
        .clone()
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

    // HTTP dashboard. Resolved from cfg + CLI/env overrides; non-fatal if
    // it fails to bind (the unix-socket IPC keeps running).
    let dash_cfg = resolve_dashboard_config(&cfg, &args)?;
    if dash_cfg.enabled {
        let dash_cfg = Arc::new(dash_cfg);
        let pane_cache = Arc::new(PaneCache::new(dash_cfg.pane_cache_ttl));
        let store_for_dash = store.clone();
        let shutdown_rx = shutdown_tx.subscribe();
        let dash_cfg_for_task = dash_cfg.clone();
        tokio::spawn(async move {
            if let Err(e) =
                muxa::dashboard::serve(dash_cfg_for_task, store_for_dash, pane_cache, shutdown_rx)
                    .await
            {
                tracing::error!(error = %e, "dashboard server exited");
            }
        });
        tracing::info!(addr = %dash_cfg.bind, "dashboard task spawned");
    }

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

/// Collapse the daemon's CLI args + env vars + TOML into a resolved
/// dashboard config.
///
/// Per-field precedence: env > CLI flag > TOML > built-in default. clap
/// already folds `MUXA_DASHBOARD_*` envs into the parsed args (via the
/// `env =` attribute), so for those fields env-beats-flag is enforced
/// at parse time — by the time we see them in `args`, the env value
/// has already won.
fn resolve_dashboard_config(cfg: &Config, args: &Args) -> Result<DashboardConfig> {
    let enabled = if args.dashboard {
        Some(true)
    } else if args.no_dashboard {
        Some(false)
    } else {
        std::env::var("MUXA_DASHBOARD_ENABLED")
            .ok()
            .and_then(|s| match s.to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => Some(true),
                "0" | "false" | "no" | "off" => Some(false),
                _ => None,
            })
    };

    let allow_public = if args.allow_public {
        Some(true)
    } else {
        std::env::var("MUXA_DASHBOARD_ALLOW_PUBLIC")
            .ok()
            .and_then(|s| match s.to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => Some(true),
                "0" | "false" | "no" | "off" => Some(false),
                _ => None,
            })
    };

    let overrides = DashboardOverrides {
        enabled,
        bind: args.dashboard_bind.clone(),
        token: args.dashboard_token.clone(),
        allow_public,
    };

    DashboardConfig::resolve(&cfg.dashboard, &overrides)
        .map_err(|e| anyhow::anyhow!(e).context("resolving dashboard config"))
}
