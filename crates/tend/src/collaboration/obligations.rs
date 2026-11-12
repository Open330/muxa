//! What a session owes and what it is waiting for, from the mailbox.
//!
//! A session that restarts, resumes or compacts its context forgets the
//! requests it claimed and the ones it sent. The provider's `SessionStart`
//! hook (Claude Code also fires it after compaction) asks for this summary
//! and hands it to the model as a few lines of context. Nothing is claimed,
//! marked or changed by reading it.
use super::{addresses, CollaborationRequest, CollaborationStore, Participant, RequestStatus};
use serde_json::{json, Value};

/// Ids listed per category; the rest are counted.
const MAX_LISTED: usize = 5;

fn entry(request: &CollaborationRequest, peer: &Participant) -> Value {
    json!({"request_id": request.id, "kind": request.kind, "peer": peer.label()})
}

fn bounded(mut items: Vec<(time::OffsetDateTime, Value)>) -> Value {
    items.sort_by_key(|item| item.0);
    let total = items.len();
    let listed: Vec<_> = items.into_iter().take(MAX_LISTED).map(|(_, v)| v).collect();
    json!({"total": total, "listed": listed})
}

impl CollaborationStore {
    /// The obligations of `caller`, or `None` when it has none.
    pub async fn obligations(&self, caller: &Participant) -> Option<Value> {
        if caller.console {
            return None;
        }
        let requests = self.requests.read().await;
        let (mut owed, mut queued, mut waiting, mut human, mut replies) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for request in requests.values() {
            let mine = request.from.same_session(caller);
            if addresses(&request.to, caller) {
                if request.status == RequestStatus::Claimed
                    && request.expects_reply
                    && request.reply.is_none()
                {
                    owed.push((
                        request.claimed_at.unwrap_or(request.created_at),
                        entry(request, &request.from),
                    ));
                } else if request.status == RequestStatus::Queued {
                    queued.push((request.created_at, entry(request, &request.from)));
                }
            } else if mine && request.to.console && request.needs_human_response() {
                human.push((request.created_at, json!({"request_id": request.id})));
            } else if mine && request.expects_reply && !request.status.is_terminal() {
                waiting.push((request.created_at, entry(request, &request.to)));
            } else if mine && request.reply.is_some() && request.reply_read_at.is_none() {
                let at = request.reply.as_ref().map_or(request.created_at, |r| r.at);
                replies.push((at, entry(request, &request.to)));
            }
        }
        if owed.is_empty()
            && queued.is_empty()
            && waiting.is_empty()
            && human.is_empty()
            && replies.is_empty()
        {
            return None;
        }
        Some(json!({
            "offer": "obligations",
            "owed_replies": bounded(owed),
            "queued_for_you": bounded(queued),
            "waiting_on": bounded(waiting),
            "waiting_on_person": bounded(human),
            "unread_replies": bounded(replies),
        }))
    }
}

#[cfg(test)]
mod tests {
    use crate::collaboration::{
        CollaborationOptions, CollaborationStore, HumanAction, NewRequest, Participant,
        RequestKind, RequestStatus,
    };

    fn participant(session: &str, pane: &str) -> Participant {
        let mut participant: Participant = serde_json::from_value(serde_json::json!({
            "agent_kind":"codex", "agent_session_id":session, "pane":pane,
            "socket":"default", "room":{"host":"tmux","socket":"default","window_id":"@1"},
            "state":"idle"
        }))
        .unwrap();
        participant.launch_purpose = crate::collaboration::LaunchPurpose::Worker;
        participant
    }

    async fn send(store: &CollaborationStore, from: &Participant, to: &Participant) -> String {
        store
            .create(
                from.clone(),
                to.clone(),
                NewRequest {
                    body: "x".into(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap()
            .id
    }

    #[tokio::test]
    async fn a_session_learns_what_it_owes_and_awaits_and_nothing_changes() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let me = participant("me", "%1");
        let peer = participant("peer", "%2");
        assert!(store.obligations(&me).await.is_none());
        let owed = send(&store, &peer, &me).await;
        store.claim_by_id(&me, &owed).await.unwrap();
        let queued = send(&store, &peer, &me).await;
        let waiting = send(&store, &me, &peer).await;
        let answered = send(&store, &me, &peer).await;
        store.claim_by_id(&peer, &answered).await.unwrap();
        store
            .reply(
                &peer,
                &answered,
                RequestStatus::Completed,
                "done".into(),
                vec![],
            )
            .await
            .unwrap();
        let human = store
            .create(
                me.clone(),
                Participant::console(me.room.clone()),
                NewRequest {
                    kind: RequestKind::Question,
                    body: "which?".into(),
                    human_action: Some(HumanAction::Choice),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap()
            .id;
        let summary = store.obligations(&me).await.unwrap();
        let ids = |key: &str| {
            summary[key]["listed"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["request_id"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids("owed_replies"), vec![owed.clone()]);
        assert_eq!(ids("queued_for_you"), vec![queued.clone()]);
        assert_eq!(ids("waiting_on"), vec![waiting]);
        assert_eq!(ids("waiting_on_person"), vec![human]);
        assert_eq!(ids("unread_replies"), vec![answered.clone()]);
        // Reading it consumed nothing.
        let row = store.requests.read().await[&queued].clone();
        assert_eq!(row.status, RequestStatus::Queued);
        assert!(store.requests.read().await[&answered]
            .reply_read_at
            .is_none());
    }

    #[tokio::test]
    async fn long_lists_are_bounded_with_a_total() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let me = participant("me", "%1");
        let peer = participant("peer", "%2");
        for _ in 0..8 {
            send(&store, &me, &peer).await;
        }
        let summary = store.obligations(&me).await.unwrap();
        assert_eq!(summary["waiting_on"]["total"], 8);
        assert_eq!(summary["waiting_on"]["listed"].as_array().unwrap().len(), 5);
    }
}
