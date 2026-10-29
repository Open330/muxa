//! Operator diagnostics over the existing memory projection; never copies bodies.
use super::{
    CollaborationError, CollaborationRequest, CollaborationStore, RequestKind, RequestStatus,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

pub const COLLABORATION_HEALTH_CAPABILITY: &str = "collaboration_health_v1";
/// v2 uses a distinct operation name, so a downgrade between connections
/// cannot make an old daemon ignore the filters and perform broad expiry.
pub const FILTERED_EXPIRE_CAPABILITY: &str = "collaboration_expire_filtered_v2";

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CollaborationHealth {
    #[serde(default)]
    pub synthetic_agents: crate::agent_directory::SyntheticDiagnostics,
    pub interrupted_requests: usize,
    pub binding_unavailable: usize,
    pub pending_recovery_decisions: usize,
    pub example_request_ids: Vec<String>,
    pub stale_queued: usize,
    pub stale_quiet_notices: usize,
    pub example_quiet_notice_ids: Vec<String>,
}

impl CollaborationRequest {
    pub fn is_undelivered_quiet_notice(&self) -> bool {
        self.kind == RequestKind::Notice
            && !self.expects_reply
            && !self.should_notify()
            && self.human_action.is_none()
            && self.notified_at.is_none()
            && self.wake_delivery.is_none()
    }
}

fn sample(ids: &mut Vec<(OffsetDateTime, String)>, request: &CollaborationRequest) {
    // Imported legacy IDs need not have the generated ID's small size. Keep
    // counts exact but omit oversized identifiers from the bounded sample.
    if request.id.len() > 128 {
        return;
    }
    // Deterministic and bounded even for a large mailbox.
    ids.push((request.created_at, request.id.clone()));
    ids.sort_unstable();
    ids.truncate(3);
}

impl CollaborationStore {
    pub async fn health(&self) -> Result<CollaborationHealth, CollaborationError> {
        self.ensure_enabled()?;
        let requests = self.requests.read().await;
        let cutoff = OffsetDateTime::now_utc() - time::Duration::days(1);
        let mut health = CollaborationHealth::default();
        let mut interrupted_examples = Vec::new();
        let mut quiet_examples = Vec::new();
        for request in requests.values() {
            health.pending_recovery_decisions += usize::from(
                request.from.agent_session_id == "muxa:peer-recovery"
                    && request.needs_human_response(),
            );
            // Human recovery records mirror the parent's interruption; only
            // the exact original request owns an interruption episode.
            if request.peer_interrupted() {
                health.interrupted_requests += 1;
                let info = request.interruption.as_ref().expect("active interruption");
                health.binding_unavailable += usize::from(info.reason == "binding_unavailable");
                sample(&mut interrupted_examples, request);
            }
            if request.status == RequestStatus::Queued
                && request.created_at < cutoff
                && !request.needs_human_response()
            {
                health.stale_queued += 1;
                if request.is_undelivered_quiet_notice() {
                    health.stale_quiet_notices += 1;
                    sample(&mut quiet_examples, request);
                }
            }
        }
        health.example_request_ids = interrupted_examples.into_iter().map(|(_, id)| id).collect();
        health.example_quiet_notice_ids = quiet_examples.into_iter().map(|(_, id)| id).collect();
        Ok(health)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collaboration::{
        CollaborationOptions, HumanAction, NewRequest, Participant, PeerInterruption,
    };

    fn participant(session: &str) -> Participant {
        serde_json::from_value(
            serde_json::json!({"agent_kind":"codex","agent_session_id":session,
            "pane":"%1","room":{"host":"tmux","window_id":"@1"},"state":"idle"}),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn health_counts_originals_not_recovery_copies_without_reading_or_mutating_bodies() {
        let store = CollaborationStore::in_memory(CollaborationOptions {
            max_message_bytes: 100_000,
            ..Default::default()
        });
        for index in 0..6 {
            let mut parent = store
                .create(
                    participant("a"),
                    participant("b"),
                    NewRequest {
                        body: format!("PRIVATE {}", "x".repeat(50_000)),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            parent.status = if index == 5 {
                RequestStatus::Completed
            } else {
                RequestStatus::Claimed
            };
            parent.interruption = Some(PeerInterruption {
                request_id: parent.id.clone(),
                reason: if index == 4 {
                    "stopped"
                } else {
                    "binding_unavailable"
                }
                .into(),
                active: true,
                observed_at: parent.created_at,
                reset_at: None,
                action_request_id: Some(format!("action-{index}")),
                decision: None,
            });
            let mut action = parent.clone();
            action.id = format!("action-{index}");
            action.parent_request_id = Some(parent.id.clone());
            action.from.agent_session_id = "muxa:peer-recovery".into();
            action.to = Participant::console(parent.from.room.clone());
            action.kind = RequestKind::Question;
            action.human_action = Some(HumanAction::Choice);
            action.status = if index < 3 {
                RequestStatus::Queued
            } else {
                RequestStatus::Completed
            };
            let mut rows = store.requests.write().await;
            rows.insert(parent.id.clone(), parent);
            rows.insert(action.id.clone(), action);
        }
        let before = store.requests.read().await.clone();
        let changes = store.subscribe();
        let health = store.health().await.unwrap();
        assert_eq!(health.interrupted_requests, 5);
        assert_eq!(health.binding_unavailable, 4);
        assert_eq!(health.pending_recovery_decisions, 3);
        assert_eq!(health.example_request_ids.len(), 3);
        let serialized = serde_json::to_string(&health).unwrap();
        assert!(!serialized.contains("PRIVATE"));
        assert!(serialized.len() < 1500);
        assert_eq!(*store.requests.read().await, before);
        assert!(!changes.has_changed().unwrap());
    }

    #[tokio::test]
    async fn selective_expiry_rechecks_id_age_delivery_claim_and_notice_intent() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let mut ids = Vec::new();
        for index in 0..9 {
            let mut request = store
                .create(
                    participant("a"),
                    participant("b"),
                    NewRequest {
                        kind: RequestKind::Notice,
                        expects_reply: false,
                        body: format!("notice-{index}"),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            request.created_at -= time::Duration::days(2);
            match index {
                2 => request.notified_at = Some(OffsetDateTime::now_utc()),
                3 => request.notify = Some(true),
                4 => {
                    request.kind = RequestKind::Task;
                    request.expects_reply = true;
                }
                5 => request.status = RequestStatus::Claimed,
                6 => request.created_at = OffsetDateTime::now_utc(),
                7 => request.wake_delivery = Some(super::super::WakeDeliveryState::Prepared),
                8 => {
                    request.kind = RequestKind::Question;
                    request.expects_reply = true;
                    request.human_action = Some(HumanAction::Choice);
                    request.to.console = true;
                }
                _ => (),
            }
            ids.push(request.id.clone());
            store
                .requests
                .write()
                .await
                .insert(request.id.clone(), request);
        }
        assert_eq!(store.health().await.unwrap().stale_quiet_notices, 2);
        let cutoff = OffsetDateTime::now_utc() - time::Duration::days(1);
        let before = store.requests.read().await.clone();
        let preview = store
            .expire_queued_filtered(cutoff, true, true, Some(&ids[0]))
            .await
            .unwrap();
        assert_eq!(preview.len(), 1);
        assert_eq!(preview[0].id, ids[0]);
        assert_eq!(*store.requests.read().await, before);
        let expired = store
            .expire_queued_filtered(cutoff, false, true, Some(&ids[0]))
            .await
            .unwrap();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].status, RequestStatus::Expired);
        // The preview is not a reservation: a concurrent claim wins.
        store
            .requests
            .write()
            .await
            .get_mut(&ids[1])
            .unwrap()
            .status = RequestStatus::Claimed;
        assert!(store
            .expire_queued_filtered(cutoff, false, true, None)
            .await
            .unwrap()
            .is_empty());
        let rows = store.requests.read().await;
        for id in &ids[2..] {
            assert_eq!(rows[id], before[id]);
        }
        drop(rows);
        let broad = store.expire_queued_before(cutoff, false).await.unwrap();
        assert!(
            !broad.iter().any(|r| r.id == ids[8]),
            "human decisions must survive legacy bulk expiry too"
        );
        assert_eq!(
            store.requests.read().await[&ids[8]].status,
            RequestStatus::Queued
        );
    }
}
