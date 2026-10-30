//! Explicit operator housekeeping, distinct from a recipient's work report.
use super::{
    CollaborationError, CollaborationRequest, CollaborationStore, RequestKind, RequestStatus,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

pub const RETIRE_CAPABILITY: &str = "collaboration_retire_v1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RequestRetirement {
    pub reason: String,
    pub evidence: String,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

impl CollaborationStore {
    /// Retire one reviewed interrupted attempt.
    /// This does not stop processes, report success, answer a human decision,
    /// or authorize a replacement. Evidence is the operator's assertion.
    pub async fn retire_request(
        &self,
        request_id: &str,
        reason: &str,
        evidence: &str,
        dry_run: bool,
    ) -> Result<CollaborationRequest, CollaborationError> {
        self.ensure_enabled()?;
        if reason.trim().is_empty()
            || evidence.trim().is_empty()
            || reason.len() > 1024
            || evidence.len() > 4096
        {
            return Err(CollaborationError::InvalidRetirement(
                "nonempty reason (max 1024 bytes) and evidence (max 4096 bytes) are required"
                    .into(),
            ));
        }
        let _transaction = self.transaction_lock.lock().await;
        let requests = self.requests.read().await;
        let original = requests
            .get(request_id)
            .ok_or_else(|| CollaborationError::NotFound(request_id.into()))?;
        if let Some(retirement) = &original.retirement {
            if retirement.reason == reason && retirement.evidence == evidence {
                return Ok(original.clone());
            }
        }
        if original.status.is_terminal() {
            return Err(CollaborationError::AlreadyTerminal(request_id.into()));
        }
        if original.human_action.is_some()
            || original.to.console
            || original.from.agent_session_id == "muxa:peer-recovery"
            || original.wake_delivery.is_some()
            || original.kind == RequestKind::Notice
            || !original.expects_reply
            || !original.peer_interrupted()
        {
            return Err(CollaborationError::InvalidRetirement("only an interrupted original request without in-flight wake delivery can be retired; human decisions are protected".into()));
        }
        let mut retired = original.clone();
        retired.status = RequestStatus::Cancelled;
        retired.retirement = Some(RequestRetirement {
            reason: reason.into(),
            evidence: evidence.into(),
            at: OffsetDateTime::now_utc(),
        });
        if let Some(info) = &mut retired.interruption {
            info.active = false;
        }
        let mut changed = vec![retired.clone()];
        if let Some(action) = original
            .interruption
            .as_ref()
            .and_then(|i| i.action_request_id.as_ref())
            .and_then(|id| requests.get(id))
        {
            if action.from.agent_session_id == "muxa:peer-recovery"
                && action.parent_request_id.as_deref() == Some(request_id)
                && action.to.console
                && action.kind == RequestKind::Question
                && action.human_action == Some(super::HumanAction::Choice)
                && action.reply.is_none()
                && !action.status.is_terminal()
            {
                let mut closed = action.clone();
                closed.status = RequestStatus::Cancelled;
                if let Some(info) = &mut closed.interruption {
                    info.active = false;
                }
                changed.push(closed);
            }
        }
        drop(requests);
        if !dry_run {
            self.commit_request_rows(&changed).await?;
            self.publish_change();
        }
        Ok(retired)
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

    async fn interrupted(store: &CollaborationStore) -> CollaborationRequest {
        let mut parent = store
            .create(
                participant("sender"),
                participant("recipient"),
                NewRequest {
                    body: "original instructions".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        parent.status = RequestStatus::Claimed;
        parent.interruption = Some(PeerInterruption {
            request_id: parent.id.clone(),
            reason: "binding_unavailable".into(),
            active: true,
            observed_at: parent.created_at,
            reset_at: None,
            action_request_id: Some(format!("action-{}", parent.id)),
            decision: None,
        });
        let mut action = parent.clone();
        action.id = parent
            .interruption
            .as_ref()
            .unwrap()
            .action_request_id
            .clone()
            .unwrap();
        action.parent_request_id = Some(parent.id.clone());
        action.from.agent_session_id = "muxa:peer-recovery".into();
        action.to = Participant::console(parent.from.room.clone());
        action.kind = RequestKind::Question;
        action.human_action = Some(HumanAction::Choice);
        action.status = RequestStatus::Queued;
        store
            .commit_request_rows(&[parent.clone(), action])
            .await
            .unwrap();
        parent
    }

    #[tokio::test]
    async fn retirement_is_atomic_durable_idempotent_and_never_a_human_answer_or_peer_reply() {
        let dir = tempfile::tempdir().unwrap();
        let opts = CollaborationOptions {
            path: Some(dir.path().join("requests.sqlite3")),
            ..Default::default()
        };
        let store = CollaborationStore::load(opts.clone()).await.unwrap();
        let parent = interrupted(&store).await;
        let before = store.requests.read().await.clone();
        let changes = store.subscribe();
        let preview = store
            .retire_request(&parent.id, "verified handoff", "replacement-request", true)
            .await
            .unwrap();
        assert_eq!(preview.status, RequestStatus::Cancelled);
        assert_eq!(*store.requests.read().await, before);
        assert!(!changes.has_changed().unwrap());
        let retired = store
            .retire_request(&parent.id, "verified handoff", "replacement-request", false)
            .await
            .unwrap();
        assert_eq!(retired.body, parent.body);
        assert_eq!(retired.to, parent.to);
        assert!(retired.reply.is_none());
        assert!(!retired.peer_interrupted());
        let action = store.requests.read().await[retired
            .interruption
            .as_ref()
            .unwrap()
            .action_request_id
            .as_ref()
            .unwrap()]
        .clone();
        assert_eq!(action.status, RequestStatus::Cancelled);
        assert!(action.reply.is_none());
        assert!(action.interruption.is_some_and(|i| !i.active));
        assert_eq!(store.health().await.unwrap().pending_recovery_decisions, 0);
        assert_eq!(
            store
                .retire_request(&parent.id, "verified handoff", "replacement-request", false)
                .await
                .unwrap(),
            retired
        );
        drop(store);
        let loaded = CollaborationStore::load(opts).await.unwrap();
        assert_eq!(loaded.requests.read().await[&parent.id], retired);
        loaded.reconcile_peer_interruptions(&[]).await.unwrap();
        assert_eq!(loaded.health().await.unwrap().pending_recovery_decisions, 0);
        assert_eq!(loaded.requests.read().await[&parent.id], retired);
        let stable = loaded.subscribe();
        loaded.reconcile_peer_interruptions(&[]).await.unwrap();
        assert!(!stable.has_changed().unwrap());
    }

    #[tokio::test]
    async fn retirement_rechecks_after_preview_and_retains_late_reports_without_reopening() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let parent = interrupted(&store).await;
        store
            .retire_request(&parent.id, "withdrawn review", "update:2", true)
            .await
            .unwrap();
        store
            .reply(
                &parent.to,
                &parent.id,
                RequestStatus::Completed,
                "real peer result".into(),
                vec![],
                vec![],
            )
            .await
            .unwrap();
        assert!(matches!(
            store
                .retire_request(&parent.id, "withdrawn review", "update:2", false)
                .await,
            Err(CollaborationError::AlreadyTerminal(_))
        ));
        let second = interrupted(&store).await;
        store
            .retire_request(&second.id, "obsolete review", "commit:abc", false)
            .await
            .unwrap();
        let late = store
            .reply(
                &second.to,
                &second.id,
                RequestStatus::Completed,
                "late real report".into(),
                vec![],
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(late.status, RequestStatus::Cancelled);
        assert!(late.reply.is_none());
        assert_eq!(late.late_replies.len(), 1);
        assert_eq!(late.late_replies[0].body, "late real report");
        let retry = store
            .reply(
                &second.to,
                &second.id,
                RequestStatus::Completed,
                "late real report".into(),
                vec![],
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(retry.late_replies.len(), 1);
        let mut metadata = late.clone();
        metadata.omit_list_bodies();
        assert!(metadata.retirement.unwrap().evidence.is_empty());
        assert!(!serde_json::to_string(&metadata.late_replies)
            .unwrap()
            .contains("late real report"));
    }

    #[tokio::test]
    async fn retirement_requires_evidence_and_protects_healthy_work_humans_and_inflight_wakes() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let parent = interrupted(&store).await;
        for (reason, evidence) in [(" ", "proof"), ("reason", ""), ("reason", " \n")] {
            assert!(store
                .retire_request(&parent.id, reason, evidence, false)
                .await
                .is_err());
        }
        assert!(store
            .retire_request(&parent.id, &"r".repeat(1025), "proof", false)
            .await
            .is_err());
        let before = store.requests.read().await.clone();
        let action_id = parent
            .interruption
            .as_ref()
            .unwrap()
            .action_request_id
            .as_ref()
            .unwrap();
        assert!(store
            .retire_request(action_id, "reason", "proof", false)
            .await
            .is_err());
        for variant in 0..4 {
            let mut changed = parent.clone();
            match variant {
                0 => changed.interruption = None,
                1 => changed.human_action = Some(HumanAction::Approval),
                2 => {
                    changed.kind = RequestKind::Notice;
                    changed.expects_reply = false;
                }
                _ => changed.wake_delivery = Some(super::super::WakeDeliveryState::Prepared),
            }
            store
                .requests
                .write()
                .await
                .insert(changed.id.clone(), changed);
            assert!(store
                .retire_request(&parent.id, "reason", "proof", false)
                .await
                .is_err());
        }
        store
            .requests
            .write()
            .await
            .insert(parent.id.clone(), parent.clone());
        assert_eq!(*store.requests.read().await, before);
    }

    #[tokio::test]
    async fn failed_retirement_rolls_back_parent_action_and_revision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("requests.sqlite3");
        let store = CollaborationStore::load(CollaborationOptions {
            path: Some(path.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
        let parent = interrupted(&store).await;
        let before = store.requests.read().await.clone();
        let changes = store.subscribe();
        let db = rusqlite::Connection::open(path).unwrap();
        db.execute_batch("CREATE TRIGGER reject_retire BEFORE INSERT ON collaboration_requests WHEN NEW.id LIKE 'action-%' BEGIN SELECT RAISE(ABORT,'injected failure after parent write'); END;").unwrap();
        assert!(store
            .retire_request(&parent.id, "withdrawn", "update:2", false)
            .await
            .is_err());
        assert_eq!(*store.requests.read().await, before);
        assert!(!changes.has_changed().unwrap());
        let durable: String = db
            .query_row(
                "SELECT status FROM collaboration_requests WHERE id=?",
                [&parent.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(durable, "claimed");
    }

    #[tokio::test]
    async fn answered_action_and_decision_are_not_rewritten_by_retirement() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let parent = interrupted(&store).await;
        let action_id = parent
            .interruption
            .as_ref()
            .unwrap()
            .action_request_id
            .as_ref()
            .unwrap();
        let action = store.requests.read().await[action_id].clone();
        let answered = store
            .reply(
                &action.to,
                action_id,
                RequestStatus::Completed,
                "wait and recheck".into(),
                vec![],
                vec![],
            )
            .await
            .unwrap();
        store
            .retire_request(&parent.id, "withdrawn", "update:2", false)
            .await
            .unwrap();
        assert_eq!(store.requests.read().await[action_id], answered);
    }

    #[tokio::test]
    async fn retirement_and_late_reports_start_a_new_history_retention_window() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let mut parent = interrupted(&store).await;
        parent.created_at -= time::Duration::days(30);
        store
            .requests
            .write()
            .await
            .insert(parent.id.clone(), parent.clone());
        let retired = store
            .retire_request(&parent.id, "withdrawn", "update:2", false)
            .await
            .unwrap();
        assert_eq!(
            super::super::request_latest_activity(&retired),
            retired.retirement.as_ref().unwrap().at
        );
        assert_eq!(
            store
                .prune_history(OffsetDateTime::now_utc() - time::Duration::days(1))
                .await
                .unwrap(),
            0
        );
        let late = store
            .reply(
                &parent.to,
                &parent.id,
                RequestStatus::Blocked,
                "later evidence".into(),
                vec![],
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(
            super::super::request_latest_activity(&late),
            late.late_replies[0].at
        );
    }

    #[tokio::test]
    async fn unrelated_actions_survive_retirement() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let parent = interrupted(&store).await;
        let action_id = parent
            .interruption
            .as_ref()
            .unwrap()
            .action_request_id
            .as_ref()
            .unwrap();
        store
            .requests
            .write()
            .await
            .get_mut(action_id)
            .unwrap()
            .parent_request_id = Some("another-parent".into());
        let unrelated = store.requests.read().await[action_id].clone();
        store
            .retire_request(&parent.id, "withdrawn", "coordinator:update", false)
            .await
            .unwrap();
        assert_eq!(store.requests.read().await[action_id], unrelated);
    }
}
