use super::*;

#[test]
fn alt_digits_pick_the_screen() {
    // Plain letters are the filter, so the axis lives on a modifier.
    let mut app = App::with_legacy_config(WatchConfig::default());
    assert!(matches!(
        alt_key_action(&mut app, '2'),
        Action::SetScreen(WatchScreen::Collab)
    ));
    assert!(matches!(
        alt_key_action(&mut app, '1'),
        Action::SetScreen(WatchScreen::Topology)
    ));
}

#[test]
fn board_prompts_every_live_agent_of_the_work_and_nothing_else() {
    let (mut app, keys) = board_with_agents();
    assert!(matches!(
        press_key(&mut app, KeyCode::Char('P')),
        Action::None
    ));
    match app.collaboration_composer.as_ref().map(|c| &c.target) {
        Some(CollaborationComposeTarget::PromptPanes { panes, unavailable }) => {
            assert!(unavailable.is_empty());
            assert_eq!(panes, &keys[..2], "the stopped agent is not prompted");
        }
        other => panic!("expected a Work-wide prompt composer, got {other:?}"),
    }
    for c in "go".chars() {
        let _ = press_key(&mut app, KeyCode::Char(c));
    }
    match press_key(&mut app, KeyCode::Enter) {
        Action::Quick(QuickAction::SendPromptToMany { panes, text, .. }) => {
            assert_eq!((panes.as_slice(), text.as_str()), (&keys[..2], "go"));
        }
        _ => panic!("submitting should send to every agent"),
    }
}

#[test]
fn board_agent_cursor_picks_who_p_and_r_address() {
    let (mut app, keys) = board_with_agents();
    let _ = press_key(&mut app, KeyCode::Tab);
    assert_eq!(app.board.selected_agent_index(""), 1);
    let _ = press_key(&mut app, KeyCode::Tab);
    assert_eq!(
        app.board.selected_agent_index(""),
        0,
        "wraps over live agents only"
    );
    let _ = press_key(&mut app, KeyCode::BackTab);
    match press_key(&mut app, KeyCode::Char('R')) {
        Action::AskConfirm(ConfirmPopup {
            on_confirm: ConfirmAction::Quick(QuickAction::InterruptPane(key)),
            ..
        }) => assert_eq!(key, keys[1]),
        _ => panic!("R should confirm interrupting the agent under the cursor"),
    }
    match press_key(&mut app, KeyCode::Char('A')) {
        Action::AskConfirm(ConfirmPopup {
            on_confirm: ConfirmAction::Quick(QuickAction::InterruptPanes { panes, .. }),
            message,
        }) => {
            assert_eq!(panes, keys[..2]);
            assert!(message.contains("all 2 agents"), "{message}");
        }
        _ => panic!("A should confirm interrupting every live agent"),
    }
    let _ = press_key(&mut app, KeyCode::Char('p'));
    assert!(matches!(
        app.collaboration_composer.as_ref().map(|c| &c.target),
        Some(CollaborationComposeTarget::PromptTopology { pane }) if *pane == keys[1]
    ));
}

#[test]
fn board_authority_ends_with_its_operation() {
    let (mut app, keys) = board_with_tracked_topology_agent();
    let topology_interrupt = || {
        Action::AskConfirm(ConfirmPopup {
            message: "Interrupt %1?".into(),
            on_confirm: ConfirmAction::Quick(QuickAction::InterruptPane(keys[0].clone())),
        })
    };
    // Nothing is authorised before a board operation opens.
    assert!(matches!(
        gate_board_action(&mut app, topology_interrupt()),
        Action::None
    ));
    // Open the board's own interrupt, then cancel it.
    assert!(matches!(
        press_key(&mut app, KeyCode::Char('R')),
        Action::AskConfirm(_)
    ));
    let _ = gate_board_action(&mut app, Action::ConfirmCancel);
    assert!(
        matches!(
            gate_board_action(&mut app, topology_interrupt()),
            Action::None
        ),
        "a cancelled board interrupt must not authorise a later one"
    );
    // A composer submitted once uses its authority up.
    let _ = press_key(&mut app, KeyCode::Char('p'));
    let _ = press_key(&mut app, KeyCode::Char('x'));
    assert!(matches!(
        press_key(&mut app, KeyCode::Enter),
        Action::Quick(QuickAction::SendPromptTo { .. })
    ));
    let again = Action::Quick(QuickAction::SendPromptTo {
        pane: keys[0].clone(),
        text: "x".into(),
    });
    assert!(matches!(gate_board_action(&mut app, again), Action::None));
    assert!(matches!(
        gate_board_action(&mut app, topology_interrupt()),
        Action::None
    ));
}

#[test]
fn board_m_messages_exactly_the_cursor_agent_and_ignores_topology_marks() {
    let (mut app, keys) = board_ready_to_message();
    let marked_peer = collab_participant("%3", "callabo", "CAL-1");
    app.collaboration.room = Some(RoomContext {
        current: marked_peer.clone(),
        peers: vec![marked_peer],
        unread: 0,
        unread_replies: 0,
    });
    app.collaboration_marks.insert(CollaborationMark {
        pane: "%3".into(),
        agent_session_id: None,
    });
    assert_eq!(resolved_marks(&app).len(), 1, "the topology mark is live");
    assert!(matches!(
        press_key(&mut app, KeyCode::Char('m')),
        Action::OpenCollaborationMessage
    ));
    open_watch_collaboration_composer(&mut app);
    match app.collaboration_composer.as_ref().map(|c| &c.target) {
        Some(CollaborationComposeTarget::Send {
            target, pane_key, ..
        }) => {
            assert_eq!(
                target, "session:%1",
                "the exact agent session, not the pane"
            );
            assert_eq!(pane_key.as_ref(), Some(&keys[0]));
        }
        other => panic!("expected a message to %1, got {other:?}"),
    }
    assert!(board_message_stale(&app).is_none());
}

#[test]
fn board_m_is_not_sent_to_a_replacement_agent_in_the_same_pane() {
    let (mut app, _) = board_ready_to_message();
    let _ = press_key(&mut app, KeyCode::Char('m'));
    open_watch_collaboration_composer(&mut app);
    // A new agent session starts in %1 while the message is being written.
    let panes: Vec<PaneInfo> = ["%1", "%2", "%3"]
        .iter()
        .enumerate()
        .map(|(index, pane)| {
            fake_topology_pane(
                "/tmp/a",
                "$1",
                "callabo",
                "@4",
                "CAL-1",
                1,
                pane,
                u32::try_from(index).unwrap(),
            )
        })
        .collect();
    app.set_data(
        vec![topology_agent(
            "other",
            "%1",
            "a",
            AgentState::Working,
            "",
            0,
        )],
        panes,
    );
    assert!(board_message_stale(&app).is_some_and(|reason| reason.contains("no longer running")));
    // Cancelling ends the board's authority over that recipient.
    let _ = gate_board_action(&mut app, Action::CancelCollaborationComposer);
    assert!(app.board_pending.is_none());
}

#[test]
fn a_board_message_the_daemon_refuses_keeps_its_draft_and_recipient() {
    let (mut app, _) = board_ready_to_message();
    let _ = press_key(&mut app, KeyCode::Char('m'));
    open_watch_collaboration_composer(&mut app);
    for c in "hello".chars() {
        let _ = press_key(&mut app, KeyCode::Char(c));
    }
    // The cached topology still shows the session, so watch lets it go…
    assert!(board_message_stale(&app).is_none());
    let pending = app.board_pending.clone();
    let draft = app.collaboration_composer.take();
    // …and the daemon refuses it: the session is gone.
    let outcome = finish_board_message(
        &mut app,
        draft,
        ActionOutcome::Err("collaboration send failed: unknown target session:%1".into()),
    );
    assert!(matches!(outcome, ActionOutcome::Err(ref m) if m.contains("draft kept")));
    match app.collaboration_composer.as_ref() {
        Some(CollaborationComposer {
            target: CollaborationComposeTarget::Send { target, .. },
            input,
            ..
        }) => assert_eq!((target.as_str(), input.as_str()), ("session:%1", "hello")),
        other => panic!("the draft must come back, got {other:?}"),
    }
    assert_eq!(
        app.board_pending, pending,
        "and its frozen recipient with it"
    );
    // Delivered ends both.
    let draft = app.collaboration_composer.take();
    let _ = finish_board_message(&mut app, draft, ActionOutcome::Ok("sent".into()));
    assert!(app.board_pending.is_none() && app.collaboration_composer.is_none());
}

#[test]
fn a_board_message_follows_its_session_to_another_pane() {
    let (mut app, _) = board_ready_to_message();
    let _ = press_key(&mut app, KeyCode::Char('m'));
    let panes: Vec<PaneInfo> = ["%1", "%2", "%3"]
        .iter()
        .enumerate()
        .map(|(index, pane)| {
            fake_topology_pane(
                "/tmp/a",
                "$1",
                "callabo",
                "@4",
                "CAL-1",
                1,
                pane,
                u32::try_from(index).unwrap(),
            )
        })
        .collect();
    // Session %1 was moved to pane %3.
    app.set_data(
        vec![topology_agent("%1", "%3", "a", AgentState::Working, "", 0)],
        panes,
    );
    assert!(board_message_stale(&app).is_none());
}

#[test]
fn board_m_in_window_scope_refuses_an_agent_outside_the_room() {
    let (mut app, _) = board_ready_to_message();
    app.collaboration_scope = muxa::config::CollaborationScope::Window;
    let _ = press_key(&mut app, KeyCode::Char('m'));
    open_watch_collaboration_composer(&mut app);
    assert!(
        app.collaboration_composer.is_none(),
        "no keystrokes fallback either"
    );
    assert!(app.board_pending.is_none());
    assert!(app
        .footer_hint
        .as_ref()
        .is_some_and(|hint| hint.message.contains("not in this room")));
}

#[test]
fn board_mailbox_and_claim_act_as_the_cursor_agent() {
    let (mut app, _) = board_ready_to_message();
    let _ = press_key(&mut app, KeyCode::Tab);
    match mailbox_history_scope(&app) {
        Some(MailboxHistoryScope::Pane(anchor)) => {
            assert_eq!(anchor.origin.pane, "%2");
            assert!(!anchor.origin.console);
            assert_eq!(anchor.origin.agent_session_id.as_deref(), Some("%2"));
        }
        other => panic!("the board mailbox is the cursor agent's: {other:?}"),
    }
    assert!(matches!(
        press_key(&mut app, KeyCode::Char('b')),
        Action::OpenCollaborationMailbox
    ));
    match press_key(&mut app, KeyCode::Char('i')) {
        Action::ClaimBoardInbox { origin, .. } => {
            assert_eq!(origin.pane, "%2");
            assert!(!origin.console, "claiming is the recipient's move");
            assert!(origin.exact_session, "never a successor in that pane");
        }
        _ => panic!("i should claim for the cursor agent"),
    }
}

#[test]
fn board_collaboration_is_refused_by_a_daemon_that_cannot_pin_a_session() {
    let (mut app, _) = board_ready_to_message();
    app.exact_session_supported = false;
    for c in ['m', 'b', 'i'] {
        assert!(
            matches!(press_key(&mut app, KeyCode::Char(c)), Action::None),
            "{c}"
        );
    }
    assert!(app.board_pending.is_none());
    assert!(app
        .footer_hint
        .as_ref()
        .is_some_and(|hint| hint.message.contains("exact agent session")));
}

#[test]
fn a_board_message_footer_does_not_offer_just_send() {
    let (mut app, _) = board_ready_to_message();
    let _ = press_key(&mut app, KeyCode::Char('m'));
    open_watch_collaboration_composer(&mut app);
    let mut terminal = Terminal::new(TestBackend::new(160, 20)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let screen = (0..terminal.backend().buffer().area().height)
        .map(|y| row_text(terminal.backend().buffer(), y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(screen.contains("read-only/execute"), "{screen}");
    assert!(!screen.contains("just-send"), "{screen}");
}

#[test]
fn a_board_message_is_always_a_request_and_keeps_its_draft() {
    let (mut app, _) = board_ready_to_message();
    app.collaboration_compose_defaults.mode = ComposeSendMode::JustSend;
    let _ = press_key(&mut app, KeyCode::Char('m'));
    open_watch_collaboration_composer(&mut app);
    let mode = |app: &App| match app.collaboration_composer.as_ref().map(|c| &c.target) {
        Some(CollaborationComposeTarget::Send { mode, .. }) => *mode,
        other => panic!("expected a request composer, got {other:?}"),
    };
    assert_eq!(
        mode(&app),
        ComposeSendMode::ReadOnly,
        "a persisted just-send is not used"
    );
    let ctrl_e = || Event::Key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
    let _ = handle_event(ctrl_e(), &mut app);
    assert_eq!(mode(&app), ComposeSendMode::Execute);
    let _ = handle_event(ctrl_e(), &mut app);
    assert_eq!(
        mode(&app),
        ComposeSendMode::ReadOnly,
        "just-send is skipped"
    );
    // Should one appear anyway, Enter refuses it and keeps the draft.
    let _ = press_key(&mut app, KeyCode::Char('x'));
    if let Some(CollaborationComposeTarget::Send { mode, .. }) =
        app.collaboration_composer.as_mut().map(|c| &mut c.target)
    {
        *mode = ComposeSendMode::JustSend;
    }
    assert!(matches!(press_key(&mut app, KeyCode::Enter), Action::None));
    assert_eq!(
        app.collaboration_composer
            .as_ref()
            .map(|c| c.input.as_str()),
        Some("x")
    );
}

#[test]
fn a_session_pinned_mailbox_hides_a_predecessors_history_in_the_same_pane() {
    let mut predecessor = collab_participant("%1", "callabo", "CAL-1");
    predecessor.agent_session_id = "old".into();
    let mut current = predecessor.clone();
    current.agent_session_id = "new".into();
    let anchor = |session: Option<&str>| {
        MailboxHistoryScope::Pane(MailboxAnchor {
            origin: CollaborationOrigin {
                exact_session: session.is_some(),
                pane: "%1".into(),
                socket: None,
                console: false,
                agent_session_id: session.map(str::to_owned),
            },
            label: "codex@%1".into(),
        })
    };
    assert!(anchor(Some("new")).matches(&current));
    assert!(!anchor(Some("new")).matches(&predecessor));
    assert!(
        anchor(None).matches(&predecessor),
        "unpinned anchors keep pane history"
    );
}

#[test]
fn the_board_counts_each_works_open_collaboration_once_by_agent_session() {
    let (app, _) = board_with_agents();
    let work = app.board.selected_work("").unwrap().clone();
    let request = |id: &str, from: &str, to: &str, status: RequestStatus| {
        let mut request = collab_request("%1");
        request.id = id.into();
        request.from.agent_session_id = from.into();
        request.to.agent_session_id = to.into();
        request.status = status;
        request
    };
    let requests = vec![
        request("r1", "elsewhere", "%1", RequestStatus::Queued),
        // Listed twice (a page boundary, say): still one request.
        request("r1", "elsewhere", "%1", RequestStatus::Queued),
        request("r2", "%2", "elsewhere", RequestStatus::Claimed),
        // Between two of its own agents: owed and awaited.
        request("r3", "%1", "%3", RequestStatus::Queued),
        request("r4", "elsewhere", "%2", RequestStatus::Completed),
        request("r5", "elsewhere", "other-work", RequestStatus::Queued),
    ];
    let summary = board_screen::collab_summary_counts(
        &work,
        &muxa::collaboration::CollaborationOpenCounts::from_requests(&requests),
    );
    assert_eq!(
        (summary.open_to, summary.open_from),
        (2, 2),
        "r1 + r3 to it; r2 + r3 from it; r4 is done; r5 is not its"
    );
    let mut collab = BoardCollab::default();
    assert_eq!(collab.line(&work), "collab  …");
    collab.value = Some(Err("refused".into()));
    assert_eq!(collab.line(&work), "collab  unavailable");
    collab.value = Some(Ok(
        muxa::collaboration::CollaborationOpenCounts::from_requests(&requests),
    ));
    assert_eq!(
        collab.line(&work),
        "collab  2 open to its agents · 2 open from them"
    );
}

#[test]
fn enter_and_preview_follow_the_agent_the_detail_marks() {
    // The ▸ agent is %1 (working); %2 is the one waiting on you, which
    // is where Enter used to go regardless of the cursor.
    let (mut app, keys) = board_with_agents();
    match press_key(&mut app, KeyCode::Enter) {
        Action::AttachTopologyPane(key) => assert_eq!(key, keys[0]),
        _ => panic!("Enter goes to the ▸ agent"),
    }
    match press_key(&mut app, KeyCode::Char('o')) {
        Action::OpenPreviewFor(key) => assert_eq!(key, keys[0]),
        _ => panic!("o previews the ▸ agent"),
    }
}

#[test]
fn board_c_copies_the_cursor_agents_prompt_and_nothing_else() {
    let (mut app, _) = board_with_agents();
    let mut work = app.board.selected_work("").unwrap().clone();
    work.runs[0].panes[0].agent.as_mut().unwrap().last_prompt = Some("ship it".into());
    app.board
        .set_snapshot(board_snapshot(vec![work]), Vec::new());
    assert!(matches!(
        press_key(&mut app, KeyCode::Char('c')),
        Action::Quick(QuickAction::CopyPrompt(ref prompt)) if prompt == "ship it"
    ));
    // A copy aimed at the hidden topology selection's prompt is refused.
    assert!(matches!(
        gate_board_action(
            &mut app,
            Action::Quick(QuickAction::CopyPrompt("other".into()))
        ),
        Action::None
    ));
    let _ = press_key(&mut app, KeyCode::Tab);
    assert!(matches!(
        press_key(&mut app, KeyCode::Char('c')),
        Action::None
    ));
}

#[test]
fn board_k_confirms_closing_exactly_the_cursor_agents_pane() {
    let (mut app, keys) = board_with_agents();
    let _ = press_key(&mut app, KeyCode::Tab);
    match press_key(&mut app, KeyCode::Char('K')) {
        Action::AskConfirm(ConfirmPopup {
            on_confirm: ConfirmAction::Quick(QuickAction::TerminateNode(TopologyNodeKey::Pane(key))),
            message,
        }) => {
            assert_eq!(key, keys[1]);
            assert!(message.contains("end its agent"), "{message}");
        }
        _ => panic!("K should confirm closing the ▸ agent's pane"),
    }
    // Cancelled, it authorises nothing later.
    let _ = gate_board_action(&mut app, Action::ConfirmCancel);
    let later = Action::AskConfirm(ConfirmPopup {
        message: "Close?".into(),
        on_confirm: ConfirmAction::Quick(QuickAction::TerminateNode(TopologyNodeKey::Pane(
            keys[1].clone(),
        ))),
    });
    assert!(matches!(gate_board_action(&mut app, later), Action::None));
}

#[test]
fn the_board_lists_a_works_workers_by_agent_session_and_points_at_prune() {
    use muxa::collaboration::{WorkerEntry, WorkerFlag, WorkerInventory};
    let (app, _) = board_with_agents();
    let work = app.board.selected_work("").unwrap().clone();
    let worker = |session: &str, flags: Vec<WorkerFlag>| WorkerEntry {
        pane: "%1".into(),
        agent_kind: AgentKind::Codex,
        agent_session_id: session.into(),
        room: muxa::collaboration::RoomId {
            host: "tmux".into(),
            socket: Some("a".into()),
            window_id: "@4".into(),
        },
        tmux_session_name: None,
        window_name: None,
        roles: Vec::new(),
        alias: None,
        state: AgentState::Idle,
        pending: false,
        draining: false,
        observed_idle: None,
        queued_requests: 0,
        claimed_requests: 0,
        flags,
    };
    let inventory = |workers| WorkerInventory {
        workers,
        idle_threshold_secs: BOARD_WORKER_IDLE_SECS,
        recorded_grants: 0,
    };
    let mut board_workers = BoardWorkers::default();
    assert_eq!(board_workers.line(&work), "workers  …");
    board_workers.value = Some(Err("old daemon".into()));
    assert_eq!(board_workers.line(&work), "workers  unavailable");
    board_workers.value = Some(Ok(inventory(vec![worker("elsewhere", Vec::new())])));
    assert_eq!(board_workers.line(&work), "workers  none");
    board_workers.value = Some(Ok(inventory(vec![
        worker("%1", vec![WorkerFlag::LongIdle]),
        worker("%1", vec![WorkerFlag::LongIdle]),
        worker("%3", vec![WorkerFlag::Error, WorkerFlag::PossibleOverlap]),
        worker("elsewhere", vec![WorkerFlag::Error]),
    ])));
    assert_eq!(
        board_workers.line(&work),
        "workers  2 · 1 long idle · 1 in error · 1 possibly overlapping — review candidates with `muxa workers prune` (--error for errored)"
    );
}

#[test]
fn mailbox_x_cancels_only_a_queued_request_this_console_sent() {
    let (mut app, _) = board_ready_to_message();
    let mut queued = collab_request("%1");
    queued.status = RequestStatus::Queued;
    let mut claimed = collab_request("%2");
    claimed.id = "req_2".into();
    claimed.status = RequestStatus::Claimed;
    app.collaboration.sent = vec![queued, claimed];
    app.collaboration_mailbox.open = true;
    app.collaboration_mailbox.tab = CollaborationMailboxTab::Sent;
    let action = press_key(&mut app, KeyCode::Char('x'));
    match &action {
        Action::AskConfirm(ConfirmPopup {
            on_confirm: ConfirmAction::CancelCollaboration { origin, request_id },
            ..
        }) => {
            assert_eq!(request_id, "req_1");
            assert!(origin.console);
        }
        _ => panic!("x should confirm cancelling the queued request"),
    }
    assert!(matches!(
        gate_board_action(&mut app, action),
        Action::AskConfirm(_)
    ));
    app.collaboration_mailbox.selected = 1;
    assert!(matches!(
        press_key(&mut app, KeyCode::Char('x')),
        Action::None
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pane_being_written_cannot_be_written_again_until_it_finishes() {
    let (mut app, keys) = board_with_agents();
    let (release, gate) = std::sync::mpsc::channel::<()>();
    start_quick_with(
        &mut app,
        QuickAction::SendPromptToMany {
            panes: keys[..2].to_vec(),
            unavailable: Vec::new(),
            text: "first".into(),
        },
        move |_| {
            let _ = gate.recv();
            ActionOutcome::Ok("done".into())
        },
    );
    // The board refuses to open anything for a busy pane…
    assert!(matches!(
        press_key(&mut app, KeyCode::Char('P')),
        Action::None
    ));
    assert!(app.collaboration_composer.is_none());
    assert!(app
        .footer_hint
        .as_ref()
        .is_some_and(|hint| hint.message.contains("still sending to a:%1")));
    // …and the run loop refuses any action that reaches one anyway.
    assert!(refuse_busy(
        &mut app,
        &QuickAction::InterruptPane(keys[1].clone())
    ));
    assert!(!refuse_busy(
        &mut app,
        &QuickAction::InterruptPane(keys[2].clone())
    ));
    release.send(()).unwrap();
    let mut finished = Vec::new();
    for _ in 0..200 {
        finished = take_finished_quick(&mut app);
        if !finished.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(finished.len(), 1);
    assert!(!refuse_busy(
        &mut app,
        &QuickAction::InterruptPane(keys[1].clone())
    ));
}

#[test]
fn many_pane_actions_count_the_intended_set_once_each() {
    let (_, keys) = board_with_agents();
    let ok = many_pane_outcome(&keys[..2], &[], "prompted", |_| Ok(()));
    assert!(matches!(ok, ActionOutcome::Ok(ref m) if m.contains("prompted 2/2")));
    // A failed write, a target that no longer resolves, and a duplicate.
    let mut writes = Vec::new();
    let partial = many_pane_outcome(
        &[keys[0].clone(), keys[1].clone(), keys[0].clone()],
        &["/tmp/a:%7".into()],
        "prompted",
        |pane| {
            writes.push(pane.pane_id.clone());
            if pane.pane_id == "%2" {
                Err("gone".into())
            } else {
                Ok(())
            }
        },
    );
    assert_eq!(writes, ["%1", "%2"], "each pane is written once");
    let ActionOutcome::Err(message) = partial else {
        panic!("partial failure must be an error")
    };
    assert!(message.contains("prompted 1/3"), "{message}");
    assert!(
        message.contains("a:%2: gone"),
        "names the server: {message}"
    );
    assert!(message.contains("/tmp/a:%7: no longer open"), "{message}");
}

#[test]
fn a_work_wide_prompt_stays_keystrokes_whatever_ctrl_e_and_tab_do() {
    let (mut app, keys) = board_with_agents();
    let _ = press_key(&mut app, KeyCode::Char('P'));
    for event in [
        Event::Key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL)),
        Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
    ] {
        let _ = handle_event(event, &mut app);
        match app.collaboration_composer.as_ref().map(|c| &c.target) {
            Some(CollaborationComposeTarget::PromptPanes { panes, .. }) => {
                assert_eq!(panes, &keys[..2]);
            }
            other => panic!("the composer changed into {other:?}"),
        }
    }
}

#[test]
fn board_freezes_targets_when_the_composer_opens() {
    let (mut app, keys) = board_with_agents();
    let _ = press_key(&mut app, KeyCode::Char('P'));
    // A refresh lands mid-compose and the Work loses an agent.
    let mut work = app.board.selected_work("").unwrap().clone();
    work.runs[0].panes.remove(0);
    app.board
        .set_snapshot(board_snapshot(vec![work]), Vec::new());
    for c in "hi".chars() {
        let _ = press_key(&mut app, KeyCode::Char(c));
    }
    match press_key(&mut app, KeyCode::Enter) {
        Action::Quick(QuickAction::SendPromptToMany { panes, .. }) => {
            assert_eq!(panes, keys[..2], "the set shown when it opened");
        }
        _ => panic!("the frozen prompt must still be sent"),
    }
}

#[test]
fn board_agent_cursor_follows_the_agent_across_a_refresh() {
    let (mut app, keys) = board_with_agents();
    let _ = press_key(&mut app, KeyCode::Tab);
    // A refresh puts a new agent first: the cursor stays on %2.
    let mut work = app.board.selected_work("").unwrap().clone();
    let mut newcomer = work.runs[0].panes[0].clone();
    newcomer.pane_id = "%0".into();
    work.runs[0].panes.insert(0, newcomer);
    app.board
        .set_snapshot(board_snapshot(vec![work]), Vec::new());
    match press_key(&mut app, KeyCode::Char('R')) {
        Action::AskConfirm(ConfirmPopup {
            on_confirm: ConfirmAction::Quick(QuickAction::InterruptPane(key)),
            ..
        }) => assert_eq!(key, keys[1]),
        _ => panic!("R should still address %2"),
    }
}

#[test]
fn board_reports_agents_that_no_longer_resolve_instead_of_dropping_them() {
    let (mut app, keys) = board_with_agents();
    let mut work = app.board.selected_work("").unwrap().clone();
    let mut ghost = work.runs[0].panes[0].clone();
    ghost.pane_id = "%7".into();
    work.runs[0].panes.push(ghost);
    app.board
        .set_snapshot(board_snapshot(vec![work]), Vec::new());
    match press_key(&mut app, KeyCode::Char('A')) {
        Action::AskConfirm(ConfirmPopup {
            on_confirm: ConfirmAction::Quick(QuickAction::InterruptPanes { panes, unavailable }),
            message,
        }) => {
            assert_eq!(panes, keys[..2]);
            assert_eq!(unavailable.len(), 1, "{unavailable:?}");
            assert!(message.contains("all 3 agents, 1 unavailable"), "{message}");
        }
        _ => panic!("A should confirm with the unavailable agent named"),
    }
}

#[test]
fn board_sort_is_its_own_and_sort_on_the_board_never_touches_topology() {
    let (mut app, _) = board_with_agents();
    let topology_sort = app.watch_cfg.sort.clone();
    let key = |c: char| Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    let _ = handle_event(key(':'), &mut app);
    for c in "sort busy".chars() {
        let _ = handle_event(key(c), &mut app);
    }
    let action = handle_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
    );
    assert!(matches!(
        action,
        Action::SetBoardSort(muxa::config::WatchBoardSort::Busy)
    ));
    assert_eq!(app.watch_cfg.sort, topology_sort);
    assert!(matches!(
        execute_palette_command(&mut app, "sort duration"),
        Action::None
    ));
    app.apply_screen(WatchScreen::Topology);
    assert!(!matches!(
        execute_palette_command(&mut app, "sort duration"),
        Action::SetBoardSort(_) | Action::None
    ));
}

#[test]
fn board_stats_label_tells_loading_unavailable_and_figures_apart() {
    let mut stats = BoardStats::default();
    assert_eq!(stats.label("today"), "all sessions · today · ACT …");
    stats.value = Some(Err("daemon down".into()));
    assert_eq!(
        stats.label("today"),
        "all sessions · today · ACT unavailable"
    );
    stats.value = Some(Ok([
        (
            "rmux:%1".to_string(),
            crate::stats::ActiveDuration {
                active_secs: 3 * 3600,
                work_active_secs: 0,
            },
        ),
        (
            "rmux:%2".to_string(),
            crate::stats::ActiveDuration {
                active_secs: 5 * 60,
                work_active_secs: 0,
            },
        ),
    ]
    .into_iter()
    .collect()));
    assert_eq!(stats.label("7d"), "all sessions · 7d · ACT 3h05m · WACT -");
}

#[test]
fn each_work_gets_the_activity_of_its_own_panes_and_can_sort_by_it() {
    let (mut app, _) = board_with_agents();
    let mut quiet = board_work("CAL-2", None);
    quiet.latest_at = Some(OffsetDateTime::now_utc());
    let busy = app.board.selected_work("").unwrap().clone();
    app.board
        .set_snapshot(board_snapshot(vec![quiet, busy]), Vec::new());
    let act = |secs: u64| crate::stats::ActiveDuration {
        active_secs: secs,
        work_active_secs: secs / 2,
    };
    // CAL-1's panes are %1..%3; %9 belongs to nobody on the board.
    let by_pane: std::collections::BTreeMap<String, _> = [
        ("%1".to_string(), act(600)),
        ("%2".to_string(), act(60)),
        ("%9".to_string(), act(9_000)),
    ]
    .into_iter()
    .collect();
    app.board
        .set_activity(board_screen::ActivityState::Known(by_pane));
    let cal1 = app
        .board
        .visible("")
        .into_iter()
        .find(|work| work.identity.work_id == "CAL-1")
        .unwrap()
        .clone();
    assert_eq!(
        app.board.active_of(&cal1),
        board_screen::WorkActivity::Known(act(660))
    );
    app.board.set_sort(muxa::config::WatchBoardSort::Active);
    assert_eq!(app.board.visible("")[0].identity.work_id, "CAL-1");
    app.board.set_activity(board_screen::ActivityState::Loading);
    assert_eq!(
        app.board.active_of(&cal1),
        board_screen::WorkActivity::Loading
    );
    app.board
        .set_activity(board_screen::ActivityState::Unavailable);
    assert_eq!(
        app.board.active_of(&cal1).cell(),
        "n/a",
        "a failure is not loading"
    );
}

#[test]
fn the_corner_title_shortens_then_yields_instead_of_overlapping_the_counts() {
    let left = " Board · 20 Works · 0 need you · 78 unlinked · by name ";
    let full = " all sessions · today · ACT 6h04m · WACT 6h04m ".to_string();
    let short = " ACT 6h04m · WACT 6h04m ".to_string();
    let candidates = [full.clone(), short.clone()];
    assert_eq!(fit_corner_title(left, &candidates, 160), Some(full));
    assert_eq!(fit_corner_title(left, &candidates, 90), Some(short));
    assert_eq!(fit_corner_title(left, &candidates, 60), None);
}

#[test]
fn alt_3_and_the_palette_reach_the_board() {
    let mut app = App::with_legacy_config(WatchConfig::default());
    assert!(matches!(
        alt_key_action(&mut app, '3'),
        Action::SetScreen(WatchScreen::Board)
    ));
    assert!(matches!(
        execute_palette_command(&mut app, "screen board"),
        Action::SetScreen(WatchScreen::Board)
    ));
    assert!(matches!(
        execute_palette_command(&mut app, "board"),
        Action::SetScreen(WatchScreen::Board)
    ));
}

#[test]
fn board_enter_attaches_the_runs_exact_pane_not_a_same_id_pane_elsewhere() {
    let mut app = App::with_legacy_config(WatchConfig::default());
    app.set_data(
        Vec::new(),
        vec![
            fake_topology_pane("/tmp/a", "$1", "callabo", "@4", "CAL-1", 1, "%2", 0),
            fake_topology_pane("/tmp/b", "$1", "callabo", "@4", "CAL-1", 1, "%2", 0),
        ],
    );
    let on_b = app.pane_key_for("%2", Some("b")).expect("pane on b");
    // The Work projection may name the server by its path where the
    // topology uses its short name. Either way it must resolve to b.
    let mut spelled_as_path = on_b.clone();
    spelled_as_path.window.session.endpoint.socket = "/tmp/b".into();
    app.board.set_snapshot(
        board_snapshot(vec![board_work("CAL-1", Some(&spelled_as_path))]),
        Vec::new(),
    );
    app.apply_screen(WatchScreen::Board);
    match handle_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
    ) {
        Action::AttachTopologyPane(key) => assert_eq!(key, on_b),
        _ => panic!("Enter should attach the Run's pane"),
    }
}

#[test]
fn board_enter_never_substitutes_the_same_pane_id_on_another_server() {
    let mut app = App::with_legacy_config(WatchConfig::default());
    app.set_data(
        Vec::new(),
        vec![fake_topology_pane(
            "/tmp/a", "$1", "callabo", "@4", "CAL-1", 1, "%2", 0,
        )],
    );
    let mut elsewhere = app.pane_key_for("%2", Some("a")).expect("pane on a");
    elsewhere.window.session.endpoint.socket = "/tmp/c".into();
    app.board.set_snapshot(
        board_snapshot(vec![board_work("CAL-1", Some(&elsewhere))]),
        Vec::new(),
    );
    app.apply_screen(WatchScreen::Board);
    assert!(matches!(
        handle_event(
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            &mut app
        ),
        Action::None
    ));
    assert!(app
        .footer_hint
        .as_ref()
        .is_some_and(|hint| hint.message.contains("no longer open")));
}

#[test]
fn board_preview_opens_the_selected_works_run_not_the_topology_cursor() {
    let (mut app, b) = board_on_b_topology_on_a();
    for preview in [
        Event::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE)),
        Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::ALT)),
    ] {
        match handle_event(preview, &mut app) {
            Action::OpenPreviewFor(key) => assert_eq!(key, b),
            _ => panic!("preview should target the Work's Run"),
        }
    }
}

#[test]
fn board_refuses_topology_actions_aimed_at_the_hidden_cursor() {
    let (mut app, _) = board_on_b_topology_on_a();
    for c in ['k', 'x', 'm'] {
        let action = handle_event(
            Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)),
            &mut app,
        );
        assert!(
            matches!(action, Action::None),
            "Alt-{c} must not act on pane A"
        );
    }
    let kill = quick_kill_action(&app);
    assert!(
        !matches!(kill, Action::None),
        "the topology would offer a kill"
    );
    assert!(matches!(gate_board_action(&mut app, kill), Action::None));
    assert!(app
        .footer_hint
        .as_ref()
        .is_some_and(|hint| hint.message.contains("not on the Work board")));
    // Screen switching still works.
    assert!(matches!(
        handle_event(
            Event::Key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::ALT)),
            &mut app
        ),
        Action::SetScreen(WatchScreen::Topology)
    ));
}

#[test]
fn board_never_opens_a_form_for_the_hidden_topology_node() {
    let key = |c: char| Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    let (mut app, _) = board_on_b_topology_on_a();
    let selected = app.selected_node_key();
    for c in ['R', 'n', 'm', 'w', 'C', 'h', 'l'] {
        let _ = handle_event(key(c), &mut app);
        assert!(
            app.rename.is_none(),
            "{c} opened rename for the hidden node"
        );
        assert!(app.spawn.is_none(), "{c} opened spawn for the hidden node");
        assert!(
            app.collaboration_composer.is_none(),
            "{c} opened a composer"
        );
        assert!(app.work_composer.is_none(), "{c} opened the work composer");
    }
    for modifiers in [KeyModifiers::NONE, KeyModifiers::CONTROL] {
        for arrow in [KeyCode::Left, KeyCode::Right] {
            let _ = handle_event(Event::Key(KeyEvent::new(arrow, modifiers)), &mut app);
        }
        for c in ['n', 'h', 'l', 'R'] {
            let _ = handle_event(
                Event::Key(KeyEvent::new(KeyCode::Char(c), modifiers)),
                &mut app,
            );
            assert!(app.spawn.is_none(), "{modifiers:?}-{c} opened spawn");
            assert!(app.rename.is_none(), "{modifiers:?}-{c} opened rename");
        }
    }
    // The board's own Ctrl bindings are consumed before the refusal.
    assert!(matches!(
        handle_event(
            Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            &mut app
        ),
        Action::Refresh
    ));
    assert_eq!(
        app.selected_node_key(),
        selected,
        "the hidden cursor never moved"
    );
    // The real palette route: `:` then typing, then Enter.
    for command in ["rename", "kill", "message", "move"] {
        let _ = handle_event(key(':'), &mut app);
        for c in command.chars() {
            let _ = handle_event(key(c), &mut app);
        }
        let action = handle_event(
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            &mut app,
        );
        assert!(
            matches!(action, Action::None),
            ":{command} returned an action"
        );
        assert!(app.rename.is_none(), ":{command} opened rename");
    }
    // Typing a filter still works on the board.
    app.arm_explicit_search();
    let _ = handle_event(key('R'), &mut app);
    assert_eq!(app.search_query, "R");
}

#[test]
fn board_enter_refuses_an_rmux_server_that_only_shares_a_file_name() {
    let mut app = App::with_legacy_config(WatchConfig::default());
    let mut pane = fake_topology_pane(
        "/tmp/rmux-new/default",
        "$1",
        "callabo",
        "@4",
        "CAL-1",
        1,
        "rmux:%2",
        0,
    );
    pane.pane_id = "rmux:%2".into();
    app.set_data(Vec::new(), vec![pane]);
    let mut old = app
        .topology
        .sessions
        .iter()
        .flat_map(|s| s.windows.iter())
        .flat_map(|w| w.panes.iter())
        .map(|p| p.key.clone())
        .next()
        .expect("rmux pane");
    old.window.session.endpoint.socket = "/tmp/rmux-old/default".into();
    app.board.set_snapshot(
        board_snapshot(vec![board_work("CAL-1", Some(&old))]),
        Vec::new(),
    );
    app.apply_screen(WatchScreen::Board);
    assert!(matches!(
        handle_event(
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            &mut app
        ),
        Action::None
    ));
}

#[tokio::test]
async fn board_scans_never_block_and_only_one_runs_at_a_time() {
    let mut refresher = BoardRefresher::default();
    let (release, gate) = tokio::sync::oneshot::channel::<()>();
    assert!(refresher.start(async move {
        let _ = gate.await;
        (board_snapshot(vec![board_work("CAL-1", None)]), Vec::new())
    }));
    assert!(!refresher.start(async { (board_snapshot(Vec::new()), Vec::new()) }));
    assert!(
        refresher.take_finished().is_none(),
        "a pending scan is not waited on"
    );
    release.send(()).unwrap();
    let mut result = None;
    for _ in 0..100 {
        tokio::task::yield_now().await;
        if let Some(scan) = refresher.take_finished() {
            result = Some(scan);
            break;
        }
    }
    let (snapshot, _) = result.expect("scan arrives");
    assert_eq!(snapshot.works.len(), 1);
    assert!(refresher.start(async { (board_snapshot(Vec::new()), Vec::new()) }));
}

#[test]
fn an_empty_board_still_shows_why_the_scan_failed() {
    let mut app = App::with_legacy_config(WatchConfig::default());
    app.board.set_snapshot(
        board_snapshot(Vec::new()),
        vec!["scan /tmp/x: refused".into()],
    );
    app.apply_screen(WatchScreen::Board);
    let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let screen = (0..terminal.backend().buffer().area().height)
        .map(|y| row_text(terminal.backend().buffer(), y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(screen.contains("scan /tmp/x: refused"), "{screen}");
}

#[test]
fn board_enter_on_a_work_without_a_live_run_says_so() {
    let mut app = App::with_legacy_config(WatchConfig::default());
    app.board
        .set_snapshot(board_snapshot(vec![board_work("CAL-1", None)]), Vec::new());
    app.apply_screen(WatchScreen::Board);
    assert!(matches!(
        handle_event(
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            &mut app
        ),
        Action::None
    ));
    assert!(app
        .footer_hint
        .as_ref()
        .is_some_and(|hint| hint.message.contains("no live Run")));
}

#[test]
fn board_renders_works_with_their_detail_and_moves_with_j() {
    let mut app = App::with_legacy_config(WatchConfig::default());
    let mut snap = board_snapshot(vec![board_work("CAL-1", None), board_work("CAL-2", None)]);
    snap.unlinked_executions = Vec::new();
    app.board.set_snapshot(snap, Vec::new());
    app.apply_screen(WatchScreen::Board);
    let draw = |app: &mut App, width: u16| {
        let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        (0..terminal.backend().buffer().area().height)
            .map(|y| row_text(terminal.backend().buffer(), y))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let screen = draw(&mut app, 110);
    assert!(screen.contains("Board · 2 Works · 0 need you"), "{screen}");
    assert!(screen.contains("callabo/CAL-1"), "{screen}");
    assert!(screen.contains("goal  ship it"), "{screen}");
    assert!(screen.contains("go to Run"), "{screen}");
    let first = app
        .board
        .selected_work("")
        .unwrap()
        .identity
        .work_id
        .clone();
    let _ = handle_event(
        Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)),
        &mut app,
    );
    let second = app
        .board
        .selected_work("")
        .unwrap()
        .identity
        .work_id
        .clone();
    assert_ne!(first, second);
    // Wide terminals put the detail beside the list.
    let wide = draw(&mut app, 160);
    assert!(wide.contains(&format!("work {second}")), "{wide}");
}
