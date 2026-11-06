use super::*;

#[test]
fn aggregate_session_history_is_exact_socket_scoped_and_de_duplicates_internal_traffic() {
    let scope = MailboxHistoryScope::Session {
        key: SessionKey {
            endpoint: BackendEndpoint {
                host: muxa::HostKind::Tmux,
                socket: "default".into(),
            },
            session_id: "$1".into(),
        },
        label: "webapp".into(),
    };
    let internal = fake_watch_collaboration_request(
        "internal",
        participant_on("%1", "default", "$1", "webapp", "@1", "ENG-1"),
        participant_on("%2", "default", "$1", "webapp", "@2", "ENG-2"),
        RequestStatus::Completed,
    );
    let cross_socket = fake_watch_collaboration_request(
        "other-socket",
        participant_on("%1", "amux", "$1", "other", "@1", "ENG-1"),
        participant_on("%2", "amux", "$1", "other", "@2", "ENG-2"),
        RequestStatus::Completed,
    );
    let mut missing_socket = participant_on("%3", "default", "$1", "webapp", "@1", "ENG-1");
    missing_socket.socket = None;
    missing_socket.room.socket = None;
    let missing_socket = fake_watch_collaboration_request(
        "missing-socket",
        missing_socket,
        Participant::console(RoomId {
            host: "tmux".into(),
            socket: Some("default".into()),
            window_id: "@1".into(),
        }),
        RequestStatus::Completed,
    );

    let history = history_for_scope(&[internal, cross_socket, missing_socket], &scope);
    assert_eq!(
        history.len(),
        1,
        "one internal request, never one per endpoint"
    );
    assert_eq!(history[0].id, "internal");
}

#[test]
fn hierarchy_mailbox_scope_keeps_session_and_window_nodes_aggregate() {
    let panes = vec![
        fake_topology_pane("default", "$1", "webapp", "@1", "ENG-1", 0, "%1", 0),
        fake_topology_pane("default", "$1", "webapp", "@2", "ENG-2", 1, "%2", 0),
    ];
    let agents = vec![
        topology_agent("one", "%1", "default", AgentState::Idle, "one", 0),
        topology_agent("two", "%2", "default", AgentState::Working, "two", 1),
    ];
    let mut app = topology_watch(WatchView::Session, agents, panes);
    let session = app.topology.sessions[0].key.clone();
    select_tree_key(&mut app, &TopologyNodeKey::Session(session));
    assert!(matches!(
        mailbox_history_scope(&app),
        Some(MailboxHistoryScope::Session { .. })
    ));

    app.apply_view(WatchView::Window);
    let window = app.topology.sessions[0].windows[1].key.clone();
    select_tree_key(&mut app, &TopologyNodeKey::Window(window));
    assert!(matches!(
        mailbox_history_scope(&app),
        Some(MailboxHistoryScope::Window { .. })
    ));
}

#[test]
fn aggregate_mailbox_actions_are_read_only() {
    let mut app = collaboration_watch_app();
    app.collaboration.history_scope = Some(MailboxHistoryScope::Session {
        key: SessionKey {
            endpoint: BackendEndpoint {
                host: muxa::HostKind::Tmux,
                socket: "default".into(),
            },
            session_id: "$1".into(),
        },
        label: "webapp".into(),
    });
    for key in ['i', 'e', 'm'] {
        assert!(matches!(
            handle_collaboration_mailbox_event(KeyCode::Char(key), &mut app),
            Action::None
        ));
        assert!(app.footer_hint.is_some());
    }
}

#[test]
fn session_mailbox_leads_with_human_window_names_and_message_summaries() {
    let mut app = collaboration_watch_app();
    app.collaboration_mailbox.tab = CollaborationMailboxTab::Incoming;
    app.collaboration.history_scope = Some(MailboxHistoryScope::Session {
        key: SessionKey {
            endpoint: BackendEndpoint {
                host: muxa::HostKind::Tmux,
                socket: "default".into(),
            },
            session_id: "$1".into(),
        },
        label: "webapp".into(),
    });
    app.collaboration.incoming = vec![
        fake_watch_collaboration_request(
            "req_internal_one",
            participant_on("%1", "default", "$1", "webapp", "@1", "ENG-7345"),
            participant_on("%2", "default", "$1", "webapp", "@2", "ENG-8000"),
            RequestStatus::Completed,
        ),
        fake_watch_collaboration_request(
            "req_internal_two",
            participant_on("%2", "default", "$1", "webapp", "@2", "ENG-8000"),
            participant_on("%1", "default", "$1", "webapp", "@1", "ENG-7345"),
            RequestStatus::Blocked,
        ),
    ];

    let lines =
        collaboration_mailbox_request_lines(&app, 100, 20, watch_theme(WatchTheme::Classic));
    let painted = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert_eq!(lines.len(), 6, "one header and two rows per message");
    assert!(painted.contains("ENG-7345 · 1 message"), "{painted}");
    assert!(painted.contains("ENG-8000 · 1 message"), "{painted}");
    assert!(painted.contains("done"), "{painted}");
    assert!(painted.contains("blocked"), "{painted}");
    assert!(painted.contains("review the auth change"), "{painted}");
    assert!(!painted.contains("req_internal"), "{painted}");
    assert!(!painted.contains("@1"), "{painted}");
    assert!(!painted.contains("@2"), "{painted}");
}

#[test]
fn watch_collaboration_origin_is_a_console_that_carries_the_launch_pane() {
    let origin = watch_collaboration_origin_from(
        Some("%9".into()),
        None,
        Some("/tmp/tmux-1000/custom,42,7".into()),
    );

    assert!(origin.console);
    assert_eq!(origin.pane, "%9");
    assert_eq!(origin.socket.as_deref(), Some("custom"));
}

#[test]
fn watch_collaboration_origin_preserves_rmux_endpoint() {
    let origin = watch_collaboration_origin_from(
        Some("rmux:%9".into()),
        Some("/tmp/rmux-1000/default,42,7".into()),
        Some("/tmp/rmux-compat,42,7".into()),
    );

    assert!(origin.console);
    assert_eq!(origin.pane, "rmux:%9");
    assert_eq!(origin.socket.as_deref(), Some("/tmp/rmux-1000/default"));
}

/// A backend whose panes carry no collaboration endpoint, and a watch with
/// no caller pane at all, both still produce a console. Messaging must not
/// depend on where the popup was opened.
#[test]
fn watch_collaboration_origin_survives_an_unusable_launch_pane() {
    let zellij = watch_collaboration_origin_from(Some("zellij:3".into()), None, None);
    assert!(zellij.console);
    assert_eq!(zellij.pane, "zellij:3");
    assert!(zellij.socket.is_none());

    let paneless = watch_collaboration_origin_from(None, None, None);
    assert!(paneless.console);
    assert!(paneless.pane.is_empty());
}

#[test]
fn watch_message_uses_the_only_room_peer_without_extra_target_steps() {
    let mut app = collaboration_watch_app();
    app.table_state.select(Some(0));

    open_watch_collaboration_composer(&mut app);

    assert!(matches!(
        app.collaboration_composer
            .as_ref()
            .map(|composer| &composer.target),
        Some(CollaborationComposeTarget::Send { target, .. }) if target == "pane:%2"
    ));
}

#[test]
fn watch_m_and_uppercase_m_are_browse_actions_with_b_as_an_alias() {
    let mut app = collaboration_watch_app();

    assert!(matches!(
        key_action(&mut app, 'm'),
        Action::OpenCollaborationMessage
    ));
    assert!(matches!(
        key_action(&mut app, 'M'),
        Action::OpenCollaborationMailbox
    ));
    assert!(matches!(
        alt_key_action(&mut app, 'm'),
        Action::OpenAgentMove
    ));
    assert!(matches!(
        alt_key_action(&mut app, 'M'),
        Action::OpenAgentMove
    ));
    assert!(matches!(
        key_action(&mut app, 'b'),
        Action::OpenCollaborationMailbox
    ));
}

#[test]
fn mailbox_uppercase_m_closes_and_lowercase_m_composes() {
    let mut app = collaboration_watch_app();
    app.collaboration_mailbox.open = true;

    assert!(matches!(
        handle_collaboration_mailbox_event(KeyCode::Char('m'), &mut app),
        Action::OpenCollaborationMessage
    ));

    assert!(matches!(
        handle_collaboration_mailbox_event(KeyCode::Char('M'), &mut app),
        Action::None
    ));
    assert!(!app.collaboration_mailbox.open);
}

#[test]
fn watch_collaboration_composer_cycles_contract_and_submits() {
    let mut app = collaboration_watch_app();
    open_watch_collaboration_composer(&mut app);

    assert!(matches!(
        handle_collaboration_composer_event(KeyCode::Tab, KeyModifiers::NONE, &mut app),
        Action::CollaborationDefaultsChanged
    ));
    assert!(matches!(
        handle_collaboration_composer_event(KeyCode::Char('e'), KeyModifiers::CONTROL, &mut app),
        Action::CollaborationDefaultsChanged
    ));
    let composer = app.collaboration_composer.as_ref().unwrap();
    assert!(matches!(
        composer.target,
        CollaborationComposeTarget::Send {
            kind: RequestKind::Review,
            mode: ComposeSendMode::Execute,
            ..
        }
    ));

    app.collaboration_composer.as_mut().unwrap().insert('검');
    assert!(matches!(
        handle_collaboration_composer_event(KeyCode::Enter, KeyModifiers::NONE, &mut app),
        Action::SubmitCollaboration
    ));
}

#[test]
fn slash_palette_inserts_a_skill_then_requires_a_second_enter_to_send() {
    let mut app = collaboration_watch_app();
    app.message_skills.insert(
        "agent-review".into(),
        "create a new pane with codex and review our changes".into(),
    );
    open_watch_collaboration_composer(&mut app);

    assert!(matches!(
        handle_collaboration_composer_event(KeyCode::Char('/'), KeyModifiers::NONE, &mut app),
        Action::None
    ));
    assert!(app
        .collaboration_composer
        .as_ref()
        .is_some_and(|composer| composer.skill_palette.is_some()));
    for c in "review".chars() {
        let _ = handle_collaboration_composer_event(KeyCode::Char(c), KeyModifiers::NONE, &mut app);
    }

    assert!(matches!(
        handle_collaboration_composer_event(KeyCode::Enter, KeyModifiers::NONE, &mut app),
        Action::None
    ));
    let composer = app.collaboration_composer.as_ref().unwrap();
    assert!(composer.skill_palette.is_none());
    assert_eq!(
        composer.input,
        "create a new pane with codex and review our changes"
    );
    assert!(matches!(
        handle_collaboration_composer_event(KeyCode::Enter, KeyModifiers::NONE, &mut app),
        Action::SubmitCollaboration
    ));
}

#[test]
fn slash_palette_preserves_a_message_already_being_composed() {
    let mut app = collaboration_watch_app();
    app.message_skills
        .insert("review".into(), "review the current changes".into());
    open_watch_collaboration_composer(&mut app);
    let composer = app.collaboration_composer.as_mut().unwrap();
    composer.input = "Keep this context.".into();
    composer.cursor = composer.input.chars().count();

    assert!(matches!(
        handle_collaboration_composer_event(KeyCode::Char('/'), KeyModifiers::NONE, &mut app),
        Action::None
    ));
    assert!(app
        .collaboration_composer
        .as_ref()
        .is_some_and(|composer| composer.skill_palette.is_some()));
    assert!(matches!(
        handle_collaboration_composer_event(KeyCode::Enter, KeyModifiers::NONE, &mut app),
        Action::None
    ));

    let composer = app.collaboration_composer.as_ref().unwrap();
    assert_eq!(
        composer.input,
        "Keep this context.\n\nreview the current changes"
    );
    assert!(composer.skill_palette.is_none());
}

#[test]
fn slash_palette_backspace_returns_to_the_empty_composer() {
    let mut app = collaboration_watch_app();
    open_watch_collaboration_composer(&mut app);
    let _ = handle_collaboration_composer_event(KeyCode::Char('/'), KeyModifiers::NONE, &mut app);
    assert!(matches!(
        handle_collaboration_composer_event(KeyCode::Backspace, KeyModifiers::NONE, &mut app),
        Action::None
    ));
    assert!(app
        .collaboration_composer
        .as_ref()
        .is_some_and(|composer| composer.skill_palette.is_none()));
}

#[test]
fn slash_palette_renders_registered_skill_and_insert_hint() {
    let mut app = collaboration_watch_app();
    app.message_skills.insert(
        "agent-review".into(),
        "create a codex pane and review our changes".into(),
    );
    open_watch_collaboration_composer(&mut app);
    let _ = handle_collaboration_composer_event(KeyCode::Char('/'), KeyModifiers::NONE, &mut app);
    let backend = TestBackend::new(100, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let screen = (0..terminal.backend().buffer().area().height)
        .map(|y| row_text(terminal.backend().buffer(), y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(screen.contains("skills"));
    assert!(screen.contains("/agent-review"));
    assert!(screen.contains("Enter insert"));
    assert!(screen.contains("F2"));
    assert!(screen.contains("Del"));
}

#[test]
fn ask_slash_palette_inserts_text_without_changing_the_ask_target() {
    let mut app = app_with_paneless_and_pane();
    app.ask_agent = "codex".into();
    app.message_skills.insert(
        "agent-review".into(),
        "create a codex pane and review our changes".into(),
    );
    app.ask_composer = Some(AskComposer::default());

    assert!(matches!(
        handle_ask_composer_event(KeyCode::Char('/'), KeyModifiers::NONE, &mut app),
        Action::None
    ));
    for c in "review".chars() {
        let _ = handle_ask_composer_event(KeyCode::Char(c), KeyModifiers::NONE, &mut app);
    }
    assert!(matches!(
        handle_ask_composer_event(KeyCode::Enter, KeyModifiers::NONE, &mut app),
        Action::None
    ));
    let ask = app.ask_composer.as_ref().unwrap();
    assert_eq!(ask.input, "create a codex pane and review our changes");
    assert!(ask.skill_palette.is_none());
    assert_eq!(app.ask_agent, "codex");
    assert!(matches!(
        handle_ask_composer_event(KeyCode::Enter, KeyModifiers::NONE, &mut app),
        Action::SubmitAsk {
            new_conversation: false
        }
    ));
}

#[test]
fn ask_skill_can_be_inserted_into_an_existing_question() {
    let mut app = app_with_paneless_and_pane();
    app.message_skills
        .insert("review".into(), "review the current changes".into());
    app.ask_composer = Some(AskComposer {
        input: "Use this additional context.".into(),
        cursor: "Use this additional context.".chars().count(),
        ..AskComposer::default()
    });

    let _ = handle_ask_composer_event(KeyCode::Char('/'), KeyModifiers::NONE, &mut app);
    let _ = handle_ask_composer_event(KeyCode::Enter, KeyModifiers::NONE, &mut app);

    let ask = app.ask_composer.as_ref().unwrap();
    assert_eq!(
        ask.input,
        "Use this additional context.\n\nreview the current changes"
    );
    assert!(ask.skill_palette.is_none());
}

#[test]
fn watch_skill_editor_collects_a_name_and_prompt_before_saving() {
    let mut app = collaboration_watch_app();
    open_watch_collaboration_composer(&mut app);
    let _ = handle_collaboration_composer_event(KeyCode::Char('/'), KeyModifiers::NONE, &mut app);
    assert!(matches!(
        handle_message_skill_palette_event(KeyCode::F(2), KeyModifiers::NONE, &mut app),
        Action::None
    ));
    assert!(app.message_skill_editor.is_some());

    for c in "review".chars() {
        let _ = handle_message_skill_editor_event(KeyCode::Char(c), KeyModifiers::NONE, &mut app);
    }
    let _ = handle_message_skill_editor_event(KeyCode::Enter, KeyModifiers::NONE, &mut app);
    for c in "ask codex to review".chars() {
        let _ = handle_message_skill_editor_event(KeyCode::Char(c), KeyModifiers::NONE, &mut app);
    }
    let action = handle_message_skill_editor_event(KeyCode::Enter, KeyModifiers::NONE, &mut app);
    assert!(matches!(
        action,
        Action::SaveMessageSkill { ref name, ref prompt }
            if name == "review" && prompt == "ask codex to review"
    ));
    // Persistence decides whether the editor closes; a failed config write
    // must leave the user's text available for correction/retry.
    assert!(app.message_skill_editor.is_some());
}

#[test]
fn deleting_a_watch_skill_requires_confirmation() {
    let mut app = collaboration_watch_app();
    app.message_skills
        .insert("agent-review".into(), "review this".into());
    open_watch_collaboration_composer(&mut app);
    let _ = handle_collaboration_composer_event(KeyCode::Char('/'), KeyModifiers::NONE, &mut app);

    let action = handle_message_skill_palette_event(KeyCode::Delete, KeyModifiers::NONE, &mut app);
    assert!(matches!(
        action,
        Action::AskConfirm(ConfirmPopup {
            on_confirm: ConfirmAction::DeleteMessageSkill(ref name),
            ..
        }) if name == "agent-review"
    ));
    assert!(app.message_skills.contains_key("agent-review"));
}

#[test]
fn watch_skill_palette_keeps_the_ctrl_management_aliases() {
    let mut app = collaboration_watch_app();
    app.message_skills
        .insert("agent-review".into(), "review this".into());
    open_watch_collaboration_composer(&mut app);
    let _ = handle_collaboration_composer_event(KeyCode::Char('/'), KeyModifiers::NONE, &mut app);

    assert!(matches!(
        handle_message_skill_palette_event(KeyCode::Char('a'), KeyModifiers::CONTROL, &mut app),
        Action::None
    ));
    assert!(app.message_skill_editor.take().is_some());
    assert!(matches!(
        handle_message_skill_palette_event(KeyCode::Char('d'), KeyModifiers::CONTROL, &mut app),
        Action::AskConfirm(ConfirmPopup {
            on_confirm: ConfirmAction::DeleteMessageSkill(ref name),
            ..
        }) if name == "agent-review"
    ));
}

#[test]
fn ask_skill_palette_and_editor_render_as_overlays() {
    let mut app = app_with_paneless_and_pane();
    app.message_skills
        .insert("agent-review".into(), "review our changes".into());
    app.ask_composer = Some(AskComposer {
        skill_palette: Some(MessageSkillPalette::default()),
        ..AskComposer::default()
    });
    app.message_skill_editor = Some(MessageSkillEditor::default());
    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let screen = (0..terminal.backend().buffer().area().height)
        .map(|y| row_text(terminal.backend().buffer(), y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(screen.contains("skill add/update"));
    assert!(screen.contains("keeps kind/mode/ask permissions"));
}

#[test]
fn backspace_cancels_the_message_composer_only_when_already_empty() {
    let mut app = collaboration_watch_app();
    open_watch_collaboration_composer(&mut app);
    app.collaboration_composer.as_mut().unwrap().insert('x');

    assert!(matches!(
        handle_collaboration_composer_event(KeyCode::Backspace, KeyModifiers::NONE, &mut app),
        Action::None
    ));
    assert!(app
        .collaboration_composer
        .as_ref()
        .is_some_and(|composer| composer.input.is_empty()));

    assert!(matches!(
        handle_collaboration_composer_event(KeyCode::Backspace, KeyModifiers::NONE, &mut app),
        Action::CancelCollaborationComposer
    ));
}

#[test]
fn host_scope_lets_m_target_the_agent_under_the_cursor() {
    // No room at all — but scope=host says the cursor names the
    // recipient, so the contract composer opens against that agent.
    let mut app = app_with_paneless_and_pane();
    app.collaboration_scope = muxa::config::CollaborationScope::Host;
    app.collaboration.origin = Some(CollaborationOrigin {
        exact_session: false,
        pane: "%1".into(),
        socket: None,
        console: true,
        agent_session_id: None,
    });
    let pane_idx = app
        .rows
        .iter()
        .position(|r| matches!(r, WatchRow::Agent(a) if a.pane.as_deref() == Some("%42")))
        .unwrap();
    app.table_state.select(Some(pane_idx));

    open_watch_collaboration_composer(&mut app);

    assert!(
        matches!(
            app.collaboration_composer.as_ref().map(|c| &c.target),
            Some(CollaborationComposeTarget::Send { target, .. }) if target == "pane:%42"
        ),
        "host scope must open the contract composer for the cursor's agent"
    );
}

#[test]
fn session_and_window_message_target_the_lowest_index_live_agent() {
    let (agents, panes) = basic_topology_fixture();
    let mut app = topology_watch(WatchView::Window, agents, panes);
    app.collaboration_scope = muxa::config::CollaborationScope::Host;
    app.collaboration.origin = Some(CollaborationOrigin {
        exact_session: false,
        pane: "console".into(),
        socket: Some("default".into()),
        console: true,
        agent_session_id: None,
    });
    let session = app.topology.sessions[0].node_key();
    let window = app.topology.sessions[0].windows[0].node_key();

    for parent in [session, window] {
        select_tree_key(&mut app, &parent);
        open_watch_collaboration_composer(&mut app);
        assert!(matches!(
            app.collaboration_composer
                .as_ref()
                .map(|composer| &composer.target),
            Some(CollaborationComposeTarget::Send { target, .. })
                if target == "pane:%1"
        ));
        assert!(app
            .collaboration_composer
            .as_ref()
            .is_some_and(|composer| composer.label.contains("%1")));
        app.collaboration_composer = None;
    }
}

/// Marking is the same resolution `m` uses, so what is marked is what
/// would have been messaged — and `m` then addresses the set rather than
/// the cursor.
#[test]
fn marked_agents_become_the_composer_s_recipients() {
    let (agents, panes) = basic_topology_fixture();
    let mut app = topology_watch(WatchView::Pane, agents, panes);
    let first = fake_collaboration_participant("%1", "agent-1", None);
    let second = fake_collaboration_participant("%2", "agent-2", None);
    app.collaboration.origin = Some(CollaborationOrigin {
        exact_session: false,
        pane: "console".into(),
        socket: Some("default".into()),
        console: true,
        agent_session_id: None,
    });
    app.collaboration.room = Some(RoomContext {
        current: first.clone(),
        peers: vec![first.clone(), second.clone()],
        unread: 0,
        unread_replies: 0,
    });

    for pane in ["%1", "%2"] {
        let key = app
            .topology
            .sessions
            .iter()
            .flat_map(|session| session.windows.iter())
            .flat_map(|window| window.panes.iter())
            .find(|candidate| candidate.key.pane_id == pane)
            .map(|candidate| TopologyNodeKey::Pane(candidate.key.clone()))
            .expect("pane in the fixture");
        select_tree_key(&mut app, &key);
        let outcome = toggle_collaboration_mark(&mut app);
        assert!(matches!(outcome, ActionOutcome::Ok(_)), "{outcome:?}");
    }
    assert_eq!(app.collaboration_marks.len(), 2);

    open_watch_collaboration_composer(&mut app);
    let Some(CollaborationComposeTarget::Broadcast { recipients, .. }) = app
        .collaboration_composer
        .as_ref()
        .map(|composer| composer.target.clone())
    else {
        panic!("marks should open a broadcast composer");
    };
    let panes: Vec<_> = recipients.iter().map(|(pane, _)| pane.as_str()).collect();
    assert_eq!(panes, ["%1", "%2"]);
}

/// Pressing `Space` twice on one agent leaves nothing marked, and `m`
/// goes back to addressing the cursor.
#[test]
fn unmarking_the_last_agent_restores_the_single_recipient_composer() {
    let (agents, panes) = basic_topology_fixture();
    let mut app = topology_watch(WatchView::Pane, agents, panes);
    let peer = fake_collaboration_participant("%1", "agent-1", None);
    app.collaboration.origin = Some(CollaborationOrigin {
        exact_session: false,
        pane: "console".into(),
        socket: Some("default".into()),
        console: true,
        agent_session_id: None,
    });
    app.collaboration.room = Some(RoomContext {
        current: peer.clone(),
        peers: vec![peer.clone()],
        unread: 0,
        unread_replies: 0,
    });

    toggle_collaboration_mark(&mut app);
    assert_eq!(app.collaboration_marks.len(), 1);
    toggle_collaboration_mark(&mut app);
    assert!(app.collaboration_marks.is_empty());

    open_watch_collaboration_composer(&mut app);
    assert!(matches!(
        app.collaboration_composer
            .as_ref()
            .map(|composer| &composer.target),
        Some(CollaborationComposeTarget::Send { .. })
    ));
}

/// A marked agent that exited is dropped rather than addressed, and the
/// composer says how many marks it could not account for — a fan-out that
/// silently shrinks is the failure this feature exists to avoid.
#[test]
fn a_mark_whose_agent_is_gone_is_dropped_and_counted() {
    let (agents, panes) = basic_topology_fixture();
    let mut app = topology_watch(WatchView::Pane, agents, panes);
    let peer = fake_collaboration_participant("%1", "agent-1", None);
    app.collaboration.origin = Some(CollaborationOrigin {
        exact_session: false,
        pane: "console".into(),
        socket: Some("default".into()),
        console: true,
        agent_session_id: None,
    });
    app.collaboration.room = Some(RoomContext {
        current: peer.clone(),
        peers: vec![peer.clone()],
        unread: 0,
        unread_replies: 0,
    });
    app.collaboration_marks.insert(CollaborationMark {
        pane: "%1".into(),
        agent_session_id: Some("agent-1".into()),
    });
    // Marked while it was there; the room no longer lists it.
    app.collaboration_marks.insert(CollaborationMark {
        pane: "%99".into(),
        agent_session_id: Some("agent-99".into()),
    });

    assert_eq!(resolved_marks(&app).len(), 1);
    assert_eq!(stale_mark_count(&app), 1);

    open_watch_collaboration_composer(&mut app);
    let label = app
        .collaboration_composer
        .as_ref()
        .map(|composer| composer.label.clone())
        .expect("composer");
    assert!(label.contains("1 gone"), "{label}");
}

/// A pane whose agent session was replaced is a different agent. The mark
/// names the session, so it does not follow the pane to the newcomer.
#[test]
fn a_mark_does_not_survive_the_agent_it_named() {
    let (agents, panes) = basic_topology_fixture();
    let mut app = topology_watch(WatchView::Pane, agents, panes);
    let replacement = fake_collaboration_participant("%1", "agent-2", None);
    app.collaboration.origin = Some(CollaborationOrigin {
        exact_session: false,
        pane: "console".into(),
        socket: Some("default".into()),
        console: true,
        agent_session_id: None,
    });
    app.collaboration.room = Some(RoomContext {
        current: replacement.clone(),
        peers: vec![replacement],
        unread: 0,
        unread_replies: 0,
    });
    app.collaboration_marks.insert(CollaborationMark {
        pane: "%1".into(),
        agent_session_id: Some("agent-1".into()),
    });

    assert!(resolved_marks(&app).is_empty());
    assert_eq!(stale_mark_count(&app), 1);
}

/// A set cannot be addressed by keystrokes, so a composer opened against
/// marks never starts in the one mode `Enter` would refuse. Without this
/// a persisted just-send default left the operator with a hint pointing
/// at Ctrl-E and no way to send.
#[test]
fn a_marked_composer_never_opens_in_a_mode_it_cannot_send_from() {
    let (agents, panes) = basic_topology_fixture();
    let mut app = topology_watch(WatchView::Pane, agents, panes);
    let peer = fake_collaboration_participant("%1", "agent-1", None);
    app.collaboration.origin = Some(CollaborationOrigin {
        exact_session: false,
        pane: "console".into(),
        socket: Some("default".into()),
        console: true,
        agent_session_id: None,
    });
    app.collaboration.room = Some(RoomContext {
        current: peer.clone(),
        peers: vec![peer.clone()],
        unread: 0,
        unread_replies: 0,
    });
    app.collaboration_compose_defaults = CollaborationComposeDefaults {
        kind: RequestKind::Question,
        mode: ComposeSendMode::JustSend,
    };
    app.collaboration_marks.insert(CollaborationMark {
        pane: "%1".into(),
        agent_session_id: Some("agent-1".into()),
    });

    open_watch_collaboration_composer(&mut app);
    let Some(CollaborationComposeTarget::Broadcast { mode, .. }) = app
        .collaboration_composer
        .as_ref()
        .map(|composer| composer.target.clone())
    else {
        panic!("marks should open a broadcast composer");
    };
    assert_eq!(mode, ComposeSendMode::ReadOnly);
    // The footer offers the cycle below, and only the two modes in it.
    let mut terminal = Terminal::new(TestBackend::new(160, 30)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let screen = (0..terminal.backend().buffer().area().height)
        .map(|y| row_text(terminal.backend().buffer(), y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(screen.contains("Ctrl-E read-only/execute"), "{screen}");
    assert!(!screen.contains("just-send"), "{screen}");

    // And Ctrl-E cycles between the two modes a set can be sent in,
    // rather than doing nothing.
    assert!(composer_cycle_mode(&mut app));
    let Some(CollaborationComposeTarget::Broadcast { mode, .. }) = app
        .collaboration_composer
        .as_ref()
        .map(|composer| composer.target.clone())
    else {
        panic!("still a broadcast composer");
    };
    assert_eq!(mode, ComposeSendMode::Execute);
}

/// Every recipient keeps its own row. A partial failure that renders as
/// "sent" is the shape of bug this repo spent the week removing.
#[test]
fn a_broadcast_report_keeps_every_recipient_outcome() {
    let report = BroadcastReport {
        rows: vec![
            BroadcastRow {
                pane: "%1".into(),
                label: "codex@%1".into(),
                outcome: Ok("req_ab".into()),
            },
            BroadcastRow {
                pane: "%2".into(),
                label: "claude@%2".into(),
                outcome: Err("target \"pane:%2\" is not a peer in this tmux window".into()),
            },
        ],
    };
    assert_eq!(report.delivered(), 1);
    assert_eq!(report.failed(), 1);
    assert!(report.title().contains("1 delivered"));
    assert!(report.title().contains("1 failed"));
}

#[test]
fn window_scope_parent_message_uses_the_same_stable_primary_peer() {
    let (agents, panes) = basic_topology_fixture();
    let mut app = topology_watch(WatchView::Window, agents, panes);
    let first = fake_collaboration_participant("%1", "agent-1", None);
    let second = fake_collaboration_participant("%2", "agent-2", None);
    app.collaboration.origin = Some(CollaborationOrigin {
        exact_session: false,
        pane: "console".into(),
        socket: Some("default".into()),
        console: true,
        agent_session_id: None,
    });
    app.collaboration.room = Some(RoomContext {
        current: first.clone(),
        // Reverse mailbox order to prove topology order, not peer order,
        // defines the automatic target.
        peers: vec![second, first],
        unread: 0,
        unread_replies: 0,
    });
    let window = app.topology.sessions[0].windows[0].node_key();
    select_tree_key(&mut app, &window);

    open_watch_collaboration_composer(&mut app);

    assert!(matches!(
        app.collaboration_composer
            .as_ref()
            .map(|composer| &composer.target),
        Some(CollaborationComposeTarget::Send { target, .. }) if target == "pane:%1"
    ));
}

/// The reported bug: `m` worked on every row except the one the popup was
/// opened from, because that pane's agent was the sender. The console is
/// the sender now, so the launch row is an ordinary recipient — and it
/// gets the full contract composer, not the keystroke fallback.
#[test]
fn m_targets_the_pane_watch_was_opened_from() {
    let mut app = app_with_paneless_and_pane();
    app.collaboration_scope = muxa::config::CollaborationScope::Host;
    app.collaboration.origin = Some(CollaborationOrigin {
        exact_session: false,
        pane: "%42".into(),
        socket: None,
        console: true,
        agent_session_id: None,
    });
    let pane_idx = app
        .rows
        .iter()
        .position(|r| matches!(r, WatchRow::Agent(a) if a.pane.as_deref() == Some("%42")))
        .unwrap();
    app.table_state.select(Some(pane_idx));

    open_watch_collaboration_composer(&mut app);

    assert!(
        matches!(
            app.collaboration_composer.as_ref().map(|c| &c.target),
            Some(CollaborationComposeTarget::Send { target, .. }) if target == "pane:%42"
        ),
        "the launch pane must be addressable like any other row"
    );
}

/// Now that the console does not occupy the launch pane, that pane's agent
/// is an ordinary room peer — and a single-agent launch window is the
/// common shape. Under host scope the cursor is the selector, so a row
/// naming no agent must degrade to keystrokes rather than quietly
/// redirecting the message to the launch window.
#[test]
fn host_scope_never_redirects_an_agentless_row_to_the_lone_room_peer() {
    let mut app = collaboration_watch_app();
    app.collaboration_scope = muxa::config::CollaborationScope::Host;
    app.set_data(
        vec![fake_agent(
            "peer",
            Some("%2"),
            AgentKind::Codex,
            AgentState::Idle,
            None,
            None,
            None,
            None,
        )],
        vec![
            fake_pane("%2", "main", 0, 1, "codex"),
            fake_pane("%7", "other", 0, 0, "zsh"),
        ],
    );
    let bare_idx = app
        .rows
        .iter()
        .position(|row| matches!(row, WatchRow::BarePane(pane) if pane.pane_id == "%7"))
        .expect("the shell pane must show as a bare row");
    app.table_state.select(Some(bare_idx));

    open_watch_collaboration_composer(&mut app);

    assert!(
        matches!(
            app.collaboration_composer.as_ref().map(|c| &c.target),
            Some(
                CollaborationComposeTarget::Prompt { .. }
                    | CollaborationComposeTarget::PromptTopology { .. }
                    | CollaborationComposeTarget::PromptPanes { .. }
            )
        ),
        "a row with no agent must not resolve to the launch window's agent"
    );
    // And the reason names the row, not the room: under host scope the
    // launch window is not why this failed, and pointing at its peers
    // would send the operator to an unrelated window.
    let hint = app
        .footer_hint
        .as_ref()
        .expect("a reason was set")
        .message
        .clone();
    assert!(
        hint.contains("this row has no live tracked agent"),
        "host-scope hint must be about the row, got {hint:?}"
    );
}

#[test]
fn host_scope_selected_work_beats_the_only_launch_window_peer() {
    let mut app = collaboration_watch_app();
    app.collaboration_scope = muxa::config::CollaborationScope::Host;
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
            fake_agent(
                "eng-7041",
                Some("%913"),
                AgentKind::ClaudeCode,
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
            fake_pane("%913", "eng-7041", 0, 0, "claude"),
        ],
    );
    app.apply_view(WatchView::Window);
    let row_index = app
        .rows
        .iter()
        .position(|row| matches!(row, WatchRow::Work(session) if session.session == "eng-7041"))
        .unwrap();
    let visible_index = app
        .visible_targets()
        .iter()
        .position(|target| target.row_idx == row_index && target.agent_idx.is_none())
        .unwrap();
    app.table_state.select(Some(visible_index));

    open_watch_collaboration_composer(&mut app);

    assert!(matches!(
        app.collaboration_composer.as_ref().map(|composer| &composer.target),
        Some(CollaborationComposeTarget::Send { target, .. }) if target == "pane:%913"
    ));
}

#[test]
fn m_without_a_room_still_opens_a_keystrokes_composer() {
    // Collaboration being unavailable must not take `m` away — the
    // user is pointing at a pane and typing at it needs no contract.
    let mut app = app_with_paneless_and_pane();
    let pane_idx = app
        .rows
        .iter()
        .position(|r| matches!(r, WatchRow::Agent(a) if a.pane.as_deref() == Some("%42")))
        .expect("fixture has a pane-bearing row");
    app.table_state.select(Some(pane_idx));

    open_watch_collaboration_composer(&mut app);

    assert!(matches!(
        app.collaboration_composer.as_ref().map(|c| &c.target),
        Some(CollaborationComposeTarget::Prompt { pane }) if pane == "%42"
    ));

    app.collaboration_composer.as_mut().unwrap().insert('하');
    let action = handle_collaboration_composer_event(KeyCode::Enter, KeyModifiers::NONE, &mut app);
    assert!(
        matches!(
            action,
            Action::Quick(QuickAction::SendPrompt { ref pane_id, ref text })
                if pane_id == "%42" && text == "하"
        ),
        "got {action:?}"
    );
}

#[test]
fn the_prompt_only_composer_refuses_to_invent_a_contract() {
    let mut app = app_with_paneless_and_pane();
    let pane_idx = app
        .rows
        .iter()
        .position(|r| matches!(r, WatchRow::Agent(a) if a.pane.as_deref() == Some("%42")))
        .unwrap();
    app.table_state.select(Some(pane_idx));
    open_watch_collaboration_composer(&mut app);

    for key in [KeyCode::Tab, KeyCode::Char('e')] {
        let modifiers = if key == KeyCode::Char('e') {
            KeyModifiers::CONTROL
        } else {
            KeyModifiers::NONE
        };
        assert!(matches!(
            handle_collaboration_composer_event(key, modifiers, &mut app),
            Action::None
        ));
        assert!(
            matches!(
                app.collaboration_composer.as_ref().map(|c| &c.target),
                Some(CollaborationComposeTarget::Prompt { .. })
            ),
            "{key:?} must not conjure a request out of a peerless composer"
        );
    }
}

#[test]
fn ctrl_e_cycles_through_just_send_and_back() {
    let mut app = collaboration_watch_app();
    open_watch_collaboration_composer(&mut app);
    let mode_of = |app: &App| match app.collaboration_composer.as_ref().unwrap().target {
        CollaborationComposeTarget::Send { mode, .. } => mode,
        _ => unreachable!(),
    };
    assert_eq!(mode_of(&app), ComposeSendMode::ReadOnly);
    for expected in [
        ComposeSendMode::Execute,
        ComposeSendMode::JustSend,
        ComposeSendMode::ReadOnly,
    ] {
        handle_collaboration_composer_event(KeyCode::Char('e'), KeyModifiers::CONTROL, &mut app);
        assert_eq!(mode_of(&app), expected);
    }
}

#[test]
fn request_kind_and_mode_survive_closing_and_reopening_the_composer() {
    let mut app = collaboration_watch_app();
    open_watch_collaboration_composer(&mut app);

    for _ in 0..2 {
        assert!(matches!(
            handle_collaboration_composer_event(KeyCode::Tab, KeyModifiers::NONE, &mut app),
            Action::CollaborationDefaultsChanged
        ));
    }
    assert!(matches!(
        handle_collaboration_composer_event(KeyCode::Char('e'), KeyModifiers::CONTROL, &mut app),
        Action::CollaborationDefaultsChanged
    ));
    app.collaboration_composer = None;

    open_watch_collaboration_composer(&mut app);

    assert!(matches!(
        app.collaboration_composer
            .as_ref()
            .map(|composer| &composer.target),
        Some(CollaborationComposeTarget::Send {
            kind: RequestKind::Task,
            mode: ComposeSendMode::Execute,
            ..
        })
    ));
}

#[test]
fn enter_in_just_send_mode_becomes_keystrokes_not_a_request() {
    let mut app = collaboration_watch_app();
    open_watch_collaboration_composer(&mut app);
    // ReadOnly -> Execute -> JustSend
    for _ in 0..2 {
        handle_collaboration_composer_event(KeyCode::Char('e'), KeyModifiers::CONTROL, &mut app);
    }
    app.collaboration_composer.as_mut().unwrap().insert('안');
    let action = handle_collaboration_composer_event(KeyCode::Enter, KeyModifiers::NONE, &mut app);
    assert!(
        matches!(
            action,
            Action::Quick(QuickAction::SendPrompt { ref pane_id, ref text })
                if pane_id == "%2" && text == "안"
        ),
        "got {action:?}"
    );
    // The composer is consumed — nothing left to double-submit.
    assert!(app.collaboration_composer.is_none());
}

#[test]
fn m_resolves_a_peer_from_the_work_row_that_holds_it() {
    // Pointing at the session the peer lives in is enough; the user
    // should not have to expand it to the exact pane first.
    let mut app = collaboration_watch_app();
    app.apply_view(WatchView::Window);
    app.table_state.select(Some(0));

    open_watch_collaboration_composer(&mut app);

    assert!(
        matches!(
            app.collaboration_composer
                .as_ref()
                .map(|composer| &composer.target),
            Some(CollaborationComposeTarget::Send { target, .. }) if target == "pane:%2"
        ),
        "expected the peer inside the selected work row"
    );
}

#[test]
fn the_empty_room_hint_names_the_window_and_denies_the_cursor() {
    // The table spans every session on the host, so an unqualified
    // "here" reads as "the row I am on" and sends the user moving a
    // cursor that cannot affect the room.
    let hint = empty_room_hint(&fake_collaboration_participant("%1", "s1", None));
    assert!(hint.contains("main:agents"), "{hint}");
    assert!(hint.contains("not the selected row"), "{hint}");
}

#[test]
fn the_empty_room_hint_falls_back_to_the_window_id() {
    let mut current = fake_collaboration_participant("%1", "s1", None);
    current.tmux_session_name = None;
    current.window_name = None;
    let hint = empty_room_hint(&current);
    assert!(hint.contains("@1"), "{hint}");
}

#[test]
fn the_peer_hint_names_the_rows_that_can_receive() {
    // The table lists every agent on the host; without names the user
    // has no way to tell which handful of rows qualify.
    let hint = peer_choice_hint(&[
        "reviewer@%747 · webapp-set:0.1".to_string(),
        "codex@%751 · webapp-set:0.2".to_string(),
    ]);
    assert!(hint.contains("reviewer@%747 · webapp-set:0.1"), "{hint}");
    assert!(hint.contains("codex@%751 · webapp-set:0.2"), "{hint}");
}

#[test]
fn the_peer_hint_stays_on_one_line_when_the_room_is_crowded() {
    let peers: Vec<String> = (1..=6).map(|n| format!("agent@%{n}")).collect();
    let hint = peer_choice_hint(&peers);
    assert!(hint.contains("+3 more"), "{hint}");
    assert!(
        !hint.contains("%6"),
        "trimmed peers must not be listed: {hint}"
    );
}

#[test]
fn shift_tab_leaves_kind_cycling_to_tab_alone() {
    // Tab and Shift-Tab used to share an arm. Splitting them must not
    // cost Tab its job — the two are different axes, not one list.
    let mut app = collaboration_watch_app();
    open_watch_collaboration_composer(&mut app);

    assert!(matches!(
        handle_collaboration_composer_event(KeyCode::Tab, KeyModifiers::NONE, &mut app),
        Action::CollaborationDefaultsChanged
    ));
    assert!(matches!(
        app.collaboration_composer.as_ref().unwrap().target,
        CollaborationComposeTarget::Send {
            kind: RequestKind::Review,
            ..
        }
    ));
}

#[test]
fn watch_collaboration_composer_badges_make_contract_changes_visible() {
    for (kind, icon, label, background) in [
        (RequestKind::Question, "?", "QUESTION", Color::Cyan),
        (RequestKind::Review, "◆", "REVIEW", Color::Magenta),
        (RequestKind::Task, "▶", "TASK", Color::Yellow),
        (RequestKind::Notice, "!", "NOTICE", Color::Blue),
    ] {
        let badge = request_kind_badge(kind);
        assert_eq!(
            (badge.icon, badge.label, badge.background),
            (icon, label, background)
        );
    }
    assert_eq!(work_mode_badge(WorkMode::ReadOnly).icon, "○");
    assert_eq!(work_mode_badge(WorkMode::ReadOnly).background, Color::Green);
    assert_eq!(work_mode_badge(WorkMode::Execute).icon, "●");
    assert_eq!(work_mode_badge(WorkMode::Execute).background, Color::Red);

    let mut app = collaboration_watch_app();
    open_watch_collaboration_composer(&mut app);
    let theme = watch_theme(WatchTheme::Classic);
    let composer = app.collaboration_composer.as_ref().unwrap();
    let (title, border) = collaboration_composer_title(composer, theme);
    let text = title
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert!(text.contains("? QUESTION"));
    assert!(text.contains("○ READ-ONLY"));
    assert_eq!(border, Color::Cyan);

    handle_collaboration_composer_event(KeyCode::Tab, KeyModifiers::NONE, &mut app);
    handle_collaboration_composer_event(KeyCode::Char('e'), KeyModifiers::CONTROL, &mut app);
    let composer = app.collaboration_composer.as_ref().unwrap();
    let (title, border) = collaboration_composer_title(composer, theme);
    let text = title
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert!(text.contains("◆ REVIEW"));
    assert!(text.contains("● EXECUTE"));
    assert_eq!(border, Color::Red);
    assert_eq!(
        title
            .spans
            .iter()
            .find(|span| span.content.contains("REVIEW"))
            .and_then(|span| span.style.bg),
        Some(Color::Magenta)
    );
}

#[test]
fn human_mailbox_defaults_to_operator_and_replies_without_agent_claim() {
    let mut app = collaboration_watch_app();
    assert_eq!(
        app.collaboration_mailbox.tab,
        CollaborationMailboxTab::Human
    );
    let room = app.collaboration.room.as_ref().unwrap();
    let mut request = fake_watch_collaboration_request(
        "human_question",
        room.peers[0].clone(),
        Participant::console(room.current.room.clone()),
        RequestStatus::Queued,
    );
    request.expects_reply = true;
    app.collaboration.human.push(request);
    open_watch_collaboration_reply_composer(&mut app);
    assert!(
        matches!(app.collaboration_composer.as_ref().map(|c| &c.target), Some(CollaborationComposeTarget::Reply { origin, request_id, .. }) if origin.console && request_id == "human_question")
    );
    toggle_collaboration_mailbox(&mut app);
    assert_eq!(
        app.collaboration_mailbox.tab,
        CollaborationMailboxTab::Incoming
    );
}

#[test]
fn watch_mailbox_opens_reply_for_claimed_incoming_request() {
    let mut app = collaboration_watch_app();
    app.collaboration_mailbox.tab = CollaborationMailboxTab::Incoming;
    let room = app.collaboration.room.as_ref().unwrap();
    app.collaboration
        .incoming
        .push(fake_watch_collaboration_request(
            "req_watch_123456",
            room.peers[0].clone(),
            room.current.clone(),
            RequestStatus::Claimed,
        ));
    // The panel shows %1's inbox, so `e` must reply *as* %1. The origin is
    // the console and can never be a recipient, so sourcing the reply from
    // it would be rejected as a non-participant.
    app.collaboration.inbox = Some(MailboxAnchor {
        origin: CollaborationOrigin {
            exact_session: false,
            pane: "%1".into(),
            socket: None,
            console: false,
            agent_session_id: None,
        },
        label: "codex@%1".into(),
    });
    app.collaboration_mailbox.open = true;

    assert!(matches!(
        handle_collaboration_mailbox_event(KeyCode::Char('e'), &mut app),
        Action::None
    ));
    assert!(matches!(
        app.collaboration_composer
            .as_ref()
            .map(|composer| &composer.target),
        Some(CollaborationComposeTarget::Reply { request_id, origin, .. })
            if request_id == "req_watch_123456" && origin.pane == "%1" && !origin.console
    ));
}

#[test]
fn watch_mailbox_render_contains_request_and_peer() {
    let mut app = collaboration_watch_app();
    app.collaboration_mailbox.tab = CollaborationMailboxTab::Incoming;
    let room = app.collaboration.room.as_ref().unwrap();
    let mut request = fake_watch_collaboration_request(
        "req_watch_render_123456",
        room.peers[0].clone(),
        room.current.clone(),
        RequestStatus::Claimed,
    );
    request.air_artifacts.push(AirArtifactReference {
        artifact_id: format!("urn:air:sha256:{}", "b".repeat(64)),
        profile: AirArtifactProfile::PlanNativeCli,
        label: Some("ENG-6924 execution plan".into()),
        locator: None,
    });
    app.collaboration.incoming.push(request);
    app.collaboration_mailbox.open = true;
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal.draw(|f| render(f, &mut app)).unwrap();
    let dump = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();

    assert!(dump.contains("messages"));
    assert!(dump.contains("reviewer@%2"));
    assert!(dump.contains("in progress"));
    assert!(dump.contains("AIR PLAN"));
    assert!(dump.contains("bbbbbbbbbbbb"));
    assert!(dump.contains("review the auth change"));
    assert!(
        !dump.contains("req_watch_render_123456"),
        "compact mailbox should prioritize the conversation over internal ids"
    );
}
