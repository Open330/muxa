//! Re-delivery of a mailbox wake that was typed but never acted on.
//!
//! The waker marks a request notified once the host accepted the paste and the
//! separate Enter. That proves transport, not that the agent ever took a turn:
//! a draft in the composer or an Enter the terminal swallowed leaves the
//! request `Queued` with `notified_at` set, and nothing wakes the recipient
//! again.
//!
//! The waker only types into an Idle recipient, so a wake that started a turn
//! moves the recipient's registry row: a prompt (`last_prompt_at`), a state
//! change (`state_entered_at`, e.g. Working from a tool hook) or a turn's
//! response (`last_response_at`). Each attempt records when it *began*, before
//! anything was typed. A request still unclaimed [`WAKE_SUBMIT_GRACE`] later,
//! whose recipient's row is Idle and shows none of those three after that
//! instant, never started a turn: the row has been Idle and untouched since
//! before the paste. Any recorded movement, including an Idle that a timeout
//! inferred, counts as "something happened" and suppresses the retry, so a
//! lost `PromptSubmitted` hook alone cannot cause a second paste.
//!
//! Such a request gets exactly one more delivery. If that one also leaves the
//! row untouched, nothing is typed again: reconciliation raises the
//! `wake_unsubmitted` interruption (`recovery.rs`). `notified_at` is never
//! cleared, so the request keeps its delivery history for missing-binding
//! recovery and is never treated as undelivered work to hand to a successor.
use super::RequestStatus;
use super::{recovery::exact_agent, CollaborationError, CollaborationRequest, CollaborationStore};
use crate::{Agent, AgentState};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use time::OffsetDateTime;

/// How long a delivered wake may leave its recipient untouched before it is
/// treated as unsubmitted. A submitted wake moves the row within seconds.
pub const WAKE_SUBMIT_GRACE: time::Duration = time::Duration::minutes(2);
/// Mailbox wakes typed for one request: the first and one re-delivery.
pub const MAX_WAKE_ATTEMPTS: u32 = 2;

/// Mailbox wake attempts for one request, kept for the unsubmitted check.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WakeRetry {
    /// When the latest attempt began, before anything was typed.
    #[serde(with = "time::serde::rfc3339")]
    pub attempt_started_at: OffsetDateTime,
    /// Attempts typed so far.
    pub attempts: u32,
    /// The previous attempt left the recipient untouched; the next waker
    /// pass types it once more.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub redeliver: bool,
}

/// The latest wake attempt for `request` is [`WAKE_SUBMIT_GRACE`] old, the
/// request is still unclaimed, and the recipient's exact row is Idle and has
/// recorded no prompt, state change or response since the attempt began.
pub(super) fn wake_unsubmitted(
    request: &CollaborationRequest,
    agent: Option<&Agent>,
    now: OffsetDateTime,
) -> bool {
    let (Some(retry), Some(agent)) = (request.wake_retry.as_ref(), agent) else {
        return false;
    };
    let started = retry.attempt_started_at;
    still_unclaimed(request)
        && !retry.redeliver
        && now - started >= WAKE_SUBMIT_GRACE
        && untouched_since(agent, started)
}

fn still_unclaimed(request: &CollaborationRequest) -> bool {
    request.status == RequestStatus::Queued
        && request.claimed_at.is_none()
        && request.wake_delivery.is_none()
        && request.notified_at.is_some()
        && !request.to.console
}

/// The row is Idle and recorded nothing since `started`: no prompt, no state
/// change, no response, and no activity-bearing hook at all
/// (`last_activity_at` covers a response-less `Stop`, tool and notification
/// hooks, and daemon observations such as a timeout-inferred Idle; status
/// line heartbeats do not move it).
fn untouched_since(agent: &Agent, started: OffsetDateTime) -> bool {
    let untouched = |at: Option<OffsetDateTime>| at.is_none_or(|at| at <= started);
    agent.state == AgentState::Idle
        && agent.state_entered_at <= started
        && agent.last_activity_at <= started
        && untouched(agent.last_prompt_at)
        && untouched(agent.last_response_at)
}

/// How long a request may wait for its idle worker with nothing delivered
/// before it is reported as undelivered. The waker types within seconds of
/// the recipient going Idle and rescans every 30 seconds; a hook offer holds
/// the typed wake back for its own backoff, which this outlasts.
pub const WAKE_UNDELIVERED_AFTER: time::Duration = time::Duration::minutes(5);

/// A request Tend should have typed to its idle worker that has reached it
/// in no way: queued, never claimed, notified or offered recently, while the
/// exact recipient has sat Idle for [`WAKE_UNDELIVERED_AFTER`] since the
/// request was created. Some refusal kept the waker from typing it (an input
/// gate, a backend failure); the sender must learn of it rather than wait.
/// A protected primary (`to.launch_purpose`) is never typed into, so its
/// queue is not a delivery failure.
pub(super) fn wake_undelivered(
    request: &CollaborationRequest,
    agent: Option<&Agent>,
    now: OffsetDateTime,
) -> bool {
    let Some(agent) = agent else {
        return false;
    };
    request.status == RequestStatus::Queued
        && request.claimed_at.is_none()
        && request.notified_at.is_none()
        && request.wake_delivery.is_none()
        && request.wake_retry.is_none()
        && request.should_notify()
        && request.to.launch_purpose == super::LaunchPurpose::Worker
        && !request.to.console
        && !request
            .turn_offer
            .as_ref()
            .is_some_and(|offer| offer.offered_recently(now))
        && agent.state == AgentState::Idle
        && now - request.created_at.max(agent.state_entered_at) >= WAKE_UNDELIVERED_AFTER
}

/// A ready recipient's registry row key: pane, socket and session id.
pub type ReadyRecipient = (String, Option<String>, String);

impl CollaborationStore {
    /// Remember the registry rows the waker found ready, by delivery's own
    /// predicate, for requests still addressed to a launching pane (see
    /// [`Self::ready_pending_agent`]). An entry stays, even while that row is
    /// busy, until reconciliation drops it once the request is delivered,
    /// re-addressed or finished: a validated recipient is never swapped for
    /// whatever row later sits at the pane.
    pub fn note_ready_pending_recipients(&self, ready: HashMap<String, ReadyRecipient>) {
        self.ready_pending_recipients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(ready);
    }

    /// The row remembered for `request_id` (diagnostics and tests).
    pub fn ready_pending_recipient(&self, request_id: &str) -> Option<ReadyRecipient> {
        self.ready_pending_recipients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(request_id)
            .cloned()
    }

    /// Forget entries whose request no longer waits on a launching pane.
    pub(super) fn prune_ready_pending(&self, requests: &HashMap<String, CollaborationRequest>) {
        self.ready_pending_recipients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|id, _| {
                requests.get(id).is_some_and(|request| {
                    !request.status.is_terminal()
                        && super::is_pending_session(&request.to.agent_session_id)
                        && request.notified_at.is_none()
                        && request.claimed_at.is_none()
                })
            });
    }

    /// The registry row the waker last found ready for a request still
    /// addressed to a launching pane. Reconciliation has no pane list, so it
    /// cannot repeat delivery's endpoint, room and launch-mark checks; it
    /// takes the waker's verdict instead of matching any row at the pane.
    pub(super) fn ready_pending_agent<'a>(
        &self,
        request: &CollaborationRequest,
        agents: &'a [Agent],
    ) -> Option<&'a Agent> {
        if !super::is_pending_session(&request.to.agent_session_id) {
            return None;
        }
        let ready = self
            .ready_pending_recipients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (pane, socket, session) = ready.get(&request.id)?;
        agents.iter().find(|agent| {
            agent.pane.as_deref() == Some(pane.as_str())
                && agent.tmux_socket == *socket
                && agent.session_id == *session
        })
    }
}

/// The final attempt was typed and also left the recipient untouched.
pub(super) fn wake_exhausted_unsubmitted(
    request: &CollaborationRequest,
    agent: Option<&Agent>,
    now: OffsetDateTime,
) -> bool {
    request
        .wake_retry
        .as_ref()
        .is_some_and(|retry| retry.attempts >= MAX_WAKE_ATTEMPTS)
        && wake_unsubmitted(request, agent, now)
}

impl CollaborationStore {
    /// Record a mailbox wake typed for `request_id`, whose attempt began at
    /// `started_at` (before the paste). The first attempt also sets
    /// `notified_at`, as [`Self::mark_notified`] does; a re-delivery keeps it.
    pub async fn mark_wake_notified(
        &self,
        request_id: &str,
        started_at: OffsetDateTime,
    ) -> Result<(), CollaborationError> {
        {
            let _transaction = self.transaction_lock.lock().await;
            let redelivery = {
                let mut requests = self.requests.write().await;
                requests.get_mut(request_id).and_then(|request| {
                    request.notified_at?;
                    let retry = request.wake_retry.as_mut()?;
                    if !retry.redeliver {
                        return None;
                    }
                    retry.redeliver = false;
                    retry.attempts += 1;
                    retry.attempt_started_at = started_at;
                    Some(request.clone())
                })
            };
            if let Some(row) = redelivery {
                // Persisted under the transaction lock, so no newer claim or
                // reply can be overwritten by this row.
                let result = self.persist_requests(std::slice::from_ref(&row));
                self.publish_change();
                return result;
            }
        }
        self.mark_notified(request_id).await?;
        let _transaction = self.transaction_lock.lock().await;
        let row = {
            let mut requests = self.requests.write().await;
            requests.get_mut(request_id).and_then(|request| {
                (request.status == RequestStatus::Queued && request.wake_retry.is_none()).then(
                    || {
                        request.wake_retry = Some(WakeRetry {
                            attempt_started_at: started_at,
                            attempts: 1,
                            redeliver: false,
                        });
                        request.clone()
                    },
                )
            })
        };
        if let Some(row) = row {
            self.persist_requests(std::slice::from_ref(&row))?;
        }
        Ok(())
    }

    /// Re-check a scheduled re-delivery against the recipient's row read just
    /// before typing. Anything recorded since the original attempt began, or
    /// a claim, cancels it (persisted), and the caller types nothing.
    pub async fn confirm_redelivery(
        &self,
        request_id: &str,
        agent: Option<&Agent>,
    ) -> Result<bool, CollaborationError> {
        let _transaction = self.transaction_lock.lock().await;
        let mut requests = self.requests.write().await;
        let Some(request) = requests.get_mut(request_id) else {
            return Ok(false);
        };
        let Some(retry) = request.wake_retry.as_ref() else {
            return Ok(false);
        };
        if !retry.redeliver {
            return Ok(false);
        }
        let started = retry.attempt_started_at;
        if still_unclaimed(request) && agent.is_some_and(|agent| untouched_since(agent, started)) {
            return Ok(true);
        }
        if let Some(retry) = request.wake_retry.as_mut() {
            retry.redeliver = false;
        }
        let row = request.clone();
        drop(requests);
        self.persist_requests(std::slice::from_ref(&row))?;
        tracing::info!(
            request_id,
            "recipient moved after an unsubmitted wake was scheduled; not re-delivering",
        );
        Ok(false)
    }

    /// Schedule the one re-delivery of each wake that left its recipient
    /// untouched (see the module docs). Returns how many were scheduled.
    pub async fn rearm_unsubmitted_wakes(
        &self,
        agents: &[Agent],
    ) -> Result<usize, CollaborationError> {
        self.ensure_enabled()?;
        let _transaction = self.transaction_lock.lock().await;
        let now = OffsetDateTime::now_utc();
        let rows: Vec<_> = {
            let requests = self.requests.read().await;
            requests
                .values()
                .filter(|request| {
                    request
                        .wake_retry
                        .as_ref()
                        .is_some_and(|retry| retry.attempts < MAX_WAKE_ATTEMPTS)
                        && wake_unsubmitted(request, exact_agent(&request.to, agents), now)
                })
                .map(|request| {
                    let mut row = request.clone();
                    if let Some(retry) = row.wake_retry.as_mut() {
                        retry.redeliver = true;
                    }
                    row
                })
                .collect()
        };
        if !rows.is_empty() {
            for row in &rows {
                tracing::info!(
                    request_id = row.id,
                    pane = row.to.pane,
                    "collaboration wake left its idle recipient untouched; re-delivering once",
                );
            }
            self.commit_request_rows(&rows).await?;
            self.publish_change();
        }
        Ok(rows.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collaboration::turn_offer::TurnBoundaryEvent;
    use crate::collaboration::{CollaborationOptions, NewRequest, Participant};
    use crate::event::{AgentEvent, AgentId};
    use crate::AgentKind;

    async fn actor(pane: &str, session: &str) -> (Participant, Agent) {
        let store = crate::state::Store::shared();
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::Codex,
                    session_id: session.into(),
                    surface: None,
                    pane: Some(pane.into()),
                    tmux_socket: Some("default".into()),
                    cwd: None,
                },
                at: OffsetDateTime::now_utc(),
            })
            .await;
        let mut agent = store.snapshot().await.remove(0);
        agent.state = AgentState::Idle;
        agent.last_prompt_at = None;
        agent.last_response_at = None;
        agent.state_entered_at = OffsetDateTime::now_utc() - time::Duration::hours(1);
        agent.last_activity_at = agent.state_entered_at;
        let participant = serde_json::from_value(serde_json::json!({
            "launch_purpose":"worker", "agent_kind":"codex", "agent_session_id":session,
            "pane":pane, "socket":"default",
            "room":{"host":"tmux","socket":"default","window_id":"@1"}, "state":"idle"
        }))
        .unwrap();
        (participant, agent)
    }

    /// Move the latest attempt `by` into the past.
    async fn age_attempt(store: &CollaborationStore, id: &str, by: time::Duration) {
        let mut requests = store.requests.write().await;
        let retry = requests.get_mut(id).unwrap().wake_retry.as_mut().unwrap();
        retry.attempt_started_at -= by;
    }

    async fn row(store: &CollaborationStore, id: &str) -> CollaborationRequest {
        store.requests.read().await[id].clone()
    }

    fn ids(requests: &[CollaborationRequest]) -> Vec<String> {
        requests.iter().map(|request| request.id.clone()).collect()
    }

    struct Delivered {
        id: String,
        recipient: Participant,
        coordinator: Agent,
        peer: Agent,
    }

    async fn delivered(store: &CollaborationStore) -> Delivered {
        let (sender, coordinator) = actor("%1", "sender").await;
        let (recipient, peer) = actor("%2", "peer").await;
        let request = store
            .create(
                sender,
                recipient.clone(),
                NewRequest {
                    body: "review".into(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        // The waker began the attempt, pasted the notice and pressed Enter.
        store
            .mark_wake_notified(&request.id, OffsetDateTime::now_utc())
            .await
            .unwrap();
        Delivered {
            id: request.id,
            recipient,
            coordinator,
            peer,
        }
    }

    #[tokio::test]
    async fn a_wake_that_left_its_recipient_untouched_is_redelivered_once_then_interrupted() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let Delivered {
            id,
            recipient,
            coordinator,
            peer,
        } = delivered(&store).await;
        let agents = [coordinator, peer];
        // Within the grace nothing is judged.
        assert_eq!(store.rearm_unsubmitted_wakes(&agents).await.unwrap(), 0);
        // The stall: unclaimed, recipient Idle and untouched since the paste.
        age_attempt(&store, &id, WAKE_SUBMIT_GRACE).await;
        assert_eq!(store.rearm_unsubmitted_wakes(&agents).await.unwrap(), 1);
        let pending = store.pending_unnotified().await;
        assert_eq!(ids(&pending), vec![id.clone()], "the waker types it again");
        assert!(
            row(&store, &id).await.notified_at.is_some(),
            "delivery history is kept"
        );
        // Whatever prompt is submitted next carries it as a structured offer.
        let offered = store
            .offer_turn_boundary(&recipient, TurnBoundaryEvent::PromptSubmit, None, None)
            .await
            .unwrap();
        assert_eq!(offered[0]["request_id"], id.as_str());
        // The re-delivery is typed and also leaves the row untouched.
        store
            .mark_wake_notified(&id, OffsetDateTime::now_utc())
            .await
            .unwrap();
        assert!(store.pending_unnotified().await.is_empty());
        assert_eq!(row(&store, &id).await.wake_retry.unwrap().attempts, 2);
        age_attempt(&store, &id, WAKE_SUBMIT_GRACE).await;
        assert_eq!(
            store.rearm_unsubmitted_wakes(&agents).await.unwrap(),
            0,
            "only one re-delivery is typed"
        );
        store.reconcile_peer_interruptions(&agents).await.unwrap();
        let held = row(&store, &id).await;
        assert!(held.peer_interrupted());
        assert_eq!(held.interruption.unwrap().reason, "wake_unsubmitted");
        assert_eq!(held.status, RequestStatus::Queued, "nothing is consumed");
    }

    #[tokio::test]
    async fn any_recorded_movement_after_the_attempt_suppresses_a_retry() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let Delivered {
            id,
            coordinator,
            peer,
            ..
        } = delivered(&store).await;
        age_attempt(&store, &id, WAKE_SUBMIT_GRACE).await;
        let started = row(&store, &id)
            .await
            .wake_retry
            .unwrap()
            .attempt_started_at;
        let after = started + time::Duration::seconds(1);
        // A lost PromptSubmitted hook, but the turn's Stop recorded a response.
        let mut answered = peer.clone();
        answered.last_response_at = Some(after);
        // A tool hook moved it (then Idle again, possibly timeout-inferred).
        let mut moved = peer.clone();
        moved.state_entered_at = after;
        let mut prompted = peer.clone();
        prompted.last_prompt_at = Some(after);
        let mut busy = peer.clone();
        busy.state = AgentState::Working;
        for agent in [answered, moved, prompted, busy] {
            let agents = [coordinator.clone(), agent];
            assert_eq!(store.rearm_unsubmitted_wakes(&agents).await.unwrap(), 0);
        }
        // An unrelated prompt that ran shortly *before* the attempt is not
        // evidence for this wake: the retry still happens.
        let mut earlier = peer.clone();
        earlier.last_prompt_at = Some(started - time::Duration::seconds(10));
        earlier.last_response_at = Some(started - time::Duration::seconds(5));
        earlier.state_entered_at = started - time::Duration::seconds(5);
        assert_eq!(
            store
                .rearm_unsubmitted_wakes(&[coordinator, earlier])
                .await
                .unwrap(),
            1
        );
        // A claim settles it.
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let delivered = delivered(&store).await;
        age_attempt(&store, &delivered.id, WAKE_SUBMIT_GRACE).await;
        store
            .claim_by_id(&delivered.recipient, &delivered.id)
            .await
            .unwrap();
        let agents = [delivered.coordinator, delivered.peer];
        assert_eq!(store.rearm_unsubmitted_wakes(&agents).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_redelivery_keeps_missing_binding_recovery_and_is_never_rebound() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let Delivered {
            id,
            recipient,
            coordinator,
            peer,
        } = delivered(&store).await;
        age_attempt(&store, &id, WAKE_SUBMIT_GRACE).await;
        store
            .rearm_unsubmitted_wakes(&[coordinator.clone(), peer])
            .await
            .unwrap();
        // Before the re-delivery the recipient disappears.
        assert!(
            store.undelivered_requests().await.is_empty(),
            "already-transported work is not handed to a slot successor"
        );
        let mut other = recipient.clone();
        other.agent_session_id = "successor".into();
        assert!(store
            .rebind_recipient(&id, &recipient, other)
            .await
            .unwrap()
            .is_none());
        store
            .reconcile_peer_interruptions(std::slice::from_ref(&coordinator))
            .await
            .unwrap();
        {
            let mut missing = store.missing_recipients.lock().await;
            *missing.get_mut(&id).unwrap() -= time::Duration::minutes(2);
        }
        store
            .reconcile_peer_interruptions(&[coordinator])
            .await
            .unwrap();
        let held = row(&store, &id).await;
        assert!(held.peer_interrupted());
        assert_eq!(held.interruption.unwrap().reason, "binding_unavailable");
    }

    #[tokio::test]
    async fn the_interruption_clears_once_the_recipient_takes_a_turn() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let Delivered {
            id,
            coordinator,
            mut peer,
            ..
        } = delivered(&store).await;
        age_attempt(&store, &id, WAKE_SUBMIT_GRACE).await;
        let agents = [coordinator.clone(), peer.clone()];
        store.rearm_unsubmitted_wakes(&agents).await.unwrap();
        store
            .mark_wake_notified(&id, OffsetDateTime::now_utc())
            .await
            .unwrap();
        age_attempt(&store, &id, WAKE_SUBMIT_GRACE).await;
        store.reconcile_peer_interruptions(&agents).await.unwrap();
        assert!(row(&store, &id).await.peer_interrupted());
        // A person clears the draft and submits; the hook records the prompt.
        peer.last_prompt_at = Some(OffsetDateTime::now_utc());
        store
            .reconcile_peer_interruptions(&[coordinator, peer])
            .await
            .unwrap();
        assert!(!row(&store, &id).await.peer_interrupted());
    }

    /// A lost `PromptSubmitted` hook followed by a real response-less `Stop`
    /// (no state change Idle → Idle, no response text) still counts as the
    /// recipient having taken a turn.
    #[tokio::test]
    async fn a_response_less_stop_after_the_attempt_suppresses_a_retry() {
        let registry = crate::state::Store::shared();
        let id = AgentId {
            kind: AgentKind::Codex,
            session_id: "peer".into(),
            surface: None,
            pane: Some("%2".into()),
            tmux_socket: Some("default".into()),
            cwd: None,
        };
        let long_ago = OffsetDateTime::now_utc() - time::Duration::hours(1);
        let stop = |at| AgentEvent::TurnStopped {
            id: id.clone(),
            response: None,
            recap: None,
            ai_title: None,
            idle_confirmed: false,
            at,
        };
        registry
            .apply(&AgentEvent::Started {
                id: id.clone(),
                at: long_ago,
            })
            .await;
        registry.apply(&stop(long_ago)).await;
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let Delivered { id: request, .. } = delivered(&store).await;
        age_attempt(&store, &request, WAKE_SUBMIT_GRACE * 2).await;
        let untouched = registry.snapshot().await;
        assert_eq!(untouched[0].state, AgentState::Idle);
        let (_, coordinator) = actor("%1", "sender").await;
        // Without the Stop, this is a stall.
        let probe = CollaborationStore::in_memory(CollaborationOptions::default());
        let Delivered { id: probe_id, .. } = delivered(&probe).await;
        age_attempt(&probe, &probe_id, WAKE_SUBMIT_GRACE * 2).await;
        assert_eq!(
            probe
                .rearm_unsubmitted_wakes(&[coordinator.clone(), untouched[0].clone()])
                .await
                .unwrap(),
            1
        );
        // The turn the wake started ends with a response-less Stop.
        registry
            .apply(&stop(
                OffsetDateTime::now_utc() - time::Duration::minutes(1),
            ))
            .await;
        let after = registry.snapshot().await;
        assert_eq!(after[0].state, AgentState::Idle);
        assert_eq!(after[0].state_entered_at, untouched[0].state_entered_at);
        assert!(after[0].last_response_at.is_none());
        assert_eq!(
            store
                .rearm_unsubmitted_wakes(&[coordinator, after[0].clone()])
                .await
                .unwrap(),
            0
        );
    }

    /// Movement after the re-delivery was scheduled (a delayed hook, a turn
    /// while the backend was unavailable) cancels it before anything is typed.
    #[tokio::test]
    async fn a_scheduled_redelivery_is_cancelled_by_later_movement() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let Delivered {
            id,
            coordinator,
            mut peer,
            ..
        } = delivered(&store).await;
        age_attempt(&store, &id, WAKE_SUBMIT_GRACE).await;
        store
            .rearm_unsubmitted_wakes(&[coordinator, peer.clone()])
            .await
            .unwrap();
        assert!(store.confirm_redelivery(&id, Some(&peer)).await.unwrap());
        peer.last_activity_at = OffsetDateTime::now_utc();
        assert!(!store.confirm_redelivery(&id, Some(&peer)).await.unwrap());
        assert!(
            store.pending_unnotified().await.is_empty(),
            "the cancelled re-delivery is no longer pending"
        );
        assert!(!store.confirm_redelivery(&id, None).await.unwrap());
    }
    /// Create a request to an idle worker that nothing has delivered, made
    /// `age` ago.
    async fn undelivered(
        store: &CollaborationStore,
        age: time::Duration,
    ) -> (String, Participant, [Agent; 2]) {
        let (sender, coordinator) = actor("%1", "sender").await;
        let (recipient, peer) = actor("%2", "peer").await;
        let request = store
            .create(
                sender,
                recipient.clone(),
                NewRequest {
                    body: "implement".into(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        store
            .requests
            .write()
            .await
            .get_mut(&request.id)
            .unwrap()
            .created_at -= age;
        (request.id, recipient, [coordinator, peer])
    }

    /// The 2026-11-14 stall: a task to an idle worker was never typed (its
    /// pane refused input) and sat Queued for 50 minutes with no trace. After
    /// the bound the sender sees `wake_undelivered`; a claim clears it.
    #[tokio::test]
    async fn a_request_its_idle_worker_never_received_is_reported_undelivered() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let (id, recipient, agents) =
            undelivered(&store, WAKE_UNDELIVERED_AFTER - time::Duration::seconds(30)).await;
        store.reconcile_peer_interruptions(&agents).await.unwrap();
        assert!(
            !row(&store, &id).await.peer_interrupted(),
            "within the bound the waker may still type it"
        );
        store
            .requests
            .write()
            .await
            .get_mut(&id)
            .unwrap()
            .created_at -= time::Duration::minutes(1);
        store.reconcile_peer_interruptions(&agents).await.unwrap();
        let held = row(&store, &id).await;
        assert!(held.peer_interrupted());
        assert_eq!(held.interruption.unwrap().reason, "wake_undelivered");
        assert_eq!(held.status, RequestStatus::Queued, "nothing is consumed");

        store.claim_by_id(&recipient, &id).await.unwrap();
        store.reconcile_peer_interruptions(&agents).await.unwrap();
        assert!(
            !row(&store, &id).await.peer_interrupted(),
            "delivery clears it"
        );
    }

    async fn reason_of(store: &CollaborationStore, id: &str) -> Option<String> {
        store.requests.read().await[id]
            .interruption
            .as_ref()
            .map(|i| i.reason.clone())
    }

    /// Waiting is not a delivery failure when nothing would type it: a
    /// protected primary's queue, `wake = "never"`, a busy recipient, a
    /// recent hook offer, or a recipient idle only briefly.
    #[tokio::test]
    async fn expected_waits_are_not_reported_undelivered() {
        let old = WAKE_UNDELIVERED_AFTER + time::Duration::minutes(10);

        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let (id, _, agents) = undelivered(&store, old).await;
        store
            .requests
            .write()
            .await
            .get_mut(&id)
            .unwrap()
            .to
            .launch_purpose = crate::collaboration::LaunchPurpose::Primary;
        store.reconcile_peer_interruptions(&agents).await.unwrap();
        assert_eq!(reason_of(&store, &id).await, None, "a primary's queue");

        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        store.set_turn_continuation(false);
        let (id, _, agents) = undelivered(&store, old).await;
        store.reconcile_peer_interruptions(&agents).await.unwrap();
        assert_eq!(reason_of(&store, &id).await, None, "wake = never");

        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let (id, _, [coordinator, mut peer]) = undelivered(&store, old).await;
        peer.state = AgentState::Working;
        store
            .reconcile_peer_interruptions(&[coordinator.clone(), peer.clone()])
            .await
            .unwrap();
        assert_eq!(reason_of(&store, &id).await, None, "a busy recipient");
        // Idle again only a minute ago: the waker has not had its chance.
        peer.state = AgentState::Idle;
        peer.state_entered_at = OffsetDateTime::now_utc() - time::Duration::minutes(1);
        store
            .reconcile_peer_interruptions(&[coordinator, peer])
            .await
            .unwrap();
        assert_eq!(reason_of(&store, &id).await, None, "briefly idle");

        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        store.set_hook_transport(true);
        let (id, recipient, agents) = undelivered(&store, old).await;
        let offers = store
            .offer_turn_boundary(&recipient, TurnBoundaryEvent::Stop, None, None)
            .await
            .unwrap();
        assert_eq!(offers[0]["request_id"], id.as_str());
        store.reconcile_peer_interruptions(&agents).await.unwrap();
        assert_eq!(reason_of(&store, &id).await, None, "a recent hook offer");
    }
    /// A task to a launching worker (`pending-pane:` address) whose pane is
    /// ready and Idle, but every write to it is refused, is bounded too. The
    /// row is the one delivery validated, found by `ready_row_key` from the
    /// delivery participant: here a synthetic pre-hook row, whose id differs
    /// from the participant's. A row merely at the pane id does not count,
    /// and once the validated recipient is busy the item clears.
    #[tokio::test]
    async fn a_pending_worker_ready_but_refusing_writes_is_reported_undelivered() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let (sender, coordinator) = actor("%1", "sender").await;
        let (mut launching, mut synthetic) = actor("%2", "peer").await;
        launching.agent_session_id = "pending-pane:default:%2".into();
        synthetic.session_id = format!("{}7:default:%2", crate::state::SYNTHETIC_SESSION_PREFIX);
        let id = store
            .create(
                sender,
                launching.clone(),
                NewRequest {
                    body: "implement".into(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap()
            .id;
        store
            .requests
            .write()
            .await
            .get_mut(&id)
            .unwrap()
            .created_at -= WAKE_UNDELIVERED_AFTER + time::Duration::minutes(1);
        let mut agents = [coordinator, synthetic];
        store.reconcile_peer_interruptions(&agents).await.unwrap();
        assert_eq!(
            reason_of(&store, &id).await,
            None,
            "without the waker's verdict a row at the pane id is not the recipient"
        );
        // The waker's pass: delivery found `launching` ready, writes refused.
        let key = crate::collaboration::ready_row_key(&launching, &agents).unwrap();
        assert_eq!(key.2, agents[1].session_id, "the synthetic row's own id");
        store.note_ready_pending_recipients(HashMap::from([(id.clone(), key)]));
        store.reconcile_peer_interruptions(&agents).await.unwrap();
        assert_eq!(
            reason_of(&store, &id).await.as_deref(),
            Some("wake_undelivered")
        );
        assert!(row(&store, &id).await.peer_interrupted());
        // The recipient starts other work without claiming it: the item
        // clears, as it does for an exact-session recipient.
        agents[1].state = AgentState::Working;
        store.reconcile_peer_interruptions(&agents).await.unwrap();
        assert!(!row(&store, &id).await.peer_interrupted());
    }

    /// Delivered without being re-addressed: the item clears and the
    /// remembered recipient is forgotten.
    #[tokio::test]
    async fn a_pending_request_delivered_at_last_clears_and_is_forgotten() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let (sender, coordinator) = actor("%1", "sender").await;
        let (mut launching, peer) = actor("%2", "peer").await;
        launching.agent_session_id = "pending-pane:default:%2".into();
        let id = store
            .create(
                sender,
                launching.clone(),
                NewRequest {
                    body: "implement".into(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap()
            .id;
        store
            .requests
            .write()
            .await
            .get_mut(&id)
            .unwrap()
            .created_at -= WAKE_UNDELIVERED_AFTER + time::Duration::minutes(1);
        let agents = [coordinator, peer];
        let key = crate::collaboration::ready_row_key(&launching, &agents).unwrap();
        store.note_ready_pending_recipients(HashMap::from([(id.clone(), key)]));
        store.reconcile_peer_interruptions(&agents).await.unwrap();
        assert!(row(&store, &id).await.peer_interrupted());
        store
            .mark_wake_notified(&id, OffsetDateTime::now_utc())
            .await
            .unwrap();
        store.reconcile_peer_interruptions(&agents).await.unwrap();
        assert!(!row(&store, &id).await.peer_interrupted());
        assert!(store.ready_pending_recipients.lock().unwrap().is_empty());
    }
}
