use super::*;
use ratatui::backend::TestBackend;
use std::collections::HashMap;
use tend::collaboration::RoomId;
use tend::event::{AgentKind, AgentState};
use time::OffsetDateTime;

#[allow(clippy::too_many_arguments)]
pub(super) fn fake_agent(
    session: &str,
    pane: Option<&str>,
    kind: AgentKind,
    state: AgentState,
    prompt: Option<&str>,
    model: Option<&str>,
    ctx: Option<f32>,
    cost: Option<f64>,
) -> Agent {
    let now = OffsetDateTime::now_utc();
    Agent {
        ask_origin: None,
        tmux_socket: None,
        tmux_session: None,
        kind,
        session_id: session.into(),
        surface: None,
        pane: pane.map(Into::into),
        pid: None,
        workload: tend::WorkloadSummary::default(),
        subagents: Vec::new(),
        cwd: None,
        state,
        last_prompt: prompt.map(Into::into),
        last_prompt_at: None,
        last_response: None,
        recap: None,
        ai_title: None,
        pending_command: None,
        last_notification: None,
        model: model.map(Into::into),
        context_used_pct: ctx,
        cost_usd: cost,
        rate_limit_5h_pct: None,
        rate_limit_5h_resets_at: None,
        rate_limit_7d_pct: None,
        rate_limit_7d_resets_at: None,
        rate_limited_until: None,
        rate_limit_scope: None,
        rate_limit_source: None,
        state_source: None,
        started_at: now,
        last_activity_at: now,
        state_entered_at: now,
        last_prompt_origin: None,
        last_event_at: None,
        last_response_at: None,
        awaiting_reply: None,
    }
}

pub(super) fn fake_pane(
    pane: &str,
    session: &str,
    window: u32,
    pane_idx: u32,
    cmd: &str,
) -> PaneInfo {
    PaneInfo {
        session_group: None,
        agent_program: None,
        worker_launch_id: None,
        agent_role: None,
        agent_alias: None,
        workspace_id: None,
        work_id: None,
        socket: None,
        pane_id: pane.into(),
        session_id: String::new(),
        session: session.into(),
        window_id: String::new(),
        window_name: String::new(),
        window_index: window.to_string(),
        pane_index: pane_idx.to_string(),
        tty: "/dev/pts/0".into(),
        current_command: cmd.into(),
        title: cmd.into(),
        pane_pid: 0,
        current_path: String::new(),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn fake_topology_pane(
    socket: &str,
    session_id: &str,
    session_name: &str,
    window_id: &str,
    window_name: &str,
    window_index: u32,
    pane_id: &str,
    pane_index: u32,
) -> PaneInfo {
    PaneInfo {
        session_group: None,
        agent_program: None,
        worker_launch_id: None,
        agent_role: None,
        agent_alias: None,
        workspace_id: None,
        work_id: None,
        socket: Some(socket.into()),
        pane_id: pane_id.into(),
        session_id: session_id.into(),
        session: session_name.into(),
        window_id: window_id.into(),
        window_name: window_name.into(),
        window_index: window_index.to_string(),
        pane_index: pane_index.to_string(),
        tty: format!("/dev/pts/{pane_index}"),
        current_command: "codex".into(),
        title: "codex".into(),
        pane_pid: 100 + pane_index,
        current_path: format!("/repo/{session_name}/{window_name}"),
    }
}

pub(super) fn topology_agent(
    agent_session_id: &str,
    pane_id: &str,
    socket: &str,
    state: AgentState,
    prompt: &str,
    activity_offset: i64,
) -> Agent {
    let mut agent = fake_agent(
        agent_session_id,
        Some(pane_id),
        AgentKind::Codex,
        state,
        Some(prompt),
        Some("gpt-5"),
        Some(12.0),
        Some(0.42),
    );
    agent.tmux_socket = Some(socket.into());
    let now = OffsetDateTime::now_utc();
    agent.started_at = now - time::Duration::minutes(10);
    agent.last_activity_at =
        now - time::Duration::minutes(5) + time::Duration::seconds(activity_offset);
    agent.state_entered_at = now - time::Duration::seconds(30);
    agent
}

pub(super) fn topology_watch(view: WatchView, agents: Vec<Agent>, panes: Vec<PaneInfo>) -> App {
    let mut app = App::with_config(WatchConfig {
        view,
        layout: WatchLayout::Tree,
        tree_expansion: WatchTreeExpansion::Always,
        sort: vec![WatchSortKey::Name, WatchSortKey::Pane],
        hide_paneless: false,
        ..WatchConfig::default()
    });
    app.set_data(agents, panes);
    app
}

pub(super) fn select_tree_key(app: &mut App, key: &TopologyNodeKey) {
    let index = app
        .tree_targets()
        .iter()
        .position(|target| &target.key == key)
        .unwrap_or_else(|| panic!("tree target not visible: {key:?}"));
    app.table_state.select(Some(index));
}

pub(super) fn basic_topology_fixture() -> (Vec<Agent>, Vec<PaneInfo>) {
    let panes = vec![
        fake_topology_pane("default", "$1", "tend", "@1", "TEST-1", 0, "%1", 0),
        fake_topology_pane("default", "$1", "tend", "@1", "TEST-1", 0, "%2", 1),
    ];
    let agents = vec![
        topology_agent(
            "agent-1",
            "%1",
            "default",
            AgentState::Working,
            "edit auth",
            1,
        ),
        topology_agent(
            "agent-2",
            "%2",
            "default",
            AgentState::WaitingInput,
            "review auth",
            2,
        ),
    ];
    (agents, panes)
}

pub(super) fn two_window_topology_fixture() -> (Vec<Agent>, Vec<PaneInfo>) {
    let panes = vec![
        fake_topology_pane("default", "$1", "tend", "@1", "TEST-1", 0, "%1", 0),
        fake_topology_pane("default", "$1", "tend", "@2", "TEST-2", 1, "%2", 0),
    ];
    let agents = vec![
        topology_agent("agent-1", "%1", "default", AgentState::Working, "one", 1),
        topology_agent("agent-2", "%2", "default", AgentState::Idle, "two", 2),
    ];
    (agents, panes)
}

/// The shift the terminal actually reports alongside the capital, so the
/// binding is exercised the way a keyboard delivers it.
pub(super) fn press_new_window(app: &mut App) -> Action {
    handle_event(
        Event::Key(KeyEvent::new(KeyCode::Char('C'), KeyModifiers::SHIFT)),
        app,
    )
}

pub(super) fn work_layout_app() -> App {
    let panes = vec![
        fake_topology_pane("default", "$1", "alpha", "@1", "AUTH", 0, "%1", 0),
        fake_topology_pane("default", "$1", "alpha", "@1", "AUTH", 0, "%2", 1),
        fake_topology_pane("default", "$2", "beta", "@2", "DOCS", 0, "%3", 0),
    ];
    let agents = vec![
        topology_agent("auth", "%1", "default", AgentState::Working, "auth", 1),
        topology_agent("auth2", "%2", "default", AgentState::Idle, "auth", 2),
        topology_agent("docs", "%3", "default", AgentState::Idle, "docs", 3),
    ];
    let mut app = App::with_config(WatchConfig {
        view: WatchView::Window,
        layout: WatchLayout::Work,
        sort: vec![WatchSortKey::Name, WatchSortKey::Pane],
        hide_paneless: false,
        ..WatchConfig::default()
    });
    app.set_data(agents, panes);
    app
}

pub(super) fn fake_collaboration_participant(
    pane: &str,
    session_id: &str,
    alias: Option<&str>,
) -> Participant {
    Participant {
        launch_purpose: tend::collaboration::LaunchPurpose::Primary,
        draining: false,
        agent_kind: AgentKind::Codex,
        agent_session_id: session_id.into(),
        pane: pane.into(),
        socket: Some("default".into()),
        room: RoomId {
            host: "tmux".into(),
            socket: Some("default".into()),
            window_id: "@1".into(),
        },
        tmux_session_id: Some("$1".into()),
        tmux_session_name: Some("main".into()),
        window_name: Some("agents".into()),
        state: AgentState::Idle,
        cwd: Some("/tmp/project".into()),
        alias: alias.map(str::to_string),
        roles: Vec::new(),
        console: false,
    }
}

pub(super) fn fake_watch_collaboration_request(
    id: &str,
    from: Participant,
    to: Participant,
    status: RequestStatus,
) -> CollaborationRequest {
    let now = OffsetDateTime::now_utc();
    CollaborationRequest {
        decision: None,
        turn_offer: None,
        sender_follow: None,
        wake_retry: None,
        user_authorized: false,
        task_contract: None,
        attempt: None,
        supersedes_request_id: None,
        superseded_by: None,
        late_replies: Vec::new(),
        dispatch_id: None,
        source_node_id: None,
        delegation_parent: None,
        interruption: None,
        retirement: None,
        human_action: None,
        initiator: None,
        updates: Vec::new(),
        id: id.into(),
        from,
        to,
        provenance: None,
        kind: RequestKind::Review,
        body: "review the auth change".into(),
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
        status,
        created_at: now,
        claimed_at: (status == RequestStatus::Claimed).then_some(now),
        wake_delivery: None,
        notified_at: None,
        reply_notified_at: None,
        reply_read_at: None,
        reply_wake: None,
        reply: None,
        withdrew_reply_wakes: Vec::new(),
        rebound_from: Vec::new(),
        to_slot: None,
    }
}

pub(super) fn participant_on(
    pane: &str,
    socket: &str,
    session_id: &str,
    session: &str,
    window_id: &str,
    window: &str,
) -> Participant {
    let mut participant = fake_collaboration_participant(pane, pane, None);
    participant.socket = Some(socket.into());
    participant.room.socket = Some(socket.into());
    participant.room.window_id = window_id.into();
    participant.tmux_session_id = Some(session_id.into());
    participant.tmux_session_name = Some(session.into());
    participant.window_name = Some(window.into());
    participant
}

pub(super) fn collaboration_watch_app() -> App {
    let cfg = WatchConfig {
        view: WatchView::Pane,
        sort: vec![WatchSortKey::PaneId],
        hide_paneless: false,
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    app.set_data(
        vec![
            fake_agent(
                "self",
                Some("%1"),
                AgentKind::Codex,
                AgentState::Idle,
                None,
                None,
                None,
                None,
            ),
            fake_agent(
                "peer",
                Some("%2"),
                AgentKind::Codex,
                AgentState::Idle,
                None,
                None,
                None,
                None,
            ),
        ],
        vec![
            fake_pane("%1", "main", 0, 0, "codex"),
            fake_pane("%2", "main", 0, 1, "codex"),
        ],
    );
    let current = fake_collaboration_participant("%1", "self", Some("builder"));
    let peer = fake_collaboration_participant("%2", "peer", Some("reviewer"));
    app.collaboration = WatchCollaboration {
        origin: Some(CollaborationOrigin {
            exact_session: false,
            pane: "%1".into(),
            socket: Some("default".into()),
            console: true,
            agent_session_id: None,
        }),
        room: Some(RoomContext {
            current,
            peers: vec![peer],
            unread: 0,
            unread_replies: 0,
        }),
        ..WatchCollaboration::default()
    };
    app
}

/// One long answer, so the reader has something to scroll through and
/// the detail pane provably cannot show all of it.
pub(super) fn app_with_long_answer(lines: usize) -> App {
    let mut app = app_with_paneless_and_pane();
    let now = OffsetDateTime::now_utc();
    app.ask_entries = vec![tend::ask::AskEntry {
        conversation_id: None,
        id: "ask_long".into(),
        prompt: "explain the reconciler".into(),
        answer: (0..lines)
            .map(|i| format!("answer line {i}"))
            .collect::<Vec<_>>()
            .join("\n"),
        status: tend::ask::AskStatus::Answered,
        agent: "claude".into(),
        agent_session_id: None,
        cwd: "/repo/tend".into(),
        asked_at: now,
        answered_at: Some(now),
        cost_usd: None,
        error: None,
        workspace_tools: false,
    }];
    app.ask_panel.open = true;
    app
}

pub(super) fn ask_key(app: &mut App, code: KeyCode) -> Action {
    handle_event(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)), app)
}

pub(super) fn grouped_session(id: &str, name: &str, attached: u32, group: &str) -> SessionInfo {
    SessionInfo {
        session_id: id.into(),
        name: name.into(),
        attached_clients: attached,
        group: Some(group.into()),
    }
}

pub(super) fn fake_session(id: &str, name: &str, attached_clients: u32) -> SessionInfo {
    SessionInfo {
        group: None,
        session_id: id.into(),
        name: name.into(),
        attached_clients,
    }
}

pub(super) fn fake_session_activity(
    id: &str,
    name: &str,
    total_attached_secs: u64,
    attached_since: Option<OffsetDateTime>,
) -> SessionActivity {
    SessionActivity {
        session_id: id.into(),
        name: name.into(),
        attached_clients: u32::from(attached_since.is_some()),
        total_attached_secs,
        attached_since,
        last_seen_at: OffsetDateTime::now_utc(),
    }
}

pub(super) fn plain_text(text: &Text<'_>) -> String {
    text.lines
        .iter()
        .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
        .collect()
}

pub(super) fn display_col_of(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .find(needle)
        .map(|idx| unicode_width::UnicodeWidthStr::width(&haystack[..idx]))
}

/// Build an agent with an explicit `last_activity_at` so sort tests
/// don't depend on `fake_agent`'s wall-clock jitter.
pub(super) fn fake_agent_at(
    session_id: &str,
    pane: &str,
    last_activity_at: OffsetDateTime,
) -> Agent {
    let mut a = fake_agent(
        session_id,
        Some(pane),
        AgentKind::ClaudeCode,
        AgentState::Idle,
        None,
        None,
        None,
        None,
    );
    a.last_activity_at = last_activity_at;
    a
}

/// Helper: pull the rendered LIMITS cell down to a single concatenated
/// string so tests can assert on substrings without juggling spans.
pub(super) fn limits_cell_string(a: &Agent, now: OffsetDateTime) -> String {
    let text = WatchColumn::Limits.agent_text(
        a,
        now,
        &[],
        watch_theme(WatchTheme::Classic),
        Spinner::OFF,
        WatchSummary::default(),
    );
    text.lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect::<String>()
}

/// Helper: pull the foreground colour of the first span — sufficient
/// for the cell tests since LIMITS only ever emits a single span.
pub(super) fn limits_cell_fg(a: &Agent, now: OffsetDateTime) -> Option<Color> {
    WatchColumn::Limits
        .agent_text(
            a,
            now,
            &[],
            watch_theme(WatchTheme::Classic),
            Spinner::OFF,
            WatchSummary::default(),
        )
        .lines
        .first()
        .and_then(|l| l.spans.first())
        .and_then(|s| s.style.fg)
}

pub(super) fn outcome_with_marker(session: &str) -> RefreshOutcome {
    RefreshOutcome::Full(FullRefresh {
        agents: vec![fake_agent(
            session,
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Idle,
            None,
            None,
            None,
            None,
        )],
        panes: vec![],
        sessions: vec![],
        session_activity: vec![],
        pipeline_runs: vec![],
        error: None,
    })
}

/// Read one full visual row of text from the `TestBackend` buffer. Trims
/// trailing whitespace so callers can substring-match without juggling
/// padding.
pub(super) fn row_text(buf: &ratatui::buffer::Buffer, y: u16) -> String {
    let area = buf.area();
    let mut s = String::with_capacity(usize::from(area.width));
    for x in 0..area.width {
        s.push_str(buf.cell((x, y)).map_or("", ratatui::buffer::Cell::symbol));
    }
    s.trim_end().to_string()
}

/// Build an `App` configured to put the detail line on the Prompt
/// column with three rows of agents. Returns the constructed app.
pub(super) fn three_agent_app(detail: tend::config::DetailConfig) -> App {
    // Pin sort to PaneId so the assertions about which row is at
    // which index don't drift with the default [Workspace, Activity]
    // sort once `fake_agent` timestamps differ across runs.
    let cfg = WatchConfig {
        view: WatchView::Pane,
        detail,
        sort: vec![WatchSortKey::PaneId],
        ..Default::default()
    };
    let mut app = App::with_legacy_config(cfg);
    seed_three_agents(&mut app);
    app
}

/// Drop the canonical three-agent fixture into an arbitrary App —
/// lets tests pick their own `WatchConfig` (e.g. to pin
/// `[watch.preview] default_content`) and then reuse the same
/// agent set the rest of the suite expects.
pub(super) fn seed_three_agents(app: &mut App) {
    let mut a1 = fake_agent(
        "s1",
        Some("%1"),
        AgentKind::ClaudeCode,
        AgentState::Working,
        Some("ALPHAprompt"),
        Some("Opus"),
        Some(7.0),
        Some(0.05),
    );
    a1.last_response = Some("ALPHAresp".into());
    let mut a2 = fake_agent(
        "s2",
        Some("%2"),
        AgentKind::ClaudeCode,
        AgentState::Idle,
        Some("BETAprompt"),
        None,
        None,
        None,
    );
    a2.last_response = Some("BETAresp".into());
    let mut a3 = fake_agent(
        "s3",
        Some("%3"),
        AgentKind::ClaudeCode,
        AgentState::Working,
        Some("GAMMAprompt"),
        None,
        None,
        None,
    );
    a3.last_response = Some("GAMMAresp".into());
    app.set_data(vec![a1, a2, a3], vec![]);
}

/// Drive `handle_event` with a single `Char(c)` keystroke and return
/// the resulting Action. Centralises the boilerplate so each preview
/// test reads as "press X, expect Y".
pub(super) fn key_action(app: &mut App, c: char) -> Action {
    handle_event(
        Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)),
        app,
    )
}

pub(super) fn alt_key_action(app: &mut App, c: char) -> Action {
    handle_event(
        Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)),
        app,
    )
}

/// Press a key and apply the resulting `Action` to `app` the same
/// way the main run loop does. Mirrors the dispatch table in
/// `watch::run` so tests can read as "press X, expect Y" without
/// inlining the open/close/toggle book-keeping every time.
pub(super) fn press(app: &mut App, c: char) {
    let action = if app.preview.is_none() && c.eq_ignore_ascii_case(&'p') {
        alt_key_action(app, c)
    } else {
        key_action(app, c)
    };
    match action {
        Action::OpenPreview => {
            if let Some(pane) = app.selected_pane() {
                app.preview = Some(PreviewState {
                    pane_id: pane,
                    pane_key: None,
                    scroll: 0,
                    mode: PreviewMode::Popup,
                    content: app.watch_cfg.preview.default_content,
                });
            }
        }
        Action::ClosePreview => {
            app.preview = None;
            app.pane_capture = None;
        }
        Action::OpenPreviewFor(key) => open_preview_for(app, key),
        Action::SetBoardSort(sort) => app.board.set_sort(sort),
        Action::OpenWorkUp => app.work_composer = Some(WorkComposer::default()),
        // The real handler shells out to tmux; the test harness stops at
        // the state change so nothing spawns a window.
        Action::SubmitWorkUp => app.work_composer = None,
        // Same reasoning: the run loop applies the rename through tmux, so
        // the harness only takes the composer the way the run loop does.
        Action::SubmitRename => app.rename = None,
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
                p.scroll = 0;
                if matches!(p.content, PreviewContent::PromptResponse) {
                    app.pane_capture = None;
                }
            }
        }
        Action::SetSort(preset) => {
            app.apply_sort_preset(preset);
        }
        Action::SetView(view) => {
            app.apply_view(view);
        }
        Action::SetLayout(layout) => {
            app.apply_layout(layout);
        }
        Action::SetCollabLayout(layout) => {
            app.collab.set_layout(layout);
            app.watch_cfg.collab_layout = layout;
        }
        // The run loop also reloads the fleet listing here; the harness
        // stops at the state change so no test needs a daemon.
        Action::SetScreen(screen) => {
            app.apply_screen(screen);
        }
        // Quick-action paths aren't exercised through `press` —
        // tests that need them call `handle_event` directly so
        // they can inspect the `Action` variant. Anything we
        // encounter here is treated as "no-op" for the existing
        // preview-suite assertions.
        Action::None
        // The real handler calls the daemon; the harness stops here.
        | Action::ClaimBoardInbox { .. }
        | Action::OpenAgentMove
        | Action::SubmitAgentMove(_)
        | Action::Quit
        | Action::TryDemo
        | Action::Refresh
        | Action::AttachPane(_)
        | Action::AttachTopologyPane(_)
        | Action::NewWindow { .. }
        | Action::OpenCollaborationMessage
        | Action::OpenCollaborationMailbox
        | Action::SubmitCollaboration
        | Action::SaveMessageSkill { .. }
        | Action::CancelCollaborationComposer
        | Action::CollaborationDefaultsChanged
        | Action::InspectorSplitChanged
        | Action::OpenAsk
        | Action::SubmitAsk { .. }
        | Action::OpenAskPanel
        | Action::SelectAskConversation(_)
        | Action::CycleAskAgent
        | Action::ClaimCollaborationInbox
        | Action::AskConfirm(_)
        | Action::ConfirmYes
        | Action::ConfirmCancel
        | Action::Quick(_)
        | Action::RestartDaemon
        | Action::NotApplicable(_) => {}
        // Mirrors the run loop: marking and dismissing are pure state.
        Action::ToggleCollaborationMark => {
            let outcome = toggle_collaboration_mark(app);
            apply_outcome_to_app(app, outcome);
        }
        Action::DismissBroadcastReport => app.broadcast_report = None,
    }
}

pub(super) fn work_preview_app() -> App {
    let cfg = WatchConfig {
        view: WatchView::Window,
        sort: vec![WatchSortKey::Name],
        preview: tend::config::PreviewConfig {
            default_content: PreviewContent::PromptResponse,
        },
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    let t0 = time::macros::datetime!(2026-04-28 09:00:00 UTC);
    let t1 = time::macros::datetime!(2026-04-28 10:00:00 UTC);
    let mut alpha = fake_agent_at("alpha", "%1", t0);
    alpha.last_prompt = Some("ALPHAprompt".into());
    alpha.last_response = Some("ALPHAresp".into());
    let mut beta = fake_agent_at("beta", "%2", t1);
    beta.last_prompt = Some("BETAprompt".into());
    beta.last_response = Some("BETAresp".into());
    app.set_data_with_sessions(
        vec![alpha, beta],
        vec![
            fake_pane("%1", "main", 0, 0, "claude"),
            fake_pane("%2", "main", 0, 1, "codex"),
        ],
        vec![fake_session("$1", "main", 1)],
        vec![],
    );
    app.table_state.select(Some(0));
    app
}

/// `c` toggles the preview content axis: `PromptResponse` → `LivePane`
/// → `PromptResponse`. Geometry mode (popup vs fullscreen) is unaffected
/// — the two axes compose. Scroll resets so the new content surface
/// starts at the top instead of mid-line.
/// Overlay preset that opens to `PromptResponse` — used by tests that
/// want to pin the starting content axis instead of inheriting whatever
/// the global default happens to be. Keeps test intent stable across
/// future default flips.
pub(super) fn cfg_with_prompt_default() -> WatchConfig {
    WatchConfig {
        view: WatchView::Pane,
        preview: tend::config::PreviewConfig {
            default_content: PreviewContent::PromptResponse,
        },
        ..WatchConfig::default()
    }
}

/// Recorder stub for `Effects` — captures every call so tests can
/// assert on what the dispatcher tried to do without spawning a
/// real subprocess. Each method's response is configurable so we
/// can exercise both success and failure branches.
#[derive(Default)]
pub(super) struct RecorderEffects {
    kill_calls: Vec<String>,
    ctrl_c_calls: Vec<String>,
    copy_calls: Vec<String>,
    send_prompt_calls: Vec<(String, String)>,
    spawn_calls: Vec<(String, String)>,
    kill_result: Option<std::result::Result<(), String>>,
    ctrl_c_result: Option<std::result::Result<(), String>>,
    copy_result: Option<std::result::Result<String, String>>,
    send_prompt_result: Option<std::result::Result<(), String>>,
}

impl Effects for RecorderEffects {
    fn spawn_work(
        &mut self,
        dir: &str,
        launch: &str,
        work: Option<&str>,
    ) -> std::result::Result<String, String> {
        self.spawn_calls.push((dir.to_string(), launch.to_string()));
        let _ = work;
        Ok("spawned-test".into())
    }

    fn kill_pane(&mut self, pane_id: &str) -> std::result::Result<(), String> {
        self.kill_calls.push(pane_id.to_string());
        self.kill_result.clone().unwrap_or(Ok(()))
    }
    fn send_ctrl_c(&mut self, pane_id: &str) -> std::result::Result<(), String> {
        self.ctrl_c_calls.push(pane_id.to_string());
        self.ctrl_c_result.clone().unwrap_or(Ok(()))
    }
    fn copy_to_clipboard(&mut self, text: &str) -> std::result::Result<String, String> {
        self.copy_calls.push(text.to_string());
        self.copy_result
            .clone()
            .unwrap_or_else(|| Ok("pbcopy".into()))
    }
    fn send_prompt(&mut self, pane_id: &str, text: &str) -> std::result::Result<(), String> {
        self.send_prompt_calls
            .push((pane_id.to_string(), text.to_string()));
        self.send_prompt_result.clone().unwrap_or(Ok(()))
    }
}

/// Build an app that has a paneless agent (Claude SDK sub-process
/// whose ancestry walk failed) at row 0 and a normal pane-bearing
/// agent at row 1. Lets `K` / `R` / `c` tests drive against both
/// shapes without re-doing the fixture each time.
pub(super) fn app_with_paneless_and_pane() -> App {
    let cfg = WatchConfig {
        view: WatchView::Pane,
        // Disable hide_paneless so the row stays in the table
        // — the picker default would filter it out and the test
        // would have to look elsewhere.
        hide_paneless: false,
        sort: vec![WatchSortKey::PaneId],
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    let paneless = fake_agent(
        "no-pane",
        None,
        AgentKind::ClaudeCode,
        AgentState::Idle,
        None,
        None,
        None,
        None,
    );
    let panefull = fake_agent(
        "with-pane",
        Some("%42"),
        AgentKind::ClaudeCode,
        AgentState::Working,
        Some("hello world"),
        None,
        None,
        None,
    );
    app.set_data(
        vec![paneless, panefull],
        vec![fake_pane("%42", "main", 2, 0, "claude")],
    );
    app
}

pub(super) fn collab_participant(pane: &str, session: &str, window: &str) -> Participant {
    Participant {
        launch_purpose: tend::collaboration::LaunchPurpose::Primary,
        draining: false,
        agent_kind: AgentKind::ClaudeCode,
        agent_session_id: format!("agent-{pane}"),
        pane: pane.into(),
        socket: Some("default".into()),
        room: tend::collaboration::RoomId {
            host: "tmux".into(),
            socket: Some("default".into()),
            window_id: "@4".into(),
        },
        tmux_session_id: Some("$1".into()),
        tmux_session_name: Some(session.into()),
        window_name: Some(window.into()),
        state: AgentState::Idle,
        cwd: None,
        alias: None,
        roles: Vec::new(),
        console: false,
    }
}

/// Longer than a row can show, so a frame carrying the tail proves the
/// detail pane rendered rather than the excerpt.
pub(super) const COLLAB_FIXTURE_BODY: &str =
    "check the upload chunking before the multipart retry lands downstream";

pub(super) fn collab_request(to_pane: &str) -> CollaborationRequest {
    CollaborationRequest {
        decision: None,
        turn_offer: None,
        sender_follow: None,
        wake_retry: None,
        user_authorized: false,
        task_contract: None,
        attempt: None,
        supersedes_request_id: None,
        superseded_by: None,
        late_replies: Vec::new(),
        dispatch_id: None,
        source_node_id: None,
        delegation_parent: None,
        interruption: None,
        retirement: None,
        human_action: None,
        initiator: None,
        updates: Vec::new(),
        id: "req_1".into(),
        from: collab_participant("%1", "webapp", "ENG-7330"),
        to: collab_participant(to_pane, "webapp", "ENG-7330"),
        provenance: None,
        kind: RequestKind::Review,
        body: COLLAB_FIXTURE_BODY.into(),
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
        status: RequestStatus::Queued,
        created_at: OffsetDateTime::now_utc(),
        claimed_at: None,
        wake_delivery: None,
        notified_at: None,
        reply_notified_at: None,
        reply_read_at: None,
        reply_wake: None,
        reply: None,
        withdrew_reply_wakes: Vec::new(),
        rebound_from: Vec::new(),
        to_slot: None,
    }
}

/// A watch sitting on the collab screen, with one request whose peer pane
/// is `to_pane` and a topology that really holds `%2`.
pub(super) fn collab_app(to_pane: &str) -> App {
    let mut app = App::with_legacy_config(WatchConfig::default());
    app.set_data(
        Vec::new(),
        vec![fake_topology_pane(
            "default", "$1", "webapp", "@4", "ENG-7330", 4, "%2", 0,
        )],
    );
    app.collab.set_requests(vec![collab_request(to_pane)]);
    app.apply_screen(WatchScreen::Collab);
    app
}

pub(super) fn board_work(id: &str, key: Option<&PaneKey>) -> tend::work::WorkSnapshotItem {
    let at = OffsetDateTime::now_utc();
    tend::work::WorkSnapshotItem {
        identity: tend::work::WorkIdentity {
            workspace_id: "webapp".into(),
            work_id: id.into(),
        },
        title: format!("work {id}"),
        goal: Some("ship it".into()),
        next_action: None,
        stage: tend::work::BoardStage::InProgress,
        idle: None,
        signals: Vec::new(),
        external_items: Vec::new(),
        runs: key
            .map(|key| tend::work::RunSnapshot {
                id: "run".into(),
                state: tend::work::RunState::Running,
                linked: true,
                work: None,
                execution: tend::work::ExecutionIdentity {
                    host: key.window.session.endpoint.host,
                    socket: key.window.session.endpoint.socket.clone(),
                    session_id: key.window.session.session_id.clone(),
                    window_id: key.window.window_id.clone(),
                },
                session_name: "webapp".into(),
                window_name: id.into(),
                window_index: "1".into(),
                cwd: None,
                panes: vec![tend::work::RunPaneSnapshot {
                    pane_id: key.pane_id.clone(),
                    pane_index: "0".into(),
                    current_command: "codex".into(),
                    title: String::new(),
                    current_path: String::new(),
                    attach_command: String::new(),
                    role: None,
                    task: None,
                    agent: None,
                }],
                latest_at: None,
            })
            .into_iter()
            .collect(),
        participants: 0,
        latest_at: Some(at),
        source: tend::work::WorkSource::Persisted,
        metadata: tend::work::WorkMetadata {
            title: None,
            goal: None,
            next_action: None,
            stage: tend::work::WorkStage::default(),
            updated_at: at,
        },
    }
}

pub(super) fn board_snapshot(works: Vec<tend::work::WorkSnapshotItem>) -> tend::work::WorkSnapshot {
    tend::work::WorkSnapshot {
        schema_version: 1,
        generated_at: OffsetDateTime::now_utc(),
        workspaces: Vec::new(),
        works,
        unlinked_executions: Vec::new(),
    }
}

/// A Work whose one Run holds three agent panes on socket a: %1 working,
/// %2 waiting, %3 stopped. Returns the app on the board and the keys.
pub(super) fn board_with_agents() -> (App, Vec<PaneKey>) {
    let mut app = App::with_legacy_config(WatchConfig::default());
    app.set_data(
        Vec::new(),
        ["%1", "%2", "%3"]
            .iter()
            .enumerate()
            .map(|(index, pane)| {
                fake_topology_pane(
                    "/tmp/a",
                    "$1",
                    "webapp",
                    "@4",
                    "ENG-1",
                    1,
                    pane,
                    u32::try_from(index).unwrap(),
                )
            })
            .collect(),
    );
    let keys: Vec<PaneKey> = ["%1", "%2", "%3"]
        .iter()
        .map(|pane| app.pane_key_for(pane, Some("a")).unwrap())
        .collect();
    let mut work = board_work("ENG-1", Some(&keys[0]));
    let template = work.runs[0].panes[0].clone();
    work.runs[0].panes = [
        ("%1", AgentState::Working),
        ("%2", AgentState::WaitingInput),
        ("%3", AgentState::Stopped),
    ]
    .into_iter()
    .map(|(pane, state)| {
        let mut snapshot = template.clone();
        snapshot.pane_id = pane.into();
        snapshot.agent = Some(fake_agent(
            pane,
            Some(pane),
            AgentKind::Codex,
            state,
            None,
            None,
            None,
            None,
        ));
        snapshot
    })
    .collect();
    app.board
        .set_snapshot(board_snapshot(vec![work]), Vec::new());
    app.apply_screen(WatchScreen::Board);
    (app, keys)
}

pub(super) fn press_key(app: &mut App, code: KeyCode) -> Action {
    handle_event(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)), app)
}

/// The board with agents, plus the topology tracking the agent on %1 so
/// a topology interrupt has a real target.
pub(super) fn board_with_tracked_topology_agent() -> (App, Vec<PaneKey>) {
    let (mut app, keys) = board_with_agents();
    let panes: Vec<PaneInfo> = ["%1", "%2", "%3"]
        .iter()
        .enumerate()
        .map(|(index, pane)| {
            fake_topology_pane(
                "/tmp/a",
                "$1",
                "webapp",
                "@4",
                "ENG-1",
                1,
                pane,
                u32::try_from(index).unwrap(),
            )
        })
        .collect();
    app.set_data(
        vec![topology_agent("%1", "%1", "a", AgentState::Working, "", 0)],
        panes,
    );
    (app, keys)
}

pub(super) fn board_ready_to_message() -> (App, Vec<PaneKey>) {
    let (mut app, keys) = board_with_tracked_topology_agent();
    app.collaboration_scope = tend::config::CollaborationScope::Host;
    app.collaboration.origin = Some(watch_collaboration_origin(Some("%99".into())));
    app.exact_session_supported = true;
    (app, keys)
}

/// Topology cursor on pane A (%1); the board on Work B, whose Run is %2.
pub(super) fn board_on_b_topology_on_a() -> (App, PaneKey) {
    let mut app = App::with_legacy_config(WatchConfig::default());
    app.set_data(
        Vec::new(),
        vec![
            fake_topology_pane("/tmp/a", "$1", "webapp", "@4", "ENG-A", 1, "%1", 0),
            fake_topology_pane("/tmp/a", "$1", "webapp", "@5", "ENG-B", 2, "%2", 0),
        ],
    );
    let b = app.pane_key_for("%2", Some("a")).expect("pane B");
    app.board.set_snapshot(
        board_snapshot(vec![board_work("ENG-B", Some(&b))]),
        Vec::new(),
    );
    app.apply_screen(WatchScreen::Board);
    (app, b)
}

pub(super) fn rename_app(level: RenameLevel, original: &str) -> App {
    let mut app = App::with_legacy_config(WatchConfig::default());
    app.rename = Some(RenameComposer {
        key: TopologyNodeKey::Window(tend::WindowKey {
            session: tend::SessionKey {
                endpoint: tend::BackendEndpoint {
                    host: tend::HostKind::Tmux,
                    socket: "default".into(),
                },
                session_id: "$1".into(),
            },
            window_id: "@4".into(),
        }),
        level,
        original: original.into(),
        input: original.into(),
        cursor: original.chars().count(),
    });
    app
}

mod snapshot_helpers {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Flatten the `TestBackend` buffer into a single visual string.
    /// One line per buffer row, trailing whitespace trimmed per line,
    /// relative-time tokens normalized so snapshots stay stable.
    pub(super) fn buffer_string(terminal: &Terminal<TestBackend>) -> String {
        let buf = terminal.backend().buffer();
        let area = buf.area();
        let mut out = String::with_capacity(usize::from(area.width + 1) * usize::from(area.height));
        for y in 0..area.height {
            let mut row = String::with_capacity(usize::from(area.width));
            for x in 0..area.width {
                row.push_str(buf.cell((x, y)).map_or("", ratatui::buffer::Cell::symbol));
            }
            let row = row.trim_end();
            out.push_str(&normalize_relative_time(row));
            out.push('\n');
        }
        out
    }

    /// Replace compact `\d+(s|m|h|d)` tokens with `<rel>` and `HH:MM:SS UTC` with
    /// `<clock>` so neither the activity column nor the header clock
    /// drifts across runs. (`render_header` reads `app.last_refresh`,
    /// which is set to `now_utc()` inside `App::with_legacy_config`/`set_data`
    /// — not injectable without a production-code change.)
    fn normalize_relative_time(s: &str) -> String {
        let bytes = s.as_bytes();
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        while i < bytes.len() {
            if let Some(consumed) = match_clock(&bytes[i..]) {
                out.push_str("<clock>");
                i += consumed;
                continue;
            }
            let digit_start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i > digit_start
                && i < bytes.len()
                && matches!(bytes[i], b's' | b'm' | b'h' | b'd')
                && bytes.get(i + 1).is_none_or(u8::is_ascii_whitespace)
            {
                // Eat the digits + unit AND any spaces that follow
                // (column padding). The relative-time text is
                // variable-length (2-4 chars) so without this the
                // column-trailing whitespace would drift across runs
                // as the value crosses the 60s / 60m / 48h boundaries.
                // Keep one separator so adjacent columns still read like
                // the production table after normalization.
                i += 1;
                let space_start = i;
                while i < bytes.len() && bytes[i] == b' ' {
                    i += 1;
                }
                out.push_str("<rel>");
                if i > space_start {
                    out.push(' ');
                }
            } else {
                // Not a relative-time token — copy the digit run and the
                // following char (if any) verbatim.
                out.push_str(&s[digit_start..i]);
                if i < bytes.len() {
                    let ch_start = i;
                    // Advance one UTF-8 char.
                    i += 1;
                    while i < bytes.len() && (bytes[i] & 0xC0) == 0x80 {
                        i += 1;
                    }
                    out.push_str(&s[ch_start..i]);
                }
            }
        }
        out
    }

    /// Return `Some(12)` if `b` starts with `HH:MM:SS UTC`, else `None`.
    fn match_clock(b: &[u8]) -> Option<usize> {
        if b.len() < 12 {
            return None;
        }
        let d = |i: usize| b[i].is_ascii_digit();
        if d(0)
            && d(1)
            && b[2] == b':'
            && d(3)
            && d(4)
            && b[5] == b':'
            && d(6)
            && d(7)
            && &b[8..12] == b" UTC"
        {
            Some(12)
        } else {
            None
        }
    }
}

pub(super) fn snapshot_app() -> App {
    // Pin sort to PaneId so row order doesn't drift on activity jitter
    // and disable hide_paneless so the paneless row stays visible in
    // the mixed-list snapshot. Pin pane view too so these row-rendering
    // snapshots stay focused on per-row visuals (state glyphs, columns,
    // selection) independent of the production default (`work`).
    let cfg = WatchConfig {
        view: WatchView::Pane,
        sort: vec![WatchSortKey::PaneId],
        hide_paneless: false,
        // Static glyphs so the golden snapshots pin layout, not an
        // animation frame. The spinner has its own dedicated test.
        spinner: false,
        ..WatchConfig::default()
    };
    App::with_legacy_config(cfg)
}

#[test]
fn snapshot_empty_state() {
    let backend = TestBackend::new(100, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = snapshot_app();
    app.demo_available = false;
    terminal.draw(|f| render(f, &mut app)).unwrap();
    insta::assert_snapshot!(snapshot_helpers::buffer_string(&terminal));
}

#[test]
fn snapshot_nested_topology_tree() {
    let panes = vec![
        fake_topology_pane("default", "$1", "tend", "@1", "TEST-0001", 0, "%12", 0),
        fake_topology_pane("default", "$1", "tend", "@1", "TEST-0001", 0, "%13", 1),
        fake_topology_pane("default", "$1", "tend", "@2", "TEST-0002", 1, "%18", 0),
        fake_topology_pane("amux", "$1", "junia", "@1", "DOCS", 0, "%1", 0),
    ];
    let agents = vec![
        topology_agent(
            "claude-impl",
            "%12",
            "default",
            AgentState::Working,
            "editing middleware",
            1,
        ),
        topology_agent(
            "codex-review",
            "%13",
            "default",
            AgentState::WaitingChoice,
            "reviewing changes",
            2,
        ),
        topology_agent(
            "codex-wait",
            "%18",
            "default",
            AgentState::Idle,
            "docs update",
            3,
        ),
        topology_agent(
            "junia-agent",
            "%1",
            "amux",
            AgentState::WaitingInput,
            "release approval",
            4,
        ),
    ];
    let mut app = topology_watch(WatchView::Pane, agents, panes);
    app.watch_cfg.spinner = false;
    let junia_key = app
        .topology
        .sessions
        .iter()
        .find(|session| session.name == "junia")
        .unwrap()
        .node_key();
    app.expanded_nodes.remove(&junia_key);
    let tend_window_key = app
        .topology
        .sessions
        .iter()
        .find(|session| session.name == "tend")
        .unwrap()
        .windows[0]
        .node_key();
    select_tree_key(&mut app, &tend_window_key);

    let backend = TestBackend::new(118, 16);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    insta::assert_snapshot!(snapshot_helpers::buffer_string(&terminal));
}

#[test]
fn oh_my_tend_theme_renders_polished_chrome() {
    let backend = TestBackend::new(100, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::with_legacy_config(WatchConfig {
        theme: Some(WatchTheme::OhMyTend),
        view: WatchView::Pane,
        sort: vec![WatchSortKey::PaneId],
        ..WatchConfig::default()
    });
    terminal.draw(|f| render(f, &mut app)).unwrap();
    let dump = snapshot_helpers::buffer_string(&terminal);

    assert!(dump.contains("oh-my-tend"));
    assert!(
        dump.contains("╭") && dump.contains("╯"),
        "oh-my-tend should switch watch chrome to rounded borders"
    );
}

#[test]
fn watch_theme_presets_render_named_chrome() {
    for (theme, title) in [
        (WatchTheme::Focus, "tend focus"),
        (WatchTheme::Ops, "tend ops"),
        (WatchTheme::Mono, "tend mono"),
        (WatchTheme::HighContrast, "tend high-contrast"),
        (WatchTheme::Minimal, "tend"),
    ] {
        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::with_legacy_config(WatchConfig {
            theme: Some(theme),
            view: WatchView::Pane,
            sort: vec![WatchSortKey::PaneId],
            ..WatchConfig::default()
        });
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let dump = snapshot_helpers::buffer_string(&terminal);

        assert!(
            dump.contains(title),
            "expected theme title {title:?} in render dump:\n{dump}"
        );
    }
}

#[test]
fn snapshot_row_working() {
    let backend = TestBackend::new(100, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = snapshot_app();
    app.set_data(
        vec![fake_agent(
            "s-work",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Working,
            Some("refactor the ipc module"),
            Some("Opus"),
            Some(34.0),
            Some(0.12),
        )],
        vec![fake_pane("%1", "alpha", 0, 0, "claude")],
    );
    terminal.draw(|f| render(f, &mut app)).unwrap();
    insta::assert_snapshot!(snapshot_helpers::buffer_string(&terminal));
}

#[test]
fn snapshot_row_waiting_input() {
    let backend = TestBackend::new(100, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = snapshot_app();
    app.set_data(
        vec![fake_agent(
            "s-wi",
            Some("%2"),
            AgentKind::Codex,
            AgentState::WaitingInput,
            Some("approve permission?"),
            None,
            None,
            None,
        )],
        vec![fake_pane("%2", "alpha", 1, 0, "codex")],
    );
    terminal.draw(|f| render(f, &mut app)).unwrap();
    insta::assert_snapshot!(snapshot_helpers::buffer_string(&terminal));
}

#[test]
fn snapshot_row_waiting_choice() {
    // Regression guard: WaitingChoice is the newest AgentState variant
    // and the one we most want to pin visually so future styling/label
    // changes show up in review.
    let backend = TestBackend::new(100, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = snapshot_app();
    app.set_data(
        vec![fake_agent(
            "s-wc",
            Some("%3"),
            AgentKind::ClaudeCode,
            AgentState::WaitingChoice,
            Some("pick a plan"),
            None,
            None,
            None,
        )],
        vec![fake_pane("%3", "alpha", 2, 0, "claude")],
    );
    terminal.draw(|f| render(f, &mut app)).unwrap();
    insta::assert_snapshot!(snapshot_helpers::buffer_string(&terminal));
}

#[test]
fn snapshot_row_error() {
    let backend = TestBackend::new(100, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = snapshot_app();
    app.set_data(
        vec![fake_agent(
            "s-err",
            Some("%4"),
            AgentKind::Antigravity,
            AgentState::Error,
            Some("boom"),
            None,
            None,
            None,
        )],
        vec![fake_pane("%4", "alpha", 3, 0, "agy")],
    );
    terminal.draw(|f| render(f, &mut app)).unwrap();
    insta::assert_snapshot!(snapshot_helpers::buffer_string(&terminal));
}

#[test]
fn snapshot_mixed_list_with_selection() {
    // One agent per state, plus a paneless agent — selection on the
    // second row so the detail-line expansion is exercised too.
    let backend = TestBackend::new(110, 16);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = snapshot_app();
    let mut a1 = fake_agent(
        "s1",
        Some("%1"),
        AgentKind::ClaudeCode,
        AgentState::Working,
        Some("alpha prompt"),
        Some("Opus"),
        Some(7.0),
        Some(0.05),
    );
    a1.last_response = Some("alpha response".into());
    let mut a2 = fake_agent(
        "s2",
        Some("%2"),
        AgentKind::Codex,
        AgentState::WaitingInput,
        Some("beta prompt"),
        None,
        None,
        None,
    );
    a2.last_response = Some("beta response".into());
    let mut a3 = fake_agent(
        "s3",
        Some("%3"),
        AgentKind::ClaudeCode,
        AgentState::WaitingChoice,
        Some("gamma prompt"),
        None,
        None,
        None,
    );
    a3.last_response = Some("gamma response".into());
    let a4 = fake_agent(
        "s4",
        None,
        AgentKind::Unknown,
        AgentState::Idle,
        None,
        None,
        None,
        None,
    );
    app.set_data(
        vec![a1, a2, a3, a4],
        vec![
            fake_pane("%1", "alpha", 0, 0, "claude"),
            fake_pane("%2", "alpha", 1, 0, "codex"),
            fake_pane("%3", "alpha", 2, 0, "claude"),
        ],
    );
    app.table_state.select(Some(1));
    terminal.draw(|f| render(f, &mut app)).unwrap();
    insta::assert_snapshot!(snapshot_helpers::buffer_string(&terminal));
}

#[test]
fn snapshot_help_overlay() {
    let backend = TestBackend::new(140, 40);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = snapshot_app();
    app.set_data(
        vec![fake_agent(
            "s1",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Idle,
            Some("hi"),
            None,
            None,
            None,
        )],
        vec![fake_pane("%1", "alpha", 0, 0, "claude")],
    );
    app.help_open = true;
    terminal.draw(|f| render(f, &mut app)).unwrap();
    insta::assert_snapshot!(snapshot_helpers::buffer_string(&terminal));
}

#[test]
fn snapshot_confirm_popup() {
    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = snapshot_app();
    app.set_data(
        vec![fake_agent(
            "s1",
            Some("%42"),
            AgentKind::ClaudeCode,
            AgentState::Working,
            Some("hello"),
            None,
            None,
            None,
        )],
        vec![fake_pane("%42", "main", 2, 0, "claude")],
    );
    app.confirm = Some(ConfirmPopup {
        message: "Kill pane main:2.0?".into(),
        on_confirm: ConfirmAction::Quick(QuickAction::KillPane("%42".into())),
    });
    terminal.draw(|f| render(f, &mut app)).unwrap();
    insta::assert_snapshot!(snapshot_helpers::buffer_string(&terminal));
}

#[test]
fn snapshot_preview_popup() {
    let backend = TestBackend::new(120, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = snapshot_app();
    let mut a = fake_agent(
        "s1",
        Some("%1"),
        AgentKind::ClaudeCode,
        AgentState::Working,
        Some("the prompt body"),
        Some("Opus"),
        Some(12.0),
        Some(0.03),
    );
    a.last_response = Some("the response body".into());
    app.set_data(vec![a], vec![fake_pane("%1", "alpha", 0, 0, "claude")]);
    app.preview = Some(PreviewState {
        pane_id: "%1".into(),
        pane_key: None,
        scroll: 0,
        mode: PreviewMode::Popup,
        content: PreviewContent::PromptResponse,
    });
    terminal.draw(|f| render(f, &mut app)).unwrap();
    insta::assert_snapshot!(snapshot_helpers::buffer_string(&terminal));
}

mod board;
mod collaboration;
mod forms;
mod input;
mod preview;
mod refresh;
mod render;
mod rows;
mod topology;
