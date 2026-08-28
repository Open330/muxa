//! muxa daemon.
//!
//! Listens on a unix socket, ingests normalized `AgentEvent`s from adapters,
//! exposes query endpoints to the CLI and tmux status line, and — when
//! opted in via `[dashboard] enabled = true` or `--dashboard` — serves a
//! HTTP dashboard alongside the unix socket, with independently configurable
//! read and PAT-gated control access.

use anyhow::{Context, Result};
use clap::Parser;
use muxa::activity::{
    ActivityEntry, ActivityLog, ActivityOptions, StateTransitionEntry, StateTransitionInput,
};
use muxa::ask::{AskOptions, AskStore};
use muxa::collaboration::{
    CollaborationClientKind, CollaborationOptions, CollaborationOriginMatch, CollaborationRequest,
    CollaborationStore, WakeDeliveryState,
};
use muxa::collaboration_audit::CollaborationAuditLog;
use muxa::config::{CollaborationWake, CollaborationWakePayload};
use muxa::config::{DashboardAuthMode, NotifierBackend};
use muxa::dashboard::{DashboardConfig, DashboardOverrides};
use muxa::history::{HistoryOptions, PaneSessionCache, PromptHistory};
use muxa::ipc::{harden_permissions, Client, RestartController, Server};
use muxa::notify::Notifier;
use muxa::pipeline_run::PipelineRunStore;
use muxa::reconcile::Reconciler;
use muxa::sinks::{webhook as webhook_sink, OhMyPromptSink, WebhookSink};
use muxa::snapshot::{self, Snapshotter, SnapshotterOptions};
use muxa::tmux::scanner::PaneCache;
use muxa::{discovery, paths, Config, Store};
use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::broadcast;

mod fleet_manager;
mod herdr_bridge;
mod screen_detect;
mod synthetic;

/// Inactivity window before a stopped agent is evicted from the in-memory store.
const STOPPED_AGENT_TTL_MINUTES: i64 = 60;
/// Cadence at which the GC task scans for evictable agents.
const GC_SWEEP_INTERVAL_SECONDS: u64 = 60;
const PANE_SESSION_CACHE_INTERVAL_SECONDS: u64 = 5;
const SHUTDOWN_TASK_TIMEOUT_SECONDS: u64 = 2;
/// Slow safety-net scan for collaboration wake delivery. Normal delivery is
/// driven by mailbox revisions and agent transitions; this only reconciles a
/// rare dropped/closed transition or state restored during startup.
const COLLABORATION_WAKE_RECONCILE_SECONDS: u64 = 30;
/// Carries the daemon image identity across an in-place re-exec.
const RESTART_GENERATION_ENV: &str = "MUXA_RESTART_GENERATION";

#[derive(Debug, Parser)]
#[command(name = "muxad", version, about = "muxa daemon")]
struct Args {
    /// Unix socket path. Overrides config and XDG default.
    #[arg(long, env = "MUXA_SOCKET")]
    socket: Option<PathBuf>,

    /// Config file path. Defaults to `$XDG_CONFIG_HOME/muxa/config.toml`.
    #[arg(long, env = "MUXA_CONFIG")]
    config: Option<PathBuf>,

    /// Enable the HTTP dashboard. Requires a bearer token unless dashboard
    /// auth is explicitly set to read-only `none`.
    #[arg(long, conflicts_with = "no_dashboard")]
    dashboard: bool,

    /// Force the HTTP dashboard off, even if the config file enabled it.
    #[arg(long, conflicts_with = "dashboard")]
    no_dashboard: bool,

    /// Dashboard bind address as `ip:port`. Defaults to `127.0.0.1:7878`.
    #[arg(long, value_name = "ADDR", env = "MUXA_DASHBOARD_BIND")]
    dashboard_bind: Option<String>,

    /// Dashboard bearer token / browser PAT. Required by `token` and
    /// `public_read` auth modes.
    #[arg(long, value_name = "TOKEN", env = "MUXA_DASHBOARD_TOKEN")]
    dashboard_token: Option<String>,

    /// Dashboard API auth mode: `token`, `public_read`, or `none`.
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
    // Resolve CLI/env dashboard overrides before spawning any background
    // task. Invalid auth bootstrap should fail without briefly starting
    // writers, scanners, or notifiers that the runtime would then abort.
    let dash_cfg = resolve_dashboard_config(&cfg, &args)?;

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
    // Persistence consumers intentionally outlive the general task graph.
    // The signal broadcast stops producers first; these channels are fired
    // only after IPC and direct Store producers have drained.
    let (activity_transition_shutdown_tx, _) = broadcast::channel::<()>(1);
    let (writer_shutdown_tx, _) = broadcast::channel::<()>(1);

    let restart = Arc::new(RestartController::new(
        restart_generation(),
        shutdown_tx.clone(),
    ));
    install_shutdown_signal_handler(Arc::clone(&restart));

    // Prompt history must exist before the store: every PromptSubmitted
    // event fans out into history alongside the live agent record.
    let pane_session_cache = PaneSessionCache::default();
    let (history, history_writer_handle) =
        build_history(&cfg, &writer_shutdown_tx, pane_session_cache.clone()).await;
    let store = Store::shared_with_history(history.clone());
    let collaboration = build_collaboration(&cfg).await;
    let collaboration_audit = build_collaboration_audit(&cfg);
    let ask = build_ask(&cfg).await;
    let pipeline_runs = PipelineRunStore::load(paths::default_pipeline_run_file())
        .context("loading durable pipeline Runs")?;

    // The set of backends this daemon observes simultaneously — tmux + herdr
    // during a migration (see `docs/MULTI_HOST.md`). Resolution honors
    // `MUXA_HOSTS` > `MUXA_HOST` > auto-detect and is never empty. Each element
    // is a cheap-to-clone `Arc`, so enumeration-shaped consumers (reconciler,
    // discovery, pane-session cache, history enrichment) iterate the whole set,
    // while the few consumers that genuinely need a single handle take the
    // `primary` (first) backend — or, for a host-specific consumer, the
    // matching backend from the set.
    let backends: Vec<muxa::SharedBackend> = muxa::active_backends();
    // Never empty (see `active_backends`); `primary` is the conventional
    // single-backend handle for consumers a namespace can't disambiguate.
    // `active_backends` orders the set by env preference, so `backends[0]` is
    // the env-preferred host (e.g. herdr when running herdr-inside-herdr) — the
    // same host the old single-backend daemon detected. Consumers that must
    // match that legacy single-host behavior (the web dashboard scanner) take
    // `primary`.
    let primary: muxa::SharedBackend = backends[0].clone();
    let sessions = muxa::PtySessionBackend::shared();
    let observing_kinds: Vec<muxa::HostKind> = backends.iter().map(|b| b.kind()).collect();
    tracing::info!(hosts = ?observing_kinds, "pane backends selected");
    refresh_pane_session_cache(&pane_session_cache, &backends).await;
    spawn_pane_session_cache_task(pane_session_cache.clone(), backends.clone(), &shutdown_tx);

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
    enrich_from_history(&store, &history, &backends).await;

    // The controller node is always a first-class Fleet host, even when
    // outbound SSH connections are disabled. Start after hydration so its
    // first published snapshot already contains restored local agents.
    let (fleet_runtime, fleet_handle) = fleet_manager::start(
        &cfg.fleet,
        store.clone(),
        backends.clone(),
        Client::new(socket.clone()),
        restart.generation(),
        shutdown_tx.subscribe(),
    )
    .await;

    let (activity_log, activity_writer_handle) =
        build_activity_log(&cfg, &writer_shutdown_tx).await;
    let activity_transition_handle = spawn_activity_transition_task(
        &store,
        activity_log.clone(),
        pane_session_cache.clone(),
        &activity_transition_shutdown_tx,
    )
    .await;
    let gc_handle = spawn_gc_task(&store, &shutdown_tx);
    let reconciler_handle = spawn_reconciler_task(&cfg, &store, &shutdown_tx, backends.clone());
    // herdr event bridge (Phase 2): spawned when herdr ∈ the observed set.
    // Translates herdr's own agent-state detection into synthetic muxa rows so
    // agents muxa has no hooks for still appear in status/watch/stats. Spawned
    // before the IPC server takes ownership of `store` so it shares the same
    // registry.
    let herdr_bridge_handle =
        herdr_bridge::spawn_herdr_bridge_task(&backends, store.clone(), &shutdown_tx);
    // herdr reverse path: push muxa's authoritative hook-derived state for REAL
    // (non-synthetic) `herdr:` rows back into herdr's UI via `pane.report_agent`,
    // releasing authority when the row stops. Subscribes to the same store
    // transition stream the notifier/activity tasks use. Spawned when herdr ∈ set.
    let herdr_report_handle =
        herdr_bridge::spawn_herdr_report_task(&backends, store.clone(), &shutdown_tx);
    // Screen-manifest fallback detection: for agent CLIs muxa has no hooks for
    // (cursor-agent, amp, copilot, …), capture matching panes and classify their
    // screen into synthetic rows. Skips herdr hosts (the herdr bridge covers
    // them) and any pane a live hook owns. Spawned before the IPC server takes
    // ownership of `store` so it shares the same registry.
    let screen_detect_handle =
        screen_detect::spawn_screen_detect_task(&cfg, &backends, store.clone(), &shutdown_tx);
    let session_activity_handle =
        spawn_session_activity_task(&cfg, &shutdown_tx, activity_log.clone(), &backends);
    let history_compaction_handle = spawn_history_compaction_task(&cfg, &store, &shutdown_tx);
    let activity_compaction_handle =
        spawn_activity_compaction_task(&cfg, activity_log.clone(), &shutdown_tx);
    let collaboration_waker_handle = spawn_collaboration_waker_task(
        &cfg,
        collaboration.clone(),
        store.clone(),
        backends.clone(),
        &shutdown_tx,
    );
    let pipeline_state_handle =
        spawn_pipeline_state_task(pipeline_runs.clone(), store.clone(), &shutdown_tx);

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

    // HTTP dashboard. A bind failure is non-fatal to unix-socket IPC.
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
        // Reads use the env-preferred backend as their primary scanner source;
        // PAT-gated control actions retain the full set so pane-id namespaces
        // route to the correct host during mixed-host migrations.
        let backends_for_dash = backends.clone();
        let dashboard_runtime = muxa::dashboard::DashboardRuntimeConfig {
            message_skills: cfg.message.skills.clone(),
            activity_path: dashboard_activity_path,
            session_activity_path: dashboard_session_activity_path,
            work_store_path: paths::default_dashboard_work_file(),
            stats_config: dashboard_stats_config,
            fleet: Some(fleet_runtime.clone()),
        };
        tokio::spawn(async move {
            if let Err(e) = muxa::dashboard::serve(
                dash_cfg_for_task,
                store_for_dash,
                pane_cache,
                sessions_for_dash,
                backends_for_dash,
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

    // The IPC server's backend is consumed for exactly one thing: routing an
    // inbound `BackendPaneSnapshot` push into `PaneBackend::ingest_pane_snapshot`
    // — a no-op on every backend except zellij (whose WASM plugin is the only
    // external snapshot source). Route those pushes to the zellij backend in the
    // set so they land in the same instance discovery/reconciler read from;
    // fall back to `primary` when zellij isn't observed (the ingest is then an
    // inert no-op anyway).
    let ipc_backend = backends
        .iter()
        .find(|b| b.kind() == muxa::HostKind::Zellij)
        .cloned()
        .unwrap_or_else(|| primary.clone());
    let server = Server::new(socket.clone(), store)
        .with_backend(ipc_backend)
        // The full observed set, so control methods (`send_prompt`,
        // `capture`) resolve the backend per pane-id namespace. `backends`
        // is ordered by env preference, so `backends[0]` is the primary
        // fallback for unclassifiable ids.
        .with_backends(backends.clone())
        .with_sessions(sessions)
        .with_collaboration(collaboration)
        .with_collaboration_audit(collaboration_audit)
        .with_ask(ask)
        .with_fleet(fleet_runtime)
        .with_pipeline_runs(pipeline_runs.clone())
        .with_restart_controller(Arc::clone(&restart));
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
    // Subscribe only after the socket is accepting connections: the worker
    // delegates physical launch to the sibling `muxa` CLI over this IPC path.
    // Its initial authoritative scan still catches completions committed
    // between server start and subscription.
    let pipeline_reconciler_handle =
        spawn_pipeline_reconciler_task(pipeline_runs, socket.clone(), &shutdown_tx);

    // Self-heal: if a tmux server is already running, inject our socket
    // path into its environment so that every pane — including any that
    // were started before this muxad instance — can reach us. This
    // complements the `tmux-env` managed block in `~/.tmux.conf` which
    // handles fresh server boots; together they cover both cold-start and
    // warm-restart scenarios without requiring the user to re-run
    // `muxa init` after every daemon or tmux server restart.
    if listener_ready {
        maybe_heal_tmux_socket_env(&socket, cfg.socket.as_deref());
        spawn_startup_discovery(&cfg, socket.clone(), backends.clone(), &shutdown_tx);
        spawn_periodic_discovery(&cfg, socket.clone(), backends.clone(), &shutdown_tx);
    }

    // Shutdown sequence (each step depends on the previous):
    //   1. Drain IPC and await direct Store/activity producers that received
    //      the general shutdown signal.
    //   2. Stop and drain the activity-transition subscriber so every final
    //      Store transition is queued to the activity writer.
    //   3. Stop and await history/activity writers. Their biased select drains
    //      every queued message before observing this dedicated signal.
    //   4. Flush the final state snapshot last.
    let server_result = handle.await;
    // The normal signal path already sent this broadcast. Re-sending is
    // harmless and also stops the task graph when the IPC server exits on an
    // internal error rather than SIGTERM/SIGINT.
    let _ = shutdown_tx.send(());
    await_shutdown_task("gc", Some(gc_handle)).await;
    await_shutdown_task("reconciler", reconciler_handle).await;
    await_shutdown_task("herdr bridge", herdr_bridge_handle).await;
    await_shutdown_task("herdr report", herdr_report_handle).await;
    await_shutdown_task("screen detection", screen_detect_handle).await;
    await_shutdown_task("session activity", session_activity_handle).await;
    await_shutdown_task("history compaction", history_compaction_handle).await;
    await_shutdown_task("activity compaction", activity_compaction_handle).await;
    await_shutdown_task("collaboration waker", collaboration_waker_handle).await;
    await_shutdown_task("pipeline reconciler", Some(pipeline_reconciler_handle)).await;
    await_shutdown_task("pipeline state projection", Some(pipeline_state_handle)).await;
    await_shutdown_task("fleet manager", Some(fleet_handle)).await;

    let _ = activity_transition_shutdown_tx.send(());
    await_shutdown_task("activity transition", activity_transition_handle).await;

    let _ = writer_shutdown_tx.send(());
    await_shutdown_task("history writer", history_writer_handle).await;
    await_shutdown_task("activity writer", activity_writer_handle).await;

    let _ = snap_shutdown_tx.send(());
    await_shutdown_task("state snapshotter", snap_handle).await;
    server_result??;
    if restart.restart_requested() {
        return Err(reexec_self().into());
    }
    Ok(())
}

/// Image generation advertised in IPC `hello`: zero on a fresh process and
/// incremented for each successful self-reexec.
fn restart_generation() -> u64 {
    std::env::var(RESTART_GENERATION_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

/// Replace this process with the binary now resolved by its original argv[0].
/// `exec` preserves pid, argv, environment, working directory and the service
/// manager's ownership while loading the newly installed inode.
fn reexec_self() -> std::io::Error {
    use std::os::unix::process::CommandExt;

    let mut argv = std::env::args_os();
    let Some(program) = argv.next() else {
        return std::io::Error::other("cannot restart: argv[0] is missing");
    };
    let next = restart_generation().saturating_add(1);
    tracing::info!(?program, generation = next, "restarting: re-executing self");
    std::process::Command::new(&program)
        .args(argv)
        .env(RESTART_GENERATION_ENV, next.to_string())
        .exec()
}

async fn await_shutdown_task(name: &'static str, handle: Option<tokio::task::JoinHandle<()>>) {
    let Some(mut handle) = handle else {
        return;
    };
    let timeout = std::time::Duration::from_secs(SHUTDOWN_TASK_TIMEOUT_SECONDS);
    match tokio::time::timeout(timeout, &mut handle).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(task = name, %error, "shutdown task failed"),
        Err(_) => {
            tracing::warn!(
                task = name,
                timeout_secs = SHUTDOWN_TASK_TIMEOUT_SECONDS,
                "shutdown task timed out; aborting",
            );
            handle.abort();
            let _ = handle.await;
        }
    }
}

/// Resolve `[ask]` into a live store. Mirrors `build_collaboration`:
/// the history path is only materialized when the feature is on, so a
/// disabled ask never creates a file.
async fn build_ask(cfg: &Config) -> Arc<AskStore> {
    let options = AskOptions {
        enabled: cfg.ask.enabled,
        agent: cfg.ask.agent.clone(),
        cwd: cfg
            .ask
            .cwd
            .clone()
            .unwrap_or_else(|| AskOptions::default().cwd),
        permission_mode: cfg.ask.permission_mode,
        additional_dirs: cfg.ask.additional_dirs.clone(),
        timeout_secs: cfg.ask.timeout_secs,
        path: cfg
            .ask
            .enabled
            .then(|| cfg.ask.path.clone().or_else(muxa::paths::default_ask_file))
            .flatten(),
        keep: cfg.ask.keep,
    };
    let store = AskStore::load(options).await;
    if cfg.ask.enabled {
        tracing::info!(
            agent = %cfg.ask.agent,
            cwd = %cfg.ask.cwd.as_deref().unwrap_or_else(|| std::path::Path::new("$HOME")).display(),
            permission_mode = ?cfg.ask.permission_mode,
            additional_dirs = ?cfg.ask.additional_dirs,
            timeout_secs = cfg.ask.timeout_secs,
            "headless ask enabled",
        );
    }
    store
}

async fn build_collaboration(cfg: &Config) -> Arc<CollaborationStore> {
    let options = CollaborationOptions {
        enabled: cfg.collaboration.enabled,
        scope: cfg.collaboration.scope,
        path: cfg
            .collaboration
            .enabled
            .then(|| {
                cfg.collaboration
                    .path
                    .clone()
                    .or_else(paths::default_collaboration_file)
            })
            .flatten(),
        max_message_bytes: cfg.collaboration.max_message_bytes,
    };
    match CollaborationStore::load(options.clone()).await {
        Ok(store) => {
            if cfg.collaboration.enabled {
                tracing::info!(
                    path = ?options.path,
                    wake = ?cfg.collaboration.wake,
                    wake_payload = ?cfg.collaboration.wake_payload,
                    "agent collaboration enabled",
                );
            }
            store
        }
        Err(error) => {
            tracing::warn!(%error, "could not load collaboration mailbox; using memory only");
            CollaborationStore::in_memory(options)
        }
    }
}

fn build_collaboration_audit(cfg: &Config) -> Arc<CollaborationAuditLog> {
    if cfg.collaboration.enabled {
        let path = cfg
            .collaboration
            .path
            .as_ref()
            .map(|mailbox| mailbox.with_file_name(paths::COLLABORATION_AUDIT_FILENAME))
            .or_else(paths::default_collaboration_audit_file);
        if let Some(path) = path {
            tracing::info!(path = %path.display(), "collaboration audit enabled");
            return CollaborationAuditLog::at_path(path);
        }
    }
    CollaborationAuditLog::in_memory()
}

/// Completion changes wake a daemon-owned reconciliation loop. The worker
/// deliberately invokes the installed `muxa` binary instead of duplicating
/// its allowlisted agent-launch policy inside muxad; the CLI atomically claims
/// ready aliases over IPC before touching tmux, so a user-triggered `work up`
/// racing this worker cannot launch a duplicate.
fn spawn_pipeline_reconciler_task(
    pipeline_runs: Arc<PipelineRunStore>,
    socket: PathBuf,
    shutdown_tx: &broadcast::Sender<()>,
) -> tokio::task::JoinHandle<()> {
    let mut changes = pipeline_runs.subscribe();
    let mut shutdown = shutdown_tx.subscribe();
    tokio::spawn(async move {
        let mut safety_scan = tokio::time::interval(std::time::Duration::from_secs(30));
        safety_scan.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let ready = pipeline_runs
                .list()
                .await
                .iter()
                .any(muxa::pipeline_run::PipelineRun::has_ready_alias);
            if ready {
                run_pipeline_reconciler(&socket).await;
            }
            tokio::select! {
                revision = changes.changed() => {
                    if revision.is_err() {
                        break;
                    }
                }
                _ = safety_scan.tick() => {}
                _ = shutdown.recv() => break,
            }
        }
    })
}

fn spawn_pipeline_state_task(
    pipeline_runs: Arc<PipelineRunStore>,
    store: muxa::SharedStore,
    shutdown_tx: &broadcast::Sender<()>,
) -> tokio::task::JoinHandle<()> {
    let mut transitions = store.subscribe();
    let mut shutdown = shutdown_tx.subscribe();
    tokio::spawn(async move {
        // Rehydrate the projection immediately. This covers agent state loaded
        // from state.json before the subscriber was installed.
        for agent in store.snapshot().await {
            if let Some(pane) = agent.pane.as_deref() {
                observe_pipeline_agent(&pipeline_runs, pane, agent.state).await;
            }
        }
        loop {
            tokio::select! {
                transition = transitions.recv() => match transition {
                    Ok(transition) => {
                        if let Some(pane) = transition.agent.pane.as_deref() {
                            observe_pipeline_agent(&pipeline_runs, pane, transition.to).await;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        for agent in store.snapshot().await {
                            if let Some(pane) = agent.pane.as_deref() {
                                observe_pipeline_agent(&pipeline_runs, pane, agent.state).await;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                _ = shutdown.recv() => break,
            }
        }
    })
}

async fn observe_pipeline_agent(
    pipeline_runs: &PipelineRunStore,
    pane: &str,
    state: muxa::AgentState,
) {
    let status = match state {
        muxa::AgentState::WaitingInput | muxa::AgentState::WaitingChoice => {
            muxa::pipeline_run::PipelineAliasStatus::Blocked
        }
        muxa::AgentState::Error | muxa::AgentState::Stopped => {
            muxa::pipeline_run::PipelineAliasStatus::Failed
        }
        muxa::AgentState::Starting | muxa::AgentState::Working | muxa::AgentState::Idle => {
            muxa::pipeline_run::PipelineAliasStatus::Running
        }
    };
    if let Err(error) = pipeline_runs.observe_pane(pane, status).await {
        tracing::warn!(pane, %error, "could not project agent state into pipeline Run");
    }
}

async fn run_pipeline_reconciler(socket: &Path) {
    let program = std::env::var_os("MUXA_PIPELINE_CLI").map_or_else(
        || {
            std::env::current_exe()
                .ok()
                .map(|path| path.with_file_name("muxa"))
                .filter(|path| path.exists())
                .unwrap_or_else(|| PathBuf::from("muxa"))
        },
        PathBuf::from,
    );
    match tokio::process::Command::new(&program)
        .args(["work", "reconcile", "--all"])
        .env("MUXA_SOCKET", socket)
        .output()
        .await
    {
        Ok(output) if output.status.success() => {
            tracing::debug!(program = %program.display(), "pipeline reconcile completed");
        }
        Ok(output) => {
            tracing::warn!(
                program = %program.display(),
                status = %output.status,
                stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                "pipeline reconcile command failed",
            );
        }
        Err(error) => {
            tracing::warn!(program = %program.display(), %error, "could not start pipeline reconciler");
        }
    }
}

fn spawn_collaboration_waker_task(
    cfg: &Config,
    collaboration: Arc<CollaborationStore>,
    store: muxa::SharedStore,
    backends: Vec<muxa::SharedBackend>,
    shutdown_tx: &broadcast::Sender<()>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !cfg.collaboration.enabled || cfg.collaboration.wake == CollaborationWake::Never {
        return None;
    }
    let mut shutdown_rx = shutdown_tx.subscribe();
    // Subscribe before spawning and before the initial authoritative scan, so
    // a request or state transition racing task startup is represented either
    // by that scan or by an already-pending signal.
    let mut mailbox_changes = collaboration.subscribe();
    let mut agent_transitions = store.subscribe();
    let wake_payload = cfg.collaboration.wake_payload;
    Some(tokio::spawn(async move {
        let mut wake_inflight = HashSet::new();
        wake_idle_collaboration_peers_with_inflight(
            &collaboration,
            &store,
            &backends,
            wake_payload,
            &mut wake_inflight,
        )
        .await;
        let mut reconcile = tokio::time::interval(std::time::Duration::from_secs(
            COLLABORATION_WAKE_RECONCILE_SECONDS,
        ));
        reconcile.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        reconcile.tick().await;
        loop {
            let should_scan = tokio::select! {
                changed = mailbox_changes.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    true
                }
                transition = agent_transitions.recv() => match transition {
                    Ok(transition) => {
                        let should_scan = transition.to == muxa::AgentState::Idle;
                        if matches!(
                            transition.to,
                            muxa::AgentState::Idle
                                | muxa::AgentState::Stopped
                                | muxa::AgentState::Error
                        ) {
                            if let Some(key) = collaboration_wake_key_for_agent(&transition.agent) {
                                wake_inflight.remove(&key);
                            }
                        }
                        should_scan
                    }
                    // A lag means the exact transition to Idle may have been
                    // dropped. Re-read the authoritative state immediately.
                    Err(broadcast::error::RecvError::Lagged(dropped)) => {
                        tracing::debug!(dropped, "collaboration waker transition stream lagged");
                        wake_inflight.clear();
                        true
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                _ = reconcile.tick() => {
                    // A successfully submitted prompt should produce a hook
                    // transition. The periodic reset is the bounded recovery
                    // path if that event was lost; the authoritative state
                    // snapshot below still refuses non-idle recipients.
                    wake_inflight.clear();
                    true
                },
                _ = shutdown_rx.recv() => break,
            };
            if should_scan {
                wake_idle_collaboration_peers_with_inflight(
                    &collaboration,
                    &store,
                    &backends,
                    wake_payload,
                    &mut wake_inflight,
                )
                .await;
            }
        }
    }))
}

#[cfg(test)]
async fn wake_idle_collaboration_peers(
    collaboration: &CollaborationStore,
    store: &muxa::SharedStore,
    backends: &[muxa::SharedBackend],
) {
    let mut wake_inflight = HashSet::new();
    wake_idle_collaboration_peers_with_inflight(
        collaboration,
        store,
        backends,
        CollaborationWakePayload::Notice,
        &mut wake_inflight,
    )
    .await;
}

#[cfg(test)]
async fn wake_idle_collaboration_peers_with_full_payload(
    collaboration: &CollaborationStore,
    store: &muxa::SharedStore,
    backends: &[muxa::SharedBackend],
) {
    let mut wake_inflight = HashSet::new();
    wake_idle_collaboration_peers_with_inflight(
        collaboration,
        store,
        backends,
        CollaborationWakePayload::Full,
        &mut wake_inflight,
    )
    .await;
}

async fn wake_idle_collaboration_peers_with_inflight(
    collaboration: &CollaborationStore,
    store: &muxa::SharedStore,
    backends: &[muxa::SharedBackend],
    wake_payload: CollaborationWakePayload,
    wake_inflight: &mut HashSet<(String, Option<String>, String)>,
) {
    let requests = collaboration.pending_unnotified().await;
    let replies = collaboration.pending_reply_unnotified().await;
    if requests.is_empty() && replies.is_empty() {
        return;
    }
    let agents = store.snapshot().await;
    let listed_backends = backends.to_vec();
    let panes = tokio::task::spawn_blocking(move || {
        listed_backends
            .iter()
            .flat_map(|backend| backend.list_panes())
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    let mut participants = muxa::collaboration::participants_from(&agents, &panes);
    collaboration.enrich_participants(&mut participants).await;

    for request in requests {
        let Some(recipient) =
            ready_collaboration_recipient(&participants, &agents, &panes, &request)
        else {
            continue;
        };
        let recipient = &recipient;
        let wake_key = collaboration_wake_key(recipient);
        if wake_inflight.contains(&wake_key) {
            continue;
        }
        let direct_wake = collaboration_wake_includes_body(wake_payload, &request);
        if direct_wake && direct_wake_body_is_terminal_safe(&request.body) {
            if deliver_full_collaboration_request(collaboration, recipient, &request, backends)
                .await
            {
                wake_inflight.insert(wake_key);
            }
            continue;
        }
        if direct_wake {
            tracing::debug!(
                request_id = request.id,
                "direct wake body contains terminal control characters; using mailbox notice",
            );
        }
        let prompt = format!(
            "[muxa:{}] New {:?} request {}. Claim/read it with muxa_inbox (MCP) or `muxa msg inbox --json`; honor kind/work_mode/paths, then respond with muxa_reply or `muxa msg reply`.",
            request.id,
            request.kind,
            collaboration_request_source(&request),
        );
        let (sent, submitted) = send_collaboration_wake(recipient, &prompt, backends).await;
        if submitted {
            wake_inflight.insert(wake_key);
        }
        if sent {
            if let Err(error) = collaboration.mark_notified(&request.id).await {
                tracing::warn!(request_id = request.id, %error, "failed to persist wake marker");
            }
            tracing::debug!(
                request_id = request.id,
                pane = recipient.pane,
                submitted,
                "collaboration wake delivered",
            );
        }
    }

    wake_senders_of_ready_replies(
        collaboration,
        &participants,
        backends,
        replies,
        wake_inflight,
    )
    .await;
}

/// Tell each sender that a terminal reply is waiting. Reply bodies always stay
/// in the mailbox, so this is a notice pass with no payload policy.
async fn wake_senders_of_ready_replies(
    collaboration: &CollaborationStore,
    participants: &[muxa::collaboration::Participant],
    backends: &[muxa::SharedBackend],
    replies: Vec<CollaborationRequest>,
    wake_inflight: &mut HashSet<(String, Option<String>, String)>,
) {
    for request in replies {
        let Some(sender) = idle_collaboration_participant(participants, &request.from) else {
            continue;
        };
        let wake_key = collaboration_wake_key(sender);
        if wake_inflight.contains(&wake_key) {
            continue;
        }
        let prompt = format!(
            "[muxa:{}] {:?} reply from {} is ready. Read it with muxa_wait_reply (MCP) or `muxa msg wait {}`.",
            request.id,
            request.status,
            request.to.label(),
            request.id,
        );
        let (sent, submitted) = send_collaboration_wake(sender, &prompt, backends).await;
        if submitted {
            wake_inflight.insert(wake_key);
        }
        if sent {
            if let Err(error) = collaboration.mark_reply_notified(&request.id).await {
                tracing::warn!(
                    request_id = request.id,
                    %error,
                    "failed to persist reply wake marker",
                );
            }
            tracing::debug!(
                request_id = request.id,
                pane = sender.pane,
                submitted,
                "collaboration reply wake delivered",
            );
        }
    }
}

async fn deliver_full_collaboration_request(
    collaboration: &CollaborationStore,
    recipient: &muxa::collaboration::Participant,
    pending: &CollaborationRequest,
    backends: &[muxa::SharedBackend],
) -> bool {
    let (request, newly_prepared) = if pending.status == muxa::collaboration::RequestStatus::Queued
    {
        match collaboration
            .prepare_direct_wake(recipient, &pending.id)
            .await
        {
            Ok(Some(request)) => (request, true),
            Ok(None) => return false,
            Err(error) => {
                tracing::warn!(
                    request_id = pending.id,
                    %error,
                    "failed to reserve direct collaboration wake",
                );
                return false;
            }
        }
    } else {
        (pending.clone(), false)
    };

    match request.wake_delivery {
        Some(WakeDeliveryState::Prepared) => {
            let prompt = if newly_prepared {
                full_collaboration_request_prompt(&request)
            } else {
                interrupted_collaboration_request_prompt(&request)
            };
            if !send_collaboration_text(recipient, &prompt, backends).await {
                return false;
            }
            if let Err(error) = collaboration.mark_wake_prompt_written(&request.id).await {
                tracing::warn!(
                    request_id = request.id,
                    %error,
                    "failed to persist direct wake prompt phase",
                );
            }
            tokio::time::sleep(muxa::backend::PROMPT_SUBMIT_GRACE).await;
            submit_direct_collaboration_prompt(collaboration, recipient, &request.id, backends)
                .await
        }
        Some(WakeDeliveryState::PromptWritten) => {
            // The body or recovery notice is already in the input buffer.
            // Retrying only Enter avoids executing the request twice.
            submit_direct_collaboration_prompt(collaboration, recipient, &request.id, backends)
                .await
        }
        None => false,
    }
}

async fn submit_direct_collaboration_prompt(
    collaboration: &CollaborationStore,
    recipient: &muxa::collaboration::Participant,
    request_id: &str,
    backends: &[muxa::SharedBackend],
) -> bool {
    if !send_collaboration_text(recipient, "\r", backends).await {
        return false;
    }
    mark_collaboration_request_notified(collaboration, request_id).await;
    true
}

async fn mark_collaboration_request_notified(collaboration: &CollaborationStore, request_id: &str) {
    if let Err(error) = collaboration.mark_notified(request_id).await {
        tracing::warn!(request_id, %error, "failed to persist wake marker");
    }
}

fn full_collaboration_request_prompt(request: &CollaborationRequest) -> String {
    let paths = serde_json::to_string(&request.paths).unwrap_or_else(|_| "[]".into());
    let air_artifacts = if request.air_artifacts.is_empty() {
        String::new()
    } else {
        format!(
            "\nair_artifacts: {}",
            serde_json::to_string(&request.air_artifacts).unwrap_or_else(|_| "[]".into())
        )
    };
    let reply = if request.expects_reply {
        format!(
            "When finished, respond with muxa_reply for request {} or `muxa msg reply {0} <body>`.",
            request.id
        )
    } else {
        "No reply is expected; do not send an acknowledgement solely for this delivery.".into()
    };
    format!(
        "[muxa:{}] New {} request {}. This request is already claimed; do not call muxa_inbox for it.\nkind: {}\nwork_mode: {}\npaths: {}{}\nexpects_reply: {}\n{}\n\n--- request body ({} bytes) ---\n{}\n--- end request body ---",
        request.id,
        collaboration_request_kind(request.kind),
        collaboration_request_source(request),
        collaboration_request_kind(request.kind),
        collaboration_work_mode(request.work_mode),
        paths,
        air_artifacts,
        request.expects_reply,
        reply,
        request.body.len(),
        request.body,
    )
}

fn interrupted_collaboration_request_prompt(request: &CollaborationRequest) -> String {
    format!(
        "[muxa:{}] Direct {} request delivery was interrupted after it was claimed. Read it with muxa_inbox (MCP) or `muxa msg inbox --json`; honor kind/work_mode/paths, then respond as requested.",
        request.id,
        collaboration_request_kind(request.kind),
    )
}

fn collaboration_request_kind(kind: muxa::collaboration::RequestKind) -> &'static str {
    match kind {
        muxa::collaboration::RequestKind::Question => "question",
        muxa::collaboration::RequestKind::Review => "review",
        muxa::collaboration::RequestKind::Task => "task",
        muxa::collaboration::RequestKind::Notice => "notice",
    }
}

fn collaboration_work_mode(mode: muxa::collaboration::WorkMode) -> &'static str {
    match mode {
        muxa::collaboration::WorkMode::ReadOnly => "read_only",
        muxa::collaboration::WorkMode::Execute => "execute",
    }
}

fn direct_wake_body_is_terminal_safe(body: &str) -> bool {
    body.chars()
        .all(|character| matches!(character, '\n' | '\t') || !character.is_control())
}

fn collaboration_wake_includes_body(
    wake_payload: CollaborationWakePayload,
    request: &CollaborationRequest,
) -> bool {
    // `operator_full` follows the sender identity resolved by muxad. Work
    // mode is intentionally irrelevant: `execute` describes requested work,
    // not proof that a human authorized direct prompt injection.
    match wake_payload {
        CollaborationWakePayload::Notice => false,
        CollaborationWakePayload::OperatorFull => request.from.console,
        CollaborationWakePayload::Full => true,
    }
}

fn collaboration_request_source(request: &CollaborationRequest) -> String {
    let represented = request.from.label();
    let Some(provenance) = request.provenance.as_ref() else {
        return format!("from {represented}");
    };
    let surface = match provenance.client_kind {
        CollaborationClientKind::Watch => "muxa watch",
        CollaborationClientKind::Mcp => "muxa MCP",
        CollaborationClientKind::Dashboard => "muxa dashboard",
        CollaborationClientKind::Cli => "muxa CLI",
        CollaborationClientKind::Unknown => "an unclassified muxa client",
    };
    let actor = match (&provenance.observed_pane, provenance.caller_pid) {
        (Some(pane), Some(pid)) => format!("caller {pane}, pid {pid}"),
        (Some(pane), None) => format!("caller {pane}"),
        (None, Some(pid)) => format!("caller pid {pid}"),
        (None, None) => "caller location unavailable".into(),
    };
    let mismatch = match provenance.origin_match {
        CollaborationOriginMatch::Matched => "",
        CollaborationOriginMatch::Mismatched => "; origin mismatch",
        CollaborationOriginMatch::Unverifiable => "; origin unverified",
    };
    format!("via {surface} representing {represented} ({actor}{mismatch})")
}

/// The participant a queued request may be delivered to *right now*.
///
/// The ordinary case is its session-pinned recipient, idle. A **pending**
/// recipient — a launched pane whose agent has not registered a session yet —
/// resolves two further ways: a real session that registered on that pane
/// after the request was queued, or the pane's own discovery/screen row once
/// it reads idle. The latter is what unblocks codex, whose `SessionStart` hook
/// cannot fire until something is typed at it. Discovery supplies the idle
/// row; screen detection's job here is the opposite one, holding delivery
/// while the pane sits on its startup approval gate.
fn ready_collaboration_recipient(
    participants: &[muxa::collaboration::Participant],
    agents: &[muxa::state::Agent],
    panes: &[muxa::tmux::PaneInfo],
    request: &CollaborationRequest,
) -> Option<muxa::collaboration::Participant> {
    if let Some(participant) = idle_collaboration_participant(participants, &request.to) {
        return Some(participant.clone());
    }
    if !muxa::collaboration::is_pending_session(&request.to.agent_session_id) {
        return None;
    }
    participants
        .iter()
        .find(|participant| {
            participant.pane == request.to.pane
                && participant.socket == request.to.socket
                && participant.state == muxa::AgentState::Idle
        })
        .cloned()
        .or_else(|| muxa::collaboration::pending_recipient_ready(&request.to, agents, panes))
}

fn idle_collaboration_participant<'a>(
    participants: &'a [muxa::collaboration::Participant],
    target: &muxa::collaboration::Participant,
) -> Option<&'a muxa::collaboration::Participant> {
    participants.iter().find(|participant| {
        participant.pane == target.pane
            && participant.socket == target.socket
            && participant.agent_session_id == target.agent_session_id
            && participant.state == muxa::AgentState::Idle
            && !participant
                .agent_session_id
                .starts_with(muxa::state::SYNTHETIC_SESSION_PREFIX)
    })
}

fn collaboration_wake_key(
    participant: &muxa::collaboration::Participant,
) -> (String, Option<String>, String) {
    (
        participant.pane.clone(),
        participant.socket.clone(),
        participant.agent_session_id.clone(),
    )
}

fn collaboration_wake_key_for_agent(
    agent: &muxa::Agent,
) -> Option<(String, Option<String>, String)> {
    Some((
        agent.pane.clone()?,
        agent.tmux_socket.clone(),
        agent.session_id.clone(),
    ))
}

async fn send_collaboration_wake(
    participant: &muxa::collaboration::Participant,
    prompt: &str,
    backends: &[muxa::SharedBackend],
) -> (bool, bool) {
    let sent = send_collaboration_text(participant, prompt, backends).await;
    let submitted = if sent {
        tokio::time::sleep(muxa::backend::PROMPT_SUBMIT_GRACE).await;
        send_collaboration_text(participant, "\r", backends).await
    } else {
        false
    };
    (sent, submitted)
}

async fn send_collaboration_text(
    participant: &muxa::collaboration::Participant,
    text: &str,
    backends: &[muxa::SharedBackend],
) -> bool {
    let Some(kind) = muxa::backend::pane_id_host_kind(&participant.pane) else {
        return false;
    };
    let Some(backend) = backends
        .iter()
        .find(|backend| backend.kind() == kind && backend.caps().send_text)
        .cloned()
    else {
        return false;
    };
    let pane = participant.pane.clone();
    let socket = participant.socket.clone();
    let text = text.to_string();
    tokio::task::spawn_blocking(move || backend.send_text_on(socket.as_deref(), &pane, &text))
        .await
        .unwrap_or(false)
}

/// Spawn the GC task: evicts long-stopped agents on a periodic timer.
///
/// Listens to the shutdown broadcast so a clean SIGTERM tears it down
/// rather than relying on the runtime falling out from under it.
fn spawn_gc_task(
    store: &muxa::SharedStore,
    shutdown_tx: &broadcast::Sender<()>,
) -> tokio::task::JoinHandle<()> {
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
    })
}

fn install_shutdown_signal_handler(restart: Arc<RestartController>) {
    tokio::spawn(async move {
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        tokio::select! {
            _ = term.recv() => tracing::info!("SIGTERM received"),
            _ = int.recv()  => tracing::info!("SIGINT received"),
        }
        // Monotonic stop state: once a signal wins, an already-open IPC
        // handler cannot re-arm a restart during the drain.
        restart.stop();
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
) -> (
    std::sync::Arc<PromptHistory>,
    Option<tokio::task::JoinHandle<()>>,
) {
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
        return (
            PromptHistory::in_memory_only(HistoryOptions {
                path: None,
                ..opts_template
            }),
            None,
        );
    }

    let Some(path) = opts_template.path.clone() else {
        tracing::warn!("history enabled but no path resolvable; falling back to in-memory only");
        return (
            PromptHistory::in_memory_only(HistoryOptions {
                path: None,
                ..opts_template
            }),
            None,
        );
    };

    match PromptHistory::spawn(opts_template.clone(), shutdown_tx.subscribe()).await {
        Ok((history, writer_handle)) => {
            tracing::info!(
                path = %path.display(),
                max_per_pane = cfg.history.max_per_pane,
                max_age_days = cfg.history.max_age_days,
                "prompt history enabled",
            );
            (history, Some(writer_handle))
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "could not open history file; falling back to in-memory only",
            );
            (
                PromptHistory::in_memory_only(HistoryOptions {
                    path: None,
                    ..opts_template
                }),
                None,
            )
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
) -> (
    Option<std::sync::Arc<ActivityLog>>,
    Option<tokio::task::JoinHandle<()>>,
) {
    if !cfg.activity.enabled {
        tracing::info!("activity ledger disabled by config");
        return (None, None);
    }
    let Some(path) = cfg
        .activity
        .path
        .clone()
        .or_else(paths::default_activity_file)
    else {
        tracing::warn!("activity ledger enabled but no path resolvable");
        return (None, None);
    };

    let opts = ActivityOptions::new(path.clone());
    match ActivityLog::spawn(opts, shutdown_tx.subscribe()).await {
        Ok((activity_log, writer_handle)) => {
            tracing::info!(
                path = %path.display(),
                max_age_days = cfg.activity.max_age_days,
                "activity ledger enabled",
            );
            (Some(activity_log), Some(writer_handle))
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "could not open activity ledger; duration stats will be incomplete",
            );
            (None, None)
        }
    }
}

/// Refresh the pane→session cache from the UNION of every observed backend's
/// pane list. Pane-id namespaces are disjoint across hosts (`%N` vs `herdr:…`
/// vs `zellij:…`), so concatenating the per-backend lists into one map can't
/// collide. Backends are enumerated concurrently off the runtime.
async fn refresh_pane_session_cache(cache: &PaneSessionCache, backends: &[muxa::SharedBackend]) {
    let handles: Vec<_> = backends
        .iter()
        .map(|backend| {
            let backend = backend.clone();
            tokio::task::spawn_blocking(move || backend.list_panes())
        })
        .collect();
    let mut entries = Vec::new();
    for handle in handles {
        let panes = handle.await.unwrap_or_default();
        entries.extend(panes.into_iter().map(|pane| (pane.pane_id, pane.session)));
    }
    cache.replace(entries);
}

fn spawn_pane_session_cache_task(
    cache: PaneSessionCache,
    backends: Vec<muxa::SharedBackend>,
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
                    refresh_pane_session_cache(&cache, &backends).await;
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
) -> Option<tokio::task::JoinHandle<()>> {
    let activity_log = activity_log?;

    let mut states = seed_activity_state_map(store.snapshot().await);
    let mut rx = store.subscribe();
    let store = store.clone();
    let mut shutdown_rx = shutdown_tx.subscribe();
    let handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                msg = rx.recv() => {
                    match msg {
                        Ok(transition) => {
                            record_activity_transition(
                                &activity_log,
                                &pane_session_cache,
                                &mut states,
                                transition,
                            );
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
                    while let Ok(transition) = rx.try_recv() {
                        record_activity_transition(
                            &activity_log,
                            &pane_session_cache,
                            &mut states,
                            transition,
                        );
                    }
                    tracing::debug!("activity transition task shutting down");
                    break;
                }
            }
        }
    });
    Some(handle)
}

fn record_activity_transition(
    activity_log: &ActivityLog,
    pane_session_cache: &PaneSessionCache,
    states: &mut HashMap<String, (muxa::AgentState, time::OffsetDateTime)>,
    transition: muxa::state::Transition,
) {
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
) -> Option<tokio::task::JoinHandle<()>> {
    if !cfg.history.enabled {
        return None;
    }
    let history = store.history().clone();
    let interval_secs = cfg.history.compact_interval_secs;
    let mut shutdown_rx = shutdown_tx.subscribe();
    let handle = tokio::spawn(async move {
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
    Some(handle)
}

fn spawn_activity_compaction_task(
    cfg: &Config,
    activity_log: Option<std::sync::Arc<ActivityLog>>,
    shutdown_tx: &broadcast::Sender<()>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !cfg.activity.enabled {
        return None;
    }
    let activity_log = activity_log?;
    let interval_secs = cfg.activity.compact_interval_secs;
    let max_age = time::Duration::days(i64::from(cfg.activity.max_age_days));
    let mut shutdown_rx = shutdown_tx.subscribe();
    let handle = tokio::spawn(async move {
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
    Some(handle)
}

/// Spawn the periodic reconciler: convergent control loop that uses tmux
/// as ground truth. Reaps records for closed panes, demotes orphaned
/// synthetic placeholders, and collapses duplicate rows so `muxa watch`
/// and `muxa status` stay in sync with the user's actual tmux state.
fn spawn_reconciler_task(
    cfg: &Config,
    store: &muxa::SharedStore,
    shutdown_tx: &broadcast::Sender<()>,
    backends: Vec<muxa::SharedBackend>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !cfg.reconciler.enabled {
        tracing::info!("reconciler disabled by config");
        return None;
    }
    // One reconciler over the whole observed set: each tick observes every
    // backend concurrently and reconciles each observation under its own host
    // kind, so a herdr timeout can't reap tmux rows (completeness is gated
    // per host). The ghost age-out sweep receives all these kinds, so a row on
    // a host NOT in the set ages out while rows on observed hosts stay governed
    // by their own reconcile pass. `LivenessSource` reaches the reconciler via
    // the blanket impl on `PaneBackend`.
    // Codex has no rate-limit hook; the reconciler learns codex usage caps
    // by polling the on-disk session rollouts. Resolve the tree once here so
    // the per-tick poll just reads files.
    let codex_sessions_root = if cfg.reconciler.codex_rollout_enabled {
        muxa::adapters::codex_rollout::default_sessions_root()
    } else {
        None
    };
    let runner = Reconciler::with_sources(
        store.clone(),
        backends,
        std::time::Duration::from_secs(cfg.reconciler.interval_secs),
    )
    .with_metrics(store.metrics())
    .with_stuck_working_timeout(std::time::Duration::from_secs(
        cfg.reconciler.stuck_working_timeout_secs,
    ))
    .with_stuck_waiting_timeout(std::time::Duration::from_secs(
        cfg.reconciler.stuck_waiting_timeout_secs,
    ))
    .with_paneless_stale_timeout(std::time::Duration::from_secs(
        cfg.reconciler.paneless_stale_timeout_secs,
    ))
    .with_codex_sessions_root(codex_sessions_root.clone());
    let shutdown_rx = shutdown_tx.subscribe();
    let handle = tokio::spawn(runner.run(shutdown_rx));
    tracing::info!(
        interval_secs = cfg.reconciler.interval_secs,
        stuck_working_timeout_secs = cfg.reconciler.stuck_working_timeout_secs,
        stuck_waiting_timeout_secs = cfg.reconciler.stuck_waiting_timeout_secs,
        paneless_stale_timeout_secs = cfg.reconciler.paneless_stale_timeout_secs,
        codex_rollout_polling = codex_sessions_root.is_some(),
        "reconciler enabled",
    );
    Some(handle)
}

/// Track cumulative session foreground time for `muxa watch --view session`.
/// The sampling source follows the active pane backend: tmux (and the zellij
/// fallback) shells out to `list-clients`; herdr queries the focused
/// workspace over its socket. All downstream accounting is shared.
fn spawn_session_activity_task(
    cfg: &Config,
    shutdown_tx: &broadcast::Sender<()>,
    activity_log: Option<std::sync::Arc<ActivityLog>>,
    backends: &[muxa::SharedBackend],
) -> Option<tokio::task::JoinHandle<()>> {
    if !cfg.session_activity.enabled {
        tracing::info!("session activity tracking disabled by config");
        return None;
    }
    let Some(path) = cfg
        .session_activity
        .path
        .clone()
        .or_else(paths::default_session_activity_file)
    else {
        tracing::warn!("session activity tracking enabled but no path resolvable");
        return None;
    };
    // One sampling source per host in the set that HAS a foreground signal:
    // tmux (`list-clients`) and herdr (focused workspace over its socket).
    // zellij has no client-attach signal, so it contributes no source. A single
    // tracker polls them all into one ledger — two independent trackers writing
    // `session-activity.json` would clobber each other (each `save()` rewrites
    // the whole file); merging is safe because the session-id keyspaces are
    // disjoint across hosts. Fall back to the default tmux sampler if the set
    // somehow yields no source (e.g. zellij-only), preserving prior behavior.
    let mut sources = Vec::new();
    for backend in backends {
        match backend.kind() {
            muxa::HostKind::Tmux => sources.push(muxa::SessionActivitySource::Tmux),
            muxa::HostKind::Herdr => sources.push(muxa::SessionActivitySource::Herdr {
                socket_path: muxa::backend::herdr::default_socket_path(),
            }),
            muxa::HostKind::Cmux | muxa::HostKind::Rmux | muxa::HostKind::Zellij => {}
        }
    }
    let source_kinds: Vec<muxa::HostKind> = backends
        .iter()
        .map(|b| b.kind())
        .filter(|kind| matches!(kind, muxa::HostKind::Tmux | muxa::HostKind::Herdr))
        .collect();
    let tracker = muxa::SessionActivityTracker::new(
        path.clone(),
        std::time::Duration::from_secs(cfg.session_activity.interval_secs),
    )
    .with_activity_log(activity_log)
    .with_sources(sources);
    let shutdown_rx = shutdown_tx.subscribe();
    let handle = tokio::spawn(tracker.run(shutdown_rx));
    tracing::info!(
        path = %path.display(),
        interval_secs = cfg.session_activity.interval_secs,
        hosts = ?source_kinds,
        "session activity tracking enabled",
    );
    Some(handle)
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
    backends: &[muxa::SharedBackend],
) {
    if history.is_empty().await {
        return;
    }
    // Wrap each (potentially blocking) backend call so the runtime doesn't
    // stall while tmux shells out / the herdr socket round-trips; enumerate the
    // whole set concurrently and union the results. Cloning the `Arc`s is the
    // cheapest way to give each `spawn_blocking` closure the `'static` handle it
    // needs. Pane namespaces are disjoint across hosts, so the union can't
    // conflate panes from different backends.
    let handles: Vec<_> = backends
        .iter()
        .map(|backend| {
            let backend = backend.clone();
            tokio::task::spawn_blocking(move || backend.list_panes())
        })
        .collect();
    let mut panes = Vec::new();
    for handle in handles {
        if let Ok(list) = handle.await {
            panes.extend(list);
        }
    }
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
            tmux_socket: pane.socket.clone(),
            tmux_session: Some(pane.session.clone()),
            cwd: entry.cwd,
            pid: None,
            workload: muxa::WorkloadSummary::default(),
            subagents: Vec::new(),
            state: muxa::AgentState::Idle,
            last_prompt: Some(entry.prompt),
            last_response: None,
            recap: None,
            ai_title: None,
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
        "public_read" | "public-read" | "read_only" | "read-only" => {
            Ok(DashboardAuthMode::PublicRead)
        }
        "none" | "off" | "public" => Ok(DashboardAuthMode::None),
        _ => anyhow::bail!(
            "invalid dashboard auth mode {s:?}; expected `token`, `public_read`, or `none`"
        ),
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
fn spawn_startup_discovery(
    cfg: &Config,
    socket: PathBuf,
    backends: Vec<muxa::SharedBackend>,
    shutdown_tx: &broadcast::Sender<()>,
) -> bool {
    if !cfg.discovery.enabled {
        tracing::debug!("startup discovery disabled by config");
        return false;
    }
    let mut shutdown_rx = shutdown_tx.subscribe();
    tokio::spawn(async move {
        // Small grace so the listener's `accept` loop is actually running
        // by the time we connect. The 250 ms figure matches the design
        // doc — anything less is racy on slower hosts, anything more
        // delays the visible backfill needlessly.
        //
        // Race the grace sleep AND the discovery pass against shutdown: a
        // SIGTERM arriving mid-discovery must not keep the process alive for
        // the full scan (which used to add a ~13 s tail to every restart and
        // pushed operators toward SIGKILL, wedging launchd's relaunch).
        tokio::select! {
            _ = shutdown_rx.recv() => {
                tracing::debug!("startup discovery cancelled before start (shutdown)");
                return;
            }
            () = tokio::time::sleep(std::time::Duration::from_millis(250)) => {}
        }
        let client = Client::new(socket);
        tokio::select! {
            _ = shutdown_rx.recv() => {
                tracing::debug!("startup discovery cancelled mid-pass (shutdown)");
            }
            report = run_discovery_all(&client, &backends) => {
                tracing::info!(
                    claude_code = report.claude_code,
                    codex = report.codex,
                    gemini_cli = report.gemini_cli,
                    skipped_known = report.skipped_known,
                    failed = report.failed,
                    "startup discovery complete",
                );
            }
        }
    });
    true
}

/// Run one discovery pass over every observed backend and sum the reports.
/// Pane namespaces are disjoint per host, so each backend's scan contributes
/// independently.
///
/// The passes fan out **concurrently** — each does its own blocking pane scan
/// (`block_in_place` inside `run_discovery`) plus async IPC, so we drive each
/// on its own task and join. A slow or unreachable host can't serialize behind
/// (or block) the others, and a failed pass (discovery error *or* join error)
/// contributes nothing while the rest still land — best-effort, matching the
/// rest of the daemon's surface, and mirroring `watch::compute_refresh`'s
/// concurrent inventory fan-out. `run_discovery` is async (does IPC), so we
/// use `tokio::spawn` rather than `spawn_blocking`; both the daemon and CLI run
/// on the multi-threaded runtime `block_in_place` requires.
async fn run_discovery_all(
    client: &Client,
    backends: &[muxa::SharedBackend],
) -> discovery::DiscoveryReport {
    let handles: Vec<_> = backends
        .iter()
        .map(|backend| {
            let client = client.clone();
            let backend = backend.clone();
            tokio::spawn(async move {
                let kind = backend.kind();
                (
                    kind,
                    discovery::run_discovery(&client, backend.as_ref()).await,
                )
            })
        })
        .collect();

    let mut total = discovery::DiscoveryReport::default();
    for handle in handles {
        match handle.await {
            Ok((_, Ok(report))) => {
                total.claude_code += report.claude_code;
                total.codex += report.codex;
                total.gemini_cli += report.gemini_cli;
                total.skipped_known += report.skipped_known;
                total.failed += report.failed;
            }
            Ok((kind, Err(e))) => {
                tracing::warn!(error = %e, host = %kind, "discovery pass failed");
            }
            Err(e) => {
                tracing::debug!(error = %e, "discovery task join failed");
            }
        }
    }
    total
}

/// Spawn the periodic discovery rescan. Startup discovery covers t=0; this
/// keeps newly-created panes appearing in `muxa status` within
/// `[discovery] interval_secs` instead of only after the agent's first hook.
/// The pass uses one `tmux list-panes` and, for wrapper foreground commands,
/// a single bounded process-table snapshot.
/// No-op when discovery is disabled or `interval_secs == 0`.
fn spawn_periodic_discovery(
    cfg: &Config,
    socket: PathBuf,
    backends: Vec<muxa::SharedBackend>,
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
                    let report = run_discovery_all(&client, &backends).await;
                    tracing::debug!(
                        claude_code = report.claude_code,
                        codex = report.codex,
                        gemini_cli = report.gemini_cli,
                        skipped_known = report.skipped_known,
                        "periodic discovery pass",
                    );
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
    use muxa::backend::{BackendCaps, HostKind, PaneBackend};
    use muxa::collaboration::{
        CollaborationPaneEvidence, CollaborationProvenance, NewRequest, RequestKind, RequestStatus,
        WorkMode,
    };
    use muxa::config::DiscoveryConfig;
    use muxa::event::{AgentEvent, AgentId, AgentKind};
    use muxa::tmux::PaneInfo;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use time::OffsetDateTime;

    #[test]
    fn dashboard_auth_parser_accepts_public_read_aliases() {
        assert_eq!(
            parse_dashboard_auth("public_read").unwrap(),
            DashboardAuthMode::PublicRead
        );
        assert_eq!(
            parse_dashboard_auth("read-only").unwrap(),
            DashboardAuthMode::PublicRead
        );
    }

    struct CollaborationWakeBackend {
        panes: Vec<PaneInfo>,
        sends: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl PaneBackend for CollaborationWakeBackend {
        fn kind(&self) -> HostKind {
            HostKind::Tmux
        }

        fn list_panes(&self) -> Vec<PaneInfo> {
            self.panes.clone()
        }

        fn resolve_pane(&self, pane_id: &str) -> Option<PaneInfo> {
            self.panes
                .iter()
                .find(|pane| pane.pane_id == pane_id)
                .cloned()
        }

        fn capture_pane(&self, _pane_id: &str) -> Option<String> {
            None
        }

        fn pane_pid_map(&self) -> HashMap<u32, String> {
            HashMap::new()
        }

        fn current_pane(&self) -> Option<String> {
            None
        }

        fn focus_pane(&self, _pane_id: &str) -> bool {
            false
        }

        fn send_text(&self, pane_id: &str, text: &str) -> bool {
            self.sends
                .lock()
                .unwrap()
                .push((pane_id.into(), text.into()));
            true
        }

        fn caps(&self) -> BackendCaps {
            BackendCaps::default()
        }
    }

    fn collaboration_pane(pane_id: &str, pane_index: &str) -> PaneInfo {
        PaneInfo {
            session_group: None,
            agent_role: None,
            agent_alias: None,
            pane_id: pane_id.into(),
            session_id: "$1".into(),
            session: "collaboration".into(),
            window_id: "@1".into(),
            window_name: "agents".into(),
            window_index: "0".into(),
            pane_index: pane_index.into(),
            tty: String::new(),
            current_command: "agent".into(),
            title: String::new(),
            current_path: "/repo".into(),
            pane_pid: 0,
            socket: Some("default".into()),
        }
    }

    async fn add_agent(store: &muxa::SharedStore, pane: &str, session_id: &str, kind: AgentKind) {
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind,
                    session_id: session_id.into(),
                    surface: None,
                    pane: Some(pane.into()),
                    tmux_socket: Some("default".into()),
                    cwd: Some("/repo".into()),
                },
                at: OffsetDateTime::now_utc(),
            })
            .await;
    }

    #[tokio::test]
    async fn collaboration_waker_reacts_to_mailbox_revision() {
        let store = muxa::Store::shared();
        add_agent(&store, "%1", "sender", AgentKind::Codex).await;
        add_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
        let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
        let participants = muxa::collaboration::participants_from(&store.snapshot().await, &panes);
        let sender = participants
            .iter()
            .find(|participant| participant.pane == "%1")
            .unwrap()
            .clone();
        let recipient = participants
            .iter()
            .find(|participant| participant.pane == "%2")
            .unwrap()
            .clone();
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let sends = Arc::new(Mutex::new(Vec::new()));
        let backend: muxa::SharedBackend = Arc::new(CollaborationWakeBackend {
            panes,
            sends: sends.clone(),
        });
        let mut cfg = Config::default();
        cfg.collaboration.enabled = true;
        cfg.collaboration.wake = CollaborationWake::IdleOnly;
        let (shutdown_tx, _) = broadcast::channel(1);
        let waker = spawn_collaboration_waker_task(
            &cfg,
            mailbox.clone(),
            store,
            vec![backend],
            &shutdown_tx,
        )
        .expect("enabled collaboration should spawn a waker");

        let request = mailbox
            .create(
                sender,
                recipient,
                NewRequest {
                    kind: RequestKind::Question,
                    body: "wake from revision".into(),
                    expects_reply: true,
                    work_mode: WorkMode::ReadOnly,
                    paths: Vec::new(),
                    air_artifacts: Vec::new(),
                },
            )
            .await
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_millis(200), async {
            loop {
                let prompt_and_submit_sent = sends.lock().unwrap().len() >= 2;
                if prompt_and_submit_sent && mailbox.pending_unnotified().await.is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("mailbox revision should trigger prompt and submit without a timer tick");
        {
            let delivered = sends.lock().unwrap();
            assert_eq!(delivered[0].0, "%2");
            assert!(delivered[0].1.contains(&request.id));
        }

        shutdown_tx.send(()).unwrap();
        waker.await.unwrap();
    }

    /// The spawn deadlock, end to end: a pane muxa launched carries only a
    /// synthetic row — no session has registered, so it is not a participant —
    /// yet the request queued against it is delivered as soon as the pane
    /// reads idle. Without this the sender would wait for a registration that
    /// codex cannot produce until something types at it.
    #[tokio::test]
    async fn collaboration_waker_delivers_to_a_launched_pane_before_it_registers() {
        let store = muxa::Store::shared();
        add_agent(&store, "%1", "sender", AgentKind::ClaudeCode).await;
        add_agent(
            &store,
            "%2",
            &format!("{}default:%2", muxa::state::SYNTHETIC_SESSION_PREFIX),
            AgentKind::Codex,
        )
        .await;
        let mut launched = collaboration_pane("%2", "1");
        launched.agent_role = Some("peer".into());
        let panes = vec![collaboration_pane("%1", "0"), launched];
        let agents = store.snapshot().await;
        let participants = muxa::collaboration::participants_from(&agents, &panes);
        assert!(
            participants
                .iter()
                .all(|participant| participant.pane != "%2"),
            "a synthetic row is not a participant — that is the whole problem",
        );
        let sender = participants
            .iter()
            .find(|participant| participant.pane == "%1")
            .unwrap()
            .clone();
        let pending = muxa::collaboration::resolve_pending_pane_target(
            &sender,
            "pane:%2",
            &participants,
            &agents,
            &panes,
            muxa::config::CollaborationScope::Window,
        )
        .expect("a launched agent pane is addressable by pane id");

        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let sends = Arc::new(Mutex::new(Vec::new()));
        let backend: muxa::SharedBackend = Arc::new(CollaborationWakeBackend {
            panes,
            sends: sends.clone(),
        });
        let mut cfg = Config::default();
        cfg.collaboration.enabled = true;
        cfg.collaboration.wake = CollaborationWake::IdleOnly;
        let (shutdown_tx, _) = broadcast::channel(1);
        let waker = spawn_collaboration_waker_task(
            &cfg,
            mailbox.clone(),
            store.clone(),
            vec![backend],
            &shutdown_tx,
        )
        .expect("enabled collaboration should spawn a waker");

        let request = mailbox
            .create(
                sender,
                pending,
                NewRequest {
                    kind: RequestKind::Review,
                    body: "review the pending diff".into(),
                    expects_reply: true,
                    work_mode: WorkMode::ReadOnly,
                    paths: Vec::new(),
                    air_artifacts: Vec::new(),
                },
            )
            .await
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_millis(500), async {
            loop {
                if sends.lock().unwrap().len() >= 2 && mailbox.pending_unnotified().await.is_empty()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("a pending pane recipient should be woken like any other");
        {
            let delivered = sends.lock().unwrap();
            assert_eq!(delivered[0].0, "%2");
            assert!(delivered[0].1.contains(&request.id));
        }

        // The session that finally registers adopts the request.
        add_agent(&store, "%2", "codex-session", AgentKind::Codex).await;
        let registered = muxa::collaboration::participants_from(
            &store.snapshot().await,
            &[collaboration_pane("%1", "0"), collaboration_pane("%2", "1")],
        )
        .into_iter()
        .find(|participant| participant.pane == "%2")
        .expect("the hook row registers the pane");
        let inbox = mailbox.claim_for(&registered).await.unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].id, request.id);

        shutdown_tx.send(()).unwrap();
        waker.await.unwrap();
    }

    #[tokio::test]
    async fn full_wake_claims_and_injects_the_structured_request_body() {
        let store = muxa::Store::shared();
        add_agent(&store, "%1", "sender", AgentKind::Codex).await;
        add_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
        let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
        let participants = muxa::collaboration::participants_from(&store.snapshot().await, &panes);
        let sender = participants
            .iter()
            .find(|participant| participant.pane == "%1")
            .unwrap()
            .clone();
        let recipient = participants
            .iter()
            .find(|participant| participant.pane == "%2")
            .unwrap()
            .clone();
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let request = mailbox
            .create(
                sender.clone(),
                recipient.clone(),
                NewRequest {
                    kind: RequestKind::Task,
                    body: "change only the authorized file".into(),
                    expects_reply: true,
                    work_mode: WorkMode::Execute,
                    paths: vec!["src/auth.rs".into()],
                    air_artifacts: Vec::new(),
                },
            )
            .await
            .unwrap();
        let sends = Arc::new(Mutex::new(Vec::new()));
        let backend: muxa::SharedBackend = Arc::new(CollaborationWakeBackend {
            panes,
            sends: sends.clone(),
        });

        wake_idle_collaboration_peers_with_full_payload(&mailbox, &store, &[backend]).await;

        {
            let delivered = sends.lock().unwrap();
            assert_eq!(delivered.len(), 2);
            assert_eq!(delivered[0].0, "%2");
            assert!(delivered[0].1.contains(&request.id));
            assert!(delivered[0].1.contains("kind: task"));
            assert!(delivered[0].1.contains("work_mode: execute"));
            assert!(delivered[0].1.contains("paths: [\"src/auth.rs\"]"));
            assert!(delivered[0].1.contains("change only the authorized file"));
            assert!(delivered[0].1.contains("already claimed"));
            assert!(!delivered[0].1.contains("Read it with muxa_inbox"));
            assert_eq!(delivered[1], ("%2".into(), "\r".into()));
        }

        let stored = mailbox.get_for(&recipient, &request.id).await.unwrap();
        assert_eq!(stored.status, RequestStatus::Claimed);
        assert!(stored.claimed_at.is_some());
        assert!(stored.notified_at.is_some());
        assert_eq!(stored.wake_delivery, None);
        assert!(mailbox.pending_unnotified().await.is_empty());

        sends.lock().unwrap().clear();
        let unsafe_request = mailbox
            .create(
                sender,
                recipient.clone(),
                NewRequest {
                    kind: RequestKind::Task,
                    body: "unsafe\u{1b}[201~\rsubmit".into(),
                    expects_reply: true,
                    work_mode: WorkMode::Execute,
                    paths: Vec::new(),
                    air_artifacts: Vec::new(),
                },
            )
            .await
            .unwrap();
        let backend: muxa::SharedBackend = Arc::new(CollaborationWakeBackend {
            panes: vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")],
            sends: sends.clone(),
        });
        wake_idle_collaboration_peers_with_full_payload(&mailbox, &store, &[backend]).await;
        {
            let delivered = sends.lock().unwrap();
            assert_eq!(delivered.len(), 2);
            assert!(delivered[0].1.contains("muxa_inbox"));
            assert!(!delivered[0].1.contains(&unsafe_request.body));
        }
        let stored = mailbox
            .get_for(&recipient, &unsafe_request.id)
            .await
            .unwrap();
        assert_eq!(stored.status, RequestStatus::Queued);
        assert!(stored.notified_at.is_some());
    }

    #[tokio::test]
    async fn operator_full_wake_injects_console_requests() {
        let store = muxa::Store::shared();
        add_agent(&store, "%1", "sender", AgentKind::Codex).await;
        add_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
        let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
        let participants = muxa::collaboration::participants_from(&store.snapshot().await, &panes);
        let sender = participants
            .iter()
            .find(|participant| participant.pane == "%1")
            .unwrap()
            .clone();
        let recipient = participants
            .iter()
            .find(|participant| participant.pane == "%2")
            .unwrap()
            .clone();
        let console = muxa::collaboration::Participant::console(sender.room.clone());
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let operator_request = mailbox
            .create_with_provenance(
                console,
                recipient.clone(),
                NewRequest {
                    kind: RequestKind::Task,
                    body: "operator request body".into(),
                    expects_reply: true,
                    work_mode: WorkMode::Execute,
                    paths: Vec::new(),
                    air_artifacts: Vec::new(),
                },
                Some(CollaborationProvenance {
                    client_kind: CollaborationClientKind::Watch,
                    caller_pid: Some(4242),
                    caller_uid: Some(1000),
                    caller_gid: Some(1000),
                    executable: Some("muxa".into()),
                    observed_pane: Some("%1".into()),
                    pane_evidence: Some(CollaborationPaneEvidence::ProcessAncestry),
                    origin_match: CollaborationOriginMatch::Matched,
                }),
            )
            .await
            .unwrap();
        let sends = Arc::new(Mutex::new(Vec::new()));
        let backend: muxa::SharedBackend = Arc::new(CollaborationWakeBackend {
            panes: panes.clone(),
            sends: sends.clone(),
        });
        let mut wake_inflight = HashSet::new();

        wake_idle_collaboration_peers_with_inflight(
            &mailbox,
            &store,
            std::slice::from_ref(&backend),
            CollaborationWakePayload::OperatorFull,
            &mut wake_inflight,
        )
        .await;
        {
            let delivered = sends.lock().unwrap();
            assert_eq!(delivered.len(), 2);
            assert!(delivered[0].1.contains("operator request body"));
            assert!(delivered[0]
                .1
                .contains("via muxa watch representing console"));
            assert!(!delivered[0].1.contains("Read it with muxa_inbox"));
        }
        let stored = mailbox
            .get_for(&recipient, &operator_request.id)
            .await
            .unwrap();
        assert_eq!(stored.status, RequestStatus::Claimed);
    }

    #[tokio::test]
    async fn operator_full_wake_keeps_agent_requests_in_the_mailbox() {
        let store = muxa::Store::shared();
        add_agent(&store, "%1", "sender", AgentKind::Codex).await;
        add_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
        let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
        let participants = muxa::collaboration::participants_from(&store.snapshot().await, &panes);
        let sender = participants
            .iter()
            .find(|participant| participant.pane == "%1")
            .unwrap()
            .clone();
        let recipient = participants
            .iter()
            .find(|participant| participant.pane == "%2")
            .unwrap()
            .clone();
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let agent_request = mailbox
            .create(
                sender,
                recipient.clone(),
                NewRequest {
                    kind: RequestKind::Task,
                    body: "agent delegated body".into(),
                    expects_reply: true,
                    work_mode: WorkMode::Execute,
                    paths: Vec::new(),
                    air_artifacts: Vec::new(),
                },
            )
            .await
            .unwrap();
        let sends = Arc::new(Mutex::new(Vec::new()));
        let backend: muxa::SharedBackend = Arc::new(CollaborationWakeBackend {
            panes,
            sends: sends.clone(),
        });
        let mut wake_inflight = HashSet::new();

        wake_idle_collaboration_peers_with_inflight(
            &mailbox,
            &store,
            &[backend],
            CollaborationWakePayload::OperatorFull,
            &mut wake_inflight,
        )
        .await;
        {
            let delivered = sends.lock().unwrap();
            assert_eq!(delivered.len(), 2);
            assert!(delivered[0].1.contains("muxa_inbox"));
            assert!(!delivered[0].1.contains("agent delegated body"));
        }
        let stored = mailbox
            .get_for(&recipient, &agent_request.id)
            .await
            .unwrap();
        assert_eq!(stored.status, RequestStatus::Queued);
    }

    #[tokio::test]
    async fn full_wake_submits_only_one_request_per_idle_generation() {
        let store = muxa::Store::shared();
        add_agent(&store, "%1", "sender", AgentKind::Codex).await;
        add_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
        let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
        let participants = muxa::collaboration::participants_from(&store.snapshot().await, &panes);
        let sender = participants
            .iter()
            .find(|participant| participant.pane == "%1")
            .unwrap()
            .clone();
        let recipient = participants
            .iter()
            .find(|participant| participant.pane == "%2")
            .unwrap()
            .clone();
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        for body in ["first body", "second body"] {
            mailbox
                .create(
                    sender.clone(),
                    recipient.clone(),
                    NewRequest {
                        kind: RequestKind::Task,
                        body: body.into(),
                        expects_reply: true,
                        work_mode: WorkMode::Execute,
                        paths: Vec::new(),
                        air_artifacts: Vec::new(),
                    },
                )
                .await
                .unwrap();
        }
        let sends = Arc::new(Mutex::new(Vec::new()));
        let backend: muxa::SharedBackend = Arc::new(CollaborationWakeBackend {
            panes,
            sends: sends.clone(),
        });
        let mut wake_inflight = HashSet::new();

        wake_idle_collaboration_peers_with_inflight(
            &mailbox,
            &store,
            std::slice::from_ref(&backend),
            CollaborationWakePayload::Full,
            &mut wake_inflight,
        )
        .await;
        assert_eq!(sends.lock().unwrap().len(), 2);
        assert_eq!(mailbox.pending_unnotified().await.len(), 1);

        // Mailbox revisions emitted by the first delivery can rescan while
        // the state hook still says Idle. The generation gate must suppress
        // a second prompt until a later Idle transition clears it.
        wake_idle_collaboration_peers_with_inflight(
            &mailbox,
            &store,
            std::slice::from_ref(&backend),
            CollaborationWakePayload::Full,
            &mut wake_inflight,
        )
        .await;
        assert_eq!(sends.lock().unwrap().len(), 2);
        assert_eq!(mailbox.pending_unnotified().await.len(), 1);

        wake_inflight.clear();
        wake_idle_collaboration_peers_with_inflight(
            &mailbox,
            &store,
            &[backend],
            CollaborationWakePayload::Full,
            &mut wake_inflight,
        )
        .await;
        assert_eq!(sends.lock().unwrap().len(), 4);
        assert!(mailbox.pending_unnotified().await.is_empty());
    }

    #[tokio::test]
    async fn full_wake_recovers_without_reinjecting_the_request_body() {
        let store = muxa::Store::shared();
        add_agent(&store, "%1", "sender", AgentKind::Codex).await;
        add_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
        let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
        let participants = muxa::collaboration::participants_from(&store.snapshot().await, &panes);
        let sender = participants
            .iter()
            .find(|participant| participant.pane == "%1")
            .unwrap()
            .clone();
        let recipient = participants
            .iter()
            .find(|participant| participant.pane == "%2")
            .unwrap()
            .clone();
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let request = mailbox
            .create(
                sender.clone(),
                recipient.clone(),
                NewRequest {
                    kind: RequestKind::Task,
                    body: "do not inject this twice".into(),
                    expects_reply: true,
                    work_mode: WorkMode::Execute,
                    paths: Vec::new(),
                    air_artifacts: Vec::new(),
                },
            )
            .await
            .unwrap();
        mailbox
            .prepare_direct_wake(&recipient, &request.id)
            .await
            .unwrap()
            .unwrap();
        let sends = Arc::new(Mutex::new(Vec::new()));
        let backend: muxa::SharedBackend = Arc::new(CollaborationWakeBackend {
            panes: panes.clone(),
            sends: sends.clone(),
        });

        wake_idle_collaboration_peers_with_full_payload(
            &mailbox,
            &store,
            std::slice::from_ref(&backend),
        )
        .await;
        {
            let delivered = sends.lock().unwrap();
            assert_eq!(delivered.len(), 2);
            assert!(delivered[0].1.contains("delivery was interrupted"));
            assert!(delivered[0].1.contains("muxa_inbox"));
            assert!(!delivered[0].1.contains("do not inject this twice"));
            assert_eq!(delivered[1], ("%2".into(), "\r".into()));
        }
        assert!(mailbox.pending_unnotified().await.is_empty());

        let second = mailbox
            .create(
                sender,
                recipient.clone(),
                NewRequest {
                    kind: RequestKind::Task,
                    body: "the prompt text is already buffered".into(),
                    expects_reply: true,
                    work_mode: WorkMode::Execute,
                    paths: Vec::new(),
                    air_artifacts: Vec::new(),
                },
            )
            .await
            .unwrap();
        mailbox
            .prepare_direct_wake(&recipient, &second.id)
            .await
            .unwrap()
            .unwrap();
        mailbox.mark_wake_prompt_written(&second.id).await.unwrap();
        sends.lock().unwrap().clear();

        wake_idle_collaboration_peers_with_full_payload(&mailbox, &store, &[backend]).await;
        assert_eq!(
            sends.lock().unwrap().as_slice(),
            &[("%2".into(), "\r".into())]
        );
        assert!(mailbox.pending_unnotified().await.is_empty());
    }

    #[tokio::test]
    async fn terminal_reply_wakes_idle_sender_without_injecting_body() {
        let store = muxa::Store::shared();
        add_agent(&store, "%1", "sender", AgentKind::Codex).await;
        add_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
        let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
        let agents = store.snapshot().await;
        let participants = muxa::collaboration::participants_from(&agents, &panes);
        let sender = participants
            .iter()
            .find(|participant| participant.pane == "%1")
            .unwrap()
            .clone();
        let recipient = participants
            .iter()
            .find(|participant| participant.pane == "%2")
            .unwrap()
            .clone();
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let request = mailbox
            .create_with_provenance(
                sender,
                recipient.clone(),
                NewRequest {
                    kind: RequestKind::Review,
                    body: "secret request body".into(),
                    expects_reply: true,
                    work_mode: WorkMode::ReadOnly,
                    paths: Vec::new(),
                    air_artifacts: Vec::new(),
                },
                Some(CollaborationProvenance {
                    client_kind: CollaborationClientKind::Watch,
                    caller_pid: Some(4242),
                    caller_uid: Some(1000),
                    caller_gid: Some(1000),
                    executable: Some("muxa".into()),
                    observed_pane: Some("%1".into()),
                    pane_evidence: Some(CollaborationPaneEvidence::ProcessAncestry),
                    origin_match: CollaborationOriginMatch::Matched,
                }),
            )
            .await
            .unwrap();

        let sends = Arc::new(Mutex::new(Vec::new()));
        let backend: muxa::SharedBackend = Arc::new(CollaborationWakeBackend {
            panes,
            sends: sends.clone(),
        });
        wake_idle_collaboration_peers(&mailbox, &store, std::slice::from_ref(&backend)).await;
        {
            let mut sends = sends.lock().unwrap();
            assert_eq!(sends.len(), 2);
            assert_eq!(sends[0].0, "%2");
            assert!(sends[0].1.contains("muxa_inbox"));
            assert!(sends[0].1.contains("muxa msg inbox --json"));
            assert!(sends[0].1.contains("kind/work_mode/paths"));
            assert!(sends[0].1.contains("via muxa watch representing codex@%1"));
            assert!(sends[0].1.contains("caller %1, pid 4242"));
            assert!(!sends[0].1.contains("secret request body"));
            assert_eq!(sends[1], ("%2".into(), "\r".into()));
            sends.clear();
        }

        mailbox.claim_for(&recipient).await.unwrap();
        mailbox
            .reply(
                &recipient,
                &request.id,
                RequestStatus::Completed,
                "secret reply body".into(),
                Vec::new(),
                Vec::new(),
            )
            .await
            .unwrap();

        wake_idle_collaboration_peers(&mailbox, &store, &[backend]).await;

        {
            let sends = sends.lock().unwrap();
            assert_eq!(sends.len(), 2);
            assert_eq!(sends[0].0, "%1");
            assert!(sends[0].1.contains("reply"));
            assert!(sends[0].1.contains(&request.id));
            assert!(sends[0].1.contains("muxa_wait_reply"));
            assert!(sends[0].1.contains("muxa msg wait"));
            assert!(!sends[0].1.contains("secret request body"));
            assert!(!sends[0].1.contains("secret reply body"));
            assert_eq!(sends[1], ("%1".into(), "\r".into()));
        }
        assert!(mailbox.pending_reply_unnotified().await.is_empty());
        assert_eq!(
            mailbox
                .unread_reply_count(
                    participants
                        .iter()
                        .find(|participant| participant.pane == "%1")
                        .unwrap(),
                )
                .await,
            1
        );
    }

    /// A console dispatches but never receives. The recipient still gets its
    /// wake; the reply has nowhere to be typed, and the wake loop must reach
    /// that conclusion by finding no participant rather than by trying to
    /// address a pane called "console".
    #[tokio::test]
    async fn a_console_sender_is_woken_for_nothing() {
        let store = muxa::Store::shared();
        add_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
        let panes = vec![collaboration_pane("%2", "1")];
        let agents = store.snapshot().await;
        let participants = muxa::collaboration::participants_from(&agents, &panes);
        let recipient = participants[0].clone();
        let console = muxa::collaboration::resolve_origin(
            &muxa::collaboration::CollaborationOrigin {
                pane: "%2".into(),
                socket: None,
                console: true,
            },
            &participants,
            &panes,
        )
        .unwrap();
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let request = mailbox
            .create(
                console.clone(),
                recipient.clone(),
                NewRequest {
                    kind: RequestKind::Task,
                    body: "dispatched by a human".into(),
                    expects_reply: true,
                    work_mode: WorkMode::ReadOnly,
                    paths: Vec::new(),
                    air_artifacts: Vec::new(),
                },
            )
            .await
            .unwrap();

        let sends = Arc::new(Mutex::new(Vec::new()));
        let backend: muxa::SharedBackend = Arc::new(CollaborationWakeBackend {
            panes,
            sends: sends.clone(),
        });
        wake_idle_collaboration_peers(&mailbox, &store, std::slice::from_ref(&backend)).await;
        {
            let mut sends = sends.lock().unwrap();
            assert_eq!(sends[0].0, "%2");
            assert!(sends[0].1.contains("from console"));
            sends.clear();
        }

        mailbox.claim_for(&recipient).await.unwrap();
        mailbox
            .reply(
                &recipient,
                &request.id,
                RequestStatus::Completed,
                "done".into(),
                Vec::new(),
                Vec::new(),
            )
            .await
            .unwrap();
        wake_idle_collaboration_peers(&mailbox, &store, &[backend]).await;

        assert!(
            sends.lock().unwrap().is_empty(),
            "a console has no pane to wake"
        );
        // Unread stays pending because nothing consumed it, and the reply is
        // readable from the recipient's own mailbox — which is where `muxa
        // watch` reads it, by pointing the cursor at that row.
        assert_eq!(
            mailbox
                .list_for(
                    &recipient,
                    muxa::collaboration::RequestMailbox::Incoming,
                    muxa::collaboration::MailboxScope::Caller,
                )
                .await
                .unwrap()[0]
                .reply
                .as_ref()
                .map(|reply| reply.body.as_str()),
            Some("done")
        );
    }

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
        let (shutdown_tx, _) = broadcast::channel::<()>(1);
        let spawned = spawn_startup_discovery(
            &cfg,
            PathBuf::from("/tmp/never-bound.sock"),
            muxa::active_backends(),
            &shutdown_tx,
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
        let (shutdown_tx, _) = broadcast::channel::<()>(1);
        let spawned = spawn_startup_discovery(
            &cfg,
            PathBuf::from("/tmp/never-bound.sock"),
            muxa::active_backends(),
            &shutdown_tx,
        );
        assert!(!spawned, "discovery must not spawn when disabled");
    }
}
