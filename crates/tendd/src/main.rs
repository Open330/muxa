//! tend daemon.
//!
//! Listens on a unix socket, ingests normalized `AgentEvent`s from adapters,
//! exposes query endpoints to the CLI and tmux status line, and — when
//! opted in via `[dashboard] enabled = true` or `--dashboard` — serves a
//! HTTP dashboard alongside the unix socket, with independently configurable
//! read and PAT-gated control access.

use anyhow::{Context, Result};
use clap::Parser;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tend::activity::{
    ActivityEntry, ActivityLog, ActivityOptions, StateTransitionEntry, StateTransitionInput,
};
use tend::ask::{AskOptions, AskStore};
use tend::automation::{
    subjects_from, AutomationAction, AutomationLedger, AutomationLedgerEntry, AutomationOutcome,
    AutomationRule, AutomationStore, AutomationSubject, Decision, PlannedFiring, Scheduler,
    SkipReason,
};
use tend::automation_judge::{self, AutomationJudgment, JudgmentContext, JudgmentDecision};
use tend::collaboration::{
    CollaborationClientKind, CollaborationOptions, CollaborationOriginMatch, CollaborationRequest,
    CollaborationStore, WakeDeliveryState,
};
use tend::collaboration_audit::CollaborationAuditLog;
use tend::config::{CollaborationWake, CollaborationWakePayload};
use tend::config::{DashboardAuthMode, NotifierBackend};
use tend::dashboard::{DashboardConfig, DashboardOverrides};
use tend::history::{HistoryOptions, PaneSessionCache, PromptHistory};
use tend::ipc::{harden_permissions, Client, RestartController, Server};
use tend::notify::Notifier;
use tend::pipeline_run::PipelineRunStore;
use tend::reconcile::Reconciler;
use tend::sinks::{webhook as webhook_sink, OhMyPromptSink, WebhookSink};
use tend::snapshot::{self, Snapshotter, SnapshotterOptions};
use tend::tmux::scanner::PaneCache;
use tend::{discovery, paths, Config, Store};
#[cfg(unix)]
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::broadcast;

mod binary_watch;
#[cfg(feature = "aas")]
mod swap_runner;
#[cfg(not(feature = "aas"))]
#[path = "swap_runner_off.rs"]
mod swap_runner;
use swap_runner::{open_swap_service, run_swap_operation, DaemonSwap};
mod fleet_manager;
mod herdr_bridge;
mod pipeline_reconciler;
mod screen_detect;
mod synthetic;

mod automation_engine;
use automation_engine::spawn_automation_task;
mod collaboration_waker;
use collaboration_waker::spawn_collaboration_waker_task;

/// Inactivity window before a stopped agent is evicted from the in-memory store.
const STOPPED_AGENT_TTL_MINUTES: i64 = 60;
/// Cadence at which the GC task scans for evictable agents.
const GC_SWEEP_INTERVAL_SECONDS: u64 = 60;
/// Spawn-archive, backup, and mailbox retention run at startup and hourly.
const STORAGE_SWEEP_INTERVAL_SECONDS: u64 = 60 * 60;
const PANE_SESSION_CACHE_INTERVAL_SECONDS: u64 = 5;
const SHUTDOWN_TASK_TIMEOUT_SECONDS: u64 = 2;
/// Slow safety-net scan for collaboration wake delivery. Normal delivery is
/// driven by mailbox revisions and agent transitions; this only reconciles a
/// rare dropped/closed transition or state restored during startup.
const COLLABORATION_WAKE_RECONCILE_SECONDS: u64 = 30;
/// Carries the daemon image identity across an in-place re-exec.
const RESTART_GENERATION_ENV: &str = "TEND_RESTART_GENERATION";
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
#[command(name = "tendd", version, about = "tend daemon")]
struct Args {
    /// Validate startup configuration and exit without side effects.
    #[arg(long, hide = true)]
    check_config: bool,
    /// Unix socket path. Overrides config and XDG default.
    #[arg(long, env = "TEND_SOCKET")]
    socket: Option<PathBuf>,

    /// Config file path. Defaults to `$XDG_CONFIG_HOME/tend/config.toml`.
    #[arg(long, env = "TEND_CONFIG")]
    config: Option<PathBuf>,

    /// Enable the HTTP dashboard. Requires a bearer token unless dashboard
    /// auth is explicitly set to read-only `none`.
    #[arg(long, conflicts_with = "no_dashboard")]
    dashboard: bool,

    /// Force the HTTP dashboard off, even if the config file enabled it.
    #[arg(long, conflicts_with = "dashboard")]
    no_dashboard: bool,

    /// Dashboard bind address as `ip:port`. Defaults to `127.0.0.1:7878`.
    #[arg(long, value_name = "ADDR", env = "TEND_DASHBOARD_BIND")]
    dashboard_bind: Option<String>,

    /// Dashboard bearer token / browser PAT. Required by `token` and
    /// `public_read` auth modes.
    #[arg(long, value_name = "TOKEN", env = "TEND_DASHBOARD_TOKEN")]
    dashboard_token: Option<String>,

    /// Dashboard API auth mode: `token`, `public_read`, or `none`.
    #[arg(long, value_name = "MODE", env = "TEND_DASHBOARD_AUTH")]
    dashboard_auth: Option<String>,

    /// Confirm that you want to bind the dashboard to a non-loopback
    /// address. Required when `dashboard.bind` is not `127.0.0.1` /
    /// `::1`.
    #[arg(long)]
    allow_public: bool,

    /// Append logs to this file instead of writing them to stderr. The file
    /// is opened shared, so a successor can append while its predecessor
    /// still holds it — what `tend daemon start` relies on on Windows, where
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
                .unwrap_or_else(|_| "tend=info".into()),
        )
        .with_writer(writer)
        // Disable ANSI escapes when stderr isn't a TTY. systemd-journald
        // captures the bytes verbatim; without this, `journalctl -u tendd`
        // is full of literal `\e[1m` debris. Same logic helps anything
        // grep-ing piped logs (the e2e tests included).
        .with_ansi(!to_file && std::io::stderr().is_terminal())
        .init();
    if to_file {
        // A panic message goes to stderr, which a file-logging daemon does
        // not keep; record it where the rest of the log is.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            tracing::error!(%info, "tendd panicked");
            previous(info);
        }));
    }

    let config_path = args.config.clone().or_else(paths::default_config_file);
    let mut cfg = Config::load_or_default(config_path.as_deref()).context("loading config")?;
    if let Some(path) = config_path.as_deref() {
        match tend::automation_defaults::install(path) {
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
    tracing::info!(
        socket = %socket.display(),
        generation = restart_generation(),
        boot_id = tend::ipc::daemon_boot_id(),
        "starting tendd"
    );
    let owns_data_dir = owns_default_data_dir(
        &socket,
        cfg.socket.as_deref(),
        args.config.as_deref(),
        paths::default_config_file().as_deref(),
    );
    if !owns_data_dir {
        tracing::info!(
            socket = %socket.display(),
            config = ?args.config,
            "socket or config override: skipping data-directory retention and the hook-spool drain, which belong to the default daemon"
        );
        // Nor does it watch the user's own tmux servers when it was given
        // its own `TMUX_TMPDIR`.
        tend::tmux::scanner::restrict_to_own_tmux_dir();
    }
    // A second daemon used to load the registry, start its snapshotter and
    // notifiers and heal tmux before its bind failed — and a KeepAlive
    // service manager restarted it every ten seconds for as long as the
    // other daemon lived, each pass rewriting state.json from its own
    // near-empty registry. Refuse before touching anything instead.
    if tend::transport::endpoint_is_live(&socket) {
        anyhow::bail!(
            "another tendd is already serving {}; not starting a second one (nothing was loaded or written)",
            socket.display()
        );
    }

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
            .with_upgrade_check(tend::binary_upgrade::UpgradeCheck::current()),
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
    // One Work store for IPC (the backlog) and the dashboard, so a save
    // from either never drops the other's records.
    let work_store = Arc::new(tokio::sync::Mutex::new(
        tend::dashboard::work_store::WorkStore::load(paths::default_dashboard_work_file()),
    ));

    // The set of backends this daemon observes simultaneously — tmux + herdr
    // during a migration (see `docs/MULTI_HOST.md`). Resolution honors
    // `TEND_HOSTS` > `TEND_HOST` > auto-detect and is never empty. Each element
    // is a cheap-to-clone `Arc`, so enumeration-shaped consumers (reconciler,
    // discovery, pane-session cache, history enrichment) iterate the whole set,
    // while the few consumers that genuinely need a single handle take the
    // `primary` (first) backend — or, for a host-specific consumer, the
    // matching backend from the set.
    let backends: Vec<tend::SharedBackend> = tend::active_backends();
    // Never empty (see `active_backends`); `primary` is the conventional
    // single-backend handle for consumers a namespace can't disambiguate.
    // `active_backends` orders the set by env preference, so `backends[0]` is
    // the env-preferred host (e.g. herdr when running herdr-inside-herdr) — the
    // same host the old single-backend daemon detected. Consumers that must
    // match that legacy single-host behavior (the web dashboard scanner) take
    // `primary`.
    let primary: tend::SharedBackend = backends[0].clone();
    let sessions = tend::PtySessionBackend::shared();
    let observing_kinds: Vec<tend::HostKind> = backends.iter().map(|b| b.kind()).collect();
    tracing::info!(hosts = ?observing_kinds, "pane backends selected");
    // One shared, short-lived `list_panes` per host for the periodic
    // observers (screen detection, pane→session cache, periodic discovery).
    // Request paths, Fleet (whose published snapshot must never move back
    // behind a fresh refresh) and the reconciler keep `backends`.
    let inventory = tend::backend::inventory::InventoryBackend::share(
        &backends,
        tend::backend::inventory::DEFAULT_INVENTORY_TTL,
    );
    refresh_pane_session_cache(&pane_session_cache, &inventory).await;
    spawn_pane_session_cache_task(pane_session_cache.clone(), inventory.clone(), &shutdown_tx);

    // Rehydrate the agent registry from the previous run's snapshot, if
    // any. Done before the IPC server starts accepting events so no
    // adapter ingest can race a half-loaded store; before discovery so
    // its `already_known` filter sees the hydrated entries and skips
    // panes we already have rich state for; before the snapshotter
    // spawns so the first save isn't a no-op overwrite of the file we
    // just read.
    hydrate_state(&cfg, &store).await;
    // Hook events that failed to ingest while this daemon was down or
    // incompatible. Replayed into the registry now, before any producer task
    // or the IPC server runs, so no live event can interleave with it.
    if owns_data_dir {
        drain_hook_spool(&store).await;
    }
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
        collaboration.clone(),
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
    let gc_handle = spawn_gc_task(
        &store,
        &collaboration,
        cfg.storage.backups_keep,
        owns_data_dir,
        &shutdown_tx,
    );
    let directory_max_age = if cfg.reconciler.enabled {
        cfg.reconciler.interval_secs.saturating_add(2)
    } else {
        2
    };
    let agent_directory = Arc::new(tend::agent_directory::AgentDirectory::with_max_age(
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
    // Translates herdr's own agent-state detection into synthetic tend rows so
    // agents tend has no hooks for still appear in status/watch/stats. Spawned
    // before the IPC server takes ownership of `store` so it shares the same
    // registry.
    let herdr_bridge_handle =
        herdr_bridge::spawn_herdr_bridge_task(&backends, store.clone(), &shutdown_tx);
    // herdr reverse path: push tend's authoritative hook-derived state for REAL
    // (non-synthetic) `herdr:` rows back into herdr's UI via `pane.report_agent`,
    // releasing authority when the row stops. Subscribes to the same store
    // transition stream the notifier/activity tasks use. Spawned when herdr ∈ set.
    let herdr_report_handle =
        herdr_bridge::spawn_herdr_report_task(&backends, store.clone(), &shutdown_tx);
    // Screen-manifest fallback detection: for agent CLIs tend has no hooks for
    // (cursor-agent, amp, copilot, …), capture matching panes and classify their
    // screen into synthetic rows. Skips herdr hosts (the herdr bridge covers
    // them) and any pane a live hook owns. Spawned before the IPC server takes
    // ownership of `store` so it shares the same registry.
    let screen_detect_handle =
        screen_detect::spawn_screen_detect_task(&cfg, &inventory, store.clone(), &shutdown_tx);
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
    // Daemon-owned swap jobs: automation swaps and `tend swap` requests
    // run through them. Without them no swap runs at all — there is no
    // unjournalled path to fall back to.
    // The account module opens only when this config turns it on
    // (`[modules] accounts`, or settings that already use accounts).
    let accounts_module = cfg.accounts_module();
    let opened_swap = if cfg!(feature = "aas") && !accounts_module.enabled {
        tracing::info!("account module not enabled; swap jobs are off");
        Err(tend::swap_api::module_unavailable(true))
    } else {
        if !accounts_module.uses.is_empty() {
            tracing::info!(uses = ?accounts_module.uses, "account module on: this config uses accounts");
        }
        open_swap_service(
            store.clone(),
            backends.clone(),
            cfg.swap.clone(),
            cfg.accounts.clone(),
            config_path.clone(),
            collaboration.clone(),
        )
        .await
    };
    // What IPC serves: the service, or the reason it did not open, so a
    // health check can still say why.
    let swap_jobs: Arc<dyn tend::swap_api::SwapJobs> = match &opened_swap {
        Ok(swap) => swap_runner::jobs(swap),
        Err(reason) => Arc::new(tend::swap_api::UnopenedSwap::new(
            reason.clone(),
            swap_runner::JOB_SCHEMA,
        )),
    };
    let swap_service = opened_swap.ok();
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
        let dashboard_runtime = tend::dashboard::DashboardRuntimeConfig {
            collaboration: Some(collaboration.clone()),
            message_skills: cfg.message.skills.clone(),
            activity_path: dashboard_activity_path,
            session_activity_path: dashboard_session_activity_path,
            work_store_path: paths::default_dashboard_work_file(),
            work_store: Some(work_store.clone()),
            stats_config: dashboard_stats_config,
            fleet: Some(fleet_runtime.clone()),
        };
        tokio::spawn(async move {
            if let Err(e) = tend::dashboard::serve(
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
        .find(|b| b.kind() == tend::HostKind::Zellij)
        .cloned()
        .unwrap_or_else(|| primary.clone());
    // WS-F: snapshot
    let mux_snapshots = tend::mux_snapshot_control::MuxSnapshotControl::new(socket.clone());
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
        .with_reviews(build_reviews())
        .with_work_store(work_store)
        .with_mux_snapshots(mux_snapshots) // WS-F: snapshot
        .with_swap(Some(swap_jobs))
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
    // delegates physical launch to the sibling `tend` CLI over this IPC path.
    // Its initial authoritative scan still catches completions committed
    // between server start and subscription.
    let pipeline_reconciler_handle =
        pipeline_reconciler::spawn(pipeline_runs, socket.clone(), &shutdown_tx);

    // Self-heal: if a tmux server is already running, inject our socket
    // path into its environment so that every pane — including any that
    // were started before this tendd instance — can reach us. This
    // complements the `tmux-env` managed block in `~/.tmux.conf` which
    // handles fresh server boots; together they cover both cold-start and
    // warm-restart scenarios without requiring the user to re-run
    // `tend init` after every daemon or tmux server restart.
    if listener_ready {
        maybe_heal_tmux_socket_env(&socket, cfg.socket.as_deref());
        spawn_startup_discovery(&cfg, socket.clone(), backends.clone(), &shutdown_tx);
        spawn_periodic_discovery(&cfg, socket.clone(), inventory.clone(), &shutdown_tx);
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
    // Untouched targets are durably deferred even if an active target does
    // not finish in time. Only untouched public targets resume on startup;
    // started targets still require reconciliation and are never replayed. Budget: IPC drain (5 s) +
    // this + runtime teardown (2 s) stays under launchd's 20 s; the 2 s
    // per-task timeouts below are a worst-case ceiling on top that only
    // hung tasks reach.
    if let Some(swap) = &swap_service {
        if !swap_runner::drain(swap, SWAP_DRAIN_ON_SHUTDOWN).await {
            tracing::warn!(
                "swap targets still active at shutdown; started targets will need reconciliation, untouched public targets will resume on the next start"
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
/// incremented for each successful self-reexec. It is not a start identity —
/// an external stop/start is back at zero — so `hello` also carries
/// `tend::ipc::daemon_boot_id()`, which every process image (a re-exec
/// included) draws afresh.
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
fn reexec_self(approved: Option<tend::binary_upgrade::ValidatedLaunch>) -> std::io::Error {
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
            .then(|| cfg.ask.path.clone().or_else(tend::paths::default_ask_file))
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
            store.set_turn_continuation(cfg.collaboration.wake != CollaborationWake::Never);
            Ok(store)
        }
        Err(error @ tend::collaboration::CollaborationError::UnsupportedSchema(_)) => {
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
            tracing::info!(path = %path.display(), max_bytes = cfg.storage.audit_max_bytes, "collaboration audit enabled");
            return CollaborationAuditLog::at_path_with_max_bytes(
                path,
                cfg.storage.audit_max_bytes,
            );
        }
    }
    CollaborationAuditLog::in_memory()
}

// WS-F: snapshot
/// Automatic workspace snapshots (`[snapshot]`, on by default every 15
/// minutes). The first one waits a full interval: right after a reboot the
/// multiplexer is empty or half rebuilt, and that is the workspace nobody
/// wants as the newest snapshot. `tend snapshot --auto` itself skips an
/// unchanged workspace and prunes old automatic snapshots.
fn spawn_mux_snapshot_task(
    config: &tend::config::SnapshotConfig,
    control: Arc<tend::mux_snapshot_control::MuxSnapshotControl>,
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
    store: tend::SharedStore,
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
    state: tend::AgentState,
) {
    let status = match state {
        tend::AgentState::WaitingInput | tend::AgentState::WaitingChoice => {
            tend::pipeline_run::PipelineAliasStatus::Blocked
        }
        tend::AgentState::Error | tend::AgentState::Stopped => {
            tend::pipeline_run::PipelineAliasStatus::Failed
        }
        tend::AgentState::Starting | tend::AgentState::Working | tend::AgentState::Idle => {
            tend::pipeline_run::PipelineAliasStatus::Running
        }
    };
    if let Err(error) = pipeline_runs.observe_pane(pane, status).await {
        tracing::warn!(pane, %error, "could not project agent state into pipeline Run");
    }
}

/// Resolve `[automation]` into a live store. The ledger is materialized
/// unconditionally — the guards read from it, and an operator who adds a
/// rule at runtime should not have to restart the daemon to get one.
/// The review queue. A store that cannot open is served as unavailable, so
/// a broken review file never keeps tendd from starting.
fn build_reviews() -> tend::review::ReviewStore {
    let Some(path) = tend::review::ReviewStore::default_path() else {
        tracing::warn!("no data directory; review items are unavailable");
        return tend::review::ReviewStore::unavailable("no data directory");
    };
    tend::review::ReviewStore::open(&path).unwrap_or_else(|error| {
        tracing::warn!(path = %path.display(), %error, "review store unavailable");
        tend::review::ReviewStore::unavailable(error.to_string())
    })
}

fn build_automation(cfg: &Config, config_path: Option<PathBuf>) -> Arc<AutomationStore> {
    let ledger = AutomationLedger::load(tend::automation::default_automation_file());
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

type WorkerIoGuard = tend::prompt_delivery::PaneInputGuard;

/// One raw injection through the backend that owns the pane's id namespace,
/// holding `io_guard` until the keys are written. Enter and Ctrl-C go
/// literally; anything else is typed as prompt text. Callers that type and
/// then submit a whole prompt use `tend::prompt_delivery` instead.
async fn send_pane_text(
    backends: &[tend::SharedBackend],
    pane: &str,
    socket: Option<&str>,
    text: &str,
    io_guard: Option<WorkerIoGuard>,
) -> bool {
    let Some(backend) = text_backend(backends, pane) else {
        return false;
    };
    let pane = pane.to_string();
    let socket = socket.map(str::to_string);
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

/// The backend that owns `pane`'s id namespace, if it can take text.
fn text_backend(backends: &[tend::SharedBackend], pane: &str) -> Option<tend::SharedBackend> {
    let kind = tend::backend::pane_id_host_kind(pane)?;
    backends
        .iter()
        .find(|backend| backend.kind() == kind && backend.caps().send_text)
        .cloned()
}

/// Spawn the GC task: evicts long-stopped agents on a periodic timer.
///
/// Listens to the shutdown broadcast so a clean SIGTERM tears it down
/// rather than relying on the runtime falling out from under it.
fn spawn_gc_task(
    store: &tend::SharedStore,
    collaboration: &Arc<CollaborationStore>,
    backups_keep: usize,
    owns_data_dir: bool,
    shutdown_tx: &broadcast::Sender<()>,
) -> tokio::task::JoinHandle<()> {
    let store = store.clone();
    let collaboration = collaboration.clone();
    let ttl = time::Duration::minutes(STOPPED_AGENT_TTL_MINUTES);
    let tick = std::time::Duration::from_secs(GC_SWEEP_INTERVAL_SECONDS);
    let storage_tick = std::time::Duration::from_secs(STORAGE_SWEEP_INTERVAL_SECONDS);
    let mut shutdown_rx = shutdown_tx.subscribe();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tick);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_storage_sweep: Option<tokio::time::Instant> = None;
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    // Directory and mailbox retention move slowly; an hourly
                    // pass is enough and keeps the per-minute tick to the
                    // in-memory registry sweep. It runs before eviction so
                    // its snapshot still sees rows about to be swept.
                    if last_storage_sweep.is_none_or(|at| at.elapsed() >= storage_tick) {
                        last_storage_sweep = Some(tokio::time::Instant::now());
                        sweep_storage(&store, &collaboration, backups_keep, owns_data_dir).await;
                    }
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

/// Hourly storage retention: spawn archives, `backups/`, and expired
/// mailbox threads. Each step logs and continues on failure.
///
/// The two data-directory sweeps run only when `owns_data_dir`: they act on
/// whatever `$XDG_DATA_HOME` resolves to, which for a daemon started on a
/// throwaway socket or config (a test, a demo) is still the operator's
/// real one. Mailbox retention follows the mailbox this daemon opened.
async fn sweep_storage(
    store: &tend::SharedStore,
    collaboration: &Arc<CollaborationStore>,
    backups_keep: usize,
    owns_data_dir: bool,
) {
    match collaboration.apply_retention().await {
        Ok(removed) if removed > 0 => {
            tracing::info!(removed, "collaboration retention pruned expired threads");
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(%error, "collaboration retention sweep failed"),
    }
    if !owns_data_dir {
        return;
    }
    if let Some(directory) = tend::paths::default_backups_dir() {
        let result = tokio::task::spawn_blocking(move || {
            tend::retention::prune_backups(&directory, backups_keep)
        })
        .await;
        match result {
            Ok(Ok(removed)) if !removed.is_empty() => tracing::info!(
                removed = removed.len(),
                keep = backups_keep,
                "storage retention pruned old backups"
            ),
            Ok(Err(error)) => tracing::warn!(%error, "backup retention sweep failed"),
            Err(error) => tracing::warn!(%error, "backup retention sweep task failed"),
            _ => {}
        }
    }
    // Snapshot first: open tickets and live rows protect launch archives,
    // including launches that have not registered an agent yet.
    let agents = store.snapshot().await;
    let mut protected: std::collections::BTreeSet<String> = collaboration
        .worker_spawn_records()
        .await
        .into_iter()
        .filter(|spawn| {
            spawn.closed_at.is_none()
                && spawn.phase != tend::collaboration::WorkerSpawnPhase::Failed
        })
        .map(|spawn| spawn.launch_id)
        .collect();
    protected.extend(store.protected_launch_archives());
    if let Some(directory) = tend::launch_identity::archive_directory() {
        let result = tokio::task::spawn_blocking(move || {
            tend::launch_identity::prune_archives(
                &directory,
                &agents,
                &protected,
                std::time::SystemTime::now(),
            )
        })
        .await;
        match result {
            Ok(Ok(removed)) if removed > 0 => tracing::debug!(removed, "gc swept launch archives"),
            Ok(Err(error)) => tracing::warn!(%error, "launch archive sweep failed"),
            Err(error) => tracing::warn!(%error, "launch archive sweep task failed"),
            _ => {}
        }
    }
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
            // one console stop a supervisor can aim at tendd alone.
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
/// to `$XDG_DATA_HOME/tend/activity.ndjson` so stats can compute duration
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
async fn refresh_pane_session_cache(cache: &PaneSessionCache, backends: &[tend::SharedBackend]) {
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
            tend::tmux::canonical_panes(&panes)
                .into_iter()
                .map(|pane| (pane.pane_id, pane.session)),
        );
    }
    cache.replace(entries);
}

fn spawn_pane_session_cache_task(
    cache: PaneSessionCache,
    backends: Vec<tend::SharedBackend>,
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
    store: &tend::SharedStore,
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
    states: &mut HashMap<String, (tend::AgentState, time::OffsetDateTime)>,
    transition: tend::state::Transition,
) {
    // An Idle -> Idle broadcast only carries a changed `awaiting_reply`; the
    // activity log records states.
    if transition.from == transition.to {
        return;
    }
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
    agents: Vec<tend::Agent>,
) -> HashMap<String, (tend::AgentState, time::OffsetDateTime)> {
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
    store: &tend::SharedStore,
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
/// synthetic placeholders, and collapses duplicate rows so `tend watch`
/// and `tend status` stay in sync with the user's actual tmux state.
fn spawn_reconciler_task(
    cfg: &Config,
    store: &tend::SharedStore,
    shutdown_tx: &broadcast::Sender<()>,
    backends: Vec<tend::SharedBackend>,
    sessions: tend::session::SharedSessionBackend,
    agent_directory: Arc<tend::agent_directory::AgentDirectory>,
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
        tend::adapters::codex_rollout::default_sessions_root()
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
/// Without this, installing a new tend leaves the old daemon serving: the
/// package manager writes the new build, the running process keeps its open
/// inode, and no service manager intervenes because nothing exited. The gap
/// closes only when someone notices and restarts it by hand — which, measured
/// on a live host, took six days.
///
/// The daemon re-execs itself rather than exiting for a supervisor to catch,
/// so this works the same whether tendd was started by launchd, systemd, or a
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

/// Track cumulative session foreground time for `tend watch --view session`.
/// The sampling source follows the active pane backend: tmux (and the zellij
/// fallback) shells out to `list-clients`; herdr queries the focused
/// workspace over its socket. All downstream accounting is shared.
fn spawn_session_activity_task(
    cfg: &Config,
    shutdown_tx: &broadcast::Sender<()>,
    activity_log: Option<std::sync::Arc<ActivityLog>>,
    backends: &[tend::SharedBackend],
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
            tend::HostKind::Tmux => sources.push(tend::SessionActivitySource::Tmux),
            tend::HostKind::Rmux => sources.push(tend::SessionActivitySource::Rmux),
            tend::HostKind::Herdr => sources.push(tend::SessionActivitySource::Herdr {
                socket_path: tend::backend::herdr::default_socket_path(),
            }),
            tend::HostKind::Cmux | tend::HostKind::Zellij => {}
        }
    }
    let source_kinds: Vec<tend::HostKind> = backends
        .iter()
        .map(|b| b.kind())
        .filter(|kind| {
            matches!(
                kind,
                tend::HostKind::Tmux | tend::HostKind::Rmux | tend::HostKind::Herdr
            )
        })
        .collect();
    let tracker = tend::SessionActivityTracker::new(
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
/// produces one disk write. Survives daemon restarts so `tend watch`
/// rehydrates with real `session_id`s, `last_prompt`s, and full
/// state/metadata instead of synthetic placeholders.
fn spawn_snapshotter_task(
    cfg: &Config,
    store: &tend::SharedStore,
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
        activity_interval: tend::snapshot::DEFAULT_ACTIVITY_INTERVAL,
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
async fn hydrate_state(cfg: &Config, store: &tend::SharedStore) {
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
    store: &tend::SharedStore,
    history: &Arc<PromptHistory>,
    backends: &[tend::SharedBackend],
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
    let panes = tend::tmux::canonical_panes(&panes);
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
        tokio::task::spawn_blocking(move || tend::process_tree::scan_codex_panes(&scanned))
            .await
            .map(|scan| {
                scan.resume_bindings
                    .into_iter()
                    .map(|binding| (binding.session_id, binding.pane.pane_id))
                    .collect()
            })
            .unwrap_or_default()
    };

    let mut candidates: Vec<tend::Agent> = Vec::new();
    for pane in &panes {
        let has_real = snapshot.iter().any(|a| {
            a.pane.as_deref() == Some(pane.pane_id.as_str())
                && !a
                    .session_id
                    .starts_with(tend::state::SYNTHETIC_SESSION_PREFIX)
                && a.state != tend::AgentState::Stopped
        });
        if has_real {
            continue;
        }
        let recents = history.recent_for_pane(&pane.pane_id, 1).await;
        let Some(entry) = recents.into_iter().next() else {
            continue;
        };
        if entry.kind == tend::AgentKind::Codex
            && !codex_resumes.contains(&(entry.session_id.clone(), pane.pane_id.clone()))
        {
            continue;
        }
        candidates.push(tend::Agent {
            ask_origin: None,
            kind: entry.kind,
            session_id: entry.session_id,
            surface: None,
            pane: Some(pane.pane_id.clone()),
            tmux_socket: pane.socket.clone(),
            tmux_session: Some(pane.session.clone()),
            cwd: entry.cwd,
            pid: None,
            workload: tend::WorkloadSummary::default(),
            subagents: Vec::new(),
            state: tend::AgentState::Idle,
            last_prompt: Some(entry.prompt),
            last_prompt_at: Some(entry.at),
            last_response: None,
            recap: None,
            ai_title: None,
            pending_command: None,
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
            state_source: None,
            started_at: entry.at,
            last_activity_at: entry.at,
            state_entered_at: entry.at,
            last_prompt_origin: None,
            last_event_at: None,
            last_response_at: None,
            awaiting_reply: None,
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
/// already folds `TEND_DASHBOARD_*` envs into the parsed args (via the
/// `env =` attribute), so for those fields env-beats-flag is enforced
/// at parse time — by the time we see them in `args`, the env value
/// has already won.
fn resolve_dashboard_config(cfg: &Config, args: &Args) -> Result<DashboardConfig> {
    let enabled = if args.dashboard {
        Some(true)
    } else if args.no_dashboard {
        Some(false)
    } else {
        std::env::var("TEND_DASHBOARD_ENABLED").ok().and_then(|s| {
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
        std::env::var("TEND_DASHBOARD_ALLOW_PUBLIC")
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
    store: &tend::SharedStore,
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
    store: &tend::SharedStore,
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

/// Inject `TEND_SOCKET` into an already-running tmux server's global
/// environment. Called once at daemon startup so panes that were
/// spawned before this tendd instance can still reach it via hooks.
fn heal_tmux_socket_env(socket: &std::path::Path) {
    let server_up = tend::tmux::tmux_command()
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
    match tend::tmux::tmux_command()
        .args(["set-environment", "-g", "TEND_SOCKET", s])
        .status()
    {
        Ok(st) if st.success() => {
            tracing::info!(socket = s, "healed TEND_SOCKET in tmux server env");
        }
        Ok(st) => {
            tracing::warn!(
                socket = s,
                status = ?st.code(),
                "tmux set-environment TEND_SOCKET failed"
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not run tmux set-environment");
        }
    }
}

/// Heal the tmux global `TEND_SOCKET` — but only for the canonical
/// daemon (see [`should_heal_tmux_socket_env`]). A non-canonical instance
/// (an explicit `--socket`/`TEND_SOCKET` override matching neither the
/// default nor the configured socket — a dashboard demo, an e2e test, a
/// throwaway `tendd --socket /tmp/…`) logs and returns without touching
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

/// Whether this tendd instance may write its socket into the tmux
/// server's global `TEND_SOCKET` (see [`heal_tmux_socket_env`]).
///
/// Only the *canonical* daemon heals: one on the XDG/default socket, or
/// on the socket named in config. An instance started with an explicit
/// `--socket` / `TEND_SOCKET` override matching neither — a dashboard
/// demo, an e2e test, a throwaway `tendd --socket /tmp/…` — must NOT
/// touch the global env. That env (and `tend init`'s tmux.conf pin)
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

/// Whether this tendd is the default daemon — on the canonical socket (see
/// [`should_heal_tmux_socket_env`]) and the default config file — and so
/// owns the data directory's retention and hook spool. A daemon given its
/// own `--socket`/`TEND_SOCKET` or `--config`/`TEND_CONFIG` (e2e tests,
/// the onboarding sandbox, demos) must not prune or drain them: without an
/// `XDG_DATA_HOME` override they are the operator's real files.
fn owns_default_data_dir(
    socket: &Path,
    cfg_socket: Option<&Path>,
    config: Option<&Path>,
    default_config: Option<&Path>,
) -> bool {
    let same = |a: &Path, b: &Path| {
        a == b
            || matches!(
                (std::fs::canonicalize(a), std::fs::canonicalize(b)),
                (Ok(a), Ok(b)) if a == b
            )
    };
    should_heal_tmux_socket_env(socket, &paths::default_socket(), cfg_socket)
        && config.is_none_or(|config| default_config.is_some_and(|default| same(config, default)))
}

/// Decide whether to fire the one-shot discovery pass and, if so, spawn it
/// onto the current tokio runtime.
///
/// Returns `true` when a task was spawned. Extracted from `main` so tests
/// can drive both branches of the `discovery.enabled` flag without having
/// to spawn the real daemon.
/// Drain the hook spool once at startup. See [`tend::hook_spool`].
///
/// Replay restores agent state through the same registry entry point the
/// ingest handler uses (scope check, `apply_with_ask`, launch claim). The
/// collaboration worker binding and paneless-Codex correlation are left to
/// the agent's next live hook and the reconciler.
async fn drain_hook_spool(store: &tend::SharedStore) {
    let Some(path) = tend::hook_spool::default_spool_file() else {
        return;
    };
    let (entries, unreadable) = tokio::task::spawn_blocking(move || tend::hook_spool::take(&path))
        .await
        .unwrap_or_default();
    if entries.is_empty() && unreadable == 0 {
        return;
    }
    let latest: HashMap<String, time::OffsetDateTime> = store
        .snapshot()
        .await
        .into_iter()
        .map(|agent| (agent.session_id, agent.last_activity_at))
        .collect();
    let (replay, mut report) = tend::hook_spool::plan_replay(
        entries,
        unreadable,
        time::OffsetDateTime::now_utc(),
        |session| latest.get(session).copied(),
    );
    for entry in replay {
        let event = &entry.event;
        let pane_host = event
            .id()
            .pane
            .as_deref()
            .and_then(tend::backend::pane_id_host_kind);
        let in_scope = pane_host.is_some_and(|host| host != tend::HostKind::Tmux)
            || tend::tmux::scanner::event_tmux_socket_in_scope(event.id().tmux_socket.as_deref());
        if !in_scope {
            report.skipped += 1;
            continue;
        }
        store.apply_with_ask(event, None).await;
        store
            .observe_launch_claim(event, entry.launch_id.as_deref())
            .await;
        report.replayed += 1;
    }
    tracing::info!(
        spooled = report.total(),
        replayed = report.replayed,
        skipped = report.skipped,
        protocol_mismatch = report
            .failures
            .get(&tend::hook_spool::IngestFailure::ProtocolMismatch)
            .copied()
            .unwrap_or(0),
        "drained hook events that failed to ingest"
    );
    if let Some(report_path) = tend::hook_spool::default_drain_report_file() {
        let result = tokio::task::spawn_blocking(move || {
            tend::hook_spool::write_report(&report_path, &report)
        })
        .await;
        if !matches!(result, Ok(Ok(()))) {
            tracing::warn!("hook spool drain report could not be written");
        }
    }
}

fn spawn_startup_discovery(
    cfg: &Config,
    socket: PathBuf,
    backends: Vec<tend::SharedBackend>,
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
    backends: &[tend::SharedBackend],
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
/// keeps newly-created panes appearing in `tend status` within
/// `[discovery] interval_secs` instead of only after the agent's first hook.
/// The pass uses one `tmux list-panes` and, for wrapper foreground commands,
/// a single bounded process-table snapshot.
/// No-op when discovery is disabled or `interval_secs == 0`.
fn spawn_periodic_discovery(
    cfg: &Config,
    socket: PathBuf,
    backends: Vec<tend::SharedBackend>,
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
mod test_support;

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
