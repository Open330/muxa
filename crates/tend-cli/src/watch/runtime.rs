use super::{
    append_human_interaction, apply_outcome_to_app, apply_rename, ask_revision_updates,
    board_message_stale, board_screen, board_window, build_watch_topology, capture_tmux_window,
    clamp_active_message_skill, clamp_collaboration_mailbox, collab_screen, create_window,
    disable_raw_mode, dispatch_quick_action, drain_pending_events, enable_raw_mode, execute,
    finish_board_message, finish_rename, focus_active_message_skill, handle_event, icons_unicode,
    io, is_ask_running, is_many_pane, mpsc, open_preview_for, open_watch_collaboration_composer,
    open_watch_collaboration_mailbox, open_work_up_window, pane_capture_target,
    pane_work_display_name, pane_work_group_key, persist_watch_collab_layout,
    persist_watch_collaboration_defaults, persist_watch_inspector_split, persist_watch_sort,
    persist_watch_value, probe_version_skew, refresh_ask_entries, refresh_board_collaboration,
    refresh_fleet_collaboration, refresh_selected_collaboration_detail,
    refresh_watch_collaboration, refuse_busy, remove_message_skill, render,
    restart_daemon_from_watch, run_watch_collaboration_composer, selected_move_target,
    session_group_key, short_collaboration_request_id, start_quick_in_background,
    sync_collab_screen_from_cache, take_finished_quick, toggle_collaboration_mark,
    upsert_message_skill, visible_ask_entries, watch_collaboration_origin, Action, ActionOutcome,
    Agent, AgentKind, AgentState, App, AskComposer, BTreeMap, Backend, BackendEndpoint,
    BoardPending, CapturedPane, CapturedWindow, Client, CollaborationMailboxTab, ConfirmAction,
    CrosstermBackend, DaemonError, DisableBracketedPaste, DisableMouseCapture, Duration,
    EnableBracketedPaste, EnableMouseCapture, EnterAlternateScreen, Event, Future, HashMap,
    HintLevel, HumanInteractionKind, LeaveAlternateScreen, MissedTickBehavior, OffsetDateTime,
    PaneInfo, PathBuf, PreviewContent, PreviewMode, PreviewState, QuickAction, RealEffects, Result,
    RuntimeError, SessionActivity, SessionInfo, SortContext, Stdout, Terminal, WatchConfig,
    WatchLayout, WatchOpenTarget, WatchRow, WatchScreen, WatchView, WindowKey, WorkComposer,
    WorkRow, BOARD_COLLAB_INTERVAL, BOARD_REFRESH_INTERVAL, BOARD_STATS_INTERVAL,
    BOARD_WORKERS_INTERVAL, BOARD_WORKER_IDLE_SECS, IDLE_REDRAW_INTERVAL, INPUT_POLL,
    OUTCOME_CAPACITY, POLL_INTERVAL, STREAMING_FALLBACK_INTERVAL, SWARM_REDRAW_INTERVAL,
    TREE_REDRAW_INTERVAL, WAKE_CAPACITY, WINDOW_CAPTURE_INTERVAL,
};

/// Restore the terminal to a sane state on drop.
///
/// We take ownership of the `Terminal` here so that panics during the
/// render loop still run this destructor via stack unwinding.
pub(super) struct TerminalGuard<B: Backend + io::Write> {
    terminal: Option<Terminal<B>>,
}

impl<B: Backend + io::Write> TerminalGuard<B> {
    pub(super) fn new(terminal: Terminal<B>) -> Self {
        Self {
            terminal: Some(terminal),
        }
    }

    pub(super) fn terminal_mut(&mut self) -> &mut Terminal<B> {
        self.terminal.as_mut().expect("terminal present")
    }
}

impl<B: Backend + io::Write> Drop for TerminalGuard<B> {
    fn drop(&mut self) {
        if let Some(mut t) = self.terminal.take() {
            let _ = disable_raw_mode();
            let _ = execute!(
                t.backend_mut(),
                LeaveAlternateScreen,
                DisableMouseCapture,
                DisableBracketedPaste
            );
            let _ = t.show_cursor();
        }
    }
}

pub(super) fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Bracketed paste lets the terminal wrap pasted content in escape
    // markers so crossterm delivers it as a single `Event::Paste(String)`
    // — including any embedded newlines — instead of a stream of key
    // events where a pasted `\n` is indistinguishable from a real Enter.
    // The prompt composer relies on this to keep a multi-line paste from
    // submitting itself at the first newline.
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

/// One result of a refresh handed back to the main loop. Two
/// shapes:
///
/// - `Full` — emitted by the periodic fallback tick (and the very
///   first refresh): full list of agents + pane inventory, replacing
///   the App's data. Catches up after `Lagged` drops on the
///   broadcast or when the stream reconnects.
/// - `SingleAgent` — emitted on every push from the subscribe
///   stream, carrying just the post-transition `Agent` payload. The
///   App applies it in place to the matching `(kind, session_id)`
///   row instead of replacing the whole list.
///
/// Routing transitions through `SingleAgent` is what stops the UI
/// from "redrawing every row on every tick": only the one row that
/// actually transitioned is touched. The fallback tick re-syncs
/// everything else periodically.
// `Full` is materially larger than `SingleAgent` (a Vec<Agent> + a
// Vec<PaneInfo>). Boxing the variant would make the enum smaller but
// adds an allocation per Full message; we send Full at most once per
// 5 s, so the size disparity is benign — silence clippy's
// large_enum_variant lint at the type definition.
#[allow(clippy::large_enum_variant)]
pub(crate) enum RefreshOutcome {
    Full(FullRefresh),
    // Retained for incremental-update regression coverage.
    #[allow(dead_code)]
    SingleAgent(Agent),
    Agents(Vec<Agent>),
}

pub(crate) struct FullRefresh {
    pub agents: Vec<Agent>,
    pub panes: Vec<PaneInfo>,
    pub sessions: Vec<SessionInfo>,
    pub session_activity: Vec<SessionActivity>,
    pub error: Option<DaemonError>,
    pub pipeline_runs: Vec<tend::pipeline_run::PipelineRun>,
}

pub(crate) struct MessageComposerConfig {
    pub skills: BTreeMap<String, String>,
    pub collaboration_scope: tend::config::CollaborationScope,
}

/// Apply a `RefreshOutcome` to `App`.
///
/// **Anti-flicker merge invariant** (added 2026-04-30): when a fresh
/// snapshot lands carrying a row whose state is `Starting` for an
/// `(kind, session_id)` we already track in a steady state
/// (`Working` / `Idle` / `WaitingInput` / `WaitingChoice` / `Error`
/// / `Stopped`), we
/// keep the previously-known state and only adopt the snapshot's
/// non-state fields (model, cost, `last_prompt`, …). Background:
///
///   - `state::Agent::new` initializes new entries to `Starting`,
///     and `Store::apply` only flips off `Starting` for events that
///     carry an explicit transition (`Started` → `Idle`,
///     `PromptSubmitted` → `Working`, etc.). Events that *don't*
///     change state (e.g. `ToolCompleted` against an `Idle` row, or
///     a `Heartbeat`) leave a freshly-inserted entry stuck in
///     `Starting` — and the v0.5.0 `Subscribe` push triggers a fresh
///     snapshot fetch on every transition, so any such transient
///     placeholder shows up in `tend watch` as a single-tick row
///     that looks like "everything is Starting" relative to its
///     peers.
///   - The user-visible symptom: "한 순간에 STATE가 모두 starting으로
///     바뀌고 업데이트되는것같다" — the eye doesn't track which one row
///     blipped, only that the column "flickered".
///
/// The merge keeps Agent identity (matched by `(kind, session_id)`)
/// stable across refreshes, so `set_data`'s sort+selection logic
/// runs on a list that already reflects the row-level UI invariant.
/// New rows are appended (the snapshot's order is preserved before
/// `set_data` re-sorts). Gone rows simply don't appear in the new
/// snapshot and so drop.
pub(crate) fn apply_outcome(app: &mut App, outcome: RefreshOutcome) {
    apply_outcome_inner(app, outcome);
    // Diff the freshly-applied states against the prior snapshot to arm any
    // done/error transition pulses.
    app.detect_pulses();
}

fn apply_outcome_inner(app: &mut App, outcome: RefreshOutcome) {
    match outcome {
        RefreshOutcome::Full(full) => apply_full(app, full),
        RefreshOutcome::Agents(agents) => {
            apply_full(
                app,
                FullRefresh {
                    agents,
                    panes: app.panes.clone(),
                    sessions: app.sessions.clone(),
                    session_activity: app.session_activity.clone(),
                    pipeline_runs: app.pipeline_runs.clone(),
                    error: app.last_error.clone(),
                },
            );
        }
        RefreshOutcome::SingleAgent(agent) => {
            let selected = app.selection_identity_for_rebuild();
            apply_single_agent(app, agent);
            app.topology = build_watch_topology(
                &app.current_agents(),
                &app.panes,
                &app.sessions,
                &app.pipeline_runs,
            );
            // Re-sort immediately so a pushed state/activity change moves the
            // row to its sorted position now instead of waiting for the 5 s
            // `Full` fallback tick. This matters most for `sort = ["state",
            // …]`, where the sort key is itself a pushed field: without it the
            // badge updates instantly but the row stays put for up to 5 s.
            // The push already merged surgically into the existing rows
            // (`apply_single_agent`), and `sort_by` is stable, so only the row
            // that actually changed moves — this no longer reintroduces the
            // "all rows jumped" jitter that the original per-push full-snapshot
            // refresh caused. Selection is pinned by `pane_id`, so the cursor
            // stays on the same agent.
            app.resort_rows_restoring_selection(selected);
        }
    }
}

pub(super) fn merge_agent_for_ui(prior: &Agent, incoming: &Agent) -> Agent {
    // Preserve rich optional fields when the incoming payload carries
    // None. A Transition broadcast captures the Agent row exactly as it
    // exists after the event, but events that don't touch a field leave
    // it at its current value. The only way the payload has None is
    // when the row was freshly created (Starting placeholder) or the
    // event legitimately cleared the field. We distinguish the two
    // cases by keeping the UI's prior value when the new one is None —
    // a real clear would require an explicit Some("") or similar,
    // which no event produces today.
    let mut merged = incoming.clone();
    if merged.state == AgentState::Starting && prior.state != AgentState::Starting {
        merged.state = prior.state;
    }
    if merged.last_prompt.is_none() {
        merged.last_prompt.clone_from(&prior.last_prompt);
    }
    if merged.last_prompt_at.is_none() {
        merged.last_prompt_at = prior.last_prompt_at;
    }
    if merged.last_response.is_none() {
        merged.last_response.clone_from(&prior.last_response);
    }
    if merged.last_notification.is_none() {
        merged
            .last_notification
            .clone_from(&prior.last_notification);
    }
    if merged.workload.is_empty() {
        merged.workload.clone_from(&prior.workload);
    }
    if merged.model.is_none() {
        merged.model.clone_from(&prior.model);
    }
    if merged.context_used_pct.is_none() {
        merged.context_used_pct = prior.context_used_pct;
    }
    if merged.cost_usd.is_none() {
        merged.cost_usd = prior.cost_usd;
    }
    // Codex rollout quota snapshots are complete: None can deliberately
    // clear a missing window, rather than mean a partial statusline reading.
    if merged.kind != AgentKind::Codex && merged.rate_limit_5h_pct.is_none() {
        merged.rate_limit_5h_pct = prior.rate_limit_5h_pct;
    }
    if merged.kind != AgentKind::Codex && merged.rate_limit_5h_resets_at.is_none() {
        merged.rate_limit_5h_resets_at = prior.rate_limit_5h_resets_at;
    }
    if merged.kind != AgentKind::Codex && merged.rate_limit_7d_pct.is_none() {
        merged.rate_limit_7d_pct = prior.rate_limit_7d_pct;
    }
    if merged.kind != AgentKind::Codex && merged.rate_limit_7d_resets_at.is_none() {
        merged.rate_limit_7d_resets_at = prior.rate_limit_7d_resets_at;
    }
    // NOTE: rate_limited_until, rate_limit_scope, and
    // rate_limit_source are intentionally NOT merged. Events like
    // `Started` and `TurnStopped` legitimately clear these fields (a
    // new session or a successful turn means the cap is lifted), and
    // preserving old values would make the UI show stale rate-limit
    // badges forever.
    merged
}

/// Apply a single push-driven `Transition.agent` to the matching
/// row in `app.rows`, leaving everything else untouched. If we don't
/// find a row for `(kind, session_id)`, append it — that's the
/// "first event for this session" case, where the next fallback
/// tick will reconcile sort order and pane labels.
pub(super) fn apply_single_agent(app: &mut App, agent: Agent) {
    if app.watch_cfg.layout == WatchLayout::Swarm || app.watch_cfg.view != WatchView::Pane {
        apply_single_agent_to_work(app, agent);
        return;
    }

    let key = (agent.kind, agent.session_id.clone());
    let mut updated = false;
    for row in &mut app.rows {
        if let WatchRow::Agent(a) = row {
            if (a.kind, a.session_id.clone()) == key {
                **a = merge_agent_for_ui(a, &agent);
                updated = true;
                break;
            }
        }
    }
    if !updated {
        app.rows.push(WatchRow::agent(agent));
    }
    // Caller (`apply_outcome`) re-sorts after this returns so a pushed change
    // reaches its sorted position without waiting for the 5 s `Full` tick. The
    // re-sort lives there (not here) so it runs once per outcome and the unit
    // tests that drive `apply_single_agent` directly can assert the pre-sort
    // merge in isolation.
}

fn apply_single_agent_to_work(app: &mut App, agent: Agent) {
    // Match the same session+window work key used by the full refresh. This
    // keeps a pushed transition inside its ticket window instead of merging
    // every agent in the workspace session.
    let sort_context = SortContext::new(
        &app.panes,
        &app.sessions,
        &app.session_activity,
        OffsetDateTime::now_utc(),
    );
    let located = sort_context.pane_for_agent(&agent).map(|pane| {
        let host = tend::backend::pane_id_host_kind(&pane.pane_id).unwrap_or(tend::HostKind::Tmux);
        (
            pane.session.clone(),
            pane_work_group_key(pane),
            Some(WindowKey::from_pane(host, pane)),
            pane_work_display_name(pane),
        )
    });
    let (session, group_key, window_key, display_name) = located.unwrap_or_else(|| {
        let session = agent
            .pane
            .as_deref()
            .map_or_else(|| "(no session)".to_string(), |p| format!("(stale {p})"));
        let group_key = session_group_key(None, &session);
        (session.clone(), group_key, None, Some(session))
    });
    let display_name = display_name.unwrap_or_else(|| {
        app.sessions
            .iter()
            .find(|item| item.session_id == session || item.name == session)
            .map_or_else(|| session.clone(), |item| item.name.clone())
    });
    for row in &mut app.rows {
        let WatchRow::Work(s) = row else {
            continue;
        };
        if s.group_key != group_key {
            continue;
        }
        if s.window_key.is_none() {
            s.window_key.clone_from(&window_key);
        }
        let key = (agent.kind, agent.session_id.clone());
        let mut updated_agent = agent.clone();
        if let Some(existing) = s
            .agents
            .iter_mut()
            .find(|a| (a.kind, a.session_id.clone()) == key)
        {
            updated_agent = merge_agent_for_ui(existing, &agent);
            *existing = updated_agent.clone();
        } else {
            s.agents.push(updated_agent.clone());
        }
        s.agents.sort_by(|a, b| {
            b.last_activity_at
                .cmp(&a.last_activity_at)
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        if let Some(pane) = updated_agent.pane.as_ref() {
            if !s.pane_ids.iter().any(|id| id == pane) {
                s.pane_ids.push(pane.clone());
                s.pane_count = s.pane_count.max(s.pane_ids.len());
            }
        }
        s.agent_states.insert(key, updated_agent.state);
        s.latest_agent = s.agents.first().cloned();
        s.representative_pane = s
            .latest_agent
            .as_ref()
            .and_then(|a| a.pane.clone())
            .or_else(|| s.representative_pane.clone());
        return;
    }

    let mut agent_states = HashMap::new();
    agent_states.insert((agent.kind, agent.session_id.clone()), agent.state);
    app.rows.push(WatchRow::Work(Box::new(WorkRow {
        display_name,
        group_key,
        window_key,
        session,
        pane_ids: agent.pane.clone().into_iter().collect(),
        representative_pane: agent.pane.clone(),
        latest_agent: Some(agent.clone()),
        agents: vec![agent],
        pane_count: 0,
        bare_summary: None,
        activity: None,
        // No pane topology behind this row, so nothing to read aliases from.
        completion: None,
        pipeline_run: None,
        agent_states,
    })));
}

fn apply_full(app: &mut App, full: FullRefresh) {
    let FullRefresh {
        agents: mut new_agents,
        panes,
        sessions,
        session_activity,
        error,
        pipeline_runs,
    } = full;

    app.last_error = error;
    app.pipeline_runs = pipeline_runs;

    // Build a lookup of the previously-known agents so the merge can
    // distinguish a genuine daemon-driven change from a transient
    // `Starting` placeholder that also regressed optional fields to
    // None. In work view, this must include every collapsed agent, not just
    // the work's latest representative.
    let mut prev_rows: HashMap<(AgentKind, String), &Agent> = HashMap::new();
    for row in &app.rows {
        match row {
            WatchRow::Agent(a) => {
                prev_rows.insert((a.kind, a.session_id.clone()), a);
            }
            WatchRow::Work(s) => {
                for agent in &s.agents {
                    prev_rows.insert((agent.kind, agent.session_id.clone()), agent);
                }
            }
            WatchRow::BarePane(_) => {}
        }
    }

    for agent in &mut new_agents {
        let key = (agent.kind, agent.session_id.clone());
        if let Some(&prior) = prev_rows.get(&key) {
            *agent = merge_agent_for_ui(prior, agent);
        }
    }

    app.set_data_with_sessions(new_agents, panes, sessions, session_activity);
}

/// The "sessions" list for one host. tmux shells `list-sessions`; herdr
/// derives sessions from its workspaces over the socket (session id = raw
/// `workspace_id`, matching `PaneInfo.session` and the session-activity
/// ledger key so the DUR column resolves; display name = workspace label,
/// falling back to the id). zellij has no session concept here, so it stays
/// empty. May shell out or hit a socket — callers MUST run it off the tokio
/// runtime.
fn sessions_for_host(host: tend::HostKind) -> Vec<SessionInfo> {
    match host {
        tend::HostKind::Tmux => tend::tmux::list_sessions().unwrap_or_default(),
        // rmux pane rows already carry the human session name. Until rmux
        // session-activity sampling lands, emitting tmux-shaped `$N`
        // SessionInfo ids here would collide with tmux's activity ledger.
        tend::HostKind::Herdr => {
            let socket = tend::backend::herdr::default_socket_path();
            tend::backend::herdr::herdr_list_workspaces(&socket)
                .into_iter()
                .map(|ws| SessionInfo {
                    group: None,
                    session_id: ws.id,
                    name: ws.label,
                    // herdr's socket API exposes no client-attach state
                    // (see docs/HERDR.md); watch never reads this field for
                    // display, and foreground time comes from the ledger.
                    attached_clients: 0,
                })
                .collect()
        }
        tend::HostKind::Cmux | tend::HostKind::Rmux | tend::HostKind::Zellij => Vec::new(),
    }
}

/// Compute one refresh outcome: pane inventory aggregated across every
/// active backend (off-runtime via `spawn_blocking` so any shell-out
/// doesn't block the runtime) plus a daemon snapshot. Kept independent of
/// `App` so the work can run on a worker thread without holding any UI
/// state.
pub(super) async fn compute_refresh(
    client: &Client,
    backends: &[tend::SharedBackend],
    session_activity_path: Option<PathBuf>,
) -> RefreshOutcome {
    // Pane inventory is independent of the daemon — fetch it even when
    // tendd is down so `tend watch` stays useful as a session picker. This
    // is the cross-multiplexer unified console: aggregate `list_panes` and
    // the per-host session sources across EVERY active backend so tmux and
    // herdr rows show side by side. Rows carry their host namespace in the
    // pane id, so a concat keeps them distinct.
    //
    // Each backend's `list_panes` / session source may shell out (tmux) or
    // hit a socket (herdr); neither may run on a tokio worker, so every one
    // goes through `spawn_blocking`. Spawning them up front runs the whole
    // fan-out concurrently — the tick budget is one host's latency, not the
    // sum (see docs/MULTI_HOST.md "Startup cost").
    let pane_tasks: Vec<_> = backends
        .iter()
        .map(|backend| {
            let backend = backend.clone();
            tokio::task::spawn_blocking(move || backend.list_panes())
        })
        .collect();
    let session_tasks: Vec<_> = backends
        .iter()
        .map(|backend| {
            let host = backend.kind();
            tokio::task::spawn_blocking(move || sessions_for_host(host))
        })
        .collect();

    let session_activity_task = async move {
        match session_activity_path {
            Some(path) => tend::session_activity::load(&path).await,
            None => Vec::new(),
        }
    };

    // The blocking tasks already run concurrently on the blocking pool;
    // await the daemon snapshot + ledger load alongside them, then collect.
    let (session_activity, snapshot, pipeline_runs) = tokio::join!(
        session_activity_task,
        client.snapshot(),
        client.pipeline_runs()
    );
    let pipeline_runs = pipeline_runs.unwrap_or_default();

    let mut panes: Vec<PaneInfo> = Vec::new();
    for task in pane_tasks {
        panes.extend(task.await.unwrap_or_default());
    }
    let mut sessions: Vec<SessionInfo> = Vec::new();
    for task in session_tasks {
        sessions.extend(task.await.unwrap_or_default());
    }

    let full = match snapshot {
        Ok(agents) => FullRefresh {
            agents,
            panes,
            sessions,
            session_activity,
            error: None,
            pipeline_runs,
        },
        Err(e) => FullRefresh {
            agents: Vec::new(),
            panes,
            sessions,
            session_activity,
            error: Some(DaemonError {
                self_describing: matches!(e, RuntimeError::NotConnected(_)),
                message: e.to_string(),
            }),
            pipeline_runs,
        },
    };
    RefreshOutcome::Full(full)
}

/// Background task that owns its own `Client` clone and produces refresh
/// outcomes on a 500 ms tick or whenever the input loop sends a wake
/// request. The task exits cleanly when *either* end of either channel
/// closes — main drops `wake_tx` to signal shutdown.
///
/// Generic over the fetcher so unit tests can swap in a closure that
/// returns a canned `RefreshOutcome` without touching tmux or the daemon.
pub(super) async fn refresh_task<F, Fut, S, A, AFut>(
    mut fetch: F,
    mut fetch_activity: A,
    mut wake: mpsc::Receiver<()>,
    out: mpsc::Sender<RefreshOutcome>,
    sub_init: S,
) where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = RefreshOutcome> + Send,
    S: Future<Output = Option<tend::ipc::TransitionStream>> + Send + 'static,
    A: FnMut() -> AFut + Send + 'static,
    AFut: Future<Output = Option<Vec<Agent>>> + Send,
{
    // Acquire the streaming subscription as the first thing the
    // background task does — `run` doesn't await it any more, so the
    // popup gets to paint its empty frame before we pay this latency.
    // `None` falls back to historical polling.
    let mut sub = sub_init.await;

    // When we have a streaming subscription, push updates handle the
    // common case with a bounded 100 ms coalescing delay. The polling
    // tick reconciles topology and any missed updates. Without the
    // subscription we fall back to the historical 500 ms cadence so
    // the watch still updates against an old daemon.
    let interval_dur = if sub.is_some() {
        STREAMING_FALLBACK_INTERVAL
    } else {
        POLL_INTERVAL
    };
    let mut tick = tokio::time::interval(interval_dur);
    // If a refresh runs longer than one tick (slow daemon, slow tmux),
    // don't pile up backlog ticks — skip them.
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // The first `tick()` fires immediately. We don't want a duplicate
    // refresh right after the priming snapshot in `run`, so consume it.
    tick.tick().await;

    loop {
        // Reduce subscribe-arm noise: the `if sub.is_some()` guard
        // on the select branch keeps the helper out of the wait set
        // entirely when we don't have a stream — `pending()` would
        // never resolve but tokio still polls it once per loop, which
        // would burn a tiny bit of CPU and obscure traces.
        tokio::select! {
            _ = tick.tick() => {
                // Periodic full sync — catches up after lagged drops
                // or any state we missed via the push stream.
                let outcome = fetch().await;
                if out.send(outcome).await.is_err() {
                    return;
                }
            }
            req = wake.recv() => {
                // None => the input loop dropped wake_tx, i.e. quit/attach.
                if req.is_none() {
                    return;
                }
                // User-triggered (`r` key, or the priming wake from
                // `run`) → also a full sync.
                let outcome = fetch().await;
                if out.send(outcome).await.is_err() {
                    return;
                }
            }
            res = recv_agent_change(&mut sub), if sub.is_some() => {
                if res.is_some() {
                    if let Some(agents) = fetch_activity().await {
                        if out.send(RefreshOutcome::Agents(agents)).await.is_err() {
                            return;
                        }
                    }
                } else {
                    // Daemon closed the stream OR parse/IO error.
                    // Drop the subscription and continue with pure
                    // polling so the user keeps a working (if
                    // higher-latency) view.
                    sub = None;
                    tick = tokio::time::interval(POLL_INTERVAL);
                    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
                    tick.tick().await;
                }
            }
        }
    }
}

/// Await a compact invalidation; a closed/failed stream enables fallback polling.
async fn recv_agent_change(sub: &mut Option<tend::ipc::TransitionStream>) -> Option<()> {
    sub.as_mut()?.recv_change().await.ok().flatten()
}

/// Resolve the server identity that owns the invoking pane. This is kept as a
/// separate hint from the raw pane id because `%N` values repeat across tmux
/// sockets. Hosts without a native multi-server address use the same sentinel
/// as `TopologySnapshot`.
fn current_backend_endpoint(host: tend::HostKind, pane_id: &str) -> Option<BackendEndpoint> {
    let socket = match host {
        tend::HostKind::Tmux => tend::tmux::layout::current_socket_name().or_else(|| {
            std::env::var("TEND_TMUX_SOCKET")
                .ok()
                .filter(|socket| !socket.trim().is_empty())
        }),
        tend::HostKind::Rmux => tend::backend::rmux::endpoint_from_env(),
        tend::HostKind::Cmux => Some(tend::backend::cmux::endpoint_from_env()),
        tend::HostKind::Zellij => Some("zellij".into()),
        tend::HostKind::Herdr => Some("herdr".into()),
    }?;
    Some(BackendEndpoint {
        host,
        socket: tend::backend::pane_endpoint_identity(Some(pane_id), &socket),
    })
}

/// Ask the background refresh task for an immediate sync and let the title
/// show that one is in flight.
///
/// Coalesce repeated requests: a full wake slot already means a refresh is
/// pending, so a `try_send` failure is fine — the in-flight request picks up
/// this intent too.
pub(super) fn request_refresh(app: &mut App, wake_tx: &mpsc::Sender<()>) {
    let _ = wake_tx.try_send(());
    app.refresh_pending = true;
}

/// Initialize after the scaffold paint, before the first collaboration read.
pub(super) async fn initialize_watch_collaboration(client: &Client, app: &mut App) {
    probe_version_skew(client, app).await;
    refresh_watch_collaboration(client, app).await;
    sync_collab_screen_from_cache(app);
}

/// Entry point for `tend watch`.
///
/// Returns the exact topology pane (or a legacy raw pane target from an old
/// overlay) when the user presses Enter. The caller (`main.rs`) runs
/// the actual tmux switch-client invocation *after* this returns so the
/// terminal is already restored by the time we hand off control.
#[allow(clippy::too_many_lines)]
// mostly setup + action dispatch — extracting a helper
// for three preview-related arms hurts readability more than it helps
pub async fn run(
    client: &Client,
    watch_cfg: WatchConfig,
    message_composer: MessageComposerConfig,
    session_activity_path: Option<PathBuf>,
    activity_path: Option<PathBuf>,
    sort_persist_path: Option<PathBuf>,
    caller_pane: Option<String>,
) -> Result<Option<WatchOpenTarget>> {
    let terminal = setup_terminal()?;
    let mut guard = TerminalGuard::new(terminal);

    let mut app = App::with_config(watch_cfg);
    app.message_skills = message_composer.skills;
    // The unified console observes every active host. The set threads
    // through the priming refresh and the background task; the live
    // capture path resolves a backend per pane-id namespace instead.
    let backends: Vec<tend::SharedBackend> = tend::active_backends();

    // "Where am I" is inherently single-host (env-based) — land the cursor on
    // the user's current pane on first load. `current_pane` is env-based: the
    // env-preferred backend answers inside its own pane, but inside e.g. a
    // herdr pane launched from tmux BOTH backends could answer. `active_backends`
    // is ordered by env preference (`backends[0]` = the env-preferred host), so
    // we resolve in set order and take the first `Some` — env preference breaks
    // the tie. Single-host: identical (the one backend answers or doesn't).
    // The binding-expanded pane beats anything derived in-process: it was
    // resolved by tmux at the keypress, in the pressing client's context,
    // which no query made from inside a popup can reproduce.
    let (initial_pane, initial_host) = if let Some(pane) = caller_pane {
        if tend::backend::rmux::endpoint_from_env().is_some() {
            let pane = if pane.starts_with("rmux:") {
                pane
            } else {
                format!("rmux:{pane}")
            };
            (Some(pane), Some(tend::HostKind::Rmux))
        } else {
            (Some(pane), Some(tend::HostKind::Tmux))
        }
    } else {
        backends
            .iter()
            .find_map(|backend| {
                backend
                    .current_pane()
                    .map(|pane| (Some(pane), Some(backend.kind())))
            })
            .unwrap_or((None, None))
    };
    let initial_endpoint = initial_pane
        .as_deref()
        .and_then(|pane_id| initial_host.and_then(|host| current_backend_endpoint(host, pane_id)));
    app.set_initial_pane_on(initial_pane.clone(), initial_endpoint);
    app.collaboration.origin = Some(watch_collaboration_origin(initial_pane.clone()));
    app.collaboration_scope = message_composer.collaboration_scope;
    let watch_started_at = OffsetDateTime::now_utc();

    // Paint the first frame **before** any IPC. The popup
    // (`prefix + s` → `display-popup -E tend watch`) becomes visible
    // the instant tmux finishes spawning us, so we want the user to
    // see the table scaffold (header + empty body) immediately
    // rather than a black rectangle for the ~50-100 ms it takes to
    // shell out to `tmux list-panes` and round-trip a snapshot.
    //
    // The first real refresh fires from the background task right
    // after this — we send a wake on the channel below so it doesn't
    // wait for the 5 s fallback tick.
    guard
        .terminal_mut()
        .draw(|f| render(f, &mut app))
        .map_err(anyhow::Error::from)?;
    // The scaffold stays immediate; negotiate before any capability-dependent read.
    initialize_watch_collaboration(client, &mut app).await;

    // Background refresh task owns its own Client clone so the borrowed
    // `client: &Client` doesn't have to outlive the task. The clone is
    // cheap (a single `PathBuf`) and avoids needing an `Arc`/lifetime
    // wrapper for what is effectively immutable data. The backend is
    // already an `Arc<dyn …>` so cloning it is just a refcount bump.
    let bg_client = client.clone();
    let bg_backends = backends.clone();
    let bg_session_activity_path = session_activity_path.clone();
    let sub_client = client.clone();
    let (wake_tx, wake_rx) = mpsc::channel::<()>(WAKE_CAPACITY);
    let (outcome_tx, mut outcome_rx) = mpsc::channel::<RefreshOutcome>(OUTCOME_CAPACITY);
    let (window_capture_tx, mut window_capture_rx) = mpsc::channel::<CapturedWindow>(1);
    let mut window_capture_task: Option<(WindowKey, tokio::task::JoinHandle<()>)> = None;

    // Subscribe lazily inside the refresh task so its `await` doesn't
    // block the first paint. Falls back to polling on any error
    // (older daemon, socket unreachable, connection refused).
    let subscription_init = async move {
        match sub_client.subscribe_agent_changes().await {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::debug!(error = %e, "subscribe unavailable; falling back to polling");
                None
            }
        }
    };

    let activity_client = client.clone();
    let bg = tokio::spawn(refresh_task(
        move || {
            let client = bg_client.clone();
            let backends = bg_backends.clone();
            let session_activity_path = bg_session_activity_path.clone();
            async move { compute_refresh(&client, &backends, session_activity_path).await }
        },
        move || {
            let client = activity_client.clone();
            async move { client.snapshot().await.ok() }
        },
        wake_rx,
        outcome_tx,
        subscription_init,
    ));

    // Force the priming refresh immediately so the empty frame above
    // gets replaced by real data within ~50-100 ms instead of waiting
    // for the next fallback tick. `try_send` is safe here — the
    // channel is freshly created with capacity > 0, so the send can't
    // block or fail.
    let _ = wake_tx.try_send(());

    let mut ask_updates = ask_revision_updates(client.clone());
    let mut jump_target: Option<WatchOpenTarget> = None;

    // Repaint only when something actually changed (`needs_render`) or the
    // idle cadence elapsed, instead of once per input-poll tick. `true` here
    // forces the first in-loop frame; `Instant::now()` seeds the cadence.
    let mut needs_render = true;
    let mut last_render = std::time::Instant::now();
    let mut last_mailbox_refresh = std::time::Instant::now();
    let mut last_board_refresh = std::time::Instant::now();

    loop {
        // Animate on a fast cadence only while there's something to move:
        // the swarm view always, or the table views when `[watch] spinner`
        // is on and at least one agent is working/starting — or while a
        // transition pulse is still lit. An otherwise-idle fleet falls back to
        // the calm 1 s idle repaint.
        let spinning = (app.watch_cfg.layout == WatchLayout::Swarm || app.watch_cfg.spinner)
            && icons_unicode()
            && app.visible_spinner_active;
        let animating = spinning || app.has_active_pulse(std::time::Instant::now());
        let redraw_interval = if animating {
            if app.watch_cfg.layout == WatchLayout::Swarm {
                SWARM_REDRAW_INTERVAL
            } else {
                TREE_REDRAW_INTERVAL
            }
        } else {
            IDLE_REDRAW_INTERVAL
        };
        if needs_render || last_render.elapsed() >= redraw_interval {
            if matches!(app.watch_cfg.screen, WatchScreen::Collab)
                && !app.search_query.is_empty()
                && app.collaboration_history.search_full.is_none()
                && app.collab.unavailable.is_none()
            {
                refresh_fleet_collaboration(client, &mut app).await;
            }
            refresh_selected_collaboration_detail(client, &mut app).await;
            guard
                .terminal_mut()
                .draw(|f| render(f, &mut app))
                .map_err(anyhow::Error::from)?;
            last_render = std::time::Instant::now();
            needs_render = false;
        }

        // Drain every event already in the OS buffer in one go before
        // rendering again. Holding a key (or typing in bursts) used to
        // pile up events because the loop only handled one per
        // iteration; the render between events made the queue grow
        // faster than we drained. `poll(Duration::ZERO)` is
        // non-blocking, so this is cheap when the buffer is empty.
        let mut events = drain_pending_events()?;

        // If nothing was waiting, do exactly one bounded blocking
        // wait so an idle UI yields the CPU.
        if events.is_empty() {
            let waited = tokio::task::spawn_blocking(|| -> io::Result<Option<Event>> {
                if crossterm::event::poll(INPUT_POLL)? {
                    Ok(Some(crossterm::event::read()?))
                } else {
                    Ok(None)
                }
            })
            .await
            .map_err(anyhow::Error::from)??;
            if let Some(ev) = waited {
                events.push(ev);
            }
        }

        // Any input (key, resize, mouse) may change what's on screen, so
        // repaint next pass. An event that turns out to be a no-op costs one
        // extra render — negligible next to skipping the idle ~62 fps redraw.
        if !events.is_empty() {
            needs_render = true;
        }

        let mut quit = false;
        for ev in events {
            match handle_event(ev, &mut app) {
                Action::TryDemo => {
                    jump_target = Some(WatchOpenTarget::Demo);
                    quit = true;
                    break;
                }
                Action::Quit => {
                    quit = true;
                    break;
                }
                Action::AttachPane(pane) => {
                    jump_target = Some(WatchOpenTarget::LegacyPane(pane));
                    quit = true;
                    break;
                }
                Action::AttachTopologyPane(pane) => {
                    jump_target = Some(WatchOpenTarget::TopologyPane(pane));
                    quit = true;
                    break;
                }
                Action::NewWindow { session, cwd } => {
                    // tmux shells out; keep it off the input loop like every
                    // other side-effecting action here.
                    let created = tokio::task::spawn_blocking(move || {
                        create_window(&session, cwd.as_deref())
                    })
                    .await
                    .unwrap_or_else(|error| Err(format!("action task failed: {error}")));
                    match created {
                        // Creating a window the user cannot see would be a
                        // worse `prefix + c` than the one they already have.
                        Ok(pane) => {
                            jump_target = Some(WatchOpenTarget::TopologyPane(pane));
                            quit = true;
                            break;
                        }
                        Err(error) => {
                            app.set_hint(format!("✗ new window failed: {error}"), HintLevel::Err);
                        }
                    }
                }
                Action::Refresh => {
                    request_refresh(&mut app, &wake_tx);
                    app.collaboration_history.revision = None;
                    refresh_watch_collaboration(client, &mut app).await;
                    sync_collab_screen_from_cache(&mut app);
                }
                Action::RestartDaemon => {
                    match restart_daemon_from_watch(client).await {
                        Ok(()) => {
                            // Re-probe rather than assuming: a restart that
                            // came back on the same stale binary (a failed
                            // install, a shadowed `PATH`) must keep the bar up
                            // instead of reporting a fix that did not happen.
                            probe_version_skew(client, &mut app).await;
                            if app.version_skew.is_some() {
                                app.set_hint(
                                    "tendd restarted but is still a different build — check which binary is on PATH",
                                    HintLevel::Err,
                                );
                            } else {
                                app.last_error = None;
                                app.set_hint(
                                    "tendd restarted onto the installed binary",
                                    HintLevel::Ok,
                                );
                            }
                        }
                        Err(error) => {
                            app.set_hint(format!("tendd restart failed: {error}"), HintLevel::Err);
                        }
                    }
                    request_refresh(&mut app, &wake_tx);
                }
                Action::OpenPreviewFor(key) => open_preview_for(&mut app, key),
                Action::OpenPreview => {
                    let pane_key = if app.uses_tree() {
                        app.selected_action_pane().map(|pane| pane.key.clone())
                    } else {
                        None
                    };
                    if let Some(pane_id) = pane_key
                        .as_ref()
                        .map(|key| key.pane_id.clone())
                        .or_else(|| app.selected_pane())
                    {
                        app.preview = Some(PreviewState {
                            pane_id,
                            pane_key,
                            scroll: 0,
                            mode: PreviewMode::Popup,
                            // Honor `[watch.preview] default_content` so
                            // first-paint shape (live pane vs prompt
                            // text) matches the user's preference. `c`
                            // still toggles in either direction at runtime.
                            content: app.watch_cfg.preview.default_content,
                        });
                    }
                }
                Action::ClosePreview => {
                    app.preview = None;
                    // Drop any cached pane capture — keeping it would
                    // pin a stale snapshot in memory across preview
                    // sessions and might leak across tmux pane reuse
                    // within the same `pane_id`.
                    app.pane_capture = None;
                }
                Action::TogglePreviewMode => {
                    if let Some(p) = app.preview.as_mut() {
                        p.mode = match p.mode {
                            PreviewMode::Popup => PreviewMode::Fullscreen,
                            PreviewMode::Fullscreen => PreviewMode::Popup,
                        };
                    }
                }
                Action::TogglePreviewContent => {
                    if let Some(p) = app.preview.as_mut() {
                        p.content = match p.content {
                            PreviewContent::PromptResponse => PreviewContent::LivePane,
                            PreviewContent::LivePane => PreviewContent::PromptResponse,
                        };
                        // Reset scroll so the new content starts from
                        // the top — re-using the prompt-mode scroll
                        // offset on a wholly-different content surface
                        // tends to land mid-line.
                        p.scroll = 0;
                        // Drop the cache when leaving LivePane so the
                        // next entry starts with a fresh capture.
                        if matches!(p.content, PreviewContent::PromptResponse) {
                            app.pane_capture = None;
                        }
                    }
                }
                Action::InspectorSplitChanged => {
                    let label = app.inspector_split.label();
                    let saved = sort_persist_path
                        .as_deref()
                        .map(|path| persist_watch_inspector_split(path, label));
                    match saved {
                        Some(Ok(())) => {
                            app.set_hint(format!("list/inspector {label} (saved)"), HintLevel::Ok);
                        }
                        Some(Err(e)) => {
                            app.set_hint(
                                format!("list/inspector {label} (save failed: {e})"),
                                HintLevel::Warn,
                            );
                        }
                        None => {
                            app.set_hint(format!("list/inspector {label}"), HintLevel::Ok);
                        }
                    }
                }
                Action::SetSort(preset) => {
                    app.apply_sort_preset(preset);
                    match sort_persist_path.as_deref() {
                        Some(path) => match persist_watch_sort(path, &app.watch_cfg.sort) {
                            Ok(()) => {
                                app.set_hint(
                                    format!("sorted by {} (saved)", preset.label()),
                                    HintLevel::Ok,
                                );
                            }
                            Err(e) => {
                                app.set_hint(
                                    format!("sorted by {} (save failed: {e})", preset.label()),
                                    HintLevel::Warn,
                                );
                            }
                        },
                        None => {
                            app.set_hint(format!("sorted by {}", preset.label()), HintLevel::Ok);
                        }
                    }
                }
                Action::SetView(view) => {
                    app.apply_view(view);
                    let label = match view {
                        WatchView::Session => "session",
                        WatchView::Window => "window",
                        WatchView::Pane => "pane",
                    };
                    app.set_hint(format!("view: {label}"), HintLevel::Ok);
                }
                Action::SetScreen(screen) => {
                    app.apply_screen(screen);
                    let label = match screen {
                        WatchScreen::Topology => "topology",
                        WatchScreen::Collab => "collab",
                        WatchScreen::Board => "board",
                    };
                    app.set_hint(format!("screen: {label}"), HintLevel::Ok);
                    if screen == WatchScreen::Board
                        && app.board_refresher.start_scan(app.current_agents())
                    {
                        last_board_refresh = std::time::Instant::now();
                    }
                    // Arriving at an empty list and waiting out a poll tick
                    // would read as "nothing is happening" on a busy fleet.
                    refresh_fleet_collaboration(client, &mut app).await;
                }
                Action::SetLayout(layout) => {
                    app.apply_layout(layout);
                    let label = match layout {
                        WatchLayout::Tree => "tree",
                        WatchLayout::Swarm => "swarm",
                        WatchLayout::Work => "work",
                    };
                    app.set_hint(format!("layout: {label}"), HintLevel::Ok);
                }
                Action::SetBoardSort(sort) => {
                    app.board.set_sort(sort);
                    app.watch_cfg.board_sort = sort;
                    let label = board_screen::sort_label(sort);
                    match sort_persist_path
                        .as_deref()
                        .map(|path| persist_watch_value(path, "board_sort", label))
                    {
                        Some(Ok(())) => {
                            app.set_hint(format!("board sort: {label} (saved)"), HintLevel::Ok);
                        }
                        Some(Err(error)) => app.set_hint(
                            format!("board sort: {label} (save failed: {error})"),
                            HintLevel::Warn,
                        ),
                        None => app.set_hint(format!("board sort: {label}"), HintLevel::Ok),
                    }
                }
                Action::SetCollabLayout(layout) => {
                    app.collab.set_layout(layout);
                    app.watch_cfg.collab_layout = layout;
                    let label = collab_screen::layout_label(layout);
                    match sort_persist_path
                        .as_deref()
                        .map(|path| persist_watch_collab_layout(path, layout))
                    {
                        Some(Ok(())) => {
                            app.set_hint(format!("collab layout: {label} (saved)"), HintLevel::Ok);
                        }
                        Some(Err(error)) => app.set_hint(
                            format!("collab layout: {label} (save failed: {error})"),
                            HintLevel::Warn,
                        ),
                        None => app.set_hint(format!("collab layout: {label}"), HintLevel::Ok),
                    }
                }
                Action::AskConfirm(popup) => {
                    app.confirm = Some(popup);
                }
                Action::OpenWorkUp => {
                    app.work_composer = Some(WorkComposer::default());
                }
                Action::OpenAgentMove => {
                    let source = selected_move_target(&app);
                    match source {
                        Ok((source, scope)) => {
                            let opened = tokio::task::spawn_blocking(move || {
                                crate::agent_move::MoveMenu::open_scope(source, scope)
                            })
                            .await;
                            match opened {
                                Ok(Ok(menu)) => app.agent_move = Some(menu),
                                Ok(Err(error)) => app
                                    .set_hint(format!("move unavailable: {error}"), HintLevel::Err),
                                Err(error) => app
                                    .set_hint(format!("move unavailable: {error}"), HintLevel::Err),
                            }
                        }
                        Err(error) => app.set_hint(error, HintLevel::Warn),
                    }
                }
                Action::SubmitAgentMove(request) => {
                    let result =
                        tokio::task::spawn_blocking(move || crate::agent_move::execute(request))
                            .await;
                    match result {
                        Ok(Ok(outcome)) => {
                            crate::agent_move::refresh_locations(client, &outcome.pane).await;
                            app.agent_move = None;
                            app.set_hint(outcome.message, HintLevel::Ok);
                            request_refresh(&mut app, &wake_tx);
                            if outcome.follow {
                                jump_target = Some(WatchOpenTarget::Moved(outcome.pane));
                                quit = true;
                                break;
                            }
                        }
                        error => {
                            let error = match error {
                                Ok(Err(error)) => error.to_string(),
                                Err(error) => error.to_string(),
                                _ => unreachable!(),
                            };
                            if let Some(menu) = app.agent_move.as_mut() {
                                menu.error = Some(error);
                            }
                        }
                    }
                }
                Action::SubmitRename => {
                    finish_rename(&mut app, &wake_tx, apply_rename);
                    let _ = client.invalidate_agent_directory().await;
                }
                Action::SubmitWorkUp => {
                    if let Some(work) = app.work_composer.take() {
                        let in_tmux = std::env::var_os("TMUX").is_some();
                        match open_work_up_window(&work.input, in_tmux) {
                            Ok(id) => app.set_hint(
                                format!("work up {id} — confirm in the new window"),
                                HintLevel::Ok,
                            ),
                            Err(e) => app.set_hint(format!("work up failed: {e}"), HintLevel::Err),
                        }
                    }
                }
                Action::OpenAsk => {
                    refresh_ask_entries(client, &mut app).await;
                    app.ask_composer = Some(AskComposer::default());
                }
                Action::SubmitAsk { new_conversation } => {
                    if let Some(ask) = app.ask_composer.take() {
                        let result = if new_conversation {
                            client.ask_send_new(&ask.input).await
                        } else {
                            client.ask_send(&ask.input).await
                        };
                        match result {
                            Ok(entry) => {
                                app.set_hint(
                                    format!("asked {} — answer lands in A", entry.agent),
                                    HintLevel::Ok,
                                );
                                refresh_ask_entries(client, &mut app).await;
                                // Land on the question just asked, whatever
                                // the filter currently shows.
                                let newest = visible_ask_entries(&app).len().saturating_sub(1);
                                app.ask_panel.open_at(newest);
                            }
                            Err(e) => {
                                app.set_hint(format!("ask failed: {e}"), HintLevel::Err);
                                // Busy conversations are the common reason to
                                // retry as new. Keep the exact draft so Ctrl-E
                                // and Enter can do that without retyping it.
                                app.ask_composer = Some(ask);
                            }
                        }
                    }
                }
                Action::OpenAskPanel => {
                    refresh_ask_entries(client, &mut app).await;
                    let newest = visible_ask_entries(&app).len().saturating_sub(1);
                    app.ask_panel.open_at(newest);
                }
                Action::CycleAskAgent => {
                    // Two agents, so "cycle" is a swap. Naming the next one
                    // explicitly (rather than sending "toggle") keeps the
                    // daemon the single source of truth for what is valid.
                    let next = if app.ask_agent == "claude" {
                        "codex"
                    } else {
                        "claude"
                    };
                    match client.ask_agent(Some(next)).await {
                        Ok(agent) => {
                            app.set_hint(format!("ask agent: {agent}"), HintLevel::Ok);
                            app.ask_agent = agent;
                            refresh_ask_entries(client, &mut app).await;
                        }
                        Err(e) => app.set_hint(format!("ask agent failed: {e}"), HintLevel::Err),
                    }
                }
                Action::SelectAskConversation(id) => {
                    match client.ask_conversation_select(&id).await {
                        Ok(conversation) => {
                            app.ask_agent.clone_from(&conversation.agent);
                            app.active_ask_conversation_id = Some(conversation.id);
                            refresh_ask_entries(client, &mut app).await;
                            let newest = visible_ask_entries(&app).len().saturating_sub(1);
                            app.ask_panel.open_at(newest);
                        }
                        Err(e) => app.set_hint(
                            format!("ask conversation select failed: {e}"),
                            HintLevel::Err,
                        ),
                    }
                }
                Action::OpenCollaborationMessage => {
                    refresh_watch_collaboration(client, &mut app).await;
                    open_watch_collaboration_composer(&mut app);
                }
                Action::ToggleCollaborationMark => {
                    refresh_watch_collaboration(client, &mut app).await;
                    let outcome = toggle_collaboration_mark(&mut app);
                    apply_outcome_to_app(&mut app, outcome);
                }
                Action::DismissBroadcastReport => {
                    app.broadcast_report = None;
                }
                Action::OpenCollaborationMailbox => {
                    open_watch_collaboration_mailbox(client, &mut app).await;
                }
                Action::SubmitCollaboration => {
                    if let Some(reason) = board_message_stale(&app) {
                        // The draft stays: the operator may retarget it.
                        app.set_hint(reason, HintLevel::Err);
                        continue;
                    }
                    if let Some(composer) = app.collaboration_composer.take() {
                        // A board message keeps its draft and frozen recipient
                        // until it is delivered; a refusal hands both back.
                        let board_draft =
                            matches!(app.board_pending, Some(BoardPending::Message { .. }))
                                .then(|| composer.clone());
                        let (outcome, report) =
                            run_watch_collaboration_composer(client, composer).await;
                        let outcome = finish_board_message(&mut app, board_draft, outcome);
                        if let Some(report) = report.as_ref() {
                            // A delivered recipient consumes its mark; a failed
                            // one keeps it, so the retry is the same key press
                            // rather than a re-selection. Clearing everything
                            // would make a partial failure look like a clean
                            // slate.
                            let delivered: std::collections::HashSet<&str> = report
                                .rows
                                .iter()
                                .filter(|row| row.outcome.is_ok())
                                .map(|row| row.pane.as_str())
                                .collect();
                            app.collaboration_marks
                                .retain(|mark| !delivered.contains(mark.pane.as_str()));
                        }
                        app.broadcast_report = report;
                        apply_outcome_to_app(&mut app, outcome);
                        refresh_watch_collaboration(client, &mut app).await;
                    }
                }
                Action::SaveMessageSkill { name, prompt } => {
                    let Some(path) = sort_persist_path.as_deref() else {
                        app.set_hint(
                            "message skill save failed: no config path is available",
                            HintLevel::Err,
                        );
                        continue;
                    };
                    match upsert_message_skill(path, &name, &prompt) {
                        Ok(()) => {
                            let existed = app.message_skills.insert(name.clone(), prompt).is_some();
                            app.message_skill_editor = None;
                            focus_active_message_skill(&mut app, &name);
                            app.set_hint(
                                format!(
                                    "{} message skill /{name}",
                                    if existed { "updated" } else { "added" }
                                ),
                                HintLevel::Ok,
                            );
                        }
                        Err(error) => app.set_hint(
                            format!("message skill save failed: {error}"),
                            HintLevel::Err,
                        ),
                    }
                }
                Action::CancelCollaborationComposer => {
                    app.collaboration_composer = None;
                }
                Action::CollaborationDefaultsChanged => {
                    if let Some(Err(error)) = sort_persist_path.as_deref().map(|path| {
                        persist_watch_collaboration_defaults(
                            path,
                            app.collaboration_compose_defaults,
                        )
                    }) {
                        app.set_hint(
                            format!("message kind/mode save failed: {error}"),
                            HintLevel::Warn,
                        );
                    }
                }
                Action::ClaimBoardInbox { origin, label } => {
                    let outcome = match client.collaboration_inbox(&origin).await {
                        Ok(requests) if requests.is_empty() => {
                            ActionOutcome::Ok(format!("{label}'s collaboration inbox is empty"))
                        }
                        Ok(requests) => ActionOutcome::Ok(format!(
                            "claimed {} collaboration request{} for {label}",
                            requests.len(),
                            if requests.len() == 1 { "" } else { "s" },
                        )),
                        Err(error) => ActionOutcome::Err(format!("inbox failed: {error}")),
                    };
                    apply_outcome_to_app(&mut app, outcome);
                }
                Action::ClaimCollaborationInbox => {
                    // Claiming is the recipient's move, so `i` acts for the
                    // agent whose inbox is on screen — never for the console,
                    // which is only ever a sender.
                    let outcome = match app.collaboration.inbox.clone() {
                        Some(anchor) => match client.collaboration_inbox(&anchor.origin).await {
                            Ok(requests) if requests.is_empty() => ActionOutcome::Ok(format!(
                                "{}'s collaboration inbox is empty",
                                anchor.label
                            )),
                            Ok(requests) => ActionOutcome::Ok(format!(
                                "claimed {} collaboration request{} for {}",
                                requests.len(),
                                if requests.len() == 1 { "" } else { "s" },
                                anchor.label
                            )),
                            Err(error) => ActionOutcome::Err(format!("inbox failed: {error}")),
                        },
                        None => ActionOutcome::Err(
                            "select a row with an agent to claim its inbox".into(),
                        ),
                    };
                    apply_outcome_to_app(&mut app, outcome);
                    refresh_watch_collaboration(client, &mut app).await;
                    app.collaboration_mailbox.tab = CollaborationMailboxTab::Incoming;
                    clamp_collaboration_mailbox(&mut app);
                }
                Action::ConfirmYes => {
                    if let Some(popup) = app.confirm.take() {
                        match popup.on_confirm {
                            ConfirmAction::Quick(action) if refuse_busy(&mut app, &action) => {}
                            ConfirmAction::Quick(action) if is_many_pane(&action) => {
                                start_quick_in_background(&mut app, action);
                            }
                            ConfirmAction::Quick(action) => {
                                let invalidates = action.invalidates_directory();
                                // Pane and clipboard actions shell out
                                // synchronously. Keep them off the input loop.
                                let outcome = tokio::task::spawn_blocking(move || {
                                    let mut fx = RealEffects;
                                    dispatch_quick_action(action, &mut fx)
                                })
                                .await
                                .unwrap_or_else(|e| {
                                    ActionOutcome::Err(format!("✗ action task failed: {e}"))
                                });
                                if invalidates {
                                    let _ = client.invalidate_agent_directory().await;
                                }
                                apply_outcome_to_app(&mut app, outcome);
                            }
                            ConfirmAction::ClearAskHistory => match client.ask_clear().await {
                                Ok(removed) => {
                                    refresh_ask_entries(client, &mut app).await;
                                    app.set_hint(
                                        format!(
                                            "cleared {removed} completed ask entr{}",
                                            if removed == 1 { "y" } else { "ies" }
                                        ),
                                        HintLevel::Ok,
                                    );
                                }
                                Err(error) => app.set_hint(
                                    format!("ask history clear failed: {error}"),
                                    HintLevel::Err,
                                ),
                            },
                            ConfirmAction::DeleteAskHistory(id) => {
                                match client.ask_delete(&id).await {
                                    Ok(true) => {
                                        refresh_ask_entries(client, &mut app).await;
                                        app.set_hint(
                                            "deleted one ask history entry",
                                            HintLevel::Ok,
                                        );
                                    }
                                    Ok(false) => {
                                        refresh_ask_entries(client, &mut app).await;
                                        app.set_hint(
                                            "ask entry was running or no longer exists",
                                            HintLevel::Warn,
                                        );
                                    }
                                    Err(error) => app.set_hint(
                                        format!("ask history delete failed: {error}"),
                                        HintLevel::Err,
                                    ),
                                }
                            }
                            ConfirmAction::CancelCollaboration { origin, request_id } => {
                                let outcome = match client
                                    .collaboration_cancel_view(
                                        &origin,
                                        &request_id,
                                        tend::collaboration::CollaborationReadView::Metadata,
                                    )
                                    .await
                                {
                                    Ok(request) => ActionOutcome::Ok(format!(
                                        "cancelled {}",
                                        short_collaboration_request_id(&request.id)
                                    )),
                                    Err(error) => ActionOutcome::Err(format!(
                                        "collaboration cancel failed: {error}"
                                    )),
                                };
                                apply_outcome_to_app(&mut app, outcome);
                                refresh_watch_collaboration(client, &mut app).await;
                                clamp_collaboration_mailbox(&mut app);
                            }
                            ConfirmAction::DeleteMessageSkill(name) => {
                                let Some(path) = sort_persist_path.as_deref() else {
                                    app.set_hint(
                                        "message skill delete failed: no config path is available",
                                        HintLevel::Err,
                                    );
                                    continue;
                                };
                                match remove_message_skill(path, &name) {
                                    Ok(()) => {
                                        app.message_skills.remove(&name);
                                        clamp_active_message_skill(&mut app);
                                        app.set_hint(
                                            format!("removed message skill /{name}"),
                                            HintLevel::Ok,
                                        );
                                    }
                                    Err(error) => app.set_hint(
                                        format!("message skill delete failed: {error}"),
                                        HintLevel::Err,
                                    ),
                                }
                            }
                        }
                    }
                }
                Action::ConfirmCancel => {
                    app.confirm = None;
                }
                Action::Quick(qa) => {
                    if matches!(qa, QuickAction::ShowHelp) {
                        app.help_open = !app.help_open;
                    } else if refuse_busy(&mut app, &qa) {
                    } else if is_many_pane(&qa) {
                        start_quick_in_background(&mut app, qa);
                    } else {
                        // Copy shells out to clipboard helpers (and can fall
                        // back to a /tmp write) — off the loop thread it
                        // goes, same as the confirmed destructive actions.
                        let invalidates = qa.invalidates_directory();
                        let outcome = tokio::task::spawn_blocking(move || {
                            let mut fx = RealEffects;
                            dispatch_quick_action(qa, &mut fx)
                        })
                        .await
                        .unwrap_or_else(|e| {
                            ActionOutcome::Err(format!("✗ action task failed: {e}"))
                        });
                        if invalidates {
                            let _ = client.invalidate_agent_directory().await;
                        }
                        apply_outcome_to_app(&mut app, outcome);
                    }
                }
                Action::NotApplicable(msg) => {
                    app.set_hint(msg, HintLevel::Warn);
                }
                Action::None => {}
            }
        }

        // Live pane capture: the modal live preview and the wide-screen
        // inspector share one cache. When either is visible and the cache is
        // missing or stale (>500 ms), call into
        // the active backend on a worker thread. Bounded by the
        // existing 500 ms TTL so we never fork more than ~2 Hz,
        // regardless of how fast the input loop spins. Capability-
        // gated: backends that report `caps().capture_pane == false`
        // (zellij CLI today) skip the call and the renderer shows a
        // "(not supported)" placeholder.
        let capture_target = app
            .preview
            .as_ref()
            .filter(|preview| preview.content == PreviewContent::LivePane)
            .map(|preview| {
                preview.pane_key.as_ref().map_or_else(
                    || {
                        (
                            preview.pane_id.clone(),
                            None,
                            tend::backend::pane_id_host_kind(&preview.pane_id)
                                .unwrap_or(tend::HostKind::Tmux),
                            preview.pane_id.clone(),
                        )
                    },
                    |key| {
                        (
                            key.pane_id.clone(),
                            Some(key.window.session.endpoint.socket.clone()),
                            key.window.session.endpoint.host,
                            pane_capture_target(key),
                        )
                    },
                )
            })
            .or_else(|| {
                if !app.inspector_visible {
                    return None;
                }
                if app.uses_tree() {
                    app.selected_exact_pane().map(|pane| {
                        (
                            pane.key.pane_id.clone(),
                            Some(pane.key.window.session.endpoint.socket.clone()),
                            pane.key.window.session.endpoint.host,
                            pane_capture_target(&pane.key),
                        )
                    })
                } else {
                    app.selected_pane().map(|pane_id| {
                        let host = tend::backend::pane_id_host_kind(&pane_id)
                            .unwrap_or(tend::HostKind::Tmux);
                        (pane_id.clone(), None, host, pane_id)
                    })
                }
            });
        if let Some((capture_pane, capture_socket, capture_host, command_target)) = capture_target {
            // Resolve the capturing backend by the pane id's namespace so a
            // herdr row captures via herdr even when tmux is the primary
            // host (and vice versa). Cheap to build per capture (bounded to
            // ~2 Hz by the TTL below).
            let cap_backend = crate::backend_for_kind(capture_host);
            if cap_backend.caps().capture_pane {
                let stale = app.pane_capture.as_ref().is_none_or(|c| {
                    c.pane_id != capture_pane
                        || c.socket != capture_socket
                        || c.fetched_at.elapsed() >= Duration::from_millis(500)
                });
                if stale {
                    let pane_id = command_target;
                    let socket = capture_socket.clone();
                    let captured = tokio::task::spawn_blocking(move || {
                        cap_backend.capture_pane_on(socket.as_deref(), &pane_id)
                    })
                    .await
                    .ok()
                    .flatten();
                    app.pane_capture = Some(CapturedPane::new(
                        capture_pane,
                        capture_socket,
                        captured.unwrap_or_default(),
                    ));
                    // Fresh preview bytes — repaint to show them.
                    needs_render = true;
                }
            }
        }

        // Accept completed snapshots without ever awaiting capture on the
        // input loop. A stale result from a window the cursor already left is
        // discarded; the next selected target is scheduled immediately.
        while let Ok(captured) = window_capture_rx.try_recv() {
            window_capture_task = None;
            if app.window_capture_target.as_ref() == Some(&captured.key) {
                app.window_capture = Some(captured);
                needs_render = true;
            }
        }

        // A panic or an early sender failure produces no channel item. Reap
        // such a finished wrapper as well so one failed capture cannot leave
        // window previews permanently disabled for the rest of the run.
        if window_capture_task
            .as_ref()
            .is_some_and(|(_, task)| task.is_finished())
        {
            let (_, task) = window_capture_task.take().expect("task checked above");
            let _ = task.await;
        }

        // Geometry and pane captures run in a detached blocking task. Keep at
        // most one in flight across all selections; spawn_blocking work cannot
        // be cancelled once started, so waiting for that one result is what
        // prevents rapid navigation from piling up subprocess batches.
        if let Some(window_key) = app.window_capture_target.clone() {
            let stale = app.window_capture.as_ref().is_none_or(|capture| {
                capture.key != window_key || capture.fetched_at.elapsed() >= WINDOW_CAPTURE_INTERVAL
            });
            if stale && window_capture_task.is_none() {
                let requested = window_key.clone();
                let tx = window_capture_tx.clone();
                let task = tokio::spawn(async move {
                    if let Ok(captured) =
                        tokio::task::spawn_blocking(move || capture_tmux_window(requested)).await
                    {
                        let _ = tx.send(captured).await;
                    }
                });
                window_capture_task = Some((window_key, task));
            }
        }

        // Drain any refresh outcomes that landed since the last frame.
        // The render path never awaits the refresh — this is the only
        // place data flows back into `App`.
        let mut received_outcome = false;
        while let Ok(outcome) = outcome_rx.try_recv() {
            apply_outcome(&mut app, outcome);
            received_outcome = true;
        }
        let ask_changed = ask_updates.has_changed().unwrap_or(false);
        if ask_changed {
            ask_updates.borrow_and_update();
        }
        if received_outcome {
            app.refresh_pending = false;
            needs_render = true;
            // A pending answer arrives on the daemon's clock, not ours.
            // Piggyback on the refresh the table already does, and only
            // while the panel is open — nobody needs ask polling to run
            // behind a screen that is not showing it.
        }
        if app.ask_panel.open
            && (ask_changed || (received_outcome && app.ask_entries.iter().any(is_ask_running)))
        {
            refresh_ask_entries(client, &mut app).await;
            needs_render = true;
        }

        let mailbox_changed = app
            .collaboration_history
            .changes
            .as_ref()
            .is_some_and(|changes| changes.has_changed().unwrap_or(false))
            && app.collaboration.unavailable.is_none();
        // Many-pane writes report back when they finish, never blocking keys.
        let finished = take_finished_quick(&mut app);
        for outcome in finished {
            apply_outcome_to_app(&mut app, outcome);
            needs_render = true;
        }
        if app.board_stats.poll() {
            let activity = match &app.board_stats.value {
                None => board_screen::ActivityState::Loading,
                Some(Err(_)) => board_screen::ActivityState::Unavailable,
                Some(Ok(by_pane)) => board_screen::ActivityState::Known(by_pane.clone()),
            };
            app.board.set_activity(activity);
            needs_render = true;
        }
        if app.board_collab.poll() | app.board_workers.poll() {
            needs_render = true;
        }
        if app.watch_cfg.screen == WatchScreen::Board
            && app.board_workers.due(BOARD_WORKERS_INTERVAL)
        {
            let client = client.clone();
            app.board_workers.start(async move {
                client
                    .collaboration_workers(BOARD_WORKER_IDLE_SECS)
                    .await
                    .map_err(|error| error.to_string())
            });
        }
        if app.watch_cfg.screen == WatchScreen::Board && app.board_collab.due(BOARD_COLLAB_INTERVAL)
        {
            refresh_board_collaboration(client, &mut app);
        }
        if app.watch_cfg.screen == WatchScreen::Board && app.board_stats.due(BOARD_STATS_INTERVAL) {
            let client = client.clone();
            let config = sort_persist_path
                .clone()
                .or_else(tend::paths::default_config_file);
            let window = board_window(&app);
            app.board_stats.start(async move {
                let cfg = tend::config::Config::load_or_default(config.as_deref())
                    .map_err(|error| error.to_string())?;
                crate::stats::pane_active_stats(&client, &cfg, &window)
                    .await
                    .map_err(|error| error.to_string())
            });
        }
        // The scan runs off this loop: a slow backend must not freeze input.
        if let Some((snapshot, notes)) = app.board_refresher.take_finished() {
            app.board.set_snapshot(snapshot, notes);
            needs_render = true;
        }
        if app.watch_cfg.screen == WatchScreen::Board
            && (last_board_refresh.elapsed() >= BOARD_REFRESH_INTERVAL || !app.board.loaded)
            && !app.board_refresher.busy()
            && app.board_refresher.start_scan(app.current_agents())
        {
            last_board_refresh = std::time::Instant::now();
        }
        if (app.collaboration_mailbox.open || matches!(app.watch_cfg.screen, WatchScreen::Collab))
            && (last_mailbox_refresh.elapsed() >= Duration::from_secs(2)
                || (mailbox_changed
                    && last_mailbox_refresh.elapsed() >= Duration::from_millis(200)))
        {
            refresh_watch_collaboration(client, &mut app).await;
            sync_collab_screen_from_cache(&mut app);
            last_mailbox_refresh = std::time::Instant::now();
            needs_render = true;
        }

        if quit {
            break;
        }
    }

    // Attribute the interaction to the pane that launched watch. The cursor
    // may finish on an unrelated session; recording that selected pane made
    // operator activity look as though another window had invoked the TUI.
    let watch_pane = initial_pane;
    append_human_interaction(
        activity_path.as_deref(),
        HumanInteractionKind::TendWatch,
        &app,
        watch_pane.as_deref(),
        watch_started_at,
        OffsetDateTime::now_utc(),
    )
    .await;

    // Drop the wake sender so refresh_task's `wake.recv()` returns None
    // on its next iteration; then await the join so we don't leak a task.
    //
    // Bound the join with a short timeout: if the refresh task is mid-
    // `tmux list-panes` shell-out or stuck on a daemon snapshot to a
    // hung Unix socket, we'd otherwise wedge `tend watch` quit until
    // those resolve at the OS layer. 2 seconds is well above the
    // usual fetch latency (sub-50 ms) so a healthy run never hits it.
    drop(wake_tx);
    drop(outcome_rx);
    if let Some((_, task)) = window_capture_task.take() {
        task.abort();
    }
    if tokio::time::timeout(Duration::from_secs(2), bg)
        .await
        .is_err()
    {
        tracing::warn!("refresh task did not exit within 2s of shutdown; abandoning");
    }

    Ok(jump_target)
}
