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
use muxa::automation::{
    subjects_from, AutomationAction, AutomationLedger, AutomationLedgerEntry, AutomationOutcome,
    AutomationRule, AutomationStore, AutomationSubject, Decision, PlannedFiring, Scheduler,
    SkipReason,
};
use muxa::automation_judge::{self, AutomationJudgment, JudgmentContext, JudgmentDecision};
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
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(unix)]
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::broadcast;

mod binary_watch;
mod fleet_manager;
mod herdr_bridge;
mod pipeline_reconciler;
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
/// Safety-net rescan for the automation engine. Arming is normally driven by
/// state transitions; this catches a row that was already capped when the
/// daemon started, and a cap that landed on a row already in `Error` (which
/// produces no transition at all).
const AUTOMATION_RECONCILE_SECONDS: u64 = 30;
/// Longest the automation task parks between rescans when nothing is due.
/// Bounds the wait so a rule added at runtime is picked up promptly even
/// with an empty firing queue.
const AUTOMATION_MAX_IDLE_SECONDS: u64 = 60;
/// How long a pane scan is reused for workspace/work rule filters. Only
/// paid for when at least one enabled rule actually reads that metadata.
const AUTOMATION_PANE_CACHE_SECONDS: u64 = 10;

#[derive(Debug, Parser)]
#[allow(clippy::struct_excessive_bools)] // independent CLI switches
#[command(name = "muxad", version, about = "muxa daemon")]
struct Args {
    /// Validate startup configuration and exit without side effects.
    #[arg(long, hide = true)]
    check_config: bool,
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

    /// Append logs to this file instead of writing them to stderr. The file
    /// is opened shared, so a successor can append while its predecessor
    /// still holds it — what `muxa daemon start` relies on on Windows, where
    /// the daemon has no stderr worth keeping.
    #[arg(long, value_name = "PATH")]
    log_file: Option<PathBuf>,
}

/// How long the runtime's teardown waits for blocking tasks still running
/// once the daemon's shutdown sequence is done: a submission stuck behind a
/// swap or a hung capture must not hold the process past its stop budget.
const RUNTIME_TEARDOWN: std::time::Duration = std::time::Duration::from_secs(2);

fn main() -> Result<()> {
    run_bounded(run(), RUNTIME_TEARDOWN)
}

/// Run the daemon on a multi-threaded runtime whose teardown is bounded.
/// `#[tokio::main]`'s runtime drop waits for every blocking task without
/// limit; this one gives them `teardown` and then lets the process go.
fn run_bounded(
    daemon: impl std::future::Future<Output = Result<()>>,
    teardown: std::time::Duration,
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(daemon);
    runtime.shutdown_timeout(teardown);
    result
}

#[allow(clippy::too_many_lines)] // daemon bootstrap wires long-lived tasks in startup order
async fn run() -> Result<()> {
    let args = Args::parse();
    if args.check_config {
        let path = args.config.clone().or_else(paths::default_config_file);
        let cfg = Config::load_or_default(path.as_deref())?;
        cfg.validate_for_daemon()?;
        resolve_dashboard_config(&cfg, &args)?;
        return Ok(());
    }

    // Initialize tracing BEFORE loading config so any `tracing::warn!`
    // emitted by `Config::load` (unknown `[watch] columns` keys, unknown
    // detail-template placeholders, …) actually reaches stderr.
    // Previously the subscriber was installed after `Config::load_or_default`
    // and those warnings were silently swallowed on the daemon path. The
    // log filter comes from `RUST_LOG` (env), not config, so there's no
    // chicken-and-egg.
    let log_file = args
        .log_file
        .as_deref()
        .map(|path| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("opening log file {}", path.display()))
        })
        .transpose()?;
    let to_file = log_file.is_some();
    let writer = match log_file {
        Some(file) => {
            tracing_subscriber::fmt::writer::BoxMakeWriter::new(std::sync::Mutex::new(file))
        }
        // Daemon logs go to stderr — stdout is reserved for any future
        // tool-friendly output, and external tooling (systemd, e2e
        // tests) expects logs on stderr.
        None => tracing_subscriber::fmt::writer::BoxMakeWriter::new(std::io::stderr),
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "muxa=info".into()),
        )
        .with_writer(writer)
        // Disable ANSI escapes when stderr isn't a TTY. systemd-journald
        // captures the bytes verbatim; without this, `journalctl -u muxad`
        // is full of literal `\e[1m` debris. Same logic helps anything
        // grep-ing piped logs (the e2e tests included).
        .with_ansi(!to_file && std::io::stderr().is_terminal())
        .init();
    if to_file {
        // A panic message goes to stderr, which a file-logging daemon does
        // not keep; record it where the rest of the log is.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            tracing::error!(%info, "muxad panicked");
            previous(info);
        }));
    }

    let config_path = args.config.clone().or_else(paths::default_config_file);
    let mut cfg = Config::load_or_default(config_path.as_deref()).context("loading config")?;
    if let Some(path) = config_path.as_deref() {
        match muxa::automation_defaults::install(path) {
            Ok(true) => {
                cfg =
                    Config::load_or_default(Some(path)).context("reloading default automations")?;
                tracing::info!("installed missing disabled default automations");
            }
            Ok(false) => {}
            Err(error) => tracing::warn!(%error, "could not install disabled default automations"),
        }
    }
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

    let restart = Arc::new(
        RestartController::new(restart_generation(), shutdown_tx.clone())
            .with_upgrade_check(muxa::binary_upgrade::UpgradeCheck::current()),
    );
    install_shutdown_signal_handler(Arc::clone(&restart));

    // Prompt history must exist before the store: every PromptSubmitted
    // event fans out into history alongside the live agent record.
    let pane_session_cache = PaneSessionCache::default();
    let (history, history_writer_handle) =
        build_history(&cfg, &writer_shutdown_tx, pane_session_cache.clone()).await;
    let store = Store::shared_with_history(history.clone());
    let collaboration = build_collaboration(&cfg).await?;
    let collaboration_audit = build_collaboration_audit(&cfg);
    let ask = build_ask(&cfg, config_path.clone(), Some(socket.clone())).await;
    let automation = build_automation(&cfg, config_path.clone());
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
    store
        .reconcile_native_surfaces(|id| sessions.session_liveness(id))
        .await;
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
        collaboration.subscribe(),
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
    let directory_max_age = if cfg.reconciler.enabled {
        cfg.reconciler.interval_secs.saturating_add(2)
    } else {
        2
    };
    let agent_directory = Arc::new(muxa::agent_directory::AgentDirectory::with_max_age(
        std::time::Duration::from_secs(directory_max_age),
    ));
    let reconciler_handle = spawn_reconciler_task(
        &cfg,
        &store,
        &shutdown_tx,
        backends.clone(),
        sessions.clone(),
        Arc::clone(&agent_directory),
    );
    let binary_watch_handle = spawn_binary_watch_task(&cfg, &shutdown_tx, Arc::clone(&restart));
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
    // Daemon-owned swap jobs: automation swaps and `muxa swap` requests
    // run through them. Without them no swap runs at all — there is no
    // unjournalled path to fall back to.
    let swap_service = open_swap_service(
        store.clone(),
        backends.clone(),
        cfg.swap.clone(),
        cfg.accounts.clone(),
        config_path.clone(),
        collaboration.clone(),
    )
    .await;
    // Automation engine. Subscribes to the same transition broadcast the
    // waker uses, so it must be spawned before the IPC server takes
    // ownership of `store`.
    let automation_handle = spawn_automation_task(
        automation.clone(),
        ask.clone(),
        store.clone(),
        backends.clone(),
        cfg.swap.clone(),
        cfg.accounts.clone(),
        collaboration.clone(),
        swap_service.clone(),
        &shutdown_tx,
    );

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
        tokio::spawn(Notifier::new().run_human_requests(collaboration.clone()));
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
            collaboration: Some(collaboration.clone()),
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
    // WS-F: snapshot
    let mux_snapshots = muxa::mux_snapshot_control::MuxSnapshotControl::new(socket.clone());
    let mux_snapshot_handle =
        spawn_mux_snapshot_task(&cfg.snapshot, Arc::clone(&mux_snapshots), &shutdown_tx);
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
        .with_agent_directory(agent_directory)
        .with_automation(automation)
        .with_config_path(config_path.clone())
        .with_fleet(fleet_runtime)
        .with_pipeline_runs(pipeline_runs.clone())
        .with_mux_snapshots(mux_snapshots) // WS-F: snapshot
        .with_swap(
            swap_service
                .as_ref()
                .map(|swap| Arc::clone(&swap.service) as Arc<dyn muxa::swap_service::SwapJobs>),
        )
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
        pipeline_reconciler::spawn(pipeline_runs, socket.clone(), &shutdown_tx);

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
    // Swap jobs: take no new one, and give running ones a short moment to
    // reach their end while the runtime and stores they use still exist.
    // A job still running when it passes is recovered on the next start as
    // needing reconciliation, never replayed. Budget: IPC drain (5 s) +
    // this + runtime teardown (2 s) stays under launchd's 20 s; the 2 s
    // per-task timeouts below are a worst-case ceiling on top that only
    // hung tasks reach.
    if let Some(swap) = &swap_service {
        if !swap.service.drain(SWAP_DRAIN_ON_SHUTDOWN).await {
            tracing::warn!(
                "swap jobs still running at shutdown; they will be marked for reconciliation on the next start"
            );
        }
    }
    await_shutdown_task("gc", Some(gc_handle)).await;
    await_shutdown_task("reconciler", reconciler_handle).await;
    await_shutdown_task("binary watch", binary_watch_handle).await;
    await_shutdown_task("herdr bridge", herdr_bridge_handle).await;
    await_shutdown_task("herdr report", herdr_report_handle).await;
    await_shutdown_task("screen detection", screen_detect_handle).await;
    await_shutdown_task("session activity", session_activity_handle).await;
    await_shutdown_task("history compaction", history_compaction_handle).await;
    await_shutdown_task("activity compaction", activity_compaction_handle).await;
    await_shutdown_task("collaboration waker", collaboration_waker_handle).await;
    await_shutdown_task("automation", Some(automation_handle)).await;
    await_shutdown_task("pipeline reconciler", Some(pipeline_reconciler_handle)).await;
    await_shutdown_task("pipeline state projection", Some(pipeline_state_handle)).await;
    await_shutdown_task("fleet manager", Some(fleet_handle)).await;
    await_shutdown_task("workspace snapshot", mux_snapshot_handle).await; // WS-F: snapshot

    let _ = activity_transition_shutdown_tx.send(());
    await_shutdown_task("activity transition", activity_transition_handle).await;

    let _ = writer_shutdown_tx.send(());
    await_shutdown_task("history writer", history_writer_handle).await;
    await_shutdown_task("activity writer", activity_writer_handle).await;

    let _ = snap_shutdown_tx.send(());
    await_shutdown_task("state snapshotter", snap_handle).await;
    server_result??;
    if restart.restart_requested() {
        return Err(reexec_self(restart.validated_launch().await).into());
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
///
/// Windows has no `exec`: the replacement is spawned as a new process and this
/// one exits once the spawn succeeds. The IPC endpoint was already released by
/// the drain above, so the successor can bind it; the pid changes.
fn reexec_self(approved: Option<muxa::binary_upgrade::ValidatedLaunch>) -> std::io::Error {
    #[cfg(unix)]
    use std::os::unix::process::CommandExt;

    let mut argv = std::env::args_os();
    let Some(program) = argv.next() else {
        return std::io::Error::other("cannot restart: argv[0] is missing");
    };
    let next = restart_generation().saturating_add(1);
    tracing::info!(?program, generation = next, "restarting: re-executing self");
    let target = match &approved {
        Some(checked) => match checked.execution_path() {
            Ok(path) => path,
            Err(error) => return error,
        },
        None => PathBuf::from(&program),
    };
    let mut command = std::process::Command::new(&target);
    #[cfg(unix)]
    command.arg0(&program);
    command
        .args(argv)
        .env(RESTART_GENERATION_ENV, next.to_string());
    #[cfg(unix)]
    {
        command.exec()
    }
    #[cfg(not(unix))]
    {
        match command.spawn() {
            Ok(_) => std::process::exit(0),
            Err(error) => error,
        }
    }
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
async fn build_ask(
    cfg: &Config,
    config_path: Option<PathBuf>,
    socket: Option<PathBuf>,
) -> Arc<AskStore> {
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
        providers: cfg.ask.providers.clone(),
        config_path,
        socket,
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

async fn build_collaboration(cfg: &Config) -> Result<Arc<CollaborationStore>> {
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
        retention_days: cfg.collaboration.retention_days,
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
            Ok(store)
        }
        Err(error @ muxa::collaboration::CollaborationError::UnsupportedSchema(_)) => {
            Err(anyhow::Error::new(error)
                .context("unsupported durable mailbox schema; refusing a downgrade"))
        }
        Err(error) => {
            tracing::error!(%error,"collaboration disabled: durable mailbox could not be loaded; other daemon services remain available");
            Ok(CollaborationStore::unavailable(options, error.to_string()))
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

// WS-F: snapshot
/// Automatic workspace snapshots (`[snapshot]`, on by default every 15
/// minutes). The first one waits a full interval: right after a reboot the
/// multiplexer is empty or half rebuilt, and that is the workspace nobody
/// wants as the newest snapshot. `muxa snapshot --auto` itself skips an
/// unchanged workspace and prunes old automatic snapshots.
fn spawn_mux_snapshot_task(
    config: &muxa::config::SnapshotConfig,
    control: Arc<muxa::mux_snapshot_control::MuxSnapshotControl>,
    shutdown_tx: &broadcast::Sender<()>,
) -> Option<tokio::task::JoinHandle<()>> {
    if config.auto_interval_minutes == 0 {
        return None;
    }
    let period = std::time::Duration::from_secs(config.auto_interval_minutes.saturating_mul(60));
    let keep = config.keep_auto;
    let mut shutdown = shutdown_tx.subscribe();
    Some(tokio::spawn(async move {
        let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticks.tick() => {}
                _ = shutdown.recv() => break,
            }
            match control.auto(keep).await {
                Ok(outcome) if outcome["skipped"].as_bool() == Some(true) => {
                    let reason = outcome["reason"].as_str().unwrap_or("skipped");
                    if reason == "ambiguous_server" {
                        tracing::warn!(
                            detail = outcome["message"].as_str().unwrap_or_default(),
                            "automatic snapshot skipped"
                        );
                    } else {
                        tracing::debug!(reason, "automatic snapshot skipped");
                    }
                }
                Ok(outcome) => tracing::debug!(
                    id = outcome["id"].as_str().unwrap_or_default(),
                    "automatic snapshot written"
                ),
                Err(error) => tracing::warn!(%error, "automatic snapshot failed"),
            }
        }
    }))
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

/// Resolve `[automation]` into a live store. The ledger is materialized
/// unconditionally — the guards read from it, and an operator who adds a
/// rule at runtime should not have to restart the daemon to get one.
fn build_automation(cfg: &Config, config_path: Option<PathBuf>) -> Arc<AutomationStore> {
    let ledger = AutomationLedger::load(muxa::automation::default_automation_file());
    let store = AutomationStore::new(cfg.automation.clone(), config_path, ledger);
    let enabled_rules = cfg
        .automation
        .rule
        .iter()
        .filter(|rule| rule.enabled)
        .count();
    if enabled_rules > 0 {
        tracing::info!(
            rules = cfg.automation.rule.len(),
            enabled = enabled_rules,
            engine_enabled = cfg.automation.enabled,
            paused_until = ?cfg.automation.paused_until,
            "automation rules loaded",
        );
    }
    store
}

/// How long shutdown gives running swap jobs to reach their end; see the
/// shutdown sequence for how it fits the stop budget.
const SWAP_DRAIN_ON_SHUTDOWN: std::time::Duration = std::time::Duration::from_secs(8);

/// How long the automation engine waits on one swap job. Running out only
/// stops the wait; the job carries on under the service.
const AUTOMATION_SWAP_WAIT: std::time::Duration = std::time::Duration::from_secs(600);

/// The swap job service and the node it belongs to.
struct DaemonSwap {
    service: Arc<muxa::swap_service::SwapService>,
    node_id: String,
}

/// Open the swap job service off the reactor. A node identity that cannot
/// be persisted disables swaps rather than inventing one: repeat requests
/// are recognised by it across restarts.
async fn open_swap_service(
    store: muxa::SharedStore,
    backends: Vec<muxa::SharedBackend>,
    swap: muxa::account_swap::SwapConfig,
    accounts: muxa::accounts::AccountsConfig,
    config_path: Option<std::path::PathBuf>,
    collaboration: Arc<CollaborationStore>,
) -> Option<Arc<DaemonSwap>> {
    let runtime = tokio::runtime::Handle::current();
    let receipts = Arc::new(muxa::swap_service::PromptReceipts::default());
    spawn_prompt_receipts(&store, Arc::clone(&receipts));
    let opened = tokio::task::spawn_blocking(move || {
        let path = paths::default_node_id_file().ok_or("no data directory for the node id")?;
        let node_id = muxa::fleet::load_or_create_node_id(&path)
            .map_err(|error| format!("node id at {}: {error}", path.display()))?
            .as_str()
            .to_owned();
        let context = DaemonContext {
            runtime,
            store,
            backends,
            swap,
            accounts,
            config_path,
            node_id: node_id.clone(),
            collaboration,
            receipts,
        };
        muxa::swap_service::SwapService::open(None, Arc::new(context)).map(|service| DaemonSwap {
            service: Arc::new(service),
            node_id,
        })
    })
    .await;
    match opened {
        Ok(Ok(swap)) => Some(Arc::new(swap)),
        Ok(Err(error)) => {
            tracing::warn!(%error, "swap jobs disabled");
            None
        }
        Err(error) => {
            tracing::warn!(%error, "swap jobs disabled: could not open their store");
            None
        }
    }
}

/// Feed every agent prompt hook the store receives into `receipts`, its
/// text dropped, for a swap follow-up's evidence. A lagging receiver only
/// loses evidence, never claims any.
fn spawn_prompt_receipts(
    store: &muxa::SharedStore,
    receipts: Arc<muxa::swap_service::PromptReceipts>,
) {
    let mut prompts = store.subscribe_prompts();
    tokio::spawn(async move {
        loop {
            match prompts.recv().await {
                Ok(record) => receipts.record(&record),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// What the swap service reads of the daemon. Its methods block on the
/// runtime, so the service only ever calls them from its own threads.
struct DaemonContext {
    runtime: tokio::runtime::Handle,
    store: muxa::SharedStore,
    backends: Vec<muxa::SharedBackend>,
    swap: muxa::account_swap::SwapConfig,
    accounts: muxa::accounts::AccountsConfig,
    config_path: Option<std::path::PathBuf>,
    node_id: String,
    collaboration: Arc<CollaborationStore>,
    receipts: Arc<muxa::swap_service::PromptReceipts>,
}

impl muxa::swap_service::ContextProvider for DaemonContext {
    fn turn_probe(&self) -> Option<Arc<dyn muxa::account_swap::TurnProbe>> {
        Some(Arc::clone(&self.receipts) as Arc<dyn muxa::account_swap::TurnProbe>)
    }

    fn snapshot(&self) -> Result<muxa::swap_service::Snapshot, String> {
        Ok(muxa::swap_service::Snapshot {
            node_id: self.node_id.clone(),
            agents: self.runtime.block_on(self.store.snapshot()),
            backends: self.backends.clone(),
            config: self.swap.clone(),
            accounts: self.accounts.clone(),
            config_path: self.config_path.clone(),
        })
    }

    /// The same exclusion every other writer to the pane takes, and the
    /// same refusal of a pane whose input is blocked or draining.
    fn acquire_guard(
        &self,
        pane: &str,
        socket: Option<&str>,
    ) -> Result<Box<dyn muxa::swap_service::TargetGuard>, String> {
        let collaboration = Arc::clone(&self.collaboration);
        let pane = pane.to_owned();
        let socket = socket.map(str::to_owned);
        self.runtime.block_on(async move {
            let gate = collaboration.worker_action_guard(&pane).await;
            if collaboration
                .worker_input_blocked(&pane, socket.as_deref())
                .await
            {
                return Err("input to this pane is blocked or draining".to_owned());
            }
            Ok(Box::new(PaneGuard {
                pane,
                socket,
                _gate: Arc::new(gate),
            }) as Box<dyn muxa::swap_service::TargetGuard>)
        })
    }
}

/// A held pane guard. The daemon's guard is per pane name, so with no
/// server it covers that pane on any; with one, only that endpoint — a
/// stale server never authorises another.
struct PaneGuard {
    pane: String,
    socket: Option<String>,
    _gate: WorkerIoGuard,
}

impl muxa::swap_service::TargetGuard for PaneGuard {
    fn covers(&self, endpoint: &str) -> bool {
        match &self.socket {
            Some(socket) => endpoint == muxa::swap_service::endpoint(&self.pane, Some(socket)),
            None => endpoint
                .strip_prefix(self.pane.as_str())
                .is_some_and(|rest| rest.starts_with('@')),
        }
    }
}

/// The engine: watch state transitions, arm matching rules, hold their
/// firings until due, re-check them against the live store, and act.
///
/// Modelled on the collaboration waker directly above — same transition
/// subscription, same debounce-by-key discipline, same slow reconcile tick
/// as the recovery path. The differences are that firings are *scheduled*
/// rather than immediate (hence the heap) and that every decision is
/// recorded in the ledger.
///
/// Always spawned, even with no rules: a rule written through
/// `automation_set_rule` must take effect without a daemon restart, and an
/// engine with nothing to do costs one refcount bump per transition.
#[allow(clippy::too_many_arguments)] // Explicit shared services for the automation task.
fn spawn_automation_task(
    automation: Arc<AutomationStore>,
    ask: Arc<AskStore>,
    store: muxa::SharedStore,
    backends: Vec<muxa::SharedBackend>,
    swap: muxa::account_swap::SwapConfig,
    accounts: muxa::accounts::AccountsConfig,
    collaboration: Arc<CollaborationStore>,
    swap_service: Option<Arc<DaemonSwap>>,
    shutdown_tx: &broadcast::Sender<()>,
) -> tokio::task::JoinHandle<()> {
    let mut shutdown_rx = shutdown_tx.subscribe();
    // Subscribe before the first authoritative scan so a transition racing
    // task startup is represented either by that scan or by a pending signal.
    let mut transitions = store.subscribe();
    let mut config_changes = automation.subscribe();
    tokio::spawn(async move {
        let mut engine = AutomationEngine::new(automation, store, backends);
        engine.ask = Some(ask);
        engine.swap = swap;
        engine.accounts = accounts;
        engine.collaboration = Some(collaboration);
        engine.swap_service = swap_service;
        engine.rescan().await;
        let mut reconcile =
            tokio::time::interval(std::time::Duration::from_secs(AUTOMATION_RECONCILE_SECONDS));
        reconcile.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        reconcile.tick().await;
        loop {
            let idle = engine.time_until_next_firing();
            tokio::select! {
                transition = transitions.recv() => match transition {
                    Ok(transition) => engine.observe(&transition.agent).await,
                    // A lag means an arming transition may have been dropped.
                    // Re-read the authoritative state immediately.
                    Err(broadcast::error::RecvError::Lagged(dropped)) => {
                        tracing::debug!(dropped, "automation transition stream lagged");
                        engine.rescan().await;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                changed = config_changes.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    engine.pending.clear();
                    engine.armed.clear();
                    engine.queued_rules.clear();
                    engine.drain_reservations.clear();
                    // A rule was added, edited, enabled, or paused. Drop
                    // pending firings whose rule no longer exists and
                    // re-evaluate everything against the new set.
                    engine.rescan().await;
                }
                () = tokio::time::sleep(idle) => engine.fire_due().await,
                completed = engine.judgments.join_next(), if !engine.judgments.is_empty() => {
                    match completed {
                        Some(Ok(completed)) => engine.finish_judgment(completed).await,
                        Some(Err(error)) => tracing::warn!(%error, "automation judgment task failed; no action taken"),
                        None => {}
                    }
                }
                _ = reconcile.tick() => engine.rescan().await,
                _ = shutdown_rx.recv() => break,
            }
        }
    })
}

/// Live state of the automation engine: the pending firing queue, the
/// in-flight key set, and a short-lived pane scan for the workspace/work
/// filters.
struct AutomationEngine {
    automation: Arc<AutomationStore>,
    store: muxa::SharedStore,
    backends: Vec<muxa::SharedBackend>,
    /// Firings waiting on their fire time, earliest first.
    pending: BinaryHeap<std::cmp::Reverse<PlannedFiring>>,
    /// `(rule, pane, episode)` already queued. Together with the ledger's
    /// own episode check this is what makes "never twice for one cap
    /// episode" hold both within a daemon run and across a restart.
    armed: HashSet<(String, String, String)>,
    queued_rules: HashMap<(String, String, String), (AutomationRule, u64)>,
    collaboration: Option<Arc<CollaborationStore>>,
    drain_reservations:
        HashMap<(String, String, String), muxa::collaboration::WorkerAutomationReservation>,
    panes: Vec<muxa::tmux::PaneInfo>,
    panes_read_at: Option<std::time::Instant>,
    ask: Option<Arc<AskStore>>,
    judgments: tokio::task::JoinSet<JudgedFiring>,
    /// `[swap]` defaults for the `swap_account` action.
    swap: muxa::account_swap::SwapConfig,
    /// Daemon-owned swap jobs the `swap_account` action submits to; `None`
    /// when they could not be opened, in which case that action is refused.
    swap_service: Option<Arc<DaemonSwap>>,
    /// `[accounts]`, which the `swap_account` action moves agents between
    /// when a provider lists them.
    accounts: muxa::accounts::AccountsConfig,
}

struct JudgedFiring {
    firing: PlannedFiring,
    rule: AutomationRule,
    subject: AutomationSubject,
    revision: u64,
    context: Option<JudgmentContext>,
    judgment: AutomationJudgment,
    _drain_reservation: Option<muxa::collaboration::WorkerAutomationReservation>,
}

impl AutomationEngine {
    fn new(
        automation: Arc<AutomationStore>,
        store: muxa::SharedStore,
        backends: Vec<muxa::SharedBackend>,
    ) -> Self {
        Self {
            automation,
            store,
            backends,
            pending: BinaryHeap::new(),
            armed: HashSet::new(),
            queued_rules: HashMap::new(),
            collaboration: None,
            drain_reservations: HashMap::new(),
            panes: Vec::new(),
            panes_read_at: None,
            ask: None,
            judgments: tokio::task::JoinSet::new(),
            swap: muxa::account_swap::SwapConfig::default(),
            swap_service: None,
            accounts: muxa::accounts::AccountsConfig::default(),
        }
    }

    /// How long to park before the next firing is due, bounded so a rule
    /// added at runtime is still picked up promptly.
    fn time_until_next_firing(&self) -> std::time::Duration {
        let cap = std::time::Duration::from_secs(AUTOMATION_MAX_IDLE_SECONDS);
        let Some(next) = self.pending.peek() else {
            return cap;
        };
        let remaining = next.0.fire_at - time::OffsetDateTime::now_utc();
        if remaining.is_negative() {
            return std::time::Duration::ZERO;
        }
        remaining.unsigned_abs().min(cap)
    }

    /// Evaluate every enabled rule against one agent that just changed state.
    async fn observe(&mut self, agent: &muxa::Agent) {
        let now = time::OffsetDateTime::now_utc();
        let (active, rules) = self.automation.active_rules(now).await;
        if !active || rules.is_empty() {
            return;
        }
        let subject = self.subject_for(&rules, agent).await;
        self.arm_all(&rules, std::slice::from_ref(&subject), now)
            .await;
    }

    /// Authoritative pass over the whole registry. Recovers the two cases a
    /// transition cannot cover: a row already capped when the daemon
    /// started, and a `RateLimited` landing on a row that was already in
    /// `Error` (no state change, so no transition is broadcast).
    async fn rescan(&mut self) {
        let now = time::OffsetDateTime::now_utc();
        let (active, rules) = self.automation.active_rules(now).await;
        // A firing whose rule was disabled or deleted must not survive.
        let live: HashSet<&str> = rules.iter().map(|rule| rule.name.as_str()).collect();
        self.pending
            .retain(|firing| live.contains(firing.0.rule.as_str()));
        self.armed
            .retain(|(rule, _, _)| live.contains(rule.as_str()));
        self.queued_rules
            .retain(|(rule, _, _), _| live.contains(rule.as_str()));
        self.drain_reservations
            .retain(|(rule, _, _), _| live.contains(rule.as_str()));
        if !active || rules.is_empty() {
            return;
        }
        let agents = self.store.snapshot().await;
        let subjects = if rules.iter().any(AutomationRule::needs_pane_metadata) {
            self.refresh_panes().await;
            subjects_from(&agents, &self.panes)
        } else {
            subjects_from(&agents, &[])
        };
        self.arm_all(&rules, &subjects, now).await;
    }

    async fn arm_all(
        &mut self,
        rules: &[AutomationRule],
        subjects: &[AutomationSubject],
        now: time::OffsetDateTime,
    ) {
        let revision = *self.automation.subscribe().borrow();
        let config = self.automation.config().await;
        let ledger = self.automation.ledger();
        for rule in rules {
            if config.rule_named(&rule.name) != Some(rule) {
                continue;
            }
            for subject in subjects {
                let Some(pane) = subject.pane.clone() else {
                    continue;
                };
                // Cheap pre-filter before touching the ledger. Most
                // evaluations are "this agent is not in the state this rule
                // watches", and a guard read scans up to 500 entries.
                if subject.current_event() != Some(rule.on) {
                    continue;
                }
                let episode = subject.episode();
                let key = (rule.name.clone(), pane.clone(), episode.clone());
                if self.armed.contains(&key) {
                    continue;
                }
                let guards = ledger.guard_state(&rule.name, &pane, &episode, now).await;
                match Scheduler::arm(&config, rule, subject, guards, now, jitter_ratio()) {
                    Decision::Fire(firing) => {
                        if let Some(collaboration) = &self.collaboration {
                            let Some(reservation) = collaboration
                                .reserve_worker_automation(
                                    &subject.agent_session_id,
                                    &pane,
                                    subject.socket.as_deref(),
                                )
                                .await
                            else {
                                continue;
                            };
                            self.drain_reservations.insert(key.clone(), reservation);
                        }
                        tracing::info!(
                            rule = %rule.name,
                            pane = %pane,
                            action = %firing.action,
                            fire_at = %firing.fire_at,
                            "automation armed",
                        );
                        self.queued_rules
                            .insert(key.clone(), (rule.clone(), revision));
                        self.armed.insert(key);
                        self.pending.push(std::cmp::Reverse(*firing));
                    }
                    // Arm-time skips are the common case (every rule sees
                    // every agent), so they stay in the trace rather than
                    // flooding the ledger. Fire-time skips are recorded.
                    Decision::Skip(reason) => tracing::trace!(
                        rule = %rule.name,
                        pane = %pane,
                        %reason,
                        "automation did not arm",
                    ),
                }
            }
        }
    }

    /// Act on every firing whose time has come, re-checking each against
    /// the live store first.
    async fn fire_due(&mut self) {
        let now = time::OffsetDateTime::now_utc();
        let mut due = Vec::new();
        while self
            .pending
            .peek()
            .is_some_and(|firing| firing.0.fire_at <= now)
        {
            if let Some(std::cmp::Reverse(firing)) = self.pending.pop() {
                due.push(firing);
            }
        }
        for firing in due {
            self.armed.remove(&(
                firing.rule.clone(),
                firing.pane.clone(),
                firing.episode.clone(),
            ));
            self.fire(&firing, now).await;
            let key = (
                firing.rule.clone(),
                firing.pane.clone(),
                firing.episode.clone(),
            );
            if !self.armed.contains(&key) {
                self.drain_reservations.remove(&key);
            }
        }
    }

    async fn fire(&mut self, firing: &PlannedFiring, now: time::OffsetDateTime) {
        let key = (
            firing.rule.clone(),
            firing.pane.clone(),
            firing.episode.clone(),
        );
        let Some((queued_rule, revision)) = self.queued_rules.remove(&key) else {
            return;
        };
        let config = self.automation.config().await;
        let Some(rule) = config.rule_named(&firing.rule).cloned() else {
            // The rule was removed while this firing waited.
            return;
        };
        let subject = self.live_subject(&rule, &firing.agent_session_id).await;
        let ledger = self.automation.ledger();
        if queued_rule != rule
            || revision != *self.automation.subscribe().borrow()
            || subject
                .as_ref()
                .is_some_and(|subject| subject.socket != firing.socket)
        {
            ledger
                .append(ledger_entry(
                    firing,
                    now,
                    AutomationOutcome::Skipped,
                    Some("Queued rule or pane endpoint changed".into()),
                ))
                .await;
            return;
        }
        let guards = ledger
            .guard_state(&firing.rule, &firing.pane, &firing.episode, now)
            .await;
        match Scheduler::confirm(&config, &rule, firing, subject.as_ref(), guards, now) {
            Decision::Skip(reason) => {
                tracing::info!(
                    rule = %firing.rule,
                    pane = %firing.pane,
                    %reason,
                    "automation skipped at fire time",
                );
                ledger
                    .append(ledger_entry(
                        firing,
                        now,
                        AutomationOutcome::Skipped,
                        Some(reason.to_string()),
                    ))
                    .await;
            }
            Decision::Fire(confirmed) => {
                if rule.ask_condition.is_some() {
                    if let Some(subject) = subject {
                        self.start_judgment(*confirmed, rule, subject, now, revision)
                            .await;
                    }
                    return;
                }
                let delivered = self.perform(&confirmed, &rule, revision).await;
                let outcome = if delivered {
                    AutomationOutcome::Fired
                } else {
                    AutomationOutcome::Failed
                };
                let detail = if delivered {
                    confirmed.text.clone()
                } else {
                    Some(SkipReason::ActionFailed.to_string())
                };
                tracing::info!(
                    rule = %confirmed.rule,
                    pane = %confirmed.pane,
                    action = %confirmed.action,
                    %outcome,
                    "automation acted",
                );
                ledger
                    .append(ledger_entry(&confirmed, now, outcome, detail))
                    .await;
            }
        }
    }

    async fn start_judgment(
        &mut self,
        firing: PlannedFiring,
        rule: AutomationRule,
        subject: AutomationSubject,
        now: time::OffsetDateTime,
        revision: u64,
    ) {
        let slot = self.automation.try_judgment_slot();
        if self.judgments.len() >= 2 || slot.is_err() {
            let mut deferred = firing;
            deferred.fire_at = now + time::Duration::seconds(1);
            self.queued_rules.insert(
                (
                    deferred.rule.clone(),
                    deferred.pane.clone(),
                    deferred.episode.clone(),
                ),
                (rule, revision),
            );
            self.armed.insert((
                deferred.rule.clone(),
                deferred.pane.clone(),
                deferred.episode.clone(),
            ));
            self.pending.push(std::cmp::Reverse(deferred));
            return;
        }
        let permit = slot.expect("judgment slot checked");
        if revision != *self.automation.subscribe().borrow() {
            return;
        }
        let condition = rule
            .ask_condition
            .as_ref()
            .expect("Ask condition checked")
            .clone();
        let ledger = self.automation.ledger();
        let reservation = ledger_entry(
            &firing,
            now,
            AutomationOutcome::Judging,
            Some(format!(
                "Ask provider {}: judgment reserved",
                condition.provider
            )),
        );
        if let Err(reason) = ledger
            .reserve_judgment(reservation, condition.max_per_hour, rule.cooldown())
            .await
        {
            ledger
                .append(ledger_entry(
                    &firing,
                    now,
                    AutomationOutcome::Skipped,
                    Some(reason),
                ))
                .await;
            return;
        }
        let ask = self.ask.clone();
        let backends = self.backends.clone();
        let drain_reservation = self.drain_reservations.remove(&(
            firing.rule.clone(),
            firing.pane.clone(),
            firing.episode.clone(),
        ));
        self.judgments.spawn(async move {
            let _permit = permit;
            let context = automation_judge::capture_context(&subject, &backends).await;
            let judgment = match (&ask, &context) {
                (Some(ask), Ok(context)) => {
                    automation_judge::evaluate(ask, &condition, context).await
                }
                (_, Err(_)) => {
                    AutomationJudgment::unknown(&condition, "Current pane context is unavailable")
                }
                (None, _) => AutomationJudgment::unknown(&condition, "Ask is unavailable"),
            };
            JudgedFiring {
                firing,
                rule,
                subject,
                revision,
                context: context.ok(),
                judgment,
                _drain_reservation: drain_reservation,
            }
        });
    }

    async fn finish_judgment(&mut self, completed: JudgedFiring) {
        let condition = completed
            .rule
            .ask_condition
            .as_ref()
            .expect("judged rule has a condition");
        let mut execution = "not_matched".to_string();
        let mut outcome = AutomationOutcome::Judged;
        if condition.observe_only {
            execution = "observe_only".into();
        } else if completed.judgment.decision == JudgmentDecision::Match {
            match self.confirm_judgment(&completed).await {
                Ok(()) => {
                    if self
                        .perform(&completed.firing, &completed.rule, completed.revision)
                        .await
                    {
                        execution = "fired".into();
                        outcome = AutomationOutcome::Fired;
                    } else {
                        execution = "action_failed".into();
                        outcome = AutomationOutcome::Failed;
                    }
                }
                Err(reason) => execution = reason,
            }
        }
        let detail = serde_json::json!({
            "judgment": completed.judgment,
            "condition": condition.prompt,
            "observe_only": condition.observe_only,
            "execution": execution,
        })
        .to_string();
        self.automation
            .ledger()
            .append(ledger_entry(
                &completed.firing,
                time::OffsetDateTime::now_utc(),
                outcome,
                Some(detail),
            ))
            .await;
    }

    async fn confirm_judgment(&mut self, completed: &JudgedFiring) -> Result<(), String> {
        if *self.automation.subscribe().borrow() != completed.revision {
            return Err("configuration_changed".into());
        }
        self.panes_read_at = None;
        let subject = self
            .live_subject(&completed.rule, &completed.firing.agent_session_id)
            .await
            .ok_or("pane_gone")?;
        if subject != completed.subject {
            return Err("agent_state_changed".into());
        }
        let fresh = automation_judge::capture_context(&subject, &self.backends)
            .await
            .map_err(|_| "context_unavailable")?;
        if completed.context.as_ref() != Some(&fresh) {
            return Err("context_changed".into());
        }
        let config = self.automation.config().await;
        let Some(rule) = config.rule_named(&completed.firing.rule) else {
            return Err("rule_removed".into());
        };
        if rule != &completed.rule || *self.automation.subscribe().borrow() != completed.revision {
            return Err("configuration_changed".into());
        }
        let now = time::OffsetDateTime::now_utc();
        let latest = self
            .live_subject(rule, &completed.firing.agent_session_id)
            .await;
        if latest.as_ref() != Some(&completed.subject) {
            return Err("agent_state_changed".into());
        }
        let mut guards = self
            .automation
            .ledger()
            .guard_state(
                &completed.firing.rule,
                &completed.firing.pane,
                &completed.firing.episode,
                now,
            )
            .await;
        guards.episode_handled = false;
        match Scheduler::confirm(
            &config,
            rule,
            &completed.firing,
            latest.as_ref(),
            guards,
            now,
        ) {
            Decision::Fire(_) => Ok(()),
            Decision::Skip(reason) => Err(reason.to_string()),
        }
    }

    /// Carry out one action. Every arm keeps the injection inside this
    /// function, so "what an automation can do to a pane" has exactly one
    /// place to read.
    async fn perform(
        &mut self,
        firing: &PlannedFiring,
        rule: &AutomationRule,
        revision: u64,
    ) -> bool {
        let io_guard = if let Some(collaboration) = &self.collaboration {
            let gate = collaboration.worker_action_guard(&firing.pane).await;
            if collaboration
                .worker_input_blocked(&firing.pane, firing.socket.as_deref())
                .await
            {
                return false;
            }
            Some(Arc::new(gate))
        } else {
            None
        };
        match firing.action {
            AutomationAction::PressEnter => {
                self.confirm_selected_yes(firing, io_guard.clone()).await
            }
            AutomationAction::SendPrompt => {
                let Some(text) = firing.text.as_deref() else {
                    return false;
                };
                if !send_automation_text(&self.backends, firing, text, io_guard.clone()).await {
                    return false;
                }
                if !firing.submit {
                    return true;
                }
                // Native hosts use explicit paste boundaries; only hosts
                // without that support need a best-effort fallback delay.
                tokio::time::sleep(muxa::backend::prompt_submit_grace(&firing.pane)).await;
                send_automation_text(&self.backends, firing, "\r", io_guard.clone()).await
            }
            // A notice, not a keystroke: nothing is typed into the pane. The
            // ledger entry is the durable half, and `muxa automation log`
            // is where an operator reads it back.
            AutomationAction::Notify => {
                tracing::warn!(
                    rule = %firing.rule,
                    pane = %firing.pane,
                    agent = %firing.agent,
                    message = firing.text.as_deref().unwrap_or_default(),
                    "automation notice",
                );
                true
            }
            // ETX — the same byte `muxa agent control --action interrupt`
            // writes into a native PTY session.
            AutomationAction::Interrupt => {
                send_automation_text(&self.backends, firing, "\u{3}", io_guard.clone()).await
            }
            // The guard moves into the swap job: no copy of it is kept here
            // while the job runs.
            AutomationAction::SwapAccount => {
                self.swap_account(firing, rule, revision, io_guard).await
            }
        }
    }

    /// Move the capped agent onto another `aas` account and resume it. The
    /// firing was re-checked just before this, so the agent is the one the
    /// rule armed on; `force` only lets a cap that reads as `Starting` through.
    async fn swap_account(
        &mut self,
        firing: &PlannedFiring,
        rule: &AutomationRule,
        revision: u64,
        io_guard: Option<WorkerIoGuard>,
    ) -> bool {
        let Some(swap) = self.swap_service.clone() else {
            tracing::warn!(
                rule = %firing.rule,
                pane = %firing.pane,
                "automation swap refused: swap jobs are unavailable on this daemon"
            );
            return false;
        };
        // The guard may have been waited on: the firing has to still be due,
        // for the same rule, agent and server, now that it is held.
        if let Err(reason) = self.swap_still_due(firing, rule, revision).await {
            tracing::info!(
                rule = %firing.rule,
                pane = %firing.pane,
                %reason,
                "automation swap no longer due once the pane was free"
            );
            return false;
        }
        let key = muxa::swap_service::automation_request_key(
            &swap.node_id,
            &firing.pane,
            firing.socket.as_deref(),
            &firing.agent_session_id,
            &firing.rule,
            &firing.episode,
        );
        let overrides = muxa::swap_service::SwapOverrides {
            targets: vec![firing.agent_session_id.clone()],
            to: firing.account.clone(),
            force: true,
            continue_text: firing.text.clone(),
            ..muxa::swap_service::SwapOverrides::default()
        };
        let guard = io_guard.map(|gate| {
            Box::new(PaneGuard {
                pane: firing.pane.clone(),
                socket: firing.socket.clone(),
                _gate: gate,
            }) as Box<dyn muxa::swap_service::TargetGuard>
        });
        let service = Arc::clone(&swap.service);
        let submit_key = key.clone();
        let accepted = tokio::task::spawn_blocking(move || {
            service.submit_automation(&submit_key, &overrides, guard)
        })
        .await;
        let job = match accepted {
            Ok(Ok(accepted)) => accepted.job_id,
            Ok(Err(error)) => {
                tracing::warn!(rule = %firing.rule, pane = %firing.pane, %key, %error, "automation swap not accepted");
                return false;
            }
            Err(error) => {
                tracing::warn!(rule = %firing.rule, pane = %firing.pane, %key, %error, "automation swap submission failed");
                return false;
            }
        };
        // The engine waits here, one firing at a time; a wait that runs out
        // only stops waiting — the job runs on under the service.
        match swap.service.wait(&job, AUTOMATION_SWAP_WAIT).await {
            Ok(view) => {
                let swapped = view.entries.len() == 1
                    && view.entries[0].disposition.as_deref() == Some("swapped");
                tracing::info!(
                    rule = %firing.rule,
                    pane = %firing.pane,
                    job = %job,
                    state = %view.state,
                    disposition = view.entries.first().and_then(|entry| entry.disposition.as_deref()).unwrap_or("none"),
                    "automation account swap",
                );
                swapped
            }
            Err(error) => {
                tracing::warn!(
                    rule = %firing.rule,
                    pane = %firing.pane,
                    job = %job,
                    %error,
                    "automation account swap incomplete"
                );
                false
            }
        }
    }

    /// Whether `firing` is still due now that its pane's guard is held:
    /// the same rule and configuration revision, the agent still there on
    /// the same pane and server, and the scheduler still saying fire.
    async fn swap_still_due(
        &mut self,
        firing: &PlannedFiring,
        expected: &AutomationRule,
        revision: u64,
    ) -> Result<(), String> {
        // Rescan first: everything read after this await is current, and a
        // rule edited while the scan ran fails the revision check below.
        if expected.needs_pane_metadata() {
            self.panes_read_at = None;
            self.refresh_panes().await;
        }
        let config = self.automation.config().await;
        let Some(rule) = config.rule_named(&firing.rule).cloned() else {
            return Err("rule_removed".into());
        };
        if rule != *expected || *self.automation.subscribe().borrow() != revision {
            return Err("configuration_changed".into());
        }
        let latest = self.live_subject(&rule, &firing.agent_session_id).await;
        match &latest {
            None => return Err("agent_gone".into()),
            Some(subject)
                if subject.socket != firing.socket
                    || subject.pane.as_deref() != Some(firing.pane.as_str()) =>
            {
                return Err("endpoint_changed".into());
            }
            Some(_) => {}
        }
        let now = time::OffsetDateTime::now_utc();
        let guards = self
            .automation
            .ledger()
            .guard_state(&firing.rule, &firing.pane, &firing.episode, now)
            .await;
        if let Decision::Skip(reason) =
            Scheduler::confirm(&config, &rule, firing, latest.as_ref(), guards, now)
        {
            return Err(reason.to_string());
        }
        // The ledger read awaited too: nothing may have moved since.
        if *self.automation.subscribe().borrow() != revision {
            return Err("configuration_changed".into());
        }
        let Some(agent) = self.store.by_session(&firing.agent_session_id).await else {
            return Err("agent_gone".into());
        };
        let now_subject = AutomationSubject::from_agent(&agent);
        if now_subject.episode() != firing.episode
            || now_subject.socket != firing.socket
            || now_subject.pane.as_deref() != Some(firing.pane.as_str())
        {
            return Err("agent_state_changed".into());
        }
        if *self.automation.subscribe().borrow() != revision {
            return Err("configuration_changed".into());
        }
        Ok(())
    }

    /// Capture immediately before Enter; a waiting state alone cannot identify
    /// the selected option. Recheck state/config after the blocking capture.
    async fn confirm_selected_yes(
        &self,
        firing: &PlannedFiring,
        io_guard: Option<WorkerIoGuard>,
    ) -> bool {
        let revision = *self.automation.subscribe().borrow();
        let Some(kind) = muxa::backend::pane_id_host_kind(&firing.pane) else {
            return false;
        };
        let Some(backend) = self
            .backends
            .iter()
            .find(|backend| backend.kind() == kind)
            .cloned()
        else {
            return false;
        };
        let pane = firing.pane.clone();
        let socket = firing.socket.clone();
        let capture =
            tokio::task::spawn_blocking(move || backend.capture_pane_on(socket.as_deref(), &pane));
        let Ok(Ok(Some(screen))) =
            tokio::time::timeout(std::time::Duration::from_secs(3), capture).await
        else {
            return false;
        };
        if !muxa::automation_choice::affirmative_choice_visible(&screen) {
            return false;
        }
        let config = self.automation.config().await;
        let Some(rule) = config.rule_named(&firing.rule) else {
            return false;
        };
        if revision != *self.automation.subscribe().borrow() {
            return false;
        }
        if !config.active_at(time::OffsetDateTime::now_utc())
            || !rule.enabled
            || rule.action != AutomationAction::PressEnter
        {
            return false;
        }
        let Some(agent) = self.store.by_session(&firing.agent_session_id).await else {
            return false;
        };
        let subject = AutomationSubject::from_agent(&agent);
        if subject.pane.as_deref() != Some(firing.pane.as_str())
            || subject.socket != firing.socket
            || subject.episode() != firing.episode
            || !subject.satisfies(muxa::automation::AutomationCondition::WaitingInput)
        {
            return false;
        }
        send_automation_text(&self.backends, firing, "\r", io_guard.clone()).await
    }

    /// The registry row behind a firing, as the live store has it now.
    async fn live_subject(
        &mut self,
        rule: &AutomationRule,
        agent_session_id: &str,
    ) -> Option<AutomationSubject> {
        let agent = self.store.by_session(agent_session_id).await?;
        Some(self.subject_for(std::slice::from_ref(rule), &agent).await)
    }

    async fn subject_for(
        &mut self,
        rules: &[AutomationRule],
        agent: &muxa::Agent,
    ) -> AutomationSubject {
        if rules.iter().any(AutomationRule::needs_pane_metadata) {
            self.refresh_panes().await;
            let agents = [agent.clone()];
            let mut subjects = subjects_from(&agents, &self.panes);
            if let Some(subject) = subjects.pop() {
                return subject;
            }
        }
        AutomationSubject::from_agent(agent)
    }

    /// Refresh the pane scan when it has gone stale. Only reached when a
    /// rule actually reads workspace/work metadata.
    async fn refresh_panes(&mut self) {
        let fresh = self.panes_read_at.is_some_and(|at| {
            at.elapsed() < std::time::Duration::from_secs(AUTOMATION_PANE_CACHE_SECONDS)
        });
        if fresh {
            return;
        }
        let backends = self.backends.clone();
        let scan = tokio::task::spawn_blocking(move || {
            backends
                .iter()
                .flat_map(|backend| backend.list_panes())
                .collect::<Vec<_>>()
        });
        self.panes = tokio::time::timeout(std::time::Duration::from_secs(3), scan)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        self.panes_read_at = Some(std::time::Instant::now());
    }
}

fn ledger_entry(
    firing: &PlannedFiring,
    at: time::OffsetDateTime,
    outcome: AutomationOutcome,
    detail: Option<String>,
) -> AutomationLedgerEntry {
    AutomationLedgerEntry {
        rule: firing.rule.clone(),
        pane: firing.pane.clone(),
        agent: firing.agent,
        fired_at: at,
        action: firing.action,
        outcome,
        detail,
        episode: Some(firing.episode.clone()),
    }
}

/// A random ratio in `[0, 1)` for the per-firing jitter. Derived from a v4
/// UUID so the engine needs no RNG dependency of its own; the scheduler
/// takes the ratio as an argument precisely so tests can pin it.
fn jitter_ratio() -> f64 {
    let bits = u64::from(uuid::Uuid::new_v4().as_fields().0);
    #[allow(clippy::cast_precision_loss)]
    {
        bits as f64 / f64::from(u32::MAX)
    }
}

/// Inject literal text into the firing's pane through the backend that owns
/// its id namespace. Mirrors `send_collaboration_text`; a backend without
/// `send_text` (zellij) simply refuses, which the caller records as a
/// failure rather than a silent success.
type WorkerIoGuard = Arc<tokio::sync::OwnedMutexGuard<()>>;

async fn send_automation_text(
    backends: &[muxa::SharedBackend],
    firing: &PlannedFiring,
    text: &str,
    io_guard: Option<WorkerIoGuard>,
) -> bool {
    let Some(kind) = muxa::backend::pane_id_host_kind(&firing.pane) else {
        return false;
    };
    let Some(backend) = backends
        .iter()
        .find(|backend| backend.kind() == kind && backend.caps().send_text)
        .cloned()
    else {
        return false;
    };
    let pane = firing.pane.clone();
    let socket = firing.socket.clone();
    let text = text.to_string();
    tokio::task::spawn_blocking(move || {
        let _held_guard = io_guard;
        if text == "\r" || text == "\u{3}" {
            backend.send_text_on(socket.as_deref(), &pane, &text)
        } else {
            backend.send_prompt_text_on(socket.as_deref(), &pane, &text)
        }
    })
    .await
    .unwrap_or(false)
}

fn spawn_collaboration_waker_task(
    cfg: &Config,
    collaboration: Arc<CollaborationStore>,
    store: muxa::SharedStore,
    backends: Vec<muxa::SharedBackend>,
    shutdown_tx: &broadcast::Sender<()>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !cfg.collaboration.enabled || !collaboration.enabled() {
        return None;
    }
    let mut shutdown_rx = shutdown_tx.subscribe();
    // Subscribe before spawning and before the initial authoritative scan, so
    // a request or state transition racing task startup is represented either
    // by that scan or by an already-pending signal.
    let mut mailbox_changes = collaboration.subscribe();
    let mut agent_transitions = store.subscribe();
    let wake_payload = cfg.collaboration.wake_payload;
    let wake_enabled = cfg.collaboration.wake != CollaborationWake::Never;
    Some(tokio::spawn(async move {
        let mut wake_inflight = HashSet::new();
        if let Err(error) = collaboration
            .reconcile_peer_interruptions(&store.snapshot().await)
            .await
        {
            tracing::warn!(%error, "initial peer interruption reconciliation failed");
        }
        if wake_enabled {
            wake_idle_collaboration_peers_with_inflight(
                &collaboration,
                &store,
                &backends,
                wake_payload,
                &mut wake_inflight,
            )
            .await;
        }
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
                            clear_wake_inflight_for_agent(&mut wake_inflight, &transition.agent);
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
                if let Err(error) = collaboration
                    .reconcile_peer_interruptions(&store.snapshot().await)
                    .await
                {
                    tracing::warn!(%error, "peer interruption reconciliation failed");
                }
                if wake_enabled {
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
    let agents = store.snapshot().await;
    let stranded = stranded_requests(collaboration, &agents).await;
    if requests.is_empty() && replies.is_empty() && stranded.is_empty() {
        return;
    }
    let listed_backends = backends.to_vec();
    let discovery_agents = agents.clone();
    let (panes, mut participants) = tokio::task::spawn_blocking(move || {
        let panes = listed_backends
            .iter()
            .flat_map(|backend| backend.list_panes())
            .collect::<Vec<_>>();
        let participants = muxa::collaboration::participants_from(&discovery_agents, &panes);
        (panes, participants)
    })
    .await
    .unwrap_or_default();
    collaboration.enrich_participants(&mut participants).await;
    collaboration
        .enrich_launch_purposes(&mut participants, &panes)
        .await;
    collaboration.enrich_worker_drains(&mut participants).await;

    // Follow vanished recipients for every undelivered request, quiet ones
    // included; a readdressed request comes back through the next pass.
    for request in &stranded {
        follow_vanished_recipient(collaboration, request, &participants, &agents, &panes).await;
    }

    for request in requests {
        let Some(recipient) =
            ready_collaboration_recipient(&participants, &agents, &panes, &request)
        else {
            continue;
        };
        let recipient = &recipient;
        if !request.from.console && !recipient.accepts_peer_work() {
            continue;
        }
        let wake_key = collaboration_wake_key(recipient);
        if wake_inflight.contains(&wake_key) {
            continue;
        }
        let quiet = collaboration.quiet_notices_waiting(recipient).await;
        let direct_wake = request.wake_delivery.is_some()
            || collaboration_wake_includes_body(wake_payload, &request);
        if direct_wake && direct_wake_body_is_terminal_safe(&request.body) {
            if deliver_full_collaboration_request(
                collaboration,
                recipient,
                &request,
                backends,
                quiet,
                wake_payload,
            )
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
        let prompt = with_quiet_notices(mailbox_collaboration_request_prompt(&request), quiet);
        let (sent, submitted) =
            send_collaboration_wake(collaboration, recipient, &prompt, backends, store).await;
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
        store,
    )
    .await;
}

/// Undelivered requests (quiet ones included) whose pinned session is no
/// longer registered at its pane. Checked against the registry first so a
/// lingering quiet notice to a present agent costs no pane scan.
async fn stranded_requests(
    collaboration: &CollaborationStore,
    agents: &[muxa::state::Agent],
) -> Vec<CollaborationRequest> {
    collaboration
        .undelivered_requests()
        .await
        .into_iter()
        .filter(|request| {
            !muxa::collaboration::is_pending_session(&request.to.agent_session_id)
                && !agents.iter().any(|agent| {
                    agent.session_id == request.to.agent_session_id
                        && agent.state != muxa::AgentState::Stopped
                        && agent.pane.as_deref() == Some(request.to.pane.as_str())
                })
        })
        .collect()
}

/// Readdress an undelivered request whose pinned recipient is gone: to the
/// same session if it moved panes, or to the slot's single live successor
/// for a slot-addressed request. The store's change signal brings the
/// request back through the waker, now aimed at a live recipient.
async fn follow_vanished_recipient(
    collaboration: &CollaborationStore,
    request: &CollaborationRequest,
    participants: &[muxa::collaboration::Participant],
    agents: &[muxa::state::Agent],
    panes: &[muxa::tmux::PaneInfo],
) {
    let Some(successor) = muxa::collaboration::successor_for(request, participants, agents, panes)
    else {
        return;
    };
    match collaboration
        .rebind_recipient(&request.id, &request.to, successor)
        .await
    {
        Ok(Some(rebound)) => tracing::info!(
            request_id = rebound.id,
            from = request.to.pane,
            to = rebound.to.pane,
            "collaboration request followed its recipient",
        ),
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(request_id = request.id, %error, "failed to readdress request");
        }
    }
}

/// Tell each sender that terminal replies are waiting. Reply bodies always
/// stay in the mailbox, so this is a notice pass with no payload policy.
///
/// `replies` is only a hint of who may be owed. The store decides what each
/// wake carries when it reserves the batch, so a reply the sender read or
/// withdrew after the scan is not announced, and replies that piled up while
/// the sender was busy arrive as one prompt rather than one per idle turn.
async fn wake_senders_of_ready_replies(
    collaboration: &CollaborationStore,
    participants: &[muxa::collaboration::Participant],
    backends: &[muxa::SharedBackend],
    replies: Vec<CollaborationRequest>,
    wake_inflight: &mut HashSet<(String, Option<String>, String)>,
    store: &muxa::Store,
) {
    let mut senders = Vec::new();
    for request in &replies {
        let Some(sender) = idle_collaboration_participant(participants, &request.from) else {
            continue;
        };
        let wake_key = collaboration_wake_key(sender);
        if !senders.contains(&wake_key) {
            senders.push(wake_key);
        }
    }
    for wake_key in senders {
        if wake_inflight.contains(&wake_key) {
            continue;
        }
        let Some(sender) = participants
            .iter()
            .find(|participant| collaboration_wake_key(participant) == wake_key)
        else {
            continue;
        };
        let batch = match collaboration.reserve_reply_wakes(sender).await {
            Ok(Some(batch)) => batch,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(pane = sender.pane, %error, "failed to reserve reply wake");
                continue;
            }
        };
        // `participants` came from a scan taken before the reservation. Type
        // nothing into a pane whose agent has since started work or been
        // replaced; its replies stay owed for its next idle turn.
        if !muxa::collaboration::still_idle_owner(&store.by_pane(&sender.pane).await, sender) {
            if let Err(error) = collaboration.complete_reply_wakes(&batch, false).await {
                tracing::warn!(pane = sender.pane, %error, "failed to release reply wake");
            }
            continue;
        }
        let prompt = reply_wake_prompt(&batch, time::OffsetDateTime::now_utc());
        let (sent, submitted) =
            send_collaboration_wake(collaboration, sender, &prompt, backends, store).await;
        if submitted {
            wake_inflight.insert(wake_key);
        }
        if let Err(error) = collaboration.complete_reply_wakes(&batch, sent).await {
            tracing::warn!(pane = sender.pane, %error, "failed to persist reply wake outcome");
        }
        if sent {
            tracing::debug!(
                request_ids = ?batch.requests.iter().map(|request| &request.id).collect::<Vec<_>>(),
                pane = sender.pane,
                submitted,
                "collaboration reply wake delivered",
            );
        }
    }
}

/// The wake text for one reserved batch. A single reply keeps the long-
/// standing one-line form. Each entry names how long ago it was requested
/// once that is long enough to matter, so a sender that moved on can tell
/// at a glance which result it is looking at.
fn reply_wake_prompt(
    batch: &muxa::collaboration::ReplyWakeBatch,
    now: time::OffsetDateTime,
) -> String {
    let entry = |request: &CollaborationRequest| {
        format!(
            "{:?} reply from {}{}{}",
            request.status,
            request.to.label(),
            requested_ago(request.created_at, now)
                .map(|ago| format!(" (requested {ago} ago)"))
                .unwrap_or_default(),
            request
                .attempt
                .as_ref()
                .map(|a| format!(
                    " [attempt {}, basis {}; recipient report, not independently verified]",
                    a.id,
                    a.basis_revision.as_deref().unwrap_or("unspecified")
                ))
                .unwrap_or_default(),
        )
    };
    if let ([request], 0) = (batch.requests.as_slice(), batch.remaining) {
        return format!(
            "[muxa:{}] {} is ready. Read it with muxa_wait_reply (MCP) or `muxa msg wait {}`.",
            request.id,
            entry(request),
            request.id,
        );
    }
    let entries = batch
        .requests
        .iter()
        .map(|request| format!("{}: {}", request.id, entry(request)))
        .collect::<Vec<_>>()
        .join("; ");
    let more = if batch.remaining == 0 {
        String::new()
    } else {
        format!(" {} more will follow.", batch.remaining)
    };
    format!(
        "[muxa] {} replies are ready: {entries}.{more} Read each with muxa_wait_reply (MCP) or `muxa msg wait ID`.",
        batch.requests.len(),
    )
}

/// Coarse age of a request, or `None` while it is recent enough that the
/// sender is plainly still waiting on it.
fn requested_ago(created_at: time::OffsetDateTime, now: time::OffsetDateTime) -> Option<String> {
    let minutes = (now - created_at).whole_minutes();
    match minutes {
        ..30 => None,
        30..120 => Some(format!("{minutes}m")),
        120..2880 => Some(format!("{}h", minutes / 60)),
        _ => Some(format!("{}d", minutes / 1440)),
    }
}

async fn deliver_full_collaboration_request(
    collaboration: &CollaborationStore,
    recipient: &muxa::collaboration::Participant,
    pending: &CollaborationRequest,
    backends: &[muxa::SharedBackend],
    quiet: usize,
    wake_payload: CollaborationWakePayload,
) -> bool {
    let io_guard = Arc::new(collaboration.worker_action_guard(&recipient.pane).await);
    if collaboration
        .worker_input_blocked(&recipient.pane, recipient.socket.as_deref())
        .await
    {
        return false;
    }
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
            let prompt = with_quiet_notices(
                if newly_prepared {
                    if wake_payload == CollaborationWakePayload::Adaptive && !request.from.console {
                        // Re-evaluate after the atomic claim: amendments may
                        // have arrived since the waker selected this request.
                        muxa::collaboration::instruction::short_inline_prompt(&request)
                            .unwrap_or_else(|| mailbox_collaboration_request_prompt(&request))
                    } else {
                        full_collaboration_request_prompt(&request)
                    }
                } else {
                    interrupted_collaboration_request_prompt(&request)
                },
                quiet,
            );
            if !send_collaboration_text(recipient, &prompt, backends, io_guard.clone()).await {
                return false;
            }
            if let Err(error) = collaboration.mark_wake_prompt_written(&request.id).await {
                tracing::warn!(
                    request_id = request.id,
                    %error,
                    "failed to persist direct wake prompt phase",
                );
            }
            tokio::time::sleep(muxa::backend::prompt_submit_grace(&recipient.pane)).await;
            submit_direct_collaboration_prompt(
                collaboration,
                recipient,
                &request.id,
                backends,
                io_guard.clone(),
            )
            .await
        }
        Some(WakeDeliveryState::PromptWritten) => {
            // The body or recovery notice is already in the input buffer.
            // Retrying only Enter avoids executing the request twice.
            if let Some(successor) = &request.superseded_by {
                let suffix=format!("\n[muxa] This request was superseded by {successor}; do not act on it. Read the mailbox for the current task.");
                if !send_collaboration_text(recipient, &suffix, backends, io_guard.clone()).await {
                    return false;
                }
            }
            submit_direct_collaboration_prompt(
                collaboration,
                recipient,
                &request.id,
                backends,
                io_guard.clone(),
            )
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
    io_guard: WorkerIoGuard,
) -> bool {
    if !send_collaboration_text(recipient, "\r", backends, io_guard.clone()).await {
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

fn collaboration_reply_instruction(request: &CollaborationRequest) -> String {
    if let (Some(contract), Some(attempt)) = (&request.task_contract, &request.attempt) {
        return format!("Carries task contract {}, attempt {}. Completion requires a typed report. Use a supporting CLI: `muxa msg reply {} BODY --report FILE`; upgrade/reload an older MCP if it omits the contract/report fields. Follow the full contract and do not infer completion from prose.",contract.task_id,attempt.id,request.id);
    }
    if request.expects_reply {
        format!(
            "When finished, respond with muxa_reply for request {} or `muxa msg reply {0} <body>`.",
            request.id
        )
    } else {
        "No reply is expected; do not send an acknowledgement solely for this delivery.".into()
    }
}

fn mailbox_collaboration_request_prompt(request: &CollaborationRequest) -> String {
    let represented = request.from.label();
    let source = if request.provenance.as_ref().is_some_and(|p| {
        matches!(
            p.client_kind,
            CollaborationClientKind::Watch | CollaborationClientKind::Dashboard
        )
    }) {
        collaboration_request_source(request)
    } else {
        format!("from {represented}")
    };
    let mismatch = request
        .provenance
        .as_ref()
        .map_or(String::new(), |p| match p.origin_match {
            CollaborationOriginMatch::Matched => String::new(),
            other => format!(" (origin {other})"),
        });
    let reply = if request.task_contract.is_some() {
        collaboration_reply_instruction(request)
    } else if request.expects_reply {
        format!(
            "Reply: `muxa msg reply {} BODY` (or muxa_reply).",
            request.id
        )
    } else {
        "No reply is expected.".into()
    };
    format!("[muxa:{}] {:?} {source}{mismatch}. Read one: muxa_inbox(request_id=\"{}\") or `muxa msg accept {} --json`. Honor kind/work_mode/paths. {reply}",
        request.id,request.kind,request.id,request.id)
}

fn full_collaboration_request_prompt(request: &CollaborationRequest) -> String {
    let paths = serde_json::to_string(&request.paths).unwrap_or_else(|_| "[]".into());
    let mut air_artifacts = if request.air_artifacts.is_empty() {
        String::new()
    } else {
        format!(
            "\nair_artifacts: {}",
            serde_json::to_string(&request.air_artifacts).unwrap_or_else(|_| "[]".into())
        )
    };
    if let Some(contract) = &request.task_contract {
        use std::fmt::Write as _;
        let _=write!(air_artifacts,"\ntask_contract: {}\nattempt: {}\nFor completed work, provide a typed report with the attempt id, basis_revision, every criterion and evidence. These remain recipient-reported claims.",serde_json::to_string(contract).unwrap_or_default(),serde_json::to_string(&request.attempt).unwrap_or_default());
    }
    let reply = collaboration_reply_instruction(request);
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

/// Mention unread one-way notices in a wake that is going out anyway: the
/// recipient hears about them without a terminal write of their own.
fn with_quiet_notices(mut prompt: String, quiet: usize) -> String {
    if quiet > 0 {
        let _ = std::fmt::Write::write_fmt(
            &mut prompt,
            format_args!(
            "\n[muxa] Also waiting: {quiet} one-way notice{} (no reply expected); read with muxa_inbox or `muxa msg inbox`.",
            if quiet == 1 { "" } else { "s" },
            ),
        );
    }
    prompt
}

fn interrupted_collaboration_request_prompt(request: &CollaborationRequest) -> String {
    format!(
        "[muxa:{}] Direct {} request delivery was interrupted after it was claimed. Read only this request with muxa_inbox(request_id=\"{}\") (MCP) or `muxa msg accept {} --json`; honor kind/work_mode/paths. {}",
        request.id,
        collaboration_request_kind(request.kind),
        request.id,
        request.id,
        collaboration_reply_instruction(request),
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
    muxa::collaboration::instruction::terminal_safe(body)
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
        CollaborationWakePayload::Adaptive => {
            request.from.console
                || muxa::collaboration::instruction::short_inline_prompt(request).is_some()
        }
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
    idle_pending_recipient(participants, &request.to)
        .cloned()
        .or_else(|| {
            let mut ready =
                muxa::collaboration::pending_recipient_ready(&request.to, agents, panes)?;
            if let Some(enriched) = participants
                .iter()
                .find(|p| p.pane == ready.pane && p.socket == ready.socket && p.room == ready.room)
            {
                ready.launch_purpose = enriched.launch_purpose;
            }
            Some(ready)
        })
}

/// The idle agent that may adopt a request still addressed to a launching
/// pane. Uses the same predicate delivery reserves with: matching on pane and
/// socket alone picked whatever agent later reused that pane id in another
/// window, and the refused reservation then repeated on every tick.
fn idle_pending_recipient<'a>(
    participants: &'a [muxa::collaboration::Participant],
    to: &muxa::collaboration::Participant,
) -> Option<&'a muxa::collaboration::Participant> {
    participants.iter().find(|participant| {
        participant.state == muxa::AgentState::Idle
            && muxa::collaboration::addresses(to, participant)
    })
}

fn idle_collaboration_participant<'a>(
    participants: &'a [muxa::collaboration::Participant],
    target: &muxa::collaboration::Participant,
) -> Option<&'a muxa::collaboration::Participant> {
    participants.iter().find(|participant| {
        participant.same_session(target)
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

/// Drop the wake debounce for everything addressed at this agent's pane.
///
/// Keyed by endpoint rather than session id on purpose: a request queued
/// against a pane that had not registered yet carries a `pending-pane:`
/// placeholder, which no registry row will ever name. Matching the row's own
/// session id would leave that entry suppressing further wakes to the pane
/// until the slow reconcile tick cleared the whole set.
fn clear_wake_inflight_for_agent(
    wake_inflight: &mut HashSet<(String, Option<String>, String)>,
    agent: &muxa::Agent,
) {
    let Some(pane) = agent.pane.as_deref() else {
        return;
    };
    wake_inflight
        .retain(|(key_pane, key_socket, _)| key_pane != pane || *key_socket != agent.tmux_socket);
}

async fn send_collaboration_wake(
    collaboration: &CollaborationStore,
    participant: &muxa::collaboration::Participant,
    prompt: &str,
    backends: &[muxa::SharedBackend],
    store: &muxa::Store,
) -> (bool, bool) {
    let io_guard = Arc::new(collaboration.worker_action_guard(&participant.pane).await);
    if collaboration
        .worker_input_blocked(&participant.pane, participant.socket.as_deref())
        .await
    {
        return (false, false);
    }
    let agents = store.by_pane(&participant.pane).await;
    let observer = muxa::prompt_delivery::SubmissionObserver::new(
        store,
        &agents,
        participant.socket.as_deref(),
        prompt,
    );
    let sent = send_collaboration_text(participant, prompt, backends, io_guard.clone()).await;
    let submitted = if sent {
        tokio::time::sleep(muxa::backend::prompt_submit_grace(&participant.pane)).await;
        send_collaboration_text(participant, "\r", backends, io_guard.clone()).await
    } else {
        false
    };
    if submitted {
        // A missing hook must not delay mailbox bookkeeping, the next peer's
        // wake, or shutdown. This bounded observer only logs evidence and
        // never sends keys or retries the body.
        let pane = participant.pane.clone();
        tokio::spawn(async move {
            let confirmed = observer.confirm().await;
            tracing::debug!(pane, submission_confirmed = ?confirmed,
                "collaboration Enter delivered; application acknowledgement observed separately");
        });
    }
    (sent, submitted)
}

async fn send_collaboration_text(
    participant: &muxa::collaboration::Participant,
    text: &str,
    backends: &[muxa::SharedBackend],
    io_guard: WorkerIoGuard,
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
    tokio::task::spawn_blocking(move || {
        let _held_guard = io_guard;
        if text == "\r" || text == "\u{3}" {
            backend.send_text_on(socket.as_deref(), &pane, &text)
        } else {
            backend.send_prompt_text_on(socket.as_deref(), &pane, &text)
        }
    })
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
        #[cfg(unix)]
        {
            let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
            let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
            tokio::select! {
                _ = term.recv() => tracing::info!("SIGTERM received"),
                _ = int.recv()  => tracing::info!("SIGINT received"),
            }
        }
        // Windows has no SIGTERM. Ctrl+C, closing the console, and the
        // shutdown a service stop or logoff sends all arrive as console
        // control events.
        #[cfg(windows)]
        {
            use tokio::signal::windows;
            let mut ctrl_c = windows::ctrl_c().expect("install Ctrl+C handler");
            let mut close = windows::ctrl_close().expect("install console-close handler");
            let mut shutdown = windows::ctrl_shutdown().expect("install shutdown handler");
            // A process group cannot be sent Ctrl+C, only Ctrl+Break — the
            // one console stop a supervisor can aim at muxad alone.
            let mut ctrl_break = windows::ctrl_break().expect("install Ctrl+Break handler");
            tokio::select! {
                _ = ctrl_c.recv() => tracing::info!("Ctrl+C received"),
                _ = ctrl_break.recv() => tracing::info!("Ctrl+Break received"),
                _ = close.recv() => tracing::info!("console close received"),
                _ = shutdown.recv() => tracing::info!("system shutdown received"),
            }
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
        // This name is persisted into the activity log and history; a
        // grouped session must be recorded as itself, not as a `~view~`.
        entries.extend(
            muxa::tmux::canonical_panes(&panes)
                .into_iter()
                .map(|pane| (pane.pane_id, pane.session)),
        );
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
    sessions: muxa::session::SharedSessionBackend,
    agent_directory: Arc<muxa::agent_directory::AgentDirectory>,
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
    .with_native_sessions(sessions)
    .with_stuck_working_timeout(std::time::Duration::from_secs(
        cfg.reconciler.stuck_working_timeout_secs,
    ))
    .with_stuck_waiting_timeout(std::time::Duration::from_secs(
        cfg.reconciler.stuck_waiting_timeout_secs,
    ))
    .with_paneless_stale_timeout(std::time::Duration::from_secs(
        cfg.reconciler.paneless_stale_timeout_secs,
    ))
    .with_paneless_max_live(cfg.reconciler.paneless_max_live_rows)
    .with_codex_sessions_root(codex_sessions_root.clone())
    .with_agent_directory(agent_directory);
    let shutdown_rx = shutdown_tx.subscribe();
    let handle = tokio::spawn(runner.run(shutdown_rx));
    tracing::info!(
        interval_secs = cfg.reconciler.interval_secs,
        stuck_working_timeout_secs = cfg.reconciler.stuck_working_timeout_secs,
        stuck_waiting_timeout_secs = cfg.reconciler.stuck_waiting_timeout_secs,
        paneless_stale_timeout_secs = cfg.reconciler.paneless_stale_timeout_secs,
        paneless_max_live_rows = cfg.reconciler.paneless_max_live_rows,
        codex_rollout_polling = codex_sessions_root.is_some(),
        "reconciler enabled",
    );
    Some(handle)
}

/// Re-exec onto the installed binary once an upgrade replaces it.
///
/// Without this, installing a new muxa leaves the old daemon serving: the
/// package manager writes the new build, the running process keeps its open
/// inode, and no service manager intervenes because nothing exited. The gap
/// closes only when someone notices and restarts it by hand — which, measured
/// on a live host, took six days.
///
/// The daemon re-execs itself rather than exiting for a supervisor to catch,
/// so this works the same whether muxad was started by launchd, systemd, or a
/// terminal, and a failed exec leaves the old image running instead of a hole
/// where the daemon was.
fn spawn_binary_watch_task(
    cfg: &Config,
    shutdown_tx: &broadcast::Sender<()>,
    restart: Arc<RestartController>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !cfg.daemon.restart_on_new_binary {
        return None;
    }
    let Some(path) = binary_watch::reexec_target() else {
        tracing::warn!("cannot resolve the binary to watch; upgrades will not restart the daemon");
        return None;
    };
    #[cfg(target_os = "linux")]
    let running = binary_watch::BinaryIdentity::read(std::path::Path::new("/proc/self/exe"));
    #[cfg(not(target_os = "linux"))]
    let running = std::env::current_exe()
        .ok()
        .and_then(|image| binary_watch::BinaryIdentity::read(&image));
    let Some(baseline) = running else {
        // Watching from no baseline would make the first successful read look
        // like an upgrade and restart a daemon that nothing had replaced.
        tracing::warn!(
            ?path,
            "cannot read the installed binary; upgrades will not restart the daemon"
        );
        return None;
    };
    let interval = std::time::Duration::from_secs(cfg.daemon.binary_poll_secs.max(1));
    let mut shutdown_rx = shutdown_tx.subscribe();
    tracing::info!(
        ?path,
        poll_secs = cfg.daemon.binary_poll_secs,
        "binary watch enabled"
    );
    Some(tokio::spawn(async move {
        let mut watch = binary_watch::BinaryWatch::new(baseline);
        let mut blocked = None;
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick of a tokio interval completes immediately, and the
        // baseline was read moments ago — burn it rather than re-stat.
        tick.tick().await;
        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => return,
                _ = tick.tick() => {}
            }
            let path = path.clone();
            // `stat` on a cold path can block on the filesystem, and this
            // runtime also serves IPC.
            let observed =
                tokio::task::spawn_blocking(move || binary_watch::BinaryIdentity::read(&path))
                    .await
                    .unwrap_or(None);
            if watch.observe(observed) || blocked.is_some() {
                let stamp = restart.upgrade_stamp().ok().flatten();
                if stamp.is_some() && stamp == blocked {
                    continue;
                }
                blocked = stamp;
                match restart.request_checked_restart().await {
                    Ok(true) => {
                        tracing::info!(
                            "installed binary changed; restarting onto validated target"
                        );
                        return;
                    }
                    Ok(false) => return,
                    Err(reason) => {
                        tracing::warn!(%reason,"upgrade blocked; daemon stays available");
                    }
                }
            }
        }
    }))
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
    // tmux and rmux (`list-clients`) and herdr (focused workspace over its socket).
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
            muxa::HostKind::Rmux => sources.push(muxa::SessionActivitySource::Rmux),
            muxa::HostKind::Herdr => sources.push(muxa::SessionActivitySource::Herdr {
                socket_path: muxa::backend::herdr::default_socket_path(),
            }),
            muxa::HostKind::Cmux | muxa::HostKind::Zellij => {}
        }
    }
    let source_kinds: Vec<muxa::HostKind> = backends
        .iter()
        .map(|b| b.kind())
        .filter(|kind| {
            matches!(
                kind,
                muxa::HostKind::Tmux | muxa::HostKind::Rmux | muxa::HostKind::Herdr
            )
        })
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
        activity_interval: muxa::snapshot::DEFAULT_ACTIVITY_INTERVAL,
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
    // One candidate per pane, not one per `~view~` listing of it.
    let panes = muxa::tmux::canonical_panes(&panes);
    if panes.is_empty() {
        return;
    }

    // Snapshot the current registry to decide which panes need
    // enrichment. A pane is "covered" iff a *real* (non-synthetic) live
    // agent already represents it — synthetic placeholders are the
    // exact thing this pass is trying to upgrade away from.
    let snapshot = store.snapshot().await;

    // A Codex prompt's recorded pane is not evidence: behind the shared
    // app-server it was whichever pane started the server, so seeding by it
    // resurrects a closed thread onto a stranger's pane. Seed a Codex entry
    // only where a live `codex resume <id>` proves it; the thread's next hook
    // re-registers it otherwise.
    let codex_resumes: HashSet<(String, String)> = {
        let scanned = panes.clone();
        tokio::task::spawn_blocking(move || muxa::process_tree::scan_codex_panes(&scanned))
            .await
            .map(|scan| {
                scan.resume_bindings
                    .into_iter()
                    .map(|binding| (binding.session_id, binding.pane.pane_id))
                    .collect()
            })
            .unwrap_or_default()
    };

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
        if entry.kind == muxa::AgentKind::Codex
            && !codex_resumes.contains(&(entry.session_id.clone(), pane.pane_id.clone()))
        {
            continue;
        }
        candidates.push(muxa::Agent {
            ask_origin: None,
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
            last_prompt_at: Some(entry.at),
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

    fn room_participant(
        pane: &str,
        session: &str,
        window: &str,
    ) -> muxa::collaboration::Participant {
        muxa::collaboration::Participant {
            launch_purpose: muxa::collaboration::LaunchPurpose::Primary,
            draining: false,
            agent_kind: AgentKind::Codex,
            agent_session_id: session.into(),
            pane: pane.into(),
            socket: Some("/tmp/rmux-1/default".into()),
            room: muxa::collaboration::RoomId {
                host: "rmux".into(),
                socket: Some("/tmp/rmux-1/default".into()),
                window_id: window.into(),
            },
            tmux_session_id: None,
            tmux_session_name: None,
            window_name: None,
            state: muxa::AgentState::Idle,
            cwd: None,
            alias: None,
            roles: Vec::new(),
            console: false,
        }
    }

    /// A request still waiting on a launching agent must not be offered to
    /// whatever agent later reuses that pane id in another window: delivery
    /// refuses it, so picking it only repeats a failing reservation forever.
    #[test]
    fn a_pending_request_is_offered_only_to_its_own_room() {
        let to = room_participant(
            "rmux:%75",
            "pending-pane:/tmp/rmux-1/default:rmux:%75",
            "@42",
        );
        let elsewhere = room_participant("rmux:%75", "thread-other", "@13");
        assert!(idle_pending_recipient(std::slice::from_ref(&elsewhere), &to).is_none());

        let launched = room_participant("rmux:%75", "thread-launched", "@42");
        let found = idle_pending_recipient(&[elsewhere, launched.clone()], &to).cloned();
        assert_eq!(found, Some(launched));
    }

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

    /// Records every attempt and accepts input only while `accepting` is set,
    /// like a pane whose application is not taking keystrokes.
    struct RefusingWakeBackend {
        panes: Vec<PaneInfo>,
        attempts: Arc<Mutex<Vec<(String, String)>>>,
        accepting: Arc<std::sync::atomic::AtomicBool>,
    }

    impl PaneBackend for RefusingWakeBackend {
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
            self.attempts
                .lock()
                .unwrap()
                .push((pane_id.into(), text.into()));
            self.accepting.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn caps(&self) -> BackendCaps {
            BackendCaps::default()
        }
    }

    fn collaboration_pane(pane_id: &str, pane_index: &str) -> PaneInfo {
        PaneInfo {
            session_group: None,
            agent_program: None,
            worker_launch_id: None,
            agent_role: None,
            agent_alias: None,
            workspace_id: None,
            work_id: None,
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

    fn worker_participants(
        agents: &[muxa::Agent],
        panes: &[PaneInfo],
    ) -> Vec<muxa::collaboration::Participant> {
        let mut participants = muxa::collaboration::participants_from(agents, panes);
        for participant in &mut participants {
            participant.launch_purpose = muxa::collaboration::LaunchPurpose::Worker;
        }
        participants
    }

    async fn authorize_test_workers(
        mailbox: &CollaborationStore,
        participants: &[muxa::collaboration::Participant],
    ) {
        for participant in participants
            .iter()
            .filter(|p| !muxa::collaboration::is_pending_session(&p.agent_session_id))
        {
            mailbox
                .set_purpose(participant, muxa::collaboration::LaunchPurpose::Worker)
                .await
                .unwrap();
        }
    }

    /// The junia case: a coordinator's review request to a window's reviewer
    /// whose pane was replaced before it was delivered. Addressed by role,
    /// it follows to the new reviewer in the same work; nothing is sent to
    /// the vanished pane.
    #[tokio::test]
    async fn a_role_addressed_request_follows_a_replaced_reviewer() {
        let store = muxa::Store::shared();
        add_agent(&store, "%1", "coordinator", AgentKind::ClaudeCode).await;
        add_agent(&store, "%2", "old-reviewer", AgentKind::Codex).await;
        let stamp = |mut pane: PaneInfo| {
            pane.workspace_id = Some("junia".into());
            pane.work_id = Some("S1".into());
            pane.agent_role = Some("reviewer".into());
            pane
        };
        let coordinator_pane = collaboration_pane("%1", "0");
        let old_pane = stamp(collaboration_pane("%2", "1"));
        let panes = vec![coordinator_pane.clone(), old_pane];
        let participants = muxa::collaboration::participants_from(&store.snapshot().await, &panes);
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        authorize_test_workers(&mailbox, &participants).await;
        let coordinator = participants
            .iter()
            .find(|p| p.pane == "%1")
            .unwrap()
            .clone();
        let old = participants
            .iter()
            .find(|p| p.pane == "%2")
            .unwrap()
            .clone();
        let request = mailbox
            .create(
                coordinator.clone(),
                old.clone(),
                NewRequest {
                    body: "review S1".into(),
                    recipient_slot: Some(muxa::collaboration::RecipientSlot::of(
                        &old,
                        &panes,
                        Some("reviewer"),
                        None,
                    )),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();

        // A quiet notice to the same slot wakes nobody but must follow too.
        let notice = mailbox
            .create(
                coordinator.clone(),
                old.clone(),
                NewRequest {
                    kind: RequestKind::Notice,
                    body: "fyi".into(),
                    expects_reply: false,
                    notify: Some(false),
                    recipient_slot: Some(muxa::collaboration::RecipientSlot::of(
                        &old,
                        &panes,
                        Some("reviewer"),
                        None,
                    )),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();

        // The reviewer pane goes away and a new reviewer session starts in a
        // new pane of the same work window.
        store.forget_agent("old-reviewer").await;
        add_agent(&store, "%3", "new-reviewer", AgentKind::Codex).await;
        let new_pane = stamp(collaboration_pane("%3", "1"));
        let panes = vec![coordinator_pane, new_pane];
        let fresh = muxa::collaboration::participants_from(&store.snapshot().await, &panes);
        authorize_test_workers(&mailbox, &fresh).await;

        let sends = Arc::new(Mutex::new(Vec::new()));
        let backend: muxa::SharedBackend = Arc::new(CollaborationWakeBackend {
            panes,
            sends: sends.clone(),
        });
        for _ in 0..2 {
            wake_idle_collaboration_peers(&mailbox, &store, std::slice::from_ref(&backend)).await;
        }
        {
            let sends = sends.lock().unwrap();
            assert!(sends.iter().all(|(pane, _)| pane != "%2"), "{sends:?}");
            assert!(
                sends
                    .iter()
                    .any(|(pane, text)| pane == "%3" && text.contains(&request.id)),
                "the new reviewer was not woken: {sends:?}"
            );
        }
        let stored = mailbox.get_for(&coordinator, &request.id).await.unwrap();
        assert_eq!(stored.to.agent_session_id, "new-reviewer");
        assert_eq!(stored.rebound_from, ["old-reviewer"]);
        let notice = mailbox.get_for(&coordinator, &notice.id).await.unwrap();
        assert_eq!(
            notice.to.agent_session_id, "new-reviewer",
            "quiet notice stranded"
        );
    }

    async fn deliver_quiet_progress_burst(
        mailbox: &CollaborationStore,
        store: &muxa::SharedStore,
        backends: &[muxa::SharedBackend],
        payload: CollaborationWakePayload,
        sender: &muxa::collaboration::Participant,
        recipient: &muxa::collaboration::Participant,
    ) -> Vec<String> {
        let mut inflight = HashSet::new();
        let mut progress_ids = Vec::new();
        // Reproduce the reported volume and let the waker scan after each
        // message, including fresh idle generations and periodic rescans.
        for index in 0..39 {
            let progress = mailbox
                .create(
                    sender.clone(),
                    recipient.clone(),
                    NewRequest {
                        kind: RequestKind::Notice,
                        body: format!("checkpoint {index}"),
                        expects_reply: false,
                        ..NewRequest::default()
                    },
                )
                .await
                .unwrap();
            progress_ids.push(progress.id);
            inflight.clear();
            for _ in 0..2 {
                wake_idle_collaboration_peers_with_inflight(
                    mailbox,
                    store,
                    backends,
                    payload,
                    &mut inflight,
                )
                .await;
            }
        }
        progress_ids
    }

    #[tokio::test]
    async fn collaboration_waker_suppresses_progress_but_delivers_handoffs_without_ack() {
        for payload in [
            CollaborationWakePayload::Notice,
            CollaborationWakePayload::OperatorFull,
            CollaborationWakePayload::Adaptive,
            CollaborationWakePayload::Full,
        ] {
            let store = muxa::Store::shared();
            add_agent(&store, "%1", "sender", AgentKind::Codex).await;
            add_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
            let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
            let participants = worker_participants(&store.snapshot().await, &panes);
            let sender = participants
                .iter()
                .find(|p| p.pane == "%1")
                .unwrap()
                .clone();
            let recipient = participants
                .iter()
                .find(|p| p.pane == "%2")
                .unwrap()
                .clone();
            let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
            authorize_test_workers(&mailbox, &participants).await;
            let sends = Arc::new(Mutex::new(Vec::new()));
            let backend: muxa::SharedBackend = Arc::new(CollaborationWakeBackend {
                panes,
                sends: sends.clone(),
            });
            let backends = vec![backend];
            let mut inflight = HashSet::new();
            let progress_ids = deliver_quiet_progress_burst(
                &mailbox, &store, &backends, payload, &sender, &recipient,
            )
            .await;
            assert!(
                sends.lock().unwrap().is_empty(),
                "39 quiet notices must cause zero terminal writes"
            );
            let handoff = mailbox
                .create(
                    sender,
                    recipient.clone(),
                    NewRequest {
                        kind: RequestKind::Notice,
                        body: "device released".into(),
                        expects_reply: false,
                        notify: Some(true),
                        ..NewRequest::default()
                    },
                )
                .await
                .unwrap();
            for prompt in [
                mailbox_collaboration_request_prompt(&handoff),
                full_collaboration_request_prompt(&handoff),
                interrupted_collaboration_request_prompt(&handoff),
            ] {
                assert!(prompt.contains("No reply is expected"));
                assert!(!prompt.contains("respond with muxa_reply"));
            }
            for _ in 0..2 {
                wake_idle_collaboration_peers_with_inflight(
                    &mailbox,
                    &store,
                    &backends,
                    payload,
                    &mut inflight,
                )
                .await;
            }
            {
                let delivered = sends.lock().unwrap();
                assert_eq!(
                    delivered.len(),
                    2,
                    "one prompt and one submit, no duplicate wake"
                );
                assert!(delivered[0].1.contains(&handoff.id));
                assert!(
                    delivered[0].1.contains("Also waiting: 39 one-way notices"),
                    "a wake that goes out anyway names the quiet notices: {}",
                    delivered[0].1
                );
                assert!(progress_ids.iter().all(|id| !delivered[0].1.contains(id)));
                assert!(delivered[0].1.contains("No reply is expected"));
            }
            let inbox = mailbox.claim_for(&recipient).await.unwrap();
            assert_eq!(
                inbox
                    .iter()
                    .filter(|request| progress_ids.contains(&request.id))
                    .count(),
                39,
                "all quiet notices must remain readable at the checkpoint"
            );
            assert_eq!(
                mailbox.pending_reply_unnotified().await,
                [] as [muxa::collaboration::CollaborationRequest; 0]
            );
        }
    }

    #[tokio::test]
    async fn peer_recovery_runs_when_terminal_wake_is_disabled() {
        let store = muxa::Store::shared();
        add_capped_agent(&store, "%1", "sender").await;
        add_capped_agent(&store, "%2", "recipient").await;
        let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
        let participants = worker_participants(&store.snapshot().await, &panes);
        let sender = participants
            .iter()
            .find(|p| p.pane == "%1")
            .unwrap()
            .clone();
        let recipient = participants
            .iter()
            .find(|p| p.pane == "%2")
            .unwrap()
            .clone();
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        authorize_test_workers(&mailbox, &participants).await;
        mailbox
            .create(
                sender,
                recipient,
                NewRequest {
                    body: "review".into(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        let mut cfg = Config::default();
        cfg.collaboration.enabled = true;
        cfg.collaboration.wake = CollaborationWake::Never;
        let (shutdown, _) = broadcast::channel(1);
        let worker =
            spawn_collaboration_waker_task(&cfg, mailbox.clone(), store, vec![], &shutdown)
                .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if mailbox.pending_human_notifications().await.len() == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        shutdown.send(()).unwrap();
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn collaboration_waker_reacts_to_mailbox_revision() {
        let store = muxa::Store::shared();
        add_agent(&store, "%1", "sender", AgentKind::Codex).await;
        add_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
        let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
        let participants = worker_participants(&store.snapshot().await, &panes);
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
        authorize_test_workers(&mailbox, &participants).await;
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
                    task_contract: None,
                    supersedes_request_id: None,
                    dispatch_id: None,
                    source_node_id: None,
                    delegation_parent: None,
                    human_action: None,
                    initiator: None,
                    kind: RequestKind::Question,
                    body: "wake from revision".into(),
                    expects_reply: true,
                    notify: None,
                    work_mode: WorkMode::ReadOnly,
                    thread_id: None,
                    parent_request_id: None,
                    workspace_id: None,
                    work_id: None,
                    run_id: None,
                    paths: Vec::new(),
                    artifacts: Vec::new(),
                    links: Vec::new(),
                    air_artifacts: Vec::new(),
                    withdraw_reply_wakes: Vec::new(),
                    recipient_slot: None,
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

    /// A pending recipient's debounce is keyed by the placeholder session id,
    /// which no registry row will ever name. Clearing has to be endpoint-based
    /// or a second request to the same pane stays suppressed until the slow
    /// reconcile tick.
    #[tokio::test]
    async fn wake_debounce_for_a_pending_pane_clears_on_that_pane_s_transition() {
        let store = muxa::Store::shared();
        add_agent(&store, "%2", "synthetic-7:default:%2", AgentKind::Codex).await;
        let row = store
            .snapshot()
            .await
            .into_iter()
            .find(|agent| agent.pane.as_deref() == Some("%2"))
            .expect("the pane has a row");

        let mut wake_inflight = HashSet::new();
        wake_inflight.insert((
            "%2".to_string(),
            Some("default".to_string()),
            "pending-pane:default:%2".to_string(),
        ));
        wake_inflight.insert((
            "%3".to_string(),
            Some("default".to_string()),
            "other-session".to_string(),
        ));

        clear_wake_inflight_for_agent(&mut wake_inflight, &row);

        assert!(
            !wake_inflight.iter().any(|(pane, _, _)| pane == "%2"),
            "the pending key for this pane must clear",
        );
        assert_eq!(wake_inflight.len(), 1, "other panes are untouched");
    }

    #[tokio::test]
    async fn primary_receives_no_peer_prompt_in_any_payload_mode() {
        for payload in [
            CollaborationWakePayload::Notice,
            CollaborationWakePayload::OperatorFull,
            CollaborationWakePayload::Adaptive,
            CollaborationWakePayload::Full,
        ] {
            let store = muxa::Store::shared();
            add_agent(&store, "%1", "sender", AgentKind::Codex).await;
            add_agent(&store, "%2", "human", AgentKind::ClaudeCode).await;
            let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
            let participants =
                muxa::collaboration::participants_from(&store.snapshot().await, &panes);
            let sender = participants
                .iter()
                .find(|p| p.pane == "%1")
                .unwrap()
                .clone();
            let recipient = participants
                .iter()
                .find(|p| p.pane == "%2")
                .unwrap()
                .clone();
            let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
            let request = mailbox
                .create(
                    sender,
                    recipient.clone(),
                    NewRequest {
                        body: "peer must not interrupt human".into(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let sends = Arc::new(Mutex::new(Vec::new()));
            let backend: muxa::SharedBackend = Arc::new(CollaborationWakeBackend {
                panes,
                sends: sends.clone(),
            });
            wake_idle_collaboration_peers_with_inflight(
                &mailbox,
                &store,
                &[backend],
                payload,
                &mut HashSet::new(),
            )
            .await;
            assert!(sends.lock().unwrap().is_empty());
            assert_eq!(
                mailbox
                    .peek_for(&recipient, &request.id)
                    .await
                    .unwrap()
                    .status,
                muxa::collaboration::RequestStatus::Queued
            );
        }
    }

    /// The spawn deadlock, end to end: a pane muxa launched carries only a
    /// synthetic row — no session has registered, so it is not a participant —
    /// yet the request queued against it is delivered as soon as the pane
    /// reads idle. Without this the sender would wait for a registration that
    /// codex cannot produce until something types at it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[allow(clippy::too_many_lines)] // end-to-end wake setup is intentionally kept in one scenario
    async fn collaboration_waker_delivers_to_a_launched_pane_before_it_registers() {
        struct Worker(std::process::Child);
        impl Drop for Worker {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let worker = Worker(
            std::process::Command::new("bash")
                .args(["-c", "exec -a 'codex --no-daemon' sleep 30"])
                .spawn()
                .unwrap(),
        );
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
        launched.agent_program = Some("codex".into());
        launched.pane_pid = worker.0.id();
        let panes = vec![collaboration_pane("%1", "0"), launched];
        let agents = store.snapshot().await;
        let participants = worker_participants(&agents, &panes);
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
        authorize_test_workers(&mailbox, &participants).await;
        mailbox
            .register_worker_launch(
                uuid::Uuid::new_v4().to_string(),
                panes[1].clone(),
                AgentKind::Codex,
            )
            .await
            .unwrap();
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
                    task_contract: None,
                    supersedes_request_id: None,
                    dispatch_id: None,
                    source_node_id: None,
                    delegation_parent: None,
                    human_action: None,
                    initiator: None,
                    kind: RequestKind::Review,
                    body: "review the pending diff".into(),
                    expects_reply: true,
                    notify: None,
                    work_mode: WorkMode::ReadOnly,
                    thread_id: None,
                    parent_request_id: None,
                    workspace_id: None,
                    work_id: None,
                    run_id: None,
                    paths: Vec::new(),
                    artifacts: Vec::new(),
                    links: Vec::new(),
                    air_artifacts: Vec::new(),
                    withdraw_reply_wakes: Vec::new(),
                    recipient_slot: None,
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
        mailbox
            .observe_worker_hook(&AgentId {
                kind: AgentKind::Codex,
                session_id: "codex-session".into(),
                surface: None,
                pane: Some("%2".into()),
                tmux_socket: Some("default".into()),
                cwd: None,
            })
            .await
            .unwrap();
        let mut registered = vec![registered];
        mailbox.enrich_launch_purposes(&mut registered, &[]).await;
        let inbox = mailbox.claim_for(&registered[0]).await.unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].id, request.id);

        shutdown_tx.send(()).unwrap();
        waker.await.unwrap();
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // end-to-end wake setup is intentionally kept in one scenario
    async fn full_wake_claims_and_injects_the_structured_request_body() {
        let store = muxa::Store::shared();
        add_agent(&store, "%1", "sender", AgentKind::Codex).await;
        add_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
        let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
        let participants = worker_participants(&store.snapshot().await, &panes);
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
        authorize_test_workers(&mailbox, &participants).await;
        let request = mailbox
            .create(
                sender.clone(),
                recipient.clone(),
                NewRequest {
                    task_contract: None,
                    supersedes_request_id: None,
                    dispatch_id: None,
                    source_node_id: None,
                    delegation_parent: None,
                    human_action: None,
                    initiator: None,
                    kind: RequestKind::Task,
                    body: "change only the authorized file".into(),
                    expects_reply: true,
                    notify: None,
                    work_mode: WorkMode::Execute,
                    thread_id: None,
                    parent_request_id: None,
                    workspace_id: None,
                    work_id: None,
                    run_id: None,
                    paths: vec!["src/auth.rs".into()],
                    artifacts: Vec::new(),
                    links: Vec::new(),
                    air_artifacts: Vec::new(),
                    withdraw_reply_wakes: Vec::new(),
                    recipient_slot: None,
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
        assert_eq!(
            mailbox.pending_unnotified().await,
            [] as [muxa::collaboration::CollaborationRequest; 0]
        );

        sends.lock().unwrap().clear();
        let unsafe_request = mailbox
            .create(
                sender,
                recipient.clone(),
                NewRequest {
                    task_contract: None,
                    supersedes_request_id: None,
                    dispatch_id: None,
                    source_node_id: None,
                    delegation_parent: None,
                    human_action: None,
                    initiator: None,
                    kind: RequestKind::Task,
                    body: "unsafe\u{1b}[201~\rsubmit".into(),
                    expects_reply: true,
                    notify: None,
                    work_mode: WorkMode::Execute,
                    thread_id: None,
                    parent_request_id: None,
                    workspace_id: None,
                    work_id: None,
                    run_id: None,
                    paths: Vec::new(),
                    artifacts: Vec::new(),
                    links: Vec::new(),
                    air_artifacts: Vec::new(),
                    withdraw_reply_wakes: Vec::new(),
                    recipient_slot: None,
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
        let participants = worker_participants(&store.snapshot().await, &panes);
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
        authorize_test_workers(&mailbox, &participants).await;
        let operator_request = mailbox
            .create_with_provenance(
                console,
                recipient.clone(),
                NewRequest {
                    task_contract: None,
                    supersedes_request_id: None,
                    dispatch_id: None,
                    source_node_id: None,
                    delegation_parent: None,
                    human_action: None,
                    initiator: None,
                    kind: RequestKind::Task,
                    body: "operator request body".into(),
                    expects_reply: true,
                    notify: None,
                    work_mode: WorkMode::Execute,
                    thread_id: None,
                    parent_request_id: None,
                    workspace_id: None,
                    work_id: None,
                    run_id: None,
                    paths: Vec::new(),
                    artifacts: Vec::new(),
                    links: Vec::new(),
                    air_artifacts: Vec::new(),
                    withdraw_reply_wakes: Vec::new(),
                    recipient_slot: None,
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
        let participants = worker_participants(&store.snapshot().await, &panes);
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
        authorize_test_workers(&mailbox, &participants).await;
        let agent_request = mailbox
            .create(
                sender,
                recipient.clone(),
                NewRequest {
                    task_contract: None,
                    supersedes_request_id: None,
                    dispatch_id: None,
                    source_node_id: None,
                    delegation_parent: None,
                    human_action: None,
                    initiator: None,
                    kind: RequestKind::Task,
                    body: "agent delegated body".into(),
                    expects_reply: true,
                    notify: None,
                    work_mode: WorkMode::Execute,
                    thread_id: None,
                    parent_request_id: None,
                    workspace_id: None,
                    work_id: None,
                    run_id: None,
                    paths: Vec::new(),
                    artifacts: Vec::new(),
                    links: Vec::new(),
                    air_artifacts: Vec::new(),
                    withdraw_reply_wakes: Vec::new(),
                    recipient_slot: None,
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
            assert!(delivered[0]
                .1
                .contains(&format!("muxa_inbox(request_id=\"{}\")", agent_request.id)));
            assert!(delivered[0]
                .1
                .contains(&format!("muxa msg accept {} --json", agent_request.id)));
            assert!(!delivered[0].1.contains("muxa msg inbox --json"));
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
        let participants = worker_participants(&store.snapshot().await, &panes);
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
        authorize_test_workers(&mailbox, &participants).await;
        for body in ["first body", "second body"] {
            mailbox
                .create(
                    sender.clone(),
                    recipient.clone(),
                    NewRequest {
                        task_contract: None,
                        supersedes_request_id: None,
                        dispatch_id: None,
                        source_node_id: None,
                        delegation_parent: None,
                        human_action: None,
                        initiator: None,
                        kind: RequestKind::Task,
                        body: body.into(),
                        expects_reply: true,
                        notify: None,
                        work_mode: WorkMode::Execute,
                        thread_id: None,
                        parent_request_id: None,
                        workspace_id: None,
                        work_id: None,
                        run_id: None,
                        paths: Vec::new(),
                        artifacts: Vec::new(),
                        links: Vec::new(),
                        air_artifacts: Vec::new(),
                        withdraw_reply_wakes: Vec::new(),
                        recipient_slot: None,
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
        assert_eq!(
            mailbox.pending_unnotified().await,
            [] as [muxa::collaboration::CollaborationRequest; 0]
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // explicit durable request/recovery fixtures
    async fn direct_wake_recovers_without_reinjecting_the_request_body() {
        for payload in [
            CollaborationWakePayload::Full,
            CollaborationWakePayload::Adaptive,
        ] {
            let store = muxa::Store::shared();
            add_agent(&store, "%1", "sender", AgentKind::Codex).await;
            add_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
            let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
            let participants = worker_participants(&store.snapshot().await, &panes);
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
            authorize_test_workers(&mailbox, &participants).await;
            let request = mailbox
                .create(
                    sender.clone(),
                    recipient.clone(),
                    NewRequest {
                        human_action: None,
                        initiator: None,
                        kind: RequestKind::Task,
                        body: "do not inject this twice".into(),
                        work_mode: WorkMode::Execute,
                        ..NewRequest::default()
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

            wake_idle_collaboration_peers_with_inflight(
                &mailbox,
                &store,
                std::slice::from_ref(&backend),
                payload,
                &mut HashSet::new(),
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
            assert_eq!(
                mailbox.pending_unnotified().await,
                [] as [muxa::collaboration::CollaborationRequest; 0]
            );

            let second = mailbox
                .create(
                    sender,
                    recipient.clone(),
                    NewRequest {
                        task_contract: None,
                        supersedes_request_id: None,
                        dispatch_id: None,
                        source_node_id: None,
                        delegation_parent: None,
                        human_action: None,
                        initiator: None,
                        kind: RequestKind::Task,
                        body: "the prompt text is already buffered".into(),
                        expects_reply: true,
                        notify: None,
                        work_mode: WorkMode::Execute,
                        thread_id: None,
                        parent_request_id: None,
                        workspace_id: None,
                        work_id: None,
                        run_id: None,
                        paths: Vec::new(),
                        artifacts: Vec::new(),
                        links: Vec::new(),
                        air_artifacts: Vec::new(),
                        withdraw_reply_wakes: Vec::new(),
                        recipient_slot: None,
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

            wake_idle_collaboration_peers_with_inflight(
                &mailbox,
                &store,
                &[backend],
                payload,
                &mut HashSet::new(),
            )
            .await;
            assert_eq!(
                sends.lock().unwrap().as_slice(),
                &[("%2".into(), "\r".into())]
            );
            assert_eq!(
                mailbox.pending_unnotified().await,
                [] as [muxa::collaboration::CollaborationRequest; 0]
            );
        }
    }

    /// Replies that piled up while the sender worked arrive as one prompt,
    /// and one the sender withdrew (it moved on) is not announced at all.
    #[tokio::test]
    async fn piled_up_replies_wake_once_and_skip_withdrawn_ones() {
        let store = muxa::Store::shared();
        add_agent(&store, "%1", "sender", AgentKind::Codex).await;
        add_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
        let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
        let participants = worker_participants(&store.snapshot().await, &panes);
        let find = |pane: &str| {
            participants
                .iter()
                .find(|participant| participant.pane == pane)
                .unwrap()
                .clone()
        };
        let (sender, recipient) = (find("%1"), find("%2"));
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        authorize_test_workers(&mailbox, &participants).await;
        let mut ids = Vec::new();
        for body in ["abandoned", "first", "second"] {
            let request = mailbox
                .create(
                    sender.clone(),
                    recipient.clone(),
                    NewRequest {
                        body: body.into(),
                        ..NewRequest::default()
                    },
                )
                .await
                .unwrap();
            ids.push(request.id);
        }
        mailbox.claim_for(&recipient).await.unwrap();
        mailbox
            .withdraw_reply_wakes(&sender, &ids[..1])
            .await
            .unwrap();
        for id in &ids {
            mailbox
                .reply(
                    &recipient,
                    id,
                    RequestStatus::Completed,
                    "done".into(),
                    Vec::new(),
                    Vec::new(),
                )
                .await
                .unwrap();
        }

        let sends = Arc::new(Mutex::new(Vec::new()));
        let backend: muxa::SharedBackend = Arc::new(CollaborationWakeBackend {
            panes,
            sends: sends.clone(),
        });
        for _ in 0..2 {
            wake_idle_collaboration_peers(&mailbox, &store, std::slice::from_ref(&backend)).await;
        }
        {
            let sends = sends.lock().unwrap();
            assert_eq!(sends.len(), 2, "one prompt and one submit: {sends:?}");
            assert_eq!(sends[0].0, "%1");
            assert!(
                sends[0].1.starts_with("[muxa] 2 replies are ready: "),
                "{}",
                sends[0].1
            );
            assert!(sends[0].1.contains(&ids[1]) && sends[0].1.contains(&ids[2]));
            assert!(!sends[0].1.contains(&ids[0]), "withdrawn reply announced");
            assert_eq!(sends[1], ("%1".into(), "\r".into()));
        }
        assert_eq!(mailbox.unread_reply_count(&sender).await, 3);
    }

    /// The waker resolves the sender from a scan, then reserves. If the
    /// sender started a turn (or its pane got a new session) in between,
    /// nothing may be typed and the reply stays owed for its next idle turn.
    #[tokio::test]
    async fn a_sender_busy_again_since_the_scan_gets_no_reply_wake() {
        for replaced in [false, true] {
            let store = muxa::Store::shared();
            add_agent(&store, "%1", "sender", AgentKind::Codex).await;
            add_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
            let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
            let participants = worker_participants(&store.snapshot().await, &panes);
            let find = |pane: &str| {
                participants
                    .iter()
                    .find(|participant| participant.pane == pane)
                    .unwrap()
                    .clone()
            };
            let (sender, recipient) = (find("%1"), find("%2"));
            let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
            authorize_test_workers(&mailbox, &participants).await;
            let request = mailbox
                .create(
                    sender.clone(),
                    recipient.clone(),
                    NewRequest {
                        body: "review".into(),
                        ..NewRequest::default()
                    },
                )
                .await
                .unwrap();
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
            let replies = mailbox.pending_reply_unnotified().await;

            if replaced {
                add_agent(&store, "%1", "fresh-session", AgentKind::Codex).await;
            } else {
                store
                    .apply(&AgentEvent::PromptSubmitted {
                        id: AgentId {
                            kind: AgentKind::Codex,
                            session_id: "sender".into(),
                            surface: None,
                            pane: Some("%1".into()),
                            tmux_socket: Some("default".into()),
                            cwd: Some("/repo".into()),
                        },
                        prompt: "next task".into(),
                        at: OffsetDateTime::now_utc(),
                    })
                    .await;
            }

            let sends = Arc::new(Mutex::new(Vec::new()));
            let backend: muxa::SharedBackend = Arc::new(CollaborationWakeBackend {
                panes,
                sends: sends.clone(),
            });
            wake_senders_of_ready_replies(
                &mailbox,
                &participants,
                std::slice::from_ref(&backend),
                replies,
                &mut HashSet::new(),
                &store,
            )
            .await;
            assert!(sends.lock().unwrap().is_empty(), "replaced={replaced}");
            assert_eq!(
                mailbox.pending_reply_unnotified().await.len(),
                1,
                "the wake stays owed (replaced={replaced})"
            );
        }
    }

    /// The running waker, not one pass: a pane that refuses input must not
    /// turn the reservation bookkeeping into a retry loop. The reply stays
    /// owed and is delivered once when the sender next goes idle.
    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one end-to-end run of the waker task
    async fn a_refusing_pane_is_retried_on_the_next_idle_turn_not_in_a_loop() {
        let store = muxa::Store::shared();
        add_agent(&store, "%1", "sender", AgentKind::Codex).await;
        add_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
        let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
        let participants = worker_participants(&store.snapshot().await, &panes);
        let find = |pane: &str| {
            participants
                .iter()
                .find(|participant| participant.pane == pane)
                .unwrap()
                .clone()
        };
        let (sender, recipient) = (find("%1"), find("%2"));
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        authorize_test_workers(&mailbox, &participants).await;
        let request = mailbox
            .create(
                sender.clone(),
                recipient.clone(),
                NewRequest {
                    body: "review".into(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        mailbox.claim_for(&recipient).await.unwrap();

        let attempts = Arc::new(Mutex::new(Vec::new()));
        let accepting = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let backend: muxa::SharedBackend = Arc::new(RefusingWakeBackend {
            panes,
            attempts: attempts.clone(),
            accepting: accepting.clone(),
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
        .unwrap();

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
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let refused = attempts
            .lock()
            .unwrap()
            .iter()
            .filter(|(pane, text)| pane == "%1" && text.contains(&request.id))
            .count();
        assert!(
            (1..=2).contains(&refused),
            "expected one attempt per published change, saw {refused}"
        );
        assert_eq!(mailbox.pending_reply_unnotified().await.len(), 1);

        accepting.store(true, std::sync::atomic::Ordering::SeqCst);
        let id = AgentId {
            kind: AgentKind::Codex,
            session_id: "sender".into(),
            surface: None,
            pane: Some("%1".into()),
            tmux_socket: Some("default".into()),
            cwd: Some("/repo".into()),
        };
        store
            .apply(&AgentEvent::PromptSubmitted {
                id: id.clone(),
                prompt: "other work".into(),
                at: OffsetDateTime::now_utc(),
            })
            .await;
        store
            .apply(&AgentEvent::TurnStopped {
                id,
                response: None,
                recap: None,
                ai_title: None,
                idle_confirmed: true,
                at: OffsetDateTime::now_utc(),
            })
            .await;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !mailbox.pending_reply_unnotified().await.is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the next idle turn delivers the owed reply");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let _ = shutdown_tx.send(());
        let _ = waker.await;
        let delivered = attempts
            .lock()
            .unwrap()
            .iter()
            .filter(|(pane, text)| pane == "%1" && text.contains(&request.id))
            .count();
        assert_eq!(
            delivered,
            refused + 1,
            "delivered exactly once after the refusals"
        );
    }

    #[tokio::test]
    async fn reply_wake_prompt_names_stale_requests() {
        let now = time::OffsetDateTime::now_utc();
        let store = muxa::Store::shared();
        add_agent(&store, "%1", "sender", AgentKind::Codex).await;
        add_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
        let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
        let participants = worker_participants(&store.snapshot().await, &panes);
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        authorize_test_workers(&mailbox, &participants).await;
        let template = mailbox
            .create(
                participants[0].clone(),
                participants[1].clone(),
                NewRequest {
                    body: "b".into(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        let request = |id: &str, age_minutes: i64| {
            let mut request = template.clone();
            request.id = id.into();
            request.status = RequestStatus::Completed;
            request.created_at = now - time::Duration::minutes(age_minutes);
            request
        };
        let single = muxa::collaboration::ReplyWakeBatch {
            token: 1,
            requests: vec![request("req_a", 5)],
            remaining: 0,
        };
        assert_eq!(
            reply_wake_prompt(&single, now),
            format!(
                "[muxa:req_a] Completed reply from {} is ready. Read it with muxa_wait_reply (MCP) or `muxa msg wait req_a`.",
                single.requests[0].to.label()
            ),
            "a fresh single reply keeps the established text"
        );
        let stale = muxa::collaboration::ReplyWakeBatch {
            token: 2,
            requests: vec![request("req_b", 9 * 60)],
            remaining: 3,
        };
        let prompt = reply_wake_prompt(&stale, now);
        assert!(prompt.contains("req_b: Completed reply from"), "{prompt}");
        assert!(prompt.contains("(requested 9h ago)"), "{prompt}");
        assert!(prompt.contains("3 more will follow."), "{prompt}");
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // end-to-end wake setup is intentionally kept in one scenario
    async fn terminal_reply_wakes_idle_sender_without_injecting_body() {
        let store = muxa::Store::shared();
        add_agent(&store, "%1", "sender", AgentKind::Codex).await;
        add_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
        let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
        let agents = store.snapshot().await;
        let participants = worker_participants(&agents, &panes);
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
        authorize_test_workers(&mailbox, &participants).await;
        mailbox
            .set_purpose(&sender, muxa::collaboration::LaunchPurpose::Primary)
            .await
            .unwrap();
        let request = mailbox
            .create_with_provenance(
                sender,
                recipient.clone(),
                NewRequest {
                    task_contract: None,
                    supersedes_request_id: None,
                    dispatch_id: None,
                    source_node_id: None,
                    delegation_parent: None,
                    human_action: None,
                    initiator: None,
                    kind: RequestKind::Review,
                    body: "secret request body".into(),
                    expects_reply: true,
                    notify: None,
                    work_mode: WorkMode::ReadOnly,
                    thread_id: None,
                    parent_request_id: None,
                    workspace_id: None,
                    work_id: None,
                    run_id: None,
                    paths: Vec::new(),
                    artifacts: Vec::new(),
                    links: Vec::new(),
                    air_artifacts: Vec::new(),
                    withdraw_reply_wakes: Vec::new(),
                    recipient_slot: None,
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
            assert!(sends[0]
                .1
                .contains(&format!("muxa msg accept {} --json", request.id)));
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
        assert_eq!(
            mailbox.pending_reply_unnotified().await,
            [] as [muxa::collaboration::CollaborationRequest; 0]
        );
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
        let participants = worker_participants(&agents, &panes);
        let recipient = participants[0].clone();
        let console = muxa::collaboration::resolve_origin(
            &muxa::collaboration::CollaborationOrigin {
                exact_session: false,
                pane: "%2".into(),
                socket: None,
                console: true,
                agent_session_id: None,
            },
            &participants,
            &panes,
        )
        .unwrap();
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        authorize_test_workers(&mailbox, &participants).await;
        let request = mailbox
            .create(
                console.clone(),
                recipient.clone(),
                NewRequest {
                    task_contract: None,
                    supersedes_request_id: None,
                    dispatch_id: None,
                    source_node_id: None,
                    delegation_parent: None,
                    human_action: None,
                    initiator: None,
                    kind: RequestKind::Task,
                    body: "dispatched by a human".into(),
                    expects_reply: true,
                    notify: None,
                    work_mode: WorkMode::ReadOnly,
                    thread_id: None,
                    parent_request_id: None,
                    workspace_id: None,
                    work_id: None,
                    run_id: None,
                    paths: Vec::new(),
                    artifacts: Vec::new(),
                    links: Vec::new(),
                    air_artifacts: Vec::new(),
                    withdraw_reply_wakes: Vec::new(),
                    recipient_slot: None,
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

    // -----------------------------------------------------------------
    // automation engine
    // -----------------------------------------------------------------

    /// A capped agent: `Started`, then a `RateLimited` whose reset time has
    /// already passed, so a `wait = "reset"` rule is due immediately.
    async fn add_capped_agent(store: &muxa::SharedStore, pane: &str, session_id: &str) {
        add_agent(store, pane, session_id, AgentKind::ClaudeCode).await;
        store
            .apply(&AgentEvent::RateLimited {
                id: AgentId {
                    kind: AgentKind::ClaudeCode,
                    session_id: session_id.into(),
                    surface: None,
                    pane: Some(pane.into()),
                    tmux_socket: Some("default".into()),
                    cwd: Some("/repo".into()),
                },
                scope: muxa::event::RateLimitScope::FiveHour,
                source: muxa::event::RateLimitSource::Statusline,
                resets_at: Some(OffsetDateTime::now_utc() - time::Duration::minutes(1)),
                message: Some("You've hit your limit".into()),
                at: OffsetDateTime::now_utc(),
            })
            .await;
    }

    fn resume_rule_config() -> muxa::automation::AutomationConfig {
        let mut rule = AutomationRule::new(
            "resume-after-limit",
            muxa::automation::AutomationEvent::RateLimited,
            AutomationAction::SendPrompt,
        );
        rule.text = Some("continue".into());
        rule.wait = Some(muxa::automation::parse_wait("reset").unwrap());
        rule.jitter = Some(muxa::automation::DurationSpec::seconds(0));
        muxa::automation::AutomationConfig {
            rule: vec![rule],
            ..muxa::automation::AutomationConfig::default()
        }
    }

    /// Every `(pane, text)` an automation typed, in order.
    type RecordedSends = Arc<Mutex<Vec<(String, String)>>>;

    fn automation_backend(panes: Vec<PaneInfo>) -> (Vec<muxa::SharedBackend>, RecordedSends) {
        let sends = Arc::new(Mutex::new(Vec::new()));
        let backend: muxa::SharedBackend = Arc::new(CollaborationWakeBackend {
            panes,
            sends: sends.clone(),
        });
        (vec![backend], sends)
    }

    struct JudgmentBackend {
        inner: CollaborationWakeBackend,
        screen: Arc<Mutex<String>>,
    }

    impl PaneBackend for JudgmentBackend {
        fn kind(&self) -> HostKind {
            HostKind::Tmux
        }
        fn list_panes(&self) -> Vec<PaneInfo> {
            self.inner.list_panes()
        }
        fn resolve_pane(&self, pane: &str) -> Option<PaneInfo> {
            self.inner.resolve_pane(pane)
        }
        fn capture_pane(&self, _: &str) -> Option<String> {
            Some(self.screen.lock().unwrap().clone())
        }
        fn pane_pid_map(&self) -> HashMap<u32, String> {
            HashMap::new()
        }
        fn current_pane(&self) -> Option<String> {
            None
        }
        fn focus_pane(&self, _: &str) -> bool {
            false
        }
        fn send_text(&self, pane: &str, text: &str) -> bool {
            self.inner.send_text(pane, text)
        }
        fn caps(&self) -> BackendCaps {
            BackendCaps::default()
        }
    }

    async fn judgment_fixture(
        observe_only: bool,
    ) -> (
        AutomationEngine,
        JudgedFiring,
        RecordedSends,
        Arc<Mutex<String>>,
    ) {
        let store = muxa::Store::shared();
        add_capped_agent(&store, "%1", "capped").await;
        let sends = Arc::new(Mutex::new(Vec::new()));
        let screen = Arc::new(Mutex::new("Continue the agreed task?".to_string()));
        let mut pane = collaboration_pane("%1", "0");
        pane.workspace_id = Some("workspace".into());
        pane.work_id = Some("work".into());
        let backend: muxa::SharedBackend = Arc::new(JudgmentBackend {
            inner: CollaborationWakeBackend {
                panes: vec![pane],
                sends: sends.clone(),
            },
            screen: screen.clone(),
        });
        let mut config = resume_rule_config();
        config.rule[0].ask_condition = Some(serde_json::from_value(serde_json::json!({
            "prompt": "Only continue the agreed task", "provider": "openai", "observe_only": observe_only,
        })).unwrap());
        let automation = AutomationStore::in_memory(config);
        let mut engine = AutomationEngine::new(automation, store, vec![backend]);
        engine.rescan().await;
        engine.fire_due().await;
        assert!(sends.lock().unwrap().is_empty());
        let mut completed = engine.judgments.join_next().await.unwrap().unwrap();
        assert_eq!(
            completed.context.as_ref().unwrap().work.as_deref(),
            Some("work")
        );
        assert_eq!(
            completed.context.as_ref().unwrap().workspace.as_deref(),
            Some("workspace")
        );
        completed.judgment.decision = JudgmentDecision::Match;
        completed.judgment.reason = "Test judgment".into();
        (engine, completed, sends, screen)
    }

    #[tokio::test]
    async fn automation_queued_rule_change_is_refused_before_dispatch() {
        let store = muxa::Store::shared();
        add_capped_agent(&store, "%1", "capped").await;
        let (backends, sends) = automation_backend(vec![collaboration_pane("%1", "0")]);
        let automation = AutomationStore::in_memory(resume_rule_config());
        let mut engine = AutomationEngine::new(automation.clone(), store, backends);
        engine.rescan().await;
        let mut edited = resume_rule_config().rule.remove(0);
        edited.text = Some("different action".into());
        automation.upsert_rule(edited).await.unwrap();
        engine.fire_due().await;
        assert!(sends.lock().unwrap().is_empty());
        assert!(engine.judgments.is_empty());
        assert!(automation.ledger().recent(1).await[0]
            .detail
            .as_deref()
            .unwrap()
            .contains("Queued rule"));
    }

    #[tokio::test]
    async fn automation_judgment_match_sends_only_the_fixed_action_once() {
        let (mut engine, completed, sends, _) = judgment_fixture(false).await;
        engine.finish_judgment(completed).await;
        assert_eq!(
            *sends.lock().unwrap(),
            vec![("%1".into(), "continue".into()), ("%1".into(), "\r".into())]
        );
        engine.rescan().await;
        engine.fire_due().await;
        assert_eq!(sends.lock().unwrap().len(), 2);
        assert!(engine.judgments.is_empty());
    }

    #[tokio::test]
    async fn automation_judgment_observation_never_sends() {
        let (mut engine, completed, sends, _) = judgment_fixture(true).await;
        engine.finish_judgment(completed).await;
        assert!(sends.lock().unwrap().is_empty());
        assert!(engine.automation.ledger().recent(1).await[0]
            .detail
            .as_ref()
            .unwrap()
            .contains("observe_only"));
        engine.rescan().await;
        assert!(engine.pending.is_empty());
    }

    #[tokio::test]
    async fn automation_judgment_changed_output_or_state_discards_match() {
        let (mut engine, completed, sends, screen) = judgment_fixture(false).await;
        *screen.lock().unwrap() = "Deploy to production?".into();
        engine.finish_judgment(completed).await;
        assert!(sends.lock().unwrap().is_empty());
        assert!(engine.automation.ledger().recent(1).await[0]
            .detail
            .as_ref()
            .unwrap()
            .contains("context_changed"));
        let (mut engine, completed, sends, _) = judgment_fixture(false).await;
        add_agent(&engine.store, "%1", "capped", AgentKind::ClaudeCode).await;
        engine.finish_judgment(completed).await;
        assert!(sends.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn automation_judgment_config_edit_discards_match() {
        let (mut engine, completed, sends, _) = judgment_fixture(false).await;
        engine
            .automation
            .set_paused_until(Some(OffsetDateTime::now_utc() + time::Duration::minutes(5)))
            .await
            .unwrap();
        engine.finish_judgment(completed).await;
        assert!(sends.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn automation_judgment_unknown_and_no_match_never_send() {
        for decision in [JudgmentDecision::Unknown, JudgmentDecision::NoMatch] {
            let (mut engine, mut completed, sends, _) = judgment_fixture(false).await;
            completed.judgment.decision = decision;
            engine.finish_judgment(completed).await;
            assert!(sends.lock().unwrap().is_empty());
            engine.rescan().await;
            assert!(engine.pending.is_empty());
        }
    }

    #[tokio::test]
    async fn automation_enter_sends_one_key_only_for_an_affirmative_choice() {
        for (screen, expected) in [
            ("› 1. Yes, continue\n  2. No\nPress enter to continue", true),
            ("  1. Yes\n› 2. No\nPress enter to continue", false),
            ("› Type your reply", false),
        ] {
            let store = muxa::Store::shared();
            add_agent(&store, "%1", "waiting", AgentKind::ClaudeCode).await;
            store
                .apply(&AgentEvent::NotificationFired {
                    id: AgentId {
                        kind: AgentKind::ClaudeCode,
                        session_id: "waiting".into(),
                        surface: None,
                        pane: Some("%1".into()),
                        tmux_socket: Some("default".into()),
                        cwd: Some("/repo".into()),
                    },
                    level: muxa::event::NotificationLevel::NeedsChoice,
                    message: "Confirm choice".into(),
                    at: OffsetDateTime::now_utc(),
                })
                .await;
            let sends = Arc::new(Mutex::new(Vec::new()));
            let backend: muxa::SharedBackend = Arc::new(JudgmentBackend {
                inner: CollaborationWakeBackend {
                    panes: vec![collaboration_pane("%1", "0")],
                    sends: sends.clone(),
                },
                screen: Arc::new(Mutex::new(screen.into())),
            });
            let mut rule = muxa::automation_defaults::rules()
                .into_iter()
                .find(|rule| rule.name == "confirm-selected-yes")
                .unwrap();
            rule.enabled = true;
            rule.wait = Some(muxa::automation::parse_wait("0s").unwrap());
            let config = muxa::automation::AutomationConfig {
                rule: vec![rule],
                ..Default::default()
            };
            let automation = AutomationStore::in_memory(config);
            let mut engine = AutomationEngine::new(automation, store, vec![backend]);
            for _ in 0..2 {
                engine.rescan().await;
                engine.fire_due().await;
            }
            let expected_sends = if expected {
                vec![("%1".into(), "\r".into())]
            } else {
                vec![]
            };
            assert_eq!(*sends.lock().unwrap(), expected_sends, "{screen}");
        }
    }

    #[tokio::test]
    async fn automation_resumes_a_capped_agent_by_typing_into_its_pane() {
        let store = muxa::Store::shared();
        add_capped_agent(&store, "%1", "capped").await;
        let (backends, sends) = automation_backend(vec![collaboration_pane("%1", "0")]);
        let automation = AutomationStore::in_memory(resume_rule_config());
        let mut engine = AutomationEngine::new(automation.clone(), store, backends);

        engine.rescan().await;
        engine.fire_due().await;

        // The prompt, then the submit CR — the same two-step the
        // collaboration waker uses.
        assert_eq!(
            *sends.lock().unwrap(),
            vec![
                ("%1".to_string(), "continue".to_string()),
                ("%1".to_string(), "\r".to_string()),
            ],
        );
        let log = automation.ledger().all().await;
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].outcome, AutomationOutcome::Fired);
        assert_eq!(log[0].rule, "resume-after-limit");
        assert_eq!(log[0].pane, "%1");
    }

    #[tokio::test]
    async fn automation_fires_once_per_cap_episode() {
        let store = muxa::Store::shared();
        add_capped_agent(&store, "%1", "capped").await;
        let (backends, sends) = automation_backend(vec![collaboration_pane("%1", "0")]);
        let automation = AutomationStore::in_memory(resume_rule_config());
        let mut engine = AutomationEngine::new(automation.clone(), store, backends);

        for _ in 0..3 {
            engine.rescan().await;
            engine.fire_due().await;
        }

        // The row is still capped and still in the same episode, so the
        // ledger's episode key stops every re-arm after the first.
        assert_eq!(sends.lock().unwrap().len(), 2, "one prompt, one submit");
        let log = automation.ledger().all().await;
        assert_eq!(log.len(), 1, "{log:?}");
    }

    #[tokio::test]
    async fn automation_skips_an_agent_that_recovered_before_the_firing_was_due() {
        let store = muxa::Store::shared();
        add_capped_agent(&store, "%1", "capped").await;
        let (backends, sends) = automation_backend(vec![collaboration_pane("%1", "0")]);
        let automation = AutomationStore::in_memory(resume_rule_config());
        let mut engine = AutomationEngine::new(automation.clone(), store.clone(), backends);
        engine.rescan().await;

        // The operator got there first: a fresh `Started` clears the cap.
        add_agent(&store, "%1", "capped", AgentKind::ClaudeCode).await;
        engine.fire_due().await;

        assert!(
            sends.lock().unwrap().is_empty(),
            "a recovered agent must not be typed into",
        );
        let log = automation.ledger().all().await;
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].outcome, AutomationOutcome::Skipped);
        assert_eq!(
            log[0].detail.as_deref(),
            Some(SkipReason::ConditionCleared.to_string()).as_deref(),
        );
    }

    #[tokio::test]
    async fn automation_with_the_engine_paused_does_nothing() {
        let store = muxa::Store::shared();
        add_capped_agent(&store, "%1", "capped").await;
        let (backends, sends) = automation_backend(vec![collaboration_pane("%1", "0")]);
        let mut config = resume_rule_config();
        config.paused_until = Some(OffsetDateTime::now_utc() + time::Duration::hours(1));
        let automation = AutomationStore::in_memory(config);
        let mut engine = AutomationEngine::new(automation.clone(), store, backends);

        engine.rescan().await;
        engine.fire_due().await;

        assert!(sends.lock().unwrap().is_empty());
        assert_eq!(
            automation.ledger().all().await,
            [] as [muxa::automation::AutomationLedgerEntry; 0]
        );
    }

    #[tokio::test]
    async fn automation_with_no_rules_is_inert() {
        let store = muxa::Store::shared();
        add_capped_agent(&store, "%1", "capped").await;
        let (backends, sends) = automation_backend(vec![collaboration_pane("%1", "0")]);
        // The shipped default: enabled, with no rules at all.
        let automation = AutomationStore::in_memory(muxa::automation::AutomationConfig::default());
        let mut engine = AutomationEngine::new(automation.clone(), store, backends);

        engine.rescan().await;
        engine.fire_due().await;

        assert!(sends.lock().unwrap().is_empty());
        assert_eq!(
            automation.ledger().all().await,
            [] as [muxa::automation::AutomationLedgerEntry; 0]
        );
    }

    #[tokio::test]
    async fn automation_honours_the_workspace_and_work_filters() {
        let store = muxa::Store::shared();
        add_capped_agent(&store, "%1", "match").await;
        add_capped_agent(&store, "%2", "miss").await;
        let mut matching = collaboration_pane("%1", "0");
        matching.workspace_id = Some("callabo".into());
        matching.work_id = Some("CAL-1234".into());
        let mut other = collaboration_pane("%2", "1");
        other.workspace_id = Some("callabo".into());
        other.work_id = Some("JIRA-9".into());
        let (backends, sends) = automation_backend(vec![matching, other]);

        let mut config = resume_rule_config();
        config.rule[0].workspace = Some("callabo".into());
        config.rule[0].work = Some("^CAL-".into());
        let automation = AutomationStore::in_memory(config);
        let mut engine = AutomationEngine::new(automation, store, backends);

        engine.rescan().await;
        engine.fire_due().await;

        let sends = sends.lock().unwrap();
        assert!(
            sends.iter().all(|(pane, _)| pane == "%1"),
            "only the pane whose work id matches may be typed into: {sends:?}",
        );
        assert_eq!(sends.len(), 2);
    }
    #[tokio::test]
    async fn a_moved_sender_receives_its_reply_at_its_current_pane() {
        let store = muxa::Store::shared();
        add_agent(&store, "%1", "moving-sender", AgentKind::Codex).await;
        add_agent(&store, "%2", "reviewer", AgentKind::ClaudeCode).await;
        let initial = worker_participants(
            &store.snapshot().await,
            &[collaboration_pane("%1", "0"), collaboration_pane("%2", "1")],
        );
        let sender = initial.iter().find(|p| p.pane == "%1").unwrap().clone();
        let reviewer = initial.iter().find(|p| p.pane == "%2").unwrap().clone();
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let request = mailbox
            .create(
                sender,
                reviewer.clone(),
                NewRequest {
                    body: "Review".into(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        mailbox
            .reply(
                &reviewer,
                &request.id,
                RequestStatus::Completed,
                "Findings".into(),
                vec![],
                vec![],
            )
            .await
            .unwrap();
        // Seed the post-move registry binding, rather than simulating a new
        // Started hook (which deliberately cannot replace an established pane).
        let moved_store = muxa::Store::shared();
        let mut moved_agents = store.snapshot().await;
        moved_agents
            .iter_mut()
            .find(|a| a.session_id == "moving-sender")
            .unwrap()
            .pane = Some("%9".into());
        moved_store.seed_if_absent(moved_agents).await;
        let store = moved_store;
        add_agent(&store, "%1", "other-session", AgentKind::Codex).await;
        let panes = vec![
            collaboration_pane("%1", "0"),
            collaboration_pane("%2", "1"),
            collaboration_pane("%9", "2"),
        ];
        let participants = worker_participants(&store.snapshot().await, &panes);
        let sends = Arc::new(Mutex::new(Vec::new()));
        let backend: muxa::SharedBackend = Arc::new(CollaborationWakeBackend {
            panes,
            sends: sends.clone(),
        });
        wake_senders_of_ready_replies(
            &mailbox,
            &participants,
            &[backend],
            mailbox.pending_reply_unnotified().await,
            &mut HashSet::new(),
            &store,
        )
        .await;
        {
            let sent = sends.lock().unwrap();
            assert_eq!(sent.len(), 2);
            assert_eq!(sent[0].0, "%9");
            assert!(sent[0].1.contains(&request.id));
        }
        assert!(mailbox.pending_reply_unnotified().await.is_empty());
    }
    #[tokio::test]
    async fn a_corrupt_mailbox_never_silently_falls_back_to_memory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corrupt.sqlite");
        std::fs::write(&path, "not a database").unwrap();
        let mut config = Config::default();
        config.collaboration.enabled = true;
        config.collaboration.path = Some(path);
        assert!(build_collaboration(&config)
            .await
            .unwrap()
            .initialization_error()
            .is_some());
    }

    #[tokio::test]
    async fn contract_notices_preserve_the_cli_fallback_even_for_an_old_mcp() {
        let store = muxa::Store::shared();
        add_agent(&store, "%1", "sender", AgentKind::Codex).await;
        add_agent(&store, "%2", "reviewer", AgentKind::ClaudeCode).await;
        let participants = worker_participants(
            &store.snapshot().await,
            &[collaboration_pane("%1", "0"), collaboration_pane("%2", "1")],
        );
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let request = mailbox
            .create(
                participants[0].clone(),
                participants[1].clone(),
                NewRequest {
                    body: "Review".into(),
                    task_contract: Some(muxa::collaboration::TaskContract {
                        version: 1,
                        task_id: "typed-review".into(),
                        objective: "Review this revision".into(),
                        acceptance_criteria: vec!["Check compatibility".into()],
                        basis_revision: None,
                        inputs: vec![],
                    }),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        for prompt in [
            mailbox_collaboration_request_prompt(&request),
            interrupted_collaboration_request_prompt(&request),
        ] {
            assert!(prompt.contains("typed-review"));
            assert!(prompt.contains(&request.attempt.as_ref().unwrap().id));
            assert!(prompt.contains("--report FILE"));
            assert!(prompt.contains("upgrade/reload"));
            assert!(prompt.contains(&format!("muxa msg accept {} --json", request.id)));
        }
        let full = full_collaboration_request_prompt(&request);
        assert!(full.contains("already claimed"));
        assert!(full.contains("--report FILE"));
        assert!(full.contains("task_contract:"));
        assert!(!full.contains("muxa msg inbox"));
        assert!(!full.contains("muxa msg accept"));
    }

    #[tokio::test]
    async fn supersession_finishes_an_already_written_line_before_sending_the_stop_notice() {
        let store = muxa::Store::shared();
        add_agent(&store, "%1", "sender", AgentKind::Codex).await;
        add_agent(&store, "%2", "reviewer", AgentKind::ClaudeCode).await;
        let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
        let participants = worker_participants(&store.snapshot().await, &panes);
        let sender = participants
            .iter()
            .find(|p| p.pane == "%1")
            .unwrap()
            .clone();
        let reviewer = participants
            .iter()
            .find(|p| p.pane == "%2")
            .unwrap()
            .clone();
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        authorize_test_workers(&mailbox, &participants).await;
        let input = NewRequest {
            kind: RequestKind::Review,
            body: "Original review".into(),
            task_contract: Some(muxa::collaboration::TaskContract {
                version: 1,
                task_id: "review".into(),
                objective: "Review".into(),
                acceptance_criteria: vec!["Check code".into()],
                basis_revision: None,
                inputs: vec![],
            }),
            ..NewRequest::default()
        };
        let old = mailbox
            .create(sender.clone(), reviewer.clone(), input.clone())
            .await
            .unwrap();
        mailbox
            .prepare_direct_wake(&reviewer, &old.id)
            .await
            .unwrap();
        mailbox.mark_wake_prompt_written(&old.id).await.unwrap();
        let mut next = input;
        next.supersedes_request_id = Some(old.id.clone());
        let mut other = reviewer;
        other.pane = "%3".into();
        other.agent_session_id = "other".into();
        mailbox.create(sender, other, next).await.unwrap();
        let sends = Arc::new(Mutex::new(Vec::new()));
        let backend: muxa::SharedBackend = Arc::new(CollaborationWakeBackend {
            panes,
            sends: sends.clone(),
        });
        wake_idle_collaboration_peers_with_full_payload(
            &mailbox,
            &store,
            std::slice::from_ref(&backend),
        )
        .await;
        {
            let sent = sends.lock().unwrap();
            assert_eq!(sent.len(), 2);
            assert!(sent[0].1.contains("do not act"));
            assert_eq!(sent[1], ("%2".into(), "\r".into()));
        }
        sends.lock().unwrap().clear();
        wake_idle_collaboration_peers_with_full_payload(
            &mailbox,
            &store,
            std::slice::from_ref(&backend),
        )
        .await;
        {
            let sent = sends.lock().unwrap();
            assert_eq!(sent.len(), 2);
            assert!(sent[0].1.contains("Its results will not be used"));
            assert_eq!(sent[1].1, "\r");
        }
        sends.lock().unwrap().clear();
        wake_idle_collaboration_peers_with_full_payload(&mailbox, &store, &[backend]).await;
        assert!(sends.lock().unwrap().is_empty());
    }
    #[cfg(target_os = "linux")]
    struct Child(std::process::Child);
    #[cfg(target_os = "linux")]
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[allow(clippy::too_many_lines)] // three persisted drain gates share one worker fixture
    async fn worker_drain_gates_ordinary_wakes_full_delivery_and_automation_io() {
        let child = Child(
            std::process::Command::new("bash")
                .args(["-c", "exec -a 'codex --no-daemon' sleep 60"])
                .spawn()
                .unwrap(),
        );
        for _ in 0..300 {
            if std::fs::read(format!("/proc/{}/cmdline", child.0.id()))
                .unwrap_or_default()
                .starts_with(b"codex --no-daemon")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let store = muxa::Store::shared();
        add_agent(&store, "%2", "worker", AgentKind::Codex).await;
        let event=serde_json::from_value(serde_json::json!({"type":"turn_stopped","id":{"kind":"codex","session_id":"worker","pane":"%2","tmux_socket":"default"},"response":"ready","at":OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap()})).unwrap();
        store.apply(&event).await;
        let mut pane = collaboration_pane("%2", "1");
        pane.pane_pid = child.0.id();
        pane.current_command = "codex".into();
        let (backends, sends) = automation_backend(vec![pane.clone()]);
        let rows = store.snapshot().await;
        let participants = worker_participants(&rows, std::slice::from_ref(&pane));
        let p = participants
            .iter()
            .find(|p| p.agent_session_id == "worker")
            .unwrap();
        let c = CollaborationStore::in_memory(CollaborationOptions::default());
        authorize_test_workers(&c, std::slice::from_ref(p)).await;
        let acquired = c
            .worker_drain_acquire(
                &muxa::collaboration::WorkerDrainTarget::from(p),
                muxa::collaboration::WorkerDrainPolicy {
                    min_observed_idle_secs: 0,
                    allow_error: false,
                },
                &participants,
                &rows,
                &[pane],
            )
            .await
            .unwrap();
        assert!(matches!(
            acquired,
            muxa::collaboration::WorkerDrainAcquire::Acquired { .. }
        ));
        assert_eq!(
            send_collaboration_wake(&c, p, "notice", &backends, &store).await,
            (false, false)
        );
        let other = CollaborationStore::in_memory(CollaborationOptions::default());
        let mut sender = p.clone();
        sender.agent_session_id = "sender".into();
        sender.pane = "%1".into();
        let request = other
            .create(
                sender,
                p.clone(),
                NewRequest {
                    body: "request".into(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        let request = other
            .prepare_direct_wake(p, &request.id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            !deliver_full_collaboration_request(
                &c,
                p,
                &request,
                &backends,
                0,
                CollaborationWakePayload::Full
            )
            .await
        );
        // A swap job takes the pane the same way, and is refused the same way.
        let context = DaemonContext {
            runtime: tokio::runtime::Handle::current(),
            store: store.clone(),
            backends: backends.clone(),
            swap: muxa::account_swap::SwapConfig::default(),
            accounts: muxa::accounts::AccountsConfig::default(),
            config_path: None,
            node_id: "node".into(),
            collaboration: Arc::clone(&c),
            receipts: Arc::default(),
        };
        let (pane_id, socket) = (p.pane.clone(), p.socket.clone());
        let refused = tokio::task::spawn_blocking(move || {
            use muxa::swap_service::ContextProvider as _;
            context
                .acquire_guard(&pane_id, socket.as_deref())
                .map(|_| ())
        })
        .await
        .unwrap();
        assert!(refused.unwrap_err().contains("blocked"));
        let mut engine = AutomationEngine::new(
            AutomationStore::in_memory(resume_rule_config()),
            store,
            backends,
        );
        engine.collaboration = Some(c);
        let firing = PlannedFiring {
            rule: "rule".into(),
            pane: p.pane.clone(),
            socket: p.socket.clone(),
            agent_session_id: p.agent_session_id.clone(),
            agent: p.agent_kind,
            episode: "episode".into(),
            fire_at: OffsetDateTime::now_utc(),
            action: AutomationAction::SendPrompt,
            text: Some("automation".into()),
            submit: true,
            only_if_still: muxa::automation::AutomationCondition::Idle,
            account: None,
        };
        let rule = resume_rule_config().rule[0].clone();
        assert!(!engine.perform(&firing, &rule, 0).await);
        assert!(sends.lock().unwrap().is_empty());
    }

    /// The daemon's pane guard is per pane name: with no server it covers
    /// that pane on any, with one only that endpoint, and never another pane.
    /// The store's prompt hooks reach the swap probe, by conversation and
    /// pane, without their text.
    #[tokio::test]
    async fn prompt_hooks_reach_the_swap_probe() {
        use muxa::account_swap::TurnProbe as _;
        let store = muxa::Store::shared();
        add_agent(&store, "%1", "conv", AgentKind::ClaudeCode).await;
        let receipts = Arc::new(muxa::swap_service::PromptReceipts::default());
        spawn_prompt_receipts(&store, Arc::clone(&receipts));
        let cursor = receipts.cursor();
        let event = serde_json::from_value(serde_json::json!({
            "type": "prompt_submitted",
            "id": {"kind": "claude_code", "session_id": "conv", "pane": "%1", "tmux_socket": "default"},
            "prompt": "continue",
            "at": OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap(),
        }))
        .unwrap();
        store.apply(&event).await;
        let endpoint = muxa::swap_service::endpoint("%1", Some("default"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !receipts.prompt_after(cursor, AgentKind::ClaudeCode, "conv", &endpoint) {
            assert!(
                std::time::Instant::now() < deadline,
                "the hook never arrived"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!receipts.prompt_after(cursor, AgentKind::ClaudeCode, "other", &endpoint));
    }

    #[test]
    fn a_pane_guard_covers_its_pane_and_no_other_server() {
        use muxa::swap_service::TargetGuard as _;
        let gate = || {
            Arc::new(
                Arc::new(tokio::sync::Mutex::new(()))
                    .try_lock_owned()
                    .unwrap(),
            )
        };
        let endpoint = muxa::swap_service::endpoint;
        let any = PaneGuard {
            pane: "%1".into(),
            socket: None,
            _gate: gate(),
        };
        assert!(any.covers(&endpoint("%1", Some("/tmp/tmux-1/default"))));
        assert!(!any.covers(&endpoint("%10", Some("/tmp/tmux-1/default"))));
        assert!(!any.covers(&endpoint("%2", Some("/tmp/tmux-1/default"))));
        let pinned = PaneGuard {
            pane: "%1".into(),
            socket: Some("default".into()),
            _gate: gate(),
        };
        assert!(pinned.covers(&endpoint("%1", Some("/tmp/tmux-1/default"))));
        assert!(!pinned.covers(&endpoint("%1", Some("/tmp/tmux-1/other"))));
    }

    /// Without swap jobs an automation swap is refused outright: nothing
    /// falls back to running a swap that is not journalled.
    #[tokio::test]
    async fn an_automation_swap_without_swap_jobs_is_refused() {
        let store = muxa::Store::shared();
        let (backends, sends) = automation_backend(Vec::new());
        let mut engine = AutomationEngine::new(
            AutomationStore::in_memory(resume_rule_config()),
            store,
            backends,
        );
        let mut rule = resume_rule_config().rule[0].clone();
        rule.action = AutomationAction::SwapAccount;
        let firing = PlannedFiring {
            rule: rule.name.clone(),
            pane: "%1".into(),
            socket: Some("default".into()),
            agent_session_id: "sid".into(),
            agent: AgentKind::ClaudeCode,
            episode: "episode".into(),
            fire_at: OffsetDateTime::now_utc(),
            action: AutomationAction::SwapAccount,
            text: None,
            submit: false,
            only_if_still: muxa::automation::AutomationCondition::Idle,
            account: None,
        };
        assert!(!engine.perform(&firing, &rule, 0).await);
        assert!(sends.lock().unwrap().is_empty());
    }

    type Counter = Arc<std::sync::atomic::AtomicUsize>;

    fn count(counter: &Counter) -> usize {
        counter.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The daemon's agents as they were when the fixture was made; counts
    /// what the service asks of it.
    struct FixtureContext {
        agents: Vec<muxa::Agent>,
        snapshots: Counter,
        guards: Counter,
    }

    impl muxa::swap_service::ContextProvider for FixtureContext {
        fn snapshot(&self) -> Result<muxa::swap_service::Snapshot, String> {
            self.snapshots
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(muxa::swap_service::Snapshot {
                node_id: "node".into(),
                agents: self.agents.clone(),
                backends: Vec::new(),
                config: muxa::account_swap::SwapConfig::default(),
                accounts: muxa::accounts::AccountsConfig::default(),
                config_path: None,
            })
        }

        fn acquire_guard(
            &self,
            _pane: &str,
            _socket: Option<&str>,
        ) -> Result<Box<dyn muxa::swap_service::TargetGuard>, String> {
            self.guards
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err("an automation swap must bring its own guard".into())
        }
    }

    /// An automation engine with a swap rule over one capped agent, its
    /// swaps run by a real job service whose engine touches nothing and
    /// whose targets can be held mid-run.
    struct SwapFixture {
        engine: AutomationEngine,
        automation: Arc<AutomationStore>,
        collaboration: Arc<CollaborationStore>,
        service: Arc<muxa::swap_service::SwapService>,
        sends: RecordedSends,
        gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
        snapshots: Counter,
        guards: Counter,
        runs: Counter,
        key: String,
        _dir: tempfile::TempDir,
    }

    impl SwapFixture {
        async fn new() -> Self {
            let store = muxa::Store::shared();
            add_capped_agent(&store, "%1", "capped").await;
            let agents = store.snapshot().await;
            let episode = AutomationSubject::from_agent(&agents[0]).episode();
            let (backends, sends) = automation_backend(vec![collaboration_pane("%1", "0")]);
            let mut rule = AutomationRule::new(
                "swap-on-limit",
                muxa::automation::AutomationEvent::RateLimited,
                AutomationAction::SwapAccount,
            );
            rule.jitter = Some(muxa::automation::DurationSpec::seconds(0));
            let automation = AutomationStore::in_memory(muxa::automation::AutomationConfig {
                rule: vec![rule],
                ..muxa::automation::AutomationConfig::default()
            });
            let collaboration = CollaborationStore::in_memory(CollaborationOptions::default());
            let gate = Arc::new((Mutex::new(true), std::sync::Condvar::new()));
            let (snapshots, guards, runs) =
                (Counter::default(), Counter::default(), Counter::default());
            let dir = tempfile::tempdir().unwrap();
            let (held, ran) = (Arc::clone(&gate), Arc::clone(&runs));
            let service = Arc::new(
                muxa::swap_service::testing::open_inert(
                    &dir.path().join("swap-jobs.sqlite3"),
                    Arc::new(FixtureContext {
                        agents,
                        snapshots: Arc::clone(&snapshots),
                        guards: Arc::clone(&guards),
                    }),
                    Arc::new(move || {
                        ran.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let (open, turned) = &*held;
                        let mut open = open.lock().unwrap();
                        while !*open {
                            open = turned.wait(open).unwrap();
                        }
                    }),
                )
                .unwrap(),
            );
            let mut engine = AutomationEngine::new(automation.clone(), store, backends);
            engine.collaboration = Some(Arc::clone(&collaboration));
            engine.swap_service = Some(Arc::new(DaemonSwap {
                service: Arc::clone(&service),
                node_id: "node".into(),
            }));
            let key = muxa::swap_service::automation_request_key(
                "node",
                "%1",
                Some("default"),
                "capped",
                "swap-on-limit",
                &episode,
            );
            Self {
                engine,
                automation,
                collaboration,
                service,
                sends,
                gate,
                snapshots,
                guards,
                runs,
                key,
                _dir: dir,
            }
        }

        fn hold(&self, held: bool) {
            let (open, turned) = &*self.gate;
            *open.lock().unwrap() = !held;
            turned.notify_all();
        }

        fn take_engine(&mut self) -> AutomationEngine {
            std::mem::replace(
                &mut self.engine,
                AutomationEngine::new(
                    AutomationStore::in_memory(muxa::automation::AutomationConfig::default()),
                    muxa::Store::shared(),
                    Vec::new(),
                ),
            )
        }

        fn job(&self) -> muxa::swap_service::JobView {
            use muxa::swap_service::SwapJobs as _;
            self.service
                .status(&muxa::swap_service::JobRef::Key(self.key.clone()))
                .unwrap()
        }

        async fn pane_is_free(&self) -> bool {
            tokio::time::timeout(
                std::time::Duration::from_millis(500),
                self.collaboration.worker_action_guard("%1"),
            )
            .await
            .is_ok()
        }
    }

    /// A failed assertion still opens the gate, so the runtime's teardown
    /// never waits on a held job.
    impl Drop for SwapFixture {
        fn drop(&mut self) {
            self.hold(false);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_automation_swap_hands_its_pane_guard_to_the_job_until_it_ends() {
        let mut fixture = SwapFixture::new().await;
        fixture.hold(true);
        fixture.engine.rescan().await;
        let mut engine = fixture.take_engine();
        let firing = tokio::spawn(async move { engine.fire_due().await });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while count(&fixture.runs) == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the swap never started running"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // An accepted, journalled job is running its one target.
        assert_eq!(fixture.job().state, "running");
        // The job holds the automation's own guard: busy, and still busy
        // once the engine that submitted it is gone.
        assert!(!fixture.pane_is_free().await);
        firing.abort();
        let _ = firing.await;
        assert!(!fixture.pane_is_free().await);
        assert_eq!(fixture.job().state, "running");
        fixture.hold(false);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !fixture.pane_is_free().await {
            assert!(
                std::time::Instant::now() < deadline,
                "the job never let the pane go"
            );
        }
        let job = fixture.job();
        assert_eq!(job.state, "done");
        assert_eq!(job.entries.len(), 1);
        assert_eq!(job.entries[0].disposition.as_deref(), Some("swapped"));
        assert_eq!(count(&fixture.runs), 1);
        assert_eq!(
            count(&fixture.guards),
            0,
            "the job never takes the pane again"
        );
        assert!(fixture.sends.lock().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_automation_swap_disabled_while_it_waited_for_its_pane_is_dropped() {
        let mut fixture = SwapFixture::new().await;
        fixture.engine.rescan().await;
        let held = fixture.collaboration.worker_action_guard("%1").await;
        let mut engine = fixture.take_engine();
        let firing = tokio::spawn(async move { engine.fire_due().await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        fixture
            .automation
            .set_rule_enabled("swap-on-limit", false)
            .await
            .unwrap();
        drop(held);
        tokio::time::timeout(std::time::Duration::from_secs(5), firing)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            count(&fixture.snapshots),
            0,
            "a stale firing must not reach the swap service"
        );
        assert_eq!(count(&fixture.runs), 0);
        assert!(fixture.sends.lock().unwrap().is_empty());
    }

    #[test]
    fn the_daemon_runtime_does_not_wait_out_a_stuck_blocking_task() {
        let started = std::time::Instant::now();
        let result = run_bounded(
            async {
                tokio::task::spawn_blocking(|| {
                    std::thread::sleep(std::time::Duration::from_secs(30));
                });
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                Ok(())
            },
            std::time::Duration::from_millis(200),
        );
        assert!(result.is_ok());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "teardown took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn reference_wake_overhead_is_bounded_independently_of_body_size() {
        let participant = serde_json::json!({"agent_kind":"codex","agent_session_id":"01a1820e-a385-79d0-acfd-8ebda41e2d5b","pane":"rmux:%290","room":{"host":"rmux","window_id":"@34"},"state":"idle"});
        let mut request:CollaborationRequest=serde_json::from_value(serde_json::json!({"id":"req_0123456789abcdef_1","from":participant,"to":participant,"kind":"review","body":"short","expects_reply":true,"work_mode":"read_only","status":"queued","created_at":"2026-10-28T00:00:00Z"})).unwrap();
        let short = mailbox_collaboration_request_prompt(&request);
        request.body = "한글 장문 요청\n".repeat(4096);
        let long = mailbox_collaboration_request_prompt(&request);
        assert_eq!(short, long);
        assert!(short.len() <= 320, "{} bytes: {short}", short.len());
        assert!(short.contains("muxa msg accept"));
        assert!(short.contains("muxa_inbox(request_id="));
        assert!(!short.contains("장문"));
        if let Ok(path) = std::env::var("MUXA_WAKE_OVERHEAD_REPORT") {
            std::fs::write(path,serde_json::json!({"reference_wake_bytes":short.len(),"wake":short,"body_repetitions":0}).to_string()).unwrap();
        }
    }
    #[tokio::test]
    async fn adaptive_wake_claims_once_with_shared_instructions_and_no_initial_read() {
        assert_adaptive_wake_claims_once("verify this small change").await;
        assert_adaptive_wake_claims_once(&"한".repeat(200)).await;
    }

    async fn assert_adaptive_wake_claims_once(body: &str) {
        let store = muxa::Store::shared();
        add_agent(&store, "%1", "sender", AgentKind::Codex).await;
        add_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
        let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
        let participants = worker_participants(&store.snapshot().await, &panes);
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        authorize_test_workers(&mailbox, &participants).await;
        let request = mailbox
            .create(
                participants[0].clone(),
                participants[1].clone(),
                NewRequest {
                    kind: RequestKind::Review,
                    body: body.into(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        let sends = Arc::new(Mutex::new(Vec::new()));
        let backend: muxa::SharedBackend = Arc::new(CollaborationWakeBackend {
            panes,
            sends: sends.clone(),
        });
        wake_idle_collaboration_peers_with_inflight(
            &mailbox,
            &store,
            std::slice::from_ref(&backend),
            CollaborationWakePayload::Adaptive,
            &mut HashSet::new(),
        )
        .await;
        let saved = mailbox
            .peek_for(&participants[1], &request.id)
            .await
            .unwrap();
        assert_eq!(saved.status, RequestStatus::Claimed);
        assert!(saved.claimed_at.is_some());
        assert!(saved.notified_at.is_some());
        assert!(saved.wake_delivery.is_none());
        let delivered = sends.lock().unwrap().clone();
        assert_eq!(delivered.len(), 2);
        let inline = &delivered[0].1;
        let encoded = inline
            .split("--- peer request ---\n")
            .nth(1)
            .unwrap()
            .split("\n--- end peer request ---")
            .next()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(encoded).unwrap(),
            muxa::collaboration::instruction::inbox_instruction(&saved)
        );
        assert!(inline.contains("Claimed; skip inbox"));
        assert_eq!(delivered[1].1, "\r");
        wake_idle_collaboration_peers_with_inflight(
            &mailbox,
            &store,
            &[backend],
            CollaborationWakePayload::Adaptive,
            &mut HashSet::new(),
        )
        .await;
        assert_eq!(sends.lock().unwrap().len(), 2);
        let pull = muxa::collaboration::instruction::inbox_instruction(&saved)
            .to_string()
            .len();
        let reference = mailbox_collaboration_request_prompt(&saved).len() + pull;
        assert!(inline.len() < reference);
        if let Ok(path) = std::env::var("MUXA_SHORT_DELIVERY_REPORT") {
            std::fs::write(path,serde_json::json!({"body_bytes":saved.body.len(),"inline_received_bytes":inline.len(),"inline_extra_bytes":inline.len()-saved.body.len(),"reference_wake_and_read_bytes":reference,"reference_extra_bytes":reference-saved.body.len(),"inline_initial_reads":0,"reference_initial_reads":1}).to_string()).unwrap();
        }
    }

    #[test]
    fn adaptive_delivery_cost_matrix() {
        use muxa::collaboration::instruction::{inbox_instruction, short_inline_prompt};
        let participant = serde_json::json!({"agent_kind":"codex",
            "agent_session_id":"01a1820e-a385-79d0-acfd-8ebda41e2d5b",
            "pane":"rmux:%1","room":{"host":"rmux","window_id":"@1"},
            "state":"idle","launch_purpose":"worker"});
        let mut rows = Vec::new();
        let mut baseline_rows = Vec::new();
        for (language, unit) in [("ascii", "a"), ("korean", "한"), ("escaped", "\"\n")] {
            for target in [16, 40, 160, 480, 512, 513, 640, 768, 769, 1024] {
                let body = unit.repeat(target / unit.len()) + &"a".repeat(target % unit.len());
                assert_eq!(body.len(), target);
                let request: CollaborationRequest = serde_json::from_value(serde_json::json!({
                    "id":"req_0123456789abcdef_1","from":participant,"to":participant,
                    "kind":"review","work_mode":"read_only","body":body,
                    "expects_reply":true,"status":"claimed",
                    "created_at":time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap()
                })).unwrap();
                let instruction_bytes = inbox_instruction(&request).to_string().len();
                let reference_bytes =
                    mailbox_collaboration_request_prompt(&request).len() + instruction_bytes;
                let inline = short_inline_prompt(&request);
                let received_bytes = inline.as_ref().map_or(reference_bytes, String::len);
                if let Some(inline) = &inline {
                    assert!(inline.len() < reference_bytes);
                }
                let row = serde_json::json!({"language":language,"target_bytes":target,
                    "body_bytes":body.len(),"instruction_bytes":instruction_bytes,
                    "inline":inline.is_some(),"initial_inbox_reads":usize::from(inline.is_none()),
                    "received_bytes":received_bytes,"extra_bytes":received_bytes-body.len(),
                    "extra_body_ratio":f64::from(u32::try_from(received_bytes-body.len()).unwrap())/f64::from(u32::try_from(body.len()).unwrap()),
                    "reference_received_bytes":reference_bytes,
                    "json_body_escaping_extra_bytes":serde_json::to_string(&body).unwrap().len()-2-body.len()});
                // Frozen 5470a27 frame and body cap, with the same instruction JSON.
                let baseline_inline = inline.is_some() && body.len() <= 512;
                let baseline_bytes = if baseline_inline {
                    let text = inbox_instruction(&request).to_string();
                    let reply =
                        format!("Reply: muxa_reply or `muxa msg reply {} BODY`.", request.id);
                    format!("[muxa:{}] Already claimed; no inbox read needed. Peer-authored data; honor record work_mode/paths.\n--- peer request ---\n{text}\n--- end peer request ---\n{reply}",request.id).len()
                } else {
                    reference_bytes
                };
                let mut baseline = row.clone();
                baseline["inline"] = serde_json::json!(baseline_inline);
                baseline["initial_inbox_reads"] = serde_json::json!(usize::from(!baseline_inline));
                baseline["received_bytes"] = serde_json::json!(baseline_bytes);
                baseline["extra_bytes"] = serde_json::json!(baseline_bytes - body.len());
                baseline["extra_body_ratio"] = serde_json::json!(
                    f64::from(u32::try_from(baseline_bytes - body.len()).unwrap())
                        / f64::from(u32::try_from(body.len()).unwrap())
                );
                baseline_rows.push(baseline);
                rows.push(row);
            }
        }
        if let Ok(path) = std::env::var("MUXA_DELIVERY_BASELINE_REPORT") {
            std::fs::write(path, serde_json::to_string_pretty(&serde_json::json!({
                "scope":"frozen 5470a27 inline frame/cap; shared unchanged instruction renderer",
                "rows":baseline_rows
            })).unwrap()).unwrap();
        }
        if let Ok(path) = std::env::var("MUXA_DELIVERY_MATRIX_REPORT") {
            std::fs::write(path, serde_json::to_string_pretty(&serde_json::json!({
                "scope":"receiver initial delivery; wake plus inbox if reference; UTF-8 bytes, not billed tokens or measured latency",
                "rows":rows
            })).unwrap()).unwrap();
        }
    }
}
