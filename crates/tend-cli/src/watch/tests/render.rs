use super::*;

#[test]
fn detail_host_prefers_prompt_then_falls_back_to_last() {
    assert_eq!(
        detail_host_column(&[
            WatchColumn::Pane,
            WatchColumn::Prompt,
            WatchColumn::Activity
        ]),
        Some(1)
    );
    assert_eq!(
        detail_host_column(&[WatchColumn::Pane, WatchColumn::State, WatchColumn::Activity]),
        Some(2)
    );
    assert_eq!(detail_host_column(&[]), None);
}

#[test]
fn format_detail_interpolates_known_vars_and_collapses_lines() {
    let now = OffsetDateTime::now_utc();
    let a = fake_agent(
        "s",
        Some("%1"),
        AgentKind::ClaudeCode,
        AgentState::Working,
        Some("first line\nsecond line"),
        Some("Opus"),
        Some(7.0),
        Some(0.05),
    );
    let row = WatchRow::agent(a);
    let s = format_detail("{model} · {last_prompt}", &row, &[], now).unwrap();
    assert!(s.contains("Opus"));
    assert!(s.contains("first line · second line"));
}

#[test]
fn format_detail_resolves_last_response() {
    let now = OffsetDateTime::now_utc();
    let mut a = fake_agent(
        "s",
        Some("%1"),
        AgentKind::ClaudeCode,
        AgentState::Idle,
        None,
        None,
        None,
        None,
    );
    a.last_response = Some("here is what I did".into());
    let row = WatchRow::agent(a);
    let s = format_detail("{last_response}", &row, &[], now).unwrap();
    assert_eq!(s, "here is what I did");
}

/// The default detail template leads with `last_notification` so a
/// blocked row answers "why does this need me" (the permission / choice
/// text) at a glance, then falls back to response, then prompt.
#[test]
fn default_detail_template_prefers_notification_then_response_then_prompt() {
    let now = OffsetDateTime::now_utc();
    let template = &tend::config::DetailConfig::default().template;

    // Notification present → wins.
    let mut a = fake_agent(
        "s",
        Some("%1"),
        AgentKind::ClaudeCode,
        AgentState::WaitingInput,
        Some("run the migration?"),
        None,
        None,
        None,
    );
    a.last_response = Some("a response".into());
    a.last_notification = Some("approve permission to run rm -rf?".into());
    let row = WatchRow::agent(a);
    assert_eq!(
        format_detail(template, &row, &[], now).unwrap(),
        "approve permission to run rm -rf?"
    );

    // No notification → falls through to the response.
    let mut a = fake_agent(
        "s",
        Some("%1"),
        AgentKind::ClaudeCode,
        AgentState::Working,
        Some("my prompt"),
        None,
        None,
        None,
    );
    a.last_response = Some("assistant reply".into());
    let row = WatchRow::agent(a);
    assert_eq!(
        format_detail(template, &row, &[], now).unwrap(),
        "assistant reply"
    );
}

#[test]
fn format_detail_returns_none_when_only_dashes() {
    let now = OffsetDateTime::now_utc();
    let a = fake_agent(
        "s",
        Some("%1"),
        AgentKind::ClaudeCode,
        AgentState::Idle,
        None,
        None,
        None,
        None,
    );
    let row = WatchRow::agent(a);
    // Default template renders `—` because last_prompt is missing —
    // detail should be suppressed instead of cluttering the row.
    assert!(format_detail("{last_prompt}", &row, &[], now).is_none());
}

#[test]
fn format_detail_falls_back_to_alternative_when_primary_is_dash() {
    // Pipe-separated alternatives should resolve left-to-right and
    // pick the first non-dash variable. This is the path the default
    // `{last_response|last_prompt}` template takes when an agent has
    // submitted a prompt but no `TurnStopped` response has landed —
    // covers the common "tend watch right after typing" case.
    let now = OffsetDateTime::now_utc();
    let mut a = fake_agent(
        "s",
        Some("%1"),
        AgentKind::ClaudeCode,
        AgentState::Working,
        Some("review the diff"),
        None,
        None,
        None,
    );
    a.last_response = None;
    let row = WatchRow::agent(a);
    let s = format_detail("{last_response|last_prompt}", &row, &[], now).unwrap();
    assert_eq!(s, "review the diff");
}

#[test]
fn format_detail_picks_primary_when_present_in_chain() {
    // When the first alternative resolves to a real value, later
    // alternatives are ignored — `last_response` wins over `last_prompt`.
    let now = OffsetDateTime::now_utc();
    let mut a = fake_agent(
        "s",
        Some("%1"),
        AgentKind::ClaudeCode,
        AgentState::Idle,
        Some("the prompt"),
        None,
        None,
        None,
    );
    a.last_response = Some("the response".into());
    let row = WatchRow::agent(a);
    let s = format_detail("{last_response|last_prompt}", &row, &[], now).unwrap();
    assert_eq!(s, "the response");
}

#[test]
fn format_detail_chain_returns_dash_when_all_alternatives_empty() {
    // All alternatives produced placeholder dashes — outer
    // `format_detail` then suppresses the whole detail row, matching
    // the existing fresh-install behaviour. The chain helper itself
    // surfaces a dash (rather than `None`) so the placeholder isn't
    // mistaken for an unknown variable typo.
    let now = OffsetDateTime::now_utc();
    let a = fake_agent(
        "s",
        Some("%1"),
        AgentKind::ClaudeCode,
        AgentState::Idle,
        None,
        None,
        None,
        None,
    );
    let row = WatchRow::agent(a);
    assert!(format_detail("{last_response|last_prompt}", &row, &[], now).is_none());
}

#[test]
fn format_detail_preserves_unknown_placeholder_literal() {
    let now = OffsetDateTime::now_utc();
    let a = fake_agent(
        "s",
        Some("%1"),
        AgentKind::ClaudeCode,
        AgentState::Working,
        Some("hi"),
        None,
        None,
        None,
    );
    let row = WatchRow::agent(a);
    let s = format_detail("{nope} {last_prompt}", &row, &[], now).unwrap();
    assert!(s.contains("{nope}"));
    assert!(s.contains("hi"));
}

#[test]
fn selected_row_renders_with_extra_detail_line() {
    // Render to a TestBackend and assert the detail prefix appears in
    // the buffer for the selected row only. Default template is
    // `{last_response}`, so the agent must have a captured response
    // for the detail line to render (otherwise it suppresses).
    let backend = TestBackend::new(140, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new();
    let mut a1 = fake_agent(
        "s1",
        Some("%1"),
        AgentKind::ClaudeCode,
        AgentState::WaitingInput,
        Some("waiting prompt that is long enough to be visible in the detail line"),
        None,
        None,
        None,
    );
    a1.last_response = Some("the assistant said something".into());
    let a2 = fake_agent(
        "s2",
        Some("%2"),
        AgentKind::ClaudeCode,
        AgentState::Idle,
        Some("another prompt"),
        None,
        None,
        None,
    );
    app.set_data(vec![a1, a2], vec![]);
    terminal.draw(|f| render(f, &mut app)).unwrap();
    let buf = terminal.backend().buffer();
    let text: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    // The ↳ marker is unique to the detail line.
    assert!(text.contains("↳"), "expected detail marker in render");
}

#[test]
fn selected_row_keeps_configured_detail_alongside_workload() {
    let backend = TestBackend::new(110, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new();
    let mut agent = fake_agent(
        "s1",
        Some("%1"),
        AgentKind::ClaudeCode,
        AgentState::Working,
        Some("prompt"),
        None,
        None,
        None,
    );
    agent.last_response = Some("assistant detail remains visible".into());
    agent.workload = tend::WorkloadSummary {
        primary_pid: Some(20),
        process_count: 2,
        shell_count: 1,
        subagent_count: 0,
        helper_count: 0,
        preview: Vec::new(),
    };
    app.set_data(vec![agent], vec![]);

    terminal.draw(|f| render(f, &mut app)).unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();

    assert!(text.contains("↳ tree ▸1 +1"), "missing tree detail: {text}");
    assert!(
        text.contains("assistant detail remains visible"),
        "configured detail must remain visible beside workload detail: {text:?}"
    );
}

#[test]
fn detail_disabled_skips_expansion() {
    let cfg = WatchConfig {
        view: WatchView::Pane,
        detail: tend::config::DetailConfig {
            enabled: false,
            template: "{last_prompt}".into(),
        },
        ..Default::default()
    };
    let backend = TestBackend::new(140, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::with_legacy_config(cfg);
    app.set_data(
        vec![fake_agent(
            "s1",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Idle,
            Some("hello"),
            None,
            None,
            None,
        )],
        vec![],
    );
    terminal.draw(|f| render(f, &mut app)).unwrap();
    let buf = terminal.backend().buffer();
    let text: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(!text.contains("↳"), "detail line must be suppressed");
}

/// Mid-turn case: default config + an agent that has submitted a
/// prompt but not yet captured a response. The default template's
/// fallback (`{last_response|last_prompt}`) must surface the prompt
/// on the detail line so the user sees what's currently in flight
/// instead of an empty row.
#[test]
fn default_template_falls_back_to_last_prompt_when_no_response() {
    let backend = TestBackend::new(140, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new();
    app.set_data(
        vec![fake_agent(
            "s1",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Working,
            Some("a prompt the user just submitted"),
            None,
            None,
            None,
        )],
        vec![],
    );
    terminal.draw(|f| render(f, &mut app)).unwrap();
    let buf = terminal.backend().buffer();
    let text: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        text.contains("↳"),
        "default template must show the prompt as a fallback when last_response is None"
    );
    assert!(
        text.contains("a prompt the user just submitted"),
        "the actual fallback content (last_prompt) must be visible in the rendered buffer"
    );
}

/// Truly-empty case: an agent with neither `last_response` nor
/// `last_prompt`. Both alternatives in the default template resolve
/// to dashes, the suppression rule kicks in, the row stays one line
/// tall and no `↳` glyph appears. Preserves the fresh-install /
/// freshly-discovered-pane behaviour from before the fallback fix.
#[test]
fn default_template_suppresses_detail_when_no_response_or_prompt() {
    let backend = TestBackend::new(140, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new();
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
    terminal.draw(|f| render(f, &mut app)).unwrap();
    let buf = terminal.backend().buffer();
    let text: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        !text.contains("↳"),
        "default template must suppress detail when both last_response and last_prompt are None"
    );
}

// ---- pane=None UX safety net ------------------------------------------

#[test]
fn footer_hints_when_selected_row_has_no_pane() {
    // Build a buffer-backed render with one agent that has no pane
    // (the SDK sub-agent case). Selecting it must put a yellow
    // "no tmux pane — attach unavailable" hint in the footer, where
    // a regular agent would just show the keybinds.
    //
    // Default config hides paneless agents — we explicitly include
    // them here because that's the very thing under test.
    let backend = TestBackend::new(140, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let cfg = WatchConfig {
        view: WatchView::Pane,
        hide_paneless: false,
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    app.set_data(
        vec![fake_agent(
            "s-no-pane",
            None,
            AgentKind::ClaudeCode,
            AgentState::Working,
            Some("a sub-agent prompt"),
            None,
            None,
            None,
        )],
        vec![],
    );
    terminal.draw(|f| render(f, &mut app)).unwrap();
    let dump: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        dump.contains("attach unavailable"),
        "expected pane=None footer hint in render"
    );
    assert!(
        dump.contains("(no pane)"),
        "expected '(no pane)' label in PANE column"
    );
}

#[test]
fn footer_hides_hint_when_selected_row_has_pane() {
    // Sanity check the inverse — a regular agent must NOT show the
    // pane=None hint.
    let backend = TestBackend::new(140, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new();
    app.set_data(
        vec![fake_agent(
            "s",
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
    terminal.draw(|f| render(f, &mut app)).unwrap();
    let dump: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(!dump.contains("attach unavailable"));
    assert!(!dump.contains("(no pane)"));
}

#[test]
fn selected_pane_returns_none_for_no_pane_agent() {
    // include the paneless row under test
    let cfg = WatchConfig {
        view: WatchView::Pane,
        hide_paneless: false,
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    app.set_data(
        vec![fake_agent(
            "s",
            None,
            AgentKind::ClaudeCode,
            AgentState::Working,
            None,
            None,
            None,
            None,
        )],
        vec![],
    );
    // Table-mode Enter resolves through `selected_pane`; returning
    // None here is what lets the input handler surface a "no pane"
    // hint instead of opening the prompt composer.
    assert_eq!(app.selected_pane(), None);
}

/// Default config hides paneless agents so the picker only lists
/// rows the user can actually attach to. The row count must reflect
/// only the pane-bound agent, and `paneless_hidden` must record the
/// count of filtered rows so the footer can surface them.
#[test]
fn hide_paneless_filters_agents_by_default() {
    let mut app = App::new();
    app.set_data(
        vec![
            fake_agent(
                "with-pane",
                Some("%1"),
                AgentKind::ClaudeCode,
                AgentState::Idle,
                None,
                None,
                None,
                None,
            ),
            fake_agent(
                "no-pane",
                None,
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
    assert_eq!(app.rows.len(), 1);
    assert_eq!(app.paneless_hidden, 1);
}

/// `--include-paneless` (or `[watch] hide_paneless = false`) keeps
/// every agent visible and zeroes the filter counter.
#[test]
fn include_paneless_keeps_every_agent() {
    let cfg = WatchConfig {
        view: WatchView::Pane,
        hide_paneless: false,
        ..WatchConfig::default()
    };
    let mut app = App::with_legacy_config(cfg);
    app.set_data(
        vec![
            fake_agent(
                "with-pane",
                Some("%1"),
                AgentKind::ClaudeCode,
                AgentState::Idle,
                None,
                None,
                None,
                None,
            ),
            fake_agent(
                "no-pane",
                None,
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
    assert_eq!(app.rows.len(), 2);
    assert_eq!(app.paneless_hidden, 0);
}

/// Hidden paneless agents that are blocked on a human are tallied into
/// `paneless_attention` so the header can surface them — otherwise a
/// detached agent that goes `WaitingInput` is invisible in the picker.
/// Idle/working paneless agents count toward `paneless_hidden` but not
/// `paneless_attention`.
#[test]
fn paneless_attention_counts_only_blocked_hidden_agents() {
    let mut app = App::new(); // default hides paneless
    app.set_data(
        vec![
            fake_agent(
                "waiting-nopane",
                None,
                AgentKind::ClaudeCode,
                AgentState::WaitingInput,
                None,
                None,
                None,
                None,
            ),
            fake_agent(
                "error-nopane",
                None,
                AgentKind::ClaudeCode,
                AgentState::Error,
                None,
                None,
                None,
                None,
            ),
            fake_agent(
                "idle-nopane",
                None,
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
    assert_eq!(app.paneless_hidden, 3);
    assert_eq!(app.paneless_attention, 2);
}

#[test]
fn a_matching_daemon_raises_no_bar() {
    let app = App::new();
    assert_eq!(skew_banner(&app), None);
}

#[test]
fn a_skewed_daemon_names_both_versions_and_the_remedy() {
    let mut app = App::new();
    app.version_skew = Some(VersionSkew {
        daemon: Some("0.8.35".to_string()),
        client: "0.8.37",
    });
    let banner = skew_banner(&app).expect("a skewed daemon must raise the bar");
    assert!(banner.contains("0.8.35"), "{banner}");
    assert!(banner.contains("0.8.37"), "{banner}");
    assert!(banner.contains(":daemon restart"), "{banner}");
}

#[test]
fn a_protocol_mismatch_alone_still_raises_the_bar() {
    // The loud case: too old to answer `hello`, so there is no version to
    // print — but the remedy is identical and must still be on screen.
    let mut app = App::new();
    app.last_error = Some(DaemonError {
        message: "protocol mismatch: server=4 client=6".to_string(),
        self_describing: false,
    });
    let banner = skew_banner(&app).expect("a mismatch must raise the bar");
    assert!(banner.contains(":daemon restart"), "{banner}");
}

#[test]
fn an_unrelated_daemon_error_keeps_its_own_line() {
    // The bar outranks the error line, so it must not claim errors that
    // have nothing to do with a stale daemon.
    let mut app = App::new();
    app.last_error = Some(DaemonError {
        message: "daemon not reachable at /tmp/tend-501.sock".to_string(),
        self_describing: true,
    });
    assert_eq!(skew_banner(&app), None);
}

#[test]
fn the_skew_bar_replaces_the_error_line_on_screen() {
    let backend = TestBackend::new(140, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new();
    app.version_skew = Some(VersionSkew {
        daemon: Some("0.8.35".to_string()),
        client: "0.8.37",
    });
    app.last_error = Some(DaemonError {
        message: "protocol mismatch: server=4 client=6".to_string(),
        self_describing: false,
    });
    terminal.draw(|f| render(f, &mut app)).unwrap();
    let dump: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(dump.contains("0.8.35"), "{dump}");
    assert!(
        !dump.contains("protocol mismatch"),
        "the bare mismatch text must not take the line from the remedy: {dump}"
    );
}

/// Footer surfaces the hidden-paneless count so the rows aren't
/// silently lost. The hint also tells the user how to reveal them.
#[test]
fn footer_shows_paneless_hidden_count() {
    let backend = TestBackend::new(140, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new(); // default hides paneless
    app.set_data(
        vec![
            fake_agent(
                "with-pane",
                Some("%1"),
                AgentKind::ClaudeCode,
                AgentState::Idle,
                None,
                None,
                None,
                None,
            ),
            fake_agent(
                "no-pane-1",
                None,
                AgentKind::ClaudeCode,
                AgentState::Idle,
                None,
                None,
                None,
                None,
            ),
            fake_agent(
                "no-pane-2",
                None,
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
    terminal.draw(|f| render(f, &mut app)).unwrap();
    let dump: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        dump.contains("+2 paneless"),
        "footer must surface the hidden paneless count",
    );
    assert!(
        dump.contains("--include-paneless"),
        "footer must hint at the override flag",
    );
}

/// Selected row's host column should render exactly 2 visual lines:
/// the original cell on row N, the `↳ <detail>` hint on row N+1.
/// Non-selected rows must remain 1 line tall and no detail glyph
/// should appear on their row.
#[test]
fn detail_line_lands_on_row_below_selection() {
    let backend = TestBackend::new(140, 16);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = three_agent_app(tend::config::DetailConfig::default());
    // Select the middle row so we can probe rows on both sides.
    app.table_state.select(Some(1));
    terminal.draw(|f| render(f, &mut app)).unwrap();
    let buf = terminal.backend().buffer();

    // Layout: header chunk = 3 rows (y=0..2), then table chunk with
    // `Borders::ALL` + a header row -> data rows start at y = 3 + 1
    // (top border) + 1 (header) = y=5.
    // Rows:
    //   y=5  ALPHAresp (row 0, not selected)
    //   y=6  BETAresp  (row 1, selected, 2-line)
    //   y=7  ↳ BETAresp  (detail line — default template is `{last_response}`)
    //   y=8  GAMMAresp (row 2, not selected)
    let r0 = row_text(buf, 5);
    let r1 = row_text(buf, 6);
    let r1_detail = row_text(buf, 7);
    let r2 = row_text(buf, 8);

    assert!(r0.contains("ALPHAresp"), "row 0 missing top text: {r0:?}");
    assert!(!r0.contains("↳"), "row 0 must not carry detail: {r0:?}");
    assert!(
        r1.contains("BETAresp"),
        "selected row missing top text: {r1:?}"
    );
    assert!(
        !r1.contains("↳"),
        "selected row's first line must not be the detail line: {r1:?}"
    );
    assert!(
        r1_detail.contains("↳") && r1_detail.contains("BETAresp"),
        "detail line not on the row directly below the selection: {r1_detail:?}"
    );
    assert!(r2.contains("GAMMAresp"), "row 2 missing top text: {r2:?}");
    assert!(!r2.contains("↳"), "row 2 must not carry detail: {r2:?}");
}

/// When `[watch.detail] enabled = false`, every row should be one
/// visual line — the rows pack against each other with no gap, and
/// no `↳` glyph appears anywhere.
#[test]
fn detail_disabled_keeps_all_rows_at_one_line() {
    let backend = TestBackend::new(140, 16);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = three_agent_app(tend::config::DetailConfig {
        enabled: false,
        template: "{last_prompt}".into(),
    });
    app.table_state.select(Some(1));
    terminal.draw(|f| render(f, &mut app)).unwrap();
    let buf = terminal.backend().buffer();
    // y=5/6/7 should be the three packed rows.
    assert!(row_text(buf, 5).contains("ALPHAresp"));
    assert!(row_text(buf, 6).contains("BETAresp"));
    assert!(row_text(buf, 7).contains("GAMMAresp"));
    // No detail anywhere.
    let dump: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(!dump.contains("↳"));
}

/// `format_detail` output must round-trip through render: even a
/// custom template (referencing a non-prompt var) shows up under
/// the selected row.
#[test]
fn custom_template_renders_below_selection() {
    let detail = tend::config::DetailConfig {
        enabled: true,
        template: "model={model} prompt={last_prompt}".into(),
    };
    let backend = TestBackend::new(160, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = three_agent_app(detail);
    app.table_state.select(Some(0));
    terminal.draw(|f| render(f, &mut app)).unwrap();
    let buf = terminal.backend().buffer();
    // Selected row (idx 0) sits at y=5; detail at y=6.
    let detail_line = row_text(buf, 6);
    assert!(
        detail_line.contains("↳"),
        "expected detail glyph at y=6: {detail_line:?}"
    );
    assert!(
        detail_line.contains("model=Opus"),
        "expected interpolated model: {detail_line:?}"
    );
    assert!(
        detail_line.contains("prompt=ALPHAprompt"),
        "expected interpolated prompt: {detail_line:?}"
    );
}

// ---- detail edge cases -----------------------------------------------

#[test]
fn empty_template_returns_none() {
    let now = OffsetDateTime::now_utc();
    let a = fake_agent(
        "s",
        Some("%1"),
        AgentKind::ClaudeCode,
        AgentState::Working,
        Some("hi"),
        None,
        None,
        None,
    );
    let row = WatchRow::agent(a);
    // Empty after trimming -> suppress.
    assert!(format_detail("", &row, &[], now).is_none());
    assert!(format_detail("   ", &row, &[], now).is_none());
}

#[test]
fn literal_only_template_passes_through_verbatim() {
    let now = OffsetDateTime::now_utc();
    let a = fake_agent(
        "s",
        Some("%1"),
        AgentKind::ClaudeCode,
        AgentState::Working,
        Some("hi"),
        None,
        None,
        None,
    );
    let row = WatchRow::agent(a);
    let s = format_detail("just a literal string", &row, &[], now).unwrap();
    assert_eq!(s, "just a literal string");
}

#[test]
fn unmatched_open_brace_does_not_panic() {
    let now = OffsetDateTime::now_utc();
    let a = fake_agent(
        "s",
        Some("%1"),
        AgentKind::ClaudeCode,
        AgentState::Working,
        Some("hi"),
        None,
        None,
        None,
    );
    let row = WatchRow::agent(a);
    // Trailing `{last_prompt` (no closing `}`) — current behavior
    // emits the literal so the user sees their typo.
    let s = format_detail("oops {last_prompt", &row, &[], now).unwrap();
    assert!(s.contains("oops"));
    assert!(s.contains("{last_prompt"));
}

#[test]
fn truncate_chars_boundary_at_max() {
    // Exactly `max` -> no ellipsis.
    let s_240: String = "a".repeat(240);
    let out = truncate_chars(&s_240, 240);
    assert_eq!(out.chars().count(), 240);
    assert!(!out.ends_with('…'));

    // 241 -> ellipsis appended after 240 chars.
    let s_241: String = "a".repeat(241);
    let out = truncate_chars(&s_241, 240);
    assert_eq!(out.chars().count(), 241);
    assert!(out.ends_with('…'));
}

#[test]
fn truncate_chars_handles_multibyte_and_wide() {
    // 5 wide CJK chars, max 3 -> 3 chars + ellipsis.
    let out = truncate_chars("가나다라마", 3);
    assert_eq!(out.chars().count(), 4);
    assert!(out.ends_with('…'));
    assert!(out.starts_with("가나다"));

    // Emoji (single scalar) — no panic on byte boundaries.
    let out2 = truncate_chars("🦀🦀🦀🦀", 2);
    assert_eq!(out2.chars().count(), 3);
}

#[test]
fn very_long_detail_renders_without_panic() {
    // Long enough to exceed any reasonable terminal width plus the
    // `truncate_chars` ceiling. Default template is `{last_response}`,
    // so the long string lives on the response field.
    let long: String = "x".repeat(2000);
    let cfg = WatchConfig::default();
    let backend = TestBackend::new(80, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::with_legacy_config(cfg);
    let mut a = fake_agent(
        "s1",
        Some("%1"),
        AgentKind::ClaudeCode,
        AgentState::Working,
        Some("hi"),
        None,
        None,
        None,
    );
    a.last_response = Some(long);
    app.set_data(vec![a], vec![]);
    terminal.draw(|f| render(f, &mut app)).unwrap();
    let buf = terminal.backend().buffer();
    // The detail line is capped at 240 chars + ellipsis; we only
    // care that a `↳` glyph still got placed and rendering didn't
    // wrap-around or panic.
    let dump: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(dump.contains("↳"));
}

#[test]
fn bare_pane_detail_resolves_dashes_for_agent_only_vars() {
    // Custom template references `{model}` (agent-only). For a bare
    // pane this should resolve to `—`. With ONLY agent-only vars,
    // the suppress-empty rule does not kick in (since the template
    // has a literal "model=" prefix), so we should still get a
    // detail line — and the rendered content includes the dash.
    let now = OffsetDateTime::now_utc();
    let p = fake_pane("%99", "side", 0, 0, "vim");
    let row = WatchRow::BarePane(Box::new(p));
    let s = format_detail("model={model}", &row, &[], now).unwrap();
    assert_eq!(s, "model=—");
}

#[test]
fn bare_pane_detail_with_only_agent_var_collapses_to_dash() {
    // Template that resolves *purely* to `—` after interpolation —
    // current `format_detail` suppresses this so we don't render
    // a detail line that says nothing useful.
    let now = OffsetDateTime::now_utc();
    let p = fake_pane("%99", "side", 0, 0, "vim");
    let row = WatchRow::BarePane(Box::new(p));
    assert!(format_detail("{model}", &row, &[], now).is_none());
}

#[test]
fn selection_at_last_row_with_short_terminal_does_not_panic() {
    // 8 rows tall: header(3) + table(top border+header+rows...) + footer(1).
    // With 3 agents and the last selected (height=2), the table
    // body needs ~5 rows of content. Terminal is intentionally
    // tight to surface any clipping panics from ratatui.
    let backend = TestBackend::new(120, 9);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = three_agent_app(tend::config::DetailConfig::default());
    app.table_state.select(Some(2));
    // Render twice in case the first frame computes a different
    // viewport offset than the second (TableState retains state).
    terminal.draw(|f| render(f, &mut app)).unwrap();
    terminal.draw(|f| render(f, &mut app)).unwrap();
}

// ---- preview overlay (key `p`) ----------------------------------------
