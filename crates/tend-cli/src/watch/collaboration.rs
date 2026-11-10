use super::{
    board_cursor_agent, board_mailbox_anchor, board_screen, history_for_scope, interaction_session,
    read_board_open_requests, topology_key_label, ActionOutcome, ActivityEntry, Agent, AgentState,
    App, BoardPending, BoardScan, BroadcastReport, BroadcastRow, Client,
    CollaborationComposeTarget, CollaborationComposer, CollaborationMailboxTab, CollaborationMark,
    CollaborationOrigin, CollaborationRequest, Color, ComposeSendMode, Duration, HashMap,
    HintLevel, HumanInteractionEntry, HumanInteractionInput, HumanInteractionKind, MailboxAnchor,
    MailboxHistoryScope, MailboxScope, Modifier, NewRequest, OffsetDateTime, PaneKey, PaneNode,
    Participant, Path, RequestKind, RequestMailbox, RequestStatus, Result, RoomContext, Span,
    Style, TopologyNodeKey, TopologyNodeRef, WatchCollaboration, WatchRow, WatchScreen, WindowNode,
    WorkMode,
};

pub(crate) fn watch_collaboration_origin(initial_pane: Option<String>) -> CollaborationOrigin {
    watch_collaboration_origin_from(
        initial_pane,
        std::env::var("RMUX").ok(),
        std::env::var("TMUX").ok(),
    )
}

/// `tend watch` sends as the operator console.
///
/// The human at the keyboard is the sender — not whichever agent happens to
/// occupy the pane the popup was opened from. The pane still rides along: it
/// names the room the console is looking at and is the provenance evidence for
/// who dialled. But it no longer supplies the *identity*, which is why the
/// console can address that pane too, and why a console opened from a bare
/// shell still messages. A pane we cannot resolve therefore degrades to a
/// pane-less console rather than to no collaboration at all.
pub(super) fn watch_collaboration_origin_from(
    initial_pane: Option<String>,
    rmux: Option<String>,
    tmux: Option<String>,
) -> CollaborationOrigin {
    let pane = initial_pane.filter(|pane| !pane.is_empty());
    let socket = pane.as_deref().and_then(|pane| {
        let endpoint = match tend::backend::pane_id_host_kind(pane)? {
            tend::HostKind::Cmux => Some(tend::backend::cmux::endpoint_from_env()),
            tend::HostKind::Rmux => rmux,
            tend::HostKind::Tmux => tmux,
            tend::HostKind::Zellij | tend::HostKind::Herdr => None,
        }?;
        let path = endpoint.split(',').next()?.trim();
        (!path.is_empty()).then(|| tend::backend::pane_endpoint_identity(Some(pane), path))
    });
    CollaborationOrigin {
        exact_session: false,
        pane: pane.unwrap_or_default(),
        socket,
        console: true,
        agent_session_id: None,
    }
}

pub(super) fn collaboration_open_hint() -> &'static str {
    "collaboration unavailable — enable [collaboration].enabled and restart tendd"
}

/// Translate a daemon error into something the operator can act on.
///
/// Matched against the substring the error actually carries: it reads
/// "collaboration origin is not a **hook-correlated** tracked pane agent", so
/// the old pattern never fired and the raw wording reached the hint area. A
/// console origin does not need a tracked agent, so seeing this at all now
/// means the daemon predates the console — say that rather than repeating
/// advice about focusing an agent pane.
fn friendly_watch_collaboration_error(error: &str) -> String {
    if error.contains("origin is not") && error.contains("tracked pane agent") {
        return "tendd is too old to accept console messages — restart it after `tend upgrade`"
            .into();
    }
    error.into()
}

/// Load every room's traffic for the collaboration screen.
///
/// Console-scoped by construction: the daemon refuses a widened listing to a
/// pane agent, and the operator sitting in front of `tend watch` is exactly
/// the caller that is entitled to it. Skipped whenever the screen is not on
/// show, so a topology-only session never pays for it.
pub(super) async fn read_watch_history(
    client: &Client,
    origin: &CollaborationOrigin,
    full: bool,
) -> Result<Vec<CollaborationRequest>, String> {
    let view = if full {
        tend::collaboration::CollaborationReadView::Full
    } else {
        tend::collaboration::CollaborationReadView::Preview
    };
    let mut query = tend::collaboration::CollaborationQuery {
        limit: Some(if full { 8 } else { 500 }),
        max_bytes: full.then_some(4 * 1024 * 1024),
        ..Default::default()
    };
    let mut requests = Vec::new();
    loop {
        let result = client
            .collaboration_query_result(
                origin,
                RequestMailbox::All,
                MailboxScope::All,
                &query,
                0,
                view,
                false,
            )
            .await
            .map_err(|error| error.to_string())?;
        if let Some(mut legacy) = result.legacy_requests {
            for request in &mut legacy {
                view.apply(request);
            }
            return Ok(legacy);
        }
        if full && result.byte_budget_supported {
            query.limit = Some(500);
        }
        let page = result.page;
        requests.extend(page.requests);
        if !page.has_more {
            return Ok(requests);
        }
        query.cursor = Some(
            page.next_cursor
                .ok_or("history page omitted its continuation cursor")?,
        );
    }
}

pub(super) async fn reconcile_search_history(
    client: &Client,
    origin: &CollaborationOrigin,
    previews: &[CollaborationRequest],
    previous: &[CollaborationRequest],
) -> Result<Vec<CollaborationRequest>, String> {
    use futures_util::{StreamExt, TryStreamExt};
    let by_id: HashMap<_, _> = previous
        .iter()
        .map(|request| (request.id.as_str(), request))
        .collect();
    futures_util::stream::iter(previews)
        .map(|summary| {
            let by_id = &by_id;
            async move {
                if let Some(full) = by_id.get(summary.id.as_str()) {
                    let mut projected = (*full).clone();
                    tend::collaboration::CollaborationReadView::Preview.apply(&mut projected);
                    if &projected == summary {
                        return Ok((*full).clone());
                    }
                }
                client
                    .collaboration_peek(origin, &summary.id)
                    .await
                    .map_err(|error| error.to_string())
            }
        })
        .buffered(4)
        .try_collect()
        .await
}

pub(super) fn collaboration_revision_updates(
    client: Client,
) -> tokio::sync::watch::Receiver<Option<u64>> {
    let (sender, receiver) = tokio::sync::watch::channel(None);
    tokio::spawn(async move {
        let Ok(mut stream) = client.collaboration_requests_subscribe().await else {
            return;
        };
        sender.send_replace(Some(0));
        let mut epoch = 0_u64;
        loop {
            tokio::select! {
                update = stream.recv() => match update {
                    Ok(Some(_)) => { epoch = epoch.wrapping_add(1); sender.send_replace(Some(epoch)); }
                    _ => return,
                },
                () = sender.closed() => return,
            }
        }
    });
    receiver
}

pub(super) async fn watch_history(
    client: &Client,
    app: &mut App,
    origin: &CollaborationOrigin,
) -> Result<Vec<CollaborationRequest>, String> {
    if app
        .collaboration_history
        .changes
        .as_ref()
        .is_none_or(|changes| changes.has_changed().is_err())
    {
        let mut changes = collaboration_revision_updates(client.clone());
        // Establish the subscription before the first snapshot. Otherwise the
        // later ACK changes None to Some(0) and repeats the entire history read.
        // The client bounds the handshake; rejection closes this channel and
        // retains the legacy polling fallback. Events racing the read stay dirty.
        let _ = changes.changed().await;
        app.collaboration_history.changes = Some(changes);
        app.collaboration_history.revision = None;
    }
    // Read the revision before the snapshot. A racing update remains dirty.
    let revision = app
        .collaboration_history
        .changes
        .as_mut()
        .and_then(|changes| *changes.borrow_and_update());
    let previews = if revision.is_some() && revision == app.collaboration_history.revision {
        app.collaboration_history.previews.clone()
    } else {
        read_watch_history(client, origin, false).await?
    };
    let changed = previews != app.collaboration_history.previews;
    let searching =
        !app.search_query.is_empty() && matches!(app.watch_cfg.screen, WatchScreen::Collab);
    if searching {
        if changed || app.collaboration_history.search_full.is_none() {
            let full = if let Some(previous) = app.collaboration_history.search_full.as_deref() {
                reconcile_search_history(client, origin, &previews, previous).await?
            } else {
                read_watch_history(client, origin, true).await?
            };
            app.collaboration_history.search_full = Some(full);
        }
        app.collaboration_history.previews = previews;
        app.collaboration_history.revision = revision;
        return Ok(app
            .collaboration_history
            .search_full
            .clone()
            .unwrap_or_default());
    }
    if changed {
        app.collaboration_history.search_full = None;
    }
    app.collaboration_history.previews = previews;
    app.collaboration_history.revision = revision;
    Ok(app.collaboration_history.previews.clone())
}

fn detail_matches(summary: &CollaborationRequest, full: &CollaborationRequest) -> bool {
    summary.id == full.id
        && summary.status == full.status
        && summary.from == full.from
        && summary.to == full.to
        && summary.reply.as_ref().map(|reply| reply.at) == full.reply.as_ref().map(|reply| reply.at)
        && summary.updates.last().map(|update| update.sequence)
            == full.updates.last().map(|update| update.sequence)
}

pub(super) fn detail_for<'a>(
    app: &'a App,
    request: &'a CollaborationRequest,
) -> &'a CollaborationRequest {
    app.collaboration_history
        .detail
        .as_ref()
        .filter(|full| detail_matches(request, full))
        .unwrap_or(request)
}

pub(super) async fn refresh_selected_collaboration_detail(client: &Client, app: &mut App) {
    let request = if app.collaboration_mailbox.open {
        collaboration_requests(app).get(app.collaboration_mailbox.selected)
    } else if matches!(app.watch_cfg.screen, WatchScreen::Collab) {
        app.collab.selected_request(&app.search_query)
    } else {
        None
    };
    let Some(request) = request else {
        return;
    };
    if app
        .collaboration_history
        .detail
        .as_ref()
        .is_some_and(|full| detail_matches(request, full))
    {
        return;
    }
    if let Some(full) = app
        .collaboration_history
        .search_full
        .as_ref()
        .and_then(|history| history.iter().find(|full| detail_matches(request, full)))
    {
        app.collaboration_history.detail = Some(full.clone());
        return;
    }
    let id = request.id.clone();
    if app
        .collaboration_history
        .detail_failure
        .as_ref()
        .is_some_and(|(failed, at)| *failed == id && at.elapsed() < Duration::from_secs(2))
    {
        return;
    }
    let Some(origin) = app.collaboration.origin.clone() else {
        return;
    };
    let origin = CollaborationOrigin {
        console: true,
        ..origin
    };
    match client.collaboration_peek(&origin, &id).await {
        Ok(full) => {
            app.collaboration_history.detail = Some(full);
            app.collaboration_history.detail_failure = None;
        }
        Err(error) => {
            app.collaboration_history.detail_failure = Some((id, std::time::Instant::now()));
            app.set_hint(
                format!("conversation detail unavailable: {error}"),
                HintLevel::Err,
            );
        }
    }
}

/// How often an open Work board rescans; Works change on human timescales.
pub(super) const BOARD_REFRESH_INTERVAL: Duration = Duration::from_secs(3);

/// Build the Work board from the same projection `tend dashboard` and the
/// web dashboard use: Work records joined with every backend's windows. Issue
/// marks found on panes are attached for display (the daemon persists them).
pub(super) async fn scan_board(agents: Vec<Agent>) -> BoardScan {
    let now = OffsetDateTime::now_utc();
    let backends = std::sync::Arc::new(tend::active_backends());
    let scan = tend::dashboard::multi_host_pane_scan(backends).await;
    let notes = scan
        .errors
        .iter()
        .map(|error| format!("scan {}: {}", error.socket.display(), error.message))
        .collect();
    let records = tend::dashboard::load_work_records(tend::paths::default_dashboard_work_file());
    let mut snapshot = tend::work::build_snapshot(&scan.panes, &agents, &records, now);
    board_screen::attach_pane_issues(&mut snapshot, &scan.panes, scan.fetched_at);
    (snapshot, notes)
}

pub(super) fn sync_collab_screen_from_cache(app: &mut App) {
    if !matches!(app.watch_cfg.screen, WatchScreen::Collab) {
        return;
    }
    if let Some(error) = &app.collaboration.unavailable {
        app.collab.fail(error.clone());
        return;
    }
    let history = if app.search_query.is_empty() {
        app.collaboration_history.previews.clone()
    } else {
        app.collaboration_history
            .search_full
            .clone()
            .unwrap_or_default()
    };
    app.collab.set_requests(history);
}

pub(super) async fn refresh_fleet_collaboration(client: &Client, app: &mut App) {
    if !matches!(app.watch_cfg.screen, WatchScreen::Collab) {
        return;
    }
    let Some(origin) = app.collaboration.origin.clone() else {
        app.collab.fail(collaboration_open_hint().into());
        return;
    };
    let origin = CollaborationOrigin {
        console: true,
        ..origin
    };
    match watch_history(client, app, &origin).await {
        Ok(requests) => app.collab.set_requests(requests),
        Err(error) => app.collab.fail(friendly_watch_collaboration_error(&error)),
    }
}

pub(super) async fn open_watch_collaboration_mailbox(client: &Client, app: &mut App) {
    // Set visibility before refreshing: the first visible frame needs history,
    // rather than the body-free badges used while the mailbox was hidden.
    app.collaboration_mailbox.open = true;
    refresh_watch_collaboration(client, app).await;
    clamp_collaboration_mailbox(app);
}

pub(super) async fn read_human_requests(
    client: &Client,
    origin: &CollaborationOrigin,
    supported: bool,
) -> Result<Vec<CollaborationRequest>, String> {
    if !supported {
        return read_board_open_requests(client, origin).await;
    }
    let mut query = tend::collaboration::CollaborationQuery {
        human_only: true,
        limit: Some(500),
        ..Default::default()
    };
    let mut requests = Vec::new();
    loop {
        let page = client
            .collaboration_query_view(
                origin,
                RequestMailbox::All,
                MailboxScope::All,
                &query,
                0,
                tend::collaboration::CollaborationReadView::Metadata,
            )
            .await
            .map_err(|error| error.to_string())?;
        requests.extend(page.requests);
        if !page.has_more {
            return Ok(requests);
        }
        query.cursor = Some(
            page.next_cursor
                .ok_or("human-request page omitted its cursor")?,
        );
    }
}

async fn refresh_hidden_mailbox(
    client: &Client,
    app: &mut App,
    room: RoomContext,
    origin: &CollaborationOrigin,
) {
    app.collaboration.room = Some(room);
    match read_human_requests(client, origin, app.human_query_supported).await {
        Ok(open) => {
            app.collaboration.human = open
                .into_iter()
                .filter(CollaborationRequest::needs_human_response)
                .collect();
            app.collaboration.unavailable = None;
        }
        Err(error) => {
            app.collaboration.unavailable = Some(format!("mailbox unavailable: {error}"));
        }
    }
}

pub(super) async fn refresh_watch_collaboration(client: &Client, app: &mut App) {
    let selected_request_id = selected_collaboration_request(app).map(|request| request.id.clone());
    let Some(origin) = app.collaboration.origin.clone() else {
        app.collaboration = WatchCollaboration {
            unavailable: Some(collaboration_open_hint().into()),
            ..WatchCollaboration::default()
        };
        return;
    };
    let room = match client.collaboration_context(&origin).await {
        Ok(room) => room,
        Err(error) => {
            app.collaboration = WatchCollaboration {
                origin: Some(origin),
                unavailable: Some(friendly_watch_collaboration_error(&error.to_string())),
                ..WatchCollaboration::default()
            };
            return;
        }
    };
    let history_scope = mailbox_history_scope(app);
    // A console-wide read is the one primitive that can faithfully answer
    // session and window selections. Direction is reconstructed locally from
    // the selected topology scope: `to` is incoming, `from` is sent. This also
    // avoids N pane-mailbox round trips for a large session.
    let history_origin = CollaborationOrigin {
        console: true,
        ..origin.clone()
    };
    if !app.collaboration_mailbox.open && app.watch_cfg.screen != WatchScreen::Collab {
        // Hidden mailboxes need live routing and human-action badges, not
        // previews of every completed message in the daemon's history.
        refresh_hidden_mailbox(client, app, room, &history_origin).await;
        return;
    }
    let history = watch_history(client, app, &history_origin).await;
    match history {
        Ok(requests) => {
            let aggregate = history_scope
                .as_ref()
                .is_some_and(MailboxHistoryScope::aggregate);
            let incoming = history_scope
                .as_ref()
                .map_or_else(Vec::new, |scope| history_for_scope(&requests, scope));
            let sent = if aggregate {
                // Aggregate history is a union, not two directional mailboxes:
                // an internal session/window request touches both ends but
                // must still appear exactly once.
                Vec::new()
            } else {
                // Pane mode keeps the established two-owner contract:
                // incoming belongs to the selected recipient, while Sent is
                // this watch console's dispatch log across targets.
                requests
                    .iter()
                    .filter(|request| request.from.same_session(&room.current))
                    .cloned()
                    .collect()
            };
            let inbox = match &history_scope {
                Some(MailboxHistoryScope::Pane(anchor)) => Some(anchor.clone()),
                Some(MailboxHistoryScope::Window { .. } | MailboxHistoryScope::Session { .. })
                | None => None,
            };
            app.collaboration = WatchCollaboration {
                human: requests
                    .into_iter()
                    .filter(CollaborationRequest::needs_human_response)
                    .collect(),
                origin: Some(origin),
                room: Some(room),
                history_scope,
                inbox,
                incoming,
                sent,
                unavailable: None,
            };
            if aggregate && app.collaboration_mailbox.tab != CollaborationMailboxTab::Human {
                app.collaboration_mailbox.tab = CollaborationMailboxTab::Incoming;
            }
        }
        Err(error) => {
            app.collaboration = WatchCollaboration {
                origin: Some(origin),
                room: Some(room),
                history_scope,
                unavailable: Some(format!("mailbox unavailable: {error}")),
                ..WatchCollaboration::default()
            };
        }
    }
    if let Some(selected_request_id) = selected_request_id {
        if let Some(index) = collaboration_requests(app)
            .iter()
            .position(|request| request.id == selected_request_id)
        {
            app.collaboration_mailbox.selected = index;
        }
    }
    clamp_collaboration_mailbox(app);
}

/// Which topology scope `M` shows. Session and window nodes are deliberately
/// not collapsed to their active panes: their whole durable history is the
/// reason hierarchy-aware mailbox browsing exists.
pub(super) fn mailbox_history_scope(app: &App) -> Option<MailboxHistoryScope> {
    if app.watch_cfg.screen == WatchScreen::Board {
        return board_mailbox_anchor(app).map(MailboxHistoryScope::Pane);
    }
    if app.uses_tree() {
        return match app.selected_node()? {
            TopologyNodeRef::Session(session) => Some(MailboxHistoryScope::Session {
                key: session.key.clone(),
                label: session.name.clone(),
            }),
            TopologyNodeRef::Window(window) => {
                let session = app
                    .topology
                    .sessions
                    .iter()
                    .find(|session| session.key == window.key.session)
                    .map_or(window.key.session.session_id.as_str(), |session| {
                        session.name.as_str()
                    });
                Some(MailboxHistoryScope::Window {
                    key: window.key.clone(),
                    label: format!("{session}:{}", window.name),
                })
            }
            TopologyNodeRef::Pane(pane) => {
                mailbox_anchor_for_pane(pane).map(MailboxHistoryScope::Pane)
            }
        };
    }
    mailbox_anchor(app).map(MailboxHistoryScope::Pane)
}

fn mailbox_anchor_for_pane(pane: &PaneNode) -> Option<MailboxAnchor> {
    let agent = pane.agent.as_ref()?;
    let endpoint = &pane.key.window.session.endpoint;
    Some(MailboxAnchor {
        origin: CollaborationOrigin {
            exact_session: false,
            pane: pane.key.pane_id.clone(),
            socket: matches!(endpoint.host, tend::HostKind::Tmux | tend::HostKind::Rmux)
                .then(|| endpoint.socket.clone()),
            console: false,
            agent_session_id: None,
        },
        label: format!("{}@{}", agent.kind, pane.key.pane_id),
    })
}

/// Whose inbox `M` shows in legacy flat layouts: the exact agent under the
/// cursor. Canonical tree layouts use [`mailbox_history_scope`] above.
///
/// The mailbox overlay is modal, so the row cannot move while it is open — one
/// fetch per open is enough. Preserve the backend endpoint whenever the host
/// exposes one: tmux pane ids repeat on every server, so `%1` alone is not a
/// mailbox identity. Origin resolution normalizes full and short endpoint
/// spellings with the same rules as canonical topology.
fn mailbox_anchor(app: &App) -> Option<MailboxAnchor> {
    let (pane, socket, label) = {
        // Both halves come from the same agent. `selected_pane` falls back to a
        // work row's representative pane while `selected_agent` falls back to
        // its latest agent, and those are not always the same pane — pairing
        // them would label one agent's mailbox with another's name.
        let agent = app.selected_agent()?;
        let pane = agent.pane.clone()?;
        let socket = agent
            .tmux_socket
            .as_deref()
            .map(|endpoint| tend::backend::pane_endpoint_identity(Some(&pane), endpoint));
        let label = format!("{}@{pane}", agent.kind);
        (pane, socket, label)
    };
    Some(MailboxAnchor {
        origin: CollaborationOrigin {
            exact_session: false,
            pane,
            socket,
            console: false,
            agent_session_id: None,
        },
        label,
    })
}

/// A request needs a peer; keystrokes only need a pane. When the full
/// composer cannot open, `m` degrades to the keystrokes-only form against
/// the selected pane instead of refusing outright, and the reason the
/// contract modes are missing rides along as a warning hint.
pub(super) fn selected_move_target(
    app: &App,
) -> std::result::Result<(PaneKey, crate::agent_move::MoveScope), String> {
    if app.uses_tree() {
        if let Some(TopologyNodeRef::Session(session)) = app.selected_node() {
            if session.windows.len() == 1 {
                let pane = session.windows[0]
                    .active_pane()
                    .ok_or("selected window has no live panes")?;
                return Ok((pane.key.clone(), crate::agent_move::MoveScope::Window));
            }
            return Err("select a window or pane to move".into());
        }
    }
    let key = selected_move_key(app)?;
    let scope =
        if app.uses_tree() && matches!(app.selected_node(), Some(TopologyNodeRef::Window(_))) {
            crate::agent_move::MoveScope::Window
        } else {
            crate::agent_move::MoveScope::Pane
        };
    Ok((key, scope))
}

fn selected_move_key(app: &App) -> std::result::Result<PaneKey, String> {
    if app.uses_tree() {
        if matches!(app.selected_node(), Some(TopologyNodeRef::Session(_))) {
            return Err("select a window or pane to move".into());
        }
        return app
            .selected_action_pane()
            .map(|pane| pane.key.clone())
            .ok_or_else(|| "select an agent pane to move".into());
    }
    let pane = app.selected_pane().ok_or("select an agent pane to move")?;
    let agent = app.selected_agent();
    let matches: Vec<_> = app
        .panes
        .iter()
        .filter(|p| {
            p.pane_id == pane
                && agent
                    .and_then(|a| a.tmux_socket.as_deref())
                    .is_none_or(|s| p.socket.as_deref() == Some(s))
        })
        .collect();
    let p = matches.first().ok_or("selected pane is no longer live")?;
    if matches.iter().any(|other| other.socket != p.socket) {
        return Err("ambiguous pane endpoint — select it in tree layout".into());
    }
    let host = tend::backend::pane_id_host_kind(&pane).ok_or("unsupported pane backend")?;
    Ok(PaneKey::from_pane(host, p))
}

pub(super) fn open_prompt_only_composer(app: &mut App, reason: String) {
    let pane_key = if app.uses_tree() {
        app.selected_action_pane().map(|pane| pane.key.clone())
    } else {
        None
    };
    let Some(pane) = pane_key
        .as_ref()
        .map(|key| key.pane_id.clone())
        .or_else(|| app.selected_pane())
    else {
        app.set_hint("no tmux pane on this row", HintLevel::Err);
        return;
    };
    let label = pane_key.as_ref().map_or_else(
        || app.pane_label(&pane),
        |key| topology_key_label(&TopologyNodeKey::Pane(key.clone())),
    );
    app.collaboration_mailbox.open = false;
    app.collaboration_composer = Some(CollaborationComposer::new(
        pane_key.map_or(CollaborationComposeTarget::Prompt { pane }, |pane| {
            CollaborationComposeTarget::PromptTopology { pane }
        }),
        label.clone(),
    ));
    // Lead with what *works* — this composer sends, to exactly the pane the
    // user pointed at. A hint that opens with "only —" plus a warning color
    // reads as a refusal, and users reported it as "m doesn't work" while
    // the composer sat there fully functional.
    app.set_hint(
        format!("▷ typing to {label} as keystrokes ({reason})"),
        HintLevel::Ok,
    );
}

/// The primary room peer contained by the selected row.
///
/// Canonical tree panes are already sorted by numeric window index and then
/// pane index. Choosing the first live tracked peer makes session/window `m`
/// deterministic, while the composer title still exposes the exact resolved
/// recipient before anything is sent.
fn peer_inside_selected_row<'a>(app: &'a App, room: &'a RoomContext) -> Option<&'a Participant> {
    if app.uses_tree() {
        let panes = app.selected_topology_panes();
        return panes
            .into_iter()
            .filter(|pane| {
                pane.agent
                    .as_ref()
                    .is_some_and(|agent| agent.state != AgentState::Stopped)
            })
            .find_map(|pane| {
                room.peers
                    .iter()
                    .find(|peer| participant_matches_pane(peer, pane))
            });
    }
    let row = app.selected_row()?;
    room.peers
        .iter()
        .filter(|peer| row.contains_pane(&peer.pane))
        .min_by_key(|peer| {
            app.panes
                .iter()
                .find(|pane| pane.pane_id == peer.pane)
                .map_or((u32::MAX, u32::MAX), |pane| {
                    (
                        pane.window_index.parse::<u32>().unwrap_or(u32::MAX),
                        pane.pane_index.parse::<u32>().unwrap_or(u32::MAX),
                    )
                })
        })
}

fn participant_matches_pane(participant: &Participant, pane: &PaneNode) -> bool {
    participant.pane == pane.key.pane_id
        && participant.room.host == pane.key.window.session.endpoint.host.to_string()
        && participant.room.window_id == pane.key.window.window_id
        && participant
            .tmux_session_id
            .as_deref()
            .is_none_or(|session_id| session_id == pane.key.window.session.session_id)
        && participant.socket.as_deref().is_none_or(|socket| {
            tend::backend::pane_endpoints_match(
                Some(&participant.pane),
                socket,
                &pane.key.window.session.endpoint.socket,
            )
        })
}

pub(super) fn participant_matches_window(participant: &Participant, window: &WindowNode) -> bool {
    participant.room.host == window.key.session.endpoint.host.to_string()
        && participant.room.window_id == window.key.window_id
        && participant
            .tmux_session_id
            .as_deref()
            .is_none_or(|session_id| session_id == window.key.session.session_id)
        && participant.socket.as_deref().is_none_or(|socket| {
            tend::backend::pane_endpoints_match(
                window.panes.first().map(|pane| pane.key.pane_id.as_str()),
                socket,
                &window.key.session.endpoint.socket,
            )
        })
}

fn participant_pane_key(app: &App, participant: &Participant) -> Option<PaneKey> {
    let mut matches = app
        .topology
        .sessions
        .iter()
        .flat_map(|session| &session.windows)
        .flat_map(|window| &window.panes)
        .filter(|pane| participant_matches_pane(participant, pane));
    let first = matches.next()?.key.clone();
    matches.next().is_none().then_some(first)
}

/// Name the room, so "no peer here" points somewhere.
///
/// Only reachable under window scope. The room is the window `tend watch` was
/// opened from and is fixed for the lifetime of the process; selecting a
/// different row cannot change it, and the message has to say so — otherwise
/// the obvious reading of "here" is "the row I am looking at". The fix is
/// usually host scope rather than another agent, so name that.
pub(super) fn empty_room_hint(current: &Participant) -> String {
    let where_ = match (&current.tmux_session_name, &current.window_name) {
        (Some(session), Some(window)) => format!("{session}:{window}"),
        (Some(session), None) => session.clone(),
        _ => current.room.window_id.clone(),
    };
    format!(
        "no agent in {where_} — the room is the window watch was opened from, not the selected row; set [collaboration] scope = \"host\" to reach every window"
    )
}

/// Why `m` could not name a recipient.
///
/// Under host scope the room is not the reason: the cursor is the selector and
/// the table spans every session on the host, so `host_scope_target` has
/// already had its say and the only thing left to report is that this row does
/// does not contain a live tracked agent. Naming the launch window's peers there
/// would point the operator at an unrelated window.
fn no_target_hint(app: &App, room: &RoomContext) -> String {
    if app.collaboration_scope == tend::config::CollaborationScope::Host {
        return "this row has no live tracked agent to message".into();
    }
    if room.peers.is_empty() {
        return empty_room_hint(&room.current);
    }
    let labels = room
        .peers
        .iter()
        .map(|peer| format!("{} · {}", peer.label(), app.pane_label(&peer.pane)))
        .collect::<Vec<_>>();
    peer_choice_hint(&labels)
}

/// Name the rows that can actually receive a request.
///
/// Kept short enough to survive the single-line hint area: past three
/// peers the list is trimmed and the remainder counted, because a hint
/// that wraps off-screen helps nobody. `tend peers` prints the full set.
pub(super) fn peer_choice_hint(labels: &[String]) -> String {
    const SHOWN: usize = 3;
    let mut names = labels
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if labels.len() > SHOWN {
        use std::fmt::Write as _;
        let _ = write!(names, " +{} more", labels.len() - SHOWN);
    }
    format!("select one of these rows, then press m: {names}")
}

/// The recipient under host scope: the tracked agent selected in any window.
/// Parent nodes resolve to the first live tracked pane in canonical numeric
/// window/pane order, so messaging never requires explicit `l l` descent.
///
/// Every row is a candidate, including the pane watch was opened from. The
/// sender is the operator console, not that pane's agent, so there is no
/// self-send to guard against — and the row the operator was sitting on when
/// they hit the key is a likely thing to want to poke.
fn host_scope_target(app: &App) -> Option<(String, String, Option<PaneKey>)> {
    if app.collaboration_scope != tend::config::CollaborationScope::Host {
        return None;
    }
    if app.uses_tree() {
        let pane = app.selected_topology_panes().into_iter().find(|pane| {
            pane.agent
                .as_ref()
                .is_some_and(|agent| agent.state != AgentState::Stopped)
        })?;
        let agent = pane.agent.as_ref()?;
        let label = format!(
            "{}@{} · {}",
            agent.kind,
            pane.key.pane_id,
            topology_key_label(&TopologyNodeKey::Pane(pane.key.clone()))
        );
        return Some((pane.key.pane_id.clone(), label, Some(pane.key.clone())));
    }
    let selected = app.selected_target()?;
    let agent = match app.rows.get(selected.row_idx)? {
        WatchRow::Agent(agent) => Some(agent.as_ref()),
        WatchRow::Work(session) => selected
            .agent_idx
            .and_then(|index| session.agents.get(index))
            .or_else(|| {
                let mut candidates = session.agents.iter();
                let only = candidates.next()?;
                candidates.next().is_none().then_some(only)
            })
            .or(session.latest_agent.as_ref()),
        WatchRow::BarePane(_) => None,
    }?;
    let pane = agent.pane.clone()?;
    let label = format!("{}@{} · {}", agent.kind, pane, app.pane_label(&pane));
    Some((pane, label, None))
}

/// The turns in the selected conversation, oldest first. An older daemon has
/// no tend-owned conversation ids, so it falls back to the former provider
/// filter rather than presenting an empty panel.
pub(super) fn active_ask_conversation(app: &App) -> Option<&tend::ask::AskConversation> {
    let id = app.active_ask_conversation_id.as_ref()?;
    app.ask_conversations
        .iter()
        .find(|conversation| &conversation.id == id && conversation.agent == app.ask_agent)
}

pub(super) fn visible_ask_entries(app: &App) -> Vec<&tend::ask::AskEntry> {
    app.ask_entries
        .iter()
        .filter(|entry| {
            if app.ask_conversations_available {
                app.active_ask_conversation_id
                    .as_ref()
                    .is_some_and(|conversation_id| {
                        entry.conversation_id.as_deref() == Some(conversation_id.as_str())
                    })
            } else {
                app.ask_panel.filter.matches(&entry.agent)
            }
        })
        .collect()
}

pub(super) fn is_ask_running(entry: &tend::ask::AskEntry) -> bool {
    entry.status == tend::ask::AskStatus::Running
}

pub(super) async fn refresh_ask_entries(client: &Client, app: &mut App) {
    let conversations = client.ask_conversation_list().await;
    if let Ok((listed, active)) = conversations {
        app.ask_conversations = listed;
        app.active_ask_conversation_id = active.map(|conversation| conversation.id);
        app.ask_conversations_available = true;
    }
    let (agent, entries) = tokio::join!(
        client.ask_agent(None),
        client.ask_list_filtered(None, app.active_ask_conversation_id.as_deref())
    );
    if let Ok(agent) = agent {
        app.ask_agent = agent;
    }
    match entries {
        Ok(entries) => {
            app.ask_entries = entries;
            let visible = visible_ask_entries(app).len();
            app.ask_panel.selected = app.ask_panel.selected.min(visible.saturating_sub(1));
        }
        Err(error) => {
            app.set_hint(format!("ask history unavailable: {error}"), HintLevel::Err);
        }
    }
}

/// A separate invalidation stream: Ask turns need not produce agent hooks.
/// Dropping the receiver closes this task even on an early UI error.
pub(super) fn ask_revision_updates(client: Client) -> tokio::sync::watch::Receiver<u64> {
    let (sender, receiver) = tokio::sync::watch::channel(0);
    tokio::spawn(async move {
        let Ok(mut stream) = client.ask_subscribe().await else {
            return;
        };
        loop {
            tokio::select! {
                update = stream.recv() => match update {
                    Ok(Some(revision)) => { sender.send_replace(revision.wrapping_add(1)); }
                    _ => return,
                },
                () = sender.closed() => return,
            }
        }
    });
    receiver
}

/// Who the cursor means, for both `m` and `Space`.
///
/// Extracted so marking and messaging can never disagree about the agent under
/// the cursor: one resolution, two callers. Returns the recipient as
/// `(pane, label, pane_key, agent session)` — the same shape the single-send
/// composer already builds its target from — or the hint explaining why the
/// cursor names nobody.
fn collaboration_target_at_cursor(
    app: &App,
) -> Result<(String, String, Option<PaneKey>, Option<String>), String> {
    if app.watch_cfg.screen == WatchScreen::Board {
        return board_collaboration_target(app);
    }
    let same_room = match app.collaboration.room.as_ref() {
        None => Err(app
            .collaboration
            .unavailable
            .clone()
            .unwrap_or_else(|| collaboration_open_hint().into())),
        Some(room) if room.peers.is_empty() => Err(no_target_hint(app, room)),
        Some(room) => {
            let selected_peer = if app.uses_tree() {
                app.selected_exact_pane().and_then(|pane| {
                    room.peers
                        .iter()
                        .find(|peer| participant_matches_pane(peer, pane))
                })
            } else {
                app.selected_target().and_then(|selected| {
                    let exact = match app.rows.get(selected.row_idx)? {
                        WatchRow::Agent(agent) => agent.pane.as_deref(),
                        WatchRow::Work(session) => selected
                            .agent_idx
                            .and_then(|index| session.agents.get(index))
                            .and_then(|agent| agent.pane.as_deref()),
                        WatchRow::BarePane(_) => None,
                    }?;
                    app.collaboration.peer_for_pane(exact)
                })
            };
            selected_peer
                // Parent rows deliberately do not reuse selected_action_pane:
                // that follows latest activity and would make `m` drift between
                // agents. Resolve them by stable numeric hierarchy instead.
                .or_else(|| peer_inside_selected_row(app, room))
                // The lone-peer shortcut is a *room* convenience: with one
                // agent beside you, "send" is unambiguous. Under host scope the
                // cursor is the selector and the table spans the host, so a row
                // that names no agent must not silently become the launch
                // window's agent — which it now would, since the console no
                // longer occupies that pane and it counts as a peer.
                .or_else(|| {
                    (app.collaboration_scope != tend::config::CollaborationScope::Host
                        && room.peers.len() == 1)
                        .then(|| &room.peers[0])
                })
                .map(|peer| {
                    (
                        peer.pane.clone(),
                        peer.label(),
                        participant_pane_key(app, peer),
                        Some(peer.agent_session_id.clone()),
                    )
                })
                .ok_or_else(|| no_target_hint(app, room))
        }
    };
    // In host scope the selected tracked agent/session is the explicit
    // recipient and must win over a singleton peer in the launch window.
    // Otherwise `m` on an external session could silently address the one
    // unrelated agent beside the watch pane. Window scope retains the room
    // behavior.
    if let Some((pane, label, pane_key)) = host_scope_target(app) {
        let session = app
            .collaboration
            .peer_for_pane(&pane)
            .map(|peer| peer.agent_session_id.clone());
        return Ok((pane, label, pane_key, session));
    }
    match same_room {
        Ok((pane, label, pane_key, session)) => Ok((pane, label, pane_key, session)),
        Err(reason) => Err(reason),
    }
}

/// The board's message recipient: the frozen agent `m` was pressed on. In
/// window scope it must be a peer in this room (pane and agent session);
/// host scope addresses it by its exact pane, which the daemon refuses if
/// that pane id is ambiguous across servers.
pub(super) fn board_collaboration_target(
    app: &App,
) -> Result<(String, String, Option<PaneKey>, Option<String>), String> {
    let Some(BoardPending::Message { pane, session }) = app.board_pending.as_ref() else {
        return Err("press m on the board to message the selected agent".into());
    };
    let (key, agent) = board_cursor_agent(app)?;
    if &key != pane || &agent.session_id != session {
        return Err("the selected agent changed — press m again".into());
    }
    if app.collaboration_scope != tend::config::CollaborationScope::Host {
        let in_room = app.collaboration.room.as_ref().is_some_and(|room| {
            room.peers
                .iter()
                .any(|peer| peer.pane == key.pane_id && &peer.agent_session_id == session)
        });
        if !in_room {
            return Err(format!(
                "{}@{} is not in this room; [collaboration] scope = \"host\" reaches it",
                agent.kind, key.pane_id
            ));
        }
    }
    let label = format!(
        "{}@{} · {}",
        agent.kind,
        key.pane_id,
        topology_key_label(&TopologyNodeKey::Pane(key.clone()))
    );
    Ok((key.pane_id.clone(), label, Some(key), Some(session.clone())))
}

/// `Space`: mark or unmark the agent under the cursor.
///
/// Deliberately the same resolution `m` uses, so what you mark is what you
/// would have messaged.
pub(super) fn toggle_collaboration_mark(app: &mut App) -> ActionOutcome {
    let (pane, label, _, agent_session_id) = match collaboration_target_at_cursor(app) {
        Ok(target) => target,
        Err(reason) => return ActionOutcome::Err(reason),
    };
    let mark = CollaborationMark {
        pane,
        agent_session_id,
    };
    if app.collaboration_marks.remove(&mark) {
        ActionOutcome::Ok(format!(
            "unmarked {label} · {} marked",
            app.collaboration_marks.len()
        ))
    } else {
        app.collaboration_marks.insert(mark);
        ActionOutcome::Ok(format!(
            "marked {label} · {} marked",
            app.collaboration_marks.len()
        ))
    }
}

/// The marked recipients that still exist, as `(pane, label)`.
///
/// Marks are pruned rather than trusted: a pane that went away, or one whose
/// agent session was replaced, is dropped here instead of being addressed.
/// Ordering follows the room's peer list, which is what the confirmation and
/// the report both render — one order, so the two always agree.
pub(super) fn resolved_marks(app: &App) -> Vec<(String, String)> {
    let mut resolved = Vec::new();
    let Some(room) = app.collaboration.room.as_ref() else {
        return resolved;
    };
    for peer in &room.peers {
        let mark = CollaborationMark {
            pane: peer.pane.clone(),
            agent_session_id: Some(peer.agent_session_id.clone()),
        };
        let paneless = CollaborationMark {
            pane: peer.pane.clone(),
            agent_session_id: None,
        };
        if app.collaboration_marks.contains(&mark) || app.collaboration_marks.contains(&paneless) {
            resolved.push((peer.pane.clone(), peer.label()));
        }
    }
    resolved
}

/// Marks the room can no longer account for. Shown next to the recipients so a
/// broadcast never quietly addresses fewer agents than were marked.
pub(super) fn stale_mark_count(app: &App) -> usize {
    app.collaboration_marks
        .len()
        .saturating_sub(resolved_marks(app).len())
}

pub(super) fn open_watch_collaboration_composer(app: &mut App) {
    // Marks are made on the topology; the board messages its own cursor's
    // agent and nothing else.
    let on_board = app.watch_cfg.screen == WatchScreen::Board;
    let marked = if on_board {
        Vec::new()
    } else {
        resolved_marks(app)
    };
    if !marked.is_empty() {
        let Some(origin) = app.collaboration.origin.clone() else {
            app.set_hint(collaboration_open_hint(), HintLevel::Err);
            return;
        };
        let stale = stale_mark_count(app);
        let label = if stale == 0 {
            format!("{} marked agents", marked.len())
        } else {
            format!("{} marked agents ({stale} gone)", marked.len())
        };
        let defaults = app.collaboration_compose_defaults;
        // just-send types into one pane; a marked set has no pane to type
        // into. Opening in a mode that cannot send would leave Enter refusing
        // and Ctrl-E the only way out, so the set opens read-only instead.
        let mode = match defaults.mode {
            ComposeSendMode::JustSend => ComposeSendMode::ReadOnly,
            mode => mode,
        };
        app.collaboration_composer = Some(CollaborationComposer::new(
            CollaborationComposeTarget::Broadcast {
                origin,
                recipients: marked,
                kind: defaults.kind,
                mode,
            },
            label,
        ));
        return;
    }
    let (peer_pane, peer_label, pane_key, peer_session) = match collaboration_target_at_cursor(app)
    {
        Ok(target) => target,
        // The keystrokes fallback types into the topology selection, which
        // is not what the board shows; `p` is the board's keystrokes path.
        Err(reason) if on_board => {
            app.board_pending = None;
            app.set_hint(reason, HintLevel::Warn);
            return;
        }
        Err(reason) => {
            open_prompt_only_composer(app, reason);
            return;
        }
    };
    let Some(origin) = app.collaboration.origin.clone() else {
        app.set_hint(collaboration_open_hint(), HintLevel::Err);
        return;
    };
    app.collaboration_mailbox.open = false;
    // `codex@%469` says who; the pane position says where to look for
    // them on screen. Both, because pane ids are stable and meaningless
    // while positions are legible and drift.
    let label = format!("{peer_label} · {}", app.pane_label(&peer_pane));
    let defaults = app.collaboration_compose_defaults;
    // The board addresses one exact agent session, which the daemon resolves
    // or refuses — never a successor in the pane. It is a request only:
    // keystrokes are `p`, so just-send is not on offer here.
    let (target, mode) = match (on_board, peer_session) {
        (true, Some(session)) => (
            format!("session:{session}"),
            match defaults.mode {
                ComposeSendMode::JustSend => ComposeSendMode::ReadOnly,
                mode => mode,
            },
        ),
        _ => (format!("pane:{peer_pane}"), defaults.mode),
    };
    app.collaboration_composer = Some(CollaborationComposer::new(
        CollaborationComposeTarget::Send {
            origin,
            target,
            pane: peer_pane,
            pane_key,
            kind: defaults.kind,
            mode,
        },
        label,
    ));
}

pub(super) async fn run_watch_collaboration_composer(
    client: &Client,
    composer: CollaborationComposer,
) -> (ActionOutcome, Option<BroadcastReport>) {
    if let CollaborationComposeTarget::Broadcast {
        origin,
        recipients,
        kind,
        mode,
    } = composer.target
    {
        return run_watch_collaboration_broadcast(
            client,
            &origin,
            recipients,
            kind,
            mode,
            composer.input,
        )
        .await;
    }
    (run_watch_collaboration_single(client, composer).await, None)
}

/// Send one body to several recipients as several ordinary requests.
///
/// Sequential on purpose: each send is an independent authorization decision
/// on the daemon, and a burst of concurrent wakes into the same room is not
/// something this buys anything by. A failure stops nothing — the remaining
/// recipients are still attempted, and every outcome is reported.
async fn run_watch_collaboration_broadcast(
    client: &Client,
    origin: &CollaborationOrigin,
    recipients: Vec<(String, String)>,
    kind: RequestKind,
    mode: ComposeSendMode,
    body: String,
) -> (ActionOutcome, Option<BroadcastReport>) {
    let work_mode = match mode {
        ComposeSendMode::ReadOnly => WorkMode::ReadOnly,
        ComposeSendMode::Execute => WorkMode::Execute,
        ComposeSendMode::JustSend => {
            return (
                ActionOutcome::Err(
                    "just-send types into one pane and cannot address a marked set".into(),
                ),
                None,
            )
        }
    };
    let mut rows = Vec::with_capacity(recipients.len());
    for (pane, label) in recipients {
        let pane = pane.clone();
        let request = NewRequest {
            user_authorized: false,
            task_contract: None,
            supersedes_request_id: None,
            dispatch_id: None,
            source_node_id: None,
            delegation_parent: None,
            human_action: None,
            initiator: None,
            kind,
            body: body.clone(),
            expects_reply: kind != RequestKind::Notice,
            notify: None,
            work_mode,
            thread_id: None,
            parent_request_id: None,
            workspace_id: None,
            work_id: None,
            run_id: None,
            paths: Vec::new(),
            artifacts: Vec::new(),
            links: Vec::new(),
            air_artifacts: Vec::new(),
            withdraw_reply_wakes: Vec::new(),
            recipient_slot: None,
        };
        let outcome = client
            .collaboration_send_view(
                origin,
                &format!("pane:{pane}"),
                &request,
                tend::collaboration::CollaborationReadView::Metadata,
            )
            .await
            .map(|sent| short_collaboration_request_id(&sent.id).clone())
            .map_err(|error| error.to_string());
        rows.push(BroadcastRow {
            pane,
            label,
            outcome,
        });
    }
    let report = BroadcastReport { rows };
    let outcome = if report.failed() == 0 {
        ActionOutcome::Ok(report.title())
    } else {
        ActionOutcome::Err(report.title())
    };
    (outcome, Some(report))
}

async fn run_watch_collaboration_single(
    client: &Client,
    composer: CollaborationComposer,
) -> ActionOutcome {
    match composer.target {
        CollaborationComposeTarget::Send {
            origin,
            target,
            kind,
            mode,
            ..
        } => {
            let work_mode = match mode {
                ComposeSendMode::ReadOnly => WorkMode::ReadOnly,
                ComposeSendMode::Execute => WorkMode::Execute,
                // Enter converts just-send into a Quick(SendPrompt) before
                // anything reaches this runner; refuse loudly rather than
                // invent a contract for keystrokes.
                ComposeSendMode::JustSend => {
                    return ActionOutcome::Err(
                        "just-send types into the pane and cannot become a request".into(),
                    )
                }
            };
            let request = NewRequest {
                user_authorized: false,
                task_contract: None,
                supersedes_request_id: None,
                dispatch_id: None,
                source_node_id: None,
                delegation_parent: None,
                human_action: None,
                initiator: None,
                kind,
                body: composer.input,
                expects_reply: kind != RequestKind::Notice,
                notify: None,
                work_mode,
                thread_id: None,
                parent_request_id: None,
                workspace_id: None,
                work_id: None,
                run_id: None,
                paths: Vec::new(),
                artifacts: Vec::new(),
                links: Vec::new(),
                air_artifacts: Vec::new(),
                withdraw_reply_wakes: Vec::new(),
                recipient_slot: None,
            };
            match client
                .collaboration_send_view(
                    &origin,
                    &target,
                    &request,
                    tend::collaboration::CollaborationReadView::Metadata,
                )
                .await
            {
                Ok(request) => ActionOutcome::Ok(format!(
                    "sent {} to {} ({})",
                    short_collaboration_request_id(&request.id),
                    request.to.label(),
                    request_kind_label(kind)
                )),
                Err(error) => ActionOutcome::Err(format!("collaboration send failed: {error}")),
            }
        }
        CollaborationComposeTarget::Broadcast { .. } => {
            ActionOutcome::Err("broadcast is dispatched by its own runner".into())
        }
        CollaborationComposeTarget::Prompt { .. }
        | CollaborationComposeTarget::PromptTopology { .. }
        | CollaborationComposeTarget::PromptPanes { .. } => ActionOutcome::Err(
            "keystrokes go to the pane directly and cannot become a request".into(),
        ),
        CollaborationComposeTarget::Reply {
            origin,
            request_id,
            status,
        } => match client
            .collaboration_reply(&origin, &request_id, status, &composer.input, &[], &[])
            .await
        {
            Ok(request) => ActionOutcome::Ok(format!(
                "replied to {} ({})",
                short_collaboration_request_id(&request.id),
                request_status_label(status)
            )),
            Err(error) => ActionOutcome::Err(format!("collaboration reply failed: {error}")),
        },
    }
}

pub(super) fn collaboration_requests(app: &App) -> &[CollaborationRequest] {
    match app.collaboration_mailbox.tab {
        CollaborationMailboxTab::Human => &app.collaboration.human,
        CollaborationMailboxTab::Incoming => &app.collaboration.incoming,
        CollaborationMailboxTab::Sent => &app.collaboration.sent,
    }
}

pub(super) fn selected_collaboration_request(app: &App) -> Option<&CollaborationRequest> {
    collaboration_requests(app)
        .get(app.collaboration_mailbox.selected)
        .map(|request| detail_for(app, request))
}

pub(super) fn clamp_collaboration_mailbox(app: &mut App) {
    let len = collaboration_requests(app).len();
    app.collaboration_mailbox.selected = if len == 0 {
        0
    } else {
        app.collaboration_mailbox.selected.min(len - 1)
    };
}

pub(super) fn move_collaboration_mailbox(app: &mut App, delta: isize) {
    let len = collaboration_requests(app).len();
    if len == 0 {
        app.collaboration_mailbox.selected = 0;
        return;
    }
    app.collaboration_mailbox.selected = app
        .collaboration_mailbox
        .selected
        .saturating_add_signed(delta)
        .min(len - 1);
}

pub(super) fn toggle_collaboration_mailbox(app: &mut App) {
    app.collaboration_mailbox.tab = match app.collaboration_mailbox.tab {
        CollaborationMailboxTab::Human => CollaborationMailboxTab::Incoming,
        CollaborationMailboxTab::Incoming => CollaborationMailboxTab::Sent,
        CollaborationMailboxTab::Sent => CollaborationMailboxTab::Human,
    };
    app.collaboration_mailbox.selected = 0;
}

pub(super) fn request_kind_label(kind: RequestKind) -> &'static str {
    match kind {
        RequestKind::Question => "question",
        RequestKind::Review => "review",
        RequestKind::Task => "task",
        RequestKind::Notice => "notice",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CollaborationBadge {
    pub(super) icon: &'static str,
    pub(super) label: &'static str,
    foreground: Color,
    pub(super) background: Color,
}

impl CollaborationBadge {
    pub(super) fn span(self) -> Span<'static> {
        Span::styled(
            format!(" {} {} ", self.icon, self.label),
            Style::default()
                .fg(self.foreground)
                .bg(self.background)
                .add_modifier(Modifier::BOLD),
        )
    }
}

pub(super) fn request_kind_badge(kind: RequestKind) -> CollaborationBadge {
    match kind {
        RequestKind::Question => CollaborationBadge {
            icon: "?",
            label: "QUESTION",
            foreground: Color::Black,
            background: Color::Cyan,
        },
        RequestKind::Review => CollaborationBadge {
            icon: "◆",
            label: "REVIEW",
            foreground: Color::White,
            background: Color::Magenta,
        },
        RequestKind::Task => CollaborationBadge {
            icon: "▶",
            label: "TASK",
            foreground: Color::Black,
            background: Color::Yellow,
        },
        RequestKind::Notice => CollaborationBadge {
            icon: "!",
            label: "NOTICE",
            foreground: Color::White,
            background: Color::Blue,
        },
    }
}

pub(super) fn work_mode_badge(mode: WorkMode) -> CollaborationBadge {
    match mode {
        WorkMode::ReadOnly => CollaborationBadge {
            icon: "○",
            label: "READ-ONLY",
            foreground: Color::Black,
            background: Color::Green,
        },
        WorkMode::Execute => CollaborationBadge {
            icon: "●",
            label: "EXECUTE",
            foreground: Color::White,
            background: Color::Red,
        },
    }
}

pub(super) fn reply_status_badge(status: RequestStatus) -> CollaborationBadge {
    match status {
        RequestStatus::Completed => CollaborationBadge {
            icon: "✓",
            label: "COMPLETED",
            foreground: Color::Black,
            background: Color::Green,
        },
        RequestStatus::Blocked => CollaborationBadge {
            icon: "!",
            label: "BLOCKED",
            foreground: Color::Black,
            background: Color::Yellow,
        },
        RequestStatus::Declined => CollaborationBadge {
            icon: "×",
            label: "DECLINED",
            foreground: Color::White,
            background: Color::DarkGray,
        },
        RequestStatus::Failed => CollaborationBadge {
            icon: "■",
            label: "FAILED",
            foreground: Color::White,
            background: Color::Red,
        },
        _ => CollaborationBadge {
            icon: "·",
            label: request_status_label(status),
            foreground: Color::Black,
            background: Color::Gray,
        },
    }
}

pub(super) fn work_mode_label(mode: WorkMode) -> &'static str {
    match mode {
        WorkMode::ReadOnly => "read-only",
        WorkMode::Execute => "execute",
    }
}

fn request_status_label(status: RequestStatus) -> &'static str {
    match status {
        RequestStatus::Queued => "queued",
        RequestStatus::Claimed => "claimed",
        RequestStatus::Completed => "completed",
        RequestStatus::Blocked => "blocked",
        RequestStatus::Declined => "declined",
        RequestStatus::Failed => "failed",
        RequestStatus::Expired => "expired",
        RequestStatus::Cancelled => "cancelled",
    }
}

pub(super) fn short_collaboration_request_id(request_id: &str) -> String {
    request_id.chars().take(18).collect()
}

pub(super) async fn append_human_interaction(
    path: Option<&Path>,
    kind: HumanInteractionKind,
    app: &App,
    pane_id: Option<&str>,
    started_at: OffsetDateTime,
    ended_at: OffsetDateTime,
) {
    let Some(path) = path else {
        return;
    };
    if ended_at <= started_at {
        return;
    }
    let (session_id, session_name) = interaction_session(app, pane_id);
    let entry =
        ActivityEntry::HumanInteraction(HumanInteractionEntry::new(HumanInteractionInput {
            kind,
            pane: pane_id.map(str::to_string),
            session_id,
            session_name,
            started_at,
            ended_at,
        }));
    if let Err(e) = tend::activity::append_entry(path, &entry).await {
        tracing::warn!(error = %e, path = %path.display(), "could not append human interaction");
    }
}
