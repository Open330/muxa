//! A read-only inventory of the worker agents muxa launched for peer work.
//!
//! Only sessions positively known to be workers appear: a session whose
//! effective launch purpose is `Worker` (it consumed a process-fenced launch
//! grant) or a verified pending launch that has not registered yet. Nothing
//! is inferred from idle time, provider or a neighbouring pane, and nothing
//! here changes any state; it exists so the operator can see what muxa has
//! spawned and what is left behind.

use super::{addresses, is_pending_session, LaunchPurpose, Participant, RequestStatus, RoomId};
use crate::event::{AgentKind, AgentState};
use crate::state::Agent;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use time::OffsetDateTime;

/// One live worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerEntry {
    pub pane: String,
    pub agent_kind: AgentKind,
    pub agent_session_id: String,
    pub room: RoomId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux_session_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    pub state: AgentState,
    /// A launch whose process is verified but whose session has not
    /// registered yet (Codex registers on its first prompt).
    pub pending: bool,
    /// How long the row has read `Idle`, as observed by muxa. `None` when it
    /// is not idle or no observation exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_idle: Option<ObservedIdle>,
    /// Requests addressed to this worker that are not finished.
    pub queued_requests: usize,
    pub claimed_requests: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<WorkerFlag>,
}

/// An idle duration and where it was read from. It is an observation, not a
/// guarantee: a missed state event would make it look longer than it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedIdle {
    #[serde(with = "time::serde::rfc3339")]
    pub since: OffsetDateTime,
    pub seconds: u64,
    /// The agent-row field the time came from.
    pub source: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerFlag {
    /// In an error state, so it is not chosen for new peer work and a
    /// replacement may be spawned beside it.
    Error,
    /// Observed idle for longer than the inventory's threshold.
    LongIdle,
    /// Another live worker in the same room runs the same provider with a
    /// compatible role. Informational: separate sessions can be deliberate.
    PossibleOverlap,
}

/// Every live worker, plus counts that are context rather than findings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerInventory {
    pub workers: Vec<WorkerEntry>,
    /// Idle time past which `LongIdle` is set.
    pub idle_threshold_secs: u64,
    /// Launch grants on record, including ones whose session has ended. A
    /// historical count, not a count of live processes.
    pub recorded_grants: usize,
}

impl WorkerInventory {
    #[must_use]
    pub fn flagged(&self, flag: WorkerFlag) -> usize {
        self.workers
            .iter()
            .filter(|worker| worker.flags.contains(&flag))
            .count()
    }
}

/// Build the inventory from the enriched participants of the whole host,
/// when each agent row entered its current state, and the open requests.
#[must_use]
pub fn project<'a>(
    participants: &[Participant],
    entered: &HashMap<String, OffsetDateTime>,
    requests: impl IntoIterator<Item = (&'a Participant, RequestStatus)> + Clone,
    recorded_grants: usize,
    idle_threshold_secs: u64,
    now: OffsetDateTime,
) -> WorkerInventory {
    let mut workers: Vec<WorkerEntry> = participants
        .iter()
        .filter(|p| p.launch_purpose == LaunchPurpose::Worker && !p.console)
        .map(|p| {
            let (queued, claimed) =
                requests
                    .clone()
                    .into_iter()
                    .fold((0, 0), |(queued, claimed), (to, status)| {
                        if !addresses(to, p) {
                            return (queued, claimed);
                        }
                        match status {
                            RequestStatus::Queued => (queued + 1, claimed),
                            RequestStatus::Claimed => (queued, claimed + 1),
                            _ => (queued, claimed),
                        }
                    });
            let observed_idle = (p.state == AgentState::Idle)
                .then(|| entered.get(&p.agent_session_id))
                .flatten()
                .map(|since| ObservedIdle {
                    since: *since,
                    seconds: u64::try_from((now - *since).whole_seconds()).unwrap_or(0),
                    source: "state_entered_at".into(),
                });
            let mut flags = Vec::new();
            if p.state == AgentState::Error {
                flags.push(WorkerFlag::Error);
            }
            if observed_idle
                .as_ref()
                .is_some_and(|idle| idle.seconds > idle_threshold_secs)
            {
                flags.push(WorkerFlag::LongIdle);
            }
            WorkerEntry {
                pane: p.pane.clone(),
                agent_kind: p.agent_kind,
                agent_session_id: p.agent_session_id.clone(),
                room: p.room.clone(),
                tmux_session_name: p.tmux_session_name.clone(),
                window_name: p.window_name.clone(),
                roles: p.roles.clone(),
                alias: p.alias.clone(),
                state: p.state,
                pending: is_pending_session(&p.agent_session_id),
                observed_idle,
                queued_requests: queued,
                claimed_requests: claimed,
                flags,
            }
        })
        .collect();
    mark_possible_overlap(&mut workers);
    workers.sort_by(|a, b| (&a.room.window_id, &a.pane).cmp(&(&b.room.window_id, &b.pane)));
    WorkerInventory {
        workers,
        idle_threshold_secs,
        recorded_grants,
    }
}

/// Two workers overlap when they share a room and provider and neither
/// carries a role the other lacks. A pipeline's implementer and reviewer
/// differ by role and are left alone even when one provider runs both.
fn mark_possible_overlap(workers: &mut [WorkerEntry]) {
    let compatible = |a: &WorkerEntry, b: &WorkerEntry| {
        a.room == b.room
            && a.agent_kind == b.agent_kind
            && (a.roles.is_empty() || b.roles.is_empty() || a.roles == b.roles)
    };
    let overlapping: Vec<bool> = workers
        .iter()
        .enumerate()
        .map(|(i, a)| {
            workers
                .iter()
                .enumerate()
                .any(|(j, b)| i != j && compatible(a, b))
        })
        .collect();
    for (worker, overlaps) in workers.iter_mut().zip(overlapping) {
        if overlaps {
            worker.flags.push(WorkerFlag::PossibleOverlap);
        }
    }
}

impl super::CollaborationStore {
    /// The worker inventory over `participants` (already enriched with
    /// launch purposes) and their agent rows. Read-only.
    pub async fn worker_inventory(
        &self,
        participants: &[Participant],
        agents: &[Agent],
        idle_threshold_secs: u64,
    ) -> WorkerInventory {
        let recorded_grants = self.purposes.read().await.len();
        let entered: HashMap<String, OffsetDateTime> = agents
            .iter()
            .map(|agent| (agent.session_id.clone(), agent.state_entered_at))
            .collect();
        let requests = self.requests.read().await;
        let open: Vec<(&Participant, RequestStatus)> = requests
            .values()
            .filter(|r| matches!(r.status, RequestStatus::Queued | RequestStatus::Claimed))
            .map(|r| (&r.to, r.status))
            .collect();
        project(
            participants,
            &entered,
            open.iter().copied(),
            recorded_grants,
            idle_threshold_secs,
            OffsetDateTime::now_utc(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room(window: &str) -> RoomId {
        RoomId {
            host: "rmux".into(),
            socket: Some("/tmp/rmux".into()),
            window_id: window.into(),
        }
    }

    fn worker(pane: &str, session: &str, window: &str, kind: AgentKind) -> Participant {
        Participant {
            launch_purpose: LaunchPurpose::Worker,
            agent_kind: kind,
            agent_session_id: session.into(),
            pane: pane.into(),
            socket: Some("/tmp/rmux".into()),
            room: room(window),
            tmux_session_id: None,
            tmux_session_name: Some("muxa".into()),
            window_name: Some("main".into()),
            state: AgentState::Idle,
            cwd: None,
            alias: None,
            roles: Vec::new(),
            console: false,
        }
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()
    }

    fn inventory(participants: &[Participant], idle: &[(&str, i64)]) -> WorkerInventory {
        let entered = idle
            .iter()
            .map(|(session, hours)| ((*session).to_owned(), now() - time::Duration::hours(*hours)))
            .collect();
        project(
            participants,
            &entered,
            std::iter::empty(),
            7,
            24 * 3600,
            now(),
        )
    }

    #[test]
    fn only_positively_known_workers_are_listed() {
        let mut primary = worker("%1", "human", "@1", AgentKind::Codex);
        primary.launch_purpose = LaunchPurpose::Primary;
        let mut console = worker("console", "console", "@1", AgentKind::Codex);
        console.console = true;
        let listed = inventory(
            &[primary, console, worker("%2", "w", "@1", AgentKind::Codex)],
            &[],
        );
        assert_eq!(
            listed
                .workers
                .iter()
                .map(|w| w.pane.as_str())
                .collect::<Vec<_>>(),
            ["%2"]
        );
        assert_eq!(listed.recorded_grants, 7);
    }

    #[test]
    fn error_and_long_idle_are_flagged_from_observed_state() {
        let mut broken = worker("%3", "broken", "@1", AgentKind::ClaudeCode);
        broken.state = AgentState::Error;
        let listed = inventory(
            &[
                broken,
                worker("%4", "stale", "@2", AgentKind::Codex),
                worker("%5", "fresh", "@3", AgentKind::Codex),
                worker("%6", "unknown", "@4", AgentKind::Codex),
            ],
            &[("broken", 99), ("stale", 49), ("fresh", 2)],
        );
        let flags = |pane: &str| {
            listed
                .workers
                .iter()
                .find(|w| w.pane == pane)
                .unwrap()
                .flags
                .clone()
        };
        // An error row is not idle, however long ago it entered the state.
        assert_eq!(flags("%3"), [WorkerFlag::Error]);
        assert_eq!(flags("%4"), [WorkerFlag::LongIdle]);
        assert!(flags("%5").is_empty());
        // No observation means no idle claim at all.
        assert!(flags("%6").is_empty());
        let stale = listed.workers.iter().find(|w| w.pane == "%4").unwrap();
        let idle = stale.observed_idle.as_ref().unwrap();
        assert_eq!(idle.seconds, 49 * 3600);
        assert_eq!(idle.source, "state_entered_at");
        assert_eq!(listed.flagged(WorkerFlag::LongIdle), 1);
    }

    #[test]
    fn overlap_needs_one_room_one_provider_and_compatible_roles() {
        let mut implementer = worker("%10", "a", "@1", AgentKind::Codex);
        implementer.roles = vec!["implementer".into()];
        let mut reviewer = worker("%11", "b", "@1", AgentKind::Codex);
        reviewer.roles = vec!["reviewer".into()];
        let mut unroled = worker("%12", "c", "@2", AgentKind::ClaudeCode);
        unroled.roles = Vec::new();
        let mut twin = worker("%13", "d", "@2", AgentKind::ClaudeCode);
        twin.roles = vec!["reviewer".into()];
        let elsewhere = worker("%14", "e", "@3", AgentKind::ClaudeCode);
        let listed = inventory(&[implementer, reviewer, unroled, twin, elsewhere], &[]);
        let overlapping: Vec<&str> = listed
            .workers
            .iter()
            .filter(|w| w.flags.contains(&WorkerFlag::PossibleOverlap))
            .map(|w| w.pane.as_str())
            .collect();
        // A pipeline's implementer and reviewer differ by role: not overlap.
        assert_eq!(overlapping, ["%12", "%13"]);
    }

    #[test]
    fn open_requests_count_by_the_addressing_rule() {
        let target = worker("%20", "w", "@1", AgentKind::Codex);
        let other = worker("%21", "x", "@1", AgentKind::Codex);
        let requests = [
            (&target, RequestStatus::Queued),
            (&target, RequestStatus::Queued),
            (&target, RequestStatus::Claimed),
            (&target, RequestStatus::Completed),
            (&other, RequestStatus::Queued),
        ];
        let listed = project(
            std::slice::from_ref(&target),
            &HashMap::new(),
            requests.iter().copied(),
            0,
            3600,
            now(),
        );
        assert_eq!(listed.workers[0].queued_requests, 2);
        assert_eq!(listed.workers[0].claimed_requests, 1);
    }
}
