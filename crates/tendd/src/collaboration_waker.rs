//! The collaboration waker: tells idle recipients about pending mailbox
//! requests, follows replaced or vanished peers, and wakes senders whose
//! replies are ready.

#![allow(clippy::wildcard_imports)] // moved from `main.rs`; uses the daemon's imports

use super::*;

pub(super) fn spawn_collaboration_waker_task(
    cfg: &Config,
    collaboration: Arc<CollaborationStore>,
    store: tend::SharedStore,
    backends: Vec<tend::SharedBackend>,
    shutdown_tx: &broadcast::Sender<()>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !cfg.collaboration.enabled || !collaboration.enabled() {
        return None;
    }
    let mut shutdown_rx = shutdown_tx.subscribe();
    // Subscribe before spawning and before the initial authoritative scan, so
    // a request or state transition racing task startup is represented either
    // by that scan or by an already-pending signal.
    let mut mailbox_changes = collaboration.subscribe();
    let mut agent_transitions = store.subscribe();
    let wake_payload = cfg.collaboration.wake_payload;
    let wake_enabled = cfg.collaboration.wake != CollaborationWake::Never;
    Some(tokio::spawn(async move {
        let mut wake_inflight = HashSet::new();
        if let Err(error) = collaboration
            .reconcile_peer_interruptions(&store.snapshot().await)
            .await
        {
            tracing::warn!(%error, "initial peer interruption reconciliation failed");
        }
        retire_gone_sender_items(&collaboration).await;
        if wake_enabled {
            rearm_unsubmitted_wakes(&collaboration, &store).await;
            wake_idle_collaboration_peers_with_inflight(
                &collaboration,
                &store,
                &backends,
                wake_payload,
                &mut wake_inflight,
            )
            .await;
        }
        let mut reconcile = tokio::time::interval(std::time::Duration::from_secs(
            COLLABORATION_WAKE_RECONCILE_SECONDS,
        ));
        reconcile.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        reconcile.tick().await;
        loop {
            let should_scan = tokio::select! {
                changed = mailbox_changes.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    true
                }
                transition = agent_transitions.recv() => match transition {
                    Ok(transition) => {
                        let should_scan = transition.to == tend::AgentState::Idle;
                        if matches!(
                            transition.to,
                            tend::AgentState::Idle
                                | tend::AgentState::Stopped
                                | tend::AgentState::Error
                        ) {
                            clear_wake_inflight_for_agent(&mut wake_inflight, &transition.agent);
                        }
                        should_scan
                    }
                    // A lag means the exact transition to Idle may have been
                    // dropped. Re-read the authoritative state immediately.
                    Err(broadcast::error::RecvError::Lagged(dropped)) => {
                        tracing::debug!(dropped, "collaboration waker transition stream lagged");
                        wake_inflight.clear();
                        true
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                _ = reconcile.tick() => {
                    // A successfully submitted prompt should produce a hook
                    // transition. The periodic reset is the bounded recovery
                    // path if that event was lost; the authoritative state
                    // snapshot below still refuses non-idle recipients.
                    wake_inflight.clear();
                    true
                },
                _ = shutdown_rx.recv() => break,
            };
            if should_scan {
                if let Err(error) = collaboration
                    .reconcile_peer_interruptions(&store.snapshot().await)
                    .await
                {
                    tracing::warn!(%error, "peer interruption reconciliation failed");
                }
                retire_gone_sender_items(&collaboration).await;
                if wake_enabled {
                    rearm_unsubmitted_wakes(&collaboration, &store).await;
                    wake_idle_collaboration_peers_with_inflight(
                        &collaboration,
                        &store,
                        &backends,
                        wake_payload,
                        &mut wake_inflight,
                    )
                    .await;
                }
            }
        }
    }))
}

/// Re-arm wakes the recipient never submitted, so the pass that follows
/// delivers each of them once more. Re-arming publishes a mailbox change, so
/// the loop also rescans on its own.
async fn rearm_unsubmitted_wakes(collaboration: &CollaborationStore, store: &tend::Store) {
    if let Err(error) = collaboration
        .rearm_unsubmitted_wakes(&store.snapshot().await)
        .await
    {
        tracing::warn!(%error, "failed to re-arm unsubmitted collaboration wakes");
    }
}

#[cfg(test)]
async fn wake_idle_collaboration_peers(
    collaboration: &CollaborationStore,
    store: &tend::SharedStore,
    backends: &[tend::SharedBackend],
) {
    let mut wake_inflight = HashSet::new();
    wake_idle_collaboration_peers_with_inflight(
        collaboration,
        store,
        backends,
        CollaborationWakePayload::Notice,
        &mut wake_inflight,
    )
    .await;
}

#[cfg(test)]
async fn wake_idle_collaboration_peers_with_full_payload(
    collaboration: &CollaborationStore,
    store: &tend::SharedStore,
    backends: &[tend::SharedBackend],
) {
    let mut wake_inflight = HashSet::new();
    wake_idle_collaboration_peers_with_inflight(
        collaboration,
        store,
        backends,
        CollaborationWakePayload::Full,
        &mut wake_inflight,
    )
    .await;
}

async fn wake_idle_collaboration_peers_with_inflight(
    collaboration: &CollaborationStore,
    store: &tend::SharedStore,
    backends: &[tend::SharedBackend],
    wake_payload: CollaborationWakePayload,
    wake_inflight: &mut HashSet<(String, Option<String>, String)>,
) {
    resolve_exited_stops(collaboration).await;
    let requests = collaboration.pending_unnotified().await;
    let replies = collaboration.pending_reply_unnotified().await;
    let agents = store.snapshot().await;
    let stranded = stranded_requests(collaboration, &agents).await;
    if requests.is_empty() && replies.is_empty() && stranded.is_empty() {
        return;
    }
    let listed_backends = backends.to_vec();
    let discovery_agents = agents.clone();
    let (panes, mut participants) = tokio::task::spawn_blocking(move || {
        let panes = listed_backends
            .iter()
            .flat_map(|backend| backend.list_panes())
            .collect::<Vec<_>>();
        let participants = tend::collaboration::participants_from(&discovery_agents, &panes);
        (panes, participants)
    })
    .await
    .unwrap_or_default();
    collaboration.enrich_participants(&mut participants).await;
    collaboration
        .enrich_launch_purposes(&mut participants, &panes)
        .await;
    collaboration.enrich_worker_drains(&mut participants).await;

    // Follow vanished recipients for every undelivered request, quiet ones
    // included; a readdressed request comes back through the next pass.
    for request in &stranded {
        follow_vanished_recipient(collaboration, request, &participants, &agents, &panes).await;
    }
    // Boxed: kept off the waker future, which is already large.
    let senders = follow_vanished_senders(collaboration, &replies, &participants, &agents, &panes);
    Box::pin(senders).await;

    let mut ready_pending = std::collections::HashMap::new();
    for request in requests {
        let Some(recipient) =
            ready_collaboration_recipient(&participants, &agents, &panes, &request)
        else {
            continue;
        };
        note_ready_pending(&mut ready_pending, &request, &recipient, &agents);
        let recipient = &recipient;
        if !request.bypasses_primary_protection() && !recipient.accepts_peer_work() {
            continue;
        }
        // Hook transport: the recipient's own turn-boundary hook is carrying
        // this request; type a wake only once that offer is a backoff old.
        if collaboration.hook_transport_enabled()
            && request
                .turn_offer
                .as_ref()
                .is_some_and(|offer| offer.offered_recently(time::OffsetDateTime::now_utc()))
        {
            continue;
        }
        // Never write into a human primary's terminal for agent work, even
        // user-authorized: its provider hook hands the request over at the
        // next turn boundary instead (`tend hook … --event stop`).
        if request.awaits_turn_boundary(recipient) {
            continue;
        }
        let wake_key = collaboration_wake_key(recipient);
        if wake_inflight.contains(&wake_key) {
            continue;
        }
        let quiet = collaboration.quiet_notices_waiting(recipient).await;
        let direct_wake = direct_collaboration_wake(wake_payload, &request);
        if direct_wake && direct_wake_body_is_terminal_safe(&request.body) {
            // Boxed: keeps the waker pass's future small.
            if Box::pin(deliver_full_collaboration_request(
                collaboration,
                recipient,
                &request,
                backends,
                quiet,
                wake_payload,
            ))
            .await
            {
                wake_inflight.insert(wake_key);
            }
            continue;
        }
        if direct_wake {
            tracing::debug!(
                request_id = request.id,
                "direct wake body contains terminal control characters; using mailbox notice",
            );
        }
        let prompt = with_quiet_notices(mailbox_collaboration_request_prompt(&request), quiet);
        if send_mailbox_wake(collaboration, recipient, &request, &prompt, backends, store).await {
            wake_inflight.insert(wake_key);
        }
    }

    collaboration.note_ready_pending_recipients(ready_pending);

    wake_senders_of_ready_replies(
        collaboration,
        &participants,
        backends,
        replies,
        wake_inflight,
        store,
    )
    .await;
}

/// An unresolved stop pauses Tend input to its pane id; release the ones
/// whose process is gone before that id refuses the next agent's wake.
async fn resolve_exited_stops(collaboration: &CollaborationStore) {
    match collaboration.resolve_exited_drains().await {
        Ok(0) => {}
        Ok(closed) => tracing::info!(closed, "resolved worker stops whose process exited"),
        Err(error) => tracing::warn!(%error, "failed to resolve exited worker stops"),
    }
}

/// Reconciliation bounds undelivered wakes but has no pane list; hand it
/// delivery's own verdict for a launching pane's recipient.
fn note_ready_pending(
    ready: &mut std::collections::HashMap<String, tend::collaboration::wake_retry::ReadyRecipient>,
    request: &CollaborationRequest,
    recipient: &tend::collaboration::Participant,
    agents: &[tend::state::Agent],
) {
    if tend::collaboration::is_pending_session(&request.to.agent_session_id) {
        if let Some(key) = tend::collaboration::ready_row_key(recipient, agents) {
            ready.insert(request.id.clone(), key);
        }
    }
}

/// Undelivered requests (quiet ones included) whose pinned session is no
/// longer registered at its pane. Checked against the registry first so a
/// lingering quiet notice to a present agent costs no pane scan.
async fn stranded_requests(
    collaboration: &CollaborationStore,
    agents: &[tend::state::Agent],
) -> Vec<CollaborationRequest> {
    collaboration
        .undelivered_requests()
        .await
        .into_iter()
        .filter(|request| {
            !tend::collaboration::is_pending_session(&request.to.agent_session_id)
                && !agents.iter().any(|agent| {
                    agent.session_id == request.to.agent_session_id
                        && agent.state != tend::AgentState::Stopped
                        && agent.pane.as_deref() == Some(request.to.pane.as_str())
                })
        })
        .collect()
}

/// Readdress an undelivered request whose pinned recipient is gone: to the
/// same session if it moved panes, or to the slot's single live successor
/// for a slot-addressed request. The store's change signal brings the
/// request back through the waker, now aimed at a live recipient.
async fn follow_vanished_recipient(
    collaboration: &CollaborationStore,
    request: &CollaborationRequest,
    participants: &[tend::collaboration::Participant],
    agents: &[tend::state::Agent],
    panes: &[tend::tmux::PaneInfo],
) {
    let Some(successor) = tend::collaboration::successor_for(request, participants, agents, panes)
    else {
        return;
    };
    match collaboration
        .rebind_recipient(&request.id, &request.to, successor)
        .await
    {
        Ok(Some(rebound)) => tracing::info!(
            request_id = rebound.id,
            from = request.to.pane,
            to = rebound.to.pane,
            "collaboration request followed its recipient",
        ),
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(request_id = request.id, %error, "failed to readdress request");
        }
    }
}

/// Withdraw gone-sender items whose reply has since been read or delivered.
async fn retire_gone_sender_items(collaboration: &CollaborationStore) {
    if let Err(error) = collaboration.retire_gone_sender_items().await {
        tracing::warn!(%error, "failed to retire gone-sender items");
    }
}

/// Owed replies whose sender session is gone: rebind each to the single
/// live successor in the sender's slot, whose next idle turn is then woken,
/// or, with no successor, open one human item after the grace.
async fn follow_vanished_senders(
    collaboration: &CollaborationStore,
    replies: &[CollaborationRequest],
    participants: &[tend::collaboration::Participant],
    agents: &[tend::state::Agent],
    panes: &[tend::tmux::PaneInfo],
) {
    use tend::collaboration::sender_follow::{sender_whereabouts, SenderWhereabouts};
    for request in replies {
        match sender_whereabouts(request, participants, agents, panes) {
            SenderWhereabouts::Present => {}
            SenderWhereabouts::Successor(successor) => {
                match collaboration
                    .rebind_sender(&request.id, &request.from, *successor)
                    .await
                {
                    Ok(Some(rebound)) => tracing::info!(
                        request_id = rebound.id,
                        from = request.from.agent_session_id,
                        to = rebound.from.agent_session_id,
                        "reply wake followed its sender's slot",
                    ),
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(request_id = request.id, %error, "failed to rebind sender");
                    }
                }
            }
            SenderWhereabouts::Gone => {
                if let Err(error) = collaboration.report_gone_sender(&request.id).await {
                    tracing::warn!(request_id = request.id, %error, "failed to report gone sender");
                }
            }
        }
    }
}

/// Tell each sender that terminal replies are waiting. Reply bodies always
/// stay in the mailbox, so this is a notice pass with no payload policy.
///
/// `replies` is only a hint of who may be owed. The store decides what each
/// wake carries when it reserves the batch, so a reply the sender read or
/// withdrew after the scan is not announced, and replies that piled up while
/// the sender was busy arrive as one prompt rather than one per idle turn.
async fn wake_senders_of_ready_replies(
    collaboration: &CollaborationStore,
    participants: &[tend::collaboration::Participant],
    backends: &[tend::SharedBackend],
    replies: Vec<CollaborationRequest>,
    wake_inflight: &mut HashSet<(String, Option<String>, String)>,
    store: &tend::Store,
) {
    let mut senders = Vec::new();
    for request in &replies {
        let Some(sender) = idle_collaboration_participant(participants, &request.from) else {
            continue;
        };
        let wake_key = collaboration_wake_key(sender);
        if !senders.contains(&wake_key) {
            senders.push(wake_key);
        }
    }
    for wake_key in senders {
        if wake_inflight.contains(&wake_key) {
            continue;
        }
        let Some(sender) = participants
            .iter()
            .find(|participant| collaboration_wake_key(participant) == wake_key)
        else {
            continue;
        };
        let batch = match collaboration.reserve_reply_wakes(sender).await {
            Ok(Some(batch)) => batch,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(pane = sender.pane, %error, "failed to reserve reply wake");
                continue;
            }
        };
        // `participants` came from a scan taken before the reservation. Type
        // nothing into a pane whose agent has since started work or been
        // replaced; its replies stay owed for its next idle turn.
        if !tend::collaboration::still_idle_owner(&store.by_pane(&sender.pane).await, sender) {
            if let Err(error) = collaboration.complete_reply_wakes(&batch, false).await {
                tracing::warn!(pane = sender.pane, %error, "failed to release reply wake");
            }
            continue;
        }
        let prompt = reply_wake_prompt(&batch, time::OffsetDateTime::now_utc());
        let (sent, submitted) =
            send_collaboration_wake(collaboration, sender, &prompt, backends, store).await;
        if submitted {
            wake_inflight.insert(wake_key);
        }
        if let Err(error) = collaboration.complete_reply_wakes(&batch, sent).await {
            tracing::warn!(pane = sender.pane, %error, "failed to persist reply wake outcome");
        }
        if sent {
            tracing::debug!(
                request_ids = ?batch.requests.iter().map(|request| &request.id).collect::<Vec<_>>(),
                pane = sender.pane,
                submitted,
                "collaboration reply wake delivered",
            );
        }
    }
}

/// The wake text for one reserved batch. A single reply keeps the long-
/// standing one-line form. Each entry names how long ago it was requested
/// once that is long enough to matter, so a sender that moved on can tell
/// at a glance which result it is looking at.
fn reply_wake_prompt(
    batch: &tend::collaboration::ReplyWakeBatch,
    now: time::OffsetDateTime,
) -> String {
    let entry = |request: &CollaborationRequest| {
        format!(
            "{:?} reply from {}{}{}",
            request.status,
            request.to.label(),
            requested_ago(request.created_at, now)
                .map(|ago| format!(" (requested {ago} ago)"))
                .unwrap_or_default(),
            request
                .attempt
                .as_ref()
                .map(|a| format!(
                    " [attempt {}, basis {}; recipient report, not independently verified]",
                    a.id,
                    a.basis_revision.as_deref().unwrap_or("unspecified")
                ))
                .unwrap_or_default(),
        )
    };
    if let ([request], 0) = (batch.requests.as_slice(), batch.remaining) {
        return format!(
            "[tend:{}] {} is ready. Read it with tend_wait (MCP; tend_wait_reply before tend 0.10.15) or `tend msg wait {}`.",
            request.id,
            entry(request),
            request.id,
        );
    }
    let entries = batch
        .requests
        .iter()
        .map(|request| format!("{}: {}", request.id, entry(request)))
        .collect::<Vec<_>>()
        .join("; ");
    let more = if batch.remaining == 0 {
        String::new()
    } else {
        format!(" {} more will follow.", batch.remaining)
    };
    format!(
        "[tend] {} replies are ready: {entries}.{more} Read each with tend_wait (MCP; tend_wait_reply before tend 0.10.15) or `tend msg wait ID`.",
        batch.requests.len(),
    )
}

/// Coarse age of a request, or `None` while it is recent enough that the
/// sender is plainly still waiting on it.
fn requested_ago(created_at: time::OffsetDateTime, now: time::OffsetDateTime) -> Option<String> {
    let minutes = (now - created_at).whole_minutes();
    match minutes {
        ..30 => None,
        30..120 => Some(format!("{minutes}m")),
        120..2880 => Some(format!("{}h", minutes / 60)),
        _ => Some(format!("{}d", minutes / 1440)),
    }
}

pub(super) async fn deliver_full_collaboration_request(
    collaboration: &CollaborationStore,
    recipient: &tend::collaboration::Participant,
    pending: &CollaborationRequest,
    backends: &[tend::SharedBackend],
    quiet: usize,
    wake_payload: CollaborationWakePayload,
) -> bool {
    let io_guard = tend::prompt_delivery::lock_pane(collaboration, &recipient.pane).await;
    if collaboration
        .worker_input_blocked(&recipient.pane, recipient.socket.as_deref())
        .await
    {
        return false;
    }
    let reserved_at = if pending.status == tend::collaboration::RequestStatus::Queued {
        match collaboration.reserve_typed_wake(&pending.id).await {
            Ok(tend::collaboration::turn_offer::TypedWake::Allowed { reserved_at }) => reserved_at,
            Ok(tend::collaboration::turn_offer::TypedWake::Refused) | Err(_) => return false,
        }
    } else {
        None
    };
    let (request, newly_prepared) = if pending.status == tend::collaboration::RequestStatus::Queued
    {
        match collaboration
            .prepare_direct_wake(recipient, &pending.id)
            .await
        {
            Ok(Some(request)) => (request, true),
            other => {
                if let Err(error) = other {
                    tracing::warn!(
                        request_id = pending.id,
                        %error,
                        "failed to reserve direct collaboration wake",
                    );
                }
                if let Some(reserved_at) = reserved_at {
                    let _ = collaboration
                        .release_typed_wake(&pending.id, reserved_at)
                        .await;
                }
                return false;
            }
        }
    } else {
        (pending.clone(), false)
    };

    match request.wake_delivery {
        Some(WakeDeliveryState::Prepared) => {
            let prompt = with_quiet_notices(
                if newly_prepared {
                    if wake_payload == CollaborationWakePayload::Adaptive && !request.from.console {
                        // Re-evaluate after the atomic claim: amendments may
                        // have arrived since the waker selected this request.
                        tend::collaboration::instruction::short_inline_prompt(&request)
                            .unwrap_or_else(|| mailbox_collaboration_request_prompt(&request))
                    } else {
                        full_collaboration_request_prompt(&request)
                    }
                } else {
                    interrupted_collaboration_request_prompt(&request)
                },
                quiet,
            );
            if !send_collaboration_text(recipient, &prompt, backends, io_guard.clone()).await {
                return false;
            }
            if let Err(error) = collaboration.mark_wake_prompt_written(&request.id).await {
                tracing::warn!(
                    request_id = request.id,
                    %error,
                    "failed to persist direct wake prompt phase",
                );
            }
            tokio::time::sleep(tend::backend::prompt_submit_grace(&recipient.pane)).await;
            submit_direct_collaboration_prompt(
                collaboration,
                recipient,
                &request.id,
                backends,
                io_guard.clone(),
            )
            .await
        }
        Some(WakeDeliveryState::PromptWritten) => {
            // The body or recovery notice is already in the input buffer.
            // Retrying only Enter avoids executing the request twice.
            if let Some(successor) = &request.superseded_by {
                let suffix=format!("\n[tend] This request was superseded by {successor}; do not act on it. Read the mailbox for the current task.");
                if !send_collaboration_text(recipient, &suffix, backends, io_guard.clone()).await {
                    return false;
                }
            }
            submit_direct_collaboration_prompt(
                collaboration,
                recipient,
                &request.id,
                backends,
                io_guard.clone(),
            )
            .await
        }
        None => false,
    }
}

async fn submit_direct_collaboration_prompt(
    collaboration: &CollaborationStore,
    recipient: &tend::collaboration::Participant,
    request_id: &str,
    backends: &[tend::SharedBackend],
    io_guard: WorkerIoGuard,
) -> bool {
    if !send_collaboration_text(recipient, "\r", backends, io_guard.clone()).await {
        return false;
    }
    mark_collaboration_request_notified(collaboration, request_id).await;
    true
}

async fn mark_collaboration_request_notified(collaboration: &CollaborationStore, request_id: &str) {
    if let Err(error) = collaboration.mark_notified(request_id).await {
        tracing::warn!(request_id, %error, "failed to persist wake marker");
    }
}

fn collaboration_reply_instruction(request: &CollaborationRequest) -> String {
    if let (Some(contract), Some(attempt)) = (&request.task_contract, &request.attempt) {
        return format!("Carries task contract {}, attempt {}. Completion requires a typed report. Use a supporting CLI: `tend msg reply {} BODY --report FILE`; upgrade/reload an older MCP if it omits the contract/report fields. Follow the full contract and do not infer completion from prose.",contract.task_id,attempt.id,request.id);
    }
    if request.expects_reply {
        format!(
            "When finished, respond with tend_reply for request {} or `tend msg reply {0} <body>`.",
            request.id
        )
    } else {
        "No reply is expected; do not send an acknowledgement solely for this delivery.".into()
    }
}

fn mailbox_collaboration_request_prompt(request: &CollaborationRequest) -> String {
    let represented = request.from.label();
    let source = if request.provenance.as_ref().is_some_and(|p| {
        matches!(
            p.client_kind,
            CollaborationClientKind::Watch | CollaborationClientKind::Dashboard
        )
    }) {
        collaboration_request_source(request)
    } else {
        format!("from {represented}")
    };
    let mismatch = request
        .provenance
        .as_ref()
        .map_or(String::new(), |p| match p.origin_match {
            CollaborationOriginMatch::Matched => String::new(),
            other => format!(" (origin {other})"),
        });
    let reply = if request.task_contract.is_some() {
        collaboration_reply_instruction(request)
    } else if request.expects_reply {
        format!(
            "Reply: `tend msg reply {} BODY` (or tend_reply).",
            request.id
        )
    } else {
        "No reply is expected.".into()
    };
    format!("[tend:{}] {:?} {source}{mismatch}. Read one: tend_inbox(request_id=\"{}\") or `tend msg accept {} --json`. Honor kind/work_mode/paths. {reply}",
        request.id,request.kind,request.id,request.id)
}

fn full_collaboration_request_prompt(request: &CollaborationRequest) -> String {
    let paths = serde_json::to_string(&request.paths).unwrap_or_else(|_| "[]".into());
    let mut contract_lines = String::new();
    if let Some(contract) = &request.task_contract {
        use std::fmt::Write as _;
        let _=write!(contract_lines,"\ntask_contract: {}\nattempt: {}\nFor completed work, provide a typed report with the attempt id, basis_revision, every criterion and evidence. These remain recipient-reported claims.",serde_json::to_string(contract).unwrap_or_default(),serde_json::to_string(&request.attempt).unwrap_or_default());
    }
    let reply = collaboration_reply_instruction(request);
    format!(
        "[tend:{}] New {} request {}. This request is already claimed; do not call tend_inbox for it.\nkind: {}\nwork_mode: {}\npaths: {}{}\nexpects_reply: {}\n{}\n\n--- request body ({} bytes) ---\n{}\n--- end request body ---",
        request.id,
        collaboration_request_kind(request.kind),
        collaboration_request_source(request),
        collaboration_request_kind(request.kind),
        collaboration_work_mode(request.work_mode),
        paths,
        contract_lines,
        request.expects_reply,
        reply,
        request.body.len(),
        request.body,
    )
}

/// Mention unread one-way notices in a wake that is going out anyway: the
/// recipient hears about them without a terminal write of their own.
fn with_quiet_notices(mut prompt: String, quiet: usize) -> String {
    if quiet > 0 {
        let _ = std::fmt::Write::write_fmt(
            &mut prompt,
            format_args!(
            "\n[tend] Also waiting: {quiet} one-way notice{} (no reply expected); read with tend_inbox or `tend msg inbox`.",
            if quiet == 1 { "" } else { "s" },
            ),
        );
    }
    prompt
}

fn interrupted_collaboration_request_prompt(request: &CollaborationRequest) -> String {
    format!(
        "[tend:{}] Direct {} request delivery was interrupted after it was claimed. Read only this request with tend_inbox(request_id=\"{}\") (MCP) or `tend msg accept {} --json`; honor kind/work_mode/paths. {}",
        request.id,
        collaboration_request_kind(request.kind),
        request.id,
        request.id,
        collaboration_reply_instruction(request),
    )
}

fn collaboration_request_kind(kind: tend::collaboration::RequestKind) -> &'static str {
    match kind {
        tend::collaboration::RequestKind::Question => "question",
        tend::collaboration::RequestKind::Review => "review",
        tend::collaboration::RequestKind::Task => "task",
        tend::collaboration::RequestKind::Notice => "notice",
    }
}

fn collaboration_work_mode(mode: tend::collaboration::WorkMode) -> &'static str {
    match mode {
        tend::collaboration::WorkMode::ReadOnly => "read_only",
        tend::collaboration::WorkMode::Execute => "execute",
    }
}

fn direct_wake_body_is_terminal_safe(body: &str) -> bool {
    tend::collaboration::instruction::terminal_safe(body)
}

/// Type a mailbox notice for `request` and record the attempt. Returns
/// whether Enter was delivered.
async fn send_mailbox_wake(
    collaboration: &CollaborationStore,
    recipient: &tend::collaboration::Participant,
    request: &CollaborationRequest,
    prompt: &str,
    backends: &[tend::SharedBackend],
    store: &tend::Store,
) -> bool {
    // The guard first, then the last evidence check and the attempt clock:
    // no other Tend input to this pane can land between them and the paste.
    let guard = tend::prompt_delivery::lock_pane(collaboration, &recipient.pane).await;
    let agents = store.by_pane(&recipient.pane).await;
    if !redelivery_still_due(collaboration, &agents, recipient, request).await {
        return false;
    }
    // Final transport decision under the pane guard: the recipient's own
    // hook may have been offered this request since the scan.
    let reserved_at = match collaboration.reserve_typed_wake(&request.id).await {
        Ok(tend::collaboration::turn_offer::TypedWake::Allowed { reserved_at }) => reserved_at,
        Ok(tend::collaboration::turn_offer::TypedWake::Refused) | Err(_) => return false,
    };
    // The unsubmitted-wake check judges the recipient's row against this
    // instant, taken before anything is typed.
    let attempt_started_at = time::OffsetDateTime::now_utc();
    let (sent, submitted) = send_collaboration_wake_holding(
        guard,
        &agents,
        collaboration,
        recipient,
        prompt,
        backends,
        store,
    )
    .await;
    if !sent {
        // Nothing typed: release the reservation so the hook may take it.
        if let Some(reserved_at) = reserved_at {
            let _ = collaboration
                .release_typed_wake(&request.id, reserved_at)
                .await;
        }
    }
    if sent {
        if let Err(error) = collaboration
            .mark_wake_notified(&request.id, attempt_started_at)
            .await
        {
            tracing::warn!(request_id = request.id, %error, "failed to persist wake marker");
        }
        tracing::debug!(
            request_id = request.id,
            pane = recipient.pane,
            submitted,
            "collaboration wake delivered",
        );
    }
    submitted
}

/// False when `request` is a scheduled re-delivery whose recipient has moved
/// since the unsubmitted attempt (`agents` read under the pane guard); that
/// re-delivery is cancelled. True for every ordinary wake.
async fn redelivery_still_due(
    collaboration: &CollaborationStore,
    agents: &[tend::state::Agent],
    recipient: &tend::collaboration::Participant,
    request: &CollaborationRequest,
) -> bool {
    if !request
        .wake_retry
        .as_ref()
        .is_some_and(|retry| retry.redeliver)
    {
        return true;
    }
    let agent = agents
        .iter()
        .find(|agent| agent.session_id == recipient.agent_session_id);
    collaboration
        .confirm_redelivery(&request.id, agent)
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(request_id = request.id, %error, "failed to confirm re-delivery");
            false
        })
}

/// Whether this wake types the request itself rather than a mailbox notice.
/// A re-delivery of an unsubmitted wake always repeats the notice.
fn direct_collaboration_wake(
    wake_payload: CollaborationWakePayload,
    request: &CollaborationRequest,
) -> bool {
    let redelivery = request
        .wake_retry
        .as_ref()
        .is_some_and(|retry| retry.redeliver);
    !redelivery
        && (request.wake_delivery.is_some()
            || collaboration_wake_includes_body(wake_payload, request))
}

fn collaboration_wake_includes_body(
    wake_payload: CollaborationWakePayload,
    request: &CollaborationRequest,
) -> bool {
    // `operator_full` follows the sender identity resolved by tendd. Work
    // mode is intentionally irrelevant: `execute` describes requested work,
    // not proof that a human authorized direct prompt injection.
    match wake_payload {
        CollaborationWakePayload::Notice => false,
        CollaborationWakePayload::OperatorFull => request.from.console,
        CollaborationWakePayload::Adaptive => {
            request.from.console
                || tend::collaboration::instruction::short_inline_prompt(request).is_some()
        }
        CollaborationWakePayload::Full => true,
    }
}

fn collaboration_request_source(request: &CollaborationRequest) -> String {
    let represented = request.from.label();
    let Some(provenance) = request.provenance.as_ref() else {
        return format!("from {represented}");
    };
    let surface = match provenance.client_kind {
        CollaborationClientKind::Watch => "tend watch",
        CollaborationClientKind::Mcp => "tend MCP",
        CollaborationClientKind::Dashboard => "tend dashboard",
        CollaborationClientKind::Cli => "tend CLI",
        CollaborationClientKind::Unknown => "an unclassified tend client",
    };
    let actor = match (&provenance.observed_pane, provenance.caller_pid) {
        (Some(pane), Some(pid)) => format!("caller {pane}, pid {pid}"),
        (Some(pane), None) => format!("caller {pane}"),
        (None, Some(pid)) => format!("caller pid {pid}"),
        (None, None) => "caller location unavailable".into(),
    };
    let mismatch = match provenance.origin_match {
        CollaborationOriginMatch::Matched => "",
        CollaborationOriginMatch::Mismatched => "; origin mismatch",
        CollaborationOriginMatch::Unverifiable => "; origin unverified",
    };
    format!("via {surface} representing {represented} ({actor}{mismatch})")
}

/// The participant a queued request may be delivered to *right now*.
///
/// The ordinary case is its session-pinned recipient, idle. A **pending**
/// recipient — a launched pane whose agent has not registered a session yet —
/// resolves two further ways: a real session that registered on that pane
/// after the request was queued, or the pane's own discovery/screen row once
/// it reads idle. The latter is what unblocks codex, whose `SessionStart` hook
/// cannot fire until something is typed at it. Discovery supplies the idle
/// row; screen detection's job here is the opposite one, holding delivery
/// while the pane sits on its startup approval gate.
fn ready_collaboration_recipient(
    participants: &[tend::collaboration::Participant],
    agents: &[tend::state::Agent],
    panes: &[tend::tmux::PaneInfo],
    request: &CollaborationRequest,
) -> Option<tend::collaboration::Participant> {
    if let Some(participant) = idle_collaboration_participant(participants, &request.to) {
        return Some(participant.clone());
    }
    if !tend::collaboration::is_pending_session(&request.to.agent_session_id) {
        return None;
    }
    idle_pending_recipient(participants, &request.to)
        .cloned()
        .or_else(|| {
            let mut ready =
                tend::collaboration::pending_recipient_ready(&request.to, agents, panes)?;
            if let Some(enriched) = participants
                .iter()
                .find(|p| p.pane == ready.pane && p.socket == ready.socket && p.room == ready.room)
            {
                ready.launch_purpose = enriched.launch_purpose;
            }
            Some(ready)
        })
}

/// The idle agent that may adopt a request still addressed to a launching
/// pane. Uses the same predicate delivery reserves with: matching on pane and
/// socket alone picked whatever agent later reused that pane id in another
/// window, and the refused reservation then repeated on every tick.
fn idle_pending_recipient<'a>(
    participants: &'a [tend::collaboration::Participant],
    to: &tend::collaboration::Participant,
) -> Option<&'a tend::collaboration::Participant> {
    participants.iter().find(|participant| {
        participant.state == tend::AgentState::Idle
            && tend::collaboration::addresses(to, participant)
    })
}

fn idle_collaboration_participant<'a>(
    participants: &'a [tend::collaboration::Participant],
    target: &tend::collaboration::Participant,
) -> Option<&'a tend::collaboration::Participant> {
    participants.iter().find(|participant| {
        participant.same_session(target)
            && participant.state == tend::AgentState::Idle
            && !participant
                .agent_session_id
                .starts_with(tend::state::SYNTHETIC_SESSION_PREFIX)
    })
}

fn collaboration_wake_key(
    participant: &tend::collaboration::Participant,
) -> (String, Option<String>, String) {
    (
        participant.pane.clone(),
        participant.socket.clone(),
        participant.agent_session_id.clone(),
    )
}

/// Drop the wake debounce for everything addressed at this agent's pane.
///
/// Keyed by endpoint rather than session id on purpose: a request queued
/// against a pane that had not registered yet carries a `pending-pane:`
/// placeholder, which no registry row will ever name. Matching the row's own
/// session id would leave that entry suppressing further wakes to the pane
/// until the slow reconcile tick cleared the whole set.
fn clear_wake_inflight_for_agent(
    wake_inflight: &mut HashSet<(String, Option<String>, String)>,
    agent: &tend::Agent,
) {
    let Some(pane) = agent.pane.as_deref() else {
        return;
    };
    wake_inflight
        .retain(|(key_pane, key_socket, _)| key_pane != pane || *key_socket != agent.tmux_socket);
}

pub(super) async fn send_collaboration_wake(
    collaboration: &CollaborationStore,
    participant: &tend::collaboration::Participant,
    prompt: &str,
    backends: &[tend::SharedBackend],
    store: &tend::Store,
) -> (bool, bool) {
    let guard = tend::prompt_delivery::lock_pane(collaboration, &participant.pane).await;
    let agents = store.by_pane(&participant.pane).await;
    send_collaboration_wake_holding(
        guard,
        &agents,
        collaboration,
        participant,
        prompt,
        backends,
        store,
    )
    .await
}

/// [`send_collaboration_wake`] with the pane's input guard already held and
/// `agents` read after it was taken.
async fn send_collaboration_wake_holding(
    guard: tend::prompt_delivery::PaneInputGuard,
    agents: &[tend::state::Agent],
    collaboration: &CollaborationStore,
    participant: &tend::collaboration::Participant,
    prompt: &str,
    backends: &[tend::SharedBackend],
    store: &tend::Store,
) -> (bool, bool) {
    let Some(backend) = text_backend(backends, &participant.pane) else {
        return (false, false);
    };
    // A peer delivery: the shared path holds the pane guard from the drain
    // check through Enter, and registers the prompt before typing so the
    // hook reporting it is stamped as from a peer rather than a person.
    let delivered = tend::prompt_delivery::deliver_prompt_holding(
        guard,
        collaboration,
        store,
        agents,
        backend,
        tend::prompt_delivery::PromptDelivery {
            pane: &participant.pane,
            socket: participant.socket.as_deref(),
            text: prompt,
            submit: true,
            origin: Some(tend::PromptOrigin::Peer),
        },
    )
    .await;
    let Ok(delivered) = delivered else {
        return (false, false);
    };
    let submitted = delivered.submitted;
    if submitted {
        // A missing hook must not delay mailbox bookkeeping, the next peer's
        // wake, or shutdown. This bounded observer only logs evidence and
        // never sends keys or retries the body.
        let pane = participant.pane.clone();
        tokio::spawn(async move {
            let confirmed = delivered.confirm().await;
            tracing::debug!(pane, submission_confirmed = ?confirmed,
                "collaboration Enter delivered; application acknowledgement observed separately");
        });
    }
    (true, submitted)
}

async fn send_collaboration_text(
    participant: &tend::collaboration::Participant,
    text: &str,
    backends: &[tend::SharedBackend],
    io_guard: WorkerIoGuard,
) -> bool {
    send_pane_text(
        backends,
        &participant.pane,
        participant.socket.as_deref(),
        text,
        Some(io_guard),
    )
    .await
}

#[cfg(test)]
#[path = "collaboration_waker_tests.rs"]
mod tests;
