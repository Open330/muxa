//! Conditional Fleet mailbox reads; tokens are invalidation hints, never authority.
use super::{
    CollaborationError, CollaborationQuery, CollaborationRequest, CollaborationStore, MailboxScope,
    Participant, RequestMailbox,
};
use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};
use std::sync::atomic::Ordering;

pub const MAILBOX_SNAPSHOT_CAPABILITY: &str = "collaboration_mailbox_snapshot_v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxSnapshot {
    pub token: Option<String>,
    pub unchanged: bool,
    pub incoming: Vec<CollaborationRequest>,
    pub sent: Vec<CollaborationRequest>,
}

impl CollaborationStore {
    /// Origins are freshly resolved by IPC before this call. Never cache that
    /// authorization. All returned pages and the token share one transaction.
    pub async fn mailbox_snapshot(
        &self,
        console: &Participant,
        agent: Option<&Participant>,
        if_unchanged: Option<&str>,
    ) -> Result<MailboxSnapshot, CollaborationError> {
        self.ensure_enabled()?;
        if !console.console {
            return Err(CollaborationError::ScopeDenied);
        }
        let _transaction = self.transaction_lock.lock().await;
        let mut fingerprint = std::collections::hash_map::DefaultHasher::new();
        // Metadata only; includes resolved session/room/socket/role identity.
        serde_json::to_vec(&(console, agent))?.hash(&mut fingerprint);
        let token = format!(
            "{}:{:x}:{:x}:{:x}",
            self.read_epoch,
            self.read_revision.load(Ordering::SeqCst),
            *self.changes.borrow(),
            fingerprint.finish()
        );
        if if_unchanged == Some(token.as_str()) {
            return Ok(MailboxSnapshot {
                token: Some(token),
                unchanged: true,
                incoming: Vec::new(),
                sent: Vec::new(),
            });
        }
        let query = CollaborationQuery {
            limit: Some(crate::fleet::FLEET_MAILBOX_REQUEST_LIMIT),
            ..Default::default()
        };
        let mut incoming = if let Some(agent) = agent {
            self.query_for_offset_locked(
                agent,
                RequestMailbox::Incoming,
                MailboxScope::Caller,
                &query,
                0,
            )
            .await?
            .requests
        } else {
            Vec::new()
        };
        incoming.extend(
            self.query_for_offset_locked(
                console,
                RequestMailbox::Incoming,
                MailboxScope::Caller,
                &query,
                0,
            )
            .await?
            .requests,
        );
        let mut sent = self
            .query_for_offset_locked(
                console,
                RequestMailbox::Sent,
                MailboxScope::Caller,
                &query,
                0,
            )
            .await?
            .requests;
        let newest_first = |a: &CollaborationRequest, b: &CollaborationRequest| {
            (b.created_at, &b.id).cmp(&(a.created_at, &a.id))
        };
        incoming.sort_by(newest_first);
        sent.sort_by(newest_first);
        incoming.truncate(crate::fleet::FLEET_MAILBOX_REQUEST_LIMIT);
        sent.truncate(crate::fleet::FLEET_MAILBOX_REQUEST_LIMIT);
        Ok(MailboxSnapshot {
            token: Some(token),
            unchanged: false,
            incoming,
            sent,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collaboration::{CollaborationOptions, HumanAction, NewRequest, RequestStatus};

    fn participant(id: &str) -> Participant {
        serde_json::from_value(serde_json::json!({"agent_kind":"codex","agent_session_id":id,
            "pane":"%1","room":{"host":"tmux","socket":"default","window_id":"@1"},"state":"idle","launch_purpose":"worker"})).unwrap()
    }

    #[tokio::test]
    async fn snapshot_versions_cover_updates_replies_quiet_reservations_and_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let store = CollaborationStore::load(CollaborationOptions {
            path: Some(dir.path().join("requests.sqlite3")),
            ..Default::default()
        })
        .await
        .unwrap();
        let sender = participant("sender");
        let console = Participant::console(sender.room.clone());
        let request = store
            .create(
                sender.clone(),
                console.clone(),
                NewRequest {
                    body: "decision please".into(),
                    human_action: Some(HumanAction::Choice),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let first = store.mailbox_snapshot(&console, None, None).await.unwrap();
        assert_eq!(first.incoming[0].body, request.body);
        let hit = store
            .mailbox_snapshot(&console, None, first.token.as_deref())
            .await
            .unwrap();
        assert!(hit.unchanged && hit.incoming.is_empty() && hit.sent.is_empty());
        store
            .update_request(&sender, &request.id, "quiet progress".into())
            .await
            .unwrap();
        let updated = store
            .mailbox_snapshot(&console, None, first.token.as_deref())
            .await
            .unwrap();
        assert!(!updated.unchanged);
        assert_eq!(updated.incoming[0].updates[0].body, "quiet progress");
        store
            .reply(
                &console,
                &request.id,
                RequestStatus::Completed,
                "wait, not approval".into(),
                vec![],
                vec![],
            )
            .await
            .unwrap();
        let replied = store
            .mailbox_snapshot(&console, None, updated.token.as_deref())
            .await
            .unwrap();
        assert!(!replied.unchanged);
        assert!(replied.incoming[0].reply.is_some());
        let notifications = store.subscribe();
        let batch = store.reserve_reply_wakes(&sender).await.unwrap().unwrap();
        assert!(
            !notifications.has_changed().unwrap(),
            "reservation must stay quiet"
        );
        let reserved = store
            .mailbox_snapshot(&console, None, replied.token.as_deref())
            .await
            .unwrap();
        assert!(
            !reserved.unchanged,
            "quiet persisted markers must invalidate Full views"
        );
        assert!(reserved.incoming[0].reply_wake.is_some());
        store.complete_reply_wakes(&batch, false).await.unwrap();
        let settled = store
            .mailbox_snapshot(&console, None, reserved.token.as_deref())
            .await
            .unwrap();
        assert!(!settled.unchanged);
        store
            .delete_requests(std::slice::from_ref(&request.id))
            .unwrap();
        let deleted = store
            .mailbox_snapshot(&console, None, settled.token.as_deref())
            .await
            .unwrap();
        assert!(!deleted.unchanged && deleted.incoming.is_empty());
    }

    #[tokio::test]
    async fn snapshot_hint_cannot_cross_identities_or_survive_store_restart() {
        let dir = tempfile::tempdir().unwrap();
        let opts = CollaborationOptions {
            path: Some(dir.path().join("requests.sqlite3")),
            ..Default::default()
        };
        let store = CollaborationStore::load(opts.clone()).await.unwrap();
        let agent = participant("agent");
        let console = Participant::console(agent.room.clone());
        let first = store
            .mailbox_snapshot(&console, Some(&agent), None)
            .await
            .unwrap();
        let other = participant("other");
        assert!(
            !store
                .mailbox_snapshot(&console, Some(&other), first.token.as_deref())
                .await
                .unwrap()
                .unchanged
        );
        let mut moved = agent.clone();
        moved.room.window_id = "@2".into();
        moved.pane = "%2".into();
        assert!(
            !store
                .mailbox_snapshot(&console, Some(&moved), first.token.as_deref())
                .await
                .unwrap()
                .unchanged
        );
        assert!(store
            .mailbox_snapshot(&agent, None, first.token.as_deref())
            .await
            .is_err());
        drop(store);
        let restarted = CollaborationStore::load(opts).await.unwrap();
        assert!(
            !restarted
                .mailbox_snapshot(&console, Some(&agent), first.token.as_deref())
                .await
                .unwrap()
                .unchanged
        );
    }

    #[tokio::test]
    async fn snapshot_bounds_both_mailboxes_after_merging_and_preserves_full_contracts() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let agent = participant("agent");
        let console = Participant::console(agent.room.clone());
        for n in 0..40 {
            store
                .create(
                    console.clone(),
                    agent.clone(),
                    NewRequest {
                        body: format!("original instructions {n}"),
                        paths: vec!["allowed/file".into()],
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
        }
        let snapshot = store
            .mailbox_snapshot(&console, Some(&agent), None)
            .await
            .unwrap();
        assert_eq!(
            snapshot.incoming.len(),
            crate::fleet::FLEET_MAILBOX_REQUEST_LIMIT
        );
        assert_eq!(
            snapshot.sent.len(),
            crate::fleet::FLEET_MAILBOX_REQUEST_LIMIT
        );
        assert_eq!(snapshot.sent[0].body, "original instructions 39");
        assert_eq!(snapshot.sent[0].paths, ["allowed/file"]);
    }
}
