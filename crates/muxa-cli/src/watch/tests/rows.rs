use super::*;

#[test]
fn a_folded_workspace_counts_every_terminal_on_it() {
    // Two terminals on one workspace via a session group: tmux reports one
    // attached client per member, and topology shows the members as a
    // single node. Reporting either member's own count would understate
    // the workspace by exactly the terminals the group exists to allow.
    let all = vec![
        grouped_session("$0", "callabo", 1, "callabo"),
        grouped_session("$1", "view~9~callabo", 1, "callabo"),
        fake_session("$2", "unrelated", 3),
    ];
    assert_eq!(group_attached_clients(&all[0], &all), 2);
    // Either member answers for the whole group — the caller does not have
    // to know which one topology kept.
    assert_eq!(group_attached_clients(&all[1], &all), 2);
    // An ungrouped session is only ever itself.
    assert_eq!(group_attached_clients(&all[2], &all), 3);
}

#[test]
fn a_detached_group_member_adds_nothing() {
    // A view whose terminal has gone is still a session until tmux reaps
    // it. It must not inflate the count while it lingers.
    let all = vec![
        grouped_session("$0", "callabo", 1, "callabo"),
        grouped_session("$1", "view~9~callabo", 0, "callabo"),
    ];
    assert_eq!(group_attached_clients(&all[0], &all), 1);
}

#[test]
fn render_does_not_panic_on_test_backend() {
    let backend = TestBackend::new(140, 20);
    let mut terminal = Terminal::new(backend).unwrap();

    let mut app = App::new();
    app.set_data(
        vec![
            fake_agent(
                "sess-a",
                Some("%10"),
                AgentKind::ClaudeCode,
                AgentState::Working,
                Some(
                    "refactor the ipc module to use generics across multiple lines\nand keep going",
                ),
                Some("Opus"),
                Some(34.0),
                Some(0.12),
            ),
            fake_agent(
                "sess-b",
                Some("%11"),
                AgentKind::Codex,
                AgentState::WaitingInput,
                None,
                None,
                None,
                None,
            ),
            fake_agent(
                "sess-c",
                Some("%12"),
                AgentKind::GeminiCli,
                AgentState::Idle,
                Some("summarize this PR"),
                Some("Gemini"),
                Some(12.5),
                Some(0.01),
            ),
            fake_agent(
                "sess-d",
                None,
                AgentKind::Unknown,
                AgentState::Error,
                None,
                None,
                None,
                None,
            ),
        ],
        vec![
            fake_pane("%30", "work", 0, 0, "vim"),
            fake_pane("%31", "work", 1, 0, "cargo build"),
        ],
    );

    terminal.draw(|f| render(f, &mut app)).unwrap();
}

#[test]
fn agents_group_by_work_window_then_pane_index() {
    // Agents from sessions "alpha" and "beta" interleaved by pane id,
    // plus one stale agent whose pane no longer exists. Expect:
    //   1. all alpha agents grouped, then all beta agents grouped
    //   2. within a session, ordered by window then pane index
    //   3. stale agent at the end
    //
    // Uses an explicit `[Workspace, Pane]` sort so the assertion stays
    // independent of `last_activity_at` jitter from `fake_agent`. The
    // default `[Workspace, Activity]` is exercised by separate tests.
    let cfg = WatchConfig {
        view: WatchView::Pane,
        sort: vec![WatchSortKey::Name, WatchSortKey::Pane],
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    let mk = |session_id: &str, pane: &str| {
        fake_agent(
            session_id,
            Some(pane),
            AgentKind::ClaudeCode,
            AgentState::Idle,
            None,
            None,
            None,
            None,
        )
    };
    app.set_data(
        vec![
            mk("s-beta-1", "%50"),
            mk("s-alpha-2", "%20"),
            mk("s-stale", "%999"), // pane not in inventory
            mk("s-alpha-1", "%10"),
            mk("s-beta-2", "%40"),
        ],
        vec![
            fake_pane("%10", "alpha", 0, 0, "claude"),
            fake_pane("%20", "alpha", 1, 0, "claude"),
            fake_pane("%40", "beta", 0, 0, "claude"),
            fake_pane("%50", "beta", 0, 1, "claude"),
        ],
    );

    let agent_pane_ids: Vec<&str> = app
        .rows
        .iter()
        .filter_map(|r| match r {
            WatchRow::Agent(a) => a.pane.as_deref(),
            WatchRow::BarePane(_) | WatchRow::Work(_) => None,
        })
        .collect();

    assert_eq!(
        agent_pane_ids,
        vec!["%10", "%20", "%40", "%50", "%999"],
        "expected grouping: alpha (w0p0, w1p0), beta (w0p0, w0p1), then stale"
    );
}

#[test]
fn agent_window_pane_indices_sort_numerically_not_lex() {
    // "10" must sort AFTER "2" within a session — string comparison
    // would invert that. Regression guard for the parse::<u32>() path.
    // Uses explicit `Pane` sort so activity-jitter doesn't matter.
    let cfg = WatchConfig {
        view: WatchView::Pane,
        sort: vec![WatchSortKey::Name, WatchSortKey::Pane],
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    let mk = |session_id: &str, pane: &str| {
        fake_agent(
            session_id,
            Some(pane),
            AgentKind::ClaudeCode,
            AgentState::Idle,
            None,
            None,
            None,
            None,
        )
    };
    app.set_data(
        vec![mk("a", "%2"), mk("a", "%10"), mk("a", "%1")],
        vec![
            fake_pane("%1", "main", 0, 0, "x"),
            fake_pane("%2", "main", 0, 1, "x"),
            fake_pane("%10", "main", 0, 10, "x"),
        ],
    );

    let order: Vec<&str> = app
        .rows
        .iter()
        .filter_map(|r| match r {
            WatchRow::Agent(a) => a.pane.as_deref(),
            WatchRow::BarePane(_) | WatchRow::Work(_) => None,
        })
        .collect();
    assert_eq!(order, vec!["%1", "%2", "%10"]);
}

#[test]
fn multi_socket_topology_does_not_merge_or_hide_duplicate_native_ids() {
    let mut default_pane = fake_pane("%1", "main", 0, 0, "codex");
    default_pane.socket = Some("default".into());
    default_pane.session_id = "$1".into();
    default_pane.window_id = "@1".into();
    default_pane.window_name = "ticket".into();
    let mut amux_pane = default_pane.clone();
    amux_pane.socket = Some("amux".into());
    amux_pane.session = "other".into();

    let mut tracked = fake_agent_at("agent-default", "%1", OffsetDateTime::now_utc());
    tracked.tmux_socket = Some("default".into());

    // Pane presentation: the tracked default/%1 consumes only its exact
    // key. amux/%1 remains a bare selectable row instead of disappearing
    // through a raw-pane-id `HashSet` join.
    let mut pane_app = App::new();
    pane_app.set_data(
        vec![tracked.clone()],
        vec![default_pane.clone(), amux_pane.clone()],
    );
    assert_eq!(pane_app.topology.sessions.len(), 2);
    assert_eq!(
        pane_app
            .rows
            .iter()
            .filter(|row| matches!(row, WatchRow::BarePane(_)))
            .count(),
        1
    );
    let bare = pane_app
        .rows
        .iter()
        .find_map(|row| match row {
            WatchRow::BarePane(pane) => Some(pane),
            WatchRow::Agent(_) | WatchRow::Work(_) => None,
        })
        .expect("foreign socket pane remains visible");
    assert_eq!(bare.socket.as_deref(), Some("amux"));

    // Window presentation: the two identical `$1/@1` native ids become
    // two WorkRow compatibility shells backed by different WindowKeys.
    let cfg = WatchConfig {
        view: WatchView::Window,
        ..WatchConfig::default()
    };
    let mut window_app = App::with_legacy_config(cfg);
    window_app.set_data_with_sessions(
        vec![tracked],
        vec![default_pane, amux_pane],
        Vec::new(),
        Vec::new(),
    );
    let keys: HashSet<_> = window_app
        .rows
        .iter()
        .filter_map(|row| match row {
            WatchRow::Work(window) => window.window_key.clone(),
            WatchRow::Agent(_) | WatchRow::BarePane(_) => None,
        })
        .collect();
    assert_eq!(keys.len(), 2);
}

#[test]
fn summary_line_falls_through_recap_then_title_then_prompt() {
    let mut a = fake_agent_at("s", "%1", OffsetDateTime::now_utc());
    a.last_prompt = Some("do the thing".into());

    // Only a prompt: every mode shows it — the column never blanks.
    assert_eq!(summary_line(&a, WatchSummary::Recap), "do the thing");
    assert_eq!(summary_line(&a, WatchSummary::Title), "do the thing");
    assert_eq!(summary_line(&a, WatchSummary::Prompt), "do the thing");

    // A session title outranks the prompt, except in prompt-only mode.
    a.ai_title = Some("infra cleanup".into());
    assert_eq!(summary_line(&a, WatchSummary::Recap), "infra cleanup");
    assert_eq!(summary_line(&a, WatchSummary::Title), "infra cleanup");
    assert_eq!(summary_line(&a, WatchSummary::Prompt), "do the thing");

    // A recap outranks both, and collapses to its first line.
    a.recap = Some("Redis restored; MinIO left.\ndropped".into());
    assert_eq!(
        summary_line(&a, WatchSummary::Recap),
        "Redis restored; MinIO left."
    );
    assert_eq!(summary_line(&a, WatchSummary::Title), "infra cleanup");

    // A completed response must displace the old compaction recap.
    a.last_response = Some("Four reviews complete; CI blocked.\nDetails".into());
    assert_eq!(
        summary_line(&a, WatchSummary::Recap),
        "Four reviews complete; CI blocked."
    );
    assert_eq!(summary_line(&a, WatchSummary::Title), "infra cleanup");
    assert_eq!(summary_line(&a, WatchSummary::Prompt), "do the thing");
    assert!(inspector_agent_summary(&a).starts_with("Four reviews complete"));
    a.last_response = Some("  \n".into());
    assert_eq!(
        summary_line(&a, WatchSummary::Recap),
        "Redis restored; MinIO left."
    );

    // No recap or title source, and nothing typed yet — renders the
    // placeholder rather than an empty cell (the common Gemini shape).
    a.recap = None;
    a.ai_title = None;
    a.last_prompt = None;
    assert_eq!(summary_line(&a, WatchSummary::Recap), "-");
}

#[test]
fn default_sort_keeps_work_grouping_and_floats_latest_activity_in_each_group() {
    // Default config = [Workspace, Activity]. Two workspaces with two
    // agents each at staggered timestamps. Expect:
    //   - alpha group first, then beta group (workspace asc)
    //   - newest agent at top within each group
    let t0 = time::macros::datetime!(2026-04-28 09:00:00 UTC);
    let t1 = time::macros::datetime!(2026-04-28 10:00:00 UTC);
    let t2 = time::macros::datetime!(2026-04-28 11:00:00 UTC);
    let t3 = time::macros::datetime!(2026-04-28 12:00:00 UTC);

    let mut app = App::new();
    app.set_data(
        vec![
            fake_agent_at("a-old", "%10", t0),
            fake_agent_at("a-new", "%11", t2),
            fake_agent_at("b-old", "%20", t1),
            fake_agent_at("b-new", "%21", t3),
        ],
        vec![
            fake_pane("%10", "alpha", 0, 0, "claude"),
            fake_pane("%11", "alpha", 0, 1, "claude"),
            fake_pane("%20", "beta", 0, 0, "claude"),
            fake_pane("%21", "beta", 0, 1, "claude"),
        ],
    );

    let order: Vec<&str> = app
        .rows
        .iter()
        .filter_map(|r| match r {
            WatchRow::Agent(a) => a.pane.as_deref(),
            WatchRow::BarePane(_) | WatchRow::Work(_) => None,
        })
        .collect();
    // alpha: %11 (newer t2) before %10 (older t0); then beta: %21
    // (newer t3) before %20 (older t1).
    assert_eq!(order, vec!["%11", "%10", "%21", "%20"]);
}

#[test]
fn push_resorts_rows_so_a_state_change_moves_immediately() {
    // sort = [State, Activity]. A pushed `Error` transition must move the
    // row to its sorted position now, not on the next 5 s `Full` tick —
    // otherwise the badge updates but the row stays put ("한 박자 늦음").
    let t_old = time::macros::datetime!(2026-04-28 09:00:00 UTC);
    let t_new = time::macros::datetime!(2026-04-28 10:00:00 UTC);
    let cfg = WatchConfig {
        view: WatchView::Pane,
        sort: vec![WatchSortKey::State, WatchSortKey::Activity],
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    app.set_data(
        vec![
            fake_agent_at("a", "%1", t_old), // Idle, older
            fake_agent_at("b", "%2", t_new), // Idle, newer
        ],
        vec![
            fake_pane("%1", "main", 0, 0, "claude"),
            fake_pane("%2", "main", 0, 1, "claude"),
        ],
    );

    let order = |app: &App| -> Vec<String> {
        app.rows
            .iter()
            .filter_map(|r| match r {
                WatchRow::Agent(a) => a.pane.clone(),
                WatchRow::BarePane(_) | WatchRow::Work(_) => None,
            })
            .collect()
    };

    // Both Idle → newer activity floats first, so %2 leads, %1 trails.
    assert_eq!(order(&app), vec!["%2", "%1"]);

    // Push: agent "a" (pane %1) flips to Error (state rank 0 → top).
    let mut updated = fake_agent_at("a", "%1", t_old);
    updated.state = AgentState::Error;
    apply_outcome(&mut app, RefreshOutcome::SingleAgent(updated));

    // The re-sort ran as part of the push, so %1 is already on top.
    assert_eq!(order(&app), vec!["%1", "%2"]);
}

#[test]
fn work_view_collapses_panes_to_latest_active_agent() {
    let t0 = time::macros::datetime!(2026-04-28 09:00:00 UTC);
    let t1 = time::macros::datetime!(2026-04-28 10:00:00 UTC);
    let cfg = WatchConfig {
        view: WatchView::Window,
        sort: vec![WatchSortKey::Name],
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    app.set_data_with_sessions(
        vec![
            fake_agent_at("old", "%1", t0),
            fake_agent_at("new", "%2", t1),
        ],
        vec![
            fake_pane("%1", "main", 0, 0, "claude"),
            fake_pane("%2", "main", 0, 1, "codex"),
            fake_pane("%3", "side", 0, 0, "vim"),
        ],
        vec![fake_session("$1", "main", 1), fake_session("$2", "side", 0)],
        vec![],
    );

    assert_eq!(app.rows.len(), 2);
    let WatchRow::Work(main) = &app.rows[0] else {
        panic!("expected work row");
    };
    assert_eq!(main.session, "main");
    assert_eq!(main.representative_pane.as_deref(), Some("%2"));
    assert_eq!(
        main.latest_agent.as_ref().map(|a| a.session_id.as_str()),
        Some("new")
    );
    assert_eq!(app.selected_pane().as_deref(), Some("%2"));
}

/// Regression: a full refresh that reorders the list must not drag the
/// highlight onto a neighbouring session.
///
/// Reported as "the list looks shifted by one — I hit Enter twice on
/// `muxa` and the `amux` session opened". `set_data_with_sessions`
/// rebuilds and re-sorts on every refresh (~2 Hz), and the default sort
/// leads with `state`; when `amux` goes Error it jumps above `muxa`, so
/// holding the raw table index left the cursor on `amux`.
#[test]
fn full_refresh_reorder_keeps_the_cursor_on_the_same_work() {
    let now = time::macros::datetime!(2026-04-28 10:00:00 UTC);
    let cfg = WatchConfig {
        view: WatchView::Window,
        sort: vec![
            WatchSortKey::State,
            WatchSortKey::Name,
            WatchSortKey::Activity,
        ],
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);

    let panes = || {
        vec![
            fake_pane("%1", "amux", 0, 0, "claude"),
            fake_pane("%2", "muxa", 0, 0, "claude"),
        ]
    };
    let sessions = || vec![fake_session("$1", "amux", 0), fake_session("$2", "muxa", 0)];
    let session_names = |app: &App| -> Vec<String> {
        app.rows
            .iter()
            .filter_map(|r| match r {
                WatchRow::Work(s) => Some(s.session.clone()),
                WatchRow::Agent(_) | WatchRow::BarePane(_) => None,
            })
            .collect()
    };

    // `muxa` is blocked on an error, so it leads; `amux` is idle below it.
    let mut muxa_agent = fake_agent_at("m", "%2", now);
    muxa_agent.state = AgentState::Error;
    app.set_data_with_sessions(
        vec![fake_agent_at("a", "%1", now), muxa_agent],
        panes(),
        sessions(),
        vec![],
    );
    assert_eq!(session_names(&app), vec!["muxa", "amux"]);

    // The user puts the cursor on `muxa` (row 0 here).
    app.table_state.select(Some(0));
    assert_eq!(app.selected_pane().as_deref(), Some("%2"));

    // Next refresh: `muxa` recovers to Idle and `amux` errors, so the two
    // rows swap. The cursor must travel with `muxa`, not stay on row 0.
    let mut amux_agent = fake_agent_at("a", "%1", now);
    amux_agent.state = AgentState::Error;
    app.set_data_with_sessions(
        vec![amux_agent, fake_agent_at("m", "%2", now)],
        panes(),
        sessions(),
        vec![],
    );
    assert_eq!(session_names(&app), vec!["amux", "muxa"]);
    assert_eq!(app.table_state.selected(), Some(1));
    assert_eq!(app.selected_pane().as_deref(), Some("%2"));
    // …and Enter therefore attaches to the pane the user is looking at.
    assert!(
        matches!(quick_prompt_action(&app), Action::AttachPane(p) if p == "%2"),
        "Enter must target the highlighted session's pane"
    );
}

/// A work row's `representative_pane` is whichever of its agents was
/// most recently active, so it changes on its own. Pinning the cursor by
/// pane id alone would lose the row; identity is pinned by session name.
#[test]
fn refresh_keeps_the_cursor_when_the_representative_pane_changes() {
    let t0 = time::macros::datetime!(2026-04-28 09:00:00 UTC);
    let t1 = time::macros::datetime!(2026-04-28 10:00:00 UTC);
    let t2 = time::macros::datetime!(2026-04-28 11:00:00 UTC);
    let cfg = WatchConfig {
        view: WatchView::Window,
        sort: vec![WatchSortKey::Activity],
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    let panes = vec![
        fake_pane("%1", "side", 0, 0, "claude"),
        fake_pane("%2", "muxa", 0, 0, "claude"),
        fake_pane("%3", "muxa", 0, 1, "codex"),
    ];
    let sessions = vec![fake_session("$1", "side", 0), fake_session("$2", "muxa", 0)];

    app.set_data_with_sessions(
        vec![
            fake_agent_at("s", "%1", t0),
            fake_agent_at("m1", "%2", t1),
            fake_agent_at("m2", "%3", t0),
        ],
        panes.clone(),
        sessions.clone(),
        vec![],
    );
    assert_eq!(app.table_state.selected(), Some(0));
    assert_eq!(app.selected_pane().as_deref(), Some("%2"));

    // `side` becomes the most recently active session so the rows swap,
    // and inside `muxa` the codex pane %3 overtakes %2 as representative.
    // Both moved — the cursor still belongs on `muxa`.
    app.set_data_with_sessions(
        vec![
            fake_agent_at("s", "%1", t2),
            fake_agent_at("m1", "%2", t0),
            fake_agent_at("m2", "%3", t1),
        ],
        panes,
        sessions,
        vec![],
    );
    assert_eq!(app.table_state.selected(), Some(1));
    let WatchRow::Work(selected) = app.selected_row().expect("selection survives") else {
        panic!("expected work row");
    };
    assert_eq!(selected.session, "muxa");
    assert_eq!(selected.representative_pane.as_deref(), Some("%3"));
}

#[test]
fn work_view_label_summarizes_agent_states() {
    let now = time::macros::datetime!(2026-04-28 10:00:00 UTC);
    let cfg = WatchConfig {
        view: WatchView::Window,
        sort: vec![WatchSortKey::Name],
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    let mut working = fake_agent_at("working", "%1", now);
    working.state = AgentState::Working;
    let mut working_2 = fake_agent_at("working-2", "%5", now);
    working_2.state = AgentState::Working;
    let mut waiting = fake_agent_at("waiting", "%2", now);
    waiting.state = AgentState::WaitingInput;
    let mut idle = fake_agent_at("idle", "%3", now);
    idle.state = AgentState::Idle;
    let mut error = fake_agent_at("error", "%4", now);
    error.state = AgentState::Error;

    app.set_data_with_sessions(
        vec![working, working_2, waiting, idle, error],
        vec![
            fake_pane("%1", "main", 0, 0, "claude"),
            fake_pane("%2", "main", 0, 1, "codex"),
            fake_pane("%3", "main", 0, 2, "gemini"),
            fake_pane("%4", "main", 0, 3, "claude"),
            fake_pane("%5", "main", 0, 4, "codex"),
        ],
        vec![fake_session("$1", "main", 1)],
        vec![],
    );

    let WatchRow::Work(row) = &app.rows[0] else {
        panic!("expected work row");
    };
    let text = WatchColumn::Pane.work_text(
        row,
        now,
        &app.panes,
        watch_theme(WatchTheme::Classic),
        Spinner::OFF,
        app.watch_cfg.summary,
    );
    assert_eq!(plain_text(&text), "■ +4  main › window 0");
}

#[test]
fn work_view_state_summary_survives_long_session_name() {
    let now = time::macros::datetime!(2026-04-28 10:00:00 UTC);
    let cfg = WatchConfig {
        view: WatchView::Window,
        sort: vec![WatchSortKey::Name],
        // Assert the static gutter layout, not an animation frame.
        spinner: false,
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    let session = "a-very-long-session-name-that-would-otherwise-eat-state-markers";
    let mut working = fake_agent_at("working", "%1", now);
    working.state = AgentState::Working;
    let mut waiting = fake_agent_at("waiting", "%2", now);
    waiting.state = AgentState::WaitingInput;
    let mut idle = fake_agent_at("idle", "%3", now);
    idle.state = AgentState::Idle;

    app.set_data_with_sessions(
        vec![working, waiting, idle],
        vec![
            fake_pane("%1", session, 0, 0, "claude"),
            fake_pane("%2", session, 0, 1, "codex"),
            fake_pane("%3", session, 0, 2, "gemini"),
        ],
        vec![fake_session("$1", session, 1)],
        vec![],
    );

    let backend = TestBackend::new(64, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| render(f, &mut app)).unwrap();
    let screen = (0..terminal.backend().buffer().area().height)
        .map(|y| row_text(terminal.backend().buffer(), y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        screen.contains("▶ ● ○"),
        "state summary should remain visible before a clipped long session name:\n{screen}"
    );
}

#[test]
fn swarm_view_renders_cluster_spinner_and_subagent_tree() {
    let cfg = WatchConfig {
        view: WatchView::Window,
        layout: WatchLayout::Swarm,
        sort: vec![WatchSortKey::Name],
        ..WatchConfig::default()
    };
    let mut app = App::with_config(cfg);
    let mut agent = fake_agent(
        "worker",
        Some("%1"),
        AgentKind::ClaudeCode,
        AgentState::Working,
        Some("refactor the reconciler"),
        Some("Opus"),
        Some(17.0),
        Some(1.0),
    );
    agent.subagents = vec![muxa::state::Subagent {
        kind: "Explore".into(),
        description: Some("map the codebase".into()),
        started_at: OffsetDateTime::now_utc(),
    }];
    app.set_data_with_sessions(
        vec![agent],
        vec![fake_pane("%1", "worker", 0, 0, "claude")],
        vec![fake_session("$1", "worker", 1)],
        vec![],
    );

    let backend = TestBackend::new(120, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| render(f, &mut app)).unwrap();
    let screen = (0..terminal.backend().buffer().area().height)
        .map(|y| row_text(terminal.backend().buffer(), y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(screen.contains("Swarm"), "swarm title:\n{screen}");
    assert!(screen.contains("worker"), "work cluster header:\n{screen}");
    assert!(screen.contains("Explore"), "subagent tree row:\n{screen}");
    assert!(screen.contains("◇1"), "subagent load badge:\n{screen}");
}

#[test]
fn work_view_shows_single_agent_state_summary() {
    let now = time::macros::datetime!(2026-04-28 10:00:00 UTC);
    let cfg = WatchConfig {
        view: WatchView::Window,
        sort: vec![WatchSortKey::Name],
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    let mut agent = fake_agent_at("agent", "%1", now);
    agent.state = AgentState::Idle;
    app.set_data_with_sessions(
        vec![agent.clone()],
        vec![fake_pane("%1", "main", 0, 0, "claude")],
        vec![fake_session("$1", "main", 1)],
        vec![],
    );

    agent.state = AgentState::WaitingInput;
    agent.last_activity_at = now + time::Duration::seconds(1);
    apply_single_agent(&mut app, agent);

    let WatchRow::Work(row) = &app.rows[0] else {
        panic!("expected work row");
    };
    let text = WatchColumn::Pane.work_text(
        row,
        now + time::Duration::seconds(1),
        &app.panes,
        watch_theme(WatchTheme::Classic),
        Spinner::OFF,
        app.watch_cfg.summary,
    );
    assert_eq!(plain_text(&text), "▶     main › window 0");
}

#[test]
fn work_view_state_gutter_keeps_names_aligned() {
    let now = time::macros::datetime!(2026-04-28 10:00:00 UTC);
    let cfg = WatchConfig {
        view: WatchView::Window,
        sort: vec![WatchSortKey::Name],
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    let mut multi_working = fake_agent_at("multi-working", "%1", now);
    multi_working.state = AgentState::Working;
    let mut multi_waiting = fake_agent_at("multi-waiting", "%2", now);
    multi_waiting.state = AgentState::WaitingInput;
    let mut single_idle = fake_agent_at("single-idle", "%3", now);
    single_idle.state = AgentState::Idle;

    app.set_data_with_sessions(
        vec![multi_working, multi_waiting, single_idle],
        vec![
            fake_pane("%1", "multi", 0, 0, "claude"),
            fake_pane("%2", "multi", 0, 1, "codex"),
            fake_pane("%3", "single", 0, 0, "claude"),
        ],
        vec![
            fake_session("$1", "multi", 1),
            fake_session("$2", "single", 1),
        ],
        vec![],
    );

    let labels = app
        .rows
        .iter()
        .filter_map(|row| match row {
            WatchRow::Work(row) => {
                let text = WatchColumn::Pane.work_text(
                    row,
                    now,
                    &app.panes,
                    watch_theme(WatchTheme::Classic),
                    Spinner::OFF,
                    app.watch_cfg.summary,
                );
                Some((row.session.as_str(), plain_text(&text)))
            }
            _ => None,
        })
        .collect::<HashMap<_, _>>();

    assert_eq!(
        display_col_of(&labels["multi"], "multi"),
        Some(WORK_STATE_GUTTER_WIDTH)
    );
    assert_eq!(
        display_col_of(&labels["single"], "single"),
        Some(WORK_STATE_GUTTER_WIDTH)
    );
    assert_eq!(labels["multi"], "▶ ●   multi › window 0");
    assert_eq!(labels["single"], "○     single › window 0");
}

#[test]
fn work_view_state_gutter_compresses_overflow_before_name() {
    let now = time::macros::datetime!(2026-04-28 10:00:00 UTC);
    let cfg = WatchConfig {
        view: WatchView::Window,
        sort: vec![WatchSortKey::Name],
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    let states = [
        AgentState::Error,
        AgentState::WaitingInput,
        AgentState::WaitingChoice,
        AgentState::Working,
        AgentState::Starting,
        AgentState::Idle,
        AgentState::Stopped,
    ];
    let agents = states
        .into_iter()
        .enumerate()
        .map(|(i, state)| {
            let mut agent = fake_agent_at(&format!("agent-{i}"), &format!("%{i}"), now);
            agent.state = state;
            agent
        })
        .collect::<Vec<_>>();
    let panes = (0..states.len())
        .map(|i| {
            fake_pane(
                &format!("%{i}"),
                "crowded",
                0,
                u32::try_from(i).expect("fixture index fits u32"),
                "claude",
            )
        })
        .collect::<Vec<_>>();

    app.set_data_with_sessions(
        agents,
        panes,
        vec![fake_session("$1", "crowded", 1)],
        vec![],
    );

    let WatchRow::Work(row) = &app.rows[0] else {
        panic!("expected work row");
    };
    let text = WatchColumn::Pane.work_text(
        row,
        now,
        &app.panes,
        watch_theme(WatchTheme::Classic),
        Spinner::OFF,
        app.watch_cfg.summary,
    );
    let label = plain_text(&text);
    assert_eq!(
        display_col_of(&label, "crowded"),
        Some(WORK_STATE_GUTTER_WIDTH)
    );
    assert_eq!(label, "■ +6  crowded › window 0");
}

#[test]
fn work_view_adds_attached_time_column_and_renders_total() {
    let now = time::macros::datetime!(2026-04-28 10:00:00 UTC);
    let cfg = WatchConfig {
        view: WatchView::Window,
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    app.set_data_with_sessions(
        vec![],
        vec![fake_pane("%1", "main", 0, 0, "zsh")],
        vec![fake_session("$1", "main", 1)],
        vec![fake_session_activity(
            "$1",
            "main",
            3_600,
            Some(now - time::Duration::minutes(5)),
        )],
    );

    assert!(app.columns.contains(&WatchColumn::Duration));
    let WatchRow::Work(row) = &app.rows[0] else {
        panic!("expected work row");
    };
    let text = WatchColumn::Duration.work_text(
        row,
        now,
        &app.panes,
        watch_theme(WatchTheme::Classic),
        Spinner::OFF,
        app.watch_cfg.summary,
    );
    let cell = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect::<String>();
    assert_eq!(cell, "1h05m");
}

#[test]
fn herdr_session_row_shows_label_and_resolves_duration_by_workspace_id() {
    // herdr shape: the pane's session and the ledger key are both the
    // stable `workspace_id` (`w1`); the SessionInfo carries the human
    // label as its name (session_id == the workspace id). The row must
    // display the label, and the DUR column must resolve via the
    // session-id fallback even though no session *name* matches `w1`.
    let now = time::macros::datetime!(2026-04-28 10:00:00 UTC);
    let cfg = WatchConfig {
        view: WatchView::Window,
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    app.set_data_with_sessions(
        vec![],
        vec![fake_pane("herdr:p1", "w1", 0, 0, "zsh")],
        vec![fake_session("w1", "muxa", 0)],
        vec![fake_session_activity(
            "w1",
            "muxa",
            3_600,
            Some(now - time::Duration::minutes(5)),
        )],
    );

    let WatchRow::Work(row) = &app.rows[0] else {
        panic!("expected work row");
    };
    assert_eq!(row.session, "w1", "group key stays the workspace id");
    assert_eq!(
        row.display_name, "muxa › window 0",
        "window row keeps the labeled session ancestry"
    );

    let label = plain_text(&work_label(
        row,
        watch_theme(WatchTheme::Classic),
        Spinner::OFF,
    ));
    assert!(
        label.contains("muxa"),
        "label renders the workspace label: {label}"
    );

    let text = WatchColumn::Duration.work_text(
        row,
        now,
        &app.panes,
        watch_theme(WatchTheme::Classic),
        Spinner::OFF,
        app.watch_cfg.summary,
    );
    let cell = text
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect::<String>();
    assert_eq!(cell, "1h05m", "DUR resolves by workspace id");
}

#[test]
fn herdr_session_row_falls_back_to_workspace_id_without_label() {
    // No SessionInfo for the pane's workspace (e.g. the workspace list
    // was unreachable this refresh): the display name degrades to the id.
    let cfg = WatchConfig {
        view: WatchView::Window,
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    app.set_data_with_sessions(
        vec![],
        vec![fake_pane("herdr:p1", "w1", 0, 0, "zsh")],
        vec![],
        vec![],
    );

    let WatchRow::Work(row) = &app.rows[0] else {
        panic!("expected work row");
    };
    assert_eq!(
        row.display_name, "w1 › window 0",
        "window row falls back to the session id while retaining ancestry"
    );
}

#[test]
fn same_named_tmux_session_and_herdr_workspace_stay_distinct_rows() {
    // A tmux session named "w1" and a herdr workspace whose id is "w1"
    // share a raw session string. Grouped by the raw id alone they'd merge
    // into one corrupted row (panes from both hosts, wrong count); the
    // host-namespaced group key keeps them apart.
    let cfg = WatchConfig {
        view: WatchView::Window,
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    app.set_data_with_sessions(
        vec![],
        vec![
            // tmux "w1": two panes.
            fake_pane("%1", "w1", 0, 0, "zsh"),
            fake_pane("%2", "w1", 0, 1, "nvim"),
            // herdr workspace "w1": one pane.
            fake_pane("herdr:p1", "w1", 0, 0, "zsh"),
        ],
        vec![fake_session("$1", "w1", 1)],
        vec![],
    );

    let session_rows: Vec<&WorkRow> = app
        .rows
        .iter()
        .filter_map(|r| match r {
            WatchRow::Work(s) => Some(s.as_ref()),
            _ => None,
        })
        .collect();
    assert_eq!(
        session_rows.len(),
        2,
        "tmux w1 and herdr w1 must be two distinct rows"
    );

    let tmux_row = session_rows
        .iter()
        .find(|row| {
            row.window_key
                .as_ref()
                .is_some_and(|key| key.session.endpoint.host == muxa::HostKind::Tmux)
        })
        .expect("tmux window row present");
    let herdr_row = session_rows
        .iter()
        .find(|row| {
            row.window_key
                .as_ref()
                .is_some_and(|key| key.session.endpoint.host == muxa::HostKind::Herdr)
        })
        .expect("herdr window row present");

    // Raw session id stays "w1" on both (display/ledger key), only the
    // group key is namespaced.
    assert_eq!(tmux_row.session, "w1");
    assert_eq!(herdr_row.session, "w1");
    assert_eq!(tmux_row.pane_count, 2, "tmux row keeps its two panes");
    assert_eq!(herdr_row.pane_count, 1, "herdr row keeps its one pane");
}

// ---- multi-host aggregation + badges ---------------------------------

#[test]
fn row_host_classifies_by_pane_namespace() {
    let tmux = WatchRow::BarePane(Box::new(fake_pane("%1", "main", 0, 0, "zsh")));
    let herdr = WatchRow::BarePane(Box::new(fake_pane("herdr:p1", "w1", 0, 0, "zsh")));
    let zellij = WatchRow::BarePane(Box::new(fake_pane("zellij:7", "z", 0, 0, "zsh")));
    let legacy = WatchRow::BarePane(Box::new(fake_pane("weird-id", "x", 0, 0, "zsh")));
    assert_eq!(row_host(&tmux), Some(muxa::HostKind::Tmux));
    assert_eq!(row_host(&herdr), Some(muxa::HostKind::Herdr));
    assert_eq!(row_host(&zellij), Some(muxa::HostKind::Zellij));
    assert_eq!(row_host(&legacy), None, "unrecognized ids get no badge");
}

#[test]
fn rows_multi_host_only_when_hosts_differ() {
    let tmux_a = WatchRow::BarePane(Box::new(fake_pane("%1", "main", 0, 0, "zsh")));
    let tmux_b = WatchRow::BarePane(Box::new(fake_pane("%2", "main", 0, 1, "zsh")));
    let herdr = WatchRow::BarePane(Box::new(fake_pane("herdr:p1", "w1", 0, 0, "zsh")));
    let legacy = WatchRow::BarePane(Box::new(fake_pane("weird", "x", 0, 0, "zsh")));

    assert!(
        !rows_multi_host(std::slice::from_ref(&tmux_a)),
        "single tmux host → no badges"
    );
    assert!(
        !rows_multi_host(&[
            WatchRow::BarePane(Box::new(fake_pane("%1", "main", 0, 0, "zsh"))),
            WatchRow::BarePane(Box::new(fake_pane("%2", "main", 0, 1, "zsh"))),
        ]),
        "two tmux rows are still one host"
    );
    assert!(
        rows_multi_host(&[
            WatchRow::BarePane(Box::new(fake_pane("%1", "main", 0, 0, "zsh"))),
            WatchRow::BarePane(Box::new(fake_pane("herdr:p1", "w1", 0, 0, "zsh"))),
        ]),
        "tmux + herdr → badges on"
    );
    // Unclassifiable rows never flip the decision on their own.
    assert!(!rows_multi_host(&[legacy]));
    drop((tmux_a, tmux_b, herdr));
}

#[test]
fn mixed_host_panes_build_distinct_session_rows() {
    // A tmux session and a herdr workspace with colliding-looking keys
    // ("main" vs "w1") stay separate rows, each classified to its host.
    let cfg = WatchConfig {
        view: WatchView::Window,
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    app.set_data_with_sessions(
        vec![],
        vec![
            fake_pane("%1", "main", 0, 0, "vim"),
            fake_pane("herdr:p1", "w1", 0, 0, "zsh"),
        ],
        vec![fake_session("w1", "muxa", 0)],
        vec![],
    );
    assert_eq!(app.rows.len(), 2, "one row per host work unit");
    let hosts: std::collections::HashSet<_> = app.rows.iter().filter_map(row_host).collect();
    assert!(hosts.contains(&muxa::HostKind::Tmux));
    assert!(hosts.contains(&muxa::HostKind::Herdr));
    assert!(rows_multi_host(&app.rows));
}

#[test]
fn host_badges_render_only_in_multi_host() {
    fn render_to_string(app: &mut App) -> String {
        let backend = TestBackend::new(120, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    // Multi-host: the SESSION cells carry dim "tmux"/"herdr" tags. The
    // session names ("main"/"w1") don't contain those words, so a match
    // can only come from the badge.
    let cfg = WatchConfig {
        view: WatchView::Window,
        ..WatchConfig::default()
    };
    let mut multi = App::with_legacy_config(cfg.clone());
    multi.set_data_with_sessions(
        vec![],
        vec![
            fake_pane("%1", "main", 0, 0, "vim"),
            fake_pane("herdr:p1", "w1", 0, 0, "zsh"),
        ],
        vec![],
        vec![],
    );
    let text = render_to_string(&mut multi);
    assert!(
        text.contains("tmux"),
        "multi-host shows tmux badge: {text:?}"
    );
    assert!(
        text.contains("herdr"),
        "multi-host shows herdr badge: {text:?}"
    );

    // Single-host: no badge — a lone tmux session must not gain a
    // "herdr" (or any) host tag.
    let mut single = App::with_legacy_config(cfg);
    single.set_data_with_sessions(
        vec![],
        vec![fake_pane("%1", "main", 0, 0, "vim")],
        vec![],
        vec![],
    );
    let text = render_to_string(&mut single);
    assert!(
        !text.contains("herdr"),
        "single-host adds no host badge: {text:?}"
    );
}

#[test]
fn work_view_keeps_duration_column_visible_at_80_cols() {
    let cfg = WatchConfig {
        view: WatchView::Window,
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    // STATE is deliberately absent: the SESSION cell's leftmost cluster
    // already shows every state as icon-with-count, so the column said
    // the same thing twice at twelve columns' cost. DUR + ACT carry the
    // timing story.
    assert_eq!(
        app.columns,
        vec![
            WatchColumn::Pane,
            WatchColumn::Duration,
            WatchColumn::Activity,
            WatchColumn::Prompt,
        ]
    );
    app.set_data_with_sessions(
        vec![],
        vec![fake_pane("%1", "main", 0, 0, "zsh")],
        vec![fake_session("$1", "main", 0)],
        vec![fake_session_activity("$1", "main", 3_900, None)],
    );

    let backend = TestBackend::new(80, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| render(f, &mut app)).unwrap();
    let buf = terminal.backend().buffer();
    let screen = (0..buf.area().height)
        .map(|y| row_text(buf, y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(screen.contains("DUR"), "{screen}");
    assert!(screen.contains("1h05m"), "{screen}");
}

#[test]
fn work_view_explicit_state_column_is_preserved() {
    let cfg = WatchConfig {
        view: WatchView::Window,
        columns: vec!["pane".into(), "state".into(), "prompt".into()],
        ..WatchConfig::default()
    };
    let app = App::with_legacy_config(cfg);
    assert_eq!(
        app.columns,
        vec![
            WatchColumn::Pane,
            WatchColumn::State,
            WatchColumn::Duration,
            WatchColumn::Prompt,
        ]
    );
}

#[test]
fn activity_only_sort_floats_globally_newest_agent_to_the_top() {
    // sort = [Activity] — drops workspace grouping entirely. Expected
    // order is strict newest-first across all work rows.
    let t0 = time::macros::datetime!(2026-04-28 09:00:00 UTC);
    let t1 = time::macros::datetime!(2026-04-28 10:00:00 UTC);
    let t2 = time::macros::datetime!(2026-04-28 11:00:00 UTC);

    let cfg = WatchConfig {
        view: WatchView::Pane,
        sort: vec![WatchSortKey::Activity],
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    app.set_data(
        vec![
            fake_agent_at("alpha", "%10", t0),
            fake_agent_at("beta", "%20", t2),
            fake_agent_at("gamma", "%30", t1),
        ],
        vec![
            fake_pane("%10", "alpha", 0, 0, "x"),
            fake_pane("%20", "beta", 0, 0, "x"),
            fake_pane("%30", "gamma", 0, 0, "x"),
        ],
    );

    let order: Vec<&str> = app
        .rows
        .iter()
        .filter_map(|r| match r {
            WatchRow::Agent(a) => a.pane.as_deref(),
            WatchRow::BarePane(_) | WatchRow::Work(_) => None,
        })
        .collect();
    assert_eq!(order, vec!["%20", "%30", "%10"]);
}

#[test]
fn state_sort_prioritizes_rows_that_need_attention() {
    let now = OffsetDateTime::now_utc();
    let cfg = WatchConfig {
        view: WatchView::Pane,
        sort: vec![WatchSortKey::State],
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    let mut idle = fake_agent_at("idle", "%10", now);
    idle.state = AgentState::Idle;
    let mut working = fake_agent_at("working", "%20", now);
    working.state = AgentState::Working;
    let mut choice = fake_agent_at("choice", "%30", now);
    choice.state = AgentState::WaitingChoice;
    let mut error = fake_agent_at("error", "%40", now);
    error.state = AgentState::Error;

    app.set_data(
        vec![idle, working, choice, error],
        vec![
            fake_pane("%10", "a", 0, 0, "x"),
            fake_pane("%20", "a", 0, 1, "x"),
            fake_pane("%30", "a", 0, 2, "x"),
            fake_pane("%40", "a", 0, 3, "x"),
        ],
    );

    let order: Vec<&str> = app
        .rows
        .iter()
        .filter_map(|r| match r {
            WatchRow::Agent(a) => a.pane.as_deref(),
            WatchRow::BarePane(_) | WatchRow::Work(_) => None,
        })
        .collect();
    assert_eq!(order, vec!["%40", "%30", "%20", "%10"]);
}

#[test]
fn work_view_duration_sort_orders_longest_session_first() {
    let cfg = WatchConfig {
        view: WatchView::Window,
        sort: vec![WatchSortKey::Duration],
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    app.set_data_with_sessions(
        vec![],
        vec![
            fake_pane("%1", "short", 0, 0, "zsh"),
            fake_pane("%2", "long", 0, 0, "zsh"),
        ],
        vec![
            fake_session("$1", "short", 0),
            fake_session("$2", "long", 0),
        ],
        vec![
            fake_session_activity("$1", "short", 60, None),
            fake_session_activity("$2", "long", 3_600, None),
        ],
    );

    let order: Vec<&str> = app
        .rows
        .iter()
        .filter_map(|r| match r {
            WatchRow::Work(s) => Some(s.session.as_str()),
            WatchRow::Agent(_) | WatchRow::BarePane(_) => None,
        })
        .collect();
    assert_eq!(order, vec!["long", "short"]);
}

#[test]
fn runtime_sort_preset_reorders_rows_and_preserves_selected_pane() {
    let t0 = time::macros::datetime!(2026-04-28 09:00:00 UTC);
    let t1 = time::macros::datetime!(2026-04-28 10:00:00 UTC);
    let cfg = WatchConfig {
        view: WatchView::Pane,
        sort: vec![WatchSortKey::Name],
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    app.set_data(
        vec![
            fake_agent_at("alpha", "%10", t0),
            fake_agent_at("beta", "%20", t1),
        ],
        vec![
            fake_pane("%10", "alpha", 0, 0, "x"),
            fake_pane("%20", "beta", 0, 0, "x"),
        ],
    );
    app.table_state.select(Some(0));

    app.apply_sort_preset(WatchSortPreset::Latest);

    let order: Vec<&str> = app
        .rows
        .iter()
        .filter_map(|r| match r {
            WatchRow::Agent(a) => a.pane.as_deref(),
            WatchRow::BarePane(_) | WatchRow::Work(_) => None,
        })
        .collect();
    assert_eq!(order, vec!["%20", "%10"]);
    assert_eq!(app.selected_pane().as_deref(), Some("%10"));
}

#[test]
fn sort_keybindings_switch_sort_presets() {
    let mut app = App::new();
    for (key, preset) in [
        ('s', WatchSortPreset::Name),
        ('l', WatchSortPreset::Latest),
        ('d', WatchSortPreset::Duration),
        ('t', WatchSortPreset::State),
    ] {
        let action = handle_event(
            Event::Key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::ALT)),
            &mut app,
        );
        assert!(matches!(action, Action::SetSort(p) if p == preset));
    }
}

#[test]
fn inspector_split_persists_and_reloads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    persist_watch_inspector_split(&path, "70/30").unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("inspector_split = \"70/30\""), "{text}");

    // The saved label round-trips into the enum the renderer uses.
    assert_eq!(
        InspectorSplit::from_label("70/30"),
        Some(InspectorSplit::ListWide)
    );

    // Re-persisting another value updates in place without clobbering
    // sibling keys.
    persist_watch_sort(&path, &[WatchSortKey::Activity]).unwrap();
    persist_watch_inspector_split(&path, "30/70").unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("inspector_split = \"30/70\""));
    assert!(text.contains("sort"), "sort must survive: {text}");
}

#[test]
fn collaboration_composer_defaults_persist_and_reload() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "# keep me\n[collaboration]\nenabled = true\nscope = \"host\"\n",
    )
    .unwrap();

    persist_watch_collaboration_defaults(
        &path,
        CollaborationComposeDefaults {
            kind: RequestKind::Task,
            mode: ComposeSendMode::Execute,
        },
    )
    .unwrap();

    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.contains("# keep me"), "{saved}");
    assert!(saved.contains("collaboration_kind = \"task\""), "{saved}");
    assert!(
        saved.contains("collaboration_mode = \"execute\""),
        "{saved}"
    );
    let cfg = muxa::config::Config::load_or_default(Some(&path)).unwrap();
    assert_eq!(cfg.watch.collaboration_kind, Some(RequestKind::Task));
    assert_eq!(
        cfg.watch.collaboration_mode,
        Some(WatchCollaborationMode::Execute)
    );
    let app = App::with_legacy_config(cfg.watch);
    assert_eq!(
        app.collaboration_compose_defaults,
        CollaborationComposeDefaults {
            kind: RequestKind::Task,
            mode: ComposeSendMode::Execute,
        }
    );
}

#[test]
fn collaboration_layout_persists_and_seeds_the_screen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    persist_watch_collab_layout(&path, CollabLayout::Sequence).unwrap();

    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.contains("collab_layout = \"sequence\""), "{saved}");
    let cfg = muxa::config::Config::load_or_default(Some(&path)).unwrap();
    let app = App::with_legacy_config(cfg.watch);
    assert_eq!(app.collab.layout(), CollabLayout::Sequence);
}

#[test]
fn persist_watch_sort_creates_config_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested/config.toml");

    persist_watch_sort(&path, &[WatchSortKey::Activity]).unwrap();

    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.contains("[watch]"), "{saved}");
    assert!(saved.contains("sort = [\"latest\"]"), "{saved}");
    let cfg = muxa::config::Config::load_or_default(Some(&path)).unwrap();
    assert_eq!(cfg.watch.sort, vec![WatchSortKey::Activity]);
}

#[test]
fn persist_watch_sort_updates_sort_without_rewriting_config() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        r#"# user config
[ui]
theme = "focus"

[watch]
columns = ["pane", "state"]
sort = ["state"]
"#,
    )
    .unwrap();

    persist_watch_sort(&path, &[WatchSortKey::State, WatchSortKey::Activity]).unwrap();

    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.contains("# user config"), "{saved}");
    assert!(saved.contains("theme = \"focus\""), "{saved}");
    assert!(saved.contains("columns = [\"pane\", \"state\"]"), "{saved}");
    assert!(saved.contains("sort = [\"state\", \"latest\"]"), "{saved}");
    let cfg = muxa::config::Config::load_or_default(Some(&path)).unwrap();
    assert_eq!(
        cfg.watch.sort,
        vec![WatchSortKey::State, WatchSortKey::Activity]
    );
}

#[test]
fn pane_id_sort_produces_lexicographic_order() {
    // sort = [PaneId] — useful for screenshots / docs where stable
    // alphabetic order is preferred over recency.
    let now = OffsetDateTime::now_utc();
    let cfg = WatchConfig {
        view: WatchView::Pane,
        sort: vec![WatchSortKey::PaneId],
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    app.set_data(
        vec![
            fake_agent_at("a", "%30", now),
            fake_agent_at("a", "%1", now),
            fake_agent_at("a", "%200", now),
        ],
        vec![
            fake_pane("%1", "a", 0, 0, "x"),
            fake_pane("%30", "a", 0, 1, "x"),
            fake_pane("%200", "a", 0, 2, "x"),
        ],
    );

    let order: Vec<&str> = app
        .rows
        .iter()
        .filter_map(|r| match r {
            WatchRow::Agent(a) => a.pane.as_deref(),
            WatchRow::BarePane(_) | WatchRow::Work(_) => None,
        })
        .collect();
    // Lexicographic — "%1" < "%200" < "%30" because '2' < '3'.
    assert_eq!(order, vec!["%1", "%200", "%30"]);
}

#[test]
fn stale_agents_always_sink_to_bottom_regardless_of_sort_keys() {
    // Stale = pane no longer in the inventory. Even when the sort
    // key would otherwise float them up (e.g. very recent activity),
    // the live/stale split takes precedence.
    let now = OffsetDateTime::now_utc();
    let very_recent = now;
    let older_at = now - time::Duration::hours(1);

    let cfg = WatchConfig {
        view: WatchView::Pane,
        sort: vec![WatchSortKey::Activity],
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    app.set_data(
        vec![
            fake_agent_at("stale-but-recent", "%999", very_recent),
            fake_agent_at("live-but-older", "%10", older_at),
        ],
        vec![fake_pane("%10", "main", 0, 0, "claude")],
    );

    let order: Vec<&str> = app
        .rows
        .iter()
        .filter_map(|r| match r {
            WatchRow::Agent(a) => a.pane.as_deref(),
            WatchRow::BarePane(_) | WatchRow::Work(_) => None,
        })
        .collect();
    assert_eq!(order, vec!["%10", "%999"]);
}

#[test]
fn bare_panes_appear_after_agents_and_dedupe_by_pane_id() {
    let mut app = App::new();
    app.set_data(
        vec![fake_agent(
            "s1",
            Some("%10"),
            AgentKind::ClaudeCode,
            AgentState::Idle,
            None,
            None,
            None,
            None,
        )],
        vec![
            fake_pane("%10", "main", 0, 0, "claude"), // dedupes (matches agent)
            fake_pane("%99", "side", 2, 1, "vim"),
        ],
    );
    assert_eq!(app.rows.len(), 2);
    assert!(matches!(app.rows[0], WatchRow::Agent(_)));
    assert!(matches!(app.rows[1], WatchRow::BarePane(_)));
    // selection works across both kinds
    assert_eq!(app.selected_pane().as_deref(), Some("%10"));
    app.move_down();
    assert_eq!(app.selected_pane().as_deref(), Some("%99"));
}

#[test]
fn render_handles_empty_state() {
    let backend = TestBackend::new(100, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new();
    terminal.draw(|f| render(f, &mut app)).unwrap();
}

#[test]
fn selected_pane_returns_pane_id_for_selected_row() {
    // Lock to PaneId sort so this test stays focused on the
    // selected_pane() contract — not on the default sort behaviour
    // covered by other tests.
    let cfg = WatchConfig {
        view: WatchView::Pane,
        sort: vec![WatchSortKey::PaneId],
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    app.set_data(
        vec![
            fake_agent(
                "s1",
                Some("%1"),
                AgentKind::ClaudeCode,
                AgentState::Idle,
                None,
                None,
                None,
                None,
            ),
            fake_agent(
                "s2",
                Some("%22"),
                AgentKind::Codex,
                AgentState::Idle,
                None,
                None,
                None,
                None,
            ),
        ],
        vec![],
    );
    assert_eq!(app.selected_pane().as_deref(), Some("%1"));
    app.move_down();
    assert_eq!(app.selected_pane().as_deref(), Some("%22"));
}

#[test]
fn pane_display_falls_back_to_raw_id_when_not_in_cache() {
    // Empty pane cache simulates "no tmux running" or a stale agent
    // pane id; we expect the raw id back.
    let panes: Vec<PaneInfo> = Vec::new();
    assert_eq!(pane_display(Some("%9999"), &panes), "%9999");
    // None now renders explicitly as "(no pane)" to surface the
    // attach-unavailable case visually rather than mimicking a
    // generic "no data" dash.
    assert_eq!(pane_display(None, &panes), "(no pane)");
}

#[test]
fn pane_display_resolves_against_cached_panes() {
    let panes = vec![PaneInfo {
        session_group: None,
        agent_program: None,
        worker_launch_id: None,
        agent_role: None,
        agent_alias: None,
        workspace_id: None,
        work_id: None,
        socket: None,
        pane_id: "%42".into(),
        session_id: String::new(),
        session: "main".into(),
        window_id: String::new(),
        window_name: String::new(),
        window_index: "1".into(),
        pane_index: "0".into(),
        tty: String::new(),
        current_command: String::new(),
        title: String::new(),
        pane_pid: 0,
        current_path: String::new(),
    }];
    assert_eq!(pane_display(Some("%42"), &panes), "main:1.0");
    // Misses fall through to the raw id without panicking.
    assert_eq!(pane_display(Some("%missing"), &panes), "%missing");
}

#[test]
fn selection_movement_wraps_at_boundaries() {
    let mut app = App::new();
    app.set_data(
        vec![
            fake_agent(
                "s1",
                Some("%1"),
                AgentKind::ClaudeCode,
                AgentState::Idle,
                None,
                None,
                None,
                None,
            ),
            fake_agent(
                "s2",
                Some("%2"),
                AgentKind::ClaudeCode,
                AgentState::Idle,
                None,
                None,
                None,
                None,
            ),
        ],
        vec![],
    );
    assert_eq!(app.table_state.selected(), Some(0));
    app.move_down();
    assert_eq!(app.table_state.selected(), Some(1));
    // bottom → top
    app.move_down();
    assert_eq!(app.table_state.selected(), Some(0));
    // top → bottom
    app.move_up();
    assert_eq!(app.table_state.selected(), Some(1));
    app.move_up();
    assert_eq!(app.table_state.selected(), Some(0));
}

#[test]
fn initial_pane_preselects_matching_row_on_first_load() {
    // Lock to PaneId sort: this test cares about which row matches
    // `set_initial_pane`, not the default sort interleaving.
    let cfg = WatchConfig {
        view: WatchView::Pane,
        sort: vec![WatchSortKey::PaneId],
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    app.set_initial_pane(Some("%2".into()));
    app.set_data(
        vec![
            fake_agent(
                "s1",
                Some("%1"),
                AgentKind::ClaudeCode,
                AgentState::Idle,
                None,
                None,
                None,
                None,
            ),
            fake_agent(
                "s2",
                Some("%2"),
                AgentKind::ClaudeCode,
                AgentState::Idle,
                None,
                None,
                None,
                None,
            ),
            fake_agent(
                "s3",
                Some("%3"),
                AgentKind::ClaudeCode,
                AgentState::Idle,
                None,
                None,
                None,
                None,
            ),
        ],
        vec![],
    );
    assert_eq!(app.selected_pane().as_deref(), Some("%2"));

    // Hint is one-shot: a refresh that brings new rows must not re-snap
    // the cursor away from where the user moved it.
    app.move_down();
    assert_eq!(app.selected_pane().as_deref(), Some("%3"));
    app.set_data(
        vec![
            fake_agent(
                "s1",
                Some("%1"),
                AgentKind::ClaudeCode,
                AgentState::Idle,
                None,
                None,
                None,
                None,
            ),
            fake_agent(
                "s2",
                Some("%2"),
                AgentKind::ClaudeCode,
                AgentState::Idle,
                None,
                None,
                None,
                None,
            ),
            fake_agent(
                "s3",
                Some("%3"),
                AgentKind::ClaudeCode,
                AgentState::Idle,
                None,
                None,
                None,
                None,
            ),
        ],
        vec![],
    );
    assert_eq!(app.selected_pane().as_deref(), Some("%3"));
}

#[test]
fn render_scrolls_viewport_to_initial_pane_when_far_down_the_list() {
    // Reproduces the user-visible scenario: 30 agents listed, the
    // active pane is way below the viewport. We need the table to
    // auto-scroll so the highlighted row is actually on screen.
    let backend = TestBackend::new(140, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new();

    let agents: Vec<Agent> = (0..30)
        .map(|i| {
            let pane = format!("%{i}");
            let session = format!("s{i:02}");
            fake_agent(
                &session,
                Some(&pane),
                AgentKind::ClaudeCode,
                AgentState::Idle,
                None,
                None,
                None,
                None,
            )
        })
        .collect();

    app.set_initial_pane(Some("%25".into()));
    app.set_data(agents, vec![]);
    assert_eq!(app.selected_pane().as_deref(), Some("%25"));

    terminal.draw(|f| render(f, &mut app)).unwrap();

    let buf = terminal.backend().buffer().clone();
    let mut dump = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            dump.push_str(buf[(x, y)].symbol());
        }
        dump.push('\n');
    }

    assert!(
        dump.contains("%25"),
        "expected the selected '%25' row to be scrolled into view, got:\n{dump}",
    );
}

#[test]
fn initial_pane_falls_back_to_row_zero_when_unknown() {
    let mut app = App::new();
    app.set_initial_pane(Some("%does-not-exist".into()));
    app.set_data(
        vec![fake_agent(
            "s1",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Idle,
            None,
            None,
            None,
            None,
        )],
        vec![],
    );
    assert_eq!(app.selected_pane().as_deref(), Some("%1"));
}

#[test]
fn relative_time_buckets() {
    let now = OffsetDateTime::now_utc();
    assert_eq!(relative_time(now, now), "0s");
    assert_eq!(relative_time(now - time::Duration::minutes(5), now), "5m");
    assert_eq!(relative_time(now - time::Duration::hours(3), now), "3h");
    assert_eq!(relative_time(now - time::Duration::days(3), now), "3d");
}

#[test]
fn default_columns_are_prompt_forward() {
    let app = App::new();
    assert_eq!(
        app.columns,
        vec![
            WatchColumn::Pane,
            WatchColumn::StateAge,
            WatchColumn::Activity,
            WatchColumn::Prompt,
        ]
    );
}

#[test]
fn custom_columns_resolve_in_config_order() {
    let cfg = WatchConfig {
        view: WatchView::Pane,
        columns: vec!["prompt".into(), "pane".into(), "kind".into()],
        widths: HashMap::new(),
        ..Default::default()
    };
    let app = App::with_legacy_config(cfg);
    assert_eq!(
        app.columns,
        vec![WatchColumn::Prompt, WatchColumn::Pane, WatchColumn::Kind]
    );
}

#[test]
fn unknown_column_keys_are_skipped() {
    let cfg = WatchConfig {
        view: WatchView::Pane,
        columns: vec!["pane".into(), "bogus".into(), "prompt".into()],
        widths: HashMap::new(),
        ..Default::default()
    };
    let app = App::with_legacy_config(cfg);
    // "bogus" was warned-and-skipped; the rest survive in order.
    assert_eq!(app.columns, vec![WatchColumn::Pane, WatchColumn::Prompt]);
}

#[test]
fn width_spec_kinds_translate_to_constraints() {
    let mut widths = HashMap::new();
    widths.insert("pane".into(), WidthSpec::Length(40));
    widths.insert("prompt".into(), WidthSpec::Min(50));
    widths.insert("kind".into(), WidthSpec::Percentage(20));
    widths.insert("state".into(), WidthSpec::Invalid("nope".into()));
    let cfg = WatchConfig {
        view: WatchView::Pane,
        columns: vec![
            "pane".into(),
            "prompt".into(),
            "kind".into(),
            "state".into(),
        ],
        widths,
        ..Default::default()
    };
    assert_eq!(
        resolve_width(WatchColumn::Pane, &cfg),
        Constraint::Length(40)
    );
    assert_eq!(
        resolve_width(WatchColumn::Prompt, &cfg),
        Constraint::Min(50)
    );
    assert_eq!(
        resolve_width(WatchColumn::Kind, &cfg),
        Constraint::Percentage(20)
    );
    // Invalid -> column default.
    assert_eq!(
        resolve_width(WatchColumn::State, &cfg),
        WatchColumn::State.default_width()
    );
    // Missing -> column default.
    assert_eq!(
        resolve_width(WatchColumn::Activity, &cfg),
        WatchColumn::Activity.default_width()
    );
}

#[test]
fn agent_text_renders_for_each_column_kind() {
    let now = OffsetDateTime::now_utc();
    let a = fake_agent(
        "s",
        Some("%1"),
        AgentKind::ClaudeCode,
        AgentState::Working,
        Some("hello world"),
        Some("Opus"),
        Some(42.0),
        Some(0.34),
    );
    // Smoke-test that every column variant produces a Text without panic.
    for col in [
        WatchColumn::Pane,
        WatchColumn::Kind,
        WatchColumn::State,
        WatchColumn::Model,
        WatchColumn::Ctx,
        WatchColumn::Cost,
        WatchColumn::Limits,
        WatchColumn::Prompt,
        WatchColumn::Activity,
        WatchColumn::Duration,
    ] {
        let _ = col.agent_text(
            &a,
            now,
            &[],
            watch_theme(WatchTheme::Classic),
            Spinner::OFF,
            WatchSummary::default(),
        );
    }
}

#[test]
fn limits_renders_red_cap_badge_when_rate_limit_active_with_reset_time() {
    let now = OffsetDateTime::now_utc();
    let mut a = fake_agent(
        "s",
        Some("%1"),
        AgentKind::ClaudeCode,
        AgentState::Working,
        None,
        None,
        None,
        None,
    );
    a.rate_limited_until = Some(now + time::Duration::hours(2) + time::Duration::minutes(14));
    a.rate_limit_scope = Some(RateLimitScope::FiveHour);
    let s = limits_cell_string(&a, now);
    assert_eq!(s, "⛔ 5h in 2h 14m", "got {s:?}");
    assert_eq!(limits_cell_fg(&a, now), Some(Color::Red));
}

/// P0 fix: a `RateLimited` event from `StopFailure` carries no reset
/// timestamp, but the row must still render the red cap badge —
/// otherwise the user sees no visual indication that they're capped.
#[test]
fn limits_renders_red_cap_badge_when_scope_set_without_reset_time() {
    let now = OffsetDateTime::now_utc();
    let mut a = fake_agent(
        "s",
        Some("%1"),
        AgentKind::ClaudeCode,
        AgentState::Error,
        None,
        None,
        None,
        None,
    );
    // Simulate StopFailure-only signal: scope set, until unknown.
    a.rate_limit_scope = Some(RateLimitScope::Unknown);
    a.rate_limited_until = None;
    let s = limits_cell_string(&a, now);
    assert!(s.starts_with("⛔ "), "expected cap glyph: {s:?}");
    assert_eq!(limits_cell_fg(&a, now), Some(Color::Red));
}

#[test]
fn limits_ignores_expired_cap_and_falls_through_to_pct() {
    // A `rate_limited_until` in the past shouldn't drag the cell into
    // red — we want the row to recover to the utilisation view as soon
    // as the window rolls over (the daemon clears the field on the
    // next `Started`, but in-flight snapshots may still carry it).
    let now = OffsetDateTime::now_utc();
    let mut a = fake_agent(
        "s",
        Some("%1"),
        AgentKind::ClaudeCode,
        AgentState::Working,
        None,
        None,
        None,
        None,
    );
    a.rate_limit_scope = Some(RateLimitScope::FiveHour);
    a.rate_limited_until = Some(now - time::Duration::minutes(1));
    a.rate_limit_5h_pct = Some(42.0);
    let s = limits_cell_string(&a, now);
    assert!(!s.contains('⛔'), "expected no cap glyph: {s:?}");
    assert!(s.contains("5h"), "expected utilisation text: {s:?}");
    // < 80% — default colour, not yellow.
    assert_eq!(limits_cell_fg(&a, now), None);
}

#[test]
fn limits_renders_utilization_when_only_pct_is_set() {
    let now = OffsetDateTime::now_utc();
    let mut a = fake_agent(
        "s",
        Some("%1"),
        AgentKind::ClaudeCode,
        AgentState::Working,
        None,
        None,
        None,
        None,
    );
    a.rate_limit_5h_pct = Some(84.0);
    assert_eq!(limits_cell_string(&a, now), "5h 84%");
    // ≥ 80 — yellow warning colour.
    assert_eq!(limits_cell_fg(&a, now), Some(Color::Yellow));
}

#[test]
fn limits_picks_higher_pct_window_when_both_set() {
    let now = OffsetDateTime::now_utc();
    let mut a = fake_agent(
        "s",
        Some("%1"),
        AgentKind::ClaudeCode,
        AgentState::Working,
        None,
        None,
        None,
        None,
    );
    a.rate_limit_5h_pct = Some(31.0);
    a.rate_limit_7d_pct = Some(72.0);
    // 7d wins (higher), and < 80 keeps the default colour.
    assert_eq!(limits_cell_string(&a, now), "7d 72%");
    assert_eq!(limits_cell_fg(&a, now), None);
}

#[test]
fn limits_renders_dim_dash_when_nothing_is_set() {
    let now = OffsetDateTime::now_utc();
    let a = fake_agent(
        "s",
        Some("%1"),
        AgentKind::ClaudeCode,
        AgentState::Working,
        None,
        None,
        None,
        None,
    );
    assert_eq!(limits_cell_string(&a, now), "-");
    assert_eq!(limits_cell_fg(&a, now), Some(Color::DarkGray));
}

#[test]
fn limits_cap_badge_prefix_matches_scope() {
    let now = OffsetDateTime::now_utc();
    let mut a = fake_agent(
        "s",
        Some("%1"),
        AgentKind::ClaudeCode,
        AgentState::Working,
        None,
        None,
        None,
        None,
    );
    a.rate_limited_until = Some(now + time::Duration::hours(1));

    a.rate_limit_scope = Some(RateLimitScope::SevenDay);
    assert!(
        limits_cell_string(&a, now).contains("7d "),
        "SevenDay should prefix with `7d `"
    );

    a.rate_limit_scope = Some(RateLimitScope::FiveHour);
    assert!(
        limits_cell_string(&a, now).contains("5h "),
        "FiveHour should prefix with `5h `"
    );

    // Unknown scope still renders the cap (scope is set), but
    // without a window-prefix string.
    a.rate_limit_scope = Some(RateLimitScope::Unknown);
    let s = limits_cell_string(&a, now);
    assert!(s.starts_with("⛔ "), "Unknown should still show cap: {s:?}");
    assert!(!s.contains("5h "), "Unknown should not prefix `5h `");
    assert!(!s.contains("7d "), "Unknown should not prefix `7d `");

    // No scope at all → no cap (the renderer's load-bearing gate).
    a.rate_limit_scope = None;
    let s = limits_cell_string(&a, now);
    assert!(!s.contains('⛔'), "None scope should drop the cap: {s:?}");
}

/// Capped without a reset timestamp — rendered with a bare scope or
/// the literal "rate limited" suffix, no relative-time string.
#[test]
fn limits_cap_badge_without_reset_uses_capped_label() {
    let now = OffsetDateTime::now_utc();
    let mut a = fake_agent(
        "s",
        Some("%1"),
        AgentKind::ClaudeCode,
        AgentState::Error,
        None,
        None,
        None,
        None,
    );
    a.rate_limit_scope = Some(RateLimitScope::FiveHour);
    a.rate_limited_until = None;
    assert_eq!(limits_cell_string(&a, now), "⛔ 5h capped");

    a.rate_limit_scope = Some(RateLimitScope::Unknown);
    assert_eq!(limits_cell_string(&a, now), "⛔ rate limited");
}

#[test]
fn bare_pane_summary_lands_in_prompt_column() {
    // Render with a column set that includes Prompt but excludes the
    // others — we verify the BarePane row is built without panic and
    // that the prompt column carries the summary.
    let cfg = WatchConfig {
        view: WatchView::Pane,
        columns: vec!["pane".into(), "prompt".into()],
        widths: HashMap::new(),
        ..Default::default()
    };
    let mut app = App::with_legacy_config(cfg);
    app.set_data(
        vec![],
        vec![fake_pane("%99", "main", 0, 0, "vim README.md")],
    );
    assert_eq!(app.rows.len(), 1);
    let backend = TestBackend::new(120, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| render(f, &mut app)).unwrap();
    let buf = terminal.backend().buffer();
    // The pane current command should be visible somewhere in the row.
    let text: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        text.contains("vim"),
        "expected pane summary in render: {text:?}"
    );
}

// ---- background refresh task -----------------------------------------
