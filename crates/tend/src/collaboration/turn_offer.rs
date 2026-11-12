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
//!
//! The same Stop boundary also carries *reply reminders*: a recipient that
//! claimed a request expecting a reply and ends its turn without replying is
//! reminded of that request id, at most [`MAX_REPLY_REMINDERS`] times and once
//! per [`OFFER_BACKOFF`]. If it then stays Idle, peer-interruption
//! reconciliation raises `reply_overdue` (see `recovery.rs`) so the stall
//! reaches the sender and, failing that, a human.
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
/// Stop continuations reminding a recipient to reply to one claimed request.
pub const MAX_REPLY_REMINDERS: u32 = MAX_STOP_OFFERS;
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
    /// A session started, resumed or (Claude Code) compacted: it is told
    /// what it owes and awaits (`obligations.rs`). Nothing is offered or
    /// recorded.
    SessionStart,
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
    /// Stop continuations reminding the claimant to reply (claimed requests).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reply_reminders: u32,
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub last_reply_reminder_at: Option<OffsetDateTime>,
    /// The latest Stop boundary the claimant reached without having replied.
    /// Cleared when it is seen working again, so `reply_overdue` is judged
    /// only from a Stop of the current idle episode.
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub unanswered_stop_at: Option<OffsetDateTime>,
    /// `hook_transport = "primary"`: turn-boundary notices that this
    /// request's reply is ready, offered to its sender.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reply_offers: u32,
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub last_reply_offer_at: Option<OffsetDateTime>,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde's skip_serializing_if signature
fn is_zero(value: &u32) -> bool {
    *value == 0
}

impl TurnOffer {
    fn reply_offer_eligible(&self, event: TurnBoundaryEvent, now: OffsetDateTime) -> bool {
        let fresh = self
            .last_reply_offer_at
            .is_none_or(|at| now - at >= OFFER_BACKOFF);
        match event {
            TurnBoundaryEvent::Stop => self.reply_offers < MAX_STOP_OFFERS && fresh,
            TurnBoundaryEvent::PromptSubmit => fresh,
        }
    }

    /// Offered at a turn boundary within the backoff: the session's own
    /// hook is carrying it, so the waker does not also type a wake for it.
    pub fn offered_recently(&self, now: OffsetDateTime) -> bool {
        let recent = |at: Option<OffsetDateTime>| at.is_some_and(|at| now - at < OFFER_BACKOFF);
        recent(self.last_stop_offer_at) || recent(self.last_prompt_offer_at)
    }

    /// The reply notice was offered within the backoff.
    pub fn reply_offered_recently(&self, now: OffsetDateTime) -> bool {
        self.last_reply_offer_at
            .is_some_and(|at| now - at < OFFER_BACKOFF)
    }

    fn reminder_eligible(&self, now: OffsetDateTime) -> bool {
        self.reply_reminders < MAX_REPLY_REMINDERS
            && self
                .last_reply_reminder_at
                .is_none_or(|at| now - at >= OFFER_BACKOFF)
    }

    /// At least one reminder was offered, and the claimant's latest Stop
    /// without a reply is a full backoff old and still the current episode.
    pub fn reminder_overdue(&self, now: OffsetDateTime) -> bool {
        self.reply_reminders > 0
            && self
                .unanswered_stop_at
                .is_some_and(|at| now - at >= OFFER_BACKOFF)
    }

    fn eligible(&self, event: TurnBoundaryEvent, now: OffsetDateTime) -> bool {
        let recent = |at: Option<OffsetDateTime>| at.is_some_and(|at| now - at < OFFER_BACKOFF);
        match event {
            TurnBoundaryEvent::Stop => {
                self.stop_offers < MAX_STOP_OFFERS && !recent(self.last_stop_offer_at)
            }
            // A turn the user started may surface work a lost Stop offer
            // missed; only its own recent offer holds it back.
            TurnBoundaryEvent::PromptSubmit => !recent(self.last_prompt_offer_at),
            TurnBoundaryEvent::SessionStart => false,
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
            TurnBoundaryEvent::SessionStart => {}
        }
    }
}

impl CollaborationStore {
    /// Whether a Stop hook may continue a turn with an offer. False when
    /// collaboration wakes are disabled (`wake = "never"`).
    pub fn set_turn_continuation(&self, enabled: bool) {
        self.turn_continuation.store(enabled, Ordering::SeqCst);
    }

    /// `collaboration.hook_transport = "primary"`: workers are offered
    /// queued requests, and senders their ready replies, at their own turn
    /// boundaries; typed wakes are left for agents idle with nothing
    /// offered recently.
    pub fn set_hook_transport(&self, enabled: bool) {
        self.hook_transport.store(enabled, Ordering::SeqCst);
    }

    pub fn hook_transport_enabled(&self) -> bool {
        self.hook_transport.load(Ordering::SeqCst)
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
        if event == TurnBoundaryEvent::SessionStart {
            return Ok(self.obligations(caller).await.into_iter().collect());
        }
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
        let hook_transport = self.hook_transport_enabled();
        let mut candidates = self
            .queued_offer_candidates(caller, event, now, hook_transport)
            .await;
        candidates.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));

        let mut offers = Vec::new();
        let mut rows = Vec::new();
        let mut used = 0usize;
        if event == TurnBoundaryEvent::Stop {
            for mut request in self.reply_owing_requests(caller).await {
                let offer = request.turn_offer.get_or_insert_with(TurnOffer::default);
                // Every Stop while the reply is owed is recorded, reminded or
                // not: it is the deterministic boundary `reply_overdue` needs.
                offer.unanswered_stop_at = Some(now);
                if offers.len() < max_requests && offer.reminder_eligible(now) {
                    let full = reminder_payload(&request, true);
                    let payload = if used + full.to_string().len() <= max_bytes {
                        Some(full)
                    } else if offers.is_empty() {
                        Some(reminder_payload(&request, false))
                    } else {
                        None
                    };
                    if let Some(payload) = payload {
                        used += payload.to_string().len();
                        let offer = request.turn_offer.get_or_insert_with(TurnOffer::default);
                        offer.reply_reminders += 1;
                        offer.last_reply_reminder_at = Some(now);
                        offers.push(payload);
                    }
                }
                rows.push(request);
            }
        }
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
        if hook_transport {
            let budget = (max_requests, max_bytes, used);
            self.offer_ready_replies(caller, event, now, budget, &mut offers, &mut rows)
                .await;
        }
        if !rows.is_empty() {
            self.commit_request_rows(&rows).await?;
            self.publish_change();
        }
        Ok(offers)
    }

    /// Queued requests `caller` may be offered at this boundary.
    async fn queued_offer_candidates(
        &self,
        caller: &Participant,
        event: TurnBoundaryEvent,
        now: OffsetDateTime,
        hook_transport: bool,
    ) -> Vec<super::CollaborationRequest> {
        let requests = self.requests.read().await;
        requests
            .values()
            .filter(|request| {
                request.status == RequestStatus::Queued
                    && addresses(&request.to, caller)
                    // Judged against the session as it is now.
                    && (request.awaits_turn_boundary(caller)
                        // Hook transport: a worker takes its queued
                        // requests at its own turn boundary too.
                        || (hook_transport
                            && caller.accepts_peer_work()
                            && request.wake_delivery.is_none())
                        // A wake that was typed but never submitted also
                        // rides along with whatever prompt is submitted
                        // next, as a structured offer.
                        || (event == TurnBoundaryEvent::PromptSubmit
                            && request.wake_retry.as_ref().is_some_and(|retry| {
                                retry.redeliver
                                    || retry.attempts >= super::wake_retry::MAX_WAKE_ATTEMPTS
                            })))
                    && (event == TurnBoundaryEvent::PromptSubmit || request.should_notify())
                    && request
                        .turn_offer
                        .as_ref()
                        .is_none_or(|offer| offer.eligible(event, now))
            })
            .cloned()
            .collect()
    }

    /// Add `reply_ready` notices for `caller` within the remaining budget
    /// (`max_requests`, `max_bytes`, bytes already used), recording each.
    async fn offer_ready_replies(
        &self,
        caller: &Participant,
        event: TurnBoundaryEvent,
        now: OffsetDateTime,
        (max_requests, max_bytes, mut used): (usize, usize, usize),
        offers: &mut Vec<Value>,
        rows: &mut Vec<super::CollaborationRequest>,
    ) {
        for mut request in self.ready_replies_for(caller, event, now).await {
            if offers.len() >= max_requests {
                break;
            }
            let payload = reply_ready_payload(&request);
            let len = payload.to_string().len();
            if used + len > max_bytes && !offers.is_empty() {
                break;
            }
            used += len;
            let offer = request.turn_offer.get_or_insert_with(TurnOffer::default);
            offer.reply_offers += 1;
            offer.last_reply_offer_at = Some(now);
            offers.push(payload);
            rows.push(request);
        }
    }

    /// Replies owed a wake to `caller`, oldest first, not mid-wake and not
    /// offered within the backoff.
    async fn ready_replies_for(
        &self,
        caller: &Participant,
        event: TurnBoundaryEvent,
        now: OffsetDateTime,
    ) -> Vec<super::CollaborationRequest> {
        let requests = self.requests.read().await;
        let mut ready: Vec<_> = requests
            .values()
            .filter(|request| {
                request.from.same_session(caller)
                    && request.reply_wake_owed()
                    && request
                        .turn_offer
                        .as_ref()
                        .is_none_or(|offer| offer.reply_offer_eligible(event, now))
            })
            .cloned()
            .collect();
        ready.sort_by_key(|request| request.reply.as_ref().map(|reply| reply.at));
        ready
    }
}

impl CollaborationStore {
    /// Claimed requests `caller` owes a reply to, oldest claim first.
    /// Recipients that are waiting on a request of their own are left alone:
    /// ending a turn to wait for that reply's wake is the intended pattern,
    /// not a stall.
    async fn reply_owing_requests(&self, caller: &Participant) -> Vec<super::CollaborationRequest> {
        let requests = self.requests.read().await;
        let mut due: Vec<_> = requests
            .values()
            .filter(|request| {
                owes_reply(request, caller)
                    && !waits_on_own_request(requests.values(), caller, request.claimed_at)
            })
            .cloned()
            .collect();
        due.sort_by(|a, b| (a.claimed_at, &a.id).cmp(&(b.claimed_at, &b.id)));
        due
    }
}

/// `caller` claimed `request`, which still expects a reply, and is a
/// recipient that may be reminded: a worker, or a primary taking work the user
/// authorized for it. A human's own claim in their primary is left to them.
pub(super) fn owes_reply(request: &super::CollaborationRequest, caller: &Participant) -> bool {
    request.status == RequestStatus::Claimed
        && request.expects_reply
        && request.reply.is_none()
        && !request.to.console
        && !request.peer_interrupted()
        && addresses(&request.to, caller)
        && (caller.accepts_peer_work() || request.awaits_turn_boundary(caller))
}

/// Whether `participant` has a request of its own, sent at or after `since`,
/// whose reply it is still waiting for: open and expecting a reply, or
/// answered with its reply wake not yet delivered.
pub(super) fn waits_on_own_request<'a>(
    requests: impl IntoIterator<Item = &'a super::CollaborationRequest>,
    participant: &Participant,
    since: Option<OffsetDateTime>,
) -> bool {
    requests.into_iter().any(|other| {
        other.from.same_session(participant)
            && other.expects_reply
            && since.is_none_or(|since| other.created_at >= since)
            && (!other.status.is_terminal()
                || other.reply_wake_owed()
                || matches!(other.reply_wake, Some(super::ReplyWake::Reserved { .. })))
    })
}

fn reminder_payload(request: &super::CollaborationRequest, full: bool) -> Value {
    if !full {
        return json!({"request_id": request.id, "offer": "reply_reminder"});
    }
    json!({
        "request_id": request.id,
        "from": request.from.label(),
        "kind": request.kind,
        "offer": "reply_reminder",
        "note": "you claimed this request and ended your turn without replying",
    })
}

fn reply_ready_payload(request: &super::CollaborationRequest) -> Value {
    json!({
        "request_id": request.id,
        "from": request.to.label(),
        "status": request.reply.as_ref().map(|reply| reply.status),
        "offer": "reply_ready",
    })
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
                offer.last_reply_reminder_at = offer.last_reply_reminder_at.map(|at| at - by);
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
        let after_claim = store
            .offer_turn_boundary(&primary(), TurnBoundaryEvent::Stop, None, None)
            .await
            .unwrap();
        // No second offer; only a reminder that the claimed request owes a reply.
        assert_eq!(ids(&after_claim), vec![id]);
        assert_eq!(after_claim[0]["offer"], "reply_reminder");
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

    fn worker() -> Participant {
        let mut worker = participant("worker", "%4");
        worker.launch_purpose = LaunchPurpose::Worker;
        worker
    }

    async fn claimed_by_worker(store: &CollaborationStore, body: &str) -> String {
        let id = store
            .create(
                sender(),
                worker(),
                NewRequest {
                    body: body.into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .id;
        store.claim_by_id(&worker(), &id).await.unwrap();
        id
    }

    async fn stop(store: &CollaborationStore, caller: &Participant) -> Vec<Value> {
        store
            .offer_turn_boundary(caller, TurnBoundaryEvent::Stop, None, None)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_claimed_request_left_unanswered_at_stop_is_reminded_with_backoff_and_a_cap() {
        let store = store();
        let id = claimed_by_worker(&store, "review the diff").await;
        // The stall: the worker claimed the request and ended its turn.
        let reminders = stop(&store, &worker()).await;
        assert_eq!(ids(&reminders), vec![id.clone()]);
        assert_eq!(reminders[0]["offer"], "reply_reminder");
        let row = store.peek_for(&worker(), &id).await.unwrap();
        assert_eq!(
            row.status,
            RequestStatus::Claimed,
            "a reminder changes no status"
        );
        let offer = row.turn_offer.unwrap();
        assert_eq!(offer.reply_reminders, 1);
        assert_eq!(offer.stop_offers, 0, "reminders keep their own counter");
        // Once per backoff.
        assert!(stop(&store, &worker()).await.is_empty());
        for _ in 1..MAX_REPLY_REMINDERS {
            age_offers(&store, OFFER_BACKOFF).await;
            assert_eq!(ids(&stop(&store, &worker()).await), vec![id.clone()]);
        }
        age_offers(&store, OFFER_BACKOFF).await;
        assert!(
            stop(&store, &worker()).await.is_empty(),
            "an ignored reminder cannot loop the turn"
        );
        // A user's own turn is not interrupted with reminders.
        assert!(store
            .offer_turn_boundary(&worker(), TurnBoundaryEvent::PromptSubmit, None, None)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn replies_waits_on_own_requests_and_human_claims_get_no_reminder() {
        let store = store();
        let id = claimed_by_worker(&store, "answered").await;
        store
            .reply(
                &worker(),
                &id,
                RequestStatus::Completed,
                "done".into(),
                vec![],
            )
            .await
            .unwrap();
        assert!(stop(&store, &worker()).await.is_empty());

        // Waiting for its own delegated request's reply wake is not a stall.
        let id = claimed_by_worker(&store, "needs a sub-review").await;
        let sub = store
            .create(
                worker(),
                participant("reviewer", "%5"),
                NewRequest {
                    body: "sub-review".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(stop(&store, &worker()).await.is_empty());
        store.cancel_for(&worker(), &sub.id).await.unwrap();
        assert_eq!(ids(&stop(&store, &worker()).await), vec![id]);

        // A human accepted this by id in their own primary; they drive it.
        let store = self::store();
        let id = send(&store, "manual", false).await;
        store.claim_by_id(&primary(), &id).await.unwrap();
        assert!(stop(&store, &primary()).await.is_empty());
        // User-authorized work handed to the primary's model is reminded.
        let id = send(&store, "authorized", true).await;
        store.claim_by_id(&primary(), &id).await.unwrap();
        assert_eq!(ids(&stop(&store, &primary()).await), vec![id]);
    }

    #[tokio::test]
    async fn reminders_share_the_byte_budget_and_an_oversized_head_is_compacted() {
        let store = store();
        // A sender label too large for any full reminder within 1 KB.
        let mut loud = sender();
        loud.alias = Some("a".repeat(2_000));
        let mut claimed = Vec::new();
        for body in ["one", "two"] {
            let id = store
                .create(
                    loud.clone(),
                    worker(),
                    NewRequest {
                        body: body.into(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap()
                .id;
            store.claim_by_id(&worker(), &id).await.unwrap();
            claimed.push(id);
        }
        let offers = store
            .offer_turn_boundary(&worker(), TurnBoundaryEvent::Stop, None, Some(1024))
            .await
            .unwrap();
        let total: usize = offers.iter().map(|offer| offer.to_string().len()).sum();
        assert!(total <= 1024, "{total}");
        // The head goes out compact; the next waits for a later Stop.
        assert_eq!(ids(&offers), vec![claimed[0].clone()]);
        assert!(offers[0].get("from").is_none());
        // Both claims are recorded as unanswered at this Stop either way.
        for id in &claimed {
            let row = store.peek_for(&worker(), id).await.unwrap();
            assert!(row.turn_offer.unwrap().unanswered_stop_at.is_some());
        }
        let second = store.peek_for(&worker(), &claimed[1]).await.unwrap();
        assert_eq!(second.turn_offer.unwrap().reply_reminders, 0);
        // Mixed with a queued offer to a primary, reminders count first and
        // the offer behind them stays within the same budget.
        let store = self::store();
        let claimed = send(&store, "authorized and claimed", true).await;
        store.claim_by_id(&primary(), &claimed).await.unwrap();
        let queued = send(&store, &"x".repeat(900), true).await;
        let offers = store
            .offer_turn_boundary(&primary(), TurnBoundaryEvent::Stop, None, Some(1024))
            .await
            .unwrap();
        let total: usize = offers.iter().map(|offer| offer.to_string().len()).sum();
        assert!(total <= 1024, "{total}");
        assert_eq!(ids(&offers), vec![claimed.clone()]);
        assert_eq!(offers[0]["offer"], "reply_reminder");
        let row = store.peek_for(&primary(), &queued).await.unwrap();
        assert!(
            row.turn_offer.is_none(),
            "an offer cut by the budget is not recorded"
        );
    }

    #[tokio::test]
    async fn hook_transport_offers_a_workers_queued_requests_and_holds_the_typed_wake() {
        let store = store();
        let id = store
            .create(
                sender(),
                worker(),
                NewRequest {
                    body: "review".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .id;
        // Off (the default): a worker's request waits for its typed wake.
        assert!(stop(&store, &worker()).await.is_empty());
        store.set_hook_transport(true);
        let offers = stop(&store, &worker()).await;
        assert_eq!(ids(&offers), vec![id.clone()]);
        assert_eq!(offers[0]["offer"], "claim_before_acting");
        let row = store.peek_for(&worker(), &id).await.unwrap();
        assert_eq!(row.status, RequestStatus::Queued, "an offer claims nothing");
        let now = OffsetDateTime::now_utc();
        assert!(row.turn_offer.as_ref().unwrap().offered_recently(now));
        assert!(!row
            .turn_offer
            .unwrap()
            .offered_recently(now + OFFER_BACKOFF));
    }

    #[tokio::test]
    async fn hook_transport_tells_a_sender_its_reply_is_ready_and_holds_the_reply_wake() {
        let store = store();
        store.set_hook_transport(true);
        let reviewer = participant("reviewer", "%6");
        let mut reviewer = reviewer;
        reviewer.launch_purpose = LaunchPurpose::Worker;
        let id = store
            .create(
                worker(),
                reviewer.clone(),
                NewRequest {
                    body: "review".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .id;
        store.claim_by_id(&reviewer, &id).await.unwrap();
        store
            .reply(
                &reviewer,
                &id,
                RequestStatus::Completed,
                "lgtm".into(),
                vec![],
            )
            .await
            .unwrap();
        let offers = stop(&store, &worker()).await;
        assert_eq!(ids(&offers), vec![id.clone()]);
        assert_eq!(offers[0]["offer"], "reply_ready");
        assert_eq!(offers[0]["status"], "completed");
        // The typed reply wake waits out the backoff...
        assert!(store
            .reserve_reply_wakes(&worker())
            .await
            .unwrap()
            .is_none());
        assert!(stop(&store, &worker()).await.is_empty(), "once per backoff");
        // ...and the reply stays owed (an offer is not a read).
        assert!(store
            .peek_for(&worker(), &id)
            .await
            .unwrap()
            .reply_wake_owed());
        age_offers(&store, OFFER_BACKOFF).await;
        {
            let mut requests = store.requests.write().await;
            let offer = requests.get_mut(&id).unwrap().turn_offer.as_mut().unwrap();
            offer.last_reply_offer_at = offer.last_reply_offer_at.map(|at| at - OFFER_BACKOFF);
        }
        assert!(store
            .reserve_reply_wakes(&worker())
            .await
            .unwrap()
            .is_some());
    }
}
