use super::*;

#[test]
#[ignore = "requires the isolated scripts/rmux-jump-check.py server"]
#[allow(clippy::too_many_lines)] // one disposable lifecycle verifies the native command contract
fn live_rmux_topology_controls() {
    let endpoint = muxa::BackendEndpoint {
        host: muxa::HostKind::Rmux,
        socket: std::env::var("MUXA_RMUX_TEST_ENDPOINT").unwrap(),
    };
    let run = |args: &[&str]| crate::mux_control::capture(&endpoint, args).unwrap();
    let id = run(&[
        "new-session",
        "-dP",
        "-F",
        "#{session_id}",
        "-s",
        "controls",
        "/bin/sh",
    ]);
    let session = SessionKey {
        endpoint: endpoint.clone(),
        session_id: id.trim().into(),
    };
    let pane = create_window(&session, None).unwrap();
    assert!(pane.pane_id.starts_with("rmux:%"));
    let rename = |key, level, name: &str| {
        apply_rename(&RenameComposer {
            key,
            level,
            original: String::new(),
            input: name.into(),
            cursor: name.len(),
        })
        .unwrap()
    };
    rename(
        TopologyNodeKey::Window(pane.window.clone()),
        RenameLevel::Window,
        "review work",
    );
    assert_eq!(
        run(&[
            "display-message",
            "-p",
            "-t",
            &pane.window.window_id,
            "#{window_name}"
        ])
        .trim(),
        "review-work"
    );
    rename(
        TopologyNodeKey::Pane(pane.clone()),
        RenameLevel::Pane,
        "Reviewer",
    );
    assert_eq!(
        run(&[
            "display-message",
            "-p",
            "-t",
            &pane.window.window_id,
            "#{pane_title}"
        ])
        .trim(),
        "Reviewer"
    );
    rename(
        TopologyNodeKey::Session(session.clone()),
        RenameLevel::Session,
        "controls-renamed",
    );
    assert_eq!(
        run(&[
            "display-message",
            "-p",
            "-t",
            &session.session_id,
            "#{session_name}"
        ])
        .trim(),
        "controls-renamed"
    );
    let mut effects = RealEffects;
    effects
        .spawn_at(
            &TopologyNodeKey::Window(pane.window.clone()),
            "/tmp",
            "",
            "test",
            "/bin/sh",
        )
        .unwrap();
    assert_eq!(
        run(&[
            "list-panes",
            "-t",
            &pane.window.window_id,
            "-F",
            "#{pane_id}"
        ])
        .lines()
        .count(),
        2
    );
    let captured = capture_tmux_window(pane.window.clone());
    assert_eq!(captured.panes.len(), 2);
    assert!(captured.panes.iter().all(|pane| pane.text.is_some()));
    effects
        .send_prompt_to(&pane, "printf '__RMUX_%s__\\n' CONTROL")
        .unwrap();
    let backend = muxa::RmuxBackend::with_endpoint(&endpoint.socket);
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let output = muxa::PaneBackend::capture_pane(&backend, &pane.pane_id).unwrap();
        if output.contains("__RMUX_CONTROL__") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "prompt never executed: {output}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    effects.interrupt_pane(&pane).unwrap();
    effects
        .terminate_node(&TopologyNodeKey::Pane(pane.clone()))
        .unwrap();
    assert_eq!(
        run(&[
            "list-panes",
            "-t",
            &pane.window.window_id,
            "-F",
            "#{pane_id}"
        ])
        .lines()
        .count(),
        1
    );
    effects
        .terminate_node(&TopologyNodeKey::Window(pane.window))
        .unwrap();
    effects
        .terminate_node(&TopologyNodeKey::Session(session))
        .unwrap();
}

#[test]
fn state_markers_are_single_cell() {
    let theme = watch_theme(WatchTheme::Classic);
    for state in [
        AgentState::Working,
        AgentState::WaitingInput,
        AgentState::WaitingChoice,
        AgentState::Error,
        AgentState::Idle,
        AgentState::Starting,
        AgentState::Stopped,
    ] {
        let (marker, _) = state_marker(state, theme, Spinner::OFF);
        assert_eq!(unicode_width::UnicodeWidthStr::width(marker), 1);
    }
}

#[test]
fn spinner_animates_only_working_and_starting() {
    let theme = watch_theme(WatchTheme::Classic);
    let on = Spinner {
        frame: 0,
        enabled: true,
    };
    // Working / starting animate to dot / half-circle frames.
    assert!(SWARM_DOTS.contains(&state_marker(AgentState::Working, theme, on).0));
    assert!(SWARM_START.contains(&state_marker(AgentState::Starting, theme, on).0));
    // Every other state keeps the shared static icon (also used by
    // `muxa status` / status-line).
    for state in [
        AgentState::Idle,
        AgentState::WaitingInput,
        AgentState::WaitingChoice,
        AgentState::Error,
        AgentState::Stopped,
    ] {
        assert_eq!(state_marker(state, theme, on).0, crate::state_icon(state));
    }
    // Advancing the frame advances the dot.
    let f0 = state_marker(
        AgentState::Working,
        theme,
        Spinner {
            frame: 0,
            enabled: true,
        },
    )
    .0;
    let f1 = state_marker(
        AgentState::Working,
        theme,
        Spinner {
            frame: 1,
            enabled: true,
        },
    )
    .0;
    assert_ne!(f0, f1);
    // Disabled falls back to the static icon everywhere.
    assert_eq!(
        state_marker(AgentState::Working, theme, Spinner::OFF).0,
        crate::state_icon(AgentState::Working)
    );
}

#[test]
fn pulse_kind_classifies_transitions() {
    // Turn finished.
    assert_eq!(
        pulse_kind(Some(AgentState::Working), AgentState::Idle),
        Some(PulseKind::Done)
    );
    // Entered error from anywhere but error.
    assert_eq!(
        pulse_kind(Some(AgentState::WaitingInput), AgentState::Error),
        Some(PulseKind::Error)
    );
    // No flash: already error, first sight, spin-up settling, or non-idle.
    assert_eq!(pulse_kind(Some(AgentState::Error), AgentState::Error), None);
    assert_eq!(pulse_kind(None, AgentState::Error), None);
    assert_eq!(
        pulse_kind(Some(AgentState::Starting), AgentState::Idle),
        None
    );
    assert_eq!(
        pulse_kind(Some(AgentState::Working), AgentState::WaitingInput),
        None
    );
}

#[test]
fn done_pulse_flashes_state_cell_on_work_to_idle() {
    let cfg = WatchConfig {
        view: WatchView::Pane,
        sort: vec![WatchSortKey::PaneId],
        hide_paneless: false,
        spinner: true,
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    let working = fake_agent(
        "s1",
        Some("%1"),
        AgentKind::ClaudeCode,
        AgentState::Working,
        Some("refactor"),
        None,
        None,
        None,
    );
    let panes = vec![fake_pane("%1", "a", 0, 0, "claude")];
    // Prime prev-state = Working (first sight never flashes).
    app.set_data(vec![working.clone()], panes.clone());
    app.detect_pulses();
    // Working → Idle arms a Done pulse.
    let mut idle = working;
    idle.state = AgentState::Idle;
    app.set_data(vec![idle], panes);
    app.detect_pulses();

    let backend = TestBackend::new(100, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| render(f, &mut app)).unwrap();
    let screen = (0..terminal.backend().buffer().area().height)
        .map(|y| row_text(terminal.backend().buffer(), y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        screen.contains('✓'),
        "done pulse ✓ should overlay the State cell:\n{screen}"
    );
}

#[test]
fn workload_cell_summarizes_shells_and_processes() {
    let mut agent = fake_agent(
        "s",
        Some("%1"),
        AgentKind::ClaudeCode,
        AgentState::Working,
        Some("prompt"),
        None,
        None,
        None,
    );
    agent.workload = muxa::WorkloadSummary {
        primary_pid: Some(20),
        process_count: 2,
        shell_count: 1,
        subagent_count: 0,
        helper_count: 1,
        preview: vec![
            muxa::WorkloadProcess {
                pid: 30,
                parent_pid: 20,
                depth: 2,
                kind: WorkloadProcessKind::Shell,
                command: "zsh".into(),
            },
            muxa::WorkloadProcess {
                pid: 31,
                parent_pid: 30,
                depth: 3,
                kind: WorkloadProcessKind::Process,
                command: "python3".into(),
            },
        ],
    };

    assert_eq!(agent_workload_badge(&agent).as_deref(), Some("▸1 +1"));
    assert_eq!(
        workload_detail_string(&agent),
        "shell:1 · process:1 · helper:1 · zsh(sh) -> python3"
    );
}

#[test]
fn helper_only_workload_stays_out_of_primary_cell() {
    let mut agent = fake_agent(
        "s",
        Some("%1"),
        AgentKind::ClaudeCode,
        AgentState::Working,
        Some("prompt"),
        None,
        None,
        None,
    );
    agent.workload = muxa::WorkloadSummary {
        primary_pid: Some(20),
        process_count: 0,
        shell_count: 0,
        subagent_count: 0,
        helper_count: 1,
        preview: Vec::new(),
    };

    assert_eq!(agent_workload_badge(&agent), None);
    assert_eq!(workload_detail_string(&agent), "helper:1");
}

#[test]
fn watch_tree_and_inspector_use_the_shared_pane_color_without_recoloring_state() {
    let socket = "/tmp/rmux-test/default";
    let panes = vec![fake_topology_pane(
        socket, "$1", "muxa", "@1", "main", 0, "rmux:%35", 0,
    )];
    let agents = vec![topology_agent(
        "color-test",
        "rmux:%35",
        socket,
        AgentState::Working,
        "review",
        1,
    )];
    let app = topology_watch(WatchView::Pane, agents, panes);
    let target = app
        .tree_targets()
        .into_iter()
        .find(|target| matches!(target.key, TopologyNodeKey::Pane(_)))
        .unwrap();
    let node = app.topology.find(&target.key).unwrap();
    let TopologyNodeRef::Pane(pane) = node else {
        unreachable!()
    };
    let theme = watch_theme(WatchTheme::default());
    let color = crate::pane_identity::pane_color("rmux:%35", Some(socket));
    let label = tree_node_label(&target, node, false, theme);
    let id = label
        .lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content == "rmux:%35")
        .unwrap();
    assert_eq!(id.style.fg, Some(color));
    let spin = Spinner {
        frame: 0,
        enabled: false,
    };
    let roster = inspector_pane_roster_line(pane, 100, OffsetDateTime::now_utc(), theme, spin);
    let id = roster
        .spans
        .iter()
        .find(|span| span.content == "rmux:%35")
        .unwrap();
    assert_eq!(id.style.fg, Some(color));
    let (glyph, state) = state_marker(AgentState::Working, theme, spin);
    let status = roster
        .spans
        .iter()
        .find(|span| span.content == glyph)
        .unwrap();
    assert_eq!(status.style, state);
}

#[test]
fn latest_session_order_updates_on_same_state_activity_snapshot() {
    let panes = vec![
        fake_topology_pane("default", "$1", "alpha", "@1", "WORK", 0, "%1", 0),
        fake_topology_pane("default", "$2", "beta", "@2", "WORK", 0, "%2", 0),
    ];
    let alpha = topology_agent(
        "alpha-agent",
        "%1",
        "default",
        AgentState::Working,
        "alpha",
        1,
    );
    let beta = topology_agent(
        "beta-agent",
        "%2",
        "default",
        AgentState::Working,
        "beta",
        2,
    );
    let mut app = topology_watch(WatchView::Session, vec![alpha.clone(), beta.clone()], panes);
    app.apply_sort_preset(WatchSortPreset::Latest);
    assert_eq!(app.sorted_sessions()[0].name, "beta");
    let selected = app.sorted_sessions()[0].node_key();
    select_tree_key(&mut app, &selected);

    let mut updated = alpha;
    updated.last_activity_at = beta.last_activity_at + time::Duration::seconds(10);
    apply_outcome(
        &mut app,
        RefreshOutcome::Agents(vec![updated.clone(), beta]),
    );
    assert_eq!(app.sorted_sessions()[0].name, "alpha");
    assert_eq!(app.selected_node_key(), Some(selected));
    assert_eq!(
        app.sorted_sessions()[0].windows[0].panes[0]
            .agent
            .as_ref()
            .unwrap()
            .state,
        AgentState::Working
    );

    // A legitimate clearing of Codex quota fields must survive the UI merge.
    updated.rate_limit_7d_pct = Some(100.0);
    let mut incoming = updated.clone();
    incoming.rate_limit_7d_pct = None;
    assert_eq!(
        merge_agent_for_ui(&updated, &incoming).rate_limit_7d_pct,
        None
    );
}

/// Two windows so the single-window compaction does not fold the window
/// row into its session — the `C` binding has to answer from either.
#[test]
fn empty_watch_enter_opens_agent_creation_without_a_selected_row() {
    let mut app = App::with_config(WatchConfig::default());
    assert!(onboarding_empty(&app));
    let action = handle_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
    );
    assert!(matches!(action, Action::None));
    let composer = app.spawn.as_ref().expect("agent creation form");
    assert!(composer.target.is_none());
    assert!(!composer.dir.is_empty());
}

#[test]
fn empty_watch_demo_key_requires_a_supported_environment() {
    let mut app = App::with_config(WatchConfig::default());
    app.demo_available = true;
    assert!(matches!(
        handle_event(
            Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)),
            &mut app
        ),
        Action::TryDemo
    ));
    app.demo_available = false;
    assert!(!matches!(
        handle_event(
            Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)),
            &mut app
        ),
        Action::TryDemo
    ));
}

#[test]
fn filters_and_hidden_agents_do_not_offer_first_run_actions() {
    let mut app = App::with_config(WatchConfig::default());
    app.attention_only = true;
    assert!(!onboarding_empty(&app));
    app.attention_only = false;
    app.search_query = "codex".into();
    assert!(!onboarding_empty(&app));
    app.search_query.clear();
    app.paneless_hidden = 1;
    assert!(!onboarding_empty(&app));
}

#[test]
fn move_scope_follows_the_selected_window_or_pane() {
    let (agents, panes) = two_window_topology_fixture();
    let mut app = topology_watch(WatchView::Pane, agents, panes);
    let session = app.topology.sessions[0].key.clone();
    let window = app.topology.sessions[0].windows[0].key.clone();
    let pane = app.topology.sessions[0].windows[0].panes[0].key.clone();
    select_tree_key(&mut app, &TopologyNodeKey::Window(window.clone()));
    let (key, scope) = selected_move_target(&app).unwrap();
    assert_eq!(scope, crate::agent_move::MoveScope::Window);
    assert_eq!(key.window, window);
    select_tree_key(&mut app, &TopologyNodeKey::Pane(pane.clone()));
    assert_eq!(
        selected_move_target(&app).unwrap(),
        (pane, crate::agent_move::MoveScope::Pane)
    );
    select_tree_key(&mut app, &TopologyNodeKey::Session(session));
    assert!(selected_move_target(&app).is_err());
}

#[test]
fn a_compacted_single_window_row_moves_the_whole_window() {
    let (agents, panes) = basic_topology_fixture();
    let mut app = topology_watch(WatchView::Pane, agents, panes);
    let session = app.topology.sessions[0].key.clone();
    let window = app.topology.sessions[0].windows[0].key.clone();
    select_tree_key(&mut app, &TopologyNodeKey::Session(session));
    let (key, scope) = selected_move_target(&app).unwrap();
    assert_eq!(scope, crate::agent_move::MoveScope::Window);
    assert_eq!(key.window, window);
}

#[test]
fn shift_c_creates_a_window_in_the_session_the_cursor_is_inside() {
    // `prefix + c` works from any window of a session; the watch binding
    // has to behave the same whichever level the cursor stopped on.
    let (agents, panes) = two_window_topology_fixture();
    let mut app = topology_watch(WatchView::Pane, agents, panes);
    let session_key = app.topology.sessions[0].key.clone();
    let window_key = app.topology.sessions[0].windows[1].key.clone();
    let pane_key = app.topology.sessions[0].windows[1].panes[0].key.clone();

    for target in [
        TopologyNodeKey::Session(session_key.clone()),
        TopologyNodeKey::Window(window_key.clone()),
        TopologyNodeKey::Pane(pane_key),
    ] {
        select_tree_key(&mut app, &target);
        match press_new_window(&mut app) {
            Action::NewWindow { session, cwd } => {
                assert_eq!(session, session_key, "wrong session for {target:?}");
                // The window opens where the selected row works, not
                // where the console was launched from.
                assert!(
                    cwd.is_some_and(|dir| dir.starts_with("/repo/muxa/")),
                    "no working directory for {target:?}"
                );
            }
            other => panic!("expected a new window for {target:?}, got {other:?}"),
        }
    }
}

#[test]
fn shift_c_takes_the_exact_window_the_cursor_names() {
    let (agents, panes) = two_window_topology_fixture();
    let mut app = topology_watch(WatchView::Pane, agents, panes);
    let window_key = app.topology.sessions[0].windows[1].key.clone();
    select_tree_key(&mut app, &TopologyNodeKey::Window(window_key));
    assert!(matches!(
        press_new_window(&mut app),
        Action::NewWindow { cwd: Some(ref dir), .. } if dir == "/repo/muxa/TEST-2"
    ));
}

#[test]
fn shift_c_is_a_filter_character_once_a_search_is_armed() {
    // Every browse key gives way to typing once `/` is down — otherwise
    // the filter could never match a name that starts with one.
    let (agents, panes) = two_window_topology_fixture();
    let mut app = topology_watch(WatchView::Pane, agents, panes);
    app.arm_explicit_search();
    assert!(matches!(press_new_window(&mut app), Action::None));
    assert_eq!(app.search_query, "C");
}

#[test]
fn typing_does_not_start_a_filter_until_slash_arms_it() {
    // `/` is the only entrance: an unbound letter used to open a search
    // under the cursor, which made every future binding a breaking change.
    let (agents, panes) = two_window_topology_fixture();
    let mut app = topology_watch(WatchView::Pane, agents, panes);

    assert!(matches!(key_action(&mut app, 'x'), Action::None));
    assert!(app.search_query.is_empty(), "a bare letter must not filter");
    let hint = app.footer_hint.as_ref().expect("a hint names the way in");
    assert!(
        hint.message.contains('/'),
        "unexpected hint: {}",
        hint.message
    );

    assert!(matches!(key_action(&mut app, '/'), Action::None));
    assert!(matches!(key_action(&mut app, 'x'), Action::None));
    assert_eq!(app.search_query, "x", "typing filters once armed");
}

#[test]
fn new_window_plan_is_prefix_c_on_the_selected_session() {
    // Create detached so another terminal's current window never moves;
    // the invoking client jumps after obtaining its private view.
    let args = plan_new_window("$1", None);
    assert!(args.iter().any(|arg| arg == "-d"), "{args:?}");
    assert_eq!(args[0], "new-window");
    assert!(args.windows(2).any(|pair| pair == ["-t", "$1"]), "{args:?}");
    assert!(!args.iter().any(|arg| arg == "-c"), "{args:?}");
}

#[test]
fn new_window_plan_only_passes_a_directory_that_still_exists() {
    let live = std::env::temp_dir();
    let live = live.to_str().expect("a utf-8 temp dir");
    let args = plan_new_window("$1", Some(live));
    assert!(args.windows(2).any(|pair| pair == ["-c", live]), "{args:?}");

    // A recorded cwd outlives the directory; `-c` on a deleted path fails
    // the whole command, and no window is worse than one in $HOME.
    let args = plan_new_window("$1", Some("/definitely/not/here"));
    assert!(!args.iter().any(|arg| arg == "-c"), "{args:?}");
}

#[test]
fn the_created_window_is_keyed_by_the_session_we_targeted() {
    // `@N`/`%N` repeat across tmux servers, so the reported ids are only
    // an identity once the session they came from is stapled back on.
    let session = SessionKey {
        endpoint: BackendEndpoint {
            host: muxa::HostKind::Tmux,
            socket: "work".into(),
        },
        session_id: "$3".into(),
    };
    let pane = parse_new_window_target(&session, "@7 %21").expect("a window and a pane");
    assert_eq!(pane.pane_id, "%21");
    assert_eq!(pane.window.window_id, "@7");
    assert_eq!(pane.window.session, session);
    assert!(parse_new_window_target(&session, "").is_err());
    assert!(parse_new_window_target(&session, "@7").is_err());
}

#[test]
fn tree_view_defaults_control_expansion_without_losing_ancestry() {
    let (agents, panes) = basic_topology_fixture();
    for (view, expected_depths) in [
        (WatchView::Session, vec![0]),
        (WatchView::Window, vec![0, 1]),
        (WatchView::Pane, vec![0, 1, 1]),
    ] {
        let app = topology_watch(view, agents.clone(), panes.clone());
        assert_eq!(
            app.tree_targets()
                .iter()
                .map(|target| target.depth)
                .collect::<Vec<_>>(),
            expected_depths
        );
    }
}

#[test]
fn the_work_layout_is_one_flat_row_per_window() {
    // Sessions and panes fold away: the workspace is a column here, and a
    // Work's panes are summarised by the ALIASES column.
    let app = work_layout_app();
    let targets = app.tree_targets();
    assert_eq!(targets.len(), 2);
    assert!(targets.iter().all(|target| target.depth == 0));
    let names = targets
        .iter()
        .map(|target| match app.topology.find(&target.key).unwrap() {
            TopologyNodeRef::Window(window) => window.name.clone(),
            other => panic!("work rows are window nodes, got {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["AUTH", "DOCS"]);
}

#[test]
fn a_work_row_still_addresses_a_window_node() {
    // Everything that acts on the cursor speaks topology keys, so this is
    // what keeps attach, preview and the composer working unchanged.
    let mut app = work_layout_app();
    assert!(matches!(
        app.selected_node(),
        Some(TopologyNodeRef::Window(window)) if window.name == "AUTH"
    ));
    // Rows from different sessions are siblings of nothing, so the tree's
    // accordion movement would have stranded the cursor here.
    app.move_down();
    assert!(matches!(
        app.selected_node(),
        Some(TopologyNodeRef::Window(window)) if window.name == "DOCS"
    ));
}

#[test]
fn w_returns_to_the_layout_it_interrupted() {
    // Always returning to `tree` would silently discard a swarm the
    // operator was in the middle of using.
    let mut app = App::with_config(WatchConfig {
        layout: WatchLayout::Swarm,
        ..WatchConfig::default()
    });
    assert!(matches!(
        key_action(&mut app, 'W'),
        Action::SetLayout(WatchLayout::Work)
    ));
    app.apply_layout(WatchLayout::Work);
    assert!(matches!(
        key_action(&mut app, 'W'),
        Action::SetLayout(WatchLayout::Swarm)
    ));
}

#[test]
fn the_palette_reaches_the_work_table() {
    let mut app = App::with_legacy_config(WatchConfig::default());
    assert!(matches!(
        execute_palette_command(&mut app, "layout work"),
        Action::SetLayout(WatchLayout::Work)
    ));
}

#[test]
fn the_work_table_mirrors_the_work_list_columns() {
    let mut app = work_layout_app();
    // Wide enough that the inspector split still leaves every column.
    let backend = ratatui::backend::TestBackend::new(240, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| render(f, &mut app)).unwrap();
    let painted: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    for column in ["WORK", "WORKSPACE", "GEN", "ALIASES", "DONE", "CWD"] {
        assert!(painted.contains(column), "missing {column} in {painted}");
    }
    assert!(painted.contains("AUTH"), "{painted}");
    assert!(painted.contains("alpha"), "{painted}");
    // Two claudes in one window collapse rather than repeating the word.
    assert!(painted.contains("codex x2"), "{painted}");
}

#[test]
fn focus_expansion_opens_only_the_selected_session() {
    let panes = vec![
        fake_topology_pane("default", "$1", "alpha", "@1", "AUTH", 0, "%1", 0),
        fake_topology_pane("default", "$2", "beta", "@2", "DOCS", 0, "%2", 0),
    ];
    let agents = vec![
        topology_agent("auth", "%1", "default", AgentState::Working, "auth", 1),
        topology_agent("docs", "%2", "default", AgentState::Idle, "docs", 2),
    ];
    let mut app = App::with_config(WatchConfig {
        view: WatchView::Window,
        layout: WatchLayout::Tree,
        tree_expansion: WatchTreeExpansion::Focus,
        sort: vec![WatchSortKey::Name, WatchSortKey::Pane],
        hide_paneless: false,
        ..WatchConfig::default()
    });
    app.set_data(agents, panes);

    let labels = |app: &App| {
        app.tree_targets()
            .iter()
            .map(|target| match app.topology.find(&target.key).unwrap() {
                TopologyNodeRef::Session(session) => format!("s:{}", session.name),
                TopologyNodeRef::Window(window) => format!("w:{}", window.name),
                TopologyNodeRef::Pane(pane) => format!("p:{}", pane.key.pane_id),
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(labels(&app), vec!["s:alpha", "w:AUTH", "s:beta"]);

    // Alpha's window is visible as context, but vertical movement stays
    // at session depth. One keypress reaches beta and folds alpha.
    app.move_down();
    assert!(matches!(
        app.selected_node(),
        Some(TopologyNodeRef::Session(session)) if session.name == "beta"
    ));
    assert_eq!(labels(&app), vec!["s:alpha", "s:beta", "w:DOCS"]);

    app.move_up();
    app.move_into_work();
    assert!(matches!(
        app.selected_node(),
        Some(TopologyNodeRef::Window(window)) if window.name == "AUTH"
    ));
    app.move_to_work_parent();
    assert!(matches!(
        app.selected_node(),
        Some(TopologyNodeRef::Session(session)) if session.name == "alpha"
    ));

    // A single window has no useful sibling stop, so j bubbles to the
    // session group instead of wrapping onto the same window forever.
    app.move_into_work();
    app.move_down();
    assert!(matches!(
        app.selected_node(),
        Some(TopologyNodeRef::Session(session)) if session.name == "beta"
    ));
    app.move_into_work();
    app.move_up();
    assert!(matches!(
        app.selected_node(),
        Some(TopologyNodeRef::Session(session)) if session.name == "alpha"
    ));
}

#[test]
fn focus_navigation_moves_within_each_sibling_group() {
    let panes = vec![
        fake_topology_pane("default", "$1", "alpha", "@1", "AUTH", 0, "%1", 0),
        fake_topology_pane("default", "$1", "alpha", "@1", "AUTH", 0, "%2", 1),
        fake_topology_pane("default", "$1", "alpha", "@2", "DOCS", 1, "%3", 0),
        fake_topology_pane("default", "$2", "beta", "@3", "REVIEW", 0, "%4", 0),
    ];
    let agents = vec![
        topology_agent("auth-1", "%1", "default", AgentState::Working, "auth", 1),
        topology_agent("auth-2", "%2", "default", AgentState::Idle, "auth", 2),
        topology_agent("docs", "%3", "default", AgentState::Idle, "docs", 3),
        topology_agent("review", "%4", "default", AgentState::Idle, "review", 4),
    ];
    let mut app = App::with_config(WatchConfig {
        view: WatchView::Pane,
        layout: WatchLayout::Tree,
        tree_expansion: WatchTreeExpansion::Focus,
        sort: vec![WatchSortKey::Name, WatchSortKey::Pane],
        hide_paneless: false,
        ..WatchConfig::default()
    });
    app.set_data(agents, panes);

    // l descends into alpha's first window. j skips the visible pane
    // context and selects the next window in the same session.
    app.move_into_work();
    assert!(matches!(
        app.selected_node(),
        Some(TopologyNodeRef::Window(window)) if window.name == "AUTH"
    ));
    app.move_down();
    assert!(matches!(
        app.selected_node(),
        Some(TopologyNodeRef::Window(window)) if window.name == "DOCS"
    ));

    // DOCS has one pane, so descending and pressing j bubbles back to the
    // window siblings and advances to AUTH instead of getting stuck.
    app.move_into_work();
    assert!(matches!(
        app.selected_node(),
        Some(TopologyNodeRef::Pane(pane)) if pane.key.pane_id == "%3"
    ));
    app.move_down();
    assert!(matches!(
        app.selected_node(),
        Some(TopologyNodeRef::Window(window)) if window.name == "AUTH"
    ));

    // AUTH has two panes, so pane-level j stays within that real sibling
    // group. h then ascends one level per keypress.
    app.move_into_work();
    assert!(matches!(
        app.selected_node(),
        Some(TopologyNodeRef::Pane(pane)) if pane.key.pane_id == "%1"
    ));
    app.move_down();
    assert!(matches!(
        app.selected_node(),
        Some(TopologyNodeRef::Pane(pane)) if pane.key.pane_id == "%2"
    ));
    app.move_to_work_parent();
    assert!(matches!(
        app.selected_node(),
        Some(TopologyNodeRef::Window(_))
    ));
    app.move_to_work_parent();
    assert!(matches!(
        app.selected_node(),
        Some(TopologyNodeRef::Session(_))
    ));
}

#[test]
fn focus_explicit_descent_selects_and_keeps_a_pane_beyond_auto_depth() {
    let panes = vec![fake_topology_pane(
        "default", "$1", "alpha", "@1", "AUTH", 0, "%1", 0,
    )];
    let agents = vec![topology_agent(
        "auth",
        "%1",
        "default",
        AgentState::Working,
        "auth",
        1,
    )];

    for view in [WatchView::Session, WatchView::Window] {
        let mut app = App::with_config(WatchConfig {
            view,
            layout: WatchLayout::Tree,
            tree_expansion: WatchTreeExpansion::Focus,
            hide_paneless: false,
            ..WatchConfig::default()
        });
        app.set_data(agents.clone(), panes.clone());

        // Each l enters one hierarchy level even when that child sits
        // below the configured automatic expansion depth.
        app.move_into_work();
        assert!(matches!(
            app.selected_node(),
            Some(TopologyNodeRef::Window(_))
        ));
        app.move_into_work();
        assert!(matches!(
            app.selected_node(),
            Some(TopologyNodeRef::Pane(pane)) if pane.key.pane_id == "%1"
        ));

        // A refresh must not fold the selected pane out of the target
        // list merely because view=session/window.
        app.set_data(agents.clone(), panes.clone());
        assert!(matches!(
            app.selected_node(),
            Some(TopologyNodeRef::Pane(pane)) if pane.key.pane_id == "%1"
        ));
        app.move_to_work_parent();
        assert!(matches!(
            app.selected_node(),
            Some(TopologyNodeRef::Window(_))
        ));
    }
}

#[test]
fn tree_expansion_policy_supports_always_and_manual() {
    let panes = vec![
        fake_topology_pane("default", "$1", "alpha", "@1", "AUTH", 0, "%1", 0),
        fake_topology_pane("default", "$2", "beta", "@2", "DOCS", 0, "%2", 0),
    ];
    let agents = vec![
        topology_agent("auth", "%1", "default", AgentState::Working, "auth", 1),
        topology_agent("docs", "%2", "default", AgentState::Idle, "docs", 2),
    ];

    for (policy, expected_depths) in [
        (WatchTreeExpansion::Always, vec![0, 1, 0, 1]),
        (WatchTreeExpansion::Manual, vec![0, 0]),
    ] {
        let mut app = App::with_config(WatchConfig {
            view: WatchView::Window,
            layout: WatchLayout::Tree,
            tree_expansion: policy,
            sort: vec![WatchSortKey::Name, WatchSortKey::Pane],
            hide_paneless: false,
            ..WatchConfig::default()
        });
        app.set_data(agents.clone(), panes.clone());
        assert_eq!(
            app.tree_targets()
                .iter()
                .map(|target| target.depth)
                .collect::<Vec<_>>(),
            expected_depths
        );
        if policy == WatchTreeExpansion::Always {
            app.move_down();
            assert!(matches!(
                app.selected_node_key(),
                Some(TopologyNodeKey::Window(_))
            ));
        }
    }
}

#[test]
fn pane_focus_skips_a_single_window_and_descends_directly_to_its_pane() {
    let panes = vec![fake_topology_pane(
        "default", "$1", "alpha", "@1", "AUTH", 0, "%1", 0,
    )];
    let agents = vec![topology_agent(
        "auth",
        "%1",
        "default",
        AgentState::Working,
        "auth",
        1,
    )];
    let mut app = App::with_config(WatchConfig {
        view: WatchView::Pane,
        layout: WatchLayout::Tree,
        tree_expansion: WatchTreeExpansion::Focus,
        hide_paneless: false,
        ..WatchConfig::default()
    });
    app.set_data(agents, panes);
    assert_eq!(
        app.tree_targets()
            .iter()
            .map(|target| target.depth)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert!(app.tree_targets()[0].compacted_window.is_some());
    assert!(matches!(
        app.tree_targets()[1].key,
        TopologyNodeKey::Pane(_)
    ));

    app.move_into_work();
    assert!(matches!(
        app.selected_node(),
        Some(TopologyNodeRef::Pane(pane)) if pane.key.pane_id == "%1"
    ));
    app.move_to_work_parent();
    assert!(matches!(
        app.selected_node(),
        Some(TopologyNodeRef::Session(session)) if session.name == "alpha"
    ));
}

#[test]
fn pane_filter_restores_the_full_single_window_ancestry() {
    let (agents, panes) = basic_topology_fixture();
    let mut app = topology_watch(WatchView::Pane, agents, panes);
    assert_eq!(
        app.tree_targets()
            .iter()
            .map(|target| target.depth)
            .collect::<Vec<_>>(),
        vec![0, 1, 1]
    );

    app.search_query = "auth".into();
    let filtered = app.tree_targets();
    assert_eq!(
        filtered
            .iter()
            .map(|target| target.depth)
            .collect::<Vec<_>>(),
        vec![0, 1, 2, 2]
    );
    assert!(filtered
        .iter()
        .all(|target| target.compacted_window.is_none()));
}

#[test]
fn compacted_row_keeps_session_actions_and_exact_window_identity() {
    let (agents, panes) = basic_topology_fixture();
    let mut app = topology_watch(WatchView::Pane, agents, panes);
    let session_key = app.topology.sessions[0].node_key();
    let window_key = app.topology.sessions[0].windows[0].key.clone();
    let compacted = &app.tree_targets()[0];
    assert_eq!(compacted.key, session_key);
    assert_eq!(compacted.compacted_window.as_ref(), Some(&window_key));

    app.table_state.select(Some(0));
    assert!(matches!(
        quick_kill_action(&app),
        Action::AskConfirm(ConfirmPopup {
            on_confirm: ConfirmAction::Quick(QuickAction::TerminateNode(ref key)),
            ..
        }) if key == &session_key
    ));

    app.apply_view(WatchView::Window);
    assert!(app
        .tree_targets()
        .iter()
        .any(|target| target.key == TopologyNodeKey::Window(window_key.clone())));
}

#[test]
fn expanded_single_child_chain_shows_state_only_on_the_deepest_row() {
    let panes = vec![fake_topology_pane(
        "default", "$1", "muxa", "@1", "AUTH", 0, "%1", 0,
    )];
    let agents = vec![topology_agent(
        "agent-1",
        "%1",
        "default",
        AgentState::Working,
        "edit auth",
        1,
    )];
    let app = topology_watch(WatchView::Pane, agents, panes);
    let targets = app.tree_targets();
    assert_eq!(targets.len(), 2);

    let redundant = targets
        .iter()
        .map(|target| {
            let node = app.topology.find(&target.key).unwrap();
            tree_state_is_redundant(target, node)
        })
        .collect::<Vec<_>>();
    assert_eq!(redundant, vec![true, false]);

    let theme = watch_theme(WatchTheme::default());
    let state_cells = targets
        .iter()
        .map(|target| {
            let node = app.topology.find(&target.key).unwrap();
            tree_state_cell(target, node, theme, Spinner::OFF)
        })
        .collect::<Vec<_>>();
    assert!(state_cells[0].lines.is_empty());
    assert!(!state_cells[1].lines.is_empty());
}

#[test]
fn collapsed_parents_and_branch_points_keep_aggregate_state() {
    let panes = vec![
        fake_topology_pane("default", "$1", "muxa", "@1", "AUTH", 0, "%1", 0),
        fake_topology_pane("default", "$1", "muxa", "@1", "AUTH", 0, "%2", 1),
    ];
    let agents = vec![
        topology_agent("agent-1", "%1", "default", AgentState::Working, "one", 1),
        topology_agent("agent-2", "%2", "default", AgentState::Idle, "two", 2),
    ];

    let session_app = topology_watch(WatchView::Session, agents.clone(), panes.clone());
    let session_target = &session_app.tree_targets()[0];
    let session = session_app.topology.find(&session_target.key).unwrap();
    assert!(!tree_state_is_redundant(session_target, session));

    let pane_app = topology_watch(WatchView::Pane, agents, panes);
    let targets = pane_app.tree_targets();
    let session = pane_app.topology.find(&targets[0].key).unwrap();
    // The session and its only window are one visual row. Its two pane
    // children form a real branch, so the compacted parent keeps the
    // aggregate marker.
    assert!(!tree_state_is_redundant(&targets[0], session));
}

#[test]
fn window_summary_aggregates_all_child_agent_activity() {
    let (agents, panes) = basic_topology_fixture();
    let app = topology_watch(WatchView::Pane, agents, panes);
    let summary = window_node_summary(&app.topology.sessions[0].windows[0], WatchLayout::Tree);
    assert_eq!(summary, "2 agents · review auth / edit auth");
}

#[test]
fn window_summary_folds_duplicate_activity_and_handles_bare_panes() {
    let panes = vec![
        fake_topology_pane("default", "$1", "muxa", "@1", "TEST-1", 0, "%1", 0),
        fake_topology_pane("default", "$1", "muxa", "@1", "TEST-1", 0, "%2", 1),
    ];
    let agents = vec![
        topology_agent(
            "one",
            "%1",
            "default",
            AgentState::Working,
            "shared task",
            1,
        ),
        topology_agent("two", "%2", "default", AgentState::Idle, "shared task", 2),
    ];
    let app = topology_watch(WatchView::Pane, agents, panes.clone());
    assert_eq!(
        window_node_summary(&app.topology.sessions[0].windows[0], WatchLayout::Tree),
        "2 agents · shared task ×2"
    );

    let app = topology_watch(WatchView::Pane, Vec::new(), panes);
    assert_eq!(
        window_node_summary(&app.topology.sessions[0].windows[0], WatchLayout::Tree),
        "2 panes · no tracked agents"
    );
}

#[test]
fn latest_updates_reorder_windows_without_changing_selected_target() {
    for (view, snapshot) in [
        (WatchView::Window, false),
        (WatchView::Pane, false),
        (WatchView::Window, true),
        (WatchView::Pane, true),
    ] {
        let panes = vec![
            fake_topology_pane("default", "$1", "alpha", "@1", "AUTH", 0, "%1", 0),
            fake_topology_pane("default", "$1", "alpha", "@2", "DOCS", 1, "%2", 0),
        ];
        let auth = topology_agent("auth", "%1", "default", AgentState::Working, "auth", 1);
        let docs = topology_agent("docs", "%2", "default", AgentState::Working, "docs", 2);
        let mut app = topology_watch(view, vec![auth.clone(), docs], panes);
        app.apply_sort_preset(WatchSortPreset::Latest);
        let selected = app
            .tree_targets()
            .into_iter()
            .find(|target| match &target.key {
                TopologyNodeKey::Window(key) => view == WatchView::Window && key.window_id == "@2",
                TopologyNodeKey::Pane(key) => view == WatchView::Pane && key.pane_id == "%2",
                TopologyNodeKey::Session(_) => false,
            })
            .unwrap()
            .key;
        let index = app
            .tree_targets()
            .iter()
            .position(|target| target.key == selected)
            .unwrap();
        app.table_state.select(Some(index));
        let mut updated = auth;
        updated.last_activity_at += time::Duration::minutes(10);
        let outcome = if snapshot {
            let agents = app
                .current_agents()
                .into_iter()
                .map(|agent| {
                    if agent.session_id == updated.session_id {
                        updated.clone()
                    } else {
                        agent
                    }
                })
                .collect();
            RefreshOutcome::Agents(agents)
        } else {
            RefreshOutcome::SingleAgent(updated)
        };
        apply_outcome(&mut app, outcome);
        let windows = app.sorted_windows(app.topology.sessions[0].windows.iter());
        assert_eq!(
            windows[0].key.window_id, "@1",
            "latest activity should move AUTH first"
        );
        assert_eq!(
            app.selected_node_key(),
            Some(selected),
            "push must preserve the selected target in {view:?}"
        );
    }
}

#[test]
fn initial_pane_selects_nearest_visible_ancestor_after_empty_first_frame() {
    let panes = vec![
        fake_topology_pane("default", "$1", "alpha", "@1", "AUTH", 0, "%1", 0),
        fake_topology_pane("default", "$1", "alpha", "@2", "DOCS", 1, "%2", 0),
        fake_topology_pane("default", "$2", "beta", "@3", "REVIEW", 0, "%3", 0),
    ];
    let agents = vec![
        topology_agent("auth", "%1", "default", AgentState::Working, "auth", 1),
        topology_agent("docs", "%2", "default", AgentState::Idle, "docs", 2),
        topology_agent(
            "review",
            "%3",
            "default",
            AgentState::WaitingInput,
            "review",
            3,
        ),
    ];

    for view in [WatchView::Session, WatchView::Window, WatchView::Pane] {
        let mut app = App::with_config(WatchConfig {
            view,
            layout: WatchLayout::Tree,
            sort: vec![WatchSortKey::Name, WatchSortKey::Pane],
            hide_paneless: false,
            ..WatchConfig::default()
        });
        app.set_initial_pane(Some("%3".into()));
        assert!(app.tree_targets().is_empty());

        app.set_data(agents.clone(), panes.clone());

        match (view, app.selected_node_key().unwrap()) {
            (WatchView::Session, TopologyNodeKey::Session(key)) => {
                assert_eq!(key.session_id, "$2");
            }
            (WatchView::Window, TopologyNodeKey::Window(key)) => {
                assert_eq!(key.window_id, "@3");
            }
            (WatchView::Pane, TopologyNodeKey::Pane(key)) => {
                assert_eq!(key.pane_id, "%3");
            }
            (view, key) => panic!("unexpected initial selection for {view:?}: {key:?}"),
        }
    }
}

#[test]
fn initial_pane_endpoint_disambiguates_duplicate_native_ids() {
    let panes = vec![
        fake_topology_pane("default", "$1", "alpha", "@1", "AUTH", 0, "%1", 0),
        fake_topology_pane("amux", "$1", "beta", "@1", "AUTH", 0, "%1", 0),
    ];
    let agents = vec![
        topology_agent("default-agent", "%1", "default", AgentState::Idle, "one", 1),
        topology_agent("amux-agent", "%1", "amux", AgentState::Working, "two", 2),
    ];
    let mut app = App::with_config(WatchConfig {
        view: WatchView::Pane,
        layout: WatchLayout::Tree,
        sort: vec![WatchSortKey::Name, WatchSortKey::Pane],
        hide_paneless: false,
        ..WatchConfig::default()
    });
    app.set_initial_pane_on(
        Some("%1".into()),
        Some(BackendEndpoint {
            host: muxa::HostKind::Tmux,
            socket: "amux".into(),
        }),
    );

    app.set_data(agents, panes);

    let TopologyNodeKey::Pane(key) = app.selected_node_key().unwrap() else {
        panic!("expected the exact pane to be selected");
    };
    assert_eq!(key.window.session.endpoint.socket, "amux");
}

#[test]
fn tree_left_and_right_expand_enter_child_collapse_and_return_parent() {
    let (agents, panes) = basic_topology_fixture();
    let mut app = topology_watch(WatchView::Session, agents, panes);
    assert_eq!(app.tree_targets().len(), 1);

    app.move_into_work();
    assert_eq!(app.tree_targets().len(), 2);
    assert_eq!(app.table_state.selected(), Some(0));
    app.move_into_work();
    assert!(matches!(
        app.selected_node_key(),
        Some(TopologyNodeKey::Window(_))
    ));
    app.move_into_work();
    assert_eq!(app.tree_targets().len(), 4);
    app.move_into_work();
    assert!(matches!(
        app.selected_node_key(),
        Some(TopologyNodeKey::Pane(_))
    ));
    app.move_to_work_parent();
    assert!(matches!(
        app.selected_node_key(),
        Some(TopologyNodeKey::Window(_))
    ));
    app.move_to_work_parent();
    assert_eq!(app.tree_targets().len(), 2);
}

#[test]
fn tree_refresh_preserves_full_key_selection_and_expansion() {
    let (agents, panes) = basic_topology_fixture();
    let mut app = topology_watch(WatchView::Pane, agents.clone(), panes.clone());
    let pane_key = app.topology.sessions[0].windows[0].panes[1].node_key();
    select_tree_key(&mut app, &pane_key);

    let mut reordered = panes;
    reordered.reverse();
    app.set_data(agents, reordered);

    assert_eq!(app.selected_node_key().as_ref(), Some(&pane_key));
    assert!(app.expanded_nodes.contains(&TopologyNodeKey::Session(
        app.topology.sessions[0].key.clone()
    )));
    assert!(app.expanded_nodes.contains(&TopologyNodeKey::Window(
        app.topology.sessions[0].windows[0].key.clone()
    )));
}

#[test]
fn tree_search_and_attention_keep_matching_ancestors() {
    let mut other = topology_agent(
        "other-agent",
        "%9",
        "amux",
        AgentState::Idle,
        "unrelated docs",
        0,
    );
    other.last_response = Some("nothing relevant".into());
    let panes = vec![
        fake_topology_pane("default", "$1", "muxa", "@1", "AUTH", 0, "%1", 0),
        fake_topology_pane("amux", "$2", "other", "@2", "DOCS", 0, "%9", 0),
    ];
    let agents = vec![
        topology_agent(
            "waiting-agent",
            "%1",
            "default",
            AgentState::WaitingChoice,
            "needle token",
            1,
        ),
        other,
    ];
    let mut app = topology_watch(WatchView::Session, agents, panes);
    app.search_query = "needle".into();
    let targets = app.tree_targets();
    assert_eq!(
        targets
            .iter()
            .map(|target| target.depth)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert!(targets.iter().all(|target| match &target.key {
        TopologyNodeKey::Session(key) => key.endpoint.socket == "default",
        TopologyNodeKey::Window(key) => key.session.endpoint.socket == "default",
        TopologyNodeKey::Pane(key) => key.window.session.endpoint.socket == "default",
    }));

    app.search_query.clear();
    app.attention_only = true;
    assert_eq!(
        app.tree_targets()
            .iter()
            .map(|target| target.depth)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
}

#[test]
fn clearing_tree_filter_restores_the_full_hidden_selection_key() {
    let panes = vec![
        fake_topology_pane("default", "$1", "alpha", "@1", "AUTH", 0, "%1", 0),
        fake_topology_pane("amux", "$1", "beta", "@1", "DOCS", 0, "%1", 0),
    ];
    let agents = vec![
        topology_agent("auth", "%1", "default", AgentState::Working, "needle", 1),
        topology_agent("docs", "%1", "amux", AgentState::Idle, "manual", 2),
    ];
    let mut app = topology_watch(WatchView::Pane, agents, panes);
    let hidden_key = app
        .topology
        .sessions
        .iter()
        .find(|session| session.key.endpoint.socket == "amux")
        .unwrap()
        .windows[0]
        .panes[0]
        .node_key();
    select_tree_key(&mut app, &hidden_key);

    app.edit_search(|query| query.push_str("needle"));
    assert_ne!(app.selected_node_key().as_ref(), Some(&hidden_key));
    app.clear_search();

    assert_eq!(app.selected_node_key().as_ref(), Some(&hidden_key));
}

#[test]
fn tree_sort_is_sibling_scoped_and_keeps_each_subtree_contiguous() {
    let panes = vec![
        fake_topology_pane("default", "$2", "zeta", "@3", "z-window", 1, "%3", 0),
        fake_topology_pane("default", "$2", "zeta", "@2", "a-window", 0, "%2", 0),
        fake_topology_pane("amux", "$1", "alpha", "@1", "only", 0, "%1", 0),
    ];
    let agents = vec![
        topology_agent("a3", "%3", "default", AgentState::Idle, "three", 30),
        topology_agent("a2", "%2", "default", AgentState::Idle, "two", 20),
        topology_agent("a1", "%1", "amux", AgentState::Idle, "one", 10),
    ];
    let app = topology_watch(WatchView::Pane, agents, panes);
    let targets = app.tree_targets();
    let labels = targets
        .iter()
        .map(|target| match app.topology.find(&target.key).unwrap() {
            TopologyNodeRef::Session(session) => format!("s:{}", session.name),
            TopologyNodeRef::Window(window) => format!("w:{}", window.name),
            TopologyNodeRef::Pane(pane) => format!("p:{}", pane.key.pane_id),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        labels,
        vec![
            "s:alpha",
            "p:%1",
            "s:zeta",
            "w:a-window",
            "p:%2",
            "w:z-window",
            "p:%3"
        ]
    );
}

#[test]
fn pane_duration_sort_uses_state_age_not_agent_lifetime() {
    let panes = vec![
        fake_topology_pane("default", "$1", "main", "@1", "work", 0, "%1", 0),
        fake_topology_pane("default", "$1", "main", "@1", "work", 0, "%2", 1),
    ];
    let now = OffsetDateTime::now_utc();
    let mut long_lived_recent_state =
        topology_agent("long-lived", "%1", "default", AgentState::Working, "one", 0);
    long_lived_recent_state.started_at = now - time::Duration::hours(2);
    long_lived_recent_state.state_entered_at = now - time::Duration::seconds(10);
    let mut short_lived_old_state =
        topology_agent("short-lived", "%2", "default", AgentState::Idle, "two", 0);
    short_lived_old_state.started_at = now - time::Duration::minutes(1);
    short_lived_old_state.state_entered_at = now - time::Duration::seconds(50);

    let mut app = App::with_config(WatchConfig {
        view: WatchView::Pane,
        layout: WatchLayout::Tree,
        tree_expansion: WatchTreeExpansion::Always,
        sort: vec![WatchSortKey::Duration],
        hide_paneless: false,
        ..WatchConfig::default()
    });
    app.set_data(vec![long_lived_recent_state, short_lived_old_state], panes);

    let pane_ids = app
        .tree_targets()
        .into_iter()
        .filter_map(|target| match target.key {
            TopologyNodeKey::Pane(key) => Some(key.pane_id),
            TopologyNodeKey::Session(_) | TopologyNodeKey::Window(_) => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(pane_ids, vec!["%2", "%1"]);
}

#[test]
fn parent_attention_rank_uses_the_same_choice_before_input_priority_as_panes() {
    let states = StateDistribution {
        waiting_input: 1,
        waiting_choice: 1,
        ..StateDistribution::default()
    };
    assert_eq!(
        distribution_rank(&states),
        state_sort_rank(AgentState::WaitingChoice)
    );
}

#[test]
fn duplicate_native_ids_use_exact_socket_for_attach_close_interrupt_and_send() {
    let panes = vec![
        fake_topology_pane("default", "$1", "main", "@1", "work", 0, "%1", 0),
        fake_topology_pane("amux", "$1", "main", "@1", "work", 0, "%1", 0),
    ];
    let agents = vec![
        topology_agent(
            "default-agent",
            "%1",
            "default",
            AgentState::Working,
            "one",
            0,
        ),
        topology_agent("amux-agent", "%1", "amux", AgentState::Working, "two", 1),
    ];
    let mut app = topology_watch(WatchView::Pane, agents, panes);
    let pane_key = app
        .topology
        .sessions
        .iter()
        .find(|session| session.key.endpoint.socket == "amux")
        .unwrap()
        .windows[0]
        .panes[0]
        .key
        .clone();
    select_tree_key(&mut app, &TopologyNodeKey::Pane(pane_key.clone()));
    app.apply_layout(WatchLayout::Swarm);
    assert_eq!(
        app.selected_node_key(),
        Some(TopologyNodeKey::Pane(pane_key.clone()))
    );
    app.apply_layout(WatchLayout::Tree);

    assert!(matches!(
        quick_prompt_action(&app),
        Action::AttachTopologyPane(ref key) if key == &pane_key
    ));
    assert!(matches!(
        quick_kill_action(&app),
        Action::AskConfirm(ConfirmPopup {
            on_confirm: ConfirmAction::Quick(QuickAction::TerminateNode(
                TopologyNodeKey::Pane(ref key)
            )),
            ..
        }) if key == &pane_key
    ));
    assert!(matches!(
        quick_abort_action(&app),
        Action::AskConfirm(ConfirmPopup {
            on_confirm: ConfirmAction::Quick(QuickAction::InterruptPane(ref key)),
            ..
        }) if key == &pane_key
    ));

    open_prompt_only_composer(&mut app, "test".into());
    app.collaboration_composer.as_mut().unwrap().insert('x');
    assert!(matches!(
        composer_submit_action(&mut app),
        Action::Quick(QuickAction::SendPromptTo { pane: ref key, ref text })
            if key == &pane_key && text == "x"
    ));
}

#[test]
fn parent_inspector_never_renders_a_representative_pane_capture() {
    let (agents, panes) = basic_topology_fixture();
    let mut app = topology_watch(WatchView::Pane, agents, panes);
    app.pane_capture = Some(CapturedPane::new(
        "%1".into(),
        Some("default".into()),
        "SHOULD-NOT-LEAK".into(),
    ));
    app.table_state.select(Some(0));

    let backend = TestBackend::new(150, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let screen = (0..terminal.backend().buffer().area().height)
        .map(|y| row_text(terminal.backend().buffer(), y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(screen.contains("Inspector · session muxa"));
    assert!(screen.contains("2 panes"));
    assert!(!screen.contains("SHOULD-NOT-LEAK"));
}

#[test]
fn session_inspector_rolls_up_attention_activity_and_windows() {
    let (agents, panes) = basic_topology_fixture();
    let mut app = topology_watch(WatchView::Pane, agents, panes);
    app.watch_cfg.spinner = false;
    app.inspector_split = InspectorSplit::InspectorWide;

    let backend = TestBackend::new(170, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let screen = (0..terminal.backend().buffer().area().height)
        .map(|y| row_text(terminal.backend().buffer(), y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(screen.contains("Inspector · session muxa"));
    assert!(screen.contains("scope  1 windows · 2 panes · 2 agents"));
    assert!(screen.contains("attention  TEST-1/%2 · WAIT"));
    assert!(screen.contains("latest"));
    assert!(screen.contains("codex · TEST-1/%2 · review auth"));
    assert!(screen.contains("topology  WINDOWS / PANES · A/P · STATE · AGE · MODEL/CTX · LATEST"));
    assert!(screen.contains("0:TEST-1"));
    assert!(screen.contains("2/2"));
    assert!(screen.contains("├─● %1"));
    assert!(screen.contains("└─▶ %2"));
    assert!(screen.contains("gpt-5"));
    assert!(screen.contains("edit auth"));
}

#[test]
fn live_layout_omits_only_marked_sidebars_and_reclaims_their_width() {
    let row = |id, left, width, command, mark| {
        format!("{id}\t0\t{left}\t0\t{width}\t40\t0\t0\t{command}\t\t\t{mark}")
    };
    for side in ["left", "right"] {
        let offset = if side == "left" { 37 } else { 0 };
        let raw = [
            row("%9", if side == "left" { 0 } else { 104 }, 36, "muxa", "1"),
            row("%1", offset, 50, "codex", ""),
            row("%2", offset + 51, 52, "muxa", ""),
        ]
        .join("\n");
        let (panes, zoomed) = content_window_geometry(&raw);
        assert!(!zoomed);
        assert_eq!(
            panes
                .iter()
                .map(|pane| pane.pane_id.as_str())
                .collect::<Vec<_>>(),
            vec!["%1", "%2"]
        );
        assert_eq!((panes[0].left, panes[1].left + panes[1].width), (0, 103));
    }
}
#[test]
fn session_inspector_fills_available_height_then_summarizes_hidden_nodes() {
    let panes = (0..12)
        .map(|index| {
            fake_topology_pane(
                "default",
                "$1",
                "muxa",
                "@1",
                "TEST-1",
                0,
                &format!("%{index}"),
                index,
            )
        })
        .collect::<Vec<_>>();
    let agents = (0..12)
        .map(|index| {
            topology_agent(
                &format!("agent-{index}"),
                &format!("%{index}"),
                "default",
                AgentState::Working,
                &format!("task {index}"),
                i64::from(index),
            )
        })
        .collect::<Vec<_>>();
    let mut app = topology_watch(WatchView::Pane, agents, panes);
    app.watch_cfg.spinner = false;
    app.inspector_split = InspectorSplit::InspectorWide;

    let backend = TestBackend::new(170, 16);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let inspector = (0..terminal.backend().buffer().area().height)
        .map(|y| {
            row_text(terminal.backend().buffer(), y)
                .chars()
                .skip(51)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(inspector.contains("0:TEST-1"));
    assert!(inspector.contains("├─●"));
    assert!(inspector.contains("more"));
}

#[test]
fn window_inspector_rolls_up_load_and_keeps_panes_actionable() {
    let (mut agents, panes) = basic_topology_fixture();
    agents[0].workload.process_count = 7;
    agents[0].workload.shell_count = 2;
    agents[0].workload.subagent_count = 1;
    agents[0].context_used_pct = Some(82.0);
    agents[0].cost_usd = Some(1.25);
    agents[1].cost_usd = Some(0.50);
    let mut app = topology_watch(WatchView::Window, agents, panes);
    app.watch_cfg.spinner = false;
    app.inspector_split = InspectorSplit::InspectorWide;
    let window_key = app.topology.sessions[0].windows[0].node_key();
    select_tree_key(&mut app, &window_key);

    let backend = TestBackend::new(170, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let screen = (0..terminal.backend().buffer().area().height)
        .map(|y| row_text(terminal.backend().buffer(), y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(screen.contains("Inspector · window TEST-1"));
    assert!(screen.contains("scope  2 panes · 2 agents"));
    assert!(
        screen.contains("load  7 processes · 2 shells · 1 subagents · ctx max 82% · cost $1.75")
    );
    assert!(screen.contains("attention  %2 · WAIT"));
    assert!(screen.contains("panes  AGENT    STATE  AGE  MODEL      CTX  LATEST"));
    assert!(screen.contains("%1"));
    assert!(screen.contains("codex"));
    assert!(screen.contains("gpt-5"));
    assert!(screen.contains("82%"));
    assert!(screen.contains("edit auth"));
    assert!(screen.contains("review auth"));
}

#[test]
fn scaled_window_pane_rect_preserves_split_proportions() {
    let left = PaneGeometry {
        pane_id: "%1".into(),
        pane_index: "0".into(),
        left: 0,
        top: 0,
        width: 49,
        height: 20,
        active: true,
        command: "codex".into(),
        alias: None,
        spawn: None,
    };
    let right = PaneGeometry {
        pane_id: "%2".into(),
        pane_index: "1".into(),
        left: 50,
        top: 0,
        width: 50,
        height: 20,
        active: false,
        command: "codex".into(),
        alias: None,
        spawn: None,
    };
    let area = Rect::new(10, 4, 80, 12);

    assert_eq!(
        scaled_window_pane_rect(&left, 100, 20, area),
        Some(Rect::new(10, 4, 39, 12))
    );
    assert_eq!(
        scaled_window_pane_rect(&right, 100, 20, area),
        Some(Rect::new(50, 4, 40, 12))
    );
}

#[test]
fn window_inspector_renders_live_capture_in_real_pane_layout() {
    let (agents, panes) = basic_topology_fixture();
    let mut app = topology_watch(WatchView::Window, agents, panes);
    app.watch_cfg.spinner = false;
    app.inspector_split = InspectorSplit::InspectorWide;
    let window_key = app.topology.sessions[0].windows[0].key.clone();
    select_tree_key(&mut app, &TopologyNodeKey::Window(window_key.clone()));
    app.window_capture = Some(CapturedWindow {
        key: window_key,
        zoomed: false,
        fetched_at: std::time::Instant::now(),
        panes: vec![
            CapturedWindowPane {
                geometry: PaneGeometry {
                    pane_id: "%1".into(),
                    pane_index: "0".into(),
                    left: 0,
                    top: 0,
                    width: 49,
                    height: 20,
                    active: true,
                    command: "codex".into(),
                    alias: None,
                    spawn: None,
                },
                text: Some(Text::from("LEFT CAPTURE")),
            },
            CapturedWindowPane {
                geometry: PaneGeometry {
                    pane_id: "%2".into(),
                    pane_index: "1".into(),
                    left: 50,
                    top: 0,
                    width: 50,
                    height: 20,
                    active: false,
                    command: "codex".into(),
                    alias: None,
                    spawn: None,
                },
                text: Some(Text::from("RIGHT CAPTURE")),
            },
        ],
    });

    let backend = TestBackend::new(170, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let rows = (0..terminal.backend().buffer().area().height)
        .map(|y| row_text(terminal.backend().buffer(), y))
        .collect::<Vec<_>>();
    let screen = rows.join("\n");

    assert!(screen.contains("live layout  pane geometry · 1s refresh"));
    assert!(screen.contains("LEFT CAPTURE"));
    assert!(screen.contains("RIGHT CAPTURE"));
    let capture_row = rows
        .iter()
        .find(|row| row.contains("LEFT CAPTURE") && row.contains("RIGHT CAPTURE"))
        .expect("both horizontal pane captures should share one visual row");
    assert!(capture_row.find("LEFT CAPTURE").unwrap() < capture_row.find("RIGHT CAPTURE").unwrap());
}

#[test]
fn compact_window_inspector_drops_secondary_columns_first() {
    let (agents, panes) = basic_topology_fixture();
    let mut app = topology_watch(WatchView::Window, agents, panes);
    app.watch_cfg.spinner = false;
    let window_key = app.topology.sessions[0].windows[0].key.clone();
    select_tree_key(&mut app, &TopologyNodeKey::Window(window_key.clone()));
    app.window_capture = Some(CapturedWindow {
        key: window_key,
        zoomed: false,
        fetched_at: std::time::Instant::now(),
        panes: vec![CapturedWindowPane {
            geometry: PaneGeometry {
                pane_id: "%1".into(),
                pane_index: "0".into(),
                left: 0,
                top: 0,
                width: 100,
                height: 30,
                active: true,
                command: "codex".into(),
                alias: None,
                spawn: None,
            },
            text: Some(Text::from("SHOULD USE ROSTER WHEN TOO SHORT")),
        }],
    });

    // Balanced at 120 columns leaves roughly 58 usable Inspector columns;
    // 18 rows leave too little height for a legible mosaic.
    let backend = TestBackend::new(120, 18);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let screen = (0..terminal.backend().buffer().area().height)
        .map(|y| row_text(terminal.backend().buffer(), y))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(screen.contains("panes  AGENT    STATE  AGE  LATEST"));
    assert!(!screen.contains("MODEL      CTX"));
    assert!(screen.contains("%1"));
    assert!(screen.contains("codex"));
    assert!(screen.contains("edit auth"));
    assert!(!screen.contains("SHOULD USE ROSTER WHEN TOO SHORT"));
}

#[test]
fn inspector_wide_keeps_aggregate_state_glyphs_visible() {
    let panes = vec![
        fake_topology_pane("default", "$1", "muxa", "@1", "TEST-1", 0, "%1", 0),
        fake_topology_pane("default", "$1", "muxa", "@1", "TEST-1", 0, "%2", 1),
    ];
    let agents = vec![
        topology_agent("loading", "%1", "default", AgentState::Starting, "boot", 1),
        topology_agent("idle", "%2", "default", AgentState::Idle, "ready", 2),
    ];
    let mut app = topology_watch(WatchView::Window, agents, panes);
    app.watch_cfg.spinner = false;
    app.inspector_split = InspectorSplit::InspectorWide;
    let window_key = app.topology.sessions[0].windows[0].node_key();
    select_tree_key(&mut app, &window_key);

    let backend = TestBackend::new(150, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let rows = (0..terminal.backend().buffer().area().height)
        .map(|y| row_text(terminal.backend().buffer(), y))
        .collect::<Vec<_>>();
    let screen = rows.join("\n");
    // InspectorWide gives the topology 30% of 150 columns. AGE and
    // SUMMARY must yield at that width; STATE and the hierarchy are the
    // two columns that may never disappear.
    let topology = rows
        .iter()
        .map(|row| row.chars().take(45).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(screen.contains("Inspector · window TEST-1"));
    assert!(
        screen.contains("status  ◌ ○  starting 1 · idle 1"),
        "maximized inspector lost its state glyphs:\n{screen}"
    );
    assert!(
        topology.contains("STATE"),
        "maximized inspector squeezed out the topology STATE gutter:\n{topology}"
    );
    assert!(
        topology.contains("◌ ○"),
        "selected window lost its aggregate state glyphs:\n{topology}"
    );
    assert!(
        !topology.contains("AGE") && !topology.contains("SUMMARY"),
        "secondary columns should fold before STATE at 45 columns:\n{topology}"
    );
}

#[test]
fn topology_columns_fold_in_information_priority_order() {
    assert_eq!(
        topology_table_columns(45),
        vec![TopologyTableColumn::State, TopologyTableColumn::Node]
    );
    assert_eq!(
        topology_table_columns(60),
        vec![
            TopologyTableColumn::State,
            TopologyTableColumn::Node,
            TopologyTableColumn::Summary,
        ]
    );
    assert_eq!(
        topology_table_columns(80),
        vec![
            TopologyTableColumn::State,
            TopologyTableColumn::Node,
            TopologyTableColumn::Age,
            TopologyTableColumn::Summary,
        ]
    );
}

#[test]
fn enter_on_parent_resolves_active_descendant_but_pane_enter_stays_exact() {
    let panes = vec![
        fake_topology_pane("default", "$1", "main", "@1", "old", 0, "%1", 0),
        fake_topology_pane("default", "$1", "main", "@2", "active", 1, "%2", 0),
    ];
    let agents = vec![
        topology_agent("old", "%1", "default", AgentState::Idle, "old", 0),
        topology_agent("active", "%2", "default", AgentState::Working, "new", 60),
    ];
    let mut app = topology_watch(WatchView::Pane, agents, panes);
    app.table_state.select(Some(0));
    let active_key = app.topology.sessions[0].windows[1].panes[0].key.clone();
    assert!(matches!(
        quick_prompt_action(&app),
        Action::AttachTopologyPane(ref key) if key == &active_key
    ));

    let exact_key = app.topology.sessions[0].windows[0].panes[0].key.clone();
    select_tree_key(&mut app, &TopologyNodeKey::Pane(exact_key.clone()));
    assert!(matches!(
        quick_prompt_action(&app),
        Action::AttachTopologyPane(ref key) if key == &exact_key
    ));
}

#[test]
fn spawn_composer_only_edits_a_window_name_for_session_targets() {
    let (agents, panes) = basic_topology_fixture();
    let mut app = topology_watch(WatchView::Window, agents, panes);

    app.table_state.select(Some(0));
    assert!(matches!(key_action(&mut app, 'n'), Action::None));
    let session_spawn = app.spawn.as_mut().expect("session spawn composer");
    assert!(matches!(
        session_spawn.target.as_ref(),
        Some(TopologyNodeKey::Session(_))
    ));
    assert!(session_spawn.name.is_empty());
    session_spawn.focus = SpawnField::Dir;
    let _ = handle_spawn_event(KeyCode::Tab, KeyModifiers::NONE, &mut app);
    assert_eq!(app.spawn.as_ref().unwrap().focus, SpawnField::Name);

    app.spawn = None;
    app.table_state.select(Some(1));
    let expected_window = app.topology.sessions[0].windows[0].name.clone();
    assert!(matches!(key_action(&mut app, 'n'), Action::None));
    let window_spawn = app.spawn.as_mut().expect("window spawn composer");
    assert!(matches!(
        window_spawn.target.as_ref(),
        Some(TopologyNodeKey::Window(_))
    ));
    assert_eq!(window_spawn.name, expected_window);
    window_spawn.focus = SpawnField::Dir;
    let _ = handle_spawn_event(KeyCode::Tab, KeyModifiers::NONE, &mut app);
    assert_eq!(app.spawn.as_ref().unwrap().focus, SpawnField::Agent);
}
