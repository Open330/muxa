//! Prompt delivery into agent panes, and evidence of submission, which is
//! distinct from successful keystroke injection.
use crate::collaboration::CollaborationStore;
use crate::{Agent, PromptOrigin, PromptRecord, SharedBackend, Store};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, OwnedMutexGuard};

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

/// The exclusive right to type into one pane: the collaboration store's
/// per-pane worker action gate. Shared so a caller can hold it across several
/// injections (a wake body, then its Enter) and across endpoint checks.
pub type PaneInputGuard = Arc<OwnedMutexGuard<()>>;

/// Refusal text when a worker drain has paused the pane's input. Every
/// prompt path reports it the same way.
/// Advertised by a daemon whose `send_prompt` honours `force`
/// ([`idle_gate`]).
pub const SEND_PROMPT_STATE_GATE_CAPABILITY: &str = "send_prompt_state_gate_v1";
/// Refusal prefix of [`idle_gate`]. Nothing was typed.
pub const TARGET_NOT_IDLE: &str = "target_not_idle";

/// Refuse a gated `send_prompt` unless every live agent row on the target
/// pane (narrowed to `socket` and `session_id` when given) is Idle.
///
/// A working agent would get the text queued into, or merged with, its
/// current turn; a permission prompt or choice menu would take it as an
/// answer. Typing is only safe into an Idle composer, so anything else, an
/// untracked pane included, is refused unless the caller forces it. The
/// state is the hook-driven registry state, not the screen.
pub fn idle_gate(
    pane: &str,
    agents: &[Agent],
    socket: Option<&str>,
    session_id: Option<&str>,
) -> Result<(), String> {
    let live: Vec<_> = agents
        .iter()
        .filter(|agent| agent.state != crate::AgentState::Stopped)
        // The same endpoint matching as the send's authority resolver, so
        // `default` and its full `-S` path name one server.
        .filter(|agent| {
            socket.is_none_or(|socket| {
                agent.tmux_socket.as_deref().is_some_and(|recorded| {
                    crate::backend::pane_endpoints_match(Some(pane), recorded, socket)
                })
            })
        })
        .filter(|agent| session_id.is_none_or(|session| agent.session_id == session))
        .collect();
    if live.is_empty() {
        return Err(format!(
            "{TARGET_NOT_IDLE}: no tracked agent on pane {pane}, so its state is unknown; \
             nothing was sent. Pass force=true to type into it anyway."
        ));
    }
    if let Some(busy) = live
        .iter()
        .find(|agent| agent.state != crate::AgentState::Idle)
    {
        return Err(format!(
            "{TARGET_NOT_IDLE}: the agent on pane {pane} is {}; nothing was sent. Wait for it to \
             be idle (tend_wait with pane), use the mailbox (tend_call_peer) for work it \
             should pick up, or pass force=true to type into it anyway.",
            busy.state
        ));
    }
    Ok(())
}

pub const WORKER_DRAINING: &str = "worker_draining: managed terminal input is paused";

/// Take the pane's input guard. Waiting here is what keeps a second prompt
/// out of an input line another delivery is still submitting.
pub async fn lock_pane(collaboration: &CollaborationStore, pane: &str) -> PaneInputGuard {
    Arc::new(collaboration.worker_action_guard(pane).await)
}

/// One prompt to type into one pane.
#[derive(Debug, Clone, Copy)]
pub struct PromptDelivery<'a> {
    pub pane: &'a str,
    pub socket: Option<&'a str>,
    pub text: &'a str,
    /// Press Enter after the text.
    pub submit: bool,
    /// `Some` registers the prompt before typing, so the hook reporting it
    /// is stamped with this origin. `None` registers nothing and leaves the
    /// hook's own attribution.
    pub origin: Option<PromptOrigin>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryRefused {
    /// A worker drain paused this endpoint's input; nothing was typed.
    WorkerDraining,
    /// The backend did not take the text; nothing landed, so a caller may
    /// retry the whole delivery.
    NotSent,
}

/// The text was typed. `submitted` says whether Enter was too.
pub struct Delivered {
    pub submitted: bool,
    observer: SubmissionObserver,
}

impl Delivered {
    /// Application evidence that the submitted prompt arrived; `None` when
    /// unknown or not submitted.
    pub async fn confirm(self) -> Option<bool> {
        if self.submitted {
            self.observer.confirm().await
        } else {
            None
        }
    }
}

/// Take the pane's guard, then deliver. See [`deliver_prompt_holding`].
pub async fn deliver_prompt(
    collaboration: &CollaborationStore,
    store: &Store,
    backend: SharedBackend,
    delivery: PromptDelivery<'_>,
) -> Result<Delivered, DeliveryRefused> {
    let guard = lock_pane(collaboration, delivery.pane).await;
    let agents = store.by_pane(delivery.pane).await;
    deliver_prompt_holding(guard, collaboration, store, &agents, backend, delivery).await
}

/// The one prompt delivery path. With the pane's guard held for the whole
/// sequence: refuse a draining worker, register the expected prompt, type the
/// text, wait the host's submit grace, press Enter, and forget the
/// registration if Enter did not land. Text and Enter are two non-atomic
/// injections; holding the guard across both is what stops another delivery
/// from typing into the same line and being submitted with this one.
///
/// `agents` are the pane's rows, read after the guard was taken.
pub async fn deliver_prompt_holding(
    guard: PaneInputGuard,
    collaboration: &CollaborationStore,
    store: &Store,
    agents: &[Agent],
    backend: SharedBackend,
    delivery: PromptDelivery<'_>,
) -> Result<Delivered, DeliveryRefused> {
    let PromptDelivery {
        pane,
        socket,
        text,
        submit,
        origin,
    } = delivery;
    if collaboration.worker_input_blocked(pane, socket).await {
        return Err(DeliveryRefused::WorkerDraining);
    }
    let observer = SubmissionObserver::new(store, agents, socket, text);
    let expected = match origin {
        Some(origin) if submit && !text.is_empty() => {
            store.expect_prompt(pane, socket, text, origin).await
        }
        _ => None,
    };
    let (pane, socket, text) = (pane.to_owned(), socket.map(str::to_owned), text.to_owned());
    // `send_text_on` is a blocking shell-out / socket call.
    let (sent, submitted) = tokio::task::spawn_blocking(move || {
        let _guard = guard;
        let socket = socket.as_deref();
        let sent = backend.send_prompt_text_on(socket, &pane, &text);
        let submitted = if sent && submit {
            if !text.is_empty() {
                std::thread::sleep(crate::backend::prompt_submit_grace(&pane));
            }
            backend.send_text_on(socket, &pane, "\r")
        } else {
            false
        };
        (sent, submitted)
    })
    .await
    .unwrap_or((false, false));
    if !submitted {
        if let Some(token) = expected {
            store.forget_expected_prompt(token);
        }
    }
    if sent {
        Ok(Delivered {
            submitted,
            observer,
        })
    } else {
        Err(DeliveryRefused::NotSent)
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

    /// The gate judges only rows on the endpoint being typed into: a live row
    /// elsewhere neither vouches for nor blocks it.
    #[tokio::test]
    async fn the_idle_gate_is_scoped_to_the_selected_endpoint() {
        let store = Store::shared();
        let mut agents = start(&store).await;
        let mut other = agents[0].clone();
        other.session_id = "other".into();
        other.tmux_socket = Some("/other.sock".into());
        agents[0].state = crate::AgentState::Idle;
        other.state = crate::AgentState::Stopped;
        let rows = [agents[0].clone(), other.clone()];
        // Idle on A, only a stopped row on B: typing into B is untracked.
        assert!(idle_gate("rmux:%7", &rows, Some("/other.sock"), None).is_err());
        assert!(idle_gate("rmux:%7", &rows, Some("/test.sock"), None).is_ok());
        // Busy on A does not refuse an idle B.
        agents[0].state = crate::AgentState::Working;
        other.state = crate::AgentState::Idle;
        let rows = [agents[0].clone(), other];
        assert!(idle_gate("rmux:%7", &rows, Some("/other.sock"), None).is_ok());
        let refused = idle_gate("rmux:%7", &rows, Some("/test.sock"), None).unwrap_err();
        assert!(refused.starts_with(TARGET_NOT_IDLE), "{refused}");
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

    /// A registration is consumed only by the hook for its own prompt;
    /// another registration's hook, or a forget, does not consume it.
    #[tokio::test]
    async fn a_registration_is_pending_until_its_own_hook() {
        use crate::PromptOrigin;
        let store = Store::shared();
        start(&store).await;
        let earlier = store
            .expect_prompt("rmux:%7", Some("/test.sock"), "earlier", PromptOrigin::Tend)
            .await
            .unwrap();
        let ours = store
            .expect_prompt(
                "rmux:%7",
                Some("/test.sock"),
                "continue",
                PromptOrigin::Tend,
            )
            .await
            .unwrap();
        assert!(!store.expected_prompt_consumed(earlier) && !store.expected_prompt_consumed(ours));
        submit(&store, "original", "/test.sock", "earlier").await;
        assert!(store.expected_prompt_consumed(earlier));
        assert!(!store.expected_prompt_consumed(ours));
        submit(&store, "original", "/test.sock", "continue").await;
        assert!(store.expected_prompt_consumed(ours));
        // Forgotten (or expired) is not consumed.
        let dropped = store
            .expect_prompt("rmux:%7", Some("/test.sock"), "never", PromptOrigin::Tend)
            .await
            .unwrap();
        store.forget_expected_prompt(dropped);
        assert!(!store.expected_prompt_consumed(dropped));
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
        let earlier = OffsetDateTime::now_utc() - time::Duration::milliseconds(500);
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
        assert_eq!(origin_of(&store, "original").await, None);
        // A stale hook does not change live origin. The delivery is still
        // there for the prompt it typed.
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

    /// Records every injection. Typing is slow, so two unguarded deliveries
    /// would both type before either presses Enter.
    struct SlowPane {
        keys: std::sync::Mutex<Vec<String>>,
    }

    impl crate::PaneBackend for SlowPane {
        fn kind(&self) -> crate::HostKind {
            crate::HostKind::Tmux
        }
        fn list_panes(&self) -> Vec<crate::tmux::PaneInfo> {
            Vec::new()
        }
        fn resolve_pane(&self, _: &str) -> Option<crate::tmux::PaneInfo> {
            None
        }
        fn capture_pane(&self, _: &str) -> Option<String> {
            None
        }
        fn pane_pid_map(&self) -> std::collections::HashMap<u32, String> {
            std::collections::HashMap::new()
        }
        fn current_pane(&self) -> Option<String> {
            None
        }
        fn focus_pane(&self, _: &str) -> bool {
            false
        }
        fn send_text(&self, _: &str, text: &str) -> bool {
            self.keys.lock().unwrap().push(text.into());
            true
        }
        fn send_prompt_text_on(&self, _: Option<&str>, _: &str, text: &str) -> bool {
            std::thread::sleep(Duration::from_millis(100));
            self.keys.lock().unwrap().push(text.into());
            true
        }
    }

    fn slow_pane() -> Arc<SlowPane> {
        Arc::new(SlowPane {
            keys: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn delivery(text: &str) -> PromptDelivery<'_> {
        PromptDelivery {
            pane: "rmux:%7",
            socket: Some("/test.sock"),
            text,
            submit: true,
            origin: Some(PromptOrigin::Peer),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_pane_guard_keeps_two_deliveries_out_of_one_input_line() {
        let store = Store::shared();
        start(&store).await;
        let collaboration =
            CollaborationStore::in_memory(crate::collaboration::CollaborationOptions::default());
        let pane = slow_pane();
        let backend: SharedBackend = pane.clone();
        let (first, second) = tokio::join!(
            deliver_prompt(&collaboration, &store, backend.clone(), delivery("wake")),
            deliver_prompt(&collaboration, &store, backend.clone(), delivery("review")),
        );
        assert!(first.unwrap().submitted);
        assert!(second.unwrap().submitted);
        let keys = pane.keys.lock().unwrap().clone();
        // Each prompt is followed by its own Enter, in either order.
        assert!(
            keys == ["wake", "\r", "review", "\r"] || keys == ["review", "\r", "wake", "\r"],
            "deliveries interleaved: {keys:?}"
        );
    }

    #[tokio::test]
    async fn a_delivery_waits_for_a_held_pane_guard() {
        let store = Store::shared();
        start(&store).await;
        let collaboration =
            CollaborationStore::in_memory(crate::collaboration::CollaborationOptions::default());
        let pane = slow_pane();
        let held = lock_pane(&collaboration, "rmux:%7").await;
        let waiting = tokio::spawn({
            let collaboration = collaboration.clone();
            let store = store.clone();
            let backend: SharedBackend = pane.clone();
            async move {
                deliver_prompt(&collaboration, &store, backend, delivery("late"))
                    .await
                    .map(|d| d.submitted)
            }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            pane.keys.lock().unwrap().is_empty(),
            "typed under a held guard"
        );
        drop(held);
        assert_eq!(waiting.await.unwrap(), Ok(true));
        assert_eq!(*pane.keys.lock().unwrap(), ["late", "\r"]);
    }

    #[tokio::test]
    async fn a_draining_worker_is_refused_and_nothing_is_registered() {
        let store = Store::shared();
        start(&store).await;
        let collaboration =
            CollaborationStore::in_memory(crate::collaboration::CollaborationOptions::default());
        collaboration
            .block_worker_input_for_test("rmux:%7", Some("/test.sock"))
            .await;
        let pane = slow_pane();
        let refused = deliver_prompt(&collaboration, &store, pane.clone(), delivery("go")).await;
        assert!(matches!(refused, Err(DeliveryRefused::WorkerDraining)));
        assert!(pane.keys.lock().unwrap().is_empty());
        // No expected prompt was left behind for a later hook to take.
        submit(&store, "original", "/test.sock", "go").await;
        assert_eq!(
            origin_of(&store, "original").await,
            Some(PromptOrigin::Unknown)
        );
    }

    /// The 2026-11-14 stall: a stop left uncertain on 11-09 still paused
    /// input to pane %19 after a tmux restore gave that id to another agent,
    /// so every typed wake to it was refused without a trace. Once its fenced
    /// process is gone, the stop resolves and the pane takes input again.
    #[tokio::test]
    async fn a_stop_whose_process_exited_no_longer_refuses_the_pane() {
        let store = Store::shared();
        start(&store).await;
        let collaboration =
            CollaborationStore::in_memory(crate::collaboration::CollaborationOptions::default());
        // This pid with another birth: the fenced process has exited.
        let gone = crate::collaboration::ProcessFence {
            pid: std::process::id(),
            birth: "a process that has exited".into(),
        };
        collaboration
            .plant_uncertain_stop_for_test("rmux:%7", Some("/test.sock"), gone)
            .await;
        let pane = slow_pane();
        let refused = deliver_prompt(&collaboration, &store, pane.clone(), delivery("go")).await;
        assert!(matches!(refused, Err(DeliveryRefused::WorkerDraining)));

        assert_eq!(collaboration.resolve_exited_drains().await.unwrap(), 1);
        let views = collaboration.worker_drains().await;
        assert_eq!(
            views[0].phase,
            crate::collaboration::WorkerDrainPhase::Exited
        );
        let delivered = deliver_prompt(&collaboration, &store, pane.clone(), delivery("go")).await;
        assert_eq!(delivered.map(|d| d.submitted), Ok(true));
        assert_eq!(*pane.keys.lock().unwrap(), ["go", "\r"]);
        // Resolved once; nothing left to resolve.
        assert_eq!(collaboration.resolve_exited_drains().await.unwrap(), 0);
    }

    /// A stop whose process is still alive stays unresolved and keeps the
    /// pane's input paused: only exit evidence releases it.
    #[tokio::test]
    async fn a_stop_whose_process_lives_keeps_refusing_the_pane() {
        let store = Store::shared();
        start(&store).await;
        let collaboration =
            CollaborationStore::in_memory(crate::collaboration::CollaborationOptions::default());
        collaboration
            .block_worker_input_for_test("rmux:%7", Some("/test.sock"))
            .await;
        assert_eq!(collaboration.resolve_exited_drains().await.unwrap(), 0);
        let pane = slow_pane();
        let refused = deliver_prompt(&collaboration, &store, pane.clone(), delivery("go")).await;
        assert!(matches!(refused, Err(DeliveryRefused::WorkerDraining)));
        assert!(pane.keys.lock().unwrap().is_empty());
    }
}
