//! muxa daemon.
//!
//! Listens on a unix socket, ingests normalized `AgentEvent`s from adapters,
//! exposes query endpoints to the CLI and tmux status line, and — when
//! opted in via `[dashboard] enabled = true` or `--dashboard` — serves a
//! read-only HTTP dashboard alongside the unix socket.

use anyhow::{Context, Result};
use clap::Parser;
use muxa::activity::{
    ActivityEntry, ActivityLog, ActivityOptions, StateTransitionEntry, StateTransitionInput,
};
use muxa::config::{DashboardAuthMode, NotifierBackend};
use muxa::dashboard::{DashboardConfig, DashboardOverrides};
use muxa::history::{HistoryOptions, PaneSessionCache, PromptHistory};
use muxa::ipc::{harden_permissions, Client, Server};
use muxa::notify::Notifier;
use muxa::reconcile::Reconciler;
use muxa::sinks::{webhook as webhook_sink, OhMyPromptSink, WebhookSink};
use muxa::snapshot::{self, Snapshotter, SnapshotterOptions};
use muxa::tmux::scanner::PaneCache;
use muxa::{discovery, paths, Config, Store};
use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::broadcast;

/// Inactivity window before a stopped agent is evicted from the in-memory store.
const STOPPED_AGENT_TTL_MINUTES: i64 = 60;
/// Cadence at which the GC task scans for evictable agents.
const GC_SWEEP_INTERVAL_SECONDS: u64 = 60;
const PANE_SESSION_CACHE_INTERVAL_SECONDS: u64 = 5;

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

    /// Dashboard API auth mode: `token` or `none`.
    #[arg(long, value_name = "MODE", env = "MUXA_DASHBOARD_AUTH")]
    dashboard_auth: Option<String>,

    /// Confirm that you want to bind the dashboard to a non-loopback
    /// address. Required when `dashboard.bind` is not `127.0.0.1` /
    /// `::1`.
    #[arg(long)]
    allow_public: bool,
}

#[tokio::main]
#[allow(clippy::too_many_lines)] // daemon bootstrap wires long-lived tasks in startup order
async fn main() -> Result<()> {
    let args = Args::parse();

    // Initialize tracing BEFORE loading config so any `tracing::warn!`
    // emitted by `Config::load` (unknown `[watch] columns` keys, unknown
    // detail-template placeholders, …) actually reaches stderr.
    // Previously the subscriber was installed after `Config::load_or_default`
    // and those warnings were silently swallowed on the daemon path. The
    // log filter comes from `RUST_LOG` (env), not config, so there's no
    // chicken-and-egg.
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

    let config_path = args.config.clone().or_else(paths::default_config_file);
    let cfg = Config::load_or_default(config_path.as_deref()).context("loading config")?;
    // Daemon-only invariants — the dashboard wire-up and sink fan-out
    // checks the CLI deliberately skips. Failing here is preferable to
    // crashing mid-startup once we try to bind the dashboard socket.
    cfg.validate_for_daemon()
        .context("validating daemon-only config")?;

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

    install_shutdown_signal_handler(shutdown_tx.clone());

    // Prompt history must exist before the store: every PromptSubmitted
    // event fans out into history alongside the live agent record.
    let pane_session_cache = PaneSessionCache::default();
    let history = build_history(&cfg, &shutdown_tx, pane_session_cache.clone()).await;
    let store = Store::shared_with_history(history.clone());

    // One backend, shared across every consumer that previously spoke
    // to tmux directly. Resolution honors `MUXA_HOST` then auto-detects
    // from `ZELLIJ` / `TMUX`, falling back to `TmuxBackend` when no host
    // is detectable. Cheap to clone — the underlying `Arc` lets the
    // reconciler, discovery, enrichment, and snapshotter each hold a
    // handle without contention.
    let backend: muxa::SharedBackend = muxa::default_backend();
    let sessions = muxa::PtySessionBackend::shared();
    tracing::info!(host = %backend.kind(), "pane backend selected");
    refresh_pane_session_cache(&pane_session_cache, backend.clone()).await;
    spawn_pane_session_cache_task(pane_session_cache.clone(), backend.clone(), &shutdown_tx);

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

    let activity_log = build_activity_log(&cfg, &shutdown_tx).await;
    spawn_activity_transition_task(
        &store,
        activity_log.clone(),
        pane_session_cache.clone(),
        &shutdown_tx,
    )
    .await;
    spawn_gc_task(&store, &shutdown_tx);
    spawn_reconciler_task(&cfg, &store, &shutdown_tx, backend.clone());
    spawn_session_activity_task(&cfg, &shutdown_tx, activity_log.clone());
    spawn_history_compaction_task(&cfg, &store, &shutdown_tx);
    spawn_activity_compaction_task(&cfg, activity_log.clone(), &shutdown_tx);

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
        let dashboard_stats_config = cfg.stats.clone();
        let dashboard_activity_path = cfg
            .activity
            .enabled
            .then(|| {
                cfg.activity
                    .path
                    .clone()
                    .or_else(paths::default_activity_file)
            })
            .flatten();
        let dashboard_session_activity_path = cfg
            .session_activity
            .enabled
            .then(|| {
                cfg.session_activity
                    .path
                    .clone()
                    .or_else(paths::default_session_activity_file)
            })
            .flatten();
        let sessions_for_dash = sessions.clone();
        let dashboard_runtime = muxa::dashboard::DashboardRuntimeConfig {
            activity_path: dashboard_activity_path,
            session_activity_path: dashboard_session_activity_path,
            stats_config: dashboard_stats_config,
        };
        tokio::spawn(async move {
            if let Err(e) = muxa::dashboard::serve(
                dash_cfg_for_task,
                store_for_dash,
                pane_cache,
                sessions_for_dash,
                dashboard_runtime,
                shutdown_rx,
            )
            .await
            {
                tracing::error!(error = %e, "dashboard server exited");
            }
        });
        tracing::info!(addr = %dash_cfg.bind, "dashboard task spawned");
    }

    spawn_oh_my_prompt_sink(&cfg, &store, &shutdown_tx)?;
    spawn_webhook_sink(&cfg, &store, &shutdown_tx)?;

    let server = Server::new(socket.clone(), store)
        .with_backend(backend.clone())
        .with_sessions(sessions);
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

    // Self-heal: if a tmux server is already running, inject our socket
    // path into its environment so that every pane — including any that
    // were started before this muxad instance — can reach us. This
    // complements the `tmux-env` managed block in `~/.tmux.conf` which
    // handles fresh server boots; together they cover both cold-start and
    // warm-restart scenarios without requiring the user to re-run
    // `muxa init` after every daemon or tmux server restart.
    if listener_ready {
        maybe_heal_tmux_socket_env(&socket, cfg.socket.as_deref());
        spawn_startup_discovery(&cfg, socket.clone(), backend.clone());
        spawn_periodic_discovery(&cfg, socket.clone(), backend.clone(), &shutdown_tx);
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

fn install_shutdown_signal_handler(shutdown_tx: broadcast::Sender<()>) {
    tokio::spawn(async move {
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        tokio::select! {
            _ = term.recv() => tracing::info!("SIGTERM received"),
            _ = int.recv()  => tracing::info!("SIGINT received"),
        }
        let _ = shutdown_tx.send(());
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
    pane_session_cache: PaneSessionCache,
) -> std::sync::Arc<PromptHistory> {
    let opts_template = HistoryOptions {
        path: cfg
            .history
            .path
            .clone()
            .or_else(paths::default_history_file),
        max_per_pane: cfg.history.max_per_pane,
        max_age: time::Duration::days(i64::from(cfg.history.max_age_days)),
        pane_sessions: Some(pane_session_cache),
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

/// Initialize the append-only activity ledger.
///
/// When enabled, state-transition and tmux foreground intervals are appended
/// to `$XDG_DATA_HOME/muxa/activity.ndjson` so stats can compute duration
/// even after panes or tmux sessions disappear.
async fn build_activity_log(
    cfg: &Config,
    shutdown_tx: &broadcast::Sender<()>,
) -> Option<std::sync::Arc<ActivityLog>> {
    if !cfg.activity.enabled {
        tracing::info!("activity ledger disabled by config");
        return None;
    }
    let Some(path) = cfg
        .activity
        .path
        .clone()
        .or_else(paths::default_activity_file)
    else {
        tracing::warn!("activity ledger enabled but no path resolvable");
        return None;
    };

    let opts = ActivityOptions::new(path.clone());
    match ActivityLog::spawn(opts, shutdown_tx.subscribe()).await {
        Ok((activity_log, _writer_handle)) => {
            tracing::info!(
                path = %path.display(),
                max_age_days = cfg.activity.max_age_days,
                "activity ledger enabled",
            );
            Some(activity_log)
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "could not open activity ledger; duration stats will be incomplete",
            );
            None
        }
    }
}

async fn refresh_pane_session_cache(cache: &PaneSessionCache, backend: muxa::SharedBackend) {
    let panes = tokio::task::spawn_blocking(move || backend.list_panes())
        .await
        .unwrap_or_default();
    cache.replace(panes.into_iter().map(|pane| (pane.pane_id, pane.session)));
}

fn spawn_pane_session_cache_task(
    cache: PaneSessionCache,
    backend: muxa::SharedBackend,
    shutdown_tx: &broadcast::Sender<()>,
) {
    let mut shutdown_rx = shutdown_tx.subscribe();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(
            PANE_SESSION_CACHE_INTERVAL_SECONDS,
        ));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tick.tick().await;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    refresh_pane_session_cache(&cache, backend.clone()).await;
                }
                _ = shutdown_rx.recv() => {
                    tracing::debug!("pane session cache task shutting down");
                    break;
                }
            }
        }
    });
}

async fn spawn_activity_transition_task(
    store: &muxa::SharedStore,
    activity_log: Option<std::sync::Arc<ActivityLog>>,
    pane_session_cache: PaneSessionCache,
    shutdown_tx: &broadcast::Sender<()>,
) {
    let Some(activity_log) = activity_log else {
        return;
    };

    let mut states = seed_activity_state_map(store.snapshot().await);
    let mut rx = store.subscribe();
    let store = store.clone();
    let mut shutdown_rx = shutdown_tx.subscribe();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                msg = rx.recv() => {
                    match msg {
                        Ok(transition) => {
                            let agent = transition.agent.as_ref();
                            let at = agent.state_entered_at;
                            let prior_entered_at = states
                                .get(&agent.session_id)
                                .map(|(_, entered_at)| *entered_at)
                                .or(Some(agent.started_at));
                            let entry = StateTransitionEntry::new(StateTransitionInput {
                                at,
                                kind: agent.kind,
                                session_id: agent.session_id.clone(),
                                pane: agent.pane.clone(),
                                session_name: agent
                                    .pane
                                    .as_deref()
                                    .and_then(|pane| pane_session_cache.get(pane)),
                                cwd: agent.cwd.clone(),
                                from: transition.from,
                                to: transition.to,
                                state_entered_at: prior_entered_at,
                            });
                            activity_log.append(ActivityEntry::StateTransition(entry));
                            states.insert(agent.session_id.clone(), (transition.to, at));
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(
                                dropped = n,
                                "activity transition subscriber lagged; reseeding duration state",
                            );
                            states = seed_activity_state_map(store.snapshot().await);
                        }
                    }
                }
                _ = shutdown_rx.recv() => {
                    tracing::debug!("activity transition task shutting down");
                    break;
                }
            }
        }
    });
}

fn seed_activity_state_map(
    agents: Vec<muxa::Agent>,
) -> HashMap<String, (muxa::AgentState, time::OffsetDateTime)> {
    agents
        .into_iter()
        .map(|agent| (agent.session_id, (agent.state, agent.state_entered_at)))
        .collect()
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

fn spawn_activity_compaction_task(
    cfg: &Config,
    activity_log: Option<std::sync::Arc<ActivityLog>>,
    shutdown_tx: &broadcast::Sender<()>,
) {
    if !cfg.activity.enabled {
        return;
    }
    let Some(activity_log) = activity_log else {
        return;
    };
    let interval_secs = cfg.activity.compact_interval_secs;
    let max_age = time::Duration::days(i64::from(cfg.activity.max_age_days));
    let mut shutdown_rx = shutdown_tx.subscribe();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tick.tick().await;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let report = activity_log.compact(max_age).await;
                    if report.aged_out > 0 || report.rewrite_skipped {
                        tracing::debug!(
                            aged_out = report.aged_out,
                            rewrite_skipped = report.rewrite_skipped,
                            "activity compaction pass",
                        );
                    }
                }
                _ = shutdown_rx.recv() => {
                    tracing::debug!("activity compaction task shutting down");
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
    // Codex has no rate-limit hook; the reconciler learns codex usage caps
    // by polling the on-disk session rollouts. Resolve the tree once here so
    // the per-tick poll just reads files.
    let codex_sessions_root = if cfg.reconciler.codex_rollout_enabled {
        muxa::adapters::codex_rollout::default_sessions_root()
    } else {
        None
    };
    let runner = Reconciler::new(
        store.clone(),
        backend,
        std::time::Duration::from_secs(cfg.reconciler.interval_secs),
    )
    .with_metrics(store.metrics())
    .with_stuck_working_timeout(std::time::Duration::from_secs(
        cfg.reconciler.stuck_working_timeout_secs,
    ))
    .with_stuck_waiting_timeout(std::time::Duration::from_secs(
        cfg.reconciler.stuck_waiting_timeout_secs,
    ))
    .with_codex_sessions_root(codex_sessions_root.clone());
    let shutdown_rx = shutdown_tx.subscribe();
    tokio::spawn(runner.run(shutdown_rx));
    tracing::info!(
        interval_secs = cfg.reconciler.interval_secs,
        stuck_working_timeout_secs = cfg.reconciler.stuck_working_timeout_secs,
        stuck_waiting_timeout_secs = cfg.reconciler.stuck_waiting_timeout_secs,
        codex_rollout_polling = codex_sessions_root.is_some(),
        "reconciler enabled",
    );
}

/// Track cumulative tmux session attached time for `muxa watch --view session`.
fn spawn_session_activity_task(
    cfg: &Config,
    shutdown_tx: &broadcast::Sender<()>,
    activity_log: Option<std::sync::Arc<ActivityLog>>,
) {
    if !cfg.session_activity.enabled {
        tracing::info!("session activity tracking disabled by config");
        return;
    }
    let Some(path) = cfg
        .session_activity
        .path
        .clone()
        .or_else(paths::default_session_activity_file)
    else {
        tracing::warn!("session activity tracking enabled but no path resolvable");
        return;
    };
    let tracker = muxa::SessionActivityTracker::new(
        path.clone(),
        std::time::Duration::from_secs(cfg.session_activity.interval_secs),
    )
    .with_activity_log(activity_log);
    let shutdown_rx = shutdown_tx.subscribe();
    tokio::spawn(tracker.run(shutdown_rx));
    tracing::info!(
        path = %path.display(),
        interval_secs = cfg.session_activity.interval_secs,
        "session activity tracking enabled",
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
    let snapshotter =
        Snapshotter::new(store.clone(), store.dirty(), opts).with_metrics(store.metrics());
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
            surface: None,
            pane: Some(pane.pane_id.clone()),
            cwd: entry.cwd,
            pid: None,
            state: muxa::AgentState::Idle,
            last_prompt: Some(entry.prompt),
            last_response: None,
            last_notification: None,
            model: entry.model,
            context_used_pct: None,
            cost_usd: None,
            rate_limit_5h_pct: None,
            rate_limit_5h_resets_at: None,
            rate_limit_7d_pct: None,
            rate_limit_7d_resets_at: None,
            rate_limited_until: None,
            rate_limit_scope: None,
            rate_limit_source: None,
            started_at: entry.at,
            last_activity_at: entry.at,
            state_entered_at: entry.at,
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
        auth: args
            .dashboard_auth
            .as_deref()
            .map(parse_dashboard_auth)
            .transpose()?,
        token: args.dashboard_token.clone(),
        allow_public,
    };

    DashboardConfig::resolve(&cfg.dashboard, &overrides)
        .map_err(|e| anyhow::anyhow!(e).context("resolving dashboard config"))
}

fn parse_dashboard_auth(s: &str) -> Result<DashboardAuthMode> {
    match s.trim().to_ascii_lowercase().as_str() {
        "token" | "bearer" => Ok(DashboardAuthMode::Token),
        "none" | "off" | "public" => Ok(DashboardAuthMode::None),
        _ => anyhow::bail!("invalid dashboard auth mode {s:?}; expected `token` or `none`"),
    }
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

/// Resolve the webhook (Slack/Discord) sink config and, if enabled,
/// spawn its task. Mirrors `spawn_oh_my_prompt_sink`: failures to
/// resolve are surfaced at startup so a typo in the URL doesn't lead
/// to silent missed notifications.
fn spawn_webhook_sink(
    cfg: &Config,
    store: &muxa::SharedStore,
    shutdown_tx: &broadcast::Sender<()>,
) -> Result<()> {
    match WebhookSink::resolve(&cfg.sinks.webhook) {
        Ok(Some(sink)) => {
            let transition_rx = store.subscribe();
            let shutdown_rx = shutdown_tx.subscribe();
            // `webhook_sink::spawn` returns the JoinHandle; we drop it
            // because the sink lifetime is bounded by the shutdown
            // broadcast (matching the ohmyprompt pattern).
            std::mem::drop(webhook_sink::spawn(sink, transition_rx, shutdown_rx));
            tracing::info!("webhook sink enabled");
        }
        Ok(None) => {}
        Err(e) => {
            return Err(anyhow::Error::new(e).context("resolving webhook sink"));
        }
    }
    Ok(())
}

/// Inject `MUXA_SOCKET` into an already-running tmux server's global
/// environment. Called once at daemon startup so panes that were
/// spawned before this muxad instance can still reach it via hooks.
fn heal_tmux_socket_env(socket: &std::path::Path) {
    let server_up = muxa::tmux::tmux_command()
        .arg("info")
        .output()
        .is_ok_and(|o| o.status.success());
    if !server_up {
        tracing::debug!("tmux server not running — skipping socket env heal");
        return;
    }
    let Some(s) = socket.to_str() else {
        tracing::debug!("socket path is not UTF-8 — skipping socket env heal");
        return;
    };
    match muxa::tmux::tmux_command()
        .args(["set-environment", "-g", "MUXA_SOCKET", s])
        .status()
    {
        Ok(st) if st.success() => {
            tracing::info!(socket = s, "healed MUXA_SOCKET in tmux server env");
        }
        Ok(st) => {
            tracing::warn!(
                socket = s,
                status = ?st.code(),
                "tmux set-environment MUXA_SOCKET failed"
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not run tmux set-environment");
        }
    }
}

/// Heal the tmux global `MUXA_SOCKET` — but only for the canonical
/// daemon (see [`should_heal_tmux_socket_env`]). A non-canonical instance
/// (an explicit `--socket`/`MUXA_SOCKET` override matching neither the
/// default nor the configured socket — a dashboard demo, an e2e test, a
/// throwaway `muxad --socket /tmp/…`) logs and returns without touching
/// the global env, so its short-lived socket can't strand later panes
/// once it exits.
fn maybe_heal_tmux_socket_env(socket: &Path, cfg_socket: Option<&Path>) {
    if should_heal_tmux_socket_env(socket, &paths::default_socket(), cfg_socket) {
        heal_tmux_socket_env(socket);
    } else {
        tracing::debug!(
            socket = %socket.display(),
            "non-canonical socket override — skipping tmux env heal to avoid clobbering the primary daemon's global pin"
        );
    }
}

/// Whether this muxad instance may write its socket into the tmux
/// server's global `MUXA_SOCKET` (see [`heal_tmux_socket_env`]).
///
/// Only the *canonical* daemon heals: one on the XDG/default socket, or
/// on the socket named in config. An instance started with an explicit
/// `--socket` / `MUXA_SOCKET` override matching neither — a dashboard
/// demo, an e2e test, a throwaway `muxad --socket /tmp/…` — must NOT
/// touch the global env. That env (and `muxa init`'s tmux.conf pin)
/// belong to the primary daemon; when the ephemeral instance exits its
/// socket is unlinked, and every pane spawned while the global pointed at
/// it can no longer reach any daemon until the global is re-pinned.
fn should_heal_tmux_socket_env(
    socket: &Path,
    default_socket: &Path,
    cfg_socket: Option<&Path>,
) -> bool {
    socket == default_socket || cfg_socket == Some(socket)
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

/// Spawn the periodic discovery rescan. Startup discovery covers t=0; this
/// keeps newly-created panes appearing in `muxa status` within
/// `[discovery] interval_secs` instead of only after the agent's first hook.
/// Cheap — it reuses the same `tmux list-panes` the reconciler already runs.
/// No-op when discovery is disabled or `interval_secs == 0`.
fn spawn_periodic_discovery(
    cfg: &Config,
    socket: PathBuf,
    backend: muxa::SharedBackend,
    shutdown_tx: &broadcast::Sender<()>,
) {
    if !cfg.discovery.enabled || cfg.discovery.interval_secs == 0 {
        return;
    }
    let interval_secs = cfg.discovery.interval_secs;
    let mut shutdown_rx = shutdown_tx.subscribe();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Skip the immediate first fire — startup discovery already ran t=0.
        tick.tick().await;
        let client = Client::new(socket);
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    match discovery::run_discovery(&client, backend.as_ref()).await {
                        Ok(report) => tracing::debug!(
                            claude_code = report.claude_code,
                            codex = report.codex,
                            gemini_cli = report.gemini_cli,
                            skipped_known = report.skipped_known,
                            "periodic discovery pass",
                        ),
                        Err(e) => tracing::warn!(error = %e, "periodic discovery failed"),
                    }
                }
                _ = shutdown_rx.recv() => {
                    tracing::debug!("periodic discovery shutting down");
                    break;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use muxa::config::DiscoveryConfig;

    #[test]
    fn heals_canonical_default_or_configured_socket() {
        let default = Path::new("/run/user/1000/muxa.sock");
        let configured = Path::new("/custom/primary.sock");
        // Default socket → the primary daemon, may heal.
        assert!(should_heal_tmux_socket_env(default, default, None));
        // Socket named in config → also the primary, may heal even when
        // it differs from the XDG/default path.
        assert!(should_heal_tmux_socket_env(
            configured,
            default,
            Some(configured)
        ));
    }

    #[test]
    fn skips_ephemeral_override_socket() {
        // A dashboard demo / e2e daemon on a throwaway socket matching
        // neither the default nor the configured one must NOT clobber the
        // tmux global env the primary daemon owns — that poisoning is
        // exactly what strands later panes on a dead `muxa-dash.sock`.
        let default = Path::new("/run/user/1000/muxa.sock");
        let configured = Path::new("/custom/primary.sock");
        let ephemeral = Path::new("/tmp/.tmpXYZ/muxa-dash.sock");
        assert!(!should_heal_tmux_socket_env(ephemeral, default, None));
        assert!(!should_heal_tmux_socket_env(
            ephemeral,
            default,
            Some(configured)
        ));
    }

    #[tokio::test]
    async fn startup_discovery_runs_when_enabled() {
        let cfg = Config {
            discovery: DiscoveryConfig {
                enabled: true,
                ..DiscoveryConfig::default()
            },
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
            discovery: DiscoveryConfig {
                enabled: false,
                ..DiscoveryConfig::default()
            },
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
