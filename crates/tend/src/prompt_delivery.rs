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

    async fn origin_of(store: &Store, session: &str) -> Option<crate::PromptOrigin> {
        store
            .by_pane("rmux:%7")
            .await
            .into_iter()
            .find(|agent| agent.session_id == session)
            .and_then(|agent| agent.last_prompt_origin)
    }

    async fn submit_at(store: &Store, session: &str, socket: &str, text: &str, at: OffsetDateTime) {
        store
            .apply(&AgentEvent::PromptSubmitted {
                id: id(session, socket),
                prompt: text.into(),
                at,
            })
            .await;
    }

    #[tokio::test]
    async fn a_registered_delivery_stamps_exactly_the_prompt_it_typed() {
        use crate::PromptOrigin;
        let store = Store::shared();
        start(&store).await;
        let mut prompts = store.subscribe_prompts();
        let token = store
            .expect_prompt(
                "rmux:%7",
                Some("/test.sock"),
                "ship it",
                PromptOrigin::PersonViaTend,
            )
            .await;
        assert!(token.is_some());
        submit(&store, "original", "/test.sock", "ship it").await;
        assert_eq!(
            origin_of(&store, "original").await,
            Some(PromptOrigin::PersonViaTend)
        );
        assert_eq!(
            prompts.recv().await.unwrap().origin,
            PromptOrigin::PersonViaTend
        );

        // The same text again, with no new delivery, is unknown.
        submit(&store, "original", "/test.sock", "ship it").await;
        assert_eq!(
            origin_of(&store, "original").await,
            Some(PromptOrigin::Unknown)
        );

        // Two identical deliveries each stamp one prompt, in order.
        let _ = store
            .expect_prompt(
                "rmux:%7",
                Some("/test.sock"),
                "continue",
                PromptOrigin::Peer,
            )
            .await;
        let _ = store
            .expect_prompt(
                "rmux:%7",
                Some("/test.sock"),
                "continue",
                PromptOrigin::Automation,
            )
            .await;
        submit(&store, "original", "/test.sock", "continue").await;
        assert_eq!(
            origin_of(&store, "original").await,
            Some(PromptOrigin::Peer)
        );
        submit(&store, "original", "/test.sock", "continue").await;
        assert_eq!(
            origin_of(&store, "original").await,
            Some(PromptOrigin::Automation)
        );

        // A pane with no tracked agent on that endpoint registers nothing.
        assert!(store
            .expect_prompt("rmux:%7", Some("/other.sock"), "hi", PromptOrigin::Peer)
            .await
            .is_none());

        // A forgotten delivery does not match.
        let token = store
            .expect_prompt("rmux:%7", Some("/test.sock"), "x", PromptOrigin::Peer)
            .await
            .unwrap();
        store.forget_expected_prompt(token);
        submit(&store, "original", "/test.sock", "x").await;
        assert_eq!(
            origin_of(&store, "original").await,
            Some(PromptOrigin::Unknown)
        );
    }

    #[tokio::test]
    async fn a_replacement_session_in_the_same_pane_never_takes_the_old_ones_delivery() {
        use crate::PromptOrigin;
        let store = Store::shared();
        start(&store).await;
        let _ = store
            .expect_prompt(
                "rmux:%7",
                Some("/test.sock"),
                "continue",
                PromptOrigin::Peer,
            )
            .await
            .unwrap();
        store
            .apply(&AgentEvent::Started {
                id: id("replacement", "/test.sock"),
                at: OffsetDateTime::now_utc(),
            })
            .await;
        submit(&store, "replacement", "/test.sock", "continue").await;
        assert_eq!(
            origin_of(&store, "replacement").await,
            Some(PromptOrigin::Unknown)
        );
        // With two sessions on the pane, a new delivery cannot be bound.
        assert!(store
            .expect_prompt("rmux:%7", Some("/test.sock"), "go", PromptOrigin::Peer)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn cancelling_one_delivery_leaves_another_endpoints_alone() {
        use crate::PromptOrigin;
        let store = Store::shared();
        start(&store).await;
        store
            .apply(&AgentEvent::Started {
                id: id("second", "/two.sock"),
                at: OffsetDateTime::now_utc(),
            })
            .await;
        let first = store
            .expect_prompt(
                "rmux:%7",
                Some("/test.sock"),
                "continue",
                PromptOrigin::Peer,
            )
            .await
            .unwrap();
        let _ = store
            .expect_prompt(
                "rmux:%7",
                Some("/two.sock"),
                "continue",
                PromptOrigin::PersonViaTend,
            )
            .await
            .unwrap();
        store.forget_expected_prompt(first);
        submit(&store, "second", "/two.sock", "continue").await;
        assert_eq!(
            origin_of(&store, "second").await,
            Some(PromptOrigin::PersonViaTend)
        );
        submit(&store, "original", "/test.sock", "continue").await;
        assert_eq!(
            origin_of(&store, "original").await,
            Some(PromptOrigin::Unknown)
        );
    }

    #[tokio::test]
    async fn a_hook_stamped_before_the_delivery_is_an_older_prompt() {
        use crate::PromptOrigin;
        let store = Store::shared();
        start(&store).await;
        // After session start, but before the delivery was registered.
        let earlier = OffsetDateTime::now_utc();
        let _ = store
            .expect_prompt(
                "rmux:%7",
                Some("/test.sock"),
                "continue",
                PromptOrigin::Peer,
            )
            .await
            .unwrap();
        submit_at(&store, "original", "/test.sock", "continue", earlier).await;
        assert_eq!(
            origin_of(&store, "original").await,
            Some(PromptOrigin::Unknown)
        );
        // The delivery is still there for the prompt it typed.
        submit(&store, "original", "/test.sock", "continue").await;
        assert_eq!(
            origin_of(&store, "original").await,
            Some(PromptOrigin::Peer)
        );
    }

    #[tokio::test]
    async fn the_same_session_reporting_from_elsewhere_does_not_take_the_delivery() {
        use crate::PromptOrigin;
        let store = Store::shared();
        start(&store).await;
        let _ = store
            .expect_prompt(
                "rmux:%7",
                Some("/test.sock"),
                "continue",
                PromptOrigin::Peer,
            )
            .await
            .unwrap();
        // The session resumed on another server: not the prompt tend typed.
        submit(&store, "original", "/other.sock", "continue").await;
        let elsewhere = store
            .by_pane("rmux:%7")
            .await
            .into_iter()
            .find(|agent| agent.tmux_socket.as_deref() == Some("/other.sock"))
            .and_then(|agent| agent.last_prompt_origin);
        assert_ne!(elsewhere, Some(PromptOrigin::Peer));
        // Back where tend typed it, the delivery is still there.
        submit(&store, "original", "/test.sock", "continue").await;
        assert_eq!(
            origin_of(&store, "original").await,
            Some(PromptOrigin::Peer)
        );
    }

    #[test]
    fn callers_may_declare_only_a_person_peer_or_automation() {
        use crate::PromptOrigin;
        assert_eq!(PromptOrigin::declared(None), PromptOrigin::Tend);
        assert_eq!(
            PromptOrigin::declared(Some(PromptOrigin::Unknown)),
            PromptOrigin::Tend
        );
        assert_eq!(
            PromptOrigin::declared(Some(PromptOrigin::PersonViaTend)),
            PromptOrigin::PersonViaTend
        );
    }
}
