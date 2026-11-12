//! Everything pending on the person, in one ordered list, and what they
//! already decided.
//!
//! The queue joins three sources that each already exist: human requests
//! (an agent is blocked on an answer), agents waiting on input in their
//! pane, and open review items (a result waits to be looked at). Order is by
//! how much is blocked, then by age: human requests first, then waiting
//! agents (error, choice, input, asked question), then review items, oldest
//! first within each. Nothing here is inferred: each entry is a claim or a
//! state some other part of tend already records.

use crate::collaboration::{CollaborationRequest, HumanAction, RequestStatus};
use crate::decision::Decision;
use crate::review::ReviewItem;
use crate::state::Agent;
use crate::AgentState;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Daemons that serve `decision_queue` and `decision_history`.
pub const DECISION_QUEUE_CAPABILITY: &str = "decision_queue_v1";
/// Upper bound on one queue or history read.
pub const MAX_QUEUE_LIMIT: usize = 500;
const MAX_MESSAGE_CHARS: usize = 240;

/// One thing waiting on the person.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PendingItem {
    /// An agent asked the person through `tend_request_attention`.
    HumanRequest(HumanRequestEntry),
    /// An agent is blocked in its pane (prompt, choice, error, question).
    AgentWaiting(AgentWaitingEntry),
    /// A result someone asked to have reviewed.
    Review(Box<ReviewItem>),
}

impl PendingItem {
    /// When the wait began.
    #[must_use]
    pub fn since(&self) -> OffsetDateTime {
        match self {
            Self::HumanRequest(entry) => entry.created_at,
            Self::AgentWaiting(entry) => entry.since,
            Self::Review(item) => item.requested_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HumanRequestEntry {
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<HumanAction>,
    /// The question, as the agent wrote it.
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<Decision>,
    /// The asking agent's session, pane and alias.
    pub from_session_id: String,
    pub from_pane: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_id: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl HumanRequestEntry {
    #[must_use]
    pub fn from_request(request: &CollaborationRequest) -> Self {
        Self {
            request_id: request.id.clone(),
            action: request.human_action,
            body: request.body.clone(),
            decision: request.decision.clone(),
            from_session_id: request.from.agent_session_id.clone(),
            from_pane: request.from.pane.clone(),
            from_alias: request.from.alias.clone(),
            workspace_id: request.workspace_id.clone(),
            work_id: request.work_id.clone(),
            created_at: request.created_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentWaitingEntry {
    pub session_id: String,
    pub agent_kind: crate::AgentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux_socket: Option<String>,
    pub state: AgentState,
    /// The question the turn ended on, the notification, or the last prompt,
    /// first line, shortened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub since: OffsetDateTime,
}

/// The queue, bounded; `truncated` says a source had more than was read.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PendingQueue {
    pub items: Vec<PendingItem>,
    #[serde(default)]
    pub truncated: bool,
    /// Sources that could not be read (for example a review store that
    /// failed to open), so an empty section is never mistaken for none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unavailable: Vec<String>,
}

/// One past decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DecidedItem {
    /// A review item with its disposition and note.
    Review(Box<ReviewItem>),
    /// A human request and the answer it got.
    HumanAnswer(Box<HumanAnswerEntry>),
}

impl DecidedItem {
    /// When it was decided.
    #[must_use]
    pub fn decided_at(&self) -> OffsetDateTime {
        match self {
            Self::Review(item) => item
                .disposition
                .as_ref()
                .map_or(item.requested_at, |d| d.at),
            Self::HumanAnswer(entry) => entry.answered_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HumanAnswerEntry {
    #[serde(flatten)]
    pub request: HumanRequestEntry,
    /// `completed` for an answer; `declined`/`failed`/`blocked` when the
    /// operator said so.
    pub status: RequestStatus,
    pub answer: String,
    #[serde(with = "time::serde::rfc3339")]
    pub answered_at: OffsetDateTime,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DecisionHistory {
    /// Newest decision first.
    pub items: Vec<DecidedItem>,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unavailable: Vec<String>,
}

/// Does this agent row need the person: a blocking state, or an idle row
/// whose turn ended by asking them something. Same rule as `tend attend`.
#[must_use]
pub fn agent_needs_person(agent: &Agent) -> bool {
    blocking_rank(agent.state).is_some() || agent.awaiting_reply().is_some()
}

fn blocking_rank(state: AgentState) -> Option<u8> {
    match state {
        AgentState::Error => Some(0),
        AgentState::WaitingChoice => Some(1),
        AgentState::WaitingInput => Some(2),
        _ => None,
    }
}

fn agent_entry(agent: &Agent) -> AgentWaitingEntry {
    let asked = agent.awaiting_reply();
    let message = asked
        .map(|reply| reply.question.as_str())
        .or(agent.last_notification.as_deref())
        .or(agent.last_prompt.as_deref())
        .and_then(|text| text.lines().find(|line| !line.trim().is_empty()))
        .map(|line| {
            let line = line.trim();
            match line.char_indices().nth(MAX_MESSAGE_CHARS) {
                Some((cut, _)) => format!("{}…", &line[..cut]),
                None => line.to_owned(),
            }
        });
    let since = if blocking_rank(agent.state).is_some() {
        agent.state_entered_at
    } else {
        asked.map_or(agent.state_entered_at, |reply| reply.since)
    };
    AgentWaitingEntry {
        session_id: agent.session_id.clone(),
        agent_kind: agent.kind,
        pane: agent.pane.clone(),
        tmux_socket: agent.tmux_socket.clone(),
        state: agent.state,
        message,
        since,
    }
}

/// Join the three sources into one ordered, bounded queue. Inputs need not
/// be sorted; `human` and `reviews` should already be open-only.
#[must_use]
pub fn assemble_queue(
    human: &[CollaborationRequest],
    agents: &[Agent],
    reviews: Vec<ReviewItem>,
    limit: usize,
) -> PendingQueue {
    let mut human: Vec<&CollaborationRequest> = human
        .iter()
        .filter(|request| request.needs_human_response())
        .collect();
    human.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    let mut waiting: Vec<&Agent> = agents.iter().filter(|a| agent_needs_person(a)).collect();
    waiting.sort_by(|a, b| {
        let rank = |agent: &Agent| blocking_rank(agent.state).unwrap_or(3);
        rank(a)
            .cmp(&rank(b))
            .then(agent_entry(a).since.cmp(&agent_entry(b).since))
            .then(a.session_id.cmp(&b.session_id))
    });
    let mut reviews: Vec<ReviewItem> = reviews.into_iter().filter(ReviewItem::is_open).collect();
    reviews.sort_by(|a, b| a.requested_at.cmp(&b.requested_at).then(a.id.cmp(&b.id)));

    let total = human.len() + waiting.len() + reviews.len();
    let items: Vec<PendingItem> = human
        .into_iter()
        .map(|request| PendingItem::HumanRequest(HumanRequestEntry::from_request(request)))
        .chain(
            waiting
                .into_iter()
                .map(|agent| PendingItem::AgentWaiting(agent_entry(agent))),
        )
        .chain(
            reviews
                .into_iter()
                .map(|item| PendingItem::Review(Box::new(item))),
        )
        .take(limit)
        .collect();
    PendingQueue {
        truncated: total > items.len(),
        items,
        unavailable: Vec::new(),
    }
}

/// Join decided review items and answered human requests, newest first.
#[must_use]
pub fn assemble_history(
    reviews: Vec<ReviewItem>,
    answered: &[CollaborationRequest],
    limit: usize,
) -> DecisionHistory {
    let mut items: Vec<DecidedItem> = reviews
        .into_iter()
        .filter(|item| !item.is_open())
        .map(|item| DecidedItem::Review(Box::new(item)))
        .chain(answered.iter().filter_map(|request| {
            let reply = request.reply.as_ref()?;
            Some(DecidedItem::HumanAnswer(Box::new(HumanAnswerEntry {
                request: HumanRequestEntry::from_request(request),
                status: reply.status,
                answer: reply.body.clone(),
                answered_at: reply.at,
            })))
        }))
        .collect();
    items.sort_by_key(|item| std::cmp::Reverse(item.decided_at()));
    let truncated = items.len() > limit;
    items.truncate(limit);
    DecisionHistory {
        items,
        truncated,
        unavailable: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::{ActorKind, DispositionKind, ReviewActor, ReviewDisposition, ReviewTarget};
    use time::macros::datetime;

    fn agent(session: &str, state: &str, at: &str) -> Agent {
        serde_json::from_value(serde_json::json!({
            "kind": "codex", "session_id": session, "pane": format!("%{session}"),
            "state": state, "state_entered_at": at, "started_at": at, "updated_at": at, "last_activity_at": at, "last_notification": "Proceed?\nmore",
        }))
        .unwrap()
    }

    fn review(id: &str, at: OffsetDateTime, decided: Option<OffsetDateTime>) -> ReviewItem {
        ReviewItem {
            id: id.into(),
            target: ReviewTarget::Session {
                provider: None,
                session_id: "s".into(),
            },
            generation: 1,
            summary: "check".into(),
            anchor: None,
            idempotency_key: None,
            requested_at: at,
            requested_by: ReviewActor {
                kind: ActorKind::Agent,
                label: None,
            },
            decision: None,
            disposition: decided.map(|at| ReviewDisposition {
                kind: DispositionKind::Accepted,
                by: ReviewActor {
                    kind: ActorKind::User,
                    label: None,
                },
                at,
                note: Some("fine".into()),
            }),
        }
    }

    fn human(id: &str, at: &str, answered: Option<&str>) -> CollaborationRequest {
        let mut value = serde_json::json!({
            "id": id, "kind": "question", "body": "Merge?", "expects_reply": true,
            "work_mode": "read_only", "status": if answered.is_some() { "completed" } else { "queued" },
            "created_at": at, "human_action": "choice",
            "from": {"agent_kind":"codex","agent_session_id":"a","pane":"%1",
                     "room":{"host":"tmux","window_id":"@1"},"state":"idle","launch_purpose":"worker"},
            "to": {"agent_kind":"codex","agent_session_id":"console","pane":"console","console":true,
                   "room":{"host":"tmux","window_id":"@1"},"state":"idle","launch_purpose":"worker"},
        });
        if let Some(answered) = answered {
            value["reply"] =
                serde_json::json!({"status":"completed","body":"merge it","at":answered});
        }
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn queue_orders_blocked_humans_then_agents_then_reviews() {
        let agents = [
            agent("2", "waiting_input", "2026-11-12T10:00:00Z"),
            agent("1", "error", "2026-11-12T11:00:00Z"),
            agent("3", "working", "2026-11-12T09:00:00Z"),
        ];
        let humans = [
            human("rq_new", "2026-11-12T11:30:00Z", None),
            human("rq_old", "2026-11-12T08:00:00Z", None),
            human(
                "rq_done",
                "2026-11-12T07:00:00Z",
                Some("2026-11-12T07:05:00Z"),
            ),
        ];
        let reviews = vec![
            review("rv_b", datetime!(2026-11-12 09:00 UTC), None),
            review("rv_a", datetime!(2026-11-12 06:00 UTC), None),
            review(
                "rv_x",
                datetime!(2026-11-12 05:00 UTC),
                Some(datetime!(2026-11-12 05:10 UTC)),
            ),
        ];
        let queue = assemble_queue(&humans, &agents, reviews, 50);
        let order: Vec<String> = queue
            .items
            .iter()
            .map(|item| match item {
                PendingItem::HumanRequest(h) => h.request_id.clone(),
                PendingItem::AgentWaiting(a) => a.session_id.clone(),
                PendingItem::Review(r) => r.id.clone(),
            })
            .collect();
        assert_eq!(order, ["rq_old", "rq_new", "1", "2", "rv_a", "rv_b"]);
        assert!(!queue.truncated);
        let PendingItem::AgentWaiting(first) = &queue.items[2] else {
            panic!()
        };
        assert_eq!(first.message.as_deref(), Some("Proceed?"));
        let short = assemble_queue(&humans, &agents, Vec::new(), 3);
        assert_eq!(short.items.len(), 3);
        assert!(short.truncated);
    }

    #[test]
    fn history_joins_dispositions_and_answers_newest_first() {
        let history = assemble_history(
            vec![
                review("rv_open", datetime!(2026-11-12 05:00 UTC), None),
                review(
                    "rv_old",
                    datetime!(2026-11-11 05:00 UTC),
                    Some(datetime!(2026-11-11 06:00 UTC)),
                ),
            ],
            &[human(
                "rq_1",
                "2026-11-12T07:00:00Z",
                Some("2026-11-12T07:05:00Z"),
            )],
            10,
        );
        assert_eq!(history.items.len(), 2, "open items are not history");
        let DecidedItem::HumanAnswer(answer) = &history.items[0] else {
            panic!("newest first")
        };
        assert_eq!(answer.answer, "merge it");
        assert_eq!(answer.request.request_id, "rq_1");
        let json = serde_json::to_value(&history.items[0]).unwrap();
        assert_eq!(json["kind"], "human_answer");
        assert_eq!(json["request_id"], "rq_1");
        assert!(matches!(&history.items[1], DecidedItem::Review(r) if r.id == "rv_old"));
    }
}
