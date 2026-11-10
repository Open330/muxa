use super::{
    char_to_byte_idx, collaboration_open_hint, insert_message_skill_prompt, insert_str_at,
    is_ask_running, matching_skills, move_collaboration_mailbox, selected_collaboration_request,
    short_collaboration_request_id, system_clipboard_text, toggle_collaboration_mailbox,
    truncate_chars, validate_message_skill_name, visible_ask_entries, Action, App, AskComposer,
    AskReader, CollaborationComposeDefaults, CollaborationComposeTarget, CollaborationComposer,
    CollaborationMailboxTab, CollaborationOrigin, ComposeSendMode, ConfirmAction, ConfirmPopup,
    HintLevel, KeyCode, KeyModifiers, MailboxHistoryScope, MessageSkillEditor,
    MessageSkillEditorField, MessageSkillPalette, QuickAction, RequestKind, RequestStatus,
    SpawnField, TopologyNodeKey,
};

pub(super) fn handle_rename_event(code: KeyCode, modifiers: KeyModifiers, app: &mut App) -> Action {
    if code == KeyCode::Backspace
        && app
            .rename
            .as_ref()
            .is_some_and(|rename| rename.input.is_empty())
    {
        app.rename = None;
        return Action::None;
    }
    let Some(rename) = app.rename.as_mut() else {
        return Action::None;
    };
    match code {
        KeyCode::Esc => {
            app.rename = None;
            Action::None
        }
        KeyCode::Enter => {
            if rename.input.trim().is_empty() {
                app.set_hint("a name is required", HintLevel::Warn);
                Action::None
            } else if rename.input == rename.original {
                // Submitting the untouched prefill means "never mind".
                app.rename = None;
                Action::None
            } else {
                Action::SubmitRename
            }
        }
        KeyCode::Char('v') if modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(pasted) = system_clipboard_text() {
                insert_str_at(&mut rename.input, &mut rename.cursor, &pasted);
            }
            Action::None
        }
        other => {
            spawn_edit_text(other, modifiers, &mut rename.input, &mut rename.cursor);
            Action::None
        }
    }
}

pub(super) fn handle_work_composer_event(
    code: KeyCode,
    modifiers: KeyModifiers,
    app: &mut App,
) -> Action {
    if code == KeyCode::Backspace
        && app
            .work_composer
            .as_ref()
            .is_some_and(|work| work.input.is_empty())
    {
        app.work_composer = None;
        return Action::None;
    }
    let Some(work) = app.work_composer.as_mut() else {
        return Action::None;
    };
    match code {
        KeyCode::Esc => {
            app.work_composer = None;
            Action::None
        }
        KeyCode::Enter => {
            if work.input.trim().is_empty() {
                app.set_hint("work up needs a work id", HintLevel::Warn);
                Action::None
            } else {
                Action::SubmitWorkUp
            }
        }
        KeyCode::Char('v') if modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(pasted) = system_clipboard_text() {
                insert_str_at(&mut work.input, &mut work.cursor, &pasted);
            }
            Action::None
        }
        other => {
            spawn_edit_text(other, modifiers, &mut work.input, &mut work.cursor);
            Action::None
        }
    }
}

pub(super) fn handle_ask_composer_event(
    code: KeyCode,
    modifiers: KeyModifiers,
    app: &mut App,
) -> Action {
    if app
        .ask_composer
        .as_ref()
        .is_some_and(|ask| ask.skill_palette.is_some())
    {
        return handle_message_skill_palette_event(code, modifiers, app);
    }
    if code == KeyCode::Backspace
        && app
            .ask_composer
            .as_ref()
            .is_some_and(|ask| ask.input.is_empty())
    {
        app.ask_composer = None;
        return Action::None;
    }
    let Some(ask) = app.ask_composer.as_mut() else {
        return Action::None;
    };
    match code {
        KeyCode::Esc => {
            app.ask_composer = None;
            Action::None
        }
        KeyCode::Enter => {
            if ask.input.trim().is_empty() {
                app.set_hint("ask needs a question", HintLevel::Warn);
                Action::None
            } else {
                Action::SubmitAsk {
                    new_conversation: ask.new_conversation,
                }
            }
        }
        // Deciding who to ask belongs here, next to the question — the
        // composer title names the agent, so Tab changes what the title
        // says before Enter commits to it.
        KeyCode::Tab | KeyCode::BackTab => Action::CycleAskAgent,
        // Mode only: no conversation is created until a non-empty draft is
        // submitted. This makes cancelling harmless and lets a busy draft be
        // retried independently without losing its text.
        KeyCode::Char('e') if modifiers.contains(KeyModifiers::CONTROL) => {
            ask.new_conversation = !ask.new_conversation;
            Action::None
        }
        KeyCode::Char('/') if !modifiers.contains(KeyModifiers::CONTROL) => {
            ask.skill_palette = Some(MessageSkillPalette::default());
            Action::None
        }
        KeyCode::Char('v') if modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(pasted) = system_clipboard_text() {
                insert_str_at(&mut ask.input, &mut ask.cursor, &pasted);
            }
            Action::None
        }
        other => {
            spawn_edit_text(other, modifiers, &mut ask.input, &mut ask.cursor);
            Action::None
        }
    }
}

/// Ask conversation panel. `n` drafts the first turn of a new conversation,
/// `d` confirms deleting the selected completed entry, and uppercase `D`
/// confirms clearing all completed history. `r` remains the global refresh.
pub(super) fn handle_ask_panel_event(code: KeyCode, app: &mut App) -> Action {
    if app.ask_panel.reader.is_some() {
        return handle_ask_reader_event(code, app);
    }
    match code {
        KeyCode::Esc | KeyCode::Char('q' | 'A') => {
            app.ask_panel.close();
            Action::None
        }
        // Enter needs no target argument — the row is already selected, and
        // the answer is the thing the panel exists to deliver. `o` mirrors
        // the table's "open whatever is under the cursor".
        KeyCode::Enter | KeyCode::Char('o') => {
            open_ask_reader(app);
            Action::None
        }
        KeyCode::Char('a') => Action::OpenAsk,
        KeyCode::Char('n') => {
            app.ask_composer = Some(AskComposer {
                new_conversation: true,
                ..AskComposer::default()
            });
            Action::None
        }
        KeyCode::Char('d') => {
            let selected = visible_ask_entries(app)
                .get(app.ask_panel.selected)
                .map(|entry| {
                    (
                        entry.id.clone(),
                        entry.prompt.replace('\n', " "),
                        is_ask_running(entry),
                    )
                });
            match selected {
                None => {
                    app.set_hint("ask history: no entry selected", HintLevel::Warn);
                    Action::None
                }
                Some((_, _, true)) => {
                    app.set_hint("running ask history cannot be deleted", HintLevel::Warn);
                    Action::None
                }
                Some((id, prompt, false)) => Action::AskConfirm(ConfirmPopup {
                    message: format!(
                        "Delete this ask history entry? {}",
                        truncate_chars(&prompt, 72)
                    ),
                    on_confirm: ConfirmAction::DeleteAskHistory(id),
                }),
            }
        }
        KeyCode::Char('D') => {
            let completed = app
                .ask_entries
                .iter()
                .filter(|entry| !is_ask_running(entry))
                .count();
            if completed == 0 {
                app.set_hint(
                    "ask history: no completed entries to clear",
                    HintLevel::Warn,
                );
                Action::None
            } else {
                Action::AskConfirm(ConfirmPopup {
                    message: format!(
                        "Clear all {completed} completed ask history entr{}? Running asks and conversation threads will be kept.",
                        if completed == 1 { "y" } else { "ies" }
                    ),
                    on_confirm: ConfirmAction::ClearAskHistory,
                })
            }
        }
        // Tab, not the arrows: the arrows stay turn navigation while Tab
        // changes the conversation whose transcript is on screen. Selecting
        // one intentionally restores its provider too, exactly as resuming
        // that conversation requires; the composer still owns choosing a
        // provider for brand-new work.
        KeyCode::Tab | KeyCode::BackTab if app.ask_conversations_available => {
            adjacent_ask_conversation_id(app, code == KeyCode::Tab)
                .map_or(Action::None, Action::SelectAskConversation)
        }
        // Compatibility fallback for a daemon from before durable
        // conversations: keep the old provider-filtered history navigation.
        KeyCode::Tab | KeyCode::BackTab => {
            app.ask_panel.filter = app.ask_panel.filter.next();
            app.ask_panel.selected = 0;
            let label = app.ask_panel.filter.label();
            app.set_hint(format!("ask history: {label}"), HintLevel::Ok);
            Action::None
        }
        KeyCode::Char('|') => {
            app.ask_panel.detail = app.ask_panel.detail.next();
            let label = app.ask_panel.detail.label();
            app.set_hint(format!("ask detail: {label}"), HintLevel::Ok);
            Action::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let last = visible_ask_entries(app).len().saturating_sub(1);
            app.ask_panel.selected = (app.ask_panel.selected + 1).min(last);
            Action::None
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.ask_panel.selected = app.ask_panel.selected.saturating_sub(1);
            Action::None
        }
        _ => Action::None,
    }
}

/// Pick the adjacent durable conversation. The daemon list is newest first;
/// Tab walks toward older work and Shift-Tab walks back toward newer work.
fn adjacent_ask_conversation_id(app: &App, forward: bool) -> Option<String> {
    let conversations = &app.ask_conversations;
    if conversations.is_empty() {
        return None;
    }
    let current = app.active_ask_conversation_id.as_ref().and_then(|id| {
        conversations
            .iter()
            .position(|conversation| &conversation.id == id)
    });
    let next = match (current, forward) {
        (None, _) => 0,
        (Some(current), true) => (current + 1) % conversations.len(),
        (Some(current), false) => current.checked_sub(1).unwrap_or(conversations.len() - 1),
    };
    conversations
        .get(next)
        .map(|conversation| conversation.id.clone())
}

/// Pin the selected row's answer into the reader.
fn open_ask_reader(app: &mut App) {
    let Some(id) = visible_ask_entries(app)
        .get(app.ask_panel.selected)
        .map(|entry| entry.id.clone())
    else {
        app.set_hint("ask history: no entry selected", HintLevel::Warn);
        return;
    };
    app.ask_panel.reader = Some(AskReader {
        id,
        ..AskReader::default()
    });
}

/// The reader owns every key while it is open. It is a reading surface,
/// and `d`/`D` deleting the entry on screen from under the cursor is
/// exactly the surprise a reading surface must not have.
fn handle_ask_reader_event(code: KeyCode, app: &mut App) -> Action {
    match code {
        // Esc/Enter/q step back to the list; `A` closes the panel outright,
        // the same key that would have closed it from the list.
        KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q' | 'o') => {
            app.ask_panel.reader = None;
            return Action::None;
        }
        KeyCode::Char('A') => {
            app.ask_panel.close();
            return Action::None;
        }
        _ => {}
    }
    let Some(reader) = app.ask_panel.reader.as_mut() else {
        return Action::None;
    };
    let page = reader.page();
    match code {
        KeyCode::Down | KeyCode::Char('j') => reader.scroll_by(1),
        KeyCode::Up | KeyCode::Char('k') => reader.scroll_by(-1),
        KeyCode::PageDown | KeyCode::Char(' ') => reader.scroll_by(page),
        KeyCode::PageUp => reader.scroll_by(-page),
        KeyCode::Home | KeyCode::Char('g') => reader.scroll = 0,
        KeyCode::End | KeyCode::Char('G') => reader.scroll = reader.max_scroll(),
        _ => {}
    }
    Action::None
}

/// The spawn form: three fields, Tab-ordered; `Left`/`Right` cycle the
/// agent when it has focus and move the text cursor otherwise. Enter
/// launches from any field so a finished thought never needs a Tab first.
pub(super) fn handle_spawn_event(code: KeyCode, modifiers: KeyModifiers, app: &mut App) -> Action {
    let Some(spawn) = app.spawn.as_mut() else {
        return Action::None;
    };
    match code {
        KeyCode::Esc => {
            app.spawn = None;
            Action::None
        }
        KeyCode::Tab | KeyCode::Down => {
            spawn.focus = spawn.next_focus();
            Action::None
        }
        KeyCode::BackTab | KeyCode::Up => {
            spawn.focus = spawn.previous_focus();
            Action::None
        }
        KeyCode::Char('v') if modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(pasted) = system_clipboard_text() {
                if let Some((text, cursor)) = spawn.field_mut() {
                    insert_str_at(text, cursor, &pasted);
                }
            }
            Action::None
        }
        KeyCode::Left if spawn.focus == SpawnField::Agent => {
            spawn.agent = spawn.agent.prev();
            Action::None
        }
        KeyCode::Right if spawn.focus == SpawnField::Agent => {
            spawn.agent = spawn.agent.next();
            Action::None
        }
        KeyCode::Enter => {
            let dir = spawn.dir.trim().to_string();
            let prompt = spawn.prompt.trim().to_string();
            if dir.is_empty() || prompt.is_empty() {
                app.set_hint(
                    "spawn needs a directory and a first prompt",
                    HintLevel::Warn,
                );
                return Action::None;
            }
            let dir = if let Some(rest) = dir.strip_prefix("~/") {
                dirs::home_dir()
                    .map(|h| h.join(rest).display().to_string())
                    .unwrap_or(dir)
            } else if dir == "~" {
                dirs::home_dir()
                    .map(|h| h.display().to_string())
                    .unwrap_or(dir)
            } else {
                dir
            };
            if !std::path::Path::new(&dir).is_dir() {
                app.set_hint(format!("not a directory: {dir}"), HintLevel::Err);
                return Action::None;
            }
            let agent = spawn.agent;
            let name = spawn.name.trim().to_string();
            if matches!(spawn.target, Some(TopologyNodeKey::Session(_))) && name.is_empty() {
                app.set_hint("new session window needs a window name", HintLevel::Warn);
                return Action::None;
            }
            let work = (!name.is_empty()).then_some(name);
            let target = spawn.target.clone();
            app.spawn = None;
            let launch = agent.launch_command(&prompt);
            match target {
                Some(target) => Action::Quick(QuickAction::SpawnAt {
                    target,
                    launch,
                    agent_label: agent.label(),
                    dir,
                    name: work.unwrap_or_default(),
                }),
                None => Action::Quick(QuickAction::SpawnWork {
                    launch,
                    agent_label: agent.label(),
                    dir,
                    work,
                }),
            }
        }
        other => {
            if let Some((text, cursor)) = spawn.field_mut() {
                spawn_edit_text(other, modifiers, text, cursor);
            }
            Action::None
        }
    }
}

/// UTF-8-safe single-line editing shared by the spawn form's text fields.
pub(super) fn spawn_edit_text(
    code: KeyCode,
    modifiers: KeyModifiers,
    text: &mut String,
    cursor: &mut usize,
) {
    match code {
        KeyCode::Char(c) if !modifiers.contains(KeyModifiers::CONTROL) => {
            let idx = char_to_byte_idx(text, *cursor);
            text.insert(idx, c);
            *cursor += 1;
        }
        KeyCode::Backspace if *cursor > 0 => {
            let start = char_to_byte_idx(text, *cursor - 1);
            let end = char_to_byte_idx(text, *cursor);
            text.replace_range(start..end, "");
            *cursor -= 1;
        }
        KeyCode::Left => *cursor = cursor.saturating_sub(1),
        KeyCode::Right => *cursor = (*cursor + 1).min(text.chars().count()),
        KeyCode::Home => *cursor = 0,
        KeyCode::End => *cursor = text.chars().count(),
        _ => {}
    }
}

/// Resolve Tab inside the composer: cycle the per-target option, or say why
/// there is nothing to cycle. Returns whether persisted send defaults changed.
fn composer_cycle_option(app: &mut App) -> bool {
    let Some(composer) = app.collaboration_composer.as_mut() else {
        return false;
    };
    let mut hint = None;
    let changed = match &mut composer.target {
        // Kind is a request concept; in `just send` mode there is no
        // request, and silently cycling a hidden badge would surprise
        // whoever switches back.
        CollaborationComposeTarget::Send {
            mode: ComposeSendMode::JustSend,
            ..
        } => {
            hint = Some("kind applies to requests — Ctrl-E to leave just-send");
            None
        }
        CollaborationComposeTarget::Send { kind, mode, .. }
        | CollaborationComposeTarget::Broadcast { kind, mode, .. } => {
            *kind = match *kind {
                RequestKind::Question => RequestKind::Review,
                RequestKind::Review => RequestKind::Task,
                RequestKind::Task => RequestKind::Notice,
                RequestKind::Notice => RequestKind::Question,
            };
            Some(CollaborationComposeDefaults {
                kind: *kind,
                mode: *mode,
            })
        }
        CollaborationComposeTarget::Reply { status, .. } => {
            *status = match *status {
                RequestStatus::Completed => RequestStatus::Blocked,
                RequestStatus::Blocked => RequestStatus::Declined,
                RequestStatus::Declined => RequestStatus::Failed,
                _ => RequestStatus::Completed,
            };
            None
        }
        CollaborationComposeTarget::PromptPanes { .. } => {
            hint = Some("a prompt to every agent of a Work is keystrokes only");
            None
        }
        CollaborationComposeTarget::Prompt { .. }
        | CollaborationComposeTarget::PromptTopology { .. } => {
            hint = Some("requests need a same-window peer — keystrokes only here");
            None
        }
    };
    if let Some(message) = hint {
        app.set_hint(message, HintLevel::Warn);
    }
    if let Some(defaults) = changed {
        app.collaboration_compose_defaults = defaults;
        app.watch_cfg.collaboration_kind = Some(defaults.kind);
        app.watch_cfg.collaboration_mode = Some(defaults.mode.into());
        true
    } else {
        false
    }
}

/// Cycle Ctrl-E's delivery mode and remember it for the next composer.
pub(super) fn composer_cycle_mode(app: &mut App) -> bool {
    let changed = match app.collaboration_composer.as_mut().map(|c| &mut c.target) {
        Some(CollaborationComposeTarget::Send {
            kind, mode, target, ..
        }) => {
            *mode = match *mode {
                ComposeSendMode::ReadOnly => ComposeSendMode::Execute,
                // A `session:` recipient is a request to one agent session;
                // keystrokes have no session to check, so they are skipped.
                ComposeSendMode::Execute if is_session_target(target) => ComposeSendMode::ReadOnly,
                ComposeSendMode::Execute => ComposeSendMode::JustSend,
                ComposeSendMode::JustSend => ComposeSendMode::ReadOnly,
            };
            Some(CollaborationComposeDefaults {
                kind: *kind,
                mode: *mode,
            })
        }
        Some(CollaborationComposeTarget::Broadcast { kind, mode, .. }) => {
            // Two modes, not three: keystrokes cannot address a set, so the
            // cycle skips just-send rather than offering a state Enter would
            // then refuse.
            *mode = match *mode {
                ComposeSendMode::Execute => ComposeSendMode::ReadOnly,
                ComposeSendMode::ReadOnly | ComposeSendMode::JustSend => ComposeSendMode::Execute,
            };
            Some(CollaborationComposeDefaults {
                kind: *kind,
                mode: *mode,
            })
        }
        Some(
            CollaborationComposeTarget::Prompt { .. }
            | CollaborationComposeTarget::PromptTopology { .. }
            | CollaborationComposeTarget::PromptPanes { .. },
        ) => {
            app.set_hint(
                "requests need a same-window peer — keystrokes only here",
                HintLevel::Warn,
            );
            None
        }
        _ => None,
    };
    if let Some(defaults) = changed {
        app.collaboration_compose_defaults = defaults;
        app.watch_cfg.collaboration_kind = Some(defaults.kind);
        app.watch_cfg.collaboration_mode = Some(defaults.mode.into());
        true
    } else {
        false
    }
}

pub(super) fn is_session_target(target: &str) -> bool {
    target.starts_with("session:")
}

/// Resolve Enter inside the composer.
///
/// `just send` bypasses the mailbox entirely: the text goes into the pane
/// as keystrokes, through the same Quick dispatch the old prompt popup
/// used. The composer is taken here (rather than in a run-loop arm) so
/// submit stays a single code path per mode.
pub(super) fn composer_submit_action(app: &mut App) -> Action {
    let empty = app
        .collaboration_composer
        .as_ref()
        .is_none_or(|composer| composer.input.trim().is_empty());
    if empty {
        app.set_hint("message cannot be empty", HintLevel::Warn);
        return Action::None;
    }
    // Refused before the composer is taken, so the draft survives.
    if matches!(
        app.collaboration_composer.as_ref().map(|c| &c.target),
        Some(CollaborationComposeTarget::Send {
            mode: ComposeSendMode::JustSend,
            target,
            ..
        }) if is_session_target(target)
    ) {
        app.set_hint(
            "a message to one agent session is a request — use p for keystrokes",
            HintLevel::Warn,
        );
        return Action::None;
    }
    match app.collaboration_composer.take() {
        Some(CollaborationComposer {
            target: CollaborationComposeTarget::PromptTopology { pane },
            input,
            ..
        }) => Action::Quick(QuickAction::SendPromptTo { pane, text: input }),
        Some(CollaborationComposer {
            target: CollaborationComposeTarget::PromptPanes { panes, unavailable },
            input,
            ..
        }) => Action::Quick(QuickAction::SendPromptToMany {
            panes,
            unavailable,
            text: input,
        }),
        Some(CollaborationComposer {
            target:
                CollaborationComposeTarget::Send {
                    pane,
                    pane_key,
                    mode: ComposeSendMode::JustSend,
                    ..
                },
            input,
            ..
        }) => match pane_key {
            Some(pane) => Action::Quick(QuickAction::SendPromptTo { pane, text: input }),
            None => Action::Quick(QuickAction::SendPrompt {
                pane_id: pane,
                text: input,
            }),
        },
        Some(CollaborationComposer {
            target: CollaborationComposeTarget::Prompt { pane },
            input,
            ..
        }) => Action::Quick(QuickAction::SendPrompt {
            pane_id: pane,
            text: input,
        }),
        other => {
            app.collaboration_composer = other;
            Action::SubmitCollaboration
        }
    }
}

fn active_message_skill_palette(app: &App) -> Option<&MessageSkillPalette> {
    app.collaboration_composer
        .as_ref()
        .and_then(|composer| composer.skill_palette.as_ref())
        .or_else(|| {
            app.ask_composer
                .as_ref()
                .and_then(|ask| ask.skill_palette.as_ref())
        })
}

fn active_message_skill_palette_mut(app: &mut App) -> Option<&mut MessageSkillPalette> {
    if app
        .collaboration_composer
        .as_ref()
        .is_some_and(|composer| composer.skill_palette.is_some())
    {
        return app
            .collaboration_composer
            .as_mut()
            .and_then(|composer| composer.skill_palette.as_mut());
    }
    app.ask_composer
        .as_mut()
        .and_then(|ask| ask.skill_palette.as_mut())
}

fn close_active_message_skill_palette(app: &mut App) {
    if let Some(composer) = app.collaboration_composer.as_mut() {
        if composer.skill_palette.is_some() {
            composer.skill_palette = None;
            return;
        }
    }
    if let Some(ask) = app.ask_composer.as_mut() {
        ask.skill_palette = None;
    }
}

pub(super) fn focus_active_message_skill(app: &mut App, name: &str) {
    if let Some(palette) = active_message_skill_palette_mut(app) {
        palette.query = name.to_string();
        palette.selected = 0;
    }
}

pub(super) fn clamp_active_message_skill(app: &mut App) {
    if let Some(palette) = app
        .collaboration_composer
        .as_mut()
        .and_then(|composer| composer.skill_palette.as_mut())
    {
        palette.move_selection(0, &app.message_skills);
    } else if let Some(palette) = app
        .ask_composer
        .as_mut()
        .and_then(|ask| ask.skill_palette.as_mut())
    {
        palette.move_selection(0, &app.message_skills);
    }
}

fn move_message_skill_selection(app: &mut App, delta: isize) {
    if let Some(palette) = app
        .collaboration_composer
        .as_mut()
        .and_then(|composer| composer.skill_palette.as_mut())
    {
        palette.move_selection(delta, &app.message_skills);
    } else if let Some(palette) = app
        .ask_composer
        .as_mut()
        .and_then(|ask| ask.skill_palette.as_mut())
    {
        palette.move_selection(delta, &app.message_skills);
    }
}

fn selected_message_skill(app: &App) -> Option<(String, String)> {
    let palette = active_message_skill_palette(app)?;
    matching_skills(&app.message_skills, &palette.query)
        .get(palette.selected)
        .map(|(name, prompt)| ((*name).clone(), (*prompt).clone()))
}

fn insert_into_active_composer(app: &mut App, prompt: &str) {
    if app
        .collaboration_composer
        .as_ref()
        .is_some_and(|composer| composer.skill_palette.is_some())
    {
        if let Some(composer) = app.collaboration_composer.as_mut() {
            insert_message_skill_prompt(&mut composer.input, &mut composer.cursor, prompt);
            composer.skill_palette = None;
        }
        return;
    }
    if let Some(ask) = app.ask_composer.as_mut() {
        insert_message_skill_prompt(&mut ask.input, &mut ask.cursor, prompt);
        ask.skill_palette = None;
    }
}

pub(super) fn handle_message_skill_editor_event(
    code: KeyCode,
    modifiers: KeyModifiers,
    app: &mut App,
) -> Action {
    match code {
        KeyCode::Esc => {
            app.message_skill_editor = None;
            Action::None
        }
        KeyCode::Tab | KeyCode::BackTab => {
            if let Some(editor) = app.message_skill_editor.as_mut() {
                editor.cycle_field();
            }
            Action::None
        }
        KeyCode::Enter => {
            let Some(editor) = app.message_skill_editor.as_mut() else {
                return Action::None;
            };
            if editor.field == MessageSkillEditorField::Name {
                match validate_message_skill_name(&editor.name) {
                    Ok(()) => editor.field = MessageSkillEditorField::Prompt,
                    Err(error) => app.set_hint(error.to_string(), HintLevel::Warn),
                }
                return Action::None;
            }
            if editor.prompt.trim().is_empty() {
                app.set_hint("skill prompt cannot be empty", HintLevel::Warn);
                return Action::None;
            }
            Action::SaveMessageSkill {
                name: editor.name.clone(),
                prompt: editor.prompt.clone(),
            }
        }
        KeyCode::Char('v') if modifiers.contains(KeyModifiers::CONTROL) => {
            if let (Some(pasted), Some(editor)) =
                (system_clipboard_text(), app.message_skill_editor.as_mut())
            {
                let pasted = pasted.replace(['\r', '\n'], " ");
                let (text, cursor) = editor.field_mut();
                insert_str_at(text, cursor, &pasted);
            }
            Action::None
        }
        other => {
            if let Some(editor) = app.message_skill_editor.as_mut() {
                let (text, cursor) = editor.field_mut();
                spawn_edit_text(other, modifiers, text, cursor);
            }
            Action::None
        }
    }
}

pub(super) fn handle_message_skill_palette_event(
    code: KeyCode,
    modifiers: KeyModifiers,
    app: &mut App,
) -> Action {
    let control = modifiers.contains(KeyModifiers::CONTROL);
    if code == KeyCode::F(2) || (code == KeyCode::Char('a') && control) {
        app.message_skill_editor = Some(MessageSkillEditor::default());
        return Action::None;
    }
    if code == KeyCode::Delete || (code == KeyCode::Char('d') && control) {
        if let Some((name, _)) = selected_message_skill(app) {
            return Action::AskConfirm(ConfirmPopup {
                message: format!("Delete message skill /{name}?"),
                on_confirm: ConfirmAction::DeleteMessageSkill(name),
            });
        }
        app.set_hint("no message skill selected", HintLevel::Warn);
        return Action::None;
    }

    match code {
        KeyCode::Esc => {
            close_active_message_skill_palette(app);
        }
        KeyCode::Backspace => {
            let close =
                active_message_skill_palette(app).is_some_and(|palette| palette.query.is_empty());
            if close {
                close_active_message_skill_palette(app);
            } else if let Some(palette) = active_message_skill_palette_mut(app) {
                palette.backspace();
            }
        }
        KeyCode::Enter => {
            if let Some((_, prompt)) = selected_message_skill(app) {
                insert_into_active_composer(app, &prompt);
                app.set_hint(
                    "skill inserted — edit or press Enter to send",
                    HintLevel::Ok,
                );
            } else if app.message_skills.is_empty() {
                app.set_hint(
                    "no message skills — run: tend skill add <name> <prompt>",
                    HintLevel::Warn,
                );
            } else {
                app.set_hint("no matching message skill", HintLevel::Warn);
            }
        }
        KeyCode::Up | KeyCode::BackTab => move_message_skill_selection(app, -1),
        KeyCode::Down | KeyCode::Tab => move_message_skill_selection(app, 1),
        KeyCode::Char('p') if modifiers.contains(KeyModifiers::CONTROL) => {
            move_message_skill_selection(app, -1);
        }
        KeyCode::Char('n') if modifiers.contains(KeyModifiers::CONTROL) => {
            move_message_skill_selection(app, 1);
        }
        KeyCode::Char(c) if !modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(palette) = active_message_skill_palette_mut(app) {
                palette.insert(c);
            }
        }
        _ => {}
    }
    Action::None
}

pub(super) fn handle_collaboration_composer_event(
    code: KeyCode,
    modifiers: KeyModifiers,
    app: &mut App,
) -> Action {
    if app
        .collaboration_composer
        .as_ref()
        .is_some_and(|composer| composer.skill_palette.is_some())
    {
        return handle_message_skill_palette_event(code, modifiers, app);
    }
    match code {
        KeyCode::Esc => Action::CancelCollaborationComposer,
        KeyCode::Backspace
            if app
                .collaboration_composer
                .as_ref()
                .is_some_and(|composer| composer.input.is_empty()) =>
        {
            Action::CancelCollaborationComposer
        }
        KeyCode::Enter => composer_submit_action(app),
        KeyCode::Tab => {
            if composer_cycle_option(app) {
                Action::CollaborationDefaultsChanged
            } else {
                Action::None
            }
        }
        KeyCode::Char('v') if modifiers.contains(KeyModifiers::CONTROL) => {
            if let (Some(pasted), Some(composer)) =
                (system_clipboard_text(), app.collaboration_composer.as_mut())
            {
                for c in pasted.chars() {
                    composer.insert(c);
                }
            }
            Action::None
        }
        KeyCode::Char('e') if modifiers.contains(KeyModifiers::CONTROL) => {
            if composer_cycle_mode(app) {
                Action::CollaborationDefaultsChanged
            } else {
                Action::None
            }
        }
        KeyCode::Char('/') if !modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(composer) = app.collaboration_composer.as_mut() {
                composer.skill_palette = Some(MessageSkillPalette::default());
            }
            Action::None
        }
        KeyCode::Char(c) if !modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(composer) = app.collaboration_composer.as_mut() {
                composer.insert(c);
            }
            Action::None
        }
        KeyCode::Backspace => {
            if let Some(composer) = app.collaboration_composer.as_mut() {
                composer.backspace();
            }
            Action::None
        }
        KeyCode::Delete => {
            if let Some(composer) = app.collaboration_composer.as_mut() {
                composer.delete();
            }
            Action::None
        }
        KeyCode::Left => {
            if let Some(composer) = app.collaboration_composer.as_mut() {
                composer.move_left();
            }
            Action::None
        }
        KeyCode::Right => {
            if let Some(composer) = app.collaboration_composer.as_mut() {
                composer.move_right();
            }
            Action::None
        }
        KeyCode::Home => {
            if let Some(composer) = app.collaboration_composer.as_mut() {
                composer.move_home();
            }
            Action::None
        }
        KeyCode::End => {
            if let Some(composer) = app.collaboration_composer.as_mut() {
                composer.move_end();
            }
            Action::None
        }
        _ => Action::None,
    }
}

pub(super) fn handle_collaboration_mailbox_event(code: KeyCode, app: &mut App) -> Action {
    let aggregate = app
        .collaboration
        .history_scope
        .as_ref()
        .is_some_and(MailboxHistoryScope::aggregate);
    match code {
        KeyCode::Esc | KeyCode::Char('q' | 'M' | 'b') => {
            app.collaboration_mailbox.open = false;
            Action::None
        }
        KeyCode::Char('m') if aggregate => {
            app.set_hint(
                "select an exact pane before composing a message",
                HintLevel::Warn,
            );
            Action::None
        }
        KeyCode::Char('m') => Action::OpenCollaborationMessage,
        KeyCode::Tab | KeyCode::BackTab if aggregate => {
            app.collaboration_mailbox.tab =
                if app.collaboration_mailbox.tab == CollaborationMailboxTab::Human {
                    CollaborationMailboxTab::Incoming
                } else {
                    CollaborationMailboxTab::Human
                };
            app.collaboration_mailbox.selected = 0;
            Action::None
        }
        KeyCode::Tab | KeyCode::BackTab => {
            toggle_collaboration_mailbox(app);
            Action::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            move_collaboration_mailbox(app, 1);
            Action::None
        }
        KeyCode::Up | KeyCode::Char('k') => {
            move_collaboration_mailbox(app, -1);
            Action::None
        }
        KeyCode::Char('|') => {
            app.collaboration_mailbox.detail_expand =
                app.collaboration_mailbox.detail_expand.next();
            let label = app.collaboration_mailbox.detail_expand.label();
            app.set_hint(format!("mailbox detail: {label}"), HintLevel::Ok);
            Action::None
        }
        KeyCode::Char('i' | 'e')
            if aggregate && app.collaboration_mailbox.tab != CollaborationMailboxTab::Human =>
        {
            app.set_hint(
                "session/window history is read-only; select an exact recipient pane",
                HintLevel::Warn,
            );
            Action::None
        }
        KeyCode::Char('i') if app.collaboration_mailbox.tab == CollaborationMailboxTab::Human => {
            app.set_hint(
                "press e to answer the selected human request",
                HintLevel::Ok,
            );
            Action::None
        }
        KeyCode::Char('i') => Action::ClaimCollaborationInbox,
        KeyCode::Char('e') => {
            open_watch_collaboration_reply_composer(app);
            Action::None
        }
        KeyCode::Char('r') => Action::OpenCollaborationMailbox,
        KeyCode::Char('x') => confirm_collaboration_cancel(app),
        _ => Action::None,
    }
}

/// `x` in the mailbox: withdraw a request this console sent, while it is
/// still queued. It is the console's own request, so the console cancels it.
fn confirm_collaboration_cancel(app: &mut App) -> Action {
    if app.collaboration_mailbox.tab != CollaborationMailboxTab::Sent {
        app.set_hint("switch to sent requests to cancel", HintLevel::Warn);
        return Action::None;
    }
    let Some(origin) = app.collaboration.origin.clone() else {
        app.set_hint(collaboration_open_hint(), HintLevel::Err);
        return Action::None;
    };
    let Some(request) = selected_collaboration_request(app) else {
        app.set_hint("no sent request selected", HintLevel::Warn);
        return Action::None;
    };
    if request.status != RequestStatus::Queued {
        app.set_hint("only queued requests can be cancelled", HintLevel::Warn);
        return Action::None;
    }
    let request_id = request.id.clone();
    Action::AskConfirm(ConfirmPopup {
        message: format!(
            "Cancel {} to {}?",
            short_collaboration_request_id(&request_id),
            request.to.label()
        ),
        on_confirm: ConfirmAction::CancelCollaboration {
            origin: CollaborationOrigin {
                console: true,
                ..origin
            },
            request_id,
        },
    })
}

pub(super) fn open_watch_collaboration_reply_composer(app: &mut App) {
    if app.collaboration_mailbox.tab == CollaborationMailboxTab::Human {
        if let (Some(request), Some(origin)) = (
            selected_collaboration_request(app),
            app.collaboration.origin.clone(),
        ) {
            app.collaboration_composer = Some(CollaborationComposer::new(
                CollaborationComposeTarget::Reply {
                    origin: CollaborationOrigin {
                        console: true,
                        ..origin
                    },
                    request_id: request.id.clone(),
                    status: RequestStatus::Completed,
                },
                format!("reply to {}", request.from.label()),
            ));
        }
        return;
    }
    if app.collaboration_mailbox.tab != CollaborationMailboxTab::Incoming {
        app.set_hint("switch to incoming requests to reply", HintLevel::Err);
        return;
    }
    // A reply comes from the recipient. The console never receives, so `e`
    // speaks for the agent whose inbox the panel is showing.
    let Some(anchor) = app.collaboration.inbox.clone() else {
        app.set_hint("select a row with an agent to reply as", HintLevel::Err);
        return;
    };
    let Some(request) = selected_collaboration_request(app) else {
        app.set_hint("no incoming request selected", HintLevel::Err);
        return;
    };
    if request.status == RequestStatus::Queued {
        app.set_hint(
            "press i to claim the request before replying",
            HintLevel::Err,
        );
        return;
    }
    if request.status.is_terminal() {
        app.set_hint("selected request is already terminal", HintLevel::Err);
        return;
    }
    let request_id = request.id.clone();
    let label = format!(
        "{} · {}",
        request.from.label(),
        short_collaboration_request_id(&request_id)
    );
    app.collaboration_composer = Some(CollaborationComposer::new(
        CollaborationComposeTarget::Reply {
            origin: anchor.origin,
            request_id,
            status: RequestStatus::Completed,
        },
        label,
    ));
}
