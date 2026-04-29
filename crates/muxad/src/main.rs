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
use muxa::history::{HistoryOptions, PromptHistory};
use muxa::ipc::{harden_permissions, Client, Server};
use muxa::notify::Notifier;
use muxa::reconcile::Reconciler;
use muxa::sinks::OhMyPromptSink;
use muxa::snapshot::{self, Snapshotter, SnapshotterOptions};
use muxa::tmux::scanner::PaneCache;
use muxa::{discovery, paths, Config, Store};
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

    // Construct the shutdown broadcast up-front so every background task
    // we spawn below can subscribe and exit cleanly. The signal handler
    // lights it up on SIGTERM/SIGINT; the IPC server treats it as the
    // authoritative drain signal.
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

    // Prompt history must exist before the store: every PromptSubmitted
    // event fans out into history alongside the live agent record.
    let history = build_history(&cfg, &shutdown_tx).await;
    let store = Store::shared_with_history(history.clone());

    // One backend, shared across every consumer that previously spoke
    // to tmux directly. Resolution honors `MUXA_HOST` then auto-detects
    // from `ZELLIJ` / `TMUX`, falling back to `TmuxBackend` when no host
    // is detectable. Cheap to clone — the underlying `Arc` lets the
    // reconciler, discovery, enrichment, and snapshotter each hold a
    // handle without contention.
    let backend: muxa::SharedBackend = muxa::default_backend();
    tracing::info!(host = %backend.kind(), "pane backend selected");

    // Rehydrate the agent registry from the previous run's snapshot, if
    // any. Done before the IPC server starts accepting events so no
    // adapter ingest can race a half-loaded store; before discovery so
    // its `already_known` filter sees the hydrated entries and skips
    // panes we already have rich state for; before the snapshotter
    // spawns so the first save isn't a no-op overwrite of the file we
    // just read.
    hydrate_state(&cfg, &store).await;
    // Capability-gated: enrichment classifies panes by foreground
    // command, which the zellij CLI baseline doesn't expose. The
    // function itself respects `caps()` so it's safe to always call;
    // the explicit log line just makes the skip-on-zellij case
    // legible in operator traces.
    // Then layer in any panes that have prompt history on disk but
    // aren't represented in state.json (typically because the previous
    // run died before its first debounce window). This recovers the
    // real `session_id` + `last_prompt` from prompts.ndjson, so the
    // operator sees rich rows for those panes immediately on restart
    // instead of `synthetic-%X` placeholders.
    enrich_from_history(&store, &history, &backend).await;

    spawn_gc_task(&store, &shutdown_tx);
    spawn_reconciler_task(&cfg, &store, &shutdown_tx, backend.clone());
    spawn_history_compaction_task(&cfg, &store, &shutdown_tx);

    // The snapshotter listens on its own dedicated channel rather than
    // the main shutdown broadcast: it has to be the last thing to die
    // so its final flush captures every committed `Store::apply`. Main
    // signals this channel only after the IPC server has fully drained
    // its in-flight handlers — see the shutdown sequence at the bottom
    // of this function.
    let (snap_shutdown_tx, _) = broadcast::channel::<()>(1);
    let snap_handle = spawn_snapshotter_task(&cfg, &store, &snap_shutdown_tx);

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

    spawn_oh_my_prompt_sink(&cfg, &store, &shutdown_tx)?;

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
        spawn_startup_discovery(&cfg, socket.clone(), backend.clone());
    }

    // Shutdown sequence (each step depends on the previous):
    //   1. `handle.await` — IPC server drains its in-flight handlers and
    //      returns. After this point no further `Store::apply` can land.
    //   2. Signal `snap_shutdown_tx` so the snapshotter wakes from its
    //      `dirty.notified()` await and runs its final flush. State on
    //      disk now reflects every committed event up to shutdown.
    //   3. Await the snapshotter's JoinHandle (with a small timeout) so
    //      we don't drop the runtime mid-write.
    handle.await??;
    let _ = snap_shutdown_tx.send(());
    if let Some(h) = snap_handle {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), h).await;
    }
    Ok(())
}

/// Spawn the GC task: evicts long-stopped agents on a periodic timer.
///
/// Listens to the shutdown broadcast so a clean SIGTERM tears it down
/// rather than relying on the runtime falling out from under it.
fn spawn_gc_task(store: &muxa::SharedStore, shutdown_tx: &broadcast::Sender<()>) {
    let store = store.clone();
    let ttl = time::Duration::minutes(STOPPED_AGENT_TTL_MINUTES);
    let tick = std::time::Duration::from_secs(GC_SWEEP_INTERVAL_SECONDS);
    let mut shutdown_rx = shutdown_tx.subscribe();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tick);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let removed = store.gc(ttl).await;
                    if removed > 0 {
                        tracing::debug!(removed, "gc swept stopped agents");
                    }
                }
                _ = shutdown_rx.recv() => {
                    tracing::debug!("gc task shutting down");
                    break;
                }
            }
        }
    });
}

/// Initialize the prompt history layer.
///
/// When `[history] enabled = true` we hydrate the in-memory cache from
/// the configured NDJSON file (creating the parent dir if needed) and
/// spawn the writer task that owns the file handle. When disabled —
/// either via config or because the path can't be resolved — we hand
/// back an in-memory-only instance so `Store::apply` always has somewhere
/// to fan out, but nothing touches disk.
async fn build_history(
    cfg: &Config,
    shutdown_tx: &broadcast::Sender<()>,
) -> std::sync::Arc<PromptHistory> {
    let opts_template = HistoryOptions {
        path: cfg
            .history
            .path
            .clone()
            .or_else(paths::default_history_file),
        max_per_pane: cfg.history.max_per_pane,
        max_age: time::Duration::days(i64::from(cfg.history.max_age_days)),
        ..HistoryOptions::default()
    };

    if !cfg.history.enabled {
        tracing::info!("history disabled by config (in-memory only)");
        return PromptHistory::in_memory_only(HistoryOptions {
            path: None,
            ..opts_template
        });
    }

    let Some(path) = opts_template.path.clone() else {
        tracing::warn!("history enabled but no path resolvable; falling back to in-memory only");
        return PromptHistory::in_memory_only(HistoryOptions {
            path: None,
            ..opts_template
        });
    };

    match PromptHistory::spawn(opts_template.clone(), shutdown_tx.subscribe()).await {
        Ok((history, _writer_handle)) => {
            // The writer task drains itself on shutdown via its own
            // broadcast receiver, so we don't need to await the handle
            // here — the IPC server's await_shutdown call is the join
            // point for the daemon as a whole.
            tracing::info!(
                path = %path.display(),
                max_per_pane = cfg.history.max_per_pane,
                max_age_days = cfg.history.max_age_days,
                "prompt history enabled",
            );
            history
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "could not open history file; falling back to in-memory only",
            );
            PromptHistory::in_memory_only(HistoryOptions {
                path: None,
                ..opts_template
            })
        }
    }
}

/// Spawn the periodic prompt-history compaction task.
///
/// Compaction drops aged-out entries from memory and rewrites the disk
/// file from the surviving snapshot. Cheap, idempotent, and the only
/// codepath that physically removes records from disk.
fn spawn_history_compaction_task(
    cfg: &Config,
    store: &muxa::SharedStore,
    shutdown_tx: &broadcast::Sender<()>,
) {
    if !cfg.history.enabled {
        return;
    }
    let history = store.history().clone();
    let interval_secs = cfg.history.compact_interval_secs;
    let mut shutdown_rx = shutdown_tx.subscribe();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Skip the immediate first tick so the loop's cadence matches
        // `interval_secs` rather than running once at t=0 (when there's
        // nothing to compact anyway).
        tick.tick().await;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let report = history.compact().await;
                    if report.aged_out > 0 || report.rewrite_skipped {
                        tracing::debug!(
                            aged_out = report.aged_out,
                            rewrite_skipped = report.rewrite_skipped,
                            "history compaction pass",
                        );
                    }
                }
                _ = shutdown_rx.recv() => {
                    tracing::debug!("history compaction task shutting down");
                    break;
                }
            }
        }
    });
}

/// Spawn the periodic reconciler: convergent control loop that uses tmux
/// as ground truth. Reaps records for closed panes, demotes orphaned
/// synthetic placeholders, and collapses duplicate rows so `muxa watch`
/// and `muxa status` stay in sync with the user's actual tmux state.
fn spawn_reconciler_task(
    cfg: &Config,
    store: &muxa::SharedStore,
    shutdown_tx: &broadcast::Sender<()>,
    backend: muxa::SharedBackend,
) {
    if !cfg.reconciler.enabled {
        tracing::info!("reconciler disabled by config");
        return;
    }
    // The shared backend is what the rest of the daemon uses too —
    // everyone agreeing on one host means the reconciler reaps panes
    // by the same definition the watch loop, hook ancestry, and
    // discovery do. `LivenessSource` reaches the reconciler via the
    // blanket impl on `PaneBackend`.
    let runner = Reconciler::new(
        store.clone(),
        backend,
        std::time::Duration::from_secs(cfg.reconciler.interval_secs),
    );
    let shutdown_rx = shutdown_tx.subscribe();
    tokio::spawn(runner.run(shutdown_rx));
    tracing::info!(
        interval_secs = cfg.reconciler.interval_secs,
        "reconciler enabled",
    );
}

/// Spawn the snapshotter: writes the live agent registry to disk on
/// every dirty signal from the store, debounced so a burst of events
/// produces one disk write. Survives daemon restarts so `muxa watch`
/// rehydrates with real `session_id`s, `last_prompt`s, and full
/// state/metadata instead of synthetic placeholders.
fn spawn_snapshotter_task(
    cfg: &Config,
    store: &muxa::SharedStore,
    shutdown_tx: &broadcast::Sender<()>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !cfg.state.enabled {
        tracing::info!("state snapshot disabled by config");
        return None;
    }
    let Some(path) = cfg.state.path.clone().or_else(paths::default_state_file) else {
        tracing::warn!("state snapshot enabled but no path resolvable; restarts will lose state");
        return None;
    };
    let opts = SnapshotterOptions {
        path: path.clone(),
        debounce: std::time::Duration::from_millis(cfg.state.debounce_ms),
    };
    let snapshotter = Snapshotter::new(store.clone(), store.dirty(), opts);
    let shutdown_rx = shutdown_tx.subscribe();
    let handle = tokio::spawn(snapshotter.run(shutdown_rx));
    tracing::info!(
        path = %path.display(),
        debounce_ms = cfg.state.debounce_ms,
        "state snapshotter enabled",
    );
    Some(handle)
}

/// Rehydrate the registry from disk on startup. No-op when state
/// snapshotting is disabled or the file is missing — first-run daemons
/// just start empty and let discovery + live hooks populate the store.
async fn hydrate_state(cfg: &Config, store: &muxa::SharedStore) {
    if !cfg.state.enabled {
        return;
    }
    let Some(path) = cfg.state.path.clone().or_else(paths::default_state_file) else {
        return;
    };
    let initial = snapshot::load(&path).await;
    if initial.is_empty() {
        return;
    }
    store.hydrate(initial).await;
}

/// Bridge `prompts.ndjson` into the live registry on startup.
///
/// `state.json` is the authoritative restart-recovery surface, but the
/// daemon may have died before its first debounce window — leaving panes
/// whose hooks already populated `prompts.ndjson` but never made it into
/// state. For each live tmux pane that *has* a prompt-history record but
/// no real agent (i.e. only a synthetic placeholder, or nothing), seed
/// an `Idle` agent under the real `session_id` from history so the
/// operator sees rich rows immediately.
///
/// Skipped silently when tmux isn't available, when history is empty, or
/// when state snapshotting is disabled (in which case we'd rather not
/// resurrect agents the operator opted out of persisting).
async fn enrich_from_history(
    store: &muxa::SharedStore,
    history: &Arc<PromptHistory>,
    backend: &muxa::SharedBackend,
) {
    if history.is_empty().await {
        return;
    }
    // Wrap the (potentially blocking) backend call so the runtime
    // doesn't stall while tmux shells out. Cloning the `Arc` is the
    // cheapest way to give `spawn_blocking`'s closure the `'static`
    // handle it needs.
    let backend_for_blocking = backend.clone();
    let Ok(panes) = tokio::task::spawn_blocking(move || backend_for_blocking.list_panes()).await
    else {
        return;
    };
    if panes.is_empty() {
        return;
    }

    // Snapshot the current registry to decide which panes need
    // enrichment. A pane is "covered" iff a *real* (non-synthetic) live
    // agent already represents it — synthetic placeholders are the
    // exact thing this pass is trying to upgrade away from.
    let snapshot = store.snapshot().await;

    let mut candidates: Vec<muxa::Agent> = Vec::new();
    for pane in &panes {
        let has_real = snapshot.iter().any(|a| {
            a.pane.as_deref() == Some(pane.pane_id.as_str())
                && !a
                    .session_id
                    .starts_with(muxa::state::SYNTHETIC_SESSION_PREFIX)
                && a.state != muxa::AgentState::Stopped
        });
        if has_real {
            continue;
        }
        let recents = history.recent_for_pane(&pane.pane_id, 1).await;
        let Some(entry) = recents.into_iter().next() else {
            continue;
        };
        candidates.push(muxa::Agent {
            kind: entry.kind,
            session_id: entry.session_id,
            pane: Some(pane.pane_id.clone()),
            cwd: None,
            state: muxa::AgentState::Idle,
            last_prompt: Some(entry.prompt),
            last_response: None,
            last_notification: None,
            model: entry.model,
            context_used_pct: None,
            cost_usd: None,
            started_at: entry.at,
            last_activity_at: entry.at,
        });
    }

    let inserted = store.seed_if_absent(candidates).await;
    if inserted > 0 {
        tracing::info!(inserted, "enriched registry from prompt history");
    }
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
        std::env::var("MUXA_DASHBOARD_ENABLED").ok().and_then(|s| {
            match s.to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => Some(true),
                "0" | "false" | "no" | "off" => Some(false),
                _ => None,
            }
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

/// Resolve the oh-my-prompt sink config and, if enabled, spawn its
/// task. Mirrors the dashboard wire-up: the daemon's existing shutdown
/// broadcast is reused for joint shutdown.
fn spawn_oh_my_prompt_sink(
    cfg: &Config,
    store: &muxa::SharedStore,
    shutdown_tx: &broadcast::Sender<()>,
) -> Result<()> {
    match OhMyPromptSink::resolve(&cfg.sinks.oh_my_prompt) {
        Ok(Some(sink)) => {
            let prompt_rx = store.subscribe_prompts();
            let shutdown_rx = shutdown_tx.subscribe();
            tokio::spawn(async move {
                if let Err(e) = sink.run(prompt_rx, shutdown_rx).await {
                    tracing::error!(error = %e, "oh-my-prompt sink exited");
                }
            });
            tracing::info!("oh-my-prompt sink enabled");
        }
        Ok(None) => {}
        Err(e) => {
            return Err(anyhow::Error::new(e).context("resolving oh-my-prompt sink"));
        }
    }
    Ok(())
}

/// Decide whether to fire the one-shot discovery pass and, if so, spawn it
/// onto the current tokio runtime.
///
/// Returns `true` when a task was spawned. Extracted from `main` so tests
/// can drive both branches of the `discovery.enabled` flag without having
/// to spawn the real daemon.
fn spawn_startup_discovery(cfg: &Config, socket: PathBuf, backend: muxa::SharedBackend) -> bool {
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
        match discovery::run_discovery(&client, backend.as_ref()).await {
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
    use muxa::config::DiscoveryConfig;

    #[tokio::test]
    async fn startup_discovery_runs_when_enabled() {
        let cfg = Config {
            discovery: DiscoveryConfig { enabled: true },
            ..Config::default()
        };
        let spawned = spawn_startup_discovery(
            &cfg,
            PathBuf::from("/tmp/never-bound.sock"),
            muxa::default_backend(),
        );
        assert!(spawned, "discovery should spawn when enabled");
    }

    #[tokio::test]
    async fn startup_discovery_skipped_when_disabled() {
        let cfg = Config {
            discovery: DiscoveryConfig { enabled: false },
            ..Config::default()
        };
        let spawned = spawn_startup_discovery(
            &cfg,
            PathBuf::from("/tmp/never-bound.sock"),
            muxa::default_backend(),
        );
        assert!(!spawned, "discovery must not spawn when disabled");
    }
}
