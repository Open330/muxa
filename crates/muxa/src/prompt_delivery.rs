//! Evidence of submission is distinct from successful keystroke injection.
use crate::{Agent, PromptRecord, Store};
use std::time::Duration;
use tokio::sync::broadcast;

pub struct SubmissionObserver {
    receiver: broadcast::Receiver<PromptRecord>,
    session: Option<String>,
    socket: Option<String>,
    text: String,
    since: time::OffsetDateTime,
}

impl SubmissionObserver {
    /// Subscribe before injecting, so fast application hooks cannot be lost.
    /// Ambiguous sessions and empty Enter-only operations have no proof target.
    pub fn new(store: &Store, agents: &[Agent], socket: Option<&str>, text: &str) -> Self {
        let candidates: Vec<_> = agents
            .iter()
            .filter(|a| a.tmux_socket.as_deref() == socket)
            .collect();
        Self {
            receiver: store.subscribe_prompts(),
            session: (candidates.len() == 1 && !text.is_empty())
                .then(|| candidates[0].session_id.clone()),
            socket: socket.map(str::to_owned),
            text: text.to_owned(),
            since: time::OffsetDateTime::now_utc(),
        }
    }

    fn matches(&self, record: &PromptRecord) -> bool {
        self.session.as_deref() == Some(record.id.session_id.as_str())
            && self.socket == record.id.tmux_socket
            && self.text == record.prompt
            && record.at >= self.since
    }

    /// Missing evidence is unknown, not failed submission. Never inject more
    /// keys based on a timeout: hook delivery can be slow or unavailable.
    pub async fn confirm(mut self) -> Option<bool> {
        self.session.as_ref()?;
        tokio::time::timeout(Duration::from_millis(750), async {
            loop {
                match self.receiver.recv().await {
                    Ok(record) if self.matches(&record) => return Some(true),
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        })
        .await
        .ok()
        .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentEvent, AgentId, AgentKind};
    use time::OffsetDateTime;

    fn id(session: &str, socket: &str) -> AgentId {
        AgentId {
            kind: AgentKind::Codex,
            session_id: session.into(),
            surface: None,
            pane: Some("rmux:%7".into()),
            tmux_socket: Some(socket.into()),
            cwd: None,
        }
    }

    async fn start(store: &Store) -> Vec<Agent> {
        store
            .apply(&AgentEvent::Started {
                id: id("original", "/test.sock"),
                at: OffsetDateTime::now_utc(),
            })
            .await;
        store.by_pane("rmux:%7").await
    }

    async fn submit(store: &Store, session: &str, socket: &str, text: &str) {
        store
            .apply(&AgentEvent::PromptSubmitted {
                id: id(session, socket),
                prompt: text.into(),
                at: OffsetDateTime::now_utc(),
            })
            .await;
    }

    #[tokio::test]
    async fn fast_submission_is_not_lost_before_wait_starts() {
        let store = Store::shared();
        let agents = start(&store).await;
        let observer = SubmissionObserver::new(&store, &agents, Some("/test.sock"), "continue");
        submit(&store, "original", "/test.sock", "continue").await;
        assert_eq!(observer.confirm().await, Some(true));
    }

    #[tokio::test]
    async fn historical_prompt_and_reused_pane_do_not_confirm_delivery() {
        let store = Store::shared();
        let agents = start(&store).await;
        submit(&store, "original", "/test.sock", "continue").await;
        let observer = SubmissionObserver::new(&store, &agents, Some("/test.sock"), "continue");
        submit(&store, "replacement", "/test.sock", "continue").await;
        submit(&store, "original", "/other.sock", "continue").await;
        submit(&store, "original", "/test.sock", "user's different prompt").await;
        assert_eq!(observer.confirm().await, None);
    }

    #[tokio::test]
    async fn missing_hook_and_empty_enter_are_unknown() {
        let store = Store::shared();
        let agents = start(&store).await;
        let observer = SubmissionObserver::new(&store, &agents, Some("/test.sock"), "continue");
        assert_eq!(observer.confirm().await, None);
        let observer = SubmissionObserver::new(&store, &agents, Some("/test.sock"), "");
        submit(&store, "original", "/test.sock", "some pending draft").await;
        assert_eq!(observer.confirm().await, None);
    }

    #[test]
    fn native_prompt_boundaries_do_not_need_a_timer() {
        assert!(crate::backend::prompt_submit_grace("rmux:%7").is_zero());
        assert!(crate::backend::prompt_submit_grace("%7").is_zero());
        assert!(!crate::backend::prompt_submit_grace("cmux:p7").is_zero());
    }
}
