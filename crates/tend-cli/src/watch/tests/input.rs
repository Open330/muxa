use super::*;

#[test]
fn the_palette_reaches_both_screens() {
    let mut app = App::with_legacy_config(WatchConfig::default());
    assert!(matches!(
        execute_palette_command(&mut app, "screen collab"),
        Action::SetScreen(WatchScreen::Collab)
    ));
    assert!(matches!(
        execute_palette_command(&mut app, "topology"),
        Action::SetScreen(WatchScreen::Topology)
    ));
}

#[test]
fn collab_layout_toggles_by_key_and_palette_without_touching_topology_layout() {
    let mut app = collab_app("%2");
    assert_eq!(app.collab.layout(), CollabLayout::Table);
    let Action::SetCollabLayout(layout) = key_action(&mut app, 'v') else {
        panic!("v should set the collaboration layout");
    };
    app.collab.set_layout(layout);
    assert_eq!(app.collab.layout(), CollabLayout::Sequence);
    assert_eq!(app.watch_cfg.layout, WatchLayout::Tree);

    assert!(matches!(
        execute_palette_command(&mut app, "layout table"),
        Action::SetCollabLayout(CollabLayout::Table)
    ));
    assert_eq!(parse_collab_layout("history"), Some(CollabLayout::Sequence));
}

#[test]
fn collab_sequence_paints_lifelines_and_chronological_arrow() {
    let mut app = collab_app("%2");
    app.collab.set_layout(CollabLayout::Sequence);
    let backend = ratatui::backend::TestBackend::new(180, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| render(f, &mut app)).unwrap();
    let painted: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(painted.contains("SEQUENCE / LIFELINES"), "{painted}");
    assert!(painted.contains("●──────▶"), "{painted}");
    assert!(painted.contains("sequence"), "{painted}");
    assert!(painted.contains(COLLAB_FIXTURE_BODY), "{painted}");
}

#[test]
fn enter_on_a_collab_row_goes_to_the_peer_pane() {
    // The peer is the end that has the work; the sender is often the
    // console, which is nowhere to attach to.
    let mut app = collab_app("%2");
    let action = handle_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
    );
    match action {
        Action::AttachTopologyPane(key) => {
            assert_eq!(key.pane_id, "%2");
            assert_eq!(key.window.session.endpoint.socket, "default");
        }
        other => panic!("expected an attach to the peer pane, got {other:?}"),
    }
}

#[test]
fn enter_says_which_pane_is_gone_instead_of_doing_nothing() {
    // Requests outlive their panes. Silence here reads as a broken key.
    let mut app = collab_app("%99");
    let action = handle_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
    );
    assert!(matches!(action, Action::None));
    let hint = app.footer_hint.as_ref().expect("a hint explains the no-op");
    assert!(
        hint.message.contains("%99") && hint.message.contains("no longer"),
        "unexpected hint: {}",
        hint.message
    );
}

#[test]
fn the_collab_screen_still_leaves_typing_to_the_filter() {
    // `j` is a movement key only while nothing is being searched for —
    // the same rule the tree follows.
    let mut app = collab_app("%2");
    app.arm_explicit_search();
    let _ = handle_event(
        Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)),
        &mut app,
    );
    assert_eq!(app.search_query, "j", "the keystroke went to the filter");
    assert_eq!(app.collab.selected_index(), 0);
}

#[test]
fn the_collab_screen_paints_where_each_request_happened() {
    let mut app = collab_app("%2");
    let backend = ratatui::backend::TestBackend::new(160, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| render(f, &mut app)).unwrap();
    let painted: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    // Both ends, the room each sits in, and the message itself: without
    // the location a fleet-wide list cannot say where the work happened.
    assert!(painted.contains("webapp:ENG-7330"), "{painted}");
    assert!(painted.contains("Collab"), "{painted}");
    assert!(painted.contains("check the upload chunking"), "{painted}");
}

#[test]
fn the_detail_pane_carries_the_body_the_row_had_to_cut() {
    // The row can only hold an excerpt, and the body is the reason the
    // operator opened the screen.
    let mut app = collab_app("%2");
    let backend = ratatui::backend::TestBackend::new(160, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| render(f, &mut app)).unwrap();
    let painted: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(painted.contains(COLLAB_FIXTURE_BODY), "{painted}");
    assert!(painted.contains("req_1"), "the pane is titled by request");
    // The footer speaks this screen's language, not the tree's.
    assert!(painted.contains("go to peer"), "{painted}");
    assert!(!painted.contains("tree"), "{painted}");
}

#[test]
fn a_short_terminal_keeps_the_listing_instead_of_the_detail() {
    // Splitting a 12-row frame would leave three rows of listing.
    let mut app = collab_app("%2");
    let backend = ratatui::backend::TestBackend::new(160, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| render(f, &mut app)).unwrap();
    let painted: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(!painted.contains(COLLAB_FIXTURE_BODY), "{painted}");
    assert!(painted.contains("Collab"), "{painted}");
}

#[test]
fn rename_submitting_an_untouched_prefill_is_a_cancel() {
    // The form opens prefilled, so Enter on an unchanged field is the most
    // likely accident in it. Renaming a window to the name it already has
    // would still pin `automatic-rename off` as a side effect, which is a
    // real change nobody asked for.
    let mut app = rename_app(RenameLevel::Window, "eng-7306");
    let action = handle_rename_event(KeyCode::Enter, KeyModifiers::NONE, &mut app);
    assert!(matches!(action, Action::None));
    assert!(app.rename.is_none(), "the form closes");
}

#[test]
fn rename_submits_only_a_changed_name() {
    let mut app = rename_app(RenameLevel::Window, "eng-7306");
    {
        let rename = app.rename.as_mut().expect("form is open");
        spawn_edit_text(
            KeyCode::Char('!'),
            KeyModifiers::NONE,
            &mut rename.input,
            &mut rename.cursor,
        );
    }
    let action = handle_rename_event(KeyCode::Enter, KeyModifiers::NONE, &mut app);
    assert!(matches!(action, Action::SubmitRename));
    // The composer survives until the run loop takes it — the apply needs
    // the key it captured when the form opened.
    assert!(app.rename.is_some());
}

#[test]
fn rename_success_asks_for_an_immediate_refresh() {
    // The rename lands in tmux, and tendd has nothing to announce about
    // it. Without this wake the row keeps its old name until the fallback
    // tick, which reads as a rename that silently did not take.
    let mut app = rename_app(RenameLevel::Window, "eng-7306");
    let (wake_tx, mut wake_rx) = mpsc::channel::<()>(WAKE_CAPACITY);
    finish_rename(&mut app, &wake_tx, |_| Ok("renamed window".into()));
    assert!(app.rename.is_none(), "the form closes on success");
    assert!(app.refresh_pending, "the title shows the sync in flight");
    assert!(wake_rx.try_recv().is_ok(), "the refresh task was woken");
}

#[test]
fn rename_failure_keeps_the_form_and_skips_the_refresh() {
    let mut app = rename_app(RenameLevel::Window, "eng-7306");
    let (wake_tx, mut wake_rx) = mpsc::channel::<()>(WAKE_CAPACITY);
    finish_rename(&mut app, &wake_tx, |_| Err("duplicate window name".into()));
    assert!(app.rename.is_some(), "the form stays open to be corrected");
    assert!(!app.refresh_pending, "nothing changed to resync");
    assert!(wake_rx.try_recv().is_err(), "no wake for a failed rename");
}

#[test]
fn rename_refuses_an_empty_name_without_closing() {
    let mut app = rename_app(RenameLevel::Session, "webapp");
    app.rename.as_mut().unwrap().input.clear();
    app.rename.as_mut().unwrap().cursor = 0;
    // Enter on an empty field warns rather than submitting; Backspace on
    // an empty field is the deliberate way out.
    let action = handle_rename_event(KeyCode::Enter, KeyModifiers::NONE, &mut app);
    assert!(matches!(action, Action::None));
    assert!(
        app.rename.is_some(),
        "an empty name must not close the form"
    );
    handle_rename_event(KeyCode::Backspace, KeyModifiers::NONE, &mut app);
    assert!(app.rename.is_none());
}

#[test]
fn rename_escape_abandons_the_edit() {
    let mut app = rename_app(RenameLevel::Pane, "claude");
    app.rename.as_mut().unwrap().input.push_str("-review");
    handle_rename_event(KeyCode::Esc, KeyModifiers::NONE, &mut app);
    assert!(app.rename.is_none());
}

#[test]
fn window_name_taken_excludes_the_window_being_renamed() {
    let listing = "@1\tbuild\n@4\teng-7306\n@9\treview\n";
    // Keeping your own name is not a collision with yourself.
    assert!(!window_name_taken(listing, "@4", "eng-7306"));
    // Another window holding it is.
    assert!(window_name_taken(listing, "@4", "review"));
    // Free names are free.
    assert!(!window_name_taken(listing, "@4", "eng-7307"));
    // Whole-field equality, never a prefix: `build` must not match
    // `build-2`, or a rename would be refused against a name nobody has.
    assert!(!window_name_taken(listing, "@4", "build-2"));
}

#[test]
fn rename_level_names_what_tmux_actually_changes() {
    // A pane has no name in tmux; the form says "pane title" so the label
    // matches the thing that ends up different.
    assert_eq!(RenameLevel::Session.label(), "session");
    assert_eq!(RenameLevel::Window.label(), "window");
    assert_eq!(RenameLevel::Pane.label(), "pane title");
}

#[test]
fn rename_rules_preserve_label_spaces_but_normalize_window_spaces() {
    assert_eq!(
        rename_label(" Claude review updated ", RenameLevel::Pane).unwrap(),
        "Claude review updated"
    );
    assert_eq!(
        rename_label("two terminals", RenameLevel::Session).unwrap(),
        "two terminals"
    );
    assert_eq!(
        crate::tmux_work::normalize_window_name("ENG-1 review").unwrap(),
        "ENG-1-review"
    );
    assert!(rename_label("bad:name", RenameLevel::Session).is_err());
    assert!(rename_label("bad\ntitle", RenameLevel::Pane).is_err());
    assert!(rename_label("#{session_name}", RenameLevel::Pane).is_err());
    assert!(rename_label(&"a".repeat(65), RenameLevel::Pane).is_err());
}

#[test]
fn kill_action_disabled_for_paneless_row() {
    let mut app = app_with_paneless_and_pane();
    // Find the paneless row by walking the rows — sort may have
    // moved it relative to insertion order.
    let paneless_idx = app
        .rows
        .iter()
        .position(|r| matches!(r, WatchRow::Agent(a) if a.pane.is_none()))
        .expect("test fixture must include a paneless row");
    app.table_state.select(Some(paneless_idx));

    let action = quick_kill_action(&app);
    assert!(
        matches!(action, Action::NotApplicable(msg) if msg.contains("no tmux pane")),
        "K on a paneless row must yield NotApplicable, got something else"
    );
}

#[test]
fn kill_action_opens_confirm_for_pane_bearing_row() {
    let mut app = app_with_paneless_and_pane();
    let pane_idx = app
        .rows
        .iter()
        .position(|r| matches!(r, WatchRow::Agent(a) if a.pane.as_deref() == Some("%42")))
        .expect("test fixture must include a pane-bearing row");
    app.table_state.select(Some(pane_idx));

    let action = quick_kill_action(&app);
    match action {
        Action::AskConfirm(popup) => {
            assert_eq!(
                popup.on_confirm,
                ConfirmAction::Quick(QuickAction::KillPane("%42".into()))
            );
            // Pane label resolved against the inventory — should
            // be the human-readable form, not the raw `%42`.
            assert!(
                popup.message.contains("main:2.0"),
                "popup message should resolve pane label, got: {}",
                popup.message
            );
        }
        other => panic!("expected AskConfirm, got {other:?}"),
    }
}

#[test]
fn copy_action_uses_last_prompt() {
    let mut app = app_with_paneless_and_pane();
    let pane_idx = app
        .rows
        .iter()
        .position(|r| matches!(r, WatchRow::Agent(a) if a.pane.as_deref() == Some("%42")))
        .expect("test fixture must include a pane-bearing row");
    app.table_state.select(Some(pane_idx));

    let action = quick_copy_action(&app);
    match action {
        Action::Quick(QuickAction::CopyPrompt(text)) => {
            assert_eq!(text, "hello world");
        }
        other => panic!("expected Quick(CopyPrompt), got {other:?}"),
    }
}

#[test]
fn copy_action_disabled_when_no_prompt() {
    let mut app = app_with_paneless_and_pane();
    // Paneless agent has no last_prompt either — covers the
    // "row exists but has nothing to copy" case.
    let paneless_idx = app
        .rows
        .iter()
        .position(|r| matches!(r, WatchRow::Agent(a) if a.pane.is_none()))
        .unwrap();
    app.table_state.select(Some(paneless_idx));

    let action = quick_copy_action(&app);
    assert!(
        matches!(action, Action::NotApplicable(msg) if msg.contains("no prompt")),
        "c with no prompt must yield NotApplicable, got something else"
    );
}

#[test]
fn enter_attaches_to_the_selected_pane() {
    let mut app = app_with_paneless_and_pane();
    let pane_idx = app
        .rows
        .iter()
        .position(|r| matches!(r, WatchRow::Agent(a) if a.pane.as_deref() == Some("%42")))
        .expect("test fixture must include a pane-bearing row");
    app.table_state.select(Some(pane_idx));

    let action = handle_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
    );
    assert!(
        matches!(action, Action::AttachPane(ref p) if p == "%42"),
        "Enter must attach without an intermediate composer, got {action:?}"
    );
}

#[test]
fn enter_on_paneless_row_hints_instead_of_opening_prompt() {
    let mut app = app_with_paneless_and_pane();
    let paneless_idx = app
        .rows
        .iter()
        .position(|r| matches!(r, WatchRow::Agent(a) if a.pane.is_none()))
        .expect("test fixture must include a paneless row");
    app.table_state.select(Some(paneless_idx));

    let action = handle_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
    );
    assert!(
        matches!(action, Action::NotApplicable(msg) if msg.contains("no tmux pane")),
        "Enter on a paneless row must yield NotApplicable, got something else"
    );
}

#[test]
fn pasted_text_with_newline_goes_to_buffer_not_submit() {
    // Bracketed paste delivers the whole payload — newlines and all —
    // as one `Event::Paste`. It must land in the composer buffer, not
    // submit at the embedded `\n` the way a stream of key events would.
    let mut app = collaboration_watch_app();
    open_watch_collaboration_composer(&mut app);

    let action = handle_event(Event::Paste("line one\nline two".into()), &mut app);
    assert!(matches!(action, Action::None));
    assert_eq!(
        app.collaboration_composer.as_ref().unwrap().input,
        "line one\nline two"
    );

    // A subsequent real Enter is what submits.
    let action = handle_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
    );
    assert!(matches!(action, Action::SubmitCollaboration));
}

#[test]
fn paste_outside_composer_becomes_search_query() {
    let mut app = app_with_paneless_and_pane();
    let action = handle_event(Event::Paste("junk".into()), &mut app);
    assert!(matches!(action, Action::None));
    assert_eq!(app.search_query, "junk");
}

#[test]
fn confirm_popup_y_proceeds() {
    // Drives the popup state machine end-to-end: open the popup
    // via `K`, press `y`, observe that ConfirmYes lands and that
    // dispatching the payload calls the right Effects method.
    let mut app = app_with_paneless_and_pane();
    let pane_idx = app
        .rows
        .iter()
        .position(|r| matches!(r, WatchRow::Agent(a) if a.pane.as_deref() == Some("%42")))
        .unwrap();
    app.table_state.select(Some(pane_idx));

    // Open the confirm popup the same way `run` would.
    let Action::AskConfirm(popup) = quick_kill_action(&app) else {
        panic!("expected AskConfirm");
    };
    app.confirm = Some(popup);

    // Press `y` — handle_event must yield ConfirmYes.
    let action = handle_event(
        Event::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)),
        &mut app,
    );
    assert!(matches!(action, Action::ConfirmYes));

    // Mirror the run-loop dispatch: take the popup's payload and
    // route it through dispatch_quick_action.
    let ConfirmAction::Quick(payload) = app.confirm.take().unwrap().on_confirm else {
        panic!("expected a quick confirmation payload");
    };
    let mut fx = RecorderEffects::default();
    let outcome = dispatch_quick_action(payload, &mut fx);
    assert_eq!(fx.kill_calls, vec!["%42"]);
    assert!(matches!(outcome, ActionOutcome::Ok(msg) if msg.contains("killed pane %42")));
}

#[test]
fn confirm_popup_n_cancels() {
    let mut app = app_with_paneless_and_pane();
    app.confirm = Some(ConfirmPopup {
        message: "Kill pane main:2.0?".into(),
        on_confirm: ConfirmAction::Quick(QuickAction::KillPane("%42".into())),
    });

    // `n` is one of "anything that isn't y/Y/Enter" — must cancel.
    let action = handle_event(
        Event::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
        &mut app,
    );
    assert!(matches!(action, Action::ConfirmCancel));
}

#[test]
fn confirm_popup_esc_tab_arrow_all_cancel() {
    // Regression guard for the safety rail: keys you'd plausibly
    // hit by accident (Tab when switching focus, arrow when trying
    // to scroll, Esc when changing your mind) must NOT confirm.
    for key in [
        KeyCode::Esc,
        KeyCode::Tab,
        KeyCode::Up,
        KeyCode::Down,
        KeyCode::Char('q'),
        KeyCode::Char('K'), // even the same key that opened it
    ] {
        let mut app = app_with_paneless_and_pane();
        app.confirm = Some(ConfirmPopup {
            message: "Kill pane?".into(),
            on_confirm: ConfirmAction::Quick(QuickAction::KillPane("%42".into())),
        });
        let action = handle_event(Event::Key(KeyEvent::new(key, KeyModifiers::NONE)), &mut app);
        assert!(
            matches!(action, Action::ConfirmCancel),
            "key {key:?} must cancel the confirm popup"
        );
    }
}

#[test]
fn confirm_popup_enter_proceeds() {
    // Enter is also an accept gate — matches the convention from
    // most other terminal y/N prompts. Spec calls this out
    // explicitly.
    let mut app = app_with_paneless_and_pane();
    app.confirm = Some(ConfirmPopup {
        message: "Kill pane?".into(),
        on_confirm: ConfirmAction::Quick(QuickAction::KillPane("%42".into())),
    });
    let action = handle_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
    );
    assert!(matches!(action, Action::ConfirmYes));
}

#[test]
fn help_overlay_lists_every_binding() {
    // Snapshot test: the help overlay is the user's canonical
    // reference, so any drift between the keybinding matrix and
    // the help text should land here loud and clear.
    let body = help_overlay_text().join("\n");
    assert!(body.contains("/               filter rows"));
    assert!(body.contains("gg/G · Home/End first / last selectable row"));
    assert!(body.contains("↑/↓ · j/k       move siblings in focus mode; visible nodes otherwise"));
    assert!(body.contains(":              command palette"));
    assert!(body.contains("Alt-A          attention-only filter"));
    assert!(body.contains("Alt-S/L/D/T    sibling name / latest / duration / state"));
    assert!(body.contains("Alt-I / Alt-E  inspector / persistent event inbox"));
    assert!(body.contains("m / M / Alt-M  message / mailbox / move pane or window; Space marks"));
    assert!(body.contains("i / e          (in mailbox) claim inbox / reply"));
    assert!(body
        .contains("a/A · Ctrl-E/n ask / conversations · new mode/draft · Enter read · d/D delete"));
    assert!(body.contains("C / n / w / R  shell window / agent pane / work up / rename the row"));
    // The exit keys deliberately live in the overlay's border rather
    // than the matrix — the body is clipped by terminal height, and
    // "how to leave" must not be the row that falls off.
    assert!(!body.contains("q/Ctrl-C quits"));
}

#[test]
fn f1_toggles_help_overlay() {
    let mut app = app_with_paneless_and_pane();
    assert!(!app.help_open);

    // First press — opens.
    let action = handle_event(
        Event::Key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE)),
        &mut app,
    );
    assert!(matches!(action, Action::Quick(QuickAction::ShowHelp)));
    // Mirror the run-loop dispatch.
    if let Action::Quick(QuickAction::ShowHelp) = action {
        app.help_open = !app.help_open;
    }
    assert!(app.help_open, "first ? press must open the overlay");

    // Second press — closes. With help open, only F1 / Alt-? / Esc
    // are accepted; everything else is ignored to avoid
    // double-binding.
    let action = handle_event(
        Event::Key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE)),
        &mut app,
    );
    assert!(matches!(action, Action::Quick(QuickAction::ShowHelp)));
    if let Action::Quick(QuickAction::ShowHelp) = action {
        app.help_open = !app.help_open;
    }
    assert!(!app.help_open, "second ? press must close the overlay");
}

#[test]
fn help_overlay_swallows_other_keys() {
    // While help is open, K / R / C shouldn't fire — the user is
    // reading docs, not driving actions.
    let mut app = app_with_paneless_and_pane();
    app.help_open = true;

    for key in [
        KeyCode::Char('K'),
        KeyCode::Char('R'),
        KeyCode::Char('C'),
        KeyCode::Char('p'),
        KeyCode::Char('r'),
    ] {
        let action = handle_event(Event::Key(KeyEvent::new(key, KeyModifiers::NONE)), &mut app);
        assert!(
            matches!(action, Action::None),
            "while help is open, key {key:?} must be a no-op"
        );
    }
}

#[test]
fn dispatch_kill_pane_calls_effects_and_reports_pane_id() {
    let mut fx = RecorderEffects::default();
    let outcome = dispatch_quick_action(QuickAction::KillPane("%99".into()), &mut fx);
    assert_eq!(fx.kill_calls, vec!["%99"]);
    match outcome {
        ActionOutcome::Ok(msg) => assert!(msg.contains("%99")),
        other => panic!("expected Ok, got {other:?}"),
    }
}

#[test]
fn dispatch_kill_pane_reports_failure_with_message() {
    let mut fx = RecorderEffects {
        kill_result: Some(Err("tmux exited with 1".into())),
        ..Default::default()
    };
    let outcome = dispatch_quick_action(QuickAction::KillPane("%99".into()), &mut fx);
    match outcome {
        ActionOutcome::Err(msg) => {
            assert!(msg.contains("kill-pane failed"));
            assert!(msg.contains("tmux exited with 1"));
        }
        other => panic!("expected Err, got {other:?}"),
    }
}

#[test]
fn dispatch_copy_prompt_pipes_through_effects() {
    let mut fx = RecorderEffects::default();
    let outcome = dispatch_quick_action(QuickAction::CopyPrompt("hi there".into()), &mut fx);
    assert_eq!(fx.copy_calls, vec!["hi there"]);
    assert!(matches!(outcome, ActionOutcome::Ok(msg) if msg.contains("copied prompt")));
}

#[test]
fn dispatch_copy_prompt_via_tmpfile_path_surfaces_warning_text() {
    // When all clipboard helpers are missing the dispatcher writes
    // to /tmp and reports the path so the user can recover their
    // text rather than being told "copied" with nothing in their
    // clipboard.
    let mut fx = RecorderEffects {
        copy_result: Some(Ok("tmpfile:/tmp/tend-clip-1.txt".into())),
        ..Default::default()
    };
    let outcome = dispatch_quick_action(QuickAction::CopyPrompt("payload".into()), &mut fx);
    match outcome {
        ActionOutcome::Ok(msg) => {
            assert!(msg.contains("/tmp/tend-clip-1.txt"));
            assert!(msg.contains("no clipboard tool"));
        }
        other => panic!("expected Ok, got {other:?}"),
    }
}

#[test]
fn dispatch_send_prompt_calls_effects_and_reports_pane_id() {
    let mut fx = RecorderEffects::default();
    let outcome = dispatch_quick_action(
        QuickAction::SendPrompt {
            pane_id: "%99".into(),
            text: "continue".into(),
        },
        &mut fx,
    );
    assert_eq!(
        fx.send_prompt_calls,
        vec![("%99".into(), "continue".into())]
    );
    assert!(matches!(outcome, ActionOutcome::Ok(msg) if msg.contains("%99")));
}

#[test]
fn dispatch_send_prompt_reports_failure_with_message() {
    let mut fx = RecorderEffects {
        send_prompt_result: Some(Err("tmux exited with 1".into())),
        ..Default::default()
    };
    let outcome = dispatch_quick_action(
        QuickAction::SendPrompt {
            pane_id: "%99".into(),
            text: "continue".into(),
        },
        &mut fx,
    );
    match outcome {
        ActionOutcome::Err(msg) => {
            assert!(msg.contains("send failed"));
            assert!(msg.contains("tmux exited with 1"));
        }
        other => panic!("expected Err, got {other:?}"),
    }
}

#[test]
fn j_and_k_navigate_until_slash_starts_filtering() {
    let mut app = app_with_paneless_and_pane();
    let pane_idx = app
        .rows
        .iter()
        .position(|r| matches!(r, WatchRow::Agent(a) if a.pane.as_deref() == Some("%42")))
        .unwrap();
    app.table_state.select(Some(pane_idx));

    let action = key_action(&mut app, 'k');
    assert!(matches!(action, Action::None));
    assert!(app.search_query.is_empty());

    // Movement keeps `j`/`k` until `/` hands the keyboard to the filter.
    assert!(matches!(key_action(&mut app, '/'), Action::None));
    for c in "erk".chars() {
        assert!(matches!(key_action(&mut app, c), Action::None));
    }
    assert_eq!(app.search_query, "erk");

    app.clear_search();
    assert!(matches!(alt_key_action(&mut app, 'r'), Action::Refresh));
    assert!(matches!(
        alt_key_action(&mut app, 'k'),
        Action::AskConfirm(_)
    ));
}

#[test]
fn alt_i_says_which_way_the_inspector_went() {
    // The inspector starts enabled, so the first Alt-I hides a panel
    // instead of summoning one — and under 120 columns neither state
    // renders anything. Silence made a working binding get reported
    // as "only works on the second press".
    let mut app = three_agent_app(tend::config::DetailConfig::default());
    assert!(app.inspector_enabled, "inspector is on by default");

    assert!(matches!(alt_key_action(&mut app, 'i'), Action::None));
    assert!(!app.inspector_enabled);
    assert_eq!(
        app.footer_hint.as_ref().map(|h| h.message.as_str()),
        Some("inspector disabled")
    );

    assert!(matches!(alt_key_action(&mut app, 'i'), Action::None));
    assert!(app.inspector_enabled);
    assert_eq!(
        app.footer_hint.as_ref().map(|h| h.message.as_str()),
        Some("inspector enabled")
    );
}

#[test]
fn clearing_filter_restores_j_and_k_navigation() {
    let mut app = three_agent_app(tend::config::DetailConfig::default());
    app.table_state.select(Some(0));

    assert!(matches!(key_action(&mut app, 'j'), Action::None));
    assert_eq!(app.selected_pane().as_deref(), Some("%2"));
    assert!(app.search_query.is_empty());

    assert!(matches!(key_action(&mut app, '/'), Action::None));
    for c in "ejk".chars() {
        assert!(matches!(key_action(&mut app, c), Action::None));
    }
    assert_eq!(app.search_query, "ejk");

    app.clear_search();
    assert!(matches!(key_action(&mut app, 'k'), Action::None));
    assert!(app.search_query.is_empty());
}

#[test]
fn slash_filters_and_escape_clears_before_quit() {
    let mut app = three_agent_app(tend::config::DetailConfig::default());
    app.table_state.select(Some(0));

    assert!(matches!(key_action(&mut app, '/'), Action::None));
    for c in "eta".chars() {
        assert!(matches!(key_action(&mut app, c), Action::None));
    }
    assert_eq!(app.search_query, "eta");
    assert_eq!(app.visible_targets().len(), 1);
    assert_eq!(app.selected_pane().as_deref(), Some("%2"));

    let action = handle_event(
        Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        &mut app,
    );
    assert!(matches!(action, Action::None));
    assert!(app.search_query.is_empty());
    assert_eq!(app.visible_targets().len(), 3);

    let action = handle_event(
        Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        &mut app,
    );
    assert!(matches!(action, Action::Quit));
}

#[test]
fn slash_search_accepts_reserved_keys_and_restores_browse_shortcuts() {
    let mut app = three_agent_app(tend::config::DetailConfig::default());

    assert!(matches!(key_action(&mut app, '/'), Action::None));
    assert!(app.explicit_search);
    for c in "qghlro?".chars() {
        assert!(matches!(key_action(&mut app, c), Action::None));
    }
    assert_eq!(app.search_query, "qghlro?");

    let clear = handle_event(
        Event::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
        &mut app,
    );
    assert!(matches!(clear, Action::None));
    assert!(app.search_query.is_empty());
    assert!(!app.explicit_search);
    assert!(matches!(key_action(&mut app, 'q'), Action::Quit));
}

#[test]
fn explicit_search_stays_armed_after_backspacing_to_empty() {
    let mut app = three_agent_app(tend::config::DetailConfig::default());
    assert!(matches!(key_action(&mut app, '/'), Action::None));
    assert!(matches!(key_action(&mut app, 'q'), Action::None));
    assert!(matches!(
        handle_event(
            Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
            &mut app,
        ),
        Action::None
    ));
    assert!(app.search_query.is_empty());
    assert!(app.explicit_search);

    assert!(matches!(key_action(&mut app, 'q'), Action::None));
    assert_eq!(app.search_query, "q");
}

#[test]
fn conventional_browse_keys_cover_boundaries_and_pages() {
    let mut app = three_agent_app(tend::config::DetailConfig::default());
    app.table_page_rows = 2;
    app.table_state.select(Some(0));

    for (key, expected) in [
        (KeyCode::End, 2),
        (KeyCode::Home, 0),
        (KeyCode::PageDown, 2),
        (KeyCode::PageUp, 0),
    ] {
        assert!(matches!(
            handle_event(Event::Key(KeyEvent::new(key, KeyModifiers::NONE)), &mut app,),
            Action::None
        ));
        assert_eq!(app.table_state.selected(), Some(expected));
    }

    assert!(matches!(
        handle_event(
            Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL,)),
            &mut app,
        ),
        Action::None
    ));
    assert_eq!(app.table_state.selected(), Some(1));
    assert!(matches!(
        handle_event(
            Event::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL,)),
            &mut app,
        ),
        Action::None
    ));
    assert_eq!(app.table_state.selected(), Some(0));

    assert!(matches!(key_action(&mut app, 'G'), Action::None));
    assert_eq!(app.table_state.selected(), Some(2));
    assert!(matches!(key_action(&mut app, 'g'), Action::None));
    assert!(app.pending_g);
    assert!(matches!(key_action(&mut app, 'g'), Action::None));
    assert_eq!(app.table_state.selected(), Some(0));
    assert!(!app.pending_g);
}

#[test]
fn empty_filter_exposes_common_single_key_actions() {
    let mut app = three_agent_app(tend::config::DetailConfig::default());
    assert!(matches!(key_action(&mut app, 'r'), Action::Refresh));
    assert!(matches!(key_action(&mut app, 'o'), Action::OpenPreview));
    assert!(matches!(
        key_action(&mut app, '?'),
        Action::Quick(QuickAction::ShowHelp)
    ));

    assert!(matches!(key_action(&mut app, '/'), Action::None));
    for c in ['e', 'r', 'o', 'q', '?'] {
        assert!(matches!(key_action(&mut app, c), Action::None));
    }
    assert_eq!(app.search_query, "eroq?");
}

#[test]
fn command_palette_completes_and_dispatches_safe_actions() {
    let mut app = app_with_paneless_and_pane();
    let pane_idx = app
        .rows
        .iter()
        .position(
            |row| matches!(row, WatchRow::Agent(agent) if agent.pane.as_deref() == Some("%42")),
        )
        .unwrap();
    app.table_state.select(Some(pane_idx));

    assert!(matches!(key_action(&mut app, ':'), Action::None));
    for c in "sort l".chars() {
        assert!(matches!(key_action(&mut app, c), Action::None));
    }
    assert!(matches!(
        handle_event(
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            &mut app,
        ),
        Action::None
    ));
    assert_eq!(
        app.command_palette.as_ref().map(|c| c.input.as_str()),
        Some("sort latest")
    );
    assert!(matches!(
        handle_event(
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            &mut app,
        ),
        Action::SetSort(WatchSortPreset::Latest)
    ));
    assert!(app.command_palette.is_none());

    assert!(matches!(key_action(&mut app, ':'), Action::None));
    for c in "kill".chars() {
        assert!(matches!(key_action(&mut app, c), Action::None));
    }
    assert!(matches!(
        handle_event(
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            &mut app,
        ),
        Action::AskConfirm(_)
    ));
}

#[test]
fn command_view_rebuilds_cached_rows_and_preserves_a_pane_target() {
    let mut app = three_agent_app(tend::config::DetailConfig::default());
    app.table_state.select(Some(1));
    app.paneless_hidden = 2;
    app.paneless_attention = 1;
    let selected = app.selected_pane();

    app.apply_view(WatchView::Window);
    assert!(app.rows.iter().all(|row| matches!(row, WatchRow::Work(_))));
    assert!(app.columns.contains(&WatchColumn::Duration));
    assert_eq!(app.selected_pane(), selected);
    assert_eq!(app.paneless_hidden, 2);
    assert_eq!(app.paneless_attention, 1);

    app.apply_view(WatchView::Pane);
    assert!(app.rows.iter().any(|row| matches!(row, WatchRow::Agent(_))));
    assert_eq!(app.selected_pane(), selected);
}

#[test]
fn command_palette_renders_input_and_matching_suggestions() {
    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = three_agent_app(tend::config::DetailConfig::default());
    app.command_palette = Some(CommandPalette {
        input: "sort".into(),
        cursor: 4,
    });

    terminal.draw(|frame| render(frame, &mut app)).unwrap();
    let screen = (0..terminal.backend().buffer().area().height)
        .map(|y| row_text(terminal.backend().buffer(), y))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(screen.contains("commands · Enter run"));
    assert!(screen.contains(": sort"));
    assert!(screen.contains("sort latest"));
    assert!(screen.contains("sort duration"));
}

#[test]
fn attention_filter_keeps_only_blocked_agents() {
    let mut app = three_agent_app(tend::config::DetailConfig::default());
    let WatchRow::Agent(agent) = &mut app.rows[1] else {
        panic!("expected agent row");
    };
    agent.state = AgentState::WaitingInput;

    assert!(matches!(alt_key_action(&mut app, 'a'), Action::None));
    assert!(app.attention_only);
    assert_eq!(app.visible_targets().len(), 1);
    assert_eq!(app.selected_pane().as_deref(), Some("%2"));
}

#[test]
fn selected_work_auto_expands_and_children_are_exact_action_targets() {
    let mut app = work_preview_app();
    assert_eq!(app.visible_targets().len(), 3);
    assert!(matches!(
        app.selected_identity(),
        Some(RowIdentity::Work(_))
    ));
    let expected_child_pane = match &app.rows[0] {
        WatchRow::Work(session) => session.agents[0].pane.clone(),
        _ => None,
    };
    let expected_second_child_pane = match &app.rows[0] {
        WatchRow::Work(session) => session.agents[1].pane.clone(),
        _ => None,
    };

    let _ = key_action(&mut app, 'l');
    assert_eq!(app.visible_targets().len(), 3);
    assert_eq!(app.selected_pane(), expected_child_pane);
    app.move_down();
    assert_eq!(app.selected_pane(), expected_second_child_pane);
    app.move_down();
    assert_eq!(app.selected_pane(), expected_child_pane);
    let Action::AskConfirm(popup) = quick_kill_action(&app) else {
        panic!("expanded child should be killable");
    };
    assert_eq!(
        popup.on_confirm,
        ConfirmAction::Quick(QuickAction::KillPane(app.selected_pane().unwrap()))
    );

    let _ = key_action(&mut app, 'h');
    assert_eq!(app.visible_targets().len(), 3);
    assert!(matches!(
        app.selected_identity(),
        Some(RowIdentity::Work(_))
    ));
}

#[test]
fn moving_to_another_work_folds_the_previous_and_opens_the_new_one() {
    let cfg = WatchConfig {
        view: WatchView::Window,
        sort: vec![WatchSortKey::Name],
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    let now = OffsetDateTime::now_utc();
    app.set_data_with_sessions(
        vec![
            fake_agent_at("alpha", "%1", now),
            fake_agent_at("beta", "%2", now),
        ],
        vec![
            fake_pane("%1", "alpha", 0, 0, "claude"),
            fake_pane("%3", "alpha", 0, 1, "zsh"),
            fake_pane("%2", "beta", 0, 0, "codex"),
            fake_pane("%4", "beta", 0, 1, "zsh"),
        ],
        vec![
            fake_session("$1", "alpha", 1),
            fake_session("$2", "beta", 1),
        ],
        vec![],
    );

    let initial = app.visible_targets();
    assert_eq!(initial.len(), 3);
    assert_eq!(initial[0].row_idx, 0);
    assert_eq!(initial[1].agent_idx, Some(0));
    assert_eq!(initial[2].row_idx, 1);

    app.move_down();

    let selected = app.selected_target().expect("second work selected");
    assert_eq!(selected.row_idx, 1);
    assert_eq!(selected.agent_idx, None);
    let switched = app.visible_targets();
    assert_eq!(switched.len(), 3);
    assert_eq!(switched[0].row_idx, 0);
    assert_eq!(switched[0].agent_idx, None);
    assert_eq!(switched[1].row_idx, 1);
    assert_eq!(switched[1].agent_idx, None);
    assert_eq!(switched[2].row_idx, 1);
    assert_eq!(switched[2].agent_idx, Some(0));
}

#[test]
fn single_pane_work_keeps_detail_without_a_redundant_child_row() {
    let cfg = WatchConfig {
        view: WatchView::Window,
        sort: vec![WatchSortKey::Name],
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    let now = OffsetDateTime::now_utc();
    let mut agent = fake_agent_at("solo-agent", "%1", now);
    agent.last_response = Some("solo detail remains visible".into());
    app.set_data_with_sessions(
        vec![agent],
        vec![fake_pane("%1", "solo", 0, 0, "codex")],
        vec![fake_session("$1", "solo", 1)],
        vec![],
    );

    assert_eq!(app.visible_targets().len(), 1);
    app.move_into_work();
    assert_eq!(app.visible_targets().len(), 1);
    assert!(matches!(
        app.selected_identity(),
        Some(RowIdentity::Work(_))
    ));

    let backend = TestBackend::new(110, 14);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| render(f, &mut app)).unwrap();
    let dump: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        dump.contains("↳ solo detail remains visible"),
        "single-pane detail missing: {dump:?}"
    );
    let session_line = row_text(terminal.backend().buffer(), 5);
    assert!(
        !session_line.contains('▸') && !session_line.contains('▾'),
        "single pane must not show an expansion marker: {session_line:?}"
    );
}

#[test]
fn single_and_multi_pane_work_names_start_in_the_same_column() {
    let cfg = WatchConfig {
        view: WatchView::Window,
        sort: vec![WatchSortKey::Name],
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    let now = OffsetDateTime::now_utc();
    app.set_data_with_sessions(
        vec![
            fake_agent_at("one-agent", "%1", now),
            fake_agent_at("two-agent", "%2", now),
        ],
        vec![
            fake_pane("%1", "one-pane", 0, 0, "codex"),
            fake_pane("%2", "two-pane", 0, 0, "claude"),
            fake_pane("%3", "two-pane", 0, 1, "zsh"),
        ],
        vec![
            fake_session("$1", "one-pane", 1),
            fake_session("$2", "two-pane", 1),
        ],
        vec![],
    );

    let backend = TestBackend::new(110, 14);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| render(f, &mut app)).unwrap();
    let lines: Vec<String> = (0..terminal.backend().buffer().area.height)
        .map(|y| row_text(terminal.backend().buffer(), y))
        .collect();
    let one = lines
        .iter()
        .find(|line| line.contains("one-pane"))
        .expect("single-pane row");
    let two = lines
        .iter()
        .find(|line| line.contains("two-pane"))
        .expect("multi-pane row");
    assert_eq!(
        one.find("one-pane"),
        two.find("two-pane"),
        "work labels must share one left edge: {one:?} / {two:?}"
    );
}

#[test]
fn selected_work_and_selected_child_both_render_detail() {
    let mut app = work_preview_app();
    let expected_detail = match &app.rows[0] {
        WatchRow::Work(session) => session.agents[0]
            .last_response
            .clone()
            .expect("first child response"),
        _ => panic!("expected work row"),
    };
    let backend = TestBackend::new(140, 16);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal.draw(|f| render(f, &mut app)).unwrap();
    let parent_detail = row_text(terminal.backend().buffer(), 6);
    assert!(
        parent_detail.contains('↳') && parent_detail.contains(&expected_detail),
        "parent detail missing above children: {parent_detail:?}"
    );

    app.move_into_work();
    terminal.draw(|f| render(f, &mut app)).unwrap();
    let child_detail = row_text(terminal.backend().buffer(), 7);
    assert!(
        child_detail.contains('↳') && child_detail.contains(&expected_detail),
        "selected child detail missing: {child_detail:?}"
    );
}

#[test]
fn state_age_cell_shows_state_and_time_in_state() {
    let now = time::macros::datetime!(2026-08-03 12:00:00 UTC);
    let mut agent = fake_agent_at("waiting", "%1", now);
    agent.state = AgentState::WaitingInput;
    agent.state_entered_at = now - time::Duration::minutes(5);
    let text = state_age_text(&agent, now, watch_theme(WatchTheme::Classic), Spinner::OFF);
    assert_eq!(plain_text(&text), "▶ WAIT 5m");
}

#[test]
fn transitions_stay_in_event_inbox_until_opened() {
    let mut app = App::new();
    let working = fake_agent(
        "event-agent",
        Some("%1"),
        AgentKind::ClaudeCode,
        AgentState::Working,
        Some("finish the task"),
        None,
        None,
        None,
    );
    app.set_data(
        vec![working.clone()],
        vec![fake_pane("%1", "main", 0, 0, "claude")],
    );
    app.detect_pulses();

    let mut done = working;
    done.state = AgentState::Idle;
    done.state_entered_at = OffsetDateTime::now_utc();
    apply_outcome(&mut app, RefreshOutcome::SingleAgent(done));

    assert_eq!(app.events.len(), 1);
    assert_eq!(app.events[0].kind, WatchEventKind::Done);
    assert_eq!(app.unread_events, 1);
    app.toggle_event_inbox();
    assert!(app.event_inbox_open);
    assert_eq!(app.unread_events, 0);
}

#[test]
fn footer_hint_set_by_apply_outcome() {
    let mut app = app_with_paneless_and_pane();
    apply_outcome_to_app(&mut app, ActionOutcome::Ok("✔ ok".into()));
    let hint = app.footer_hint.as_ref().expect("hint must be set");
    assert_eq!(hint.message, "✔ ok");
    assert_eq!(hint.level, HintLevel::Ok);
    assert!(hint.fresh());

    apply_outcome_to_app(&mut app, ActionOutcome::Err("✗ bad".into()));
    assert_eq!(app.footer_hint.as_ref().unwrap().level, HintLevel::Err);
}

#[test]
fn confirm_popup_renders_without_panic_and_includes_message() {
    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = app_with_paneless_and_pane();
    app.ask_panel.open = true;
    app.confirm = Some(ConfirmPopup {
        message: "Kill pane main:2.0?".into(),
        on_confirm: ConfirmAction::Quick(QuickAction::KillPane("%42".into())),
    });
    terminal.draw(|f| render(f, &mut app)).unwrap();
    let dump: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        dump.contains("Kill pane main:2.0?"),
        "missing message: {dump:?}"
    );
    assert!(dump.contains("[y]"), "missing [y]: {dump:?}");
    assert!(dump.contains("[N]"), "missing [N]: {dump:?}");
}

#[test]
fn help_popup_renders_without_panic_and_includes_keybindings() {
    // Terminal sized generously so the 60 × 90 % help popup has
    // room for the full keybinding matrix; smaller terminals
    // would clip the bottom sections and the test would fail
    // for cosmetic rather than logical reasons.
    let backend = TestBackend::new(140, 40);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = app_with_paneless_and_pane();
    app.help_open = true;
    terminal.draw(|f| render(f, &mut app)).unwrap();
    let dump: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        dump.contains("Filter & navigation"),
        "missing filter help: {dump:?}"
    );
    assert!(
        dump.contains("Quick actions"),
        "missing Quick actions: {dump:?}",
    );
    assert!(
        dump.contains("close session/window/pane"),
        "missing close hierarchy action: {dump:?}"
    );
}

// ---- visual snapshots --------------------------------------------------
//
// Pin the full rendered buffer as a plain-text snapshot so render
// regressions get caught beyond "did not panic". Helpers strip styling
// (colours don't affect logical layout, and styling deltas would churn
// snapshots) and normalize the relative-time column ("12s" →
// "<rel>") because the production `render` path calls
// `OffsetDateTime::now_utc()` directly and we'd rather not thread an
// injectable clock through it just for tests.
