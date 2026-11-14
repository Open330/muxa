//! Daemon-owned recovery records. No synthetic agent reply or automatic execution.
use super::{
    addresses, next_request_id, CollaborationError, CollaborationReply, CollaborationRequest,
    CollaborationStore, HumanAction, Participant, RequestKind, RequestStatus, WorkMode,
};
use crate::{Agent, AgentKind, AgentState};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PeerInterruption {
    pub request_id: String,
    pub reason: String,
    pub active: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub observed_at: OffsetDateTime,
    pub reset_at: Option<String>,
    pub action_request_id: Option<String>,
    pub decision: Option<CollaborationReply>,
}

impl CollaborationRequest {
    pub fn peer_interrupted(&self) -> bool {
        !self.status.is_terminal()
            && self
                .interruption
                .as_ref()
                .is_some_and(|i| i.active && i.request_id == self.id)
    }
}

pub(super) fn exact_agent<'a>(participant: &Participant, agents: &'a [Agent]) -> Option<&'a Agent> {
    if participant.console {
        return None;
    }
    // Session identity survives pane/socket moves; process state must follow it.
    agents.iter().find(|a| {
        a.kind == participant.agent_kind
            && a.session_id == participant.agent_session_id
            && (!a
                .session_id
                .starts_with(crate::state::SYNTHETIC_SESSION_PREFIX)
                || (a.pane.as_deref() == Some(participant.pane.as_str())
                    && a.tmux_socket == participant.socket))
    })
}

fn reason(agent: &Agent) -> Option<&'static str> {
    match agent.state {
        AgentState::Error if agent.rate_limit_scope.is_some() => Some("rate_limited"),
        AgentState::Error => Some("error"),
        AgentState::Stopped => Some("stopped"),
        AgentState::WaitingChoice
            if agent.kind == AgentKind::ClaudeCode
                && agent.last_notification.as_deref().is_some_and(|message| {
                    message.starts_with(crate::adapters::claude::APPROVAL_NOTIFICATION_PREFIX)
                }) =>
        {
            Some("approval_required")
        }
        AgentState::WaitingInput if agent.last_notification.is_some() => Some("input_required"),
        _ => None,
    }
}

fn missing_binding_reason(
    request: &CollaborationRequest,
    agent: Option<&Agent>,
    missing: &mut std::collections::HashMap<String, OffsetDateTime>,
    now: OffsetDateTime,
) -> Option<&'static str> {
    if agent.is_some()
        || super::is_pending_session(&request.to.agent_session_id)
        || request.status.is_terminal()
        || (request.status != RequestStatus::Claimed
            && request.wake_delivery.is_none()
            && request.notified_at.is_none())
    {
        missing.remove(&request.id);
        return None;
    }
    let since = missing.entry(request.id.clone()).or_insert(now);
    (now - *since >= time::Duration::minutes(2)
        || request
            .interruption
            .as_ref()
            .is_some_and(|i| i.active && i.reason == "binding_unavailable"))
    .then_some("binding_unavailable")
}

/// The recipient claimed this request, was reminded at a Stop boundary to
/// reply, and is still Idle a full backoff after its latest recorded Stop
/// without a reply, with no prompt submitted since. The Stop and the prompt
/// are hook facts; a timeout-inferred Idle without a Stop never qualifies,
/// because working again clears the recorded Stop (see the caller).
fn reply_overdue<'a>(
    request: &CollaborationRequest,
    agent: Option<&Agent>,
    requests: impl IntoIterator<Item = &'a CollaborationRequest>,
    now: OffsetDateTime,
) -> Option<&'static str> {
    let offer = request.turn_offer.as_ref()?;
    let stop = offer.unanswered_stop_at?;
    let agent = agent?;
    (request.status == RequestStatus::Claimed
        && request.reply.is_none()
        && agent.state == AgentState::Idle
        && agent.last_prompt_at.is_none_or(|at| at <= stop)
        && offer.reminder_overdue(now)
        && !super::turn_offer::waits_on_own_request(requests, &request.to, request.claimed_at))
    .then_some("reply_overdue")
}

/// The claimant was seen working, or took a new prompt, after its recorded
/// Stop: that idle episode is over and must not be judged overdue later.
fn close_stale_stop(request: &mut CollaborationRequest, agent: Option<&Agent>) {
    let Some(agent) = agent else { return };
    let Some(offer) = request.turn_offer.as_mut() else {
        return;
    };
    // Only work begun after that Stop ends the episode. The Stop hook asks
    // for its offers before the stop itself is recorded, so a reconcile in
    // between still sees the turn that is ending as Working.
    if offer.unanswered_stop_at.is_some_and(|stop| {
        (agent.state == AgentState::Working && agent.state_entered_at > stop)
            || agent.last_prompt_at.is_some_and(|at| at > stop)
    }) {
        offer.unanswered_stop_at = None;
    }
}

fn recovery_action(
    parent: &CollaborationRequest,
    id: String,
    now: OffsetDateTime,
    current_reason: &str,
) -> CollaborationRequest {
    let mut action = parent.clone();
    action.id = id;
    action.late_replies.clear();
    action.retirement = None;
    action.task_contract = None;
    action.attempt = None;
    action.supersedes_request_id = None;
    action.superseded_by = None;
    action.dispatch_id = None;
    action.source_node_id = None;
    action.delegation_parent = None;
    action.from = Participant::console(parent.from.room.clone());
    action.from.console = false;
    action.from.agent_kind = AgentKind::Task;
    action.from.agent_session_id = "tend:peer-recovery".into();
    action.from.pane = "tend-recovery".into();
    action.from.alias = Some("Tend recovery".into());
    action.to = Participant::console(parent.from.room.clone());
    action.initiator = None;
    action.provenance = None;
    action.parent_request_id = Some(parent.id.clone());
    action.kind = RequestKind::Question;
    action.work_mode = WorkMode::ReadOnly;
    action.human_action = Some(HumanAction::Choice);
    action.body = recovery_body(
        &parent.to.agent_session_id,
        &parent.id,
        current_reason,
        parent
            .interruption
            .as_ref()
            .and_then(|i| i.reset_at.as_deref()),
    );
    action.status = RequestStatus::Queued;
    action.created_at = now;
    action.claimed_at = None;
    action.notified_at = None;
    action.reply_notified_at = None;
    action.reply_read_at = None;
    action.reply_wake = None;
    action.wake_delivery = None;
    action.reply = None;
    action.turn_offer = None;
    action.sender_follow = None;
    action.wake_retry = None;
    action.updates.clear();
    action
}

fn recovery_body(
    session: &str,
    request_id: &str,
    current_reason: &str,
    reset: Option<&str>,
) -> String {
    if matches!(current_reason, "approval_required" | "input_required") {
        let needed = if current_reason == "approval_required" {
            "approval"
        } else {
            "input"
        };
        return format!(
            "Tend is retaining request {request_id} while recipient {session} waits for provider {needed}.\nThe request has not completed. Open this exact recipient's prompt and review its sender/action preview. Resolve the provider prompt, or choose to wait or stop the work. Tend will observe the recipient resume and clear this recovery item automatically; do not resend the request or relaunch the agent merely to clear the hold. Answering this recovery item does not itself grant provider tool permissions."
        );
    }
    if current_reason == "reply_overdue" {
        return format!(
            "Tend is retaining request {request_id}: recipient {session} claimed it, was reminded to reply, and is idle without replying.\nThe request has not completed. Choose: wait, ask the recipient for its result (tend_update_request on {request_id}), or stop this work attempt. Tend clears this item when the recipient replies or resumes work; do not resend the request. Your answer records a decision; it does not restart, cancel, or spend automatically."
        );
    }
    if current_reason == "wake_unsubmitted" {
        return format!(
            "Tend woke recipient {session} for request {request_id} twice, but no prompt was submitted and the request was never claimed.\nThe recipient is idle; its input line may hold an unsent draft, or the terminal may have dropped Enter. Open this exact recipient's pane and look before pressing anything: submit or clear the draft, then the agent can claim {request_id}. Tend will not type the wake again. Choose: fix the pane and wait, hand the work off, or stop this attempt."
        );
    }
    if current_reason == "wake_undelivered" {
        return format!(
            "Tend could not deliver request {request_id} to recipient {session}: the recipient is idle, but no wake reached it and it never claimed the request.\nThe request has not started. Tend refused to type into that pane (for example, an unresolved stop recorded for the pane) or the terminal write failed. Open this exact recipient's pane, or have it claim {request_id} with tend_inbox; Tend clears this item once the request is delivered or claimed. Choose: fix the pane and wait, hand the work off, or stop this attempt. Do not resend the request."
        );
    }
    let observation = if current_reason == "binding_unavailable" {
        "cannot verify the recipient binding; delivery and execution are uncertain, and process exit is not established"
    } else {
        "detected an interrupted peer"
    };
    format!("Tend {observation}: {session} ({current_reason}).\nOriginal request: {request_id}\nReset: {}\nChoose: wait and recheck, arrange a safe handoff, or stop this work attempt. Recommendation: inspect existing work before reassignment; a capped agent may resume. Your answer records a decision; it does not restart, cancel, or spend automatically.", reset.unwrap_or("unknown"))
}

fn refresh_recovery_action(
    action: &CollaborationRequest,
    info: &PeerInterruption,
    now: OffsetDateTime,
    recipient_session: &str,
) -> Option<CollaborationRequest> {
    if action.from.agent_session_id != "tend:peer-recovery" {
        return None;
    }
    let mut refreshed = action.clone();
    refreshed.interruption = Some(info.clone());
    // Older bulk expiry could retire an unanswered recovery question while
    // its parent remained open. Restore that exact daemon-owned question;
    // explicit cancellation and actual replies remain terminal.
    if info.active
        && action.status == RequestStatus::Expired
        && action.reply.is_none()
        && action.kind == RequestKind::Question
        && action.to.console
        && action.parent_request_id.as_deref() == Some(info.request_id.as_str())
    {
        refreshed.status = RequestStatus::Queued;
        refreshed.notified_at = None;
    }
    if info.active && !refreshed.status.is_terminal() {
        refreshed.body = recovery_body(
            recipient_session,
            &info.request_id,
            &info.reason,
            info.reset_at.as_deref(),
        );
    }
    if !info.active && !action.status.is_terminal() {
        refreshed.status = RequestStatus::Cancelled;
    }
    if action.reply.is_some() {
        // The daemon delivered its own request's answer to the parent;
        // no service pane remains to wake.
        refreshed.mark_reply_read(now);
    }
    (refreshed != *action).then_some(refreshed)
}

impl CollaborationStore {
    /// Reconcile a local authoritative registry. Unknown/missing identities never
    /// imply death or quota exhaustion. A delivered request whose exact binding
    /// stays absent is reported as uncertain after a grace period, without
    /// resending, cancelling, or changing execution authority.
    #[allow(clippy::too_many_lines)] // Keep parent/action reconciliation and their atomic publication together.
    pub async fn reconcile_peer_interruptions(
        &self,
        agents: &[Agent],
    ) -> Result<(), CollaborationError> {
        self.ensure_enabled()?;
        let _transaction = self.transaction_lock.lock().await;
        let now = OffsetDateTime::now_utc();
        // With `wake = "never"` nothing is ever typed, so waiting is expected.
        let typed_wakes = self
            .turn_continuation
            .load(std::sync::atomic::Ordering::SeqCst);
        let snapshot = self.requests.read().await;
        let mut missing = self.missing_recipients.lock().await;
        missing.retain(|id, _| snapshot.get(id).is_some_and(|r| !r.status.is_terminal()));
        let mut changed = Vec::new();
        for original in snapshot.values().filter(|r| {
            !r.to.console
                && r.expects_reply
                && r.kind != RequestKind::Notice
                && (!r.status.is_terminal() || r.interruption.is_some())
        }) {
            let agent = exact_agent(&original.to, agents);
            let missing_reason = missing_binding_reason(original, agent, &mut missing, now);
            // A fresh worker may still be at a startup/trust dialog. Track
            // approvals only after this request was actually handed over.
            let handed_over = original.notified_at.is_some()
                || (original.status == RequestStatus::Claimed && original.wake_delivery.is_none());
            let current_reason = agent
                .and_then(reason)
                .filter(|reason| {
                    !matches!(*reason, "approval_required" | "input_required") || handed_over
                })
                .or(missing_reason)
                .or_else(|| reply_overdue(original, agent, snapshot.values(), now))
                .or_else(|| {
                    super::wake_retry::wake_exhausted_unsubmitted(original, agent, now)
                        .then_some("wake_unsubmitted")
                })
                .or_else(|| {
                    (typed_wakes && super::wake_retry::wake_undelivered(original, agent, now))
                        .then_some("wake_undelivered")
                });
            let mut parent = original.clone();
            close_stale_stop(&mut parent, agent);
            if let Some(current_reason) = current_reason.filter(|_| !parent.status.is_terminal()) {
                if !parent.peer_interrupted() {
                    parent.interruption = Some(PeerInterruption {
                        request_id: parent.id.clone(),
                        reason: current_reason.into(),
                        active: true,
                        observed_at: now,
                        reset_at: agent
                            .and_then(|a| a.rate_limited_until)
                            .map(|t| t.to_string()),
                        action_request_id: None,
                        decision: None,
                    });
                }
                let info = parent.interruption.as_mut().expect("initialized");
                info.reason = current_reason.into();
                info.reset_at = agent
                    .and_then(|a| a.rate_limited_until)
                    .map(|t| t.to_string());
                // A healthy coordinator gets an immediate wait result first. If it
                // cannot act, or has not acknowledged within two minutes, escalate.
                let coordinator_unavailable = (parent.from.console && parent.initiator.is_none())
                    || exact_agent(&parent.from, agents).is_some_and(|a| reason(a).is_some());
                let acknowledged = parent
                    .updates
                    .iter()
                    .any(|u| u.at >= info.observed_at && addresses(&parent.from, &u.author));
                let overdue = now - info.observed_at >= time::Duration::minutes(2) && !acknowledged;
                if info.action_request_id.is_none() {
                    info.action_request_id = snapshot
                        .values()
                        .find(|r| {
                            r.needs_human_response()
                                && r.parent_request_id.as_deref() == Some(parent.id.as_str())
                        })
                        .map(|r| r.id.clone());
                }
                if info.action_request_id.is_none()
                    && (matches!(current_reason, "approval_required" | "input_required")
                        || coordinator_unavailable
                        || overdue)
                {
                    let id = next_request_id(now);
                    info.action_request_id = Some(id.clone());
                    changed.push(recovery_action(&parent, id, now, current_reason));
                }
            } else if parent.status.is_terminal()
                || agent.is_some_and(|a| matches!(a.state, AgentState::Working | AgentState::Idle))
                || (agent.is_some()
                    && parent
                        .interruption
                        .as_ref()
                        .is_some_and(|i| i.reason == "binding_unavailable"))
            {
                if let Some(info) = &mut parent.interruption {
                    info.active = false;
                }
            }
            if let Some(info) = &mut parent.interruption {
                if let Some(action) = info
                    .action_request_id
                    .as_ref()
                    .and_then(|id| snapshot.get(id))
                {
                    info.decision.clone_from(&action.reply);
                    if let Some(refreshed) =
                        refresh_recovery_action(action, info, now, &original.to.agent_session_id)
                    {
                        changed.push(refreshed);
                    }
                }
            }
            if parent != *original {
                changed.push(parent);
            }
        }
        drop(missing);
        drop(snapshot);
        if !changed.is_empty() {
            self.persist_requests(&changed)?;
            let mut requests = self.requests.write().await;
            for request in changed {
                requests.insert(request.id.clone(), request);
            }
            drop(requests);
            self.publish_change();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collaboration::{CollaborationOptions, NewRequest};
    use crate::event::{AgentEvent, AgentId, RateLimitScope};
    use std::time::Duration;

    async fn actor(pane: &str, session: &str) -> (Participant, Agent) {
        let store = crate::state::Store::shared();
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::Codex,
                    session_id: session.into(),
                    surface: None,
                    pane: Some(pane.into()),
                    tmux_socket: Some("default".into()),
                    cwd: None,
                },
                at: OffsetDateTime::now_utc(),
            })
            .await;
        let mut agent = store.snapshot().await.remove(0);
        agent.state = AgentState::Working;
        let participant = serde_json::from_value(serde_json::json!({
            "launch_purpose":"worker", "agent_kind":"codex", "agent_session_id":session, "pane":pane, "socket":"default",
            "room":{"host":"tmux","socket":"default","window_id":"@1"}, "state":"working"
        }))
        .unwrap();
        (participant, agent)
    }

    #[tokio::test]
    async fn provider_approval_is_durable_and_resumes_the_same_request_without_a_resend() {
        let dir = tempfile::tempdir().unwrap();
        let options = CollaborationOptions {
            path: Some(dir.path().join("mailbox.json")),
            ..CollaborationOptions::default()
        };
        let mailbox = CollaborationStore::load(options.clone()).await.unwrap();
        let (sender, coordinator) = actor("%1", "sender").await;
        let (mut recipient, mut peer) = actor("%2", "peer").await;
        recipient.agent_kind = AgentKind::ClaudeCode;
        peer.kind = AgentKind::ClaudeCode;
        peer.last_notification = Some(format!(
            "{}Cross-session message requires approval",
            crate::adapters::claude::APPROVAL_NOTIFICATION_PREFIX
        ));
        let request = mailbox
            .create(
                sender.clone(),
                recipient.clone(),
                NewRequest {
                    body: "review only".into(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        peer.state = AgentState::WaitingChoice;
        mailbox
            .reconcile_peer_interruptions(&[coordinator.clone(), peer.clone()])
            .await
            .unwrap();
        let queued = mailbox.get_for(&sender, &request.id).await.unwrap();
        assert!(
            !queued.peer_interrupted(),
            "a startup choice before delivery is not a failed delivery"
        );
        mailbox.claim_by_id(&recipient, &request.id).await.unwrap();
        mailbox
            .reconcile_peer_interruptions(&[coordinator.clone(), peer.clone()])
            .await
            .unwrap();
        let held = mailbox
            .wait_for_terminal(&sender, &request.id, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(held.status, RequestStatus::Claimed);
        assert!(held.peer_interrupted());
        let info = held.interruption.as_ref().unwrap();
        assert_eq!(info.reason, "approval_required");
        let action_id = info
            .action_request_id
            .clone()
            .expect("approval must reach the operator immediately");
        let human = Participant::console(sender.room.clone());
        let action = mailbox.get_for(&human, &action_id).await.unwrap();
        assert!(action.needs_human_response());
        assert!(action.body.contains("do not resend"));
        drop(mailbox);
        let mailbox = CollaborationStore::load(options).await.unwrap();
        mailbox
            .reconcile_peer_interruptions(&[coordinator.clone(), peer.clone()])
            .await
            .unwrap();
        assert_eq!(
            mailbox
                .get_for(&sender, &request.id)
                .await
                .unwrap()
                .interruption
                .unwrap()
                .action_request_id
                .as_deref(),
            Some(action_id.as_str())
        );
        // Notifications persisted by older adapters used WaitingInput. They
        // must retain the same recovery item rather than silently lose a hold.
        peer.state = AgentState::WaitingInput;
        mailbox
            .reconcile_peer_interruptions(&[coordinator.clone(), peer.clone()])
            .await
            .unwrap();
        let legacy = mailbox.get_for(&sender, &request.id).await.unwrap();
        let interruption = legacy.interruption.as_ref().unwrap();
        assert_eq!(interruption.reason, "input_required");
        assert_eq!(
            interruption.action_request_id.as_deref(),
            Some(action_id.as_str())
        );
        peer.state = AgentState::Working;
        mailbox
            .reconcile_peer_interruptions(&[coordinator, peer])
            .await
            .unwrap();
        let resumed = mailbox.get_for(&sender, &request.id).await.unwrap();
        assert_eq!(resumed.id, request.id);
        assert_eq!(resumed.status, RequestStatus::Claimed);
        assert!(resumed.reply.is_none());
        assert!(!resumed.peer_interrupted());
        assert_eq!(
            mailbox.get_for(&human, &action_id).await.unwrap().status,
            RequestStatus::Cancelled
        );
    }

    // Keep the restart, authorization and recovery assertions on one episode.
    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn interrupted_wait_durable_action_decision_and_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let options = CollaborationOptions {
            path: Some(dir.path().join("mailbox.json")),
            ..CollaborationOptions::default()
        };
        let mailbox = CollaborationStore::load(options.clone()).await.unwrap();
        let (sender, mut coordinator) = actor("%1", "sender").await;
        let (recipient, mut peer) = actor("%2", "peer").await;
        let request = mailbox
            .create(
                sender.clone(),
                recipient.clone(),
                NewRequest {
                    body: "review".into(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        mailbox.claim_for(&recipient).await.unwrap();
        peer.state = AgentState::Error;
        peer.rate_limit_scope = Some(RateLimitScope::FiveHour);
        mailbox
            .reconcile_peer_interruptions(&[coordinator.clone(), peer.clone()])
            .await
            .unwrap();
        let waiting = mailbox.wait_for_terminal(&sender, &request.id, Duration::from_secs(300));
        let interrupted = tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .unwrap()
            .unwrap();
        assert!(interrupted.peer_interrupted());
        assert_eq!(interrupted.status, RequestStatus::Claimed);
        assert!(interrupted.reply.is_none());
        assert!(mailbox.pending_human_notifications().await.is_empty());
        coordinator.state = AgentState::Error;
        mailbox
            .reconcile_peer_interruptions(&[coordinator.clone(), peer.clone()])
            .await
            .unwrap();
        let action = mailbox.pending_human_notifications().await.remove(0);
        assert_eq!(action.from.agent_session_id, "tend:peer-recovery");
        assert_eq!(
            action.parent_request_id.as_deref(),
            Some(request.id.as_str())
        );
        assert_eq!(action.thread_id, request.thread_id);
        assert!(mailbox.get_for(&sender, &action.id).await.is_ok());
        let (unrelated, _) = actor("%9", "unrelated").await;
        assert!(mailbox.get_for(&unrelated, &action.id).await.is_err());
        assert!(!action.peer_interrupted());
        let restored = CollaborationStore::load(options).await.unwrap();
        restored
            .reconcile_peer_interruptions(&[coordinator.clone(), peer.clone()])
            .await
            .unwrap();
        assert_eq!(restored.pending_human_notifications().await.len(), 1);
        assert!(restored
            .reply(
                &recipient,
                &action.id,
                RequestStatus::Completed,
                "spoof".into(),
                vec![]
            )
            .await
            .is_err());
        restored
            .reply(
                &action.to,
                &action.id,
                RequestStatus::Completed,
                "Wait; do not reassign".into(),
                vec![],
            )
            .await
            .unwrap();
        let answer = restored
            .wait_for_terminal(&sender, &action.id, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(answer.reply.as_ref().unwrap().body, "Wait; do not reassign");
        restored
            .reconcile_peer_interruptions(&[coordinator.clone(), peer.clone()])
            .await
            .unwrap();
        let parent = restored.get_for(&sender, &request.id).await.unwrap();
        assert_eq!(
            parent.interruption.unwrap().decision.unwrap().body,
            "Wait; do not reassign"
        );
        assert_eq!(parent.status, RequestStatus::Claimed);
        peer.state = AgentState::Working;
        restored
            .reconcile_peer_interruptions(&[coordinator, peer])
            .await
            .unwrap();
        assert!(!restored
            .get_for(&sender, &request.id)
            .await
            .unwrap()
            .peer_interrupted());
        let archived_action = restored.get_for(&action.to, &action.id).await.unwrap();
        assert!(!archived_action.interruption.unwrap().active);
        assert!(archived_action.reply_read_at.is_some());
        assert!(restored.pending_reply_unnotified().await.is_empty());
        assert!(restored.pending_human_notifications().await.is_empty());
    }

    #[tokio::test]
    async fn fleet_initiator_gets_recovery_grace_without_gaining_authority() {
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let (initiator, _) = actor("%1", "controller-agent").await;
        let (recipient, mut peer) = actor("%2", "peer").await;
        let console = Participant::console(recipient.room.clone());
        let request = mailbox
            .create(
                console.clone(),
                recipient,
                NewRequest {
                    initiator: Some(Box::new(initiator.clone())),
                    body: "remote review".into(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        peer.state = AgentState::Error;
        mailbox
            .reconcile_peer_interruptions(&[peer.clone()])
            .await
            .unwrap();
        assert!(mailbox
            .get_for(&console, &request.id)
            .await
            .unwrap()
            .peer_interrupted());
        assert!(mailbox.pending_human_notifications().await.is_empty());
        assert!(mailbox
            .update_request(&initiator, &request.id, "spoof".into())
            .await
            .is_err());
        mailbox
            .requests
            .write()
            .await
            .get_mut(&request.id)
            .unwrap()
            .interruption
            .as_mut()
            .unwrap()
            .observed_at -= time::Duration::minutes(3);
        mailbox
            .update_request(&console, &request.id, "Controller handling recovery".into())
            .await
            .unwrap();
        mailbox.reconcile_peer_interruptions(&[peer]).await.unwrap();
        assert!(mailbox.pending_human_notifications().await.is_empty());
    }

    #[tokio::test]
    async fn terminal_interrupted_parent_still_closes_its_recovery_action() {
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let (sender, mut coordinator) = actor("%1", "sender").await;
        let (recipient, mut peer) = actor("%2", "peer").await;
        let request = mailbox
            .create(
                sender.clone(),
                recipient.clone(),
                NewRequest {
                    body: "finish during recovery".into(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        coordinator.state = AgentState::Stopped;
        peer.state = AgentState::Stopped;
        let agents = [coordinator, peer];
        mailbox.reconcile_peer_interruptions(&agents).await.unwrap();
        let parent = mailbox.peek_for(&sender, &request.id).await.unwrap();
        let action = parent.interruption.unwrap().action_request_id.unwrap();
        mailbox
            .reply(
                &recipient,
                &request.id,
                RequestStatus::Completed,
                "finished".into(),
                vec![],
            )
            .await
            .unwrap();
        mailbox.reconcile_peer_interruptions(&agents).await.unwrap();
        let rows = mailbox.requests.read().await;
        assert!(!rows[&request.id].interruption.as_ref().unwrap().active);
        assert_eq!(rows[&action].status, RequestStatus::Cancelled);
        drop(rows);
        let changes = mailbox.subscribe();
        mailbox.reconcile_peer_interruptions(&agents).await.unwrap();
        assert!(!changes.has_changed().unwrap());
    }

    #[tokio::test]
    async fn missing_delivered_binding_has_grace_and_never_reassigns_or_replays() {
        use crate::collaboration::WakeDeliveryState;
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let (sender, coordinator) = actor("%1", "sender").await;
        let (recipient, peer) = actor("%2", "peer").await;
        for (delivery, returned_state) in [
            (None, AgentState::WaitingInput),
            (Some(WakeDeliveryState::Prepared), AgentState::WaitingChoice),
            (Some(WakeDeliveryState::PromptWritten), AgentState::Starting),
        ] {
            let request = mailbox
                .create(
                    sender.clone(),
                    recipient.clone(),
                    NewRequest {
                        body: "already delivered work".into(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            mailbox.claim_by_id(&recipient, &request.id).await.unwrap();
            mailbox
                .requests
                .write()
                .await
                .get_mut(&request.id)
                .unwrap()
                .wake_delivery = delivery;
            let before = mailbox.peek_for(&sender, &request.id).await.unwrap();
            mailbox
                .reconcile_peer_interruptions(std::slice::from_ref(&coordinator))
                .await
                .unwrap();
            assert!(!mailbox
                .peek_for(&sender, &request.id)
                .await
                .unwrap()
                .peer_interrupted());
            *mailbox
                .missing_recipients
                .lock()
                .await
                .get_mut(&request.id)
                .unwrap() -= time::Duration::minutes(3);
            mailbox
                .reconcile_peer_interruptions(std::slice::from_ref(&coordinator))
                .await
                .unwrap();
            let uncertain = mailbox.peek_for(&sender, &request.id).await.unwrap();
            assert!(uncertain.peer_interrupted());
            assert_eq!(
                uncertain.interruption.as_ref().unwrap().reason,
                "binding_unavailable"
            );
            assert_eq!(uncertain.status, before.status);
            assert_eq!(uncertain.to, before.to);
            assert_eq!(uncertain.wake_delivery, delivery);
            assert_eq!(uncertain.reply, before.reply);
            assert!(
                mailbox.pending_human_notifications().await.is_empty(),
                "the coordinator receives the first recovery opportunity"
            );
            mailbox.missing_recipients.lock().await.remove(&request.id);
            // A persisted active uncertainty remains visible after tracker reset.
            mailbox
                .reconcile_peer_interruptions(std::slice::from_ref(&coordinator))
                .await
                .unwrap();
            assert!(mailbox
                .peek_for(&sender, &request.id)
                .await
                .unwrap()
                .peer_interrupted());
            let mut returned = peer.clone();
            returned.state = returned_state;
            mailbox
                .reconcile_peer_interruptions(&[coordinator.clone(), returned])
                .await
                .unwrap();
            assert!(!mailbox
                .peek_for(&sender, &request.id)
                .await
                .unwrap()
                .peer_interrupted());
            assert!(!mailbox
                .missing_recipients
                .lock()
                .await
                .contains_key(&request.id));
        }
    }

    #[tokio::test]
    async fn notified_unclaimed_binding_is_uncertain_and_return_cancels_recovery_action() {
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let (sender, coordinator) = actor("%1", "sender").await;
        let (recipient, mut peer) = actor("%2", "peer").await;
        let request = mailbox
            .create(
                sender.clone(),
                recipient,
                NewRequest {
                    body: "inbox notification delivered".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        mailbox.mark_notified(&request.id).await.unwrap();
        mailbox
            .reconcile_peer_interruptions(std::slice::from_ref(&coordinator))
            .await
            .unwrap();
        *mailbox
            .missing_recipients
            .lock()
            .await
            .get_mut(&request.id)
            .unwrap() -= time::Duration::minutes(3);
        mailbox
            .reconcile_peer_interruptions(std::slice::from_ref(&coordinator))
            .await
            .unwrap();
        let uncertain = mailbox.peek_for(&sender, &request.id).await.unwrap();
        assert!(uncertain.peer_interrupted());
        assert_eq!(uncertain.status, RequestStatus::Queued);
        mailbox
            .requests
            .write()
            .await
            .get_mut(&request.id)
            .unwrap()
            .interruption
            .as_mut()
            .unwrap()
            .observed_at -= time::Duration::minutes(3);
        mailbox
            .reconcile_peer_interruptions(std::slice::from_ref(&coordinator))
            .await
            .unwrap();
        let action = mailbox.pending_human_notifications().await.remove(0);
        assert!(action.body.contains("process exit is not established"));
        peer.state = AgentState::Stopped;
        mailbox
            .reconcile_peer_interruptions(&[coordinator.clone(), peer.clone()])
            .await
            .unwrap();
        let current_action = mailbox.peek_for(&sender, &action.id).await.unwrap();
        assert!(current_action.body.contains("(stopped)"));
        assert!(!current_action
            .body
            .contains("cannot verify the recipient binding"));
        peer.state = AgentState::Working;
        mailbox
            .reconcile_peer_interruptions(&[coordinator, peer])
            .await
            .unwrap();
        assert!(!mailbox
            .peek_for(&sender, &request.id)
            .await
            .unwrap()
            .peer_interrupted());
        assert_eq!(
            mailbox.peek_for(&sender, &action.id).await.unwrap().status,
            RequestStatus::Cancelled
        );
        assert!(mailbox.pending_human_notifications().await.is_empty());
    }

    #[tokio::test]
    async fn missing_queued_and_pending_recipients_are_not_interrupted() {
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let (sender, coordinator) = actor("%1", "sender").await;
        let (recipient, _) = actor("%2", "peer").await;
        let queued = mailbox
            .create(
                sender.clone(),
                recipient.clone(),
                NewRequest {
                    body: "not yet delivered".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        mailbox
            .reconcile_peer_interruptions(std::slice::from_ref(&coordinator))
            .await
            .unwrap();
        assert!(mailbox.missing_recipients.lock().await.is_empty());
        assert!(!mailbox
            .peek_for(&sender, &queued.id)
            .await
            .unwrap()
            .peer_interrupted());
        let mut pending = recipient;
        pending.agent_session_id =
            format!("{}default:%2", crate::collaboration::PENDING_SESSION_PREFIX);
        let request = mailbox
            .create(
                sender.clone(),
                pending,
                NewRequest {
                    body: "pending agent startup".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        mailbox
            .requests
            .write()
            .await
            .get_mut(&request.id)
            .unwrap()
            .wake_delivery = Some(crate::collaboration::WakeDeliveryState::Prepared);
        mailbox
            .reconcile_peer_interruptions(&[coordinator])
            .await
            .unwrap();
        assert!(mailbox.missing_recipients.lock().await.is_empty());
        assert!(!mailbox
            .peek_for(&sender, &request.id)
            .await
            .unwrap()
            .peer_interrupted());
    }

    #[tokio::test]
    async fn expired_unanswered_recovery_is_restored_once_but_cancellation_and_completion_stay_final(
    ) {
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let (sender, mut coordinator) = actor("%1", "sender").await;
        let (recipient, mut peer) = actor("%2", "peer").await;
        let request = mailbox
            .create(
                sender.clone(),
                recipient.clone(),
                NewRequest {
                    body: "review".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        coordinator.state = AgentState::Stopped;
        peer.state = AgentState::Stopped;
        let agents = [coordinator, peer];
        mailbox.reconcile_peer_interruptions(&agents).await.unwrap();
        let action = mailbox.pending_human_notifications().await.remove(0);
        {
            let mut rows = mailbox.requests.write().await;
            let expired = rows.get_mut(&action.id).unwrap();
            expired.status = RequestStatus::Expired;
            expired.notified_at = Some(OffsetDateTime::now_utc());
        }
        mailbox.reconcile_peer_interruptions(&agents).await.unwrap();
        let restored = mailbox.peek_for(&sender, &action.id).await.unwrap();
        assert_eq!(restored.status, RequestStatus::Queued);
        assert!(restored.notified_at.is_none());
        assert_eq!(mailbox.requests.read().await.len(), 2);
        let changes = mailbox.subscribe();
        mailbox.reconcile_peer_interruptions(&agents).await.unwrap();
        assert!(
            !changes.has_changed().unwrap(),
            "restoration must not repeat"
        );
        mailbox
            .requests
            .write()
            .await
            .get_mut(&action.id)
            .unwrap()
            .status = RequestStatus::Cancelled;
        mailbox.reconcile_peer_interruptions(&agents).await.unwrap();
        assert_eq!(
            mailbox.peek_for(&sender, &action.id).await.unwrap().status,
            RequestStatus::Cancelled
        );
        mailbox
            .requests
            .write()
            .await
            .get_mut(&action.id)
            .unwrap()
            .status = RequestStatus::Expired;
        mailbox
            .reply(
                &recipient,
                &request.id,
                RequestStatus::Completed,
                "done".into(),
                vec![],
            )
            .await
            .unwrap();
        mailbox.reconcile_peer_interruptions(&agents).await.unwrap();
        assert_eq!(
            mailbox.peek_for(&sender, &action.id).await.unwrap().status,
            RequestStatus::Expired
        );
    }

    #[tokio::test]
    async fn grace_ack_identity_and_obsolete_action() {
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let (sender, coordinator) = actor("%1", "sender").await;
        let (recipient, mut peer) = actor("%2", "peer").await;
        let request = mailbox
            .create(
                sender.clone(),
                recipient,
                NewRequest {
                    body: "task".into(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        peer.state = AgentState::Stopped;
        let mut reused = peer.clone();
        reused.session_id = "another-session".into();
        mailbox
            .reconcile_peer_interruptions(&[coordinator.clone(), reused])
            .await
            .unwrap();
        assert!(!mailbox
            .get_for(&sender, &request.id)
            .await
            .unwrap()
            .peer_interrupted());
        mailbox
            .reconcile_peer_interruptions(&[coordinator.clone(), peer.clone()])
            .await
            .unwrap();
        mailbox
            .requests
            .write()
            .await
            .get_mut(&request.id)
            .unwrap()
            .interruption
            .as_mut()
            .unwrap()
            .observed_at -= time::Duration::minutes(3);
        mailbox
            .update_request(&sender, &request.id, "Handling recovery".into())
            .await
            .unwrap();
        mailbox
            .reconcile_peer_interruptions(&[coordinator.clone(), peer.clone()])
            .await
            .unwrap();
        assert!(mailbox.pending_human_notifications().await.is_empty());
        mailbox
            .requests
            .write()
            .await
            .get_mut(&request.id)
            .unwrap()
            .updates
            .clear();
        mailbox
            .reconcile_peer_interruptions(&[coordinator.clone(), peer.clone()])
            .await
            .unwrap();
        assert_eq!(mailbox.pending_human_notifications().await.len(), 1);
        peer.state = AgentState::Working;
        mailbox
            .reconcile_peer_interruptions(&[coordinator, peer])
            .await
            .unwrap();
        assert!(mailbox.pending_human_notifications().await.is_empty());
    }
    #[tokio::test]
    async fn recovery_follows_real_session_moves_but_not_cross_socket_synthetic_ids() {
        let (participant, mut agent) = actor("%1", "real-session").await;
        agent.pane = Some("%9".into());
        assert!(exact_agent(&participant, std::slice::from_ref(&agent)).is_some());
        let mut synthetic = participant;
        synthetic.agent_session_id = format!("{}%1", crate::state::SYNTHETIC_SESSION_PREFIX);
        agent.session_id = synthetic.agent_session_id.clone();
        agent.pane = Some("%1".into());
        agent.tmux_socket = Some("other-server".into());
        assert!(exact_agent(&synthetic, &[agent]).is_none());
    }

    async fn age_reminders(mailbox: &CollaborationStore, by: time::Duration) {
        let mut requests = mailbox.requests.write().await;
        for request in requests.values_mut() {
            if let Some(offer) = request.turn_offer.as_mut() {
                offer.last_reply_reminder_at = offer.last_reply_reminder_at.map(|at| at - by);
                offer.unanswered_stop_at = offer.unanswered_stop_at.map(|at| at - by);
            }
        }
    }

    #[tokio::test]
    async fn a_reminded_recipient_that_stays_idle_without_replying_is_interrupted() {
        use crate::collaboration::turn_offer::{TurnBoundaryEvent, OFFER_BACKOFF};
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let (sender, coordinator) = actor("%1", "sender").await;
        let (recipient, mut peer) = actor("%2", "peer").await;
        let request = mailbox
            .create(
                sender.clone(),
                recipient.clone(),
                NewRequest {
                    body: "review".into(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        mailbox.claim_by_id(&recipient, &request.id).await.unwrap();
        peer.state = AgentState::Idle;
        // Idle without a reminder yet: not ours to judge (no Stop hook seen).
        mailbox
            .reconcile_peer_interruptions(&[coordinator.clone(), peer.clone()])
            .await
            .unwrap();
        assert!(!mailbox
            .get_for(&sender, &request.id)
            .await
            .unwrap()
            .peer_interrupted());
        let reminders = mailbox
            .offer_turn_boundary(&recipient, TurnBoundaryEvent::Stop, None, None)
            .await
            .unwrap();
        assert_eq!(reminders.len(), 1);
        // The reminder gets a full backoff to produce a reply.
        mailbox
            .reconcile_peer_interruptions(&[coordinator.clone(), peer.clone()])
            .await
            .unwrap();
        assert!(!mailbox
            .get_for(&sender, &request.id)
            .await
            .unwrap()
            .peer_interrupted());
        age_reminders(&mailbox, OFFER_BACKOFF).await;
        mailbox
            .reconcile_peer_interruptions(&[coordinator.clone(), peer.clone()])
            .await
            .unwrap();
        let held = mailbox.get_for(&sender, &request.id).await.unwrap();
        assert!(held.peer_interrupted(), "the silent stall now has an owner");
        assert_eq!(held.status, RequestStatus::Claimed);
        assert_eq!(held.interruption.as_ref().unwrap().reason, "reply_overdue");
        // The healthy sender sees it first; the human item follows the grace.
        assert!(mailbox.pending_human_notifications().await.is_empty());
        {
            let mut requests = mailbox.requests.write().await;
            let row = requests.get_mut(&request.id).unwrap();
            let info = row.interruption.as_mut().unwrap();
            info.observed_at -= time::Duration::minutes(2);
        }
        mailbox
            .reconcile_peer_interruptions(&[coordinator.clone(), peer.clone()])
            .await
            .unwrap();
        let action = mailbox.pending_human_notifications().await.remove(0);
        assert!(action.body.contains("was reminded to reply"));
        // The recipient resumes: the hold and its human item clear.
        peer.state = AgentState::Working;
        mailbox
            .reconcile_peer_interruptions(&[coordinator.clone(), peer.clone()])
            .await
            .unwrap();
        assert!(!mailbox
            .get_for(&sender, &request.id)
            .await
            .unwrap()
            .peer_interrupted());
        assert!(mailbox.pending_human_notifications().await.is_empty());
        // Idle again without a new Stop (e.g. inferred by a timeout): the old
        // episode's evidence is gone, so nothing re-raises.
        peer.state = AgentState::Idle;
        age_reminders(&mailbox, OFFER_BACKOFF).await;
        mailbox
            .reconcile_peer_interruptions(&[coordinator.clone(), peer.clone()])
            .await
            .unwrap();
        assert!(!mailbox
            .get_for(&sender, &request.id)
            .await
            .unwrap()
            .peer_interrupted());
        // A new Stop without a reply is a new episode and is judged again.
        mailbox
            .offer_turn_boundary(&recipient, TurnBoundaryEvent::Stop, None, None)
            .await
            .unwrap();
        age_reminders(&mailbox, OFFER_BACKOFF).await;
        mailbox
            .reconcile_peer_interruptions(&[coordinator.clone(), peer.clone()])
            .await
            .unwrap();
        assert!(mailbox
            .get_for(&sender, &request.id)
            .await
            .unwrap()
            .peer_interrupted());
    }

    #[tokio::test]
    async fn waiting_on_a_child_reply_or_its_wake_is_not_overdue() {
        use crate::collaboration::turn_offer::{TurnBoundaryEvent, OFFER_BACKOFF};
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let (sender, coordinator) = actor("%1", "sender").await;
        let (recipient, mut peer) = actor("%2", "peer").await;
        let (child_peer, _) = actor("%3", "child").await;
        let request = mailbox
            .create(
                sender.clone(),
                recipient.clone(),
                NewRequest {
                    body: "review".into(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        mailbox.claim_by_id(&recipient, &request.id).await.unwrap();
        // Reminded at a Stop, then (without any observed Working) it
        // delegates and ends its turn to wait for the child.
        mailbox
            .offer_turn_boundary(&recipient, TurnBoundaryEvent::Stop, None, None)
            .await
            .unwrap();
        let child = mailbox
            .create(
                recipient.clone(),
                child_peer.clone(),
                NewRequest {
                    body: "sub-review".into(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        assert!(mailbox
            .offer_turn_boundary(&recipient, TurnBoundaryEvent::Stop, None, None)
            .await
            .unwrap()
            .is_empty());
        age_reminders(&mailbox, OFFER_BACKOFF).await;
        peer.state = AgentState::Idle;
        let agents = [coordinator.clone(), peer.clone()];
        mailbox.reconcile_peer_interruptions(&agents).await.unwrap();
        assert!(!mailbox
            .get_for(&sender, &request.id)
            .await
            .unwrap()
            .peer_interrupted());
        // The child replies; its wake to the recipient is still owed.
        mailbox.claim_by_id(&child_peer, &child.id).await.unwrap();
        mailbox
            .reply(
                &child_peer,
                &child.id,
                RequestStatus::Completed,
                "ok".into(),
                vec![],
            )
            .await
            .unwrap();
        mailbox.reconcile_peer_interruptions(&agents).await.unwrap();
        assert!(
            !mailbox
                .get_for(&sender, &request.id)
                .await
                .unwrap()
                .peer_interrupted(),
            "an undelivered reply wake is still a wait, not a stall"
        );
        // The wake lands as a new prompt: the old Stop no longer counts.
        mailbox
            .requests
            .write()
            .await
            .get_mut(&child.id)
            .unwrap()
            .mark_reply_read(OffsetDateTime::now_utc());
        peer.last_prompt_at = Some(OffsetDateTime::now_utc());
        mailbox
            .reconcile_peer_interruptions(&[coordinator, peer])
            .await
            .unwrap();
        assert!(!mailbox
            .get_for(&sender, &request.id)
            .await
            .unwrap()
            .peer_interrupted());
    }

    /// The Stop hook asks for offers before the stop is recorded: a
    /// reconcile that runs in between sees the ending turn as Working and
    /// must keep the Stop marker, so the idle episode can still be judged.
    #[tokio::test]
    async fn a_reconcile_between_the_stop_offer_and_the_recorded_stop_keeps_the_marker() {
        use crate::collaboration::turn_offer::{TurnBoundaryEvent, OFFER_BACKOFF};
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let (sender, coordinator) = actor("%1", "sender").await;
        let (recipient, mut peer) = actor("%2", "peer").await;
        let request = mailbox
            .create(
                sender.clone(),
                recipient.clone(),
                NewRequest {
                    body: "review".into(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        mailbox.claim_by_id(&recipient, &request.id).await.unwrap();
        // The turn began before the Stop: Working, entered earlier.
        peer.state = AgentState::Working;
        peer.state_entered_at = OffsetDateTime::now_utc() - time::Duration::seconds(30);
        mailbox
            .offer_turn_boundary(&recipient, TurnBoundaryEvent::Stop, None, None)
            .await
            .unwrap();
        mailbox
            .reconcile_peer_interruptions(&[coordinator.clone(), peer.clone()])
            .await
            .unwrap();
        // Now the stop is recorded: Idle.
        peer.state = AgentState::Idle;
        peer.state_entered_at = OffsetDateTime::now_utc();
        age_reminders(&mailbox, OFFER_BACKOFF).await;
        mailbox
            .reconcile_peer_interruptions(&[coordinator, peer])
            .await
            .unwrap();
        let held = mailbox.get_for(&sender, &request.id).await.unwrap();
        assert_eq!(held.interruption.unwrap().reason, "reply_overdue");
    }
}
