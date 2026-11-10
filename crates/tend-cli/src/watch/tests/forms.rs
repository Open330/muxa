use super::*;

#[test]
fn n_opens_the_spawn_form_and_enter_launches_with_the_prompt_inline() {
    let mut app = app_with_paneless_and_pane();
    assert!(matches!(key_action(&mut app, 'n'), Action::None));
    let spawn = app.spawn.as_mut().expect("n must open the spawn form");
    // Point at a directory that certainly exists.
    spawn.dir = "/tmp".into();
    spawn.dir_cursor = spawn.dir.chars().count();
    for c in "리뷰해줘".chars() {
        spawn.prompt.push(c);
    }
    spawn.prompt_cursor = spawn.prompt.chars().count();

    // Focus the agent field (Prompt → Dir → Name → Agent) and cycle
    // to codex.
    for _ in 0..3 {
        let _ = handle_spawn_event(KeyCode::Tab, KeyModifiers::NONE, &mut app);
    }
    assert_eq!(app.spawn.as_ref().unwrap().focus, SpawnField::Agent);
    let _ = handle_spawn_event(KeyCode::Right, KeyModifiers::NONE, &mut app);
    assert_eq!(app.spawn.as_ref().unwrap().agent, SpawnAgent::Codex);

    let action = handle_spawn_event(KeyCode::Enter, KeyModifiers::NONE, &mut app);
    assert!(app.spawn.is_none(), "a successful launch closes the form");
    match action {
        Action::Quick(QuickAction::SpawnWork {
            dir,
            agent_label,
            launch,
            work,
        }) => {
            assert_eq!(work, None, "no name typed — derive from dir");
            assert_eq!(dir, "/tmp");
            assert_eq!(agent_label, "codex");
            assert_eq!(
                launch,
                "codex --dangerously-bypass-approvals-and-sandbox '리뷰해줘'"
            );
        }
        other => panic!("expected SpawnWork, got {other:?}"),
    }
}

#[test]
fn paste_while_the_spawn_form_is_open_lands_in_its_field_not_the_filter() {
    let mut app = app_with_paneless_and_pane();
    let _ = key_action(&mut app, 'n');
    let action = handle_event(Event::Paste("/tmp/pasted".into()), &mut app);
    assert!(matches!(action, Action::None));
    assert_eq!(app.spawn.as_ref().unwrap().prompt, "/tmp/pasted");
    assert!(
        app.search_query.is_empty(),
        "paste must not leak into the table filter"
    );
}

#[test]
fn a_typed_work_id_is_preserved_for_tmux_metadata() {
    let mut app = app_with_paneless_and_pane();
    let _ = key_action(&mut app, 'n');
    let spawn = app.spawn.as_mut().unwrap();
    spawn.dir = "/tmp".into();
    spawn.name = "my.review:run".into();
    spawn.prompt = "go".into();
    let action = handle_spawn_event(KeyCode::Enter, KeyModifiers::NONE, &mut app);
    assert!(matches!(
        action,
        Action::Quick(QuickAction::SpawnWork { work: Some(ref s), .. })
            if s == "my.review:run"
    ));
}

#[test]
fn wrapped_input_rows_respect_wide_glyph_widths() {
    // Six Korean chars at width 2 fill a 6-cell row three at a time —
    // char-counted chunking would cram six and overflow the border.
    let rows = wrap_input_rows("가나다라마바", 6);
    assert_eq!(
        rows,
        vec![("가나다".to_string(), 0), ("라마바".to_string(), 3)]
    );
    let rows = wrap_input_rows("abcdef", 4);
    assert_eq!(rows, vec![("abcd".to_string(), 0), ("ef".to_string(), 4)]);
    assert_eq!(wrap_input_rows("", 10), vec![(String::new(), 0)]);
}

#[test]
fn spawn_refuses_an_empty_prompt_and_a_bogus_directory() {
    let mut app = app_with_paneless_and_pane();
    let _ = key_action(&mut app, 'n');
    app.spawn.as_mut().unwrap().dir = "/tmp".into();
    let action = handle_spawn_event(KeyCode::Enter, KeyModifiers::NONE, &mut app);
    assert!(matches!(action, Action::None));
    assert!(
        app.spawn.is_some(),
        "the form must survive a validation miss"
    );

    app.spawn.as_mut().unwrap().prompt = "go".into();
    app.spawn.as_mut().unwrap().dir = "/definitely/not/a/dir".into();
    let action = handle_spawn_event(KeyCode::Enter, KeyModifiers::NONE, &mut app);
    assert!(matches!(action, Action::None));
    assert!(app.spawn.is_some());
}

#[test]
fn derived_work_names_do_not_contain_tmux_separators() {
    assert_eq!(session_base_name("/home/june/my.project:x"), "my-project-x");
}

#[test]
fn shell_single_quote_survives_embedded_quotes() {
    assert_eq!(shell_single_quote("it's"), r"'it'\''s'");
}

#[test]
fn detail_body_wraps_to_the_given_height_instead_of_one_line() {
    let rows = wrap_detail_text("body: ", "가 나 다 라 마 바 사 아 자 차", 10, 3);
    assert!(rows.len() > 1, "a long body must wrap: {rows:?}");
    assert!(rows[0].starts_with("body: "));
    assert!(rows.len() <= 3);
    // Every row must fit the width in *display cells*, not chars —
    // wide glyphs would otherwise spill past the border.
    for row in &rows {
        assert!(
            unicode_width::UnicodeWidthStr::width(row.as_str()) <= 10,
            "row overflows: {row:?}"
        );
    }
    let short = wrap_detail_text("body: ", "hi", 40, 3);
    assert_eq!(short, vec!["body: hi".to_string()]);
}

#[test]
fn detail_text_keeps_the_authors_line_breaks() {
    // A bulleted answer must stay bulleted: collapsing newlines into
    // spaces is what turned every markdown reply into one grey wall.
    let rows = wrap_detail_text("", "- one\n- two\n- three", 40, 6);
    assert_eq!(
        rows,
        vec![
            "- one".to_string(),
            "- two".to_string(),
            "- three".to_string()
        ]
    );

    // Continuation rows align under the prefix so it reads as a gutter.
    let rows = wrap_detail_text("ask: ", "alpha\nbravo", 40, 6);
    assert_eq!(
        rows,
        vec!["ask: alpha".to_string(), "     bravo".to_string()]
    );

    // Runs of blank lines collapse to one — shape without wasting the pane.
    let rows = wrap_detail_text("", "a\n\n\n\nb", 40, 6);
    assert_eq!(rows, vec!["a".to_string(), String::new(), "b".to_string()]);

    // Overflow still ellipsizes the final row.
    let rows = wrap_detail_text("", "1\n2\n3\n4\n5", 40, 3);
    assert_eq!(rows.len(), 3);
    assert!(rows[2].ends_with('…'), "{rows:?}");
}

#[test]
fn narrow_tables_fold_the_summary_column_before_the_name() {
    let cols = vec![
        WatchColumn::Pane,
        WatchColumn::Duration,
        WatchColumn::Activity,
        WatchColumn::Prompt,
    ];
    // Wide enough: everything stays.
    assert_eq!(effective_columns(&cols, 60), cols);
    // Narrow: SUMMARY folds; DUR and ACT survive at any width — six
    // columns each, and part of the row identity.
    assert_eq!(
        effective_columns(&cols, 59),
        vec![
            WatchColumn::Pane,
            WatchColumn::Duration,
            WatchColumn::Activity,
        ]
    );
    assert_eq!(effective_columns(&cols, 30), effective_columns(&cols, 59));
    // A prompt-only layout keeps its one column rather than rendering
    // an empty table.
    assert_eq!(
        effective_columns(&[WatchColumn::Prompt], 10),
        vec![WatchColumn::Prompt]
    );
}

#[test]
fn tab_filters_the_history_and_switches_the_agent_in_the_composer() {
    let mut app = app_with_paneless_and_pane();

    // Panel: Tab cycles the view, and never repoints the next question.
    app.ask_panel.open = true;
    assert_eq!(app.ask_panel.filter, AskFilter::All);
    for expected in [AskFilter::Claude, AskFilter::Codex, AskFilter::All] {
        let action = handle_event(
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            &mut app,
        );
        assert!(matches!(action, Action::None), "got {action:?}");
        assert_eq!(app.ask_panel.filter, expected);
    }
    app.ask_panel.open = false;

    // Composer: Tab is where the target agent is chosen.
    app.ask_composer = Some(AskComposer::default());
    let action = handle_event(
        Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
        &mut app,
    );
    assert!(matches!(action, Action::CycleAskAgent), "got {action:?}");
}

#[test]
fn tab_cycles_durable_ask_conversations_and_filters_the_transcript() {
    let mut app = app_with_paneless_and_pane();
    let now = OffsetDateTime::now_utc();
    let conversation = |id: &str, title: &str| tend::ask::AskConversation {
        id: id.into(),
        title: title.into(),
        agent: "claude".into(),
        agent_session_id: None,
        created_at: now,
        updated_at: now,
    };
    let entry = |id: &str, conversation_id: &str| tend::ask::AskEntry {
        id: id.into(),
        conversation_id: Some(conversation_id.into()),
        prompt: id.into(),
        answer: String::new(),
        status: tend::ask::AskStatus::Running,
        agent: "claude".into(),
        agent_session_id: None,
        cwd: "/tmp".into(),
        asked_at: now,
        answered_at: None,
        cost_usd: None,
        error: None,
        workspace_tools: false,
    };
    app.ask_conversations = vec![
        conversation("newer", "Newer work"),
        conversation("older", "Older work"),
    ];
    app.ask_conversations_available = true;
    app.active_ask_conversation_id = Some("newer".into());
    app.ask_entries = vec![entry("newer-turn", "newer"), entry("older-turn", "older")];
    app.ask_panel.open = true;

    assert_eq!(visible_ask_entries(&app)[0].id, "newer-turn");
    assert!(matches!(
        handle_event(
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            &mut app,
        ),
        Action::SelectAskConversation(id) if id == "older"
    ));
    assert!(matches!(
        handle_event(
            Event::Key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
            &mut app,
        ),
        Action::SelectAskConversation(id) if id == "older"
    ));
}

#[test]
fn ctrl_e_marks_the_draft_new_without_creating_or_discarding_it() {
    let mut app = app_with_paneless_and_pane();
    app.ask_composer = Some(AskComposer {
        input: "independent question".into(),
        cursor: "independent question".chars().count(),
        ..AskComposer::default()
    });

    let action = handle_event(
        Event::Key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL)),
        &mut app,
    );

    assert!(matches!(action, Action::None), "got {action:?}");
    let ask = app.ask_composer.as_ref().expect("composer stays open");
    assert_eq!(ask.input, "independent question");
    assert_eq!(ask.cursor, "independent question".chars().count());
    assert!(ask.new_conversation);
    assert!(matches!(
        handle_ask_composer_event(KeyCode::Enter, KeyModifiers::NONE, &mut app),
        Action::SubmitAsk {
            new_conversation: true
        }
    ));
}

#[test]
fn ask_composer_title_identifies_new_or_continued_conversation() {
    let mut app = app_with_paneless_and_pane();
    let now = OffsetDateTime::now_utc();
    app.ask_conversations = vec![tend::ask::AskConversation {
        id: "selected".into(),
        title: "Release checklist".into(),
        agent: "claude".into(),
        agent_session_id: None,
        created_at: now,
        updated_at: now,
    }];
    app.ask_conversations_available = true;
    app.active_ask_conversation_id = Some("selected".into());
    app.ask_composer = Some(AskComposer::default());

    let title_text = |app: &App| {
        ask_composer_title(app, watch_theme(app.watch_cfg.theme.unwrap_or_default()))
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    };
    let continued = title_text(&app);
    assert!(continued.contains("CONTINUE"), "{continued}");
    assert!(continued.contains("1/1 · Release checklist"), "{continued}");

    assert!(matches!(
        handle_ask_composer_event(KeyCode::Char('e'), KeyModifiers::CONTROL, &mut app),
        Action::None
    ));
    let new = title_text(&app);
    assert!(new.contains("NEW"), "{new}");
    assert!(new.contains("starts when sent"), "{new}");
    assert!(!new.contains("CONTINUE"), "{new}");
    assert!(!new.contains("Release checklist"), "{new}");

    app.ask_composer.as_mut().unwrap().new_conversation = false;
    app.active_ask_conversation_id = None;
    let first = title_text(&app);
    assert!(first.contains("NEW"), "{first}");
}

#[test]
fn n_in_the_ask_panel_opens_a_new_conversation_draft_without_resetting() {
    let mut app = app_with_paneless_and_pane();
    app.ask_panel.open = true;

    let action = handle_event(
        Event::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
        &mut app,
    );

    assert!(matches!(action, Action::None), "got {action:?}");
    assert!(app
        .ask_composer
        .as_ref()
        .is_some_and(|ask| ask.new_conversation && ask.input.is_empty()));
}

#[test]
fn backspace_closes_the_ask_composer_only_when_already_empty() {
    let mut app = app_with_paneless_and_pane();
    app.ask_composer = Some(AskComposer::default());

    assert!(matches!(
        handle_ask_composer_event(KeyCode::Char('x'), KeyModifiers::NONE, &mut app),
        Action::None
    ));
    assert!(matches!(
        handle_ask_composer_event(KeyCode::Backspace, KeyModifiers::NONE, &mut app),
        Action::None
    ));
    assert!(app
        .ask_composer
        .as_ref()
        .is_some_and(|ask| ask.input.is_empty()));

    assert!(matches!(
        handle_ask_composer_event(KeyCode::Backspace, KeyModifiers::NONE, &mut app),
        Action::None
    ));
    assert!(app.ask_composer.is_none());
}

#[test]
fn the_history_filter_selects_by_agent() {
    let mut app = app_with_paneless_and_pane();
    let mk = |agent: &str, prompt: &str| tend::ask::AskEntry {
        conversation_id: None,
        id: format!("ask_{prompt}"),
        prompt: prompt.into(),
        answer: String::new(),
        status: tend::ask::AskStatus::Answered,
        agent: agent.into(),
        agent_session_id: None,
        cwd: "/tmp".into(),
        asked_at: OffsetDateTime::now_utc(),
        answered_at: Some(OffsetDateTime::now_utc()),
        cost_usd: None,
        error: None,
        workspace_tools: false,
    };
    app.ask_entries = vec![mk("claude", "a"), mk("codex", "b"), mk("claude", "c")];

    assert_eq!(visible_ask_entries(&app).len(), 3);
    app.ask_panel.filter = AskFilter::Claude;
    let claude: Vec<_> = visible_ask_entries(&app)
        .iter()
        .map(|e| e.prompt.clone())
        .collect();
    assert_eq!(claude, vec!["a".to_string(), "c".to_string()]);
    app.ask_panel.filter = AskFilter::Codex;
    assert_eq!(visible_ask_entries(&app).len(), 1);
}

#[test]
fn enter_opens_the_full_answer_and_esc_returns_to_the_list() {
    // The panel could only ever show the first screenful of an answer,
    // with no way to reach the rest — the reader is that way.
    let mut app = app_with_long_answer(40);

    assert!(matches!(ask_key(&mut app, KeyCode::Enter), Action::None));
    let reader = app.ask_panel.reader.as_ref().expect("Enter opens a reader");
    assert_eq!(reader.id, "ask_long");
    assert_eq!(reader.scroll, 0, "a reader opens at the top");

    assert!(matches!(ask_key(&mut app, KeyCode::Esc), Action::None));
    assert!(app.ask_panel.reader.is_none(), "Esc steps back one level");
    assert!(app.ask_panel.open, "…without closing the history itself");

    // `A` from inside the reader leaves the panel altogether.
    let _ = ask_key(&mut app, KeyCode::Enter);
    assert!(matches!(
        ask_key(&mut app, KeyCode::Char('A')),
        Action::None
    ));
    assert!(!app.ask_panel.open);
    assert!(app.ask_panel.reader.is_none());
}

#[test]
fn the_reader_swallows_the_delete_keys() {
    // `d` deletes the selected entry from the list. Inside a reader the
    // same keystroke would delete the very answer being read.
    let mut app = app_with_long_answer(40);
    let _ = ask_key(&mut app, KeyCode::Enter);

    for key in ['d', 'D', 'n'] {
        assert!(
            matches!(ask_key(&mut app, KeyCode::Char(key)), Action::None),
            "{key} must not act while the reader is open"
        );
    }
    assert!(app.ask_panel.reader.is_some());
}

#[test]
fn reader_scrolling_clamps_to_the_answer() {
    let mut app = app_with_long_answer(40);
    let _ = ask_key(&mut app, KeyCode::Enter);
    // Stand in for a paint: the renderer is what measures the wrapped
    // body and the viewport, and the key handler clamps against it.
    {
        let reader = app.ask_panel.reader.as_mut().unwrap();
        reader.total_lines = 44;
        reader.view_height = 10;
    }

    let _ = ask_key(&mut app, KeyCode::Char('k'));
    assert_eq!(
        app.ask_panel.reader.as_ref().unwrap().scroll,
        0,
        "no scrolling above the top"
    );

    let _ = ask_key(&mut app, KeyCode::Char('j'));
    assert_eq!(app.ask_panel.reader.as_ref().unwrap().scroll, 1);

    let _ = ask_key(&mut app, KeyCode::PageDown);
    assert_eq!(
        app.ask_panel.reader.as_ref().unwrap().scroll,
        10,
        "a page keeps one line of overlap"
    );

    let _ = ask_key(&mut app, KeyCode::Char('G'));
    assert_eq!(
        app.ask_panel.reader.as_ref().unwrap().scroll,
        34,
        "the last screenful stays full instead of scrolling into blank"
    );
    let _ = ask_key(&mut app, KeyCode::Char('j'));
    assert_eq!(app.ask_panel.reader.as_ref().unwrap().scroll, 34);

    let _ = ask_key(&mut app, KeyCode::Char('g'));
    assert_eq!(app.ask_panel.reader.as_ref().unwrap().scroll, 0);
}

#[test]
fn the_reader_paints_the_answer_and_scrolls_to_its_end() {
    let backend = TestBackend::new(120, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = app_with_long_answer(60);
    let _ = ask_key(&mut app, KeyCode::Enter);

    let dump = |terminal: &Terminal<TestBackend>| -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    };

    terminal.draw(|f| render(f, &mut app)).unwrap();
    let top = dump(&terminal);
    assert!(
        top.contains("explain the reconciler"),
        "the question heads the reader"
    );
    assert!(top.contains("answer line 0"), "missing the answer: {top:?}");
    assert!(
        !top.contains("answer line 59"),
        "the tail must need scrolling, or this proves nothing"
    );

    // The paint measured the body; G can now reach the end of it.
    let _ = ask_key(&mut app, KeyCode::Char('G'));
    terminal.draw(|f| render(f, &mut app)).unwrap();
    let bottom = dump(&terminal);
    assert!(
        bottom.contains("answer line 59"),
        "the end of the answer stayed out of reach: {bottom:?}"
    );
}

#[test]
fn the_reader_falls_back_when_its_entry_disappears() {
    // The history is re-fetched on a timer, so the row under the reader
    // can be deleted by another surface while it is open.
    let backend = TestBackend::new(120, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = app_with_long_answer(20);
    let _ = ask_key(&mut app, KeyCode::Enter);
    app.ask_entries.clear();

    terminal.draw(|f| render(f, &mut app)).unwrap();
    assert!(app.ask_panel.reader.is_none(), "the reader closed itself");
    assert!(app.ask_panel.open, "back to the list, not out of the panel");
    assert!(app
        .footer_hint
        .as_ref()
        .is_some_and(|hint| hint.message.contains("gone")));
}

#[test]
fn ask_history_delete_keys_distinguish_one_from_all() {
    let mut app = app_with_paneless_and_pane();
    let now = OffsetDateTime::now_utc();
    let entry = |id: &str, status: tend::ask::AskStatus| tend::ask::AskEntry {
        conversation_id: None,
        id: id.into(),
        prompt: id.into(),
        answer: String::new(),
        status,
        agent: "claude".into(),
        agent_session_id: None,
        cwd: "/tmp".into(),
        asked_at: now,
        answered_at: (status != tend::ask::AskStatus::Running).then_some(now),
        cost_usd: None,
        error: None,
        workspace_tools: false,
    };
    app.ask_entries = vec![
        entry("done", tend::ask::AskStatus::Answered),
        entry("active", tend::ask::AskStatus::Running),
    ];
    app.ask_panel.open = true;

    let action = handle_event(
        Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)),
        &mut app,
    );
    let Action::AskConfirm(popup) = action else {
        panic!("d must open a confirmation, got {action:?}");
    };
    assert_eq!(
        popup.on_confirm,
        ConfirmAction::DeleteAskHistory("done".into())
    );
    assert!(popup.message.contains("done"));

    app.ask_panel.selected = 1;
    let action = handle_event(
        Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)),
        &mut app,
    );
    assert!(matches!(action, Action::None));
    assert!(app
        .footer_hint
        .as_ref()
        .is_some_and(|hint| hint.message.contains("running")));

    let action = handle_event(
        Event::Key(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT)),
        &mut app,
    );
    let Action::AskConfirm(popup) = action else {
        panic!("D must open a confirmation, got {action:?}");
    };
    assert_eq!(popup.on_confirm, ConfirmAction::ClearAskHistory);
    assert!(popup.message.contains("1 completed"));
    assert!(popup.message.contains("Running asks"));
}

#[test]
fn pipe_cycles_the_inspector_split_and_wraps() {
    let mut app = app_with_paneless_and_pane();
    assert_eq!(app.inspector_split, InspectorSplit::Balanced);
    for expected in [
        InspectorSplit::ListWide,
        InspectorSplit::InspectorWide,
        InspectorSplit::Balanced,
    ] {
        assert!(matches!(
            key_action(&mut app, '|'),
            Action::InspectorSplitChanged
        ));
        assert_eq!(app.inspector_split, expected);
    }
    // The percentages are the contract the render uses.
    assert_eq!(InspectorSplit::ListWide.list_pct(), 70);
    assert_eq!(InspectorSplit::InspectorWide.list_pct(), 30);
}
