use super::{
    ask_composer_starts_new_conversation, is_session_target, preview_position,
    selected_tree_target_from, watch_theme, App, CollaborationComposeTarget,
    CollaborationMailboxTab, Color, Frame, HintLevel, Line, MailboxHistoryScope, Modifier,
    Paragraph, PreviewContent, PreviewMode, PreviewState, Rect, Span, Style, TopologyNodeRef,
    TreeTarget, WatchScreen, WatchThemeSpec, WindowNode,
};

pub(super) fn render_footer(
    f: &mut Frame,
    area: Rect,
    app: &App,
    tree_targets: Option<&[TreeTarget]>,
) {
    let theme = watch_theme(app.watch_cfg.theme.unwrap_or_default());
    // Transient action hint takes priority over keybinding strips —
    // the user just pressed a key and wants to see the result. Falls
    // off after `FOOTER_HINT_TTL` so the keybinding strip comes back
    // automatically; we don't bother clearing the slot since the
    // freshness check is cheap.
    if let Some(hint) = app.footer_hint.as_ref() {
        if hint.fresh() {
            let style = match hint.level {
                HintLevel::Ok => Style::default().fg(Color::Green),
                HintLevel::Err => Style::default().fg(Color::Red),
                HintLevel::Warn => Style::default().fg(Color::Yellow),
            };
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(hint.message.clone(), style))),
                area,
            );
            return;
        }
    }

    if render_contextual_footer(f, area, app, theme) {
        return;
    }

    let mut spans = Vec::new();
    // Put row-specific warnings first so they survive footer clipping on
    // narrow popup layouts.
    if selected_has_no_pane(app, tree_targets) {
        spans.push(Span::styled(
            "no tmux pane — attach unavailable",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::DIM | Modifier::ITALIC),
        ));
        spans.push(Span::raw("    "));
    }
    if app.paneless_hidden > 0 {
        spans.push(Span::styled(
            format!(
                "+{} paneless (use --include-paneless to show)",
                app.paneless_hidden
            ),
            theme
                .dim_style()
                .add_modifier(Modifier::DIM | Modifier::ITALIC),
        ));
        spans.push(Span::raw("    "));
    }
    spans.extend([
        Span::styled(" j/k ", theme.key_badge()),
        Span::raw(" move  "),
        Span::styled(" h/l ", theme.key_badge()),
        Span::raw(" tree  "),
        Span::styled(" / ", theme.action_badge()),
        Span::raw(" filter  "),
        Span::styled(" : ", theme.action_badge()),
        Span::raw(" commands  "),
        Span::styled(" ⏎ ", theme.action_badge()),
        Span::raw(" jump  "),
        Span::styled(" o ", theme.key_badge()),
        Span::raw(" preview  "),
        Span::styled(" m ", theme.action_badge()),
        Span::raw(" message  "),
        Span::styled(" Alt-M ", theme.action_badge()),
        Span::raw(" move  "),
        Span::styled(" M ", theme.action_badge()),
        Span::raw(" mailbox  "),
        Span::styled(" ? ", theme.key_badge()),
        Span::raw(" help"),
    ]);
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[allow(clippy::too_many_lines)] // one arm per overlay; a dispatch table
                                 // of closures would hide which key set wins
fn render_contextual_footer(f: &mut Frame, area: Rect, app: &App, theme: WatchThemeSpec) -> bool {
    if app.message_skill_editor.is_some() {
        let spans = vec![
            Span::styled(" Enter ", theme.action_badge()),
            Span::raw("next/save  "),
            Span::styled(" Tab ", theme.key_badge()),
            Span::raw("field  "),
            Span::styled(" Ctrl-V ", theme.key_badge()),
            Span::raw("paste  "),
            Span::styled(" Esc ", theme.key_badge()),
            Span::raw("back"),
        ];
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return true;
    }

    if app.command_palette.is_some() {
        let spans = vec![
            Span::styled(" Enter ", theme.action_badge()),
            Span::raw(" run  "),
            Span::styled(" Tab ", theme.key_badge()),
            Span::raw(" complete  "),
            Span::styled(" Ctrl-W ", theme.key_badge()),
            Span::raw(" delete word  "),
            Span::styled(" Esc ", theme.key_badge()),
            Span::raw(" cancel"),
        ];
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return true;
    }

    if app.ask_composer.is_some() {
        if app
            .ask_composer
            .as_ref()
            .is_some_and(|ask| ask.skill_palette.is_some())
        {
            render_message_skill_palette_footer(f, area, theme);
            return true;
        }
        let new_conversation = ask_composer_starts_new_conversation(app);
        let spans = vec![
            Span::styled(" Enter ", theme.action_badge()),
            Span::raw(if new_conversation {
                "start  "
            } else {
                "continue  "
            }),
            Span::styled(" Tab ", theme.key_badge()),
            Span::raw("agent  "),
            Span::styled(" Ctrl-E ", theme.key_badge()),
            Span::raw("new/continue  "),
            Span::styled(" Ctrl-V ", theme.key_badge()),
            Span::raw("paste  "),
            Span::styled(" / ", theme.key_badge()),
            Span::raw("skills  "),
            Span::styled(" Esc/empty ⌫ ", theme.key_badge()),
            Span::raw("cancel"),
        ];
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return true;
    }

    if app.ask_panel.reader.is_some() {
        let spans = vec![
            Span::styled(" j/k ", theme.key_badge()),
            Span::raw("scroll  "),
            Span::styled(" PgUp/PgDn ", theme.key_badge()),
            Span::raw("page  "),
            Span::styled(" g/G ", theme.key_badge()),
            Span::raw("top/bottom  "),
            Span::styled(" Esc/⏎ ", theme.key_badge()),
            Span::raw("back to the list  "),
            Span::styled(" A ", theme.key_badge()),
            Span::raw("close"),
        ];
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return true;
    }

    if app.ask_panel.open {
        let spans = vec![
            Span::styled(" a ", theme.action_badge()),
            Span::raw("continue  "),
            Span::styled(" n ", theme.action_badge()),
            Span::raw("new conversation  "),
            Span::styled(" Tab/⇧Tab ", theme.key_badge()),
            Span::raw("conversation  "),
            Span::styled(" ⏎ ", theme.action_badge()),
            Span::raw("full answer  "),
            Span::styled(" j/k ", theme.key_badge()),
            Span::raw("select  "),
            Span::styled(" | ", theme.key_badge()),
            Span::raw("detail size  "),
            Span::styled(" Esc/A ", theme.key_badge()),
            Span::raw("close"),
        ];
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return true;
    }

    if app.spawn.is_some() {
        let spans = vec![
            Span::styled(" Enter ", theme.action_badge()),
            Span::raw("launch  "),
            Span::styled(" Tab ", theme.key_badge()),
            Span::raw("field  "),
            Span::styled(" ←/→ ", theme.key_badge()),
            Span::raw("agent  "),
            Span::styled(" Esc ", theme.key_badge()),
            Span::raw("cancel"),
        ];
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return true;
    }

    if app.collaboration_composer.is_some() {
        render_collaboration_composer_footer(f, area, app, theme);
        return true;
    }

    if app.collaboration_mailbox.open {
        if app.collaboration_mailbox.tab == CollaborationMailboxTab::Human {
            f.render_widget(
                Paragraph::new(
                    "j/k select · e reply as operator · Tab agent traffic · r refresh · M close",
                ),
                area,
            );
            return true;
        }
        let aggregate = app
            .collaboration
            .history_scope
            .as_ref()
            .is_some_and(MailboxHistoryScope::aggregate);
        let mut spans = vec![
            Span::styled(" j/k ", theme.key_badge()),
            Span::raw("message  "),
        ];
        if aggregate {
            spans.extend([
                Span::styled(" | ", theme.key_badge()),
                Span::raw("more detail  "),
                Span::styled(" history only ", theme.dim_style()),
            ]);
        } else {
            spans.extend([
                Span::styled(" Tab ", theme.action_badge()),
                Span::raw("inbox/sent  "),
                Span::styled(" i ", theme.action_badge()),
                Span::raw("claim  "),
                Span::styled(" e ", theme.action_badge()),
                Span::raw("reply  "),
                Span::styled(" | ", theme.key_badge()),
                Span::raw("more detail  "),
            ]);
        }
        spans.extend([
            Span::styled(" Esc/M ", theme.key_badge()),
            Span::raw("close"),
        ]);
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return true;
    }

    if let Some(preview) = app.preview.as_ref() {
        render_preview_footer(f, area, app, preview, theme);
        return true;
    }

    if app.explicit_search || !app.search_query.is_empty() {
        let spans = vec![
            Span::styled(" type ", theme.action_badge()),
            Span::raw(" filter  "),
            Span::styled(" Backspace ", theme.key_badge()),
            Span::raw(" edit  "),
            Span::styled(" Ctrl-W ", theme.key_badge()),
            Span::raw(" delete word  "),
            Span::styled(" Ctrl-U/Esc ", theme.key_badge()),
            Span::raw(" clear"),
        ];
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return true;
    }

    if app.pending_g {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" g… ", theme.action_badge()),
                Span::raw("press g for first row · Esc cancels"),
            ])),
            area,
        );
        return true;
    }
    // Last, so every overlay above still owns the footer while it is open.
    // The default strip advertises tree navigation and a preview, none of
    // which this screen has — a request has no children and no pane output.
    if matches!(app.watch_cfg.screen, WatchScreen::Collab) {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" j/k ", theme.key_badge()),
                Span::raw(" move  "),
                Span::styled(" / ", theme.action_badge()),
                Span::raw(" filter  "),
                Span::styled(" : ", theme.action_badge()),
                Span::raw(" commands  "),
                Span::styled(" v ", theme.action_badge()),
                Span::raw(" table/sequence  "),
                Span::styled(" ⏎ ", theme.action_badge()),
                Span::raw(" go to peer  "),
                Span::styled(" Alt-1 ", theme.key_badge()),
                Span::raw(" topology  "),
                Span::styled(" m ", theme.action_badge()),
                Span::raw(" message  "),
                Span::styled(" ? ", theme.key_badge()),
                Span::raw(" help"),
            ])),
            area,
        );
        return true;
    }
    if app.watch_cfg.screen == WatchScreen::Board {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" j/k ", theme.key_badge()),
                Span::raw(" move  "),
                Span::styled(" / ", theme.action_badge()),
                Span::raw(" filter  "),
                Span::styled(" : ", theme.action_badge()),
                Span::raw(" commands  "),
                Span::styled(" ⏎ ", theme.action_badge()),
                Span::raw(" go to Run  "),
                Span::styled(" Tab ", theme.key_badge()),
                Span::raw(" agent  "),
                Span::styled(" p/P ", theme.action_badge()),
                Span::raw(" prompt one/all  "),
                Span::styled(" R/A ", theme.action_badge()),
                Span::raw(" interrupt one/all  "),
                Span::styled(" m/b ", theme.action_badge()),
                Span::raw(" message/mailbox  "),
                Span::styled(" Alt-1 ", theme.key_badge()),
                Span::raw(" topology  "),
                Span::styled(" ? ", theme.key_badge()),
                Span::raw(" help"),
            ])),
            area,
        );
        return true;
    }
    false
}

fn render_collaboration_composer_footer(
    f: &mut Frame,
    area: Rect,
    app: &App,
    theme: WatchThemeSpec,
) {
    let composer = app.collaboration_composer.as_ref();
    if composer.is_some_and(|composer| composer.skill_palette.is_some()) {
        render_message_skill_palette_footer(f, area, theme);
        return;
    }
    let target = composer.map(|composer| &composer.target);
    // The peerless prompt form advertises no Tab/Ctrl-E: both keys only
    // explain why they do nothing, and a footer that lists dead keys
    // teaches the user to stop reading footers.
    let mut spans = vec![
        Span::styled(" Enter ", theme.action_badge()),
        Span::raw("send  "),
    ];
    match target {
        Some(
            CollaborationComposeTarget::Send { .. } | CollaborationComposeTarget::Broadcast { .. },
        ) => {
            spans.extend([
                Span::styled(" Tab ", theme.key_badge()),
                Span::raw("kind  "),
            ]);
        }
        Some(CollaborationComposeTarget::Reply { .. }) => {
            spans.extend([
                Span::styled(" Tab ", theme.key_badge()),
                Span::raw("status  "),
            ]);
        }
        Some(
            CollaborationComposeTarget::Prompt { .. }
            | CollaborationComposeTarget::PromptTopology { .. }
            | CollaborationComposeTarget::PromptPanes { .. },
        )
        | None => {}
    }
    // The modes Ctrl-E cycles through (`composer_cycle_mode`). A message to
    // one exact agent session, or to a set, is always a request, so neither
    // offers just-send.
    let modes = match target {
        Some(CollaborationComposeTarget::Send { target, .. }) if !is_session_target(target) => {
            Some("read-only/execute/just-send  ")
        }
        Some(
            CollaborationComposeTarget::Send { .. } | CollaborationComposeTarget::Broadcast { .. },
        ) => Some("read-only/execute  "),
        _ => None,
    };
    if let Some(modes) = modes {
        spans.extend([
            Span::styled(" Ctrl-E ", theme.key_badge()),
            Span::raw(modes),
        ]);
    }
    spans.extend([
        Span::styled(" / ", theme.key_badge()),
        Span::raw("skills  "),
        Span::styled(" Esc/empty ⌫ ", theme.key_badge()),
        Span::raw("cancel"),
    ]);
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_message_skill_palette_footer(f: &mut Frame, area: Rect, theme: WatchThemeSpec) {
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" Enter ", theme.action_badge()),
            Span::raw("insert  "),
            Span::styled(" ↑/↓/Tab ", theme.key_badge()),
            Span::raw("select  "),
            Span::styled(" F2 ", theme.action_badge()),
            Span::raw("add/update  "),
            Span::styled(" Del ", theme.key_badge()),
            Span::raw("delete  "),
            Span::styled(" Esc/empty ⌫ ", theme.key_badge()),
            Span::raw("back"),
        ])),
        area,
    );
}

fn render_preview_footer(
    f: &mut Frame,
    area: Rect,
    app: &App,
    preview: &PreviewState,
    theme: WatchThemeSpec,
) {
    // Preview mode rebinds the table-mode keybinds to their preview-pane
    // analogues. Toggle labels describe where the next keypress goes.
    let toggle_label = match preview.mode {
        PreviewMode::Popup => " fullscreen  ",
        PreviewMode::Fullscreen => " popup  ",
    };
    let content_label = match preview.content {
        PreviewContent::PromptResponse => " live pane  ",
        PreviewContent::LivePane => " prompt  ",
    };
    let mut spans = vec![
        Span::styled(" ↑/↓ ", theme.key_badge()),
        Span::raw(" scroll  "),
        Span::styled(" PgUp/PgDn ", theme.key_badge()),
        Span::raw(" page  "),
    ];
    if preview_position(app, preview).is_some_and(|(_, total)| total > 1) {
        spans.push(Span::styled(" [ / ] ", theme.key_badge()));
        spans.push(Span::raw(" agent  "));
    }
    spans.extend([
        Span::styled(" f ", theme.key_badge()),
        Span::raw(toggle_label),
        Span::styled(" c ", theme.key_badge()),
        Span::raw(content_label),
        Span::styled(" ⏎ ", theme.action_badge()),
        Span::raw(" jump  "),
        Span::styled(" r ", theme.key_badge()),
        Span::raw(" refresh  "),
        Span::styled(" o/p/q/Esc ", theme.key_badge()),
        Span::raw(" back"),
    ]);
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn selected_has_no_pane(app: &App, tree_targets: Option<&[TreeTarget]>) -> bool {
    if app.uses_tree() {
        let node = tree_targets
            .and_then(|targets| selected_tree_target_from(app, targets))
            .and_then(|target| app.topology.find(&target.key));
        let has_pane = match node {
            Some(TopologyNodeRef::Session(session)) => session
                .active_window()
                .and_then(WindowNode::active_pane)
                .is_some(),
            Some(TopologyNodeRef::Window(window)) => window.active_pane().is_some(),
            Some(TopologyNodeRef::Pane(_)) => true,
            None => false,
        };
        node.is_some() && !has_pane
    } else {
        app.selected_row().is_some() && app.selected_pane().is_none()
    }
}
