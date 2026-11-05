use super::{
    topology_key_label, Action, App, ConfirmAction, ConfirmPopup, HashSet, KeyCode, PaneKey,
    PreviewState, QuickAction, TopologyNodeKey, TopologyNodeRef, WatchRow, WorkRow,
};

fn preview_targets_for_pane(app: &App, pane_id: &str) -> Vec<String> {
    let Some(row) = app.rows.iter().find(|row| row.contains_pane(pane_id)) else {
        return Vec::new();
    };
    match row {
        WatchRow::Agent(a) => a.pane.clone().into_iter().collect(),
        WatchRow::BarePane(p) => vec![p.pane_id.clone()],
        WatchRow::Work(s) => work_preview_targets(s),
    }
}

pub(super) fn work_preview_targets(s: &WorkRow) -> Vec<String> {
    let agent_panes: HashSet<String> = s.agents.iter().filter_map(|a| a.pane.clone()).collect();
    let mut seen = HashSet::new();
    let mut targets = Vec::new();

    for pane in &s.pane_ids {
        if agent_panes.contains(pane) && seen.insert(pane.clone()) {
            targets.push(pane.clone());
        }
    }
    for agent in &s.agents {
        let Some(pane) = agent.pane.as_ref() else {
            continue;
        };
        if seen.insert(pane.clone()) {
            targets.push(pane.clone());
        }
    }
    if targets.is_empty() {
        if let Some(pane) = s.representative_pane.as_ref() {
            targets.push(pane.clone());
        }
    }

    targets
}

pub(super) fn preview_target_position(app: &App, pane_id: &str) -> Option<(usize, usize)> {
    let targets = preview_targets_for_pane(app, pane_id);
    let idx = targets.iter().position(|target| target == pane_id)?;
    Some((idx + 1, targets.len()))
}

fn topology_preview_target_position(app: &App, pane_key: &PaneKey) -> Option<(usize, usize)> {
    let TopologyNodeRef::Window(window) = app
        .topology
        .find(&TopologyNodeKey::Window(pane_key.window.clone()))?
    else {
        return None;
    };
    let targets: Vec<_> = window
        .panes
        .iter()
        .filter(|pane| pane.agent.is_some())
        .map(|pane| &pane.key)
        .collect();
    let index = targets
        .iter()
        .position(|candidate| *candidate == pane_key)?;
    Some((index + 1, targets.len()))
}

pub(super) fn preview_position(app: &App, preview: &PreviewState) -> Option<(usize, usize)> {
    preview.pane_key.as_ref().map_or_else(
        || preview_target_position(app, &preview.pane_id),
        |key| topology_preview_target_position(app, key),
    )
}

pub(super) fn preview_label(app: &App, preview: &PreviewState) -> String {
    preview.pane_key.as_ref().map_or_else(
        || app.pane_label(&preview.pane_id),
        |key| {
            format!(
                "{} · {}:{}",
                key.pane_id, key.window.session.endpoint.host, key.window.session.endpoint.socket
            )
        },
    )
}

fn cycle_preview_agent(app: &mut App, delta: isize) {
    if let Some(current_key) = app
        .preview
        .as_ref()
        .and_then(|preview| preview.pane_key.clone())
    {
        let Some(TopologyNodeRef::Window(window)) = app
            .topology
            .find(&TopologyNodeKey::Window(current_key.window.clone()))
        else {
            return;
        };
        let targets: Vec<PaneKey> = window
            .panes
            .iter()
            .filter(|pane| pane.agent.is_some())
            .map(|pane| pane.key.clone())
            .collect();
        if targets.len() <= 1 {
            return;
        }
        let Some(current_index) = targets.iter().position(|key| key == &current_key) else {
            return;
        };
        let next_index = if delta >= 0 {
            (current_index + 1) % targets.len()
        } else if current_index == 0 {
            targets.len() - 1
        } else {
            current_index - 1
        };
        if let Some(preview) = app.preview.as_mut() {
            preview.pane_id.clone_from(&targets[next_index].pane_id);
            preview.pane_key = Some(targets[next_index].clone());
            preview.scroll = 0;
        }
        app.pane_capture = None;
        return;
    }
    let Some(current) = app.preview.as_ref().map(|p| p.pane_id.clone()) else {
        return;
    };
    let targets = preview_targets_for_pane(app, &current);
    if targets.len() <= 1 {
        return;
    }
    let Some(current_idx) = targets.iter().position(|pane| pane == &current) else {
        return;
    };
    let next_idx = if delta >= 0 {
        (current_idx + 1) % targets.len()
    } else if current_idx == 0 {
        targets.len() - 1
    } else {
        current_idx - 1
    };
    let next_pane = targets[next_idx].clone();

    let mut changed = false;
    if let Some(preview) = app.preview.as_mut() {
        if preview.pane_id != next_pane {
            preview.pane_id = next_pane;
            preview.scroll = 0;
            changed = true;
        }
    }
    if changed {
        app.pane_capture = None;
    }
}

/// Preview mode scrolls the overlay instead of the table and opens the
/// prompt composer against the preview-pinned pane, not the table cursor.
pub(super) fn handle_preview_event(code: KeyCode, app: &mut App) -> Action {
    if matches!(code, KeyCode::Enter) {
        let preview = app.preview.as_ref().expect("preview present");
        return preview.pane_key.as_ref().map_or_else(
            || Action::AttachPane(preview.pane_id.clone()),
            |key| Action::AttachTopologyPane(key.clone()),
        );
    }

    match code {
        KeyCode::Char('q' | 'p' | 'o') | KeyCode::Esc => Action::ClosePreview,
        KeyCode::Char('f') => Action::TogglePreviewMode,
        KeyCode::Char('c') => Action::TogglePreviewContent,
        KeyCode::Char('r') => Action::Refresh,
        KeyCode::Char(']') | KeyCode::Tab => {
            cycle_preview_agent(app, 1);
            Action::None
        }
        KeyCode::Char('[') | KeyCode::BackTab => {
            cycle_preview_agent(app, -1);
            Action::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let preview = app.preview.as_mut().expect("preview present");
            preview.scroll = preview.scroll.saturating_add(1);
            Action::None
        }
        KeyCode::Up | KeyCode::Char('k') => {
            let preview = app.preview.as_mut().expect("preview present");
            preview.scroll = preview.scroll.saturating_sub(1);
            Action::None
        }
        KeyCode::PageDown => {
            let preview = app.preview.as_mut().expect("preview present");
            preview.scroll = preview.scroll.saturating_add(10);
            Action::None
        }
        KeyCode::PageUp => {
            let preview = app.preview.as_mut().expect("preview present");
            preview.scroll = preview.scroll.saturating_sub(10);
            Action::None
        }
        KeyCode::Home => {
            let preview = app.preview.as_mut().expect("preview present");
            preview.scroll = 0;
            Action::None
        }
        _ => Action::None,
    }
}

/// Resolve a `K` keystroke against the current selection. Yields a
/// confirm popup when the row qualifies (Agent with a tmux pane), a
/// "not applicable" hint otherwise. Pulled into its own helper so
/// the same logic can be unit-tested without the `handle_event`
/// keystroke matrix in the way.
pub(crate) fn quick_kill_action(app: &App) -> Action {
    if app.uses_tree() {
        return match app.selected_node_key() {
            Some(key) => Action::AskConfirm(ConfirmPopup {
                message: format!("Close {}?", topology_key_label(&key)),
                on_confirm: ConfirmAction::Quick(QuickAction::TerminateNode(key)),
            }),
            None => Action::NotApplicable("close: no topology node selected"),
        };
    }
    if app
        .selected_target()
        .is_some_and(|target| target.agent_idx.is_some())
    {
        return match app.selected_agent().and_then(|agent| agent.pane.as_deref()) {
            Some(pane_id) => Action::AskConfirm(ConfirmPopup {
                message: format!("Kill pane {}?", app.pane_label(pane_id)),
                on_confirm: ConfirmAction::Quick(QuickAction::KillPane(pane_id.to_string())),
            }),
            None => Action::NotApplicable("kill: no tmux pane on this row"),
        };
    }
    match app.selected_row() {
        Some(WatchRow::Agent(a)) => match a.pane.as_deref() {
            Some(pane_id) => Action::AskConfirm(ConfirmPopup {
                message: format!("Kill pane {}?", app.pane_label(pane_id)),
                on_confirm: ConfirmAction::Quick(QuickAction::KillPane(pane_id.to_string())),
            }),
            // Agent with no pane (Claude SDK sub-process whose
            // ancestry walk failed). `K` would be a no-op — surface
            // why.
            None => Action::NotApplicable("kill: no tmux pane on this row"),
        },
        Some(WatchRow::BarePane(p)) => Action::AskConfirm(ConfirmPopup {
            message: format!(
                "Kill pane {}:{}.{}?",
                p.session, p.window_index, p.pane_index
            ),
            on_confirm: ConfirmAction::Quick(QuickAction::KillPane(p.pane_id.clone())),
        }),
        Some(WatchRow::Work(s)) => match s.representative_pane.as_deref() {
            Some(pane_id) => Action::AskConfirm(ConfirmPopup {
                message: format!("Kill pane {}?", app.pane_label(pane_id)),
                on_confirm: ConfirmAction::Quick(QuickAction::KillPane(pane_id.to_string())),
            }),
            None => Action::NotApplicable("kill: no tmux pane on this row"),
        },
        None => Action::NotApplicable("kill: no row selected"),
    }
}

/// Resolve `R` (abort current turn). Same shape as `quick_kill_action`
/// but the destructive verb in the popup says "Abort" instead of "Kill".
pub(crate) fn quick_abort_action(app: &App) -> Action {
    if app.uses_tree() {
        return match app.selected_exact_pane() {
            Some(pane) if pane.agent.is_some() => Action::AskConfirm(ConfirmPopup {
                message: format!(
                    "Interrupt {}?",
                    topology_key_label(&TopologyNodeKey::Pane(pane.key.clone()))
                ),
                on_confirm: ConfirmAction::Quick(QuickAction::InterruptPane(pane.key.clone())),
            }),
            Some(_) => Action::NotApplicable("interrupt: pane has no tracked agent"),
            None => Action::NotApplicable("interrupt: select an exact pane"),
        };
    }
    match app.selected_row() {
        Some(WatchRow::Agent(_) | WatchRow::Work(_)) => {
            match app.selected_agent().and_then(|agent| agent.pane.as_deref()) {
                Some(pane_id) => Action::AskConfirm(ConfirmPopup {
                    message: format!("Abort current turn in {}?", app.pane_label(pane_id)),
                    on_confirm: ConfirmAction::Quick(QuickAction::AbortTurn(pane_id.to_string())),
                }),
                None => Action::NotApplicable("abort: not a tracked agent"),
            }
        }
        // Bare panes have no agent state to abort — Ctrl-C would still
        // reach the foreground process, but it's no longer a "muxa
        // agent action" in any meaningful sense. Skip rather than
        // surprise.
        Some(WatchRow::BarePane(_)) => Action::NotApplicable("abort: not a tracked agent"),
        None => Action::NotApplicable("abort: no row selected"),
    }
}

/// Resolve table-mode Enter: attach to the selected pane, immediately.
///
/// This used to open an inline prompt first, with an empty second Enter
/// meaning "attach after all" — a two-step riddle for the most common
/// action in the TUI. Typing at an agent now lives in the `m` composer,
/// whose Ctrl-E `just send` mode does what the prompt popup did.
pub(crate) fn quick_prompt_action(app: &App) -> Action {
    if app.uses_tree() {
        return match app.selected_action_pane() {
            Some(pane) => Action::AttachTopologyPane(pane.key.clone()),
            None => Action::NotApplicable("attach: selected node has no pane"),
        };
    }
    match app.selected_pane() {
        Some(pane_id) => Action::AttachPane(pane_id),
        None => Action::NotApplicable("attach: no tmux pane on this row"),
    }
}

/// Resolve `c` (copy last prompt). Non-destructive, so no confirm —
/// dispatches straight to `Quick`. Vetoes when there's no
/// prompt to copy so the user gets a hint instead of a silent no-op.
pub(crate) fn quick_copy_action(app: &App) -> Action {
    match app.selected_last_prompt() {
        Some(p) if !p.is_empty() => Action::Quick(QuickAction::CopyPrompt(p.to_string())),
        Some(_) | None => Action::NotApplicable("copy: no prompt on this row"),
    }
}

// ---- rendering ------------------------------------------------------------
