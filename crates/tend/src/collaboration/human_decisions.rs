//! Human requests as decisions: the ones still waiting on a person and the
//! ones a person answered, for the decision queue and its history.
use super::{CollaborationError, CollaborationRequest, CollaborationStore};

impl CollaborationStore {
    /// Requests to the operator still owed an answer, oldest first, and
    /// whether more than `limit` exist.
    pub async fn open_human_requests(
        &self,
        limit: usize,
    ) -> Result<(Vec<CollaborationRequest>, bool), CollaborationError> {
        self.ensure_enabled()?;
        let requests = self.requests.read().await;
        let mut open: Vec<CollaborationRequest> = requests
            .values()
            .filter(|request| request.needs_human_response())
            .cloned()
            .collect();
        drop(requests);
        open.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        let more = open.len() > limit;
        open.truncate(limit);
        Ok((open, more))
    }

    /// Human-action requests someone answered, newest answer first, and
    /// whether more than `limit` exist.
    pub async fn answered_human_requests(
        &self,
        limit: usize,
    ) -> Result<(Vec<CollaborationRequest>, bool), CollaborationError> {
        self.ensure_enabled()?;
        let requests = self.requests.read().await;
        let mut answered: Vec<CollaborationRequest> = requests
            .values()
            .filter(|request| {
                request.to.console && request.human_action.is_some() && request.reply.is_some()
            })
            .cloned()
            .collect();
        drop(requests);
        let at = |request: &CollaborationRequest| request.reply.as_ref().map(|reply| reply.at);
        answered.sort_by(|a, b| at(b).cmp(&at(a)).then(b.id.cmp(&a.id)));
        let more = answered.len() > limit;
        answered.truncate(limit);
        Ok((answered, more))
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
        for body in ["first?", "second?", "third?"] {
            let request = store
                .create(
                    sender.clone(),
                    console.clone(),
                    NewRequest {
                        body: body.into(),
                        kind: RequestKind::Question,
                        human_action: Some(HumanAction::Choice),
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
        assert_eq!(answered[0].reply.as_ref().unwrap().body, "yes");
    }
}
