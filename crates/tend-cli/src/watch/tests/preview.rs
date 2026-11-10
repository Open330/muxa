use super::*;

#[test]
fn preview_opens_with_alt_p_and_pins_selected_pane_id() {
    let mut app = three_agent_app(tend::config::DetailConfig::default());
    app.table_state.select(Some(1)); // %2

    assert!(app.preview.is_none());
    let action = alt_key_action(&mut app, 'p');
    assert!(matches!(action, Action::OpenPreview));

    // run loop applies OpenPreview by reading selected_pane(); we
    // inline the same effect here.
    if let Action::OpenPreview = action {
        if let Some(pane) = app.selected_pane() {
            app.preview = Some(PreviewState {
                pane_id: pane,
                pane_key: None,
                scroll: 0,
                mode: PreviewMode::Popup,
                content: PreviewContent::PromptResponse,
            });
        }
    }
    assert_eq!(
        app.preview.as_ref().map(|p| p.pane_id.as_str()),
        Some("%2"),
        "preview should pin the pane id of the selected row"
    );
}

#[test]
fn preview_closes_with_q_esc_p_or_o() {
    for key in [
        KeyCode::Char('q'),
        KeyCode::Esc,
        KeyCode::Char('p'),
        KeyCode::Char('o'),
    ] {
        let mut app = three_agent_app(tend::config::DetailConfig::default());
        app.preview = Some(PreviewState {
            pane_id: "%1".into(),
            pane_key: None,
            scroll: 0,
            mode: PreviewMode::Popup,
            content: PreviewContent::PromptResponse,
        });
        let action = handle_event(Event::Key(KeyEvent::new(key, KeyModifiers::NONE)), &mut app);
        assert!(
            matches!(action, Action::ClosePreview),
            "key {key:?} must request ClosePreview while in preview mode"
        );
    }
}

#[test]
fn preview_enter_opens_prompt_for_pinned_pane() {
    let mut app = three_agent_app(tend::config::DetailConfig::default());
    app.panes = vec![fake_pane("%1", "alpha", 0, 0, "claude")];
    // Select a different table row to prove Enter targets the
    // preview-pinned pane, not whatever the table cursor says now.
    app.table_state.select(Some(2));
    app.preview = Some(PreviewState {
        pane_id: "%1".into(),
        pane_key: None,
        scroll: 0,
        mode: PreviewMode::Popup,
        content: PreviewContent::PromptResponse,
    });

    let action = handle_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
    );
    assert!(
        matches!(action, Action::AttachPane(ref pane) if pane == "%1"),
        "preview Enter must attach to the pinned pane, got {action:?}"
    );
}

#[test]
fn preview_arrow_keys_scroll_instead_of_moving_table_cursor() {
    let mut app = three_agent_app(tend::config::DetailConfig::default());
    app.table_state.select(Some(0));
    app.preview = Some(PreviewState {
        pane_id: "%1".into(),
        pane_key: None,
        scroll: 0,
        mode: PreviewMode::Popup,
        content: PreviewContent::PromptResponse,
    });

    // j scrolls down by 1
    let _ = key_action(&mut app, 'j');
    assert_eq!(app.preview.as_ref().unwrap().scroll, 1);
    // PageDown jumps by 10
    let _ = handle_event(
        Event::Key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)),
        &mut app,
    );
    assert_eq!(app.preview.as_ref().unwrap().scroll, 11);
    // Home returns to top
    let _ = handle_event(
        Event::Key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)),
        &mut app,
    );
    assert_eq!(app.preview.as_ref().unwrap().scroll, 0);
    // k saturates at 0 — must not underflow
    let _ = key_action(&mut app, 'k');
    assert_eq!(app.preview.as_ref().unwrap().scroll, 0);
    // Table cursor must NOT have moved during any of the above —
    // arrow keys belong to the preview while it's open.
    assert_eq!(app.table_state.selected(), Some(0));
}

#[test]
fn work_preview_keeps_all_agent_panes_available() {
    let app = work_preview_app();
    let WatchRow::Work(row) = &app.rows[0] else {
        panic!("expected work row");
    };
    assert_eq!(row.representative_pane.as_deref(), Some("%2"));
    assert_eq!(row.agents.len(), 2);
    assert_eq!(
        work_preview_targets(row),
        vec!["%1".to_string(), "%2".to_string()]
    );
    assert_eq!(preview_target_position(&app, "%2"), Some((2, 2)));
}

#[test]
fn preview_brackets_cycle_work_agents_and_reset_cache() {
    let mut app = work_preview_app();
    app.preview = Some(PreviewState {
        pane_id: "%2".into(),
        pane_key: None,
        scroll: 7,
        mode: PreviewMode::Popup,
        content: PreviewContent::LivePane,
    });
    app.pane_capture = Some(CapturedPane::new("%2".into(), None, "old screen".into()));

    let action = handle_event(
        Event::Key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE)),
        &mut app,
    );
    assert!(matches!(action, Action::None));
    let preview = app.preview.as_ref().unwrap();
    assert_eq!(
        preview.pane_id, "%1",
        "] should wrap to the first agent pane"
    );
    assert_eq!(preview.scroll, 0, "agent switch must reset scroll");
    assert!(
        app.pane_capture.is_none(),
        "agent switch must invalidate live capture cache",
    );

    let _ = handle_event(
        Event::Key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE)),
        &mut app,
    );
    assert_eq!(
        app.preview.as_ref().map(|p| p.pane_id.as_str()),
        Some("%2"),
        "[ should wrap back to the previous agent pane",
    );
}

#[test]
fn preview_lines_show_non_representative_work_agent() {
    let app = work_preview_app();
    let lines = build_preview_lines(&app, "%1");
    let dump = lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(dump.contains("ALPHAprompt"), "missing non-latest prompt");
    assert!(dump.contains("ALPHAresp"), "missing non-latest response");
    assert!(
        !dump.contains("BETAprompt"),
        "preview leaked the session representative agent",
    );
}

#[test]
fn preview_lines_show_prompt_response_and_notification_for_active_agent() {
    let mut app = three_agent_app(tend::config::DetailConfig::default());
    // a1 was built with prompt "ALPHAprompt" + response "ALPHAresp".
    let lines = build_preview_lines(&app, "%1");
    let dump = lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(dump.contains("Last prompt:"), "missing prompt heading");
    assert!(dump.contains("ALPHAprompt"), "missing prompt body");
    assert!(dump.contains("Last response:"), "missing response heading");
    assert!(dump.contains("ALPHAresp"), "missing response body");
    // Sanity-check that other agents' content didn't leak into the
    // preview for %1 — pane pinning must isolate.
    assert!(
        !dump.contains("BETAprompt"),
        "preview leaked content from a different agent"
    );

    // With a notification set, that section appears too. Force it on
    // the matching row and re-render.
    if let WatchRow::Agent(a) = &mut app.rows[0] {
        a.last_notification = Some("ready".into());
    }
    let dump2 = build_preview_lines(&app, "%1")
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(dump2.contains("Last notification:"));
    assert!(dump2.contains("ready"));
}

#[test]
fn preview_for_unknown_pane_renders_fallback_message() {
    let app = three_agent_app(tend::config::DetailConfig::default());
    let lines = build_preview_lines(&app, "%does-not-exist");
    let dump = lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        dump.contains("no agent record"),
        "expected fallback hint, got: {dump}"
    );
}

#[test]
fn preview_render_does_not_panic() {
    let backend = TestBackend::new(120, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = three_agent_app(tend::config::DetailConfig::default());
    app.preview = Some(PreviewState {
        pane_id: "%1".into(),
        pane_key: None,
        scroll: 0,
        mode: PreviewMode::Popup,
        content: PreviewContent::PromptResponse,
    });
    terminal.draw(|f| render(f, &mut app)).unwrap();

    // Footer must show the preview-mode hints, not the table-mode
    // ones — `attach` is only meaningful with a row selected.
    let buf = terminal.backend().buffer();
    let text: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        text.contains("scroll"),
        "preview footer must show scroll hint"
    );
    assert!(text.contains("back"), "preview footer must show back hint");
    assert!(
        !text.contains("attach"),
        "preview footer must not show attach hint"
    );
}

#[test]
fn preview_defaults_to_popup_mode_when_opened() {
    // Pressing `p` from the table opens the overlay as a centred
    // popup, not full-screen — keeps surrounding rows visible.
    let mut app = three_agent_app(tend::config::DetailConfig::default());
    app.table_state.select(Some(0));
    let action = alt_key_action(&mut app, 'p');
    assert!(matches!(action, Action::OpenPreview));
    if let (Action::OpenPreview, Some(pane)) = (action, app.selected_pane()) {
        app.preview = Some(PreviewState {
            pane_id: pane,
            pane_key: None,
            scroll: 0,
            mode: PreviewMode::Popup,
            content: PreviewContent::PromptResponse,
        });
    }
    assert_eq!(
        app.preview.as_ref().map(|p| p.mode),
        Some(PreviewMode::Popup)
    );
}

#[test]
fn preview_f_toggles_between_popup_and_fullscreen() {
    let mut app = three_agent_app(tend::config::DetailConfig::default());
    app.preview = Some(PreviewState {
        pane_id: "%1".into(),
        pane_key: None,
        scroll: 0,
        mode: PreviewMode::Popup,
        content: PreviewContent::PromptResponse,
    });

    // First `f` requests TogglePreviewMode; the run loop applies the
    // flip. We mirror that here so the test asserts the end state.
    let action = key_action(&mut app, 'f');
    assert!(matches!(action, Action::TogglePreviewMode));
    if let Some(p) = app.preview.as_mut() {
        p.mode = match p.mode {
            PreviewMode::Popup => PreviewMode::Fullscreen,
            PreviewMode::Fullscreen => PreviewMode::Popup,
        };
    }
    assert_eq!(
        app.preview.as_ref().map(|p| p.mode),
        Some(PreviewMode::Fullscreen),
        "first `f` from popup must enter fullscreen"
    );

    // And `f` again flips back.
    let action = key_action(&mut app, 'f');
    if let (Action::TogglePreviewMode, Some(p)) = (action, app.preview.as_mut()) {
        p.mode = match p.mode {
            PreviewMode::Popup => PreviewMode::Fullscreen,
            PreviewMode::Fullscreen => PreviewMode::Popup,
        };
    }
    assert_eq!(
        app.preview.as_ref().map(|p| p.mode),
        Some(PreviewMode::Popup),
        "second `f` from fullscreen must return to popup"
    );
}

#[test]
fn fullscreen_preview_render_does_not_panic_and_hides_table() {
    // The takeover path: in Fullscreen mode the table is replaced
    // wholesale. We can't easily assert the table is *gone* (the
    // header still mentions agent counts), but rendering must not
    // panic and the preview footer must show the popup-toggle hint.
    let backend = TestBackend::new(140, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = three_agent_app(tend::config::DetailConfig::default());
    app.preview = Some(PreviewState {
        pane_id: "%1".into(),
        pane_key: None,
        scroll: 0,
        mode: PreviewMode::Fullscreen,
        content: PreviewContent::PromptResponse,
    });
    terminal.draw(|f| render(f, &mut app)).unwrap();

    let buf = terminal.backend().buffer();
    let text: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        text.contains("popup"),
        "fullscreen footer must offer popup toggle"
    );
    assert!(text.contains("Last prompt"));
    assert!(text.contains("Last response"));
}

#[test]
fn centered_rect_returns_inner_box_within_parent() {
    // Sanity-check the popup geometry so downstream code can trust
    // the popup never escapes its parent.
    let parent = Rect::new(0, 0, 100, 30);
    let inner = centered_rect(80, 70, parent);

    assert!(inner.x >= parent.x);
    assert!(inner.y >= parent.y);
    assert!(inner.x + inner.width <= parent.x + parent.width);
    assert!(inner.y + inner.height <= parent.y + parent.height);
    // Roughly centred — left margin matches right margin within 1
    // cell to absorb integer-percentage rounding.
    let left = inner.x - parent.x;
    let right = (parent.x + parent.width) - (inner.x + inner.width);
    assert!(
        left.abs_diff(right) <= 1,
        "popup must be horizontally centred (left={left}, right={right})"
    );
}

#[test]
fn bottom_prompt_rect_hugs_parent_bottom() {
    let parent = Rect::new(2, 3, 100, 10);
    assert_eq!(bottom_prompt_rect(parent), Rect::new(2, 10, 100, 3));

    let short = Rect::new(2, 3, 100, 2);
    assert_eq!(bottom_prompt_rect(short), short);

    assert_eq!(
        message_composer_rect(parent, true),
        Rect::new(2, 3, 100, 10)
    );
    assert_eq!(
        message_composer_rect(parent, false),
        bottom_prompt_rect(parent)
    );
}

#[test]
fn c_toggles_preview_content_and_resets_scroll() {
    let mut app = App::with_legacy_config(cfg_with_prompt_default());
    seed_three_agents(&mut app);
    app.table_state.select(Some(0));
    // Open the preview and scroll into the body.
    press(&mut app, 'p');
    app.preview.as_mut().unwrap().scroll = 7;
    assert_eq!(
        app.preview.as_ref().unwrap().content,
        PreviewContent::PromptResponse,
    );

    press(&mut app, 'c');
    let p = app.preview.as_ref().unwrap();
    assert_eq!(p.content, PreviewContent::LivePane);
    assert_eq!(p.scroll, 0, "content toggle must reset scroll");
    assert_eq!(p.mode, PreviewMode::Popup, "geometry must not flip");

    press(&mut app, 'c');
    assert_eq!(
        app.preview.as_ref().unwrap().content,
        PreviewContent::PromptResponse,
        "second `c` must flip back to prompt/response",
    );
}

/// Default `WatchConfig` opens the overlay in `LivePane` mode — this
/// is the headline UX change the `[watch.preview] default_content`
/// option ships with. A future flip of the default would land here
/// loud and clear.
#[test]
fn default_config_opens_preview_in_live_pane() {
    let mut app = three_agent_app(tend::config::DetailConfig::default());
    app.table_state.select(Some(0));
    press(&mut app, 'p');
    let p = app.preview.as_ref().unwrap();
    assert_eq!(
        p.content,
        PreviewContent::LivePane,
        "default config must open preview in LivePane mode",
    );
}

/// Setting `[watch.preview] default_content = "prompt_response"` in
/// config restores the pre-feature shape: `p` opens straight into
/// the text view. Both branches of the config knob must be wired
/// through, not just the default.
#[test]
fn prompt_response_default_opens_preview_in_text_mode() {
    let mut app = App::with_legacy_config(cfg_with_prompt_default());
    seed_three_agents(&mut app);
    app.table_state.select(Some(0));
    press(&mut app, 'p');
    assert_eq!(
        app.preview.as_ref().unwrap().content,
        PreviewContent::PromptResponse,
    );
}

/// `f` (geometry) and `c` (content) are independent — composing them
/// must produce all four combinations without one clobbering the
/// other. This is the user-visible promise of the two-axis design.
#[test]
fn f_and_c_compose_independently() {
    // Pin the starting axis state explicitly so this test reads as
    // "all four (mode, content) combinations are reachable", not
    // "the current default + 3 toggles lands somewhere expected."
    let mut app = App::with_legacy_config(cfg_with_prompt_default());
    seed_three_agents(&mut app);
    app.table_state.select(Some(0));
    press(&mut app, 'p');

    // (Popup, PromptResponse) → press f → (Fullscreen, PromptResponse)
    press(&mut app, 'f');
    let p = app.preview.as_ref().unwrap();
    assert_eq!(p.mode, PreviewMode::Fullscreen);
    assert_eq!(p.content, PreviewContent::PromptResponse);

    // → press c → (Fullscreen, LivePane)
    press(&mut app, 'c');
    let p = app.preview.as_ref().unwrap();
    assert_eq!(p.mode, PreviewMode::Fullscreen);
    assert_eq!(p.content, PreviewContent::LivePane);

    // → press f again → (Popup, LivePane) — content survives
    // geometry flip
    press(&mut app, 'f');
    let p = app.preview.as_ref().unwrap();
    assert_eq!(p.mode, PreviewMode::Popup);
    assert_eq!(p.content, PreviewContent::LivePane);
}

/// Closing the preview must drop the cached pane capture so a stale
/// snapshot doesn't leak into the next preview session (which might
/// be on a different pane that happened to reuse the same `pane_id`
/// across a tmux pane close + recreate).
#[test]
fn close_preview_clears_pane_capture_cache() {
    let mut app = three_agent_app(tend::config::DetailConfig::default());
    app.table_state.select(Some(0));
    press(&mut app, 'p');
    // Stuff the cache by hand — the actual capture path needs a real
    // tmux server, but the close-clears-cache invariant is purely
    // about the App field's lifecycle.
    app.pane_capture = Some(CapturedPane::new("%1".into(), None, "old screen".into()));
    press(&mut app, 'q');
    assert!(app.preview.is_none());
    assert!(app.pane_capture.is_none(), "cache must drop with preview");
}

/// Toggling content from `LivePane` back to `PromptResponse` drops
/// the cache too — re-entering capture mode should fetch a fresh
/// view rather than flash the last cached frame for half a second.
#[test]
fn toggle_to_prompt_drops_cache() {
    // Start from PromptResponse so the toggle direction this test
    // exercises (LivePane → PromptResponse) is unambiguous.
    let mut app = App::with_legacy_config(cfg_with_prompt_default());
    seed_three_agents(&mut app);
    app.table_state.select(Some(0));
    press(&mut app, 'p');
    press(&mut app, 'c'); // → LivePane
    app.pane_capture = Some(CapturedPane::new("%1".into(), None, "old".into()));
    press(&mut app, 'c'); // → PromptResponse
    assert!(
        app.pane_capture.is_none(),
        "leaving capture mode must invalidate cache",
    );
}

/// Capture-mode renderer must not panic on a missing or pane-id-
/// mismatched cache: it returns a placeholder instead. Covers the
/// "(capturing pane…)" first-frame path and the "(pane gone or
/// capture failed)" empty-text path.
#[test]
fn pane_capture_body_falls_back_when_cache_missing_or_empty() {
    let mut app = three_agent_app(tend::config::DetailConfig::default());
    // No cache yet → "(capturing pane…)"
    let body = build_pane_capture_body(&app, "%1");
    let dump: String = body
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect();
    assert!(dump.contains("capturing pane"));

    // Cache present but for a different pane → still placeholder.
    app.pane_capture = Some(CapturedPane::new("%999".into(), None, "irrelevant".into()));
    let body = build_pane_capture_body(&app, "%1");
    let dump: String = body
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect();
    assert!(dump.contains("capturing pane"));

    // Cache present, correct pane, but empty text → pane-gone hint.
    app.pane_capture = Some(CapturedPane::new("%1".into(), None, String::new()));
    let body = build_pane_capture_body(&app, "%1");
    let dump: String = body
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect();
    assert!(
        dump.contains("pane gone") || dump.contains("capture failed"),
        "expected empty-cache placeholder, got {dump:?}",
    );
}

/// Footer in preview mode advertises the `c` content toggle. The
/// label should reflect the *target* state so users see what the
/// next press would do, not where they already are.
#[test]
fn preview_footer_advertises_c_toggle() {
    let backend = TestBackend::new(160, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = three_agent_app(tend::config::DetailConfig::default());
    app.preview = Some(PreviewState {
        pane_id: "%1".into(),
        pane_key: None,
        scroll: 0,
        mode: PreviewMode::Popup,
        content: PreviewContent::PromptResponse,
    });
    terminal.draw(|f| render(f, &mut app)).unwrap();
    let dump: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(dump.contains(" c "), "footer must surface the c key");
    assert!(
        dump.contains("live pane"),
        "footer in PromptResponse mode must hint at flipping to live pane",
    );

    // Flip to LivePane and re-render — label should now point
    // back to "prompt".
    app.preview.as_mut().unwrap().content = PreviewContent::LivePane;
    terminal.draw(|f| render(f, &mut app)).unwrap();
    let dump: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        dump.contains(" prompt"),
        "footer in LivePane mode must hint at flipping back to prompt",
    );
}

// ---- quick actions (K / R / c / ?) ----------------------------------
