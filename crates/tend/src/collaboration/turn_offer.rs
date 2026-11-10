//! Turn-boundary offers of user-authorized agent work to human primaries.
//!
//! tendd never types such a request into a primary's terminal: the human may
//! have an unsent draft there, and paste plus Enter would submit it with the
//! request. The session's own provider hook asks for an *offer* at a turn
//! boundary instead and shows it to the model, which claims each request by id
//! before acting. An offer changes no status: a lost IPC response, a restart
//! before the hook prints, a provider that discards hook output, or another
//! hook overriding continuation all leave the request queued for the next
//! boundary. The counters below record offer *attempts*, never delivery.
use super::{addresses, CollaborationError, CollaborationStore, Participant, RequestStatus};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use time::OffsetDateTime;

/// Automatic Stop continuations per request. After that, only a turn the user
/// starts (`prompt_submit`) offers it, so an agent that ignores an offer
/// cannot loop its own turn.
pub const MAX_STOP_OFFERS: u32 = 3;
/// Minimum gap between two offers of one request through the same event.
pub const OFFER_BACKOFF: time::Duration = time::Duration::minutes(5);
/// Server-side clamps on what one hook response may carry. The byte budget
/// covers the offered payload objects; the IPC envelope and the hook's short
/// instruction text add a small fixed overhead on top, and the count clamp
/// bounds the total.
pub const MAX_OFFER_REQUESTS: usize = 8;
pub const MAX_OFFER_BYTES: usize = 24 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnBoundaryEvent {
    /// The agent finished a turn; an offer continues it.
    Stop,
    /// The user submitted a prompt; an offer rides along with it.
    PromptSubmit,
}

/// Offer attempts for one request, kept on the request for audit and
/// backoff. Attempts prove nothing about delivery.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnOffer {
    #[serde(default)]
    pub stop_offers: u32,
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub last_stop_offer_at: Option<OffsetDateTime>,
    #[serde(default)]
    pub prompt_offers: u32,
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub last_prompt_offer_at: Option<OffsetDateTime>,
}

impl TurnOffer {
    fn eligible(&self, event: TurnBoundaryEvent, now: OffsetDateTime) -> bool {
        let recent = |at: Option<OffsetDateTime>| at.is_some_and(|at| now - at < OFFER_BACKOFF);
        match event {
            TurnBoundaryEvent::Stop => {
                self.stop_offers < MAX_STOP_OFFERS && !recent(self.last_stop_offer_at)
            }
            // A turn the user started may surface work a lost Stop offer
            // missed; only its own recent offer holds it back.
            TurnBoundaryEvent::PromptSubmit => !recent(self.last_prompt_offer_at),
        }
    }

    fn record(&mut self, event: TurnBoundaryEvent, now: OffsetDateTime) {
        match event {
            TurnBoundaryEvent::Stop => {
                self.stop_offers += 1;
                self.last_stop_offer_at = Some(now);
            }
            TurnBoundaryEvent::PromptSubmit => {
                self.prompt_offers += 1;
                self.last_prompt_offer_at = Some(now);
            }
        }
    }
}

impl CollaborationStore {
    /// Whether a Stop hook may continue a turn with an offer. False when
    /// collaboration wakes are disabled (`wake = "never"`).
    pub fn set_turn_continuation(&self, enabled: bool) {
        self.turn_continuation.store(enabled, Ordering::SeqCst);
    }

    /// Select, record and return offers for `caller` at a turn boundary.
    ///
    /// Only queued, user-authorized agent requests addressed to exactly this
    /// session qualify, oldest first, within a count and serialized-byte
    /// budget. A Stop offer also honours quiet requests and the wake policy;
    /// a prompt the user submitted may carry quiet ones. A single request
    /// larger than the budget is offered as a compact id so it cannot block
    /// the queue behind it. Statuses are untouched.
    pub async fn offer_turn_boundary(
        &self,
        caller: &Participant,
        event: TurnBoundaryEvent,
        max_requests: Option<usize>,
        max_bytes: Option<usize>,
    ) -> Result<Vec<Value>, CollaborationError> {
        self.ensure_enabled()?;
        if event == TurnBoundaryEvent::Stop && !self.turn_continuation.load(Ordering::SeqCst) {
            return Ok(Vec::new());
        }
        let max_requests = max_requests
            .unwrap_or(MAX_OFFER_REQUESTS)
            .clamp(1, MAX_OFFER_REQUESTS);
        let max_bytes = max_bytes
            .unwrap_or(MAX_OFFER_BYTES)
            .clamp(1024, MAX_OFFER_BYTES);
        let _transaction = self.transaction_lock.lock().await;
        if self.worker_claim_blocked(caller).await {
            return Ok(Vec::new());
        }
        let now = OffsetDateTime::now_utc();
        let mut candidates: Vec<_> = {
            let requests = self.requests.read().await;
            requests
                .values()
                .filter(|request| {
                    request.status == RequestStatus::Queued
                        && addresses(&request.to, caller)
                        // Judged against the session as it is now.
                        && request.awaits_turn_boundary(caller)
                        && (event == TurnBoundaryEvent::PromptSubmit || request.should_notify())
                        && request
                            .turn_offer
                            .as_ref()
                            .is_none_or(|offer| offer.eligible(event, now))
                })
                .cloned()
                .collect()
        };
        candidates.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));

        let mut offers = Vec::new();
        let mut rows = Vec::new();
        let mut used = 0usize;
        for mut request in candidates {
            if offers.len() == max_requests {
                break;
            }
            let full = offer_payload(&request);
            let full_len = full.to_string().len();
            let payload = if used + full_len <= max_bytes {
                full
            } else if offers.is_empty() {
                // Oversized head: offer its id so it still makes progress.
                compact_payload(&request)
            } else {
                break;
            };
            used += payload.to_string().len();
            request
                .turn_offer
                .get_or_insert_with(TurnOffer::default)
                .record(event, now);
            offers.push(payload);
            rows.push(request);
        }
        if !rows.is_empty() {
            self.commit_request_rows(&rows).await?;
            self.publish_change();
        }
        Ok(offers)
    }
}

fn offer_payload(request: &super::CollaborationRequest) -> Value {
    let mut value = super::instruction::inbox_instruction(request);
    value["offer"] = json!("claim_before_acting");
    value
}

fn compact_payload(request: &super::CollaborationRequest) -> Value {
    json!({
        "request_id": request.id,
        "from": request.from.label(),
        "kind": request.kind,
        "offer": "claim_to_read",
        "note": "too large to include; claim it to read the instruction",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collaboration::{
        CollaborationOptions, LaunchPurpose, NewRequest, Participant, RequestKind,
    };
    use std::sync::Arc;

    fn participant(session: &str, pane: &str) -> Participant {
        serde_json::from_value(serde_json::json!({
            "agent_kind":"codex", "agent_session_id":session, "pane":pane,
            "socket":"default", "room":{"host":"tmux","socket":"default","window_id":"@1"},
            "state":"idle"
        }))
        .unwrap()
    }

    fn primary() -> Participant {
        participant("human", "%1")
    }

    fn sender() -> Participant {
        participant("agent", "%2")
    }

    async fn send(store: &CollaborationStore, body: &str, authorized: bool) -> String {
        store
            .create(
                sender(),
                primary(),
                NewRequest {
                    body: body.into(),
                    user_authorized: authorized,
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .id
    }

    fn store() -> Arc<CollaborationStore> {
        CollaborationStore::in_memory(CollaborationOptions::default())
    }

    async fn age_offers(store: &CollaborationStore, by: time::Duration) {
        let mut requests = store.requests.write().await;
        for request in requests.values_mut() {
            if let Some(offer) = request.turn_offer.as_mut() {
                offer.last_stop_offer_at = offer.last_stop_offer_at.map(|at| at - by);
                offer.last_prompt_offer_at = offer.last_prompt_offer_at.map(|at| at - by);
            }
        }
    }

    fn ids(offers: &[Value]) -> Vec<String> {
        offers
            .iter()
            .map(|offer| offer["request_id"].as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn an_offer_claims_nothing_and_is_retried_after_backoff() {
        let store = store();
        let id = send(&store, "plan the import", true).await;
        send(&store, "not authorized", false).await;
        let offers = store
            .offer_turn_boundary(&primary(), TurnBoundaryEvent::Stop, None, None)
            .await
            .unwrap();
        assert_eq!(ids(&offers), vec![id.clone()]);
        assert_eq!(offers[0]["offer"], "claim_before_acting");
        let row = store.peek_for(&primary(), &id).await.unwrap();
        assert_eq!(row.status, RequestStatus::Queued, "an offer must not claim");
        assert_eq!(row.turn_offer.as_ref().unwrap().stop_offers, 1);
        // A lost hook response: nothing again until the backoff passes.
        assert!(store
            .offer_turn_boundary(&primary(), TurnBoundaryEvent::Stop, None, None)
            .await
            .unwrap()
            .is_empty());
        age_offers(&store, OFFER_BACKOFF).await;
        let again = store
            .offer_turn_boundary(&primary(), TurnBoundaryEvent::Stop, None, None)
            .await
            .unwrap();
        assert_eq!(ids(&again), vec![id.clone()]);
        // The model's claim is what consumes it.
        let claimed = store.claim_by_id(&primary(), &id).await.unwrap();
        assert_eq!(claimed.status, RequestStatus::Claimed);
        age_offers(&store, OFFER_BACKOFF).await;
        assert!(store
            .offer_turn_boundary(&primary(), TurnBoundaryEvent::Stop, None, None)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn stop_offers_are_capped_but_a_user_turn_still_surfaces_work() {
        let store = store();
        let id = send(&store, "ignored so far", true).await;
        for _ in 0..MAX_STOP_OFFERS {
            assert_eq!(
                store
                    .offer_turn_boundary(&primary(), TurnBoundaryEvent::Stop, None, None)
                    .await
                    .unwrap()
                    .len(),
                1
            );
            age_offers(&store, OFFER_BACKOFF).await;
        }
        assert!(store
            .offer_turn_boundary(&primary(), TurnBoundaryEvent::Stop, None, None)
            .await
            .unwrap()
            .is_empty());
        let user_turn = store
            .offer_turn_boundary(&primary(), TurnBoundaryEvent::PromptSubmit, None, None)
            .await
            .unwrap();
        assert_eq!(ids(&user_turn), vec![id]);
    }

    #[tokio::test]
    async fn quiet_notices_wait_for_a_user_turn_and_wake_never_stops_continuation() {
        let store = store();
        let notice = store
            .create(
                sender(),
                primary(),
                NewRequest {
                    kind: RequestKind::Notice,
                    expects_reply: false,
                    body: "fyi".into(),
                    user_authorized: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(store
            .offer_turn_boundary(&primary(), TurnBoundaryEvent::Stop, None, None)
            .await
            .unwrap()
            .is_empty());
        let offered = store
            .offer_turn_boundary(&primary(), TurnBoundaryEvent::PromptSubmit, None, None)
            .await
            .unwrap();
        assert_eq!(ids(&offered), vec![notice.id.clone()]);
        assert_eq!(
            store.peek_for(&primary(), &notice.id).await.unwrap().status,
            RequestStatus::Queued,
            "a one-way notice stays queued until the model claims it"
        );

        let store = self::store();
        send(&store, "loud", true).await;
        store.set_turn_continuation(false);
        assert!(store
            .offer_turn_boundary(&primary(), TurnBoundaryEvent::Stop, None, None)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn the_budget_keeps_oldest_first_and_an_oversized_head_still_progresses() {
        let store = store();
        let big = send(&store, &"x\n\"y\"".repeat(2_000), true).await;
        let small = send(&store, "small", true).await;
        let offers = store
            .offer_turn_boundary(&primary(), TurnBoundaryEvent::Stop, None, Some(4096))
            .await
            .unwrap();
        // The head is too large to inline: it goes out as an id, and the
        // small request behind it is not starved.
        assert_eq!(ids(&offers), vec![big.clone(), small.clone()]);
        assert_eq!(offers[0]["offer"], "claim_to_read");
        assert!(offers[0].to_string().len() < 1024);
        assert_eq!(offers[1]["offer"], "claim_before_acting");
        let total: usize = offers.iter().map(|offer| offer.to_string().len()).sum();
        assert!(total <= 4096);
    }

    #[tokio::test]
    async fn concurrent_hooks_offer_each_request_once() {
        let store = store();
        let id = send(&store, "once", true).await;
        let recipient = primary();
        let (a, b) = tokio::join!(
            store.offer_turn_boundary(&recipient, TurnBoundaryEvent::Stop, None, None),
            store.offer_turn_boundary(&recipient, TurnBoundaryEvent::Stop, None, None),
        );
        let mut all = ids(&a.unwrap());
        all.extend(ids(&b.unwrap()));
        assert_eq!(all, vec![id]);
    }

    #[tokio::test]
    async fn a_cancelled_offer_cannot_be_claimed_or_offered_again() {
        let store = store();
        let id = send(&store, "changed my mind", true).await;
        assert_eq!(
            store
                .offer_turn_boundary(&primary(), TurnBoundaryEvent::Stop, None, None)
                .await
                .unwrap()
                .len(),
            1
        );
        store.cancel_for(&sender(), &id).await.unwrap();
        assert!(store.claim_by_id(&primary(), &id).await.is_err());
        age_offers(&store, OFFER_BACKOFF).await;
        assert!(store
            .offer_turn_boundary(&primary(), TurnBoundaryEvent::PromptSubmit, None, None)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn workers_and_other_sessions_get_no_offers() {
        let store = store();
        send(&store, "for the human", true).await;
        assert!(store
            .offer_turn_boundary(
                &participant("someone-else", "%3"),
                TurnBoundaryEvent::Stop,
                None,
                None
            )
            .await
            .unwrap()
            .is_empty());
        store
            .set_purpose(&primary(), LaunchPurpose::Worker)
            .await
            .unwrap();
        let mut worker = primary();
        worker.launch_purpose = LaunchPurpose::Worker;
        assert!(
            store
                .offer_turn_boundary(&worker, TurnBoundaryEvent::Stop, None, None)
                .await
                .unwrap()
                .is_empty(),
            "a worker takes the request through its ordinary wake"
        );
    }
}
