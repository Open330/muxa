//! Exact-session retirement reservations. Lock order: pane action gate, spawn
//! mutex, mailbox transaction, drain records. Drain operations release the
//! mailbox/spawn locks before provider IO.
use super::purpose::ProcessFence;
use super::{
    addresses, is_pending_session, CollaborationError, CollaborationStore, LaunchPurpose,
    Participant, RequestStatus, RoomId,
};
use crate::{tmux::PaneInfo, Agent, AgentKind, AgentState};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};
use time::OffsetDateTime;
use tokio::sync::{Mutex, OwnedMutexGuard};

pub const WORKER_DRAIN_CAPABILITY: &str = "collaboration_worker_drain_v1";
const LEASE_SECS: i64 = 30;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerDrainTarget {
    pub session_id: String,
    pub agent_kind: AgentKind,
    pub pane: String,
    pub socket: Option<String>,
    pub room: RoomId,
}
impl From<&Participant> for WorkerDrainTarget {
    fn from(p: &Participant) -> Self {
        Self {
            session_id: p.agent_session_id.clone(),
            agent_kind: p.agent_kind,
            pane: p.pane.clone(),
            socket: p.socket.clone(),
            room: p.room.clone(),
        }
    }
}
impl WorkerDrainTarget {
    pub(crate) fn owns(&self, p: &Participant) -> bool {
        self.session_id == p.agent_session_id
            && self.agent_kind == p.agent_kind
            && self.pane == p.pane
            && self.socket == p.socket
            && self.room == p.room
    }
    fn endpoint(&self, pane: &str, socket: Option<&str>) -> bool {
        self.pane == pane
            && socket.is_none_or(|s| {
                self.socket
                    .as_deref()
                    .is_some_and(|t| crate::backend::pane_endpoints_match(Some(pane), t, s))
            })
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerDrainPolicy {
    pub min_observed_idle_secs: u64,
    pub allow_error: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerDrainCandidate {
    pub target: WorkerDrainTarget,
    pub state: AgentState,
    #[serde(with = "time::serde::rfc3339::option")]
    pub observed_idle_since: Option<OffsetDateTime>,
    pub eligible: bool,
    pub reason: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerDrainPlan {
    pub candidates: Vec<WorkerDrainCandidate>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerDrainPhase {
    Draining,
    StopRequested,
    Exited,
    Aborted,
    Uncertain,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerDrainToken {
    pub drain_id: String,
    pub generation: u64,
    pub target: WorkerDrainTarget,
    pub process_fence: ProcessFence,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerDrainView {
    pub token: WorkerDrainToken,
    pub phase: WorkerDrainPhase,
    pub operation_id: Option<String>,
    pub detail: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // Public wire DTO; keep the token value directly accessible.
pub enum WorkerDrainAcquire {
    Acquired { token: WorkerDrainToken },
    Skipped { reason: String },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerDrainFinish {
    Exited,
    Aborted,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerDrainStopOutcome {
    Exited,
    Refused,
    LeftRunning,
    Uncertain,
    Skipped,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerDrainStop {
    pub outcome: WorkerDrainStopOutcome,
    pub drain: WorkerDrainView,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct WorkerDrainRecord {
    view: WorkerDrainView,
    policy: WorkerDrainPolicy,
    roles: Vec<String>,
    finished_at: Option<OffsetDateTime>,
    stop_outcome: Option<WorkerDrainStopOutcome>,
}
impl WorkerDrainRecord {
    fn active(&self) -> bool {
        matches!(
            self.view.phase,
            WorkerDrainPhase::StopRequested | WorkerDrainPhase::Uncertain
        ) || self.view.phase == WorkerDrainPhase::Draining
            && self.view.token.expires_at > OffsetDateTime::now_utc()
    }
    fn close(&mut self, phase: WorkerDrainPhase, detail: &str) {
        self.view.phase = phase;
        self.view.detail = detail.chars().take(512).collect();
        self.finished_at = Some(OffsetDateTime::now_utc());
    }
    fn check(&self, token: &WorkerDrainToken) -> Result<(), CollaborationError> {
        if self.view.token.drain_id != token.drain_id
            || self.view.token.generation != token.generation
            || self.view.token.target != token.target
            || self.view.token.process_fence != token.process_fence
        {
            return Err(drain_error("stale or altered drain token"));
        }
        Ok(())
    }
}
fn drain_error(message: &str) -> CollaborationError {
    CollaborationError::WorkerDrain(message.into())
}

pub(super) fn load_drains(conn: &Connection) -> Result<Vec<WorkerDrainRecord>, CollaborationError> {
    let mut statement =
        conn.prepare("SELECT payload FROM collaboration_worker_drains ORDER BY rowid")?;
    let rows = statement.query_map([], |r| r.get::<_, String>(0))?;
    let mut records: Vec<WorkerDrainRecord> = rows
        .map(|r| Ok(serde_json::from_str(&r?)?))
        .collect::<Result<_, CollaborationError>>()?;
    drop(statement);
    for record in &mut records {
        if record.view.phase == WorkerDrainPhase::StopRequested {
            record.view.phase = WorkerDrainPhase::Uncertain;
            record.view.detail =
                "daemon restarted after stop commit; verify the original process outcome".into();
        } else if record.view.phase == WorkerDrainPhase::Draining && !record.active() {
            record.close(WorkerDrainPhase::Aborted, "pre-stop lease expired");
        }
        conn.execute(
            "UPDATE collaboration_worker_drains SET payload=?2 WHERE id=?1",
            params![record.view.token.drain_id, serde_json::to_string(record)?],
        )?;
    }
    Ok(records)
}

impl CollaborationStore {
    fn persist_drain(&self, record: &WorkerDrainRecord) -> Result<(), CollaborationError> {
        if let Some(database) = &self.database {
            database.with_connection(|connection| {
                connection.execute(
                    "INSERT OR REPLACE INTO collaboration_worker_drains(id,payload) VALUES (?1,?2)",
                    params![record.view.token.drain_id, serde_json::to_string(record)?],
                )?;
                Ok(())
            })?;
        }
        Ok(())
    }
    /// A coarse pane-ID gate also serializes same-numbered panes on different
    /// sockets. The authority checks still require the exact socket and room.
    pub async fn worker_action_guard(&self, pane: &str) -> OwnedMutexGuard<()> {
        let gate = self
            .worker_action_gates
            .lock()
            .await
            .entry(pane.into())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        gate.lock_owned().await
    }
    /// Reservations cover scheduled and judged firings, not just keystrokes.
    pub async fn reserve_worker_automation(
        self: &Arc<Self>,
        session_id: &str,
        pane: &str,
        socket: Option<&str>,
    ) -> Option<WorkerAutomationReservation> {
        let _gate = self.worker_action_guard(pane).await;
        let _transaction = self.transaction_lock.lock().await;
        let mut records = self.worker_drains.lock().await;
        for r in records.iter_mut().filter(|r| {
            r.active()
                && r.view.token.target.session_id == session_id
                && r.view.token.target.endpoint(pane, socket)
        }) {
            if r.view.phase != WorkerDrainPhase::Draining {
                return None;
            }
            let mut updated = r.clone();
            updated.close(
                WorkerDrainPhase::Aborted,
                "automation vetoed the pre-stop drain",
            );
            if self.persist_drain(&updated).is_err() {
                return None;
            }
            *r = updated;
            self.publish_change();
        }
        *self
            .worker_automation
            .write()
            .await
            .entry(session_id.into())
            .or_default() += 1;
        Some(WorkerAutomationReservation {
            store: self.clone(),
            session_id: session_id.into(),
        })
    }
    pub async fn worker_drains(&self) -> Vec<WorkerDrainView> {
        self.worker_drains
            .lock()
            .await
            .iter()
            .map(|r| {
                let mut view = r.view.clone();
                if view.phase == WorkerDrainPhase::Draining && !r.active() {
                    view.phase = WorkerDrainPhase::Aborted;
                    view.detail = "pre-stop lease expired".into();
                }
                view
            })
            .collect()
    }
    pub async fn worker_input_blocked(&self, pane: &str, socket: Option<&str>) -> bool {
        self.worker_drains
            .lock()
            .await
            .iter()
            .any(|r| r.active() && r.view.token.target.endpoint(pane, socket))
    }
    pub async fn enrich_worker_drains(&self, participants: &mut [Participant]) {
        let records = self.worker_drains.lock().await;
        for p in participants {
            p.draining = records
                .iter()
                .any(|r| r.active() && r.view.token.target.owns(p));
        }
    }
    pub(super) async fn draining_worker_pool(
        &self,
        caller: &Participant,
        kind: AgentKind,
        role: Option<&str>,
    ) -> Option<WorkerDrainView> {
        self.worker_drains
            .lock()
            .await
            .iter()
            .find(|r| {
                r.active()
                    && r.view.token.target.room == caller.room
                    && r.view.token.target.agent_kind == kind
                    && role.is_none_or(|role| r.roles.iter().any(|r| r.eq_ignore_ascii_case(role)))
            })
            .map(|r| r.view.clone())
    }
    /// Called under `transaction_lock`. New work vetoes a pre-stop drain;
    /// committed or uncertain stops refuse new exact-session work.
    pub(super) async fn admit_worker_work(
        &self,
        p: &Participant,
    ) -> Result<(), CollaborationError> {
        let mut records = self.worker_drains.lock().await;
        for r in records
            .iter_mut()
            .filter(|r| r.active() && r.view.token.target.owns(p))
        {
            if r.view.phase != WorkerDrainPhase::Draining {
                return Err(drain_error(
                    "worker_retiring: retry after retirement or recovery",
                ));
            }
            let mut updated = r.clone();
            updated.close(
                WorkerDrainPhase::Aborted,
                "new work vetoed the pre-stop drain",
            );
            self.persist_drain(&updated)?;
            *r = updated;
            self.publish_change();
        }
        Ok(())
    }
    pub(super) async fn worker_claim_blocked(&self, p: &Participant) -> bool {
        self.worker_drains
            .lock()
            .await
            .iter()
            .any(|r| r.active() && r.view.token.target.owns(p))
    }
    async fn work_exclusion(&self, p: &Participant) -> Option<String> {
        let requests = self.requests.read().await;
        if requests.values().any(|r| {
            (addresses(&r.to, p) || r.from.same_session(p))
                && (matches!(r.status, RequestStatus::Queued | RequestStatus::Claimed)
                    || r.reply_wake_owed()
                    || matches!(r.reply_wake, Some(super::ReplyWake::Reserved { .. }))
                    || r.wake_delivery.is_some())
        }) {
            return Some("open_requests_or_reply_wakes".into());
        }
        None
    }
    async fn drain_candidate(
        &self,
        p: &Participant,
        agents: &[Agent],
        policy: &WorkerDrainPolicy,
    ) -> WorkerDrainCandidate {
        let target = WorkerDrainTarget::from(p);
        let since = super::worker_inventory::idle_since(p, agents);
        let row = agents.iter().find(|a| a.session_id == p.agent_session_id);
        let reason = if p.console {
            "console"
        } else if p.launch_purpose != LaunchPurpose::Worker {
            "primary"
        } else if is_pending_session(&p.agent_session_id)
            || p.agent_session_id
                .starts_with(crate::state::SYNTHETIC_SESSION_PREFIX)
        {
            "pending"
        } else if !matches!(p.agent_kind, AgentKind::ClaudeCode | AgentKind::Codex) {
            "unsupported_provider"
        } else if p.draining {
            "already_draining"
        } else if row.is_none() {
            "unknown_agent_row"
        } else if row.is_some_and(|a| {
            !a.subagents.is_empty() || a.workload.process_count > 0 || a.workload.subagent_count > 0
        }) {
            "active_child_work"
        } else if p.state == AgentState::Error && policy.allow_error {
            "eligible"
        } else if p.state != AgentState::Idle {
            "not_idle"
        } else if since.is_none() {
            "unknown_idle_observation"
        } else if since.is_some_and(|since| {
            (OffsetDateTime::now_utc() - since).whole_seconds()
                < i64::try_from(policy.min_observed_idle_secs).unwrap_or(i64::MAX)
        }) {
            "too_fresh"
        } else {
            "eligible"
        };
        let mut reason = reason.to_string();
        if reason == "eligible"
            && self
                .worker_automation
                .read()
                .await
                .get(&p.agent_session_id)
                .copied()
                .unwrap_or(0)
                > 0
        {
            reason = "active_automation".into();
        }
        if reason == "eligible" {
            if let Some(work) = self.work_exclusion(p).await {
                reason = work;
            }
        }
        WorkerDrainCandidate {
            target,
            state: p.state,
            observed_idle_since: since,
            eligible: reason == "eligible",
            reason,
        }
    }
    pub async fn worker_drain_plan(
        &self,
        participants: &[Participant],
        agents: &[Agent],
        policy: &WorkerDrainPolicy,
    ) -> WorkerDrainPlan {
        let spawns = self.worker_spawns.lock().await;
        let mut current = participants.to_vec();
        self.enrich_worker_drains(&mut current).await;
        let participants = current.as_slice();
        let _transaction = self.transaction_lock.lock().await;
        let mut candidates = Vec::new();
        for p in participants {
            let mut candidate = self.drain_candidate(p, agents, policy).await;
            if candidate.eligible && spawns.iter().any(|s| s.incomplete_for(p)) {
                candidate.eligible = false;
                candidate.reason = "unfinished_launch".into();
            }
            candidates.push(candidate);
        }
        WorkerDrainPlan { candidates }
    }
    /// Caller holds the pane action gate and supplied a fresh topology.
    pub async fn worker_drain_acquire(
        &self,
        target: &WorkerDrainTarget,
        policy: WorkerDrainPolicy,
        participants: &[Participant],
        agents: &[Agent],
        panes: &[PaneInfo],
    ) -> Result<WorkerDrainAcquire, CollaborationError> {
        self.ensure_enabled()?;
        if target.socket.is_none()
            && matches!(
                crate::backend::pane_id_host_kind(&target.pane),
                Some(crate::backend::HostKind::Tmux | crate::backend::HostKind::Rmux)
            )
        {
            return Ok(WorkerDrainAcquire::Skipped {
                reason: "unobservable_endpoint".into(),
            });
        }
        let Some(p) = participants.iter().find(|p| target.owns(p)) else {
            return Ok(WorkerDrainAcquire::Skipped {
                reason: "session_or_endpoint_changed".into(),
            });
        };
        let observed = super::unique_pane(&target.pane, target.socket.as_deref(), panes).cloned();
        let kind = target.agent_kind;
        let fence = tokio::task::spawn_blocking(move || ProcessFence::observe(&observed?, kind))
            .await
            .ok()
            .flatten();
        let Some(fence) = fence else {
            return Ok(WorkerDrainAcquire::Skipped {
                reason: "unobservable_process_fence".into(),
            });
        };
        let spawns = self.worker_spawns.lock().await;
        let _transaction = self.transaction_lock.lock().await;
        let candidate = self.drain_candidate(p, agents, &policy).await;
        if !candidate.eligible {
            return Ok(WorkerDrainAcquire::Skipped {
                reason: candidate.reason,
            });
        }
        if spawns.iter().any(|s| s.incomplete_for(p)) {
            return Ok(WorkerDrainAcquire::Skipped {
                reason: "unfinished_launch".into(),
            });
        }
        let mut records = self.worker_drains.lock().await;
        if records.iter().any(|r| {
            r.active()
                && r.view
                    .token
                    .target
                    .endpoint(&target.pane, target.socket.as_deref())
        }) {
            return Ok(WorkerDrainAcquire::Skipped {
                reason: "already_draining".into(),
            });
        }
        let generation = records
            .iter()
            .filter(|r| r.view.token.target == *target)
            .map(|r| r.view.token.generation)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let token = WorkerDrainToken {
            drain_id: uuid::Uuid::new_v4().to_string(),
            generation,
            target: target.clone(),
            process_fence: fence,
            expires_at: OffsetDateTime::now_utc() + time::Duration::seconds(LEASE_SECS),
        };
        let record = WorkerDrainRecord {
            view: WorkerDrainView {
                token: token.clone(),
                phase: WorkerDrainPhase::Draining,
                operation_id: None,
                detail: "reserved for graceful stop".into(),
            },
            policy,
            roles: p.roles.clone(),
            finished_at: None,
            stop_outcome: None,
        };
        self.persist_drain(&record)?;
        records.push(record);
        self.publish_change();
        Ok(WorkerDrainAcquire::Acquired { token })
    }
    pub async fn worker_drain_renew(
        &self,
        token: &WorkerDrainToken,
    ) -> Result<WorkerDrainToken, CollaborationError> {
        let _transaction = self.transaction_lock.lock().await;
        let mut records = self.worker_drains.lock().await;
        let r = records
            .iter_mut()
            .find(|r| r.view.token.drain_id == token.drain_id)
            .ok_or_else(|| drain_error("unknown drain"))?;
        r.check(token)?;
        if r.view.phase != WorkerDrainPhase::Draining || !r.active() {
            return Err(drain_error(
                "drain cannot renew after expiry or stop commit",
            ));
        }
        let mut updated = r.clone();
        updated.view.token.expires_at =
            OffsetDateTime::now_utc() + time::Duration::seconds(LEASE_SECS);
        self.persist_drain(&updated)?;
        *r = updated;
        Ok(r.view.token.clone())
    }
    /// A replay never dispatches a second stop, including after daemon restart.
    pub(crate) async fn worker_drain_begin_stop(
        &self,
        token: &WorkerDrainToken,
        operation_id: &str,
        participants: &[Participant],
        agents: &[Agent],
        panes: &[PaneInfo],
    ) -> Result<(WorkerDrainStop, bool), CollaborationError> {
        uuid::Uuid::parse_str(operation_id)
            .map_err(|_| drain_error("operation_id must be a UUID"))?;
        let snapshot = self
            .worker_drains
            .lock()
            .await
            .iter()
            .find(|r| r.view.token.drain_id == token.drain_id)
            .cloned()
            .ok_or_else(|| drain_error("unknown drain"))?;
        snapshot.check(token)?;
        if let Some(old) = &snapshot.view.operation_id {
            if old != operation_id {
                return Err(drain_error(
                    "another stop operation already owns this drain",
                ));
            }
            return Ok((
                WorkerDrainStop {
                    outcome: snapshot
                        .stop_outcome
                        .unwrap_or(WorkerDrainStopOutcome::Uncertain),
                    drain: snapshot.view,
                },
                false,
            ));
        }
        let current = participants.iter().find(|p| token.target.owns(p));
        let mut reason = match current {
            Some(p) => {
                let mut p = p.clone();
                p.draining = false;
                self.drain_candidate(&p, agents, &snapshot.policy)
                    .await
                    .reason
            }
            None => "session_or_endpoint_changed".into(),
        };
        let pane =
            super::unique_pane(&token.target.pane, token.target.socket.as_deref(), panes).cloned();
        let kind = token.target.agent_kind;
        let observed = tokio::task::spawn_blocking(move || ProcessFence::observe(&pane?, kind))
            .await
            .ok()
            .flatten();
        if observed.as_ref() != Some(&token.process_fence) {
            reason = "process_fence_changed".into();
        }
        let spawns = self.worker_spawns.lock().await;
        if current.is_some_and(|p| spawns.iter().any(|s| s.incomplete_for(p))) {
            reason = "unfinished_launch".into();
        }
        let _transaction = self.transaction_lock.lock().await;
        let mut records = self.worker_drains.lock().await;
        let r = records
            .iter_mut()
            .find(|r| r.view.token.drain_id == token.drain_id)
            .ok_or_else(|| drain_error("unknown drain"))?;
        r.check(token)?;
        if r.view.operation_id.is_some() {
            return Err(drain_error("stop already committed"));
        }
        if r.view.phase != WorkerDrainPhase::Draining || !r.active() {
            reason = "drain_expired_or_aborted".into();
        }
        if let Some(p) = current {
            if let Some(work) = self.work_exclusion(p).await {
                reason = work;
            }
        }
        let mut updated = r.clone();
        if reason == "eligible" {
            updated.view.phase = WorkerDrainPhase::StopRequested;
            updated.view.operation_id = Some(operation_id.into());
            updated.view.detail = "graceful stop committed".into();
        } else {
            updated.close(WorkerDrainPhase::Aborted, &reason);
            updated.stop_outcome = Some(WorkerDrainStopOutcome::Skipped);
        }
        self.persist_drain(&updated)?;
        *r = updated;
        self.publish_change();
        Ok((
            WorkerDrainStop {
                outcome: r.stop_outcome.unwrap_or(WorkerDrainStopOutcome::Uncertain),
                drain: r.view.clone(),
            },
            r.view.phase == WorkerDrainPhase::StopRequested,
        ))
    }
    pub(crate) async fn worker_drain_stop_safe(
        &self,
        token: &WorkerDrainToken,
        participants: &[Participant],
        agents: &[Agent],
    ) -> bool {
        let spawns = self.worker_spawns.lock().await;
        let _transaction = self.transaction_lock.lock().await;
        let snapshot = self
            .worker_drains
            .lock()
            .await
            .iter()
            .find(|r| r.view.token.drain_id == token.drain_id)
            .cloned();
        let Some(record) = snapshot else {
            return false;
        };
        if record.check(token).is_err() || record.view.phase != WorkerDrainPhase::StopRequested {
            return false;
        }
        let Some(p) = participants.iter().find(|p| token.target.owns(p)) else {
            return false;
        };
        if spawns.iter().any(|s| s.incomplete_for(p)) {
            return false;
        }
        if !self.effective_worker(p).await {
            return false;
        }
        let mut p = p.clone();
        p.draining = false;
        self.drain_candidate(&p, agents, &record.policy)
            .await
            .eligible
    }
    pub(crate) async fn worker_drain_complete_stop(
        &self,
        token: &WorkerDrainToken,
        outcome: WorkerDrainStopOutcome,
        detail: &str,
    ) -> Result<WorkerDrainStop, CollaborationError> {
        let exited = token.process_fence.definitely_exited();
        let mut spawns = self.worker_spawns.lock().await;
        let _transaction = self.transaction_lock.lock().await;
        let mut records = self.worker_drains.lock().await;
        let r = records
            .iter_mut()
            .find(|r| r.view.token.drain_id == token.drain_id)
            .ok_or_else(|| drain_error("unknown drain"))?;
        r.check(token)?;
        let mut updated = r.clone();
        match outcome {
            WorkerDrainStopOutcome::Exited if exited => {
                updated.close(WorkerDrainPhase::Exited, detail);
            }
            WorkerDrainStopOutcome::Refused | WorkerDrainStopOutcome::Skipped => {
                updated.close(WorkerDrainPhase::Aborted, detail);
            }
            _ => {
                updated.view.phase = WorkerDrainPhase::Uncertain;
                updated.view.detail = detail.chars().take(512).collect();
            }
        }
        updated.stop_outcome = Some(if updated.view.phase == WorkerDrainPhase::Uncertain {
            if outcome == WorkerDrainStopOutcome::LeftRunning {
                outcome
            } else {
                WorkerDrainStopOutcome::Uncertain
            }
        } else {
            outcome
        });
        if updated.view.phase == WorkerDrainPhase::Exited {
            for spawn in spawns.iter_mut() {
                self.close_drained_spawn(spawn, token)?;
            }
        }
        self.persist_drain(&updated)?;
        *r = updated;
        self.publish_change();
        Ok(WorkerDrainStop {
            outcome: r.stop_outcome.unwrap(),
            drain: r.view.clone(),
        })
    }
    pub async fn worker_drain_finish(
        &self,
        token: &WorkerDrainToken,
        outcome: WorkerDrainFinish,
        detail: &str,
    ) -> Result<WorkerDrainView, CollaborationError> {
        let exited = token.process_fence.definitely_exited();
        let mut spawns = self.worker_spawns.lock().await;
        let _transaction = self.transaction_lock.lock().await;
        let mut records = self.worker_drains.lock().await;
        let r = records
            .iter_mut()
            .find(|r| r.view.token.drain_id == token.drain_id)
            .ok_or_else(|| drain_error("unknown drain"))?;
        r.check(token)?;
        let mut updated = r.clone();
        match outcome {
            WorkerDrainFinish::Exited if exited => updated.close(WorkerDrainPhase::Exited, detail),
            WorkerDrainFinish::Aborted if r.view.phase==WorkerDrainPhase::Draining || r.view.phase==WorkerDrainPhase::Aborted => updated.close(WorkerDrainPhase::Aborted, detail),
            _ => return Err(drain_error("cannot assert exit or abort an uncertain/in-flight stop; verify the original process exited")),
        }
        if updated.view.phase == WorkerDrainPhase::Exited {
            for spawn in spawns.iter_mut() {
                self.close_drained_spawn(spawn, token)?;
            }
        }
        self.persist_drain(&updated)?;
        *r = updated;
        self.publish_change();
        Ok(r.view.clone())
    }
}

// Kept separate from the mailbox lock. Admission sees these reservations before
// it can race with a backend action; the action guard spans actual IO.
pub(super) type WorkerActionGates = Mutex<HashMap<String, Arc<Mutex<()>>>>;

/// Dropping a scheduled/active firing releases its exclusion, including all
/// error, canceled-judgment and daemon-shutdown paths.
pub struct WorkerAutomationReservation {
    store: Arc<CollaborationStore>,
    session_id: String,
}
impl Drop for WorkerAutomationReservation {
    fn drop(&mut self) {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let store = self.store.clone();
            let session = self.session_id.clone();
            runtime.spawn(async move {
                let _transaction = store.transaction_lock.lock().await;
                let mut active = store.worker_automation.write().await;
                if let Some(count) = active.get_mut(&session) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        active.remove(&session);
                    }
                }
            });
        }
    }
}

#[cfg(test)]
#[path = "worker_drain_tests.rs"]
mod tests;
