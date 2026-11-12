//! Operator diagnostics over the existing memory projection; never copies bodies.
use super::{
    CollaborationError, CollaborationRequest, CollaborationStore, RequestKind, RequestStatus,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

pub const COLLABORATION_HEALTH_CAPABILITY: &str = "collaboration_health_v1";
/// v2 uses a distinct operation name, so a downgrade between connections
/// cannot make an old daemon ignore the filters and perform broad expiry.
pub const FILTERED_EXPIRE_CAPABILITY: &str = "collaboration_expire_filtered_v2";

pub const EXPIRE_SUMMARY_CAPABILITY: &str = "collaboration_expire_summary_v1";

/// Bounded independently of mailbox size and imported request body/ID sizes.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ExpireSummary {
    pub total: usize,
    pub requests: Vec<ExpireSample>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpireSample {
    pub id: String,
    pub kind: RequestKind,
    pub from: String,
    pub to: String,
    /// Calendar date, without the time of day.
    pub created_at: String,
}

impl ExpireSummary {
    pub fn from_requests(requests: &[CollaborationRequest]) -> Self {
        let samples: Vec<_> = requests
            .iter()
            .filter(|request| request.id.len() <= 128)
            .take(100)
            .map(|request| ExpireSample {
                id: request.id.clone(),
                kind: request.kind,
                from: request.from.label().chars().take(64).collect(),
                to: request.to.label().chars().take(64).collect(),
                created_at: request.created_at.date().to_string(),
            })
            .collect();
        Self {
            total: requests.len(),
            truncated: samples.len() < requests.len(),
            requests: samples,
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CollaborationHealth {
    #[serde(default)]
    pub synthetic_agents: crate::agent_directory::SyntheticDiagnostics,
    pub interrupted_requests: usize,
    pub binding_unavailable: usize,
    pub pending_recovery_decisions: usize,
    pub example_request_ids: Vec<String>,
    pub stale_queued: usize,
    pub stale_quiet_notices: usize,
    pub example_quiet_notice_ids: Vec<String>,
    /// How agent requests that expected a reply ended, per 7-day bucket
    /// back from now (newest first), so a delivery default can be judged on
    /// evidence. Absent from older daemons.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub weekly_outcomes: Vec<WeeklyOutcome>,
}

/// Outcomes of the agent-to-agent requests created in one 7-day bucket.
/// Counts exact original requests only: human items, Tend's own service
/// requests and notices are excluded.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WeeklyOutcome {
    /// First day of the bucket (UTC calendar date).
    pub since: String,
    pub requests: usize,
    /// Finished with a reply (any reported status).
    pub replied: usize,
    pub expired: usize,
    /// Still open or cancelled, not replied.
    pub open: usize,
    /// The latest recorded interruption reason, by reason.
    pub reply_overdue: usize,
    pub wake_unsubmitted: usize,
    pub other_interruptions: usize,
}

/// Buckets reported by [`CollaborationStore::health`].
pub const OUTCOME_WEEKS: i64 = 4;

fn weekly_outcomes<'a>(
    requests: impl Iterator<Item = &'a CollaborationRequest>,
    now: OffsetDateTime,
) -> Vec<WeeklyOutcome> {
    let mut weeks: Vec<WeeklyOutcome> = (0..OUTCOME_WEEKS)
        .map(|week| WeeklyOutcome {
            since: (now - time::Duration::days(7 * (week + 1)))
                .date()
                .to_string(),
            ..WeeklyOutcome::default()
        })
        .collect();
    for request in requests {
        if !request.expects_reply
            || request.kind == RequestKind::Notice
            || request.to.console
            || request.human_action.is_some()
            || request.from.agent_session_id.starts_with("tend:")
        {
            continue;
        }
        let age = now - request.created_at;
        if age.is_negative() {
            continue;
        }
        let Ok(week) = usize::try_from(age.whole_days() / 7) else {
            continue;
        };
        let Some(bucket) = weeks.get_mut(week) else {
            continue;
        };
        bucket.requests += 1;
        if request.reply.is_some() {
            bucket.replied += 1;
        } else if request.status == RequestStatus::Expired {
            bucket.expired += 1;
        } else {
            bucket.open += 1;
        }
        match request
            .interruption
            .as_ref()
            .map(|info| info.reason.as_str())
        {
            Some("reply_overdue") => bucket.reply_overdue += 1,
            Some("wake_unsubmitted") => bucket.wake_unsubmitted += 1,
            Some(_) => bucket.other_interruptions += 1,
            None => {}
        }
    }
    weeks
}

impl CollaborationRequest {
    pub fn is_undelivered_quiet_notice(&self) -> bool {
        self.kind == RequestKind::Notice
            && !self.expects_reply
            && !self.should_notify()
            && self.human_action.is_none()
            && self.notified_at.is_none()
            && self.wake_delivery.is_none()
    }
}

fn sample(ids: &mut Vec<(OffsetDateTime, String)>, request: &CollaborationRequest) {
    // Imported legacy IDs need not have the generated ID's small size. Keep
    // counts exact but omit oversized identifiers from the bounded sample.
    if request.id.len() > 128 {
        return;
    }
    // Deterministic and bounded even for a large mailbox.
    ids.push((request.created_at, request.id.clone()));
    ids.sort_unstable();
    ids.truncate(3);
}

impl CollaborationStore {
    pub async fn health(&self) -> Result<CollaborationHealth, CollaborationError> {
        self.ensure_enabled()?;
        let requests = self.requests.read().await;
        let cutoff = OffsetDateTime::now_utc() - time::Duration::days(1);
        let mut health = CollaborationHealth::default();
        let mut interrupted_examples = Vec::new();
        let mut quiet_examples = Vec::new();
        for request in requests.values() {
            health.pending_recovery_decisions += usize::from(
                request.from.agent_session_id == "tend:peer-recovery"
                    && request.needs_human_response(),
            );
            // Human recovery records mirror the parent's interruption; only
            // the exact original request owns an interruption episode.
            if request.peer_interrupted() {
                health.interrupted_requests += 1;
                let info = request.interruption.as_ref().expect("active interruption");
                health.binding_unavailable += usize::from(info.reason == "binding_unavailable");
                sample(&mut interrupted_examples, request);
            }
            if request.status == RequestStatus::Queued
                && request.created_at < cutoff
                && !request.needs_human_response()
            {
                health.stale_queued += 1;
                if request.is_undelivered_quiet_notice() {
                    health.stale_quiet_notices += 1;
                    sample(&mut quiet_examples, request);
                }
            }
        }
        health.weekly_outcomes = weekly_outcomes(requests.values(), OffsetDateTime::now_utc());
        health.example_request_ids = interrupted_examples.into_iter().map(|(_, id)| id).collect();
        health.example_quiet_notice_ids = quiet_examples.into_iter().map(|(_, id)| id).collect();
        Ok(health)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collaboration::{
        CollaborationOptions, HumanAction, NewRequest, Participant, PeerInterruption,
    };

    fn participant(session: &str) -> Participant {
        serde_json::from_value(
            serde_json::json!({"agent_kind":"codex","agent_session_id":session,
            "pane":"%1","room":{"host":"tmux","window_id":"@1"},"state":"idle"}),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn expiry_summary_omits_oversized_imported_ids_and_bounds_escaped_ids() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let mut request = store
            .create(
                participant("a"),
                participant("b"),
                NewRequest {
                    body: "hello".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        request.id = "x".repeat(1_000_000);
        let summary = ExpireSummary::from_requests(std::slice::from_ref(&request));
        assert_eq!(summary.total, 1);
        assert!(summary.requests.is_empty());
        assert!(summary.truncated);
        request.id = "\u{0000}".repeat(128);
        request.from.alias = Some("한".repeat(1000));
        request.to.alias = Some("x".repeat(1000));
        let requests = vec![request; 1000];
        let summary = ExpireSummary::from_requests(&requests);
        assert_eq!(summary.total, 1000);
        assert_eq!(summary.requests.len(), 100);
        assert_eq!(summary.requests[0].from.chars().count(), 64);
        assert_eq!(summary.requests[0].to.chars().count(), 64);
        assert_eq!(
            summary.requests[0].created_at,
            requests[0].created_at.date().to_string()
        );
        assert!(serde_json::to_vec(&summary).unwrap().len() < 150_000);
    }

    #[tokio::test]
    async fn health_counts_originals_not_recovery_copies_without_reading_or_mutating_bodies() {
        let store = CollaborationStore::in_memory(CollaborationOptions {
            max_message_bytes: 100_000,
            ..Default::default()
        });
        for index in 0..6 {
            let mut parent = store
                .create(
                    participant("a"),
                    participant("b"),
                    NewRequest {
                        body: format!("PRIVATE {}", "x".repeat(50_000)),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            parent.status = if index == 5 {
                RequestStatus::Completed
            } else {
                RequestStatus::Claimed
            };
            parent.interruption = Some(PeerInterruption {
                request_id: parent.id.clone(),
                reason: if index == 4 {
                    "stopped"
                } else {
                    "binding_unavailable"
                }
                .into(),
                active: true,
                observed_at: parent.created_at,
                reset_at: None,
                action_request_id: Some(format!("action-{index}")),
                decision: None,
            });
            let mut action = parent.clone();
            action.id = format!("action-{index}");
            action.parent_request_id = Some(parent.id.clone());
            action.from.agent_session_id = "tend:peer-recovery".into();
            action.to = Participant::console(parent.from.room.clone());
            action.kind = RequestKind::Question;
            action.human_action = Some(HumanAction::Choice);
            action.status = if index < 3 {
                RequestStatus::Queued
            } else {
                RequestStatus::Completed
            };
            let mut rows = store.requests.write().await;
            rows.insert(parent.id.clone(), parent);
            rows.insert(action.id.clone(), action);
        }
        let before = store.requests.read().await.clone();
        let changes = store.subscribe();
        let health = store.health().await.unwrap();
        assert_eq!(health.interrupted_requests, 5);
        assert_eq!(health.binding_unavailable, 4);
        assert_eq!(health.pending_recovery_decisions, 3);
        assert_eq!(health.example_request_ids.len(), 3);
        let serialized = serde_json::to_string(&health).unwrap();
        assert!(!serialized.contains("PRIVATE"));
        assert!(serialized.len() < 1500);
        assert_eq!(*store.requests.read().await, before);
        assert!(!changes.has_changed().unwrap());
    }

    #[tokio::test]
    async fn selective_expiry_rechecks_id_age_delivery_claim_and_notice_intent() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let mut ids = Vec::new();
        for index in 0..9 {
            let mut request = store
                .create(
                    participant("a"),
                    participant("b"),
                    NewRequest {
                        kind: RequestKind::Notice,
                        expects_reply: false,
                        body: format!("notice-{index}"),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            request.created_at -= time::Duration::days(2);
            match index {
                2 => request.notified_at = Some(OffsetDateTime::now_utc()),
                3 => request.notify = Some(true),
                4 => {
                    request.kind = RequestKind::Task;
                    request.expects_reply = true;
                }
                5 => request.status = RequestStatus::Claimed,
                6 => request.created_at = OffsetDateTime::now_utc(),
                7 => request.wake_delivery = Some(super::super::WakeDeliveryState::Prepared),
                8 => {
                    request.kind = RequestKind::Question;
                    request.expects_reply = true;
                    request.human_action = Some(HumanAction::Choice);
                    request.to.console = true;
                }
                _ => (),
            }
            ids.push(request.id.clone());
            store
                .requests
                .write()
                .await
                .insert(request.id.clone(), request);
        }
        assert_eq!(store.health().await.unwrap().stale_quiet_notices, 2);
        let cutoff = OffsetDateTime::now_utc() - time::Duration::days(1);
        let before = store.requests.read().await.clone();
        let preview = store
            .expire_queued_filtered(cutoff, true, true, Some(&ids[0]))
            .await
            .unwrap();
        assert_eq!(preview.len(), 1);
        assert_eq!(preview[0].id, ids[0]);
        assert_eq!(*store.requests.read().await, before);
        let expired = store
            .expire_queued_filtered(cutoff, false, true, Some(&ids[0]))
            .await
            .unwrap();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].status, RequestStatus::Expired);
        // The preview is not a reservation: a concurrent claim wins.
        store
            .requests
            .write()
            .await
            .get_mut(&ids[1])
            .unwrap()
            .status = RequestStatus::Claimed;
        assert!(store
            .expire_queued_filtered(cutoff, false, true, None)
            .await
            .unwrap()
            .is_empty());
        let rows = store.requests.read().await;
        for id in &ids[2..] {
            assert_eq!(rows[id], before[id]);
        }
        drop(rows);
        let broad = store.expire_queued_before(cutoff, false).await.unwrap();
        assert!(
            !broad.iter().any(|r| r.id == ids[8]),
            "human decisions must survive legacy bulk expiry too"
        );
        assert_eq!(
            store.requests.read().await[&ids[8]].status,
            RequestStatus::Queued
        );
    }

    #[tokio::test]
    async fn weekly_outcomes_count_agent_requests_by_ending_and_interruption_reason() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let mut ids = Vec::new();
        for body in ["replied", "expired", "overdue", "unsubmitted", "old"] {
            ids.push(
                store
                    .create(
                        participant("a"),
                        participant("b"),
                        NewRequest {
                            body: body.into(),
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap()
                    .id,
            );
        }
        let notice = store
            .create(
                participant("a"),
                participant("b"),
                NewRequest {
                    kind: RequestKind::Notice,
                    expects_reply: false,
                    body: "fyi".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let now = OffsetDateTime::now_utc();
        {
            let mut requests = store.requests.write().await;
            let interruption = |reason: &str| PeerInterruption {
                request_id: String::new(),
                reason: reason.into(),
                active: false,
                observed_at: now,
                reset_at: None,
                action_request_id: None,
                decision: None,
            };
            requests.get_mut(&ids[0]).unwrap().reply =
                Some(crate::collaboration::CollaborationReply {
                    status: RequestStatus::Completed,
                    body: "ok".into(),
                    artifacts: vec![],
                    at: now,
                    report: None,
                });
            requests.get_mut(&ids[1]).unwrap().status = RequestStatus::Expired;
            requests.get_mut(&ids[2]).unwrap().interruption = Some(interruption("reply_overdue"));
            requests.get_mut(&ids[3]).unwrap().interruption =
                Some(interruption("wake_unsubmitted"));
            requests.get_mut(&ids[4]).unwrap().created_at = now - time::Duration::days(8);
        }
        let health = store.health().await.unwrap();
        let this_week = &health.weekly_outcomes[0];
        assert_eq!(
            this_week.requests, 4,
            "the notice and last week's request are elsewhere"
        );
        assert_eq!(this_week.replied, 1);
        assert_eq!(this_week.expired, 1);
        assert_eq!(this_week.open, 2);
        assert_eq!(this_week.reply_overdue, 1);
        assert_eq!(this_week.wake_unsubmitted, 1);
        assert_eq!(health.weekly_outcomes[1].requests, 1);
        assert_eq!(health.weekly_outcomes.len(), 4);
        let _ = notice;
    }
}
