use super::{
    board_screen, pane_target_label, topology_key_label, Action, ActionOutcome, Agent, App,
    CollaborationComposeTarget, CollaborationComposer, CollaborationOrigin, ConfirmAction,
    ConfirmPopup, HintLevel, MailboxAnchor, PaneKey, PreviewMode, PreviewState, QuickAction,
    Result, TopologyNodeKey,
};

pub(super) fn open_preview_for(app: &mut App, key: PaneKey) {
    app.preview = Some(PreviewState {
        pane_id: key.pane_id.clone(),
        pane_key: Some(key),
        scroll: 0,
        mode: PreviewMode::Popup,
        content: app.watch_cfg.preview.default_content,
    });
}

/// A board operation awaiting its submission or confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum BoardPending {
    /// A prompt composer for exactly these panes.
    Compose(Vec<PaneKey>),
    /// A confirmation for exactly this action.
    Confirm(QuickAction),
    /// A collaboration message to exactly this agent: its pane and the
    /// session running in it, so a replacement agent in a reused pane is
    /// never the one that receives it.
    Message { pane: PaneKey, session: String },
}

/// What a board action addresses, frozen when its composer or confirmation
/// opens: the exact panes, the intended agents that no longer resolve, and a
/// label naming them.
struct BoardTargets {
    panes: Vec<PaneKey>,
    unavailable: Vec<String>,
    label: String,
}

/// The agent under the cursor, or every live agent of the selected Work.
/// Each must resolve through the topology, so no raw pane id and no same-id
/// pane on another server is ever targeted; ones that do not resolve are
/// kept as `unavailable` rather than silently dropped.
fn board_agent_targets(app: &App, all: bool) -> Result<BoardTargets, String> {
    let filter = &app.search_query;
    let Some(work) = app.board.selected_work(filter) else {
        return Err("no Work selected".into());
    };
    let work_label = format!("{}/{}", work.identity.workspace_id, work.identity.work_id);
    let agents = board_screen::live_agents(work);
    if agents.is_empty() {
        return Err(format!("{work_label} has no live agent"));
    }
    let chosen: Vec<_> = if all {
        agents
    } else {
        agents
            .get(app.board.selected_agent_index(filter))
            .copied()
            .into_iter()
            .collect()
    };
    let mut panes: Vec<PaneKey> = Vec::new();
    let mut unavailable = Vec::new();
    for (run, pane) in chosen {
        match app.board_pane_key(run, &pane.pane_id) {
            Some(key) if !panes.contains(&key) => panes.push(key),
            Some(_) => {}
            None => unavailable.push(format!("{}:{}", run.execution.socket, pane.pane_id)),
        }
    }
    if panes.is_empty() {
        return Err(format!("{work_label}'s agents are no longer open"));
    }
    let label = if all {
        let missing = if unavailable.is_empty() {
            String::new()
        } else {
            format!(", {} unavailable", unavailable.len())
        };
        format!(
            "{work_label} · all {} agents{missing}",
            panes.len() + unavailable.len()
        )
    } else {
        format!(
            "{work_label} · {}",
            topology_key_label(&TopologyNodeKey::Pane(panes[0].clone()))
        )
    };
    Ok(BoardTargets {
        panes,
        unavailable,
        label,
    })
}

/// `p` / `P` on the board: a keystrokes composer for the agent under the
/// cursor, or for every live agent of the Work.
pub(super) fn board_prompt_action(app: &mut App, all: bool) -> Action {
    match board_agent_targets(app, all) {
        Ok(BoardTargets {
            mut panes,
            unavailable,
            label,
        }) => {
            if let Some(busy) = busy_target(app, &panes) {
                app.set_hint(still_sending(&busy), HintLevel::Warn);
                return Action::None;
            }
            app.board_pending = Some(BoardPending::Compose(panes.clone()));
            let target = if all {
                CollaborationComposeTarget::PromptPanes { panes, unavailable }
            } else {
                CollaborationComposeTarget::PromptTopology {
                    pane: panes.remove(0),
                }
            };
            app.collaboration_composer = Some(CollaborationComposer::new(target, label));
            Action::None
        }
        Err(hint) => {
            app.set_hint(hint, HintLevel::Warn);
            Action::None
        }
    }
}

/// `R` / `A` on the board: interrupt the agent under the cursor, or every
/// live agent of the Work, after a confirmation naming them.
pub(super) fn board_abort_action(app: &mut App, all: bool) -> Action {
    match board_agent_targets(app, all) {
        Ok(BoardTargets {
            mut panes,
            unavailable,
            label,
        }) => {
            if let Some(busy) = busy_target(app, &panes) {
                app.set_hint(still_sending(&busy), HintLevel::Warn);
                return Action::None;
            }
            let quick = if all {
                QuickAction::InterruptPanes { panes, unavailable }
            } else {
                QuickAction::InterruptPane(panes.remove(0))
            };
            app.board_pending = Some(BoardPending::Confirm(quick.clone()));
            Action::AskConfirm(ConfirmPopup {
                message: format!("Interrupt {label}?"),
                on_confirm: ConfirmAction::Quick(quick),
            })
        }
        Err(hint) => {
            app.set_hint(hint, HintLevel::Warn);
            Action::None
        }
    }
}

/// `c` on the board: copy the cursor agent's last prompt.
pub(super) fn board_copy_action(app: &mut App) -> Action {
    match board_cursor_agent(app) {
        Ok((_, agent)) => {
            if let Some(prompt) = agent.last_prompt.filter(|prompt| !prompt.is_empty()) {
                Action::Quick(QuickAction::CopyPrompt(prompt))
            } else {
                app.set_hint("the selected agent has no prompt to copy", HintLevel::Warn);
                Action::None
            }
        }
        Err(hint) => {
            app.set_hint(hint, HintLevel::Warn);
            Action::None
        }
    }
}

/// `K` on the board: close the cursor agent's pane after a confirmation
/// naming it. Unlike `R`, this ends the process, not just its turn.
pub(super) fn board_kill_action(app: &mut App) -> Action {
    let key = match board_cursor_agent(app) {
        Ok((key, _)) => key,
        Err(hint) => {
            app.set_hint(hint, HintLevel::Warn);
            return Action::None;
        }
    };
    if busy_target(app, std::slice::from_ref(&key)).is_some() {
        app.set_hint(still_sending(&key), HintLevel::Warn);
        return Action::None;
    }
    let quick = QuickAction::TerminateNode(TopologyNodeKey::Pane(key.clone()));
    app.board_pending = Some(BoardPending::Confirm(quick.clone()));
    Action::AskConfirm(ConfirmPopup {
        message: format!(
            "Close {} and end its agent?",
            topology_key_label(&TopologyNodeKey::Pane(key))
        ),
        on_confirm: ConfirmAction::Quick(quick),
    })
}

/// Whether `action` is exactly what the pending board operation was opened
/// for: its confirmation, or its composer's submission to the frozen panes.
/// Anything else — a topology action that happens to name one of those panes
/// included — is not the board's.
pub(super) fn board_pending_matches(pending: Option<&BoardPending>, action: &Action) -> bool {
    match (pending, action) {
        (
            Some(BoardPending::Confirm(frozen)),
            Action::AskConfirm(ConfirmPopup {
                on_confirm: ConfirmAction::Quick(quick),
                ..
            }),
        ) => quick == frozen,
        (
            Some(BoardPending::Compose(frozen)),
            Action::Quick(QuickAction::SendPromptTo { pane, .. }),
        ) => frozen.as_slice() == std::slice::from_ref(pane),
        (
            Some(BoardPending::Compose(frozen)),
            Action::Quick(QuickAction::SendPromptToMany { panes, .. }),
        ) => frozen == panes,
        _ => false,
    }
}

/// The agent under the board's cursor, exactly: its topology pane and the
/// agent record, or why there is none.
pub(super) fn board_cursor_agent(app: &App) -> Result<(PaneKey, Agent), String> {
    let filter = &app.search_query;
    let Some(work) = app.board.selected_work(filter) else {
        return Err("no Work selected".into());
    };
    let label = format!("{}/{}", work.identity.workspace_id, work.identity.work_id);
    let agents = board_screen::live_agents(work);
    let Some((run, pane)) = agents.get(app.board.selected_agent_index(filter)).copied() else {
        return Err(format!("{label} has no live agent"));
    };
    let Some(agent) = pane.agent.clone() else {
        return Err(format!("{label} has no live agent"));
    };
    let Some(key) = app.board_pane_key(run, &pane.pane_id) else {
        return Err(format!("{label}'s agent is no longer open"));
    };
    Ok((key, agent))
}

/// Whose mailbox the board shows: the agent under its cursor, by pane and
/// agent session, so a different agent later started in that pane is not it.
pub(super) fn board_mailbox_anchor(app: &App) -> Option<MailboxAnchor> {
    let (key, agent) = board_cursor_agent(app).ok()?;
    let endpoint = &key.window.session.endpoint;
    Some(MailboxAnchor {
        origin: CollaborationOrigin {
            pane: key.pane_id.clone(),
            socket: matches!(endpoint.host, tend::HostKind::Tmux | tend::HostKind::Rmux)
                .then(|| endpoint.socket.clone()),
            console: false,
            agent_session_id: Some(agent.session_id.clone()),
            exact_session: true,
        },
        label: format!("{}@{}", agent.kind, key.pane_id),
    })
}

/// Refuse board collaboration when the daemon cannot pin an exact agent
/// session: the fallback it would use acts as whoever holds the pane.
pub(super) fn board_collaboration_unsupported(app: &mut App) -> bool {
    if app.exact_session_supported {
        return false;
    }
    app.set_hint(
        "this tendd cannot pin an exact agent session — restart it onto this build",
        HintLevel::Err,
    );
    true
}

/// `m` on the board: message the agent under the cursor. Its identity is
/// frozen now; the composer opens for exactly that agent, and the message
/// is sent only if the same agent session still runs in that pane.
pub(super) fn board_message_action(app: &mut App) -> Action {
    if board_collaboration_unsupported(app) {
        return Action::None;
    }
    match board_cursor_agent(app) {
        Ok((pane, agent)) => {
            app.board_pending = Some(BoardPending::Message {
                pane,
                session: agent.session_id,
            });
            Action::OpenCollaborationMessage
        }
        Err(hint) => {
            app.set_hint(hint, HintLevel::Warn);
            Action::None
        }
    }
}

/// `i` on the board: claim the cursor agent's queued requests, acting as
/// that agent — the recipient — never as the console.
pub(super) fn board_claim_action(app: &mut App) -> Action {
    if board_collaboration_unsupported(app) {
        return Action::None;
    }
    if let Some(anchor) = board_mailbox_anchor(app) {
        Action::ClaimBoardInbox {
            origin: anchor.origin,
            label: anchor.label,
        }
    } else {
        app.set_hint(
            board_cursor_agent(app).err().unwrap_or_default(),
            HintLevel::Warn,
        );
        Action::None
    }
}

/// Why a board message composed for one agent session must not be sent
/// now: that session is no longer running anywhere watch can see. The send
/// follows the session (it may have moved panes), as the daemon does; this
/// only saves a round trip the daemon would refuse anyway.
pub(super) fn board_message_stale(app: &App) -> Option<String> {
    let Some(BoardPending::Message { pane, session }) = app.board_pending.as_ref() else {
        return None;
    };
    let running = app
        .topology
        .sessions
        .iter()
        .flat_map(|s| s.windows.iter())
        .flat_map(|w| w.panes.iter())
        .filter_map(|node| node.agent.as_ref())
        .any(|agent| &agent.session_id == session);
    (!running).then(|| {
        format!(
            "the agent you were writing to (in {}) is no longer running — not sent",
            pane_target_label(pane)
        )
    })
}

/// Settle a board message after the send: delivered ends the board's
/// authority; anything else puts the draft and its frozen recipient back so
/// nothing typed is lost. It is never re-sent automatically — a send whose
/// fate is unknown may have landed.
pub(super) fn finish_board_message(
    app: &mut App,
    draft: Option<CollaborationComposer>,
    outcome: ActionOutcome,
) -> ActionOutcome {
    let Some(draft) = draft else {
        return outcome;
    };
    match outcome {
        ActionOutcome::Err(error) => {
            app.collaboration_composer = Some(draft);
            ActionOutcome::Err(format!("{error} — draft kept; Enter retries, Esc drops it"))
        }
        delivered => {
            app.board_pending = None;
            delivered
        }
    }
}

/// A target pane that a background action is still writing to.
pub(super) fn busy_target(app: &App, panes: &[PaneKey]) -> Option<PaneKey> {
    panes
        .iter()
        .find(|pane| {
            app.pending_outcomes
                .iter()
                .any(|(busy, _)| busy.contains(pane))
        })
        .cloned()
}

pub(super) fn still_sending(pane: &PaneKey) -> String {
    format!(
        "still sending to {} — try again when it finishes",
        pane_target_label(pane)
    )
}

/// Resolve `Enter` on the Work board to the exact pane of one of its Runs.
pub(super) fn board_attach_action(app: &mut App) -> Action {
    match board_selected_pane(app) {
        Ok(key) => Action::AttachTopologyPane(key),
        Err(hint) => {
            app.set_hint(hint, HintLevel::Warn);
            Action::None
        }
    }
}

/// The topology pane the selected Work leads to, or why there is none. The
/// key comes from the topology, so the same pane id on another server is
/// never the one returned.
pub(super) fn board_selected_pane(app: &App) -> Result<PaneKey, String> {
    let filter = &app.search_query;
    let Some(work) = app.board.selected_work(filter) else {
        return Err("no Work selected".into());
    };
    let label = format!("{}/{}", work.identity.workspace_id, work.identity.work_id);
    // The agent the detail marks with ▸ is where Enter and `o` go, on
    // whichever Run it is; a Work with no live agent falls back to its Run.
    let agents = board_screen::live_agents(work);
    let target = agents
        .get(app.board.selected_agent_index(filter))
        .map(|(run, pane)| (*run, pane.pane_id.as_str()))
        .or_else(|| board_screen::jump_target(work));
    let Some((run, pane_id)) = target else {
        return Err(format!("{label} has no live Run"));
    };
    // The board refreshes on its own clock; the window may have closed since.
    app.board_pane_key(run, pane_id)
        .ok_or_else(|| format!("{label}'s Run is no longer open"))
}

/// Resolve `Enter` on the collaboration screen to a pane to attach to.
pub(super) fn collab_attach_action(app: &mut App, filter: &str) -> Action {
    // Enter goes where the request went, so the peer's pane is the useful
    // end. Clone what is needed before hinting: the lookup borrows `app`.
    let peer = app.collab.selected_request(filter).map(|request| {
        let peer = if request.to.console {
            &request.from
        } else {
            &request.to
        };
        (
            peer.pane.clone(),
            peer.socket.clone(),
            peer.label(),
            peer.console,
        )
    });
    let Some((pane, socket, label, console)) = peer else {
        return Action::None;
    };
    if console {
        app.set_hint("that request never left the console", HintLevel::Warn);
        return Action::None;
    }
    if let Some(key) = app.pane_key_for(&pane, socket.as_deref()) {
        return Action::AttachTopologyPane(key);
    }
    // Requests outlive their panes, so this is ordinary rather than an error:
    // say which pane is gone instead of leaving Enter silent.
    app.set_hint(format!("{label} is no longer on any pane"), HintLevel::Warn);
    Action::None
}
