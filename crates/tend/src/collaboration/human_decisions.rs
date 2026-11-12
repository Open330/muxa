//! Human requests as decisions: the ones still waiting on a person and the
//! ones a person answered, for the decision queue and its history. Only
//! the selected page is cloned out of the request map.
use super::{CollaborationError, CollaborationRequest, CollaborationStore, RequestKind};

/// A console request that asked for an answer and got one, whatever the
/// operator replied (answered, declined, failed, blocked). Cancelled or
/// retired requests carry no reply and are not decisions.
fn answered_by_person(request: &CollaborationRequest) -> bool {
    request.to.console
        && request.expects_reply
        && request.kind != RequestKind::Notice
        && request.reply.is_some()
}

impl CollaborationStore {
    /// Requests to the operator still owed an answer, oldest first, and
    /// whether more than `limit` exist.
    pub async fn open_human_requests(
        &self,
        limit: usize,
    ) -> Result<(Vec<CollaborationRequest>, bool), CollaborationError> {
        self.ensure_enabled()?;
        let requests = self.requests.read().await;
        let mut open: Vec<&CollaborationRequest> = requests
            .values()
            .filter(|request| request.needs_human_response())
            .collect();
        open.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        let more = open.len() > limit;
        Ok((open.into_iter().take(limit).cloned().collect(), more))
    }

    /// Console requests someone answered, newest answer first, and whether
    /// more than `limit` exist.
    pub async fn answered_human_requests(
        &self,
        limit: usize,
    ) -> Result<(Vec<CollaborationRequest>, bool), CollaborationError> {
        self.ensure_enabled()?;
        let requests = self.requests.read().await;
        let mut answered: Vec<&CollaborationRequest> = requests
            .values()
            .filter(|request| answered_by_person(request))
            .collect();
        let at = |request: &CollaborationRequest| request.reply.as_ref().map(|reply| reply.at);
        answered.sort_by(|a, b| at(b).cmp(&at(a)).then(b.id.cmp(&a.id)));
        let more = answered.len() > limit;
        Ok((answered.into_iter().take(limit).cloned().collect(), more))
    }
}

#[cfg(test)]
mod tests {
    use crate::collaboration::{
        CollaborationOptions, CollaborationStore, HumanAction, NewRequest, Participant,
        RequestKind, RequestStatus,
    };

    #[tokio::test]
    async fn open_and_answered_human_requests_are_separate_and_ordered() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let sender: Participant = serde_json::from_value(serde_json::json!({
            "agent_kind":"codex", "agent_session_id":"a", "pane":"%1",
            "room":{"host":"tmux","socket":"default","window_id":"@1"},
            "state":"idle", "launch_purpose":"worker"
        }))
        .unwrap();
        let console = Participant::console(sender.room.clone());
        let mut ids = Vec::new();
        for (body, action) in [
            ("first?", Some(HumanAction::Choice)),
            ("second?", None),
            ("third?", Some(HumanAction::Approval)),
        ] {
            let request = store
                .create(
                    sender.clone(),
                    console.clone(),
                    NewRequest {
                        body: body.into(),
                        kind: RequestKind::Question,
                        human_action: action,
                        ..NewRequest::default()
                    },
                )
                .await
                .unwrap();
            ids.push(request.id);
        }
        store
            .reply(
                &console,
                &ids[1],
                RequestStatus::Completed,
                "yes".into(),
                vec![],
            )
            .await
            .unwrap();
        let (open, more) = store.open_human_requests(1).await.unwrap();
        assert_eq!(open.len(), 1);
        assert!(more);
        assert_eq!(open[0].id, ids[0], "oldest first");
        let (open, _) = store.open_human_requests(10).await.unwrap();
        assert_eq!(
            open.iter().map(|r| &r.id).collect::<Vec<_>>(),
            [&ids[0], &ids[2]]
        );
        let (answered, more) = store.answered_human_requests(10).await.unwrap();
        assert!(!more);
        assert_eq!(answered.len(), 1);
        assert_eq!(
            answered[0].reply.as_ref().unwrap().body,
            "yes",
            "a plain console question without human_action is a decision too"
        );
        store
            .reply(
                &console,
                &ids[2],
                RequestStatus::Declined,
                "no".into(),
                vec![],
            )
            .await
            .unwrap();
        let (answered, _) = store.answered_human_requests(10).await.unwrap();
        assert_eq!(answered.len(), 2, "a declined answer is kept");
        assert_eq!(answered[0].id, ids[2], "newest answer first");
    }
}
