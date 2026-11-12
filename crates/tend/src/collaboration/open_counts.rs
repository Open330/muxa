//! Open-request counts for Work and agent summaries, without transporting rows.
use super::{
    request_visible, CollaborationError, CollaborationRequest, CollaborationStore, MailboxScope,
    Participant, RequestMailbox,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

pub const OPEN_COUNTS_CAPABILITY: &str = "collaboration_open_counts_v1";
pub const HUMAN_QUERY_CAPABILITY: &str = "collaboration_human_query_v1";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
/// Keys retain synthetic sender IDs (for example recovery coordinators), just as
/// listings do; Work summaries select their own real agent sessions.
pub struct CollaborationOpenCounts {
    pub incoming: BTreeMap<String, usize>,
    pub outgoing: BTreeMap<String, usize>,
}

impl CollaborationOpenCounts {
    pub fn from_requests<'a>(requests: impl IntoIterator<Item = &'a CollaborationRequest>) -> Self {
        let mut counts = Self::default();
        let mut seen = HashSet::new();
        for request in requests {
            if request.status.is_terminal() || !seen.insert(&request.id) {
                continue;
            }
            if !request.to.console {
                *counts
                    .incoming
                    .entry(request.to.agent_session_id.clone())
                    .or_default() += 1;
            }
            if !request.from.console {
                *counts
                    .outgoing
                    .entry(request.from.agent_session_id.clone())
                    .or_default() += 1;
            }
        }
        counts
    }
}

impl CollaborationStore {
    pub async fn open_counts_for(
        &self,
        caller: &Participant,
        scope: MailboxScope,
    ) -> Result<CollaborationOpenCounts, CollaborationError> {
        self.ensure_enabled()?;
        if scope != MailboxScope::Caller && !caller.console {
            return Err(CollaborationError::ScopeDenied);
        }
        // One map read is a coherent snapshot; do not queue it behind database writes.
        let requests = self.requests.read().await;
        Ok(CollaborationOpenCounts::from_requests(
            requests
                .values()
                .filter(|request| request_visible(request, caller, RequestMailbox::All, scope)),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collaboration::{
        CollaborationOptions, CollaborationQuery, HumanAction, NewRequest, RequestKind,
        RequestStatus,
    };

    fn agent(id: &str, window: &str) -> Participant {
        serde_json::from_value(serde_json::json!({
            "agent_kind":"codex", "agent_session_id":id, "pane":id,
            "room":{"host":"tmux","socket":"default","window_id":window},
            "state":"idle", "launch_purpose":"worker"
        }))
        .unwrap()
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // same fixture verifies storage parity, scopes and pagination
    async fn aggregates_and_human_filters_match_authorized_history_in_memory_and_sqlite() {
        for durable in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let store = if durable {
                CollaborationStore::load(CollaborationOptions {
                    path: Some(dir.path().join("requests.sqlite3")),
                    ..Default::default()
                })
                .await
                .unwrap()
            } else {
                CollaborationStore::in_memory(CollaborationOptions::default())
            };
            let sender = agent("sender", "@1");
            let recipient = agent("recipient", "@1");
            let console = Participant::console(sender.room.clone());
            store
                .create(
                    sender.clone(),
                    recipient,
                    NewRequest {
                        body: "large body ".repeat(1000),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            for kind in [
                RequestKind::Question,
                RequestKind::Notice,
                RequestKind::Question,
            ] {
                store
                    .create(
                        sender.clone(),
                        console.clone(),
                        NewRequest {
                            body: "human or notice".into(),
                            kind,
                            expects_reply: kind != RequestKind::Notice,
                            human_action: (kind != RequestKind::Notice)
                                .then_some(HumanAction::Choice),
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap();
            }
            let ended = store
                .create(
                    sender.clone(),
                    console.clone(),
                    NewRequest {
                        body: "ended question".into(),
                        kind: RequestKind::Question,
                        human_action: Some(HumanAction::Choice),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            store
                .reply(
                    &console,
                    &ended.id,
                    RequestStatus::Completed,
                    "done".into(),
                    vec![],
                )
                .await
                .unwrap();
            store
                .create(
                    console.clone(),
                    sender.clone(),
                    NewRequest {
                        body: "operator task".into(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            store
                .create(
                    agent("other-sender", "@2"),
                    agent("other-recipient", "@2"),
                    NewRequest {
                        body: "other room".into(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            for scope in [MailboxScope::Caller, MailboxScope::Room, MailboxScope::All] {
                let page = store
                    .query_for(
                        &console,
                        RequestMailbox::All,
                        scope,
                        &CollaborationQuery {
                            limit: Some(500),
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap();
                let expected = CollaborationOpenCounts::from_requests(&page.requests);
                let counts = store.open_counts_for(&console, scope).await.unwrap();
                assert_eq!(counts, expected, "durable={durable} scope={scope:?}");
                assert!(!counts.incoming.contains_key(&console.agent_session_id));
                assert_eq!(counts.incoming.get("sender"), Some(&1));
                assert_eq!(
                    counts.outgoing.get("sender"),
                    Some(&if scope == MailboxScope::Caller { 3 } else { 4 })
                );
            }
            assert!(matches!(
                store.open_counts_for(&sender, MailboxScope::All).await,
                Err(CollaborationError::ScopeDenied)
            ));
            assert!(matches!(
                store.open_counts_for(&sender, MailboxScope::Room).await,
                Err(CollaborationError::ScopeDenied)
            ));
            assert!(store
                .open_counts_for(&sender, MailboxScope::Caller)
                .await
                .is_ok());

            let mut query = CollaborationQuery {
                human_only: true,
                limit: Some(1),
                ..Default::default()
            };
            let first = store
                .query_for(&console, RequestMailbox::All, MailboxScope::All, &query)
                .await
                .unwrap();
            assert_eq!(
                first.total, 2,
                "filtering happens before total and pagination"
            );
            assert!(first.has_more && first.requests[0].needs_human_response());
            query.cursor = first.next_cursor;
            let second = store
                .query_for(&console, RequestMailbox::All, MailboxScope::All, &query)
                .await
                .unwrap();
            assert_eq!(second.total, 2);
            assert!(!second.has_more && second.requests[0].needs_human_response());
            assert_ne!(first.requests[0].id, second.requests[0].id);
            let mut recovery = sender.clone();
            recovery.agent_session_id = "tend:peer-recovery".into();
            store
                .create(
                    recovery,
                    sender.clone(),
                    NewRequest {
                        body: "recovery".into(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let _transaction = store.transaction_lock.lock().await;
            let counts = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                store.open_counts_for(&console, MailboxScope::All),
            )
            .await
            .expect("a map-only count must not wait behind an unrelated durable transaction")
            .unwrap();
            assert_eq!(counts.outgoing.get("tend:peer-recovery"), Some(&1));
        }
    }
}
