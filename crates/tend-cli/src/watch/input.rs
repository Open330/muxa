use super::{
    board_abort_action, board_attach_action, board_claim_action, board_collaboration_unsupported,
    board_copy_action, board_cursor_agent, board_kill_action, board_message_action,
    board_pending_matches, board_prompt_action, board_screen, board_selected_pane,
    collab_attach_action, handle_ask_composer_event, handle_ask_panel_event,
    handle_collaboration_composer_event, handle_collaboration_mailbox_event,
    handle_message_skill_editor_event, handle_preview_event, handle_rename_event,
    handle_spawn_event, handle_work_composer_event, insert_str_at, io, mpsc, onboarding_empty,
    quick_abort_action, quick_copy_action, quick_kill_action, quick_prompt_action, request_refresh,
    App, BoardPending, CollabLayout, CollaborationOrigin, CommandPalette, ConfirmAction,
    ConfirmPopup, Duration, Event, HintLevel, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    PaneKey, QuickAction, RenameComposer, RenameLevel, SessionKey, SpawnAgent, SpawnComposer,
    TopologyNodeKey, WatchLayout, WatchScreen, WatchSortPreset, WatchView,
};

pub(super) fn interaction_session(
    app: &App,
    pane_id: Option<&str>,
) -> (Option<String>, Option<String>) {
    let session_name = pane_id.and_then(|id| {
        app.panes
            .iter()
            .find(|pane| pane.pane_id == id)
            .map(|pane| pane.session.clone())
    });
    let session_id = session_name.as_deref().and_then(|name| {
        app.sessions
            .iter()
            .find(|session| session.name == name)
            .map(|session| session.session_id.clone())
    });
    (session_id, session_name)
}

/// Pull every event already sitting in the OS-side terminal input
/// buffer without ever blocking. `poll(Duration::ZERO)` returns
/// immediately; we keep reading until nothing is left. Safe to call
/// from an async context since neither call yields.
pub(super) fn drain_pending_events() -> io::Result<Vec<Event>> {
    let mut events = Vec::new();
    while crossterm::event::poll(Duration::ZERO)? {
        events.push(crossterm::event::read()?);
    }
    Ok(events)
}

#[derive(Debug)]
pub(crate) enum Action {
    OpenAgentMove,
    SubmitAgentMove(crate::agent_move::MoveRequest),
    /// Open the work composer (`w`).
    OpenWorkUp,
    /// Run `tend work up` for whatever the composer holds.
    SubmitWorkUp,
    SubmitRename,
    None,
    Quit,
    TryDemo,
    Refresh,
    /// Drain and re-exec the daemon onto whatever binary is installed now.
    /// The remedy for the skew bar, reachable without leaving watch.
    RestartDaemon,
    /// Attach to a pane that was pinned by an overlay.
    AttachPane(String),
    /// Attach using the complete host+socket ancestry.
    AttachTopologyPane(PaneKey),
    /// `c` — open a plain shell window in the session the cursor is inside
    /// and hand the terminal to it, the way `prefix + c` does from within an
    /// attached session. No agent, no composer: the point is the two steps
    /// (attach, then create) collapsing into one keystroke.
    NewWindow {
        session: SessionKey,
        /// Directory the window starts in, when the selected row knows one.
        /// `None` lets tmux inherit whatever cwd watch itself was launched
        /// from, which is the wrong project often enough to be worth saying.
        cwd: Option<String>,
    },
    /// Pop open the preview overlay for the selected row.
    OpenPreview,
    /// Preview an exact pane chosen by a screen whose cursor is not on the
    /// topology (the Work board's selected Run).
    OpenPreviewFor(PaneKey),
    /// Close the preview overlay and return to the table.
    ClosePreview,
    /// Swap the preview between popup and full-screen modes.
    TogglePreviewMode,
    /// Swap the preview content between prompt/response and live pane
    /// capture. Composes with `TogglePreviewMode` (geometry).
    TogglePreviewContent,
    /// Change the table's primary sort while staying inside the watch TUI.
    SetSort(WatchSortPreset),
    /// Change table granularity from the command palette.
    SetView(WatchView),
    /// Change presentation without changing topology granularity.
    SetLayout(WatchLayout),
    /// Change the collaboration screen's independent table/sequence axis.
    SetCollabLayout(CollabLayout),
    /// Reorder the Work board; persisted apart from the topology sort.
    SetBoardSort(tend::config::WatchBoardSort),
    SetScreen(WatchScreen),
    /// Resolve the selected row as a same-window collaboration peer and open
    /// the durable request composer.
    OpenCollaborationMessage,
    ToggleCollaborationMark,
    DismissBroadcastReport,
    /// Refresh and open incoming/sent collaboration history.
    OpenCollaborationMailbox,
    /// Submit the active collaboration request or reply composer.
    SubmitCollaboration,
    /// Persist a reusable prompt registered from the in-watch skill editor.
    SaveMessageSkill {
        name: String,
        prompt: String,
    },
    /// Close the active collaboration composer.
    CancelCollaborationComposer,
    /// `Tab` / `Ctrl-E` changed the request defaults; persist them next to
    /// the other watch UI preferences.
    CollaborationDefaultsChanged,
    /// `a` — open the headless-question composer.
    OpenAsk,
    /// Submit the composed question, either continuing the selected
    /// conversation or atomically creating its replacement with the turn.
    SubmitAsk {
        new_conversation: bool,
    },
    /// `A` — refresh and open the selected Ask conversation.
    OpenAskPanel,
    /// Select the durable conversation whose transcript the panel shows.
    SelectAskConversation(String),
    /// Point the next question at the other agent.
    CycleAskAgent,
    /// `|` moved the list/inspector divider; the run loop persists the new
    /// ratio next to the persisted sort and reports it in one hint.
    InspectorSplitChanged,
    /// Atomically claim the current agent's pending inbox.
    ClaimCollaborationInbox,
    /// Claim the queued requests of an agent chosen on the board, acting as
    /// that agent. The origin is frozen when the key is pressed.
    ClaimBoardInbox {
        origin: CollaborationOrigin,
        label: String,
    },
    /// Open a confirm popup for a destructive action. The popup itself is
    /// interpreted by the input loop; the action only dispatches when the
    /// user answers `y`.
    AskConfirm(ConfirmPopup),
    /// User answered `y` to the active confirm popup — dispatch the
    /// payload and clear the popup.
    ConfirmYes,
    /// User answered anything else (`n`, Esc, `q`, Tab, arrow keys).
    /// Just clears the popup with no side-effect.
    ConfirmCancel,
    /// Run a non-destructive quick action immediately (no confirm).
    /// Currently used for `c` (copy prompt) and the `?` help toggle.
    Quick(QuickAction),
    /// Surface a one-line "not applicable" hint in the footer because
    /// the requested action doesn't fit the selected row. String is
    /// the rendered hint body.
    NotApplicable(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchOpenTarget {
    Demo,
    Moved(PaneKey),
    TopologyPane(PaneKey),
    LegacyPane(String),
}

#[derive(Debug, Clone, Copy)]
pub(super) struct CommandSpec {
    pub(super) command: &'static str,
    pub(super) description: &'static str,
}

const COMMAND_SPECS: &[CommandSpec] = &[
    CommandSpec {
        command: "move",
        description: "move selected agent without restarting",
    },
    CommandSpec {
        command: "refresh",
        description: "refresh data now",
    },
    CommandSpec {
        command: "preview",
        description: "preview selected pane",
    },
    CommandSpec {
        command: "copy",
        description: "copy selected prompt",
    },
    CommandSpec {
        command: "message",
        description: "message selected room peer",
    },
    CommandSpec {
        command: "mailbox",
        description: "open collaboration mailbox",
    },
    CommandSpec {
        command: "attention",
        description: "toggle attention-only rows",
    },
    CommandSpec {
        command: "events",
        description: "open transition inbox",
    },
    CommandSpec {
        command: "inspector",
        description: "toggle wide-screen inspector",
    },
    CommandSpec {
        command: "sort latest",
        description: "sort by latest activity",
    },
    CommandSpec {
        command: "sort duration",
        description: "sort siblings by hierarchy duration",
    },
    CommandSpec {
        command: "sort name",
        description: "sort siblings by name",
    },
    CommandSpec {
        command: "sort state",
        description: "sort by attention state",
    },
    CommandSpec {
        command: "view session",
        description: "show session roots",
    },
    CommandSpec {
        command: "view window",
        description: "show sessions and windows",
    },
    CommandSpec {
        command: "view pane",
        description: "show individual panes",
    },
    CommandSpec {
        command: "layout tree",
        description: "show nested topology tree",
    },
    CommandSpec {
        command: "layout swarm",
        description: "show swarm clusters",
    },
    CommandSpec {
        command: "layout work",
        description: "show one row per Work",
    },
    CommandSpec {
        command: "layout table",
        description: "show collaboration as a compact table",
    },
    CommandSpec {
        command: "layout sequence",
        description: "show chronological collaboration lifelines",
    },
    CommandSpec {
        command: "screen topology",
        description: "show sessions, windows and panes",
    },
    CommandSpec {
        command: "screen collab",
        description: "show collaboration across every room",
    },
    CommandSpec {
        command: "screen board",
        description: "show Works and their Runs",
    },
    CommandSpec {
        command: "sort busy",
        description: "board: most agents working first",
    },
    CommandSpec {
        command: "kill",
        description: "close selected session/window/pane (confirm)",
    },
    CommandSpec {
        command: "abort",
        description: "interrupt selected agent pane (confirm)",
    },
    CommandSpec {
        command: "daemon restart",
        description: "restart a stale tendd onto the installed binary",
    },
    CommandSpec {
        command: "help",
        description: "show keybindings",
    },
    CommandSpec {
        command: "quit",
        description: "exit tend watch",
    },
];

pub(super) fn command_suggestions(input: &str) -> Vec<CommandSpec> {
    let query = input.trim().to_lowercase();
    COMMAND_SPECS
        .iter()
        .copied()
        .filter(|spec| query.is_empty() || spec.command.starts_with(&query))
        .take(8)
        .collect()
}

pub(super) fn execute_palette_command(app: &mut App, input: &str) -> Action {
    let normalized = input.split_whitespace().collect::<Vec<_>>().join(" ");
    let command = normalized.to_lowercase();
    // Refused before any command opens a form for the hidden topology node.
    if app.watch_cfg.screen == WatchScreen::Board && !board_palette_allows(&command) {
        app.set_hint(BOARD_REFUSAL, HintLevel::Warn);
        return Action::None;
    }
    // `sort` means the board's own order on the board; topology sort keys
    // are left alone and keep meaning what they mean on the other screens.
    if let Some(key) = command.strip_prefix("sort ") {
        if app.watch_cfg.screen == WatchScreen::Board {
            return board_screen::parse_sort(key.trim()).map_or_else(
                || {
                    app.set_hint(
                        "board sorts: attention, activity, busy, name",
                        HintLevel::Warn,
                    );
                    Action::None
                },
                Action::SetBoardSort,
            );
        }
    }
    match command.as_str() {
        "q" | "quit" => Action::Quit,
        "r" | "refresh" => Action::Refresh,
        "o" | "open" | "preview" => Action::OpenPreview,
        "m" | "message" => Action::OpenCollaborationMessage,
        "move" => Action::OpenAgentMove,
        "split" => {
            app.inspector_split = app.inspector_split.next();
            Action::InspectorSplitChanged
        }
        "b" | "mailbox" => Action::OpenCollaborationMailbox,
        "rename" => {
            app.open_rename();
            Action::None
        }
        "copy" | "yank" => quick_copy_action(app),
        "kill" => quick_kill_action(app),
        "abort" => quick_abort_action(app),
        "daemon restart" => Action::RestartDaemon,
        "help" | "?" => Action::Quick(QuickAction::ShowHelp),
        "attention" => {
            app.toggle_attention_only();
            app.set_hint(
                if app.attention_only {
                    "attention filter enabled"
                } else {
                    "attention filter disabled"
                },
                HintLevel::Ok,
            );
            Action::None
        }
        "events" => {
            app.toggle_event_inbox();
            Action::None
        }
        "inspector" => {
            app.toggle_inspector();
            Action::None
        }
        "sort latest" => Action::SetSort(WatchSortPreset::Latest),
        "sort duration" => Action::SetSort(WatchSortPreset::Duration),
        "sort name" => Action::SetSort(WatchSortPreset::Name),
        "sort state" | "sort attention" => Action::SetSort(WatchSortPreset::State),
        "view session" | "view sessions" => Action::SetView(WatchView::Session),
        "view window" | "view windows" => Action::SetView(WatchView::Window),
        "view pane" | "view panes" => Action::SetView(WatchView::Pane),
        "screen topology" | "topology" => Action::SetScreen(WatchScreen::Topology),
        "screen collab" | "collab" => Action::SetScreen(WatchScreen::Collab),
        "screen board" | "board" => Action::SetScreen(WatchScreen::Board),
        "layout work" | "work" => Action::SetLayout(WatchLayout::Work),
        "layout tree" => Action::SetLayout(WatchLayout::Tree),
        "layout swarm" => Action::SetLayout(WatchLayout::Swarm),
        "layout table" => Action::SetCollabLayout(CollabLayout::Table),
        "layout sequence" | "sequence" => Action::SetCollabLayout(CollabLayout::Sequence),
        "" => {
            app.set_hint("command: type a command or press Esc", HintLevel::Warn);
            Action::None
        }
        _ => {
            app.set_hint(format!("unknown command: {normalized}"), HintLevel::Warn);
            Action::None
        }
    }
}

pub(super) fn handle_event(ev: Event, app: &mut App) -> Action {
    let action = handle_event_on_any_screen(ev, app);
    if app.watch_cfg.screen == WatchScreen::Board {
        return gate_board_action(app, action);
    }
    action
}

/// The Work board's cursor is on a Work, not on the topology. Every action
/// that targets "the selected node" would act on whatever the hidden
/// topology cursor last pointed at, so on the board those are refused until
/// they have a Work-aware target. Preview is redirected to the selected
/// Work's Run.
pub(super) fn gate_board_action(app: &mut App, action: Action) -> Action {
    match action {
        Action::None
        | Action::Quit
        | Action::Refresh
        | Action::RestartDaemon
        | Action::AttachTopologyPane(_)
        | Action::OpenPreviewFor(_)
        | Action::ClosePreview
        | Action::TogglePreviewMode
        | Action::TogglePreviewContent
        | Action::SetView(_)
        | Action::SetLayout(_)
        | Action::SetCollabLayout(_)
        | Action::SetBoardSort(_)
        // Collaboration on the board resolves its target from the board's
        // own cursor (see `board_collaboration_target`), never the topology.
        | Action::OpenCollaborationMailbox
        | Action::SubmitCollaboration
        | Action::ClaimCollaborationInbox
        | Action::ClaimBoardInbox { .. }
        | Action::CollaborationDefaultsChanged
        // Withdrawing a request this console sent concerns no topology node.
        | Action::AskConfirm(ConfirmPopup {
            on_confirm: ConfirmAction::CancelCollaboration { .. },
            ..
        })
        | Action::InspectorSplitChanged
        | Action::DismissBroadcastReport
        | Action::Quick(QuickAction::ShowHelp) => action,
        // Ending the pending operation, either way, ends its authority.
        Action::ConfirmCancel
        | Action::CancelCollaborationComposer
        | Action::ConfirmYes
        | Action::SetScreen(_) => {
            app.board_pending = None;
            action
        }
        // `m` from the board itself froze its recipient first; `m` from the
        // mailbox (open on the board's agent) freezes it now.
        Action::OpenCollaborationMessage => {
            if matches!(app.board_pending, Some(BoardPending::Message { .. })) {
                action
            } else {
                board_message_action(app)
            }
        }
        // The pending operation's own confirmation, which stays pending
        // until answered.
        Action::AskConfirm(_) if board_pending_matches(app.board_pending.as_ref(), &action) => {
            action
        }
        // Copying is harmless, but only the cursor agent's prompt is the
        // board's: Alt-C would copy the hidden topology selection's.
        Action::Quick(QuickAction::CopyPrompt(ref prompt))
            if board_cursor_agent(app)
                .ok()
                .and_then(|(_, agent)| agent.last_prompt)
                .is_some_and(|own| &own == prompt) =>
        {
            action
        }
        // Its composer's submission, which uses the authority up.
        Action::Quick(_) if board_pending_matches(app.board_pending.as_ref(), &action) => {
            app.board_pending = None;
            action
        }
        Action::OpenPreview => match board_selected_pane(app) {
            Ok(key) => Action::OpenPreviewFor(key),
            Err(hint) => {
                app.set_hint(hint, HintLevel::Warn);
                Action::None
            }
        },
        _ => {
            app.set_hint(BOARD_REFUSAL, HintLevel::Warn);
            Action::None
        }
    }
}

/// The panes a quick action addresses, for the board actions only.
pub(super) fn board_quick_panes(quick: &QuickAction) -> Option<Vec<&PaneKey>> {
    match quick {
        QuickAction::InterruptPane(pane) | QuickAction::SendPromptTo { pane, .. } => {
            Some(vec![pane])
        }
        QuickAction::InterruptPanes { panes, .. } | QuickAction::SendPromptToMany { panes, .. } => {
            Some(panes.iter().collect())
        }
        QuickAction::TerminateNode(TopologyNodeKey::Pane(pane)) => Some(vec![pane]),
        _ => None,
    }
}

const BOARD_REFUSAL: &str = "not on the Work board yet — Alt-1 to act on the topology";

/// Browsing keys whose handlers act on the topology cursor directly — open a
/// form for it, or move it — before any `Action` exists for the outer gate to
/// refuse. Typing into a filter is unaffected: these only apply while browsing.
/// Modifiers do not matter: the shared branches match the bare key, so Ctrl-N
/// would open the same form. Alt never gets here, and the board's own Ctrl
/// bindings were consumed before this runs.
fn board_refuses_key(code: KeyCode, app: &mut App) -> bool {
    let refused = match code {
        // Expand and collapse the hidden tree.
        KeyCode::Left | KeyCode::Right => return true,
        KeyCode::Char(c) => {
            app.browse_keys_active()
                && matches!(
                    c,
                    'm' | ' ' | 'n' | 'C' | '|' | 'a' | 'A' | 'w' | 'R' | 'h' | 'l' | 'd'
                )
        }
        _ => false,
    };
    if refused {
        app.set_hint(BOARD_REFUSAL, HintLevel::Warn);
    }
    refused
}

/// Palette commands that mean the same on the board: navigation between
/// screens and presentations, global panels, and leaving.
fn board_palette_allows(command: &str) -> bool {
    matches!(
        command,
        "" | "q"
            | "quit"
            | "r"
            | "refresh"
            | "o"
            | "open"
            | "preview"
            | "help"
            | "?"
            | "topology"
            | "collab"
            | "board"
            | "attention"
            | "events"
            | "inspector"
            | "split"
            | "daemon restart"
    ) || ["screen ", "view ", "layout ", "sort "]
        .iter()
        .any(|prefix| command.starts_with(prefix))
}

#[allow(clippy::too_many_lines)] // one ordered input-mode dispatcher keeps modal precedence explicit
fn handle_event_on_any_screen(ev: Event, app: &mut App) -> Action {
    // Bracketed paste arrives as one event carrying the whole payload.
    // Prompt/command modes keep it literal; table mode treats it as a search
    // query, matching ordinary direct typing.
    if let Event::Paste(pasted) = ev {
        if let Some(menu) = app.agent_move.as_mut() {
            menu.paste(&pasted);
            return Action::None;
        }
        if let Some(editor) = app.message_skill_editor.as_mut() {
            let pasted = pasted.replace(['\r', '\n'], " ");
            let (text, cursor) = editor.field_mut();
            insert_str_at(text, cursor, &pasted);
        } else if let Some(composer) = app.collaboration_composer.as_mut() {
            if let Some(palette) = composer.skill_palette.as_mut() {
                for c in pasted.replace(['\r', '\n'], " ").chars() {
                    palette.insert(c);
                }
            } else {
                for c in pasted.chars() {
                    composer.insert(c);
                }
            }
        } else if let Some(ask) = app.ask_composer.as_mut() {
            if let Some(palette) = ask.skill_palette.as_mut() {
                for c in pasted.replace(['\r', '\n'], " ").chars() {
                    palette.insert(c);
                }
            } else {
                insert_str_at(&mut ask.input, &mut ask.cursor, &pasted);
            }
        } else if let Some(spawn) = app.spawn.as_mut() {
            // The spawn form was the one input surface missing here, so a
            // paste while it was open fell through to the search fallback
            // and started filtering the table under the form.
            if let Some((text, cursor)) = spawn.field_mut() {
                insert_str_at(text, cursor, &pasted);
            }
        } else if let Some(command) = app.command_palette.as_mut() {
            command.insert_str(&pasted.replace(['\r', '\n'], " "));
        } else if app.preview.is_none()
            && !app.help_open
            && !app.event_inbox_open
            && !app.collaboration_mailbox.open
        {
            app.edit_search(|query| query.push_str(&pasted.replace(['\r', '\n'], " ")));
        }
        return Action::None;
    }

    let Event::Key(KeyEvent {
        code,
        modifiers,
        kind,
        ..
    }) = ev
    else {
        return Action::None;
    };

    // On terminals that emit `Release` events too (crossterm 0.29 on
    // Windows / kitty protocol) we only react to initial presses so
    // actions don't fire twice.
    if kind != KeyEventKind::Press && kind != KeyEventKind::Repeat {
        return Action::None;
    }

    // Ctrl-C is global — quits regardless of mode.
    if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('c')) {
        return Action::Quit;
    }

    // Confirm popup steals every keystroke until resolved. Keeping the
    // accept gate to a single character (`y` / `Y` / Enter) and routing
    // everything else to `ConfirmCancel` is the deliberate "deliberately
    // type yes" safety rail — see `ConfirmPopup` doc.
    if app.confirm.is_some() {
        return match code {
            KeyCode::Char('y' | 'Y') | KeyCode::Enter => Action::ConfirmYes,
            // Spec calls out: anything else (incl. n, Esc, q, Tab,
            // arrows) cancels. Listing them explicitly would invite
            // someone to accidentally drop one — fall through.
            _ => Action::ConfirmCancel,
        };
    }

    if let Some(menu) = app.agent_move.as_mut() {
        return match menu.on_key(KeyEvent::new(code, modifiers)) {
            crate::agent_move::MenuEvent::None => Action::None,
            crate::agent_move::MenuEvent::Cancel => {
                app.agent_move = None;
                Action::None
            }
            crate::agent_move::MenuEvent::Submit(request) => Action::SubmitAgentMove(*request),
        };
    }

    if app.message_skill_editor.is_some() {
        return handle_message_skill_editor_event(code, modifiers, app);
    }

    if app.collaboration_composer.is_some() {
        return handle_collaboration_composer_event(code, modifiers, app);
    }

    if app.spawn.is_some() {
        return handle_spawn_event(code, modifiers, app);
    }

    if app.work_composer.is_some() {
        return handle_work_composer_event(code, modifiers, app);
    }
    if app.rename.is_some() {
        return handle_rename_event(code, modifiers, app);
    }
    if app.ask_composer.is_some() {
        return handle_ask_composer_event(code, modifiers, app);
    }

    if app.ask_panel.open {
        return handle_ask_panel_event(code, app);
    }

    if app.command_palette.is_some() {
        return handle_command_event(code, modifiers, app);
    }

    // Help overlay: `?` toggles it; `q` / `Esc` close it. Anything
    // else passes through but is ignored — we don't want `c` while
    // the overlay is open to silently copy a prompt the user can't
    // see.
    if app.broadcast_report.is_some() {
        return match code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => Action::DismissBroadcastReport,
            _ => Action::None,
        };
    }

    if app.help_open {
        return match code {
            KeyCode::F(1) | KeyCode::Esc | KeyCode::Char('q' | '?') => {
                Action::Quick(QuickAction::ShowHelp)
            }
            _ => Action::None,
        };
    }

    if app.collaboration_mailbox.open {
        return handle_collaboration_mailbox_event(code, app);
    }

    if app.event_inbox_open {
        return match code {
            KeyCode::Esc | KeyCode::Char('q') => {
                app.toggle_event_inbox();
                Action::None
            }
            KeyCode::Char(c)
                if modifiers.contains(KeyModifiers::ALT) && c.eq_ignore_ascii_case(&'e') =>
            {
                app.toggle_event_inbox();
                Action::None
            }
            _ => Action::None,
        };
    }

    if app.preview.is_some() {
        return handle_preview_event(code, app);
    }

    if modifiers.contains(KeyModifiers::ALT) {
        return match code {
            KeyCode::Char(c) if c.eq_ignore_ascii_case(&'m') => Action::OpenAgentMove,
            KeyCode::Char(c) if c.eq_ignore_ascii_case(&'p') => Action::OpenPreview,
            KeyCode::Char(c) if c.eq_ignore_ascii_case(&'r') => Action::Refresh,
            KeyCode::Char(c) if c.eq_ignore_ascii_case(&'s') => {
                Action::SetSort(WatchSortPreset::Name)
            }
            KeyCode::Char(c) if c.eq_ignore_ascii_case(&'l') => {
                Action::SetSort(WatchSortPreset::Latest)
            }
            KeyCode::Char(c) if c.eq_ignore_ascii_case(&'d') => {
                Action::SetSort(WatchSortPreset::Duration)
            }
            KeyCode::Char(c) if c.eq_ignore_ascii_case(&'t') => {
                Action::SetSort(WatchSortPreset::State)
            }
            KeyCode::Char(c) if c.eq_ignore_ascii_case(&'c') => quick_copy_action(app),
            KeyCode::Char(c) if c.eq_ignore_ascii_case(&'k') => quick_kill_action(app),
            KeyCode::Char(c) if c.eq_ignore_ascii_case(&'x') => quick_abort_action(app),
            KeyCode::Char(c) if c.eq_ignore_ascii_case(&'a') => {
                app.toggle_attention_only();
                Action::None
            }
            KeyCode::Char(c) if c.eq_ignore_ascii_case(&'i') => {
                app.toggle_inspector();
                Action::None
            }
            KeyCode::Char(c) if c.eq_ignore_ascii_case(&'e') => {
                app.toggle_event_inbox();
                Action::None
            }
            // Digits, not letters: plain typing is the filter here, so the
            // screen axis has to live on a modifier, and every letter worth
            // having is already spoken for.
            KeyCode::Char('1') => Action::SetScreen(WatchScreen::Topology),
            KeyCode::Char('2') => Action::SetScreen(WatchScreen::Collab),
            KeyCode::Char('3') => Action::SetScreen(WatchScreen::Board),
            KeyCode::Char('?') => Action::Quick(QuickAction::ShowHelp),
            _ => Action::None,
        };
    }

    // The collab screen lists requests rather than nodes, so movement has
    // nothing to move through in the tree. Everything it does not claim —
    // filter typing, `:`, `/`, `r`, `?`, `q` — falls through unchanged.
    if matches!(app.watch_cfg.screen, WatchScreen::Collab) {
        if let Some(action) = handle_collab_screen_key(code, modifiers, app) {
            return action;
        }
    }
    if app.watch_cfg.screen == WatchScreen::Board {
        if let Some(action) = handle_board_screen_key(code, modifiers, app) {
            return action;
        }
        if board_refuses_key(code, app) {
            return Action::None;
        }
    }

    if app.pending_g && !matches!(code, KeyCode::Char('g') | KeyCode::Esc) {
        app.pending_g = false;
    }

    match code {
        KeyCode::F(1) => Action::Quick(QuickAction::ShowHelp),
        KeyCode::Esc if app.explicit_search || !app.search_query.is_empty() => {
            app.clear_search();
            Action::None
        }
        KeyCode::Esc if app.pending_g => {
            app.pending_g = false;
            Action::None
        }
        KeyCode::Esc if app.attention_only => {
            app.toggle_attention_only();
            Action::None
        }
        KeyCode::Esc => Action::Quit,
        KeyCode::Enter if onboarding_empty(app) => handle_event(
            Event::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
            app,
        ),
        KeyCode::Char('d')
            if onboarding_empty(app) && app.browse_keys_active() && app.demo_available =>
        {
            Action::TryDemo
        }
        KeyCode::Enter => quick_prompt_action(app),
        KeyCode::Down => {
            app.move_down();
            Action::None
        }
        KeyCode::Up => {
            app.move_up();
            Action::None
        }
        KeyCode::Right => {
            app.move_into_work();
            Action::None
        }
        KeyCode::Left => {
            app.move_to_work_parent();
            Action::None
        }
        KeyCode::Home => {
            app.move_first();
            Action::None
        }
        KeyCode::End => {
            app.move_last();
            Action::None
        }
        KeyCode::PageDown => {
            app.move_page_down();
            Action::None
        }
        KeyCode::PageUp => {
            app.move_page_up();
            Action::None
        }
        KeyCode::Char('u')
            if modifiers.contains(KeyModifiers::CONTROL)
                && (app.explicit_search || !app.search_query.is_empty()) =>
        {
            app.clear_search();
            Action::None
        }
        KeyCode::Char('w')
            if modifiers.contains(KeyModifiers::CONTROL)
                && (app.explicit_search || !app.search_query.is_empty()) =>
        {
            app.delete_search_word();
            Action::None
        }
        KeyCode::Char('u')
            if modifiers.contains(KeyModifiers::CONTROL) && app.browse_keys_active() =>
        {
            app.move_half_page_up();
            Action::None
        }
        KeyCode::Char('d')
            if modifiers.contains(KeyModifiers::CONTROL) && app.browse_keys_active() =>
        {
            app.move_half_page_down();
            Action::None
        }
        KeyCode::Char('r') if modifiers.contains(KeyModifiers::CONTROL) => Action::Refresh,
        KeyCode::Char('/') if app.browse_keys_active() => {
            app.arm_explicit_search();
            Action::None
        }
        KeyCode::Char(':') if app.browse_keys_active() => {
            app.command_palette = Some(CommandPalette::default());
            app.pending_g = false;
            Action::None
        }
        KeyCode::Char('q') if app.browse_keys_active() => Action::Quit,
        KeyCode::Char('?') if app.browse_keys_active() => Action::Quick(QuickAction::ShowHelp),
        KeyCode::Char('r') if app.browse_keys_active() => Action::Refresh,
        KeyCode::Char('o') if app.browse_keys_active() => Action::OpenPreview,
        KeyCode::Char('m') if app.browse_keys_active() => Action::OpenCollaborationMessage,
        // Space marks the agent under the cursor for a `m` that addresses
        // several at once. Unbound before this, and the conventional mark key.
        KeyCode::Char(' ') if app.browse_keys_active() => Action::ToggleCollaborationMark,
        // `b` is the legacy alias retained after the pair became m/M.
        KeyCode::Char('M' | 'b') if app.browse_keys_active() => Action::OpenCollaborationMailbox,
        KeyCode::Char('n') if app.browse_keys_active() => {
            let fallback_dir =
                std::env::current_dir().map_or_else(|_| "~".into(), |p| p.display().to_string());
            let (target, dir, name) = app.spawn_context(fallback_dir);
            // Cursor at the end of the prefill — editing a path means
            // appending or trimming, and a cursor at column zero turns the
            // first keystroke into a prefix splice.
            let dir_cursor = dir.chars().count();
            let name_cursor = name.chars().count();
            app.spawn = Some(SpawnComposer {
                target,
                dir,
                dir_cursor,
                name,
                name_cursor,
                agent: [
                    SpawnAgent::Claude,
                    SpawnAgent::Codex,
                    SpawnAgent::Gemini,
                    SpawnAgent::Antigravity,
                    SpawnAgent::Opencode,
                ]
                .into_iter()
                .find(|agent| which::which(agent.label()).is_ok())
                .unwrap_or_default(),
                ..SpawnComposer::default()
            });
            Action::None
        }
        // tmux's own `prefix + c`, minus the attach that normally has to come
        // first. Shifted like `R`: both reshape the session rather than move
        // through it, and neither should be one stray keystroke away.
        KeyCode::Char('C') if app.browse_keys_active() => match app.new_window_context() {
            Some((session, cwd)) => Action::NewWindow { session, cwd },
            None => Action::NotApplicable("select a row first — a new window needs a session"),
        },
        KeyCode::Char('|') if app.browse_keys_active() => {
            app.inspector_split = app.inspector_split.next();
            Action::InspectorSplitChanged
        }
        KeyCode::Char('a') if app.browse_keys_active() => Action::OpenAsk,
        KeyCode::Char('w') if app.browse_keys_active() => Action::OpenWorkUp,
        KeyCode::Char('R') if app.browse_keys_active() => {
            app.open_rename();
            Action::None
        }
        KeyCode::Char('W') if app.browse_keys_active() => Action::SetLayout(app.next_work_layout()),
        KeyCode::Char('A') if app.browse_keys_active() => Action::OpenAskPanel,
        KeyCode::Char('h') if app.browse_keys_active() => {
            app.move_to_work_parent();
            Action::None
        }
        KeyCode::Char('l') if app.browse_keys_active() => {
            app.move_into_work();
            Action::None
        }
        KeyCode::Char('G') if app.browse_keys_active() => {
            app.move_last();
            Action::None
        }
        KeyCode::Char('g') if app.browse_keys_active() && app.pending_g => {
            app.pending_g = false;
            app.move_first();
            Action::None
        }
        KeyCode::Char('g') if app.browse_keys_active() => {
            app.pending_g = true;
            Action::None
        }
        KeyCode::Backspace => {
            app.edit_search(|query| {
                query.pop();
            });
            Action::None
        }
        KeyCode::Char('j')
            if app.browse_keys_active() && !modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.move_down();
            Action::None
        }
        KeyCode::Char('k')
            if app.browse_keys_active() && !modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.move_up();
            Action::None
        }
        // A keystroke is text only after `/` armed the filter. Before that
        // every letter is a binding, so an unbound one does nothing — and
        // says where the filter went, because typing used to open it and
        // that reflex outlives the change.
        KeyCode::Char(c) if !modifiers.contains(KeyModifiers::CONTROL) => {
            if app.browse_keys_active() {
                app.set_hint("press / to filter", HintLevel::Warn);
            } else {
                app.edit_search(|query| query.push(c));
            }
            Action::None
        }
        _ => Action::None,
    }
}

/// `:` command input owns keystrokes until Enter or Esc. Tab completes the
/// first visible suggestion; editing follows the same conventions as the
/// prompt composer and common shell command lines.
fn handle_command_event(code: KeyCode, modifiers: KeyModifiers, app: &mut App) -> Action {
    match code {
        KeyCode::Esc => {
            app.command_palette = None;
            Action::None
        }
        KeyCode::Enter => {
            let input = app
                .command_palette
                .take()
                .map(|command| command.input)
                .unwrap_or_default();
            execute_palette_command(app, &input)
        }
        KeyCode::Tab => {
            let completion = app
                .command_palette
                .as_ref()
                .and_then(|command| command_suggestions(&command.input).first().copied());
            if let (Some(command), Some(spec)) = (app.command_palette.as_mut(), completion) {
                command.input = spec.command.to_string();
                command.move_end();
            }
            Action::None
        }
        KeyCode::Char('w') if modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(command) = app.command_palette.as_mut() {
                command.delete_word();
            }
            Action::None
        }
        KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(command) = app.command_palette.as_mut() {
                command.clear();
            }
            Action::None
        }
        KeyCode::Char(c) if !modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(command) = app.command_palette.as_mut() {
                command.insert(c);
            }
            Action::None
        }
        KeyCode::Backspace => {
            if let Some(command) = app.command_palette.as_mut() {
                command.backspace();
            }
            Action::None
        }
        KeyCode::Delete => {
            if let Some(command) = app.command_palette.as_mut() {
                command.delete();
            }
            Action::None
        }
        KeyCode::Left => {
            if let Some(command) = app.command_palette.as_mut() {
                command.move_left();
            }
            Action::None
        }
        KeyCode::Right => {
            if let Some(command) = app.command_palette.as_mut() {
                command.move_right();
            }
            Action::None
        }
        KeyCode::Home => {
            if let Some(command) = app.command_palette.as_mut() {
                command.move_home();
            }
            Action::None
        }
        KeyCode::End => {
            if let Some(command) = app.command_palette.as_mut() {
                command.move_end();
            }
            Action::None
        }
        _ => Action::None,
    }
}

/// Ask composer: one field, Enter submits, `Ctrl-V` pastes. `/` opens the
/// shared text-skill palette; it does not alter the daemon-owned ask contract.
/// Open a tmux window running `tend work up <id>` and hand the operator over
/// to it.
///
/// Watch does not run the pipeline itself on purpose. `work up` resolves a
/// ticket, which costs money, and the CLI already owns the flow that says so
/// and waits for a yes. Re-implementing that inside the TUI would mean two
/// places where tend decides whether it may spend the operator's money, and
/// the second one would be the one nobody reviewed. So the TUI's job ends at
/// putting the operator in front of the real thing.
pub(super) fn open_work_up_window(raw: &str, in_tmux: bool) -> std::result::Result<String, String> {
    let work = crate::tmux_work::normalize_work_id(raw).map_err(|error| error.to_string())?;
    // Outside tmux, `new-window` still succeeds — it lands in whichever session
    // the server saw last, which is not one the operator is looking at. They
    // would be told to confirm in a window they cannot find, and the run would
    // hang on a prompt nobody sees. Refusing and naming the command is the
    // honest outcome.
    if !in_tmux {
        return Err(format!(
            "not inside tmux; run `tend work up {work}` from a shell instead"
        ));
    }
    let exe = std::env::current_exe()
        .map_err(|error| format!("locate the tend binary: {error}"))?
        .display()
        .to_string();
    let args = work_up_window_args(&exe, &work);
    let output = crate::mux_control::ambient_command()
        .args(&args)
        .output()
        .map_err(|error| format!("tmux: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(work)
}

/// The tmux argv for that window.
///
/// Split out to be tested without a tmux server, because the interesting part
/// is the quoting: `normalize_work_id` rejects only tabs, newlines and
/// over-long input, so a work id can still carry a quote or a semicolon and
/// this string is handed to a shell.
pub(super) fn work_up_window_args(exe: &str, work: &str) -> Vec<String> {
    work_up_window_args_in(tend::shell::Dialect::native(), exe, work)
}

/// [`work_up_window_args`] for one shell dialect: rmux on Windows runs the
/// window's command through `PowerShell`, which has neither `printf` nor
/// `read`, and reads a leading quoted program as a string.
fn work_up_window_args_in(dialect: tend::shell::Dialect, exe: &str, work: &str) -> Vec<String> {
    use tend::shell::Dialect;
    let (exe, work) = (dialect.quote(exe), dialect.quote(work));
    let command = match dialect {
        Dialect::Posix => {
            format!("{exe} work up {work}; printf '\\n-- enter to close --'; read _")
        }
        Dialect::PowerShell => format!(
            "& {exe} work up {work}; Write-Host \"`n-- enter to close --\" -NoNewline; \
             $null = Read-Host"
        ),
    };
    vec!["new-window".into(), "-n".into(), "work-up".into(), command]
}

/// POSIX single-quote quoting: wrap in `'`, and close/escape/reopen around any
/// embedded `'`.
#[cfg(test)]
pub(super) fn shell_quote(value: &str) -> String {
    tend::shell::Dialect::Posix.quote(value)
}

/// Whether `list-windows` output already gives `name` to a *different* window.
///
/// Equality on the id, not the name, is what lets a window keep the name it
/// already has: re-submitting an unchanged prefill must be a no-op, not a
/// collision with itself.
pub(super) fn window_name_taken(listing: &str, window_id: &str, name: &str) -> bool {
    listing.lines().any(|line| {
        line.split_once('\t')
            .is_some_and(|(id, current)| id.trim() != window_id && current.trim() == name)
    })
}

/// Session names and pane titles are labels, not Work window ids. Preserve
/// their internal whitespace; applying the window policy here would turn a
/// pane title such as `Claude review` into `Claude-review` as a side effect of
/// changing one character.
pub(super) fn rename_label(raw: &str, level: RenameLevel) -> std::result::Result<String, String> {
    let value = raw.trim();
    if value.is_empty() {
        return Err(format!("{} cannot be empty", level.label()));
    }
    if value.chars().any(char::is_control) {
        return Err(format!(
            "{} cannot contain control characters",
            level.label()
        ));
    }
    if value.len() > 64 {
        return Err(format!("{} is too long (max 64 bytes)", level.label()));
    }
    if value.contains("#{") || value.contains("#(") {
        return Err(format!(
            "{} cannot contain tmux format expansions",
            level.label()
        ));
    }
    if level == RenameLevel::Session && value.chars().any(|ch| matches!(ch, '.' | ':')) {
        return Err("session name cannot contain '.' or ':'".into());
    }
    Ok(value.to_string())
}

/// Apply a rename to tmux. Returns the message to show on success.
///
/// Sessions and windows have real names; a pane does not, so `RenameLevel::Pane`
/// sets the pane *title* — the string watch already displays for a pane, and the
/// only per-pane label tmux persists.
pub(super) fn apply_rename(rename: &RenameComposer) -> std::result::Result<String, String> {
    let endpoint = rename.key.endpoint();
    let name = match rename.level {
        RenameLevel::Window => {
            crate::tmux_work::normalize_window_name(&rename.input).map_err(|e| e.to_string())?
        }
        RenameLevel::Session | RenameLevel::Pane => rename_label(&rename.input, rename.level)?,
    };
    let run = |args: &[&str]| -> std::result::Result<(), String> {
        crate::mux_control::run(endpoint, args)
    };
    match (&rename.key, rename.level) {
        (TopologyNodeKey::Session(session), _) => {
            run(&["rename-session", "-t", &session.session_id, &name])?;
        }
        (TopologyNodeKey::Window(window), _) => {
            // Session-scoped uniqueness, for the reason tmux targets make it
            // matter: `session:window` matches names by prefix, so a duplicate
            // silently addresses the wrong window.
            let listing = crate::mux_control::capture(
                endpoint,
                &[
                    "list-windows",
                    "-t",
                    &window.session.session_id,
                    "-F",
                    "#{window_id}\t#{window_name}",
                ],
            )?;
            if window_name_taken(&listing, &window.window_id, &name) {
                return Err(format!(
                    "window name {name:?} is already used in this session"
                ));
            }
            // Pin the mode rather than relying on `rename-window` turning
            // automatic-rename off as a side effect.
            run(&[
                "set-window-option",
                "-t",
                &window.window_id,
                "automatic-rename",
                "off",
            ])?;
            run(&["rename-window", "-t", &window.window_id, &name])?;
        }
        (TopologyNodeKey::Pane(pane), _) => {
            run(&[
                "select-pane",
                "-t",
                pane.pane_id.strip_prefix("rmux:").unwrap_or(&pane.pane_id),
                "-T",
                &name,
            ])?;
        }
    }
    Ok(format!("renamed {} to {name:?}", rename.level.label()))
}

/// Apply the open rename form and reconcile the list with the result.
///
/// The rename lands in tmux, not in tendd, so nothing on the push stream
/// announces it. Without an explicit refresh the row keeps its old name until
/// the next fallback tick — five seconds while the subscription is live, which
/// reads as "the rename did not take". `compute_refresh` rescans the backends
/// on every wake, so one request is enough to redraw the new name.
///
/// `apply` is injected so tests can exercise both outcomes without shelling
/// out to tmux.
pub(super) fn finish_rename(
    app: &mut App,
    wake_tx: &mpsc::Sender<()>,
    apply: impl FnOnce(&RenameComposer) -> std::result::Result<String, String>,
) {
    let Some(result) = app.rename.as_ref().map(apply) else {
        return;
    };
    match result {
        Ok(message) => {
            app.rename = None;
            app.set_hint(message, HintLevel::Ok);
            request_refresh(app, wake_tx);
        }
        // Keep the form and its input open so a duplicate, invalid value, or
        // transient tmux failure can be corrected instead of making the user
        // retype it. Nothing changed, so nothing needs refreshing.
        Err(e) => app.set_hint(format!("rename failed: {e}"), HintLevel::Err),
    }
}

/// Keys the collaboration screen claims. `None` lets the key fall through to
/// the ordinary table handling.
///
/// Letters are only claimed while no filter is being typed, exactly as the
/// tree does it — otherwise `j` in a search would jump the cursor instead of
/// narrowing the list.
fn handle_collab_screen_key(
    code: KeyCode,
    modifiers: KeyModifiers,
    app: &mut App,
) -> Option<Action> {
    let filter = app.search_query.clone();
    let page = isize::try_from(app.table_page_rows.max(1)).unwrap_or(isize::MAX);
    let browsing = app.browse_keys_active();
    match code {
        KeyCode::Down => app.collab.move_visual_selection(1, &filter),
        KeyCode::Up => app.collab.move_visual_selection(-1, &filter),
        KeyCode::Char('j') if browsing => app.collab.move_visual_selection(1, &filter),
        KeyCode::Char('k') if browsing => app.collab.move_visual_selection(-1, &filter),
        KeyCode::PageDown => app.collab.move_visual_selection(page, &filter),
        KeyCode::PageUp => app.collab.move_visual_selection(-page, &filter),
        KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.collab.move_visual_selection(page / 2, &filter);
        }
        KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.collab.move_visual_selection(-(page / 2), &filter);
        }
        KeyCode::Home => app.collab.select_visual_first(&filter),
        KeyCode::End => app.collab.select_visual_last(&filter),
        KeyCode::Char('G') if browsing => app.collab.select_visual_last(&filter),
        KeyCode::Char('v') if browsing => {
            let layout = match app.collab.layout() {
                CollabLayout::Table => CollabLayout::Sequence,
                CollabLayout::Sequence => CollabLayout::Table,
            };
            return Some(Action::SetCollabLayout(layout));
        }
        KeyCode::Char('g') if browsing => {
            if app.pending_g {
                app.pending_g = false;
                app.collab.select_visual_first(&filter);
            } else {
                app.pending_g = true;
            }
        }
        // Enter goes where the request went. The peer's pane is the useful
        // end: the console dispatches from nowhere in particular, and a
        // request whose peer is the console is a reply the operator already
        // has on screen.
        KeyCode::Enter => return Some(collab_attach_action(app, &filter)),
        _ => return None,
    }
    Some(Action::None)
}

/// Keys the Work board claims. Like the collab screen, everything else —
/// filter typing, `:`, `/`, `?`, `q` — falls through to watch.
fn handle_board_screen_key(
    code: KeyCode,
    modifiers: KeyModifiers,
    app: &mut App,
) -> Option<Action> {
    let filter = app.search_query.clone();
    let page = isize::try_from(app.table_page_rows.max(1)).unwrap_or(isize::MAX);
    let browsing = app.browse_keys_active();
    match code {
        KeyCode::Down => app.board.move_selection(1, &filter),
        KeyCode::Up => app.board.move_selection(-1, &filter),
        KeyCode::Char('j') if browsing => app.board.move_selection(1, &filter),
        KeyCode::Char('k') if browsing => app.board.move_selection(-1, &filter),
        KeyCode::PageDown => app.board.move_selection(page, &filter),
        KeyCode::PageUp => app.board.move_selection(-page, &filter),
        KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.board.move_selection(page / 2, &filter);
        }
        KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.board.move_selection(-(page / 2), &filter);
        }
        KeyCode::Home => app.board.select_first(&filter),
        KeyCode::End => app.board.select_last(&filter),
        KeyCode::Char('G') if browsing => app.board.select_last(&filter),
        KeyCode::Char('g') if browsing => {
            if app.pending_g {
                app.pending_g = false;
                app.board.select_first(&filter);
            } else {
                app.pending_g = true;
            }
        }
        KeyCode::Enter => return Some(board_attach_action(app)),
        KeyCode::Tab => app.board.cycle_agent(1, &filter),
        KeyCode::BackTab => app.board.cycle_agent(-1, &filter),
        KeyCode::Char(']') if browsing => app.board.cycle_agent(1, &filter),
        KeyCode::Char('[') if browsing => app.board.cycle_agent(-1, &filter),
        KeyCode::Char('p') if browsing => return Some(board_prompt_action(app, false)),
        KeyCode::Char('P') if browsing => return Some(board_prompt_action(app, true)),
        KeyCode::Char('R') if browsing => return Some(board_abort_action(app, false)),
        KeyCode::Char('A') if browsing => return Some(board_abort_action(app, true)),
        KeyCode::Char('m') if browsing => return Some(board_message_action(app)),
        KeyCode::Char('b' | 'M') if browsing => {
            if board_collaboration_unsupported(app) {
                return Some(Action::None);
            }
            return Some(Action::OpenCollaborationMailbox);
        }
        KeyCode::Char('i') if browsing => return Some(board_claim_action(app)),
        KeyCode::Char('c') if browsing => return Some(board_copy_action(app)),
        KeyCode::Char('K') if browsing => return Some(board_kill_action(app)),
        _ => return None,
    }
    Some(Action::None)
}
