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

fn exact_agent<'a>(participant: &Participant, agents: &'a [Agent]) -> Option<&'a Agent> {
    if participant.console {
        return None;
    }
    agents.iter().find(|a| {
        a.kind == participant.agent_kind
            && a.session_id == participant.agent_session_id
            && a.pane.as_deref() == Some(&participant.pane)
            && match (a.tmux_socket.as_deref(), participant.socket.as_deref()) {
                (Some(a), Some(b)) => {
                    crate::backend::pane_endpoints_match(Some(&participant.pane), a, b)
                }
                (None, None) => true,
                _ => false,
            }
    })
}

fn reason(agent: &Agent) -> Option<&'static str> {
    match agent.state {
        AgentState::Error if agent.rate_limit_scope.is_some() => Some("rate_limited"),
        AgentState::Error => Some("error"),
        AgentState::Stopped => Some("stopped"),
        _ => None,
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
    action.from = Participant::console(parent.from.room.clone());
    action.from.console = false;
    action.from.agent_kind = AgentKind::Task;
    action.from.agent_session_id = "muxa:peer-recovery".into();
    action.from.pane = "muxa-recovery".into();
    action.from.alias = Some("Muxa recovery".into());
    action.to = Participant::console(parent.from.room.clone());
    action.initiator = None;
    action.provenance = None;
    action.parent_request_id = Some(parent.id.clone());
    action.kind = RequestKind::Question;
    action.work_mode = WorkMode::ReadOnly;
    action.human_action = Some(HumanAction::Choice);
    action.body = format!("Muxa detected an interrupted peer: {} ({}).\nOriginal request: {}\nReset: {}\nChoose: wait and recheck, arrange a safe handoff, or stop this work attempt. Recommendation: inspect existing work before reassignment; a capped agent may resume. Your answer records a decision; it does not restart, cancel, or spend automatically.", parent.to.agent_session_id, current_reason, parent.id, parent.interruption.as_ref().and_then(|i| i.reset_at.as_deref()).unwrap_or("unknown"));
    action.status = RequestStatus::Queued;
    action.created_at = now;
    action.claimed_at = None;
    action.notified_at = None;
    action.reply_notified_at = None;
    action.reply_read_at = None;
    action.wake_delivery = None;
    action.reply = None;
    action.updates.clear();
    action
}

impl CollaborationStore {
    /// Reconcile a local authoritative registry. Unknown/missing identities never
    /// imply death or quota exhaustion. Updates and human actions persist together.
    pub async fn reconcile_peer_interruptions(
        &self,
        agents: &[Agent],
    ) -> Result<(), CollaborationError> {
        self.ensure_enabled()?;
        let _transaction = self.transaction_lock.lock().await;
        let now = OffsetDateTime::now_utc();
        let snapshot = self.requests.read().await.clone();
        let mut changed = Vec::new();
        for original in snapshot
            .values()
            .filter(|r| !r.to.console && r.expects_reply && r.kind != RequestKind::Notice)
        {
            let agent = exact_agent(&original.to, agents);
            let current_reason = agent.and_then(reason);
            let mut parent = original.clone();
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
                if info.action_request_id.is_none() && (coordinator_unavailable || overdue) {
                    let id = next_request_id(now);
                    info.action_request_id = Some(id.clone());
                    changed.push(recovery_action(&parent, id, now, current_reason));
                }
            } else if parent.status.is_terminal()
                || agent.is_some_and(|a| matches!(a.state, AgentState::Working | AgentState::Idle))
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
                    if !info.active
                        && !action.status.is_terminal()
                        && action.from.agent_session_id == "muxa:peer-recovery"
                    {
                        let mut obsolete = action.clone();
                        obsolete.status = RequestStatus::Cancelled;
                        changed.push(obsolete);
                    }
                }
            }
            if parent != *original {
                changed.push(parent);
            }
        }
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
            "agent_kind":"codex", "agent_session_id":session, "pane":pane, "socket":"default",
            "room":{"host":"tmux","socket":"default","window_id":"@1"}, "state":"working"
        }))
        .unwrap();
        (participant, agent)
    }

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
        assert_eq!(action.from.agent_session_id, "muxa:peer-recovery");
        assert_eq!(
            action.parent_request_id.as_deref(),
            Some(request.id.as_str())
        );
        assert_eq!(action.thread_id, request.thread_id);
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
                vec![],
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
                vec![],
            )
            .await
            .unwrap();
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
}
