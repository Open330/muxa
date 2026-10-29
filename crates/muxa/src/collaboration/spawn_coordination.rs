//! Daemon-wide worker capacity reservations. A lease expiry is not evidence
//! that a physical launch failed: uncertain launches continue to occupy capacity.
use super::purpose::ProcessFence;
use super::{
    is_pending_session, pane_participant, pane_room, CollaborationError, CollaborationOrigin,
    CollaborationStore, LaunchPurpose, Participant, RoomId,
};
use crate::{
    event::{AgentKind, AgentState},
    tmux::PaneInfo,
};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

pub const WORKER_SPAWN_CAPABILITY: &str = "collaboration_worker_spawn_v1";
pub const WORKER_SPAWN_RELEASE_CAPABILITY: &str = "collaboration_worker_spawn_release_v1";
const LEASE_SECONDS: i64 = 30;
const CLOSED_RETENTION_DAYS: i64 = 7;

/// A provider target is a wildcard over roles; a role target requires its
/// exact role. The role stamped on a new launch is retained even for wildcard
/// callers, so provider/reviewer calls share capacity without merging roles.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerSpawnSpec {
    pub agent_kind: AgentKind,
    pub role: Option<String>,
    pub launch_role: String,
}

impl WorkerSpawnSpec {
    fn normalize(mut self) -> Result<Self, CollaborationError> {
        if !matches!(
            self.agent_kind,
            AgentKind::Codex
                | AgentKind::ClaudeCode
                | AgentKind::GeminiCli
                | AgentKind::Opencode
                | AgentKind::Antigravity
        ) {
            return Err(CollaborationError::WorkerLaunch(
                "not a worker provider".into(),
            ));
        }
        self.role = self.role.map(|r| r.trim().to_ascii_lowercase());
        self.launch_role = self.launch_role.trim().to_ascii_lowercase();
        if self.launch_role.is_empty()
            || self.launch_role.len() > 64
            || self
                .role
                .as_ref()
                .is_some_and(|r| r.is_empty() || r.len() > 64 || *r != self.launch_role)
        {
            return Err(CollaborationError::WorkerLaunch(
                "invalid worker routing role".into(),
            ));
        }
        Ok(self)
    }

    fn accepts_role(&self, roles: &[String]) -> bool {
        self.role
            .as_ref()
            .is_none_or(|role| roles.iter().any(|r| r.eq_ignore_ascii_case(role)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerSpawnPhase {
    Requested,
    Launching,
    PaneObserved,
    SessionBound,
    Failed,
    Uncertain,
}

/// Possession is not authorization: each action resolves origin again and
/// checks that it is the exact session which acquired the reservation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerSpawnTicket {
    pub launch_id: String,
    pub origin: CollaborationOrigin,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum WorkerSpawnAcquisition {
    Draining {
        drain_id: String,
        expires_at: OffsetDateTime,
    },
    Reuse {
        peer: Participant,
    },
    Pending {
        launch_id: String,
        phase: WorkerSpawnPhase,
        pane: Option<String>,
        socket: Option<String>,
        detail: Option<String>,
        /// A fenced pending pane, never a promoted real primary session.
        peer: Option<Participant>,
    },
    LaunchToken {
        ticket: WorkerSpawnTicket,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum WorkerSpawnAction {
    Begin,
    Renew,
    Observe {
        pane: String,
        socket: Option<String>,
    },
    /// The single mux create command finished unsuccessfully without a pane
    /// ID. This is a definite rejection, unlike a killed/timed-out command.
    CreateRejected {
        detail: String,
    },
    Uncertain {
        detail: String,
    },
    /// Explicit abandonment after investigation; does not stop any process.
    Release {
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct WorkerSpawnRecord {
    launch_id: String,
    owner: Participant,
    room: RoomId,
    agent_kind: AgentKind,
    role: String,
    phase: WorkerSpawnPhase,
    lease_until: OffsetDateTime,
    pane: Option<String>,
    socket: Option<String>,
    fence: Option<ProcessFence>,
    #[serde(default)]
    bound_session: Option<String>,
    detail: Option<String>,
    #[serde(default)]
    closed_at: Option<OffsetDateTime>,
}

/// Body-free lifecycle information for operator inventory and recovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerSpawnView {
    pub launch_id: String,
    pub room: RoomId,
    pub agent_kind: AgentKind,
    pub role: String,
    pub phase: WorkerSpawnPhase,
    pub pane: Option<String>,
    pub socket: Option<String>,
    pub detail: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub lease_until: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub closed_at: Option<OffsetDateTime>,
}

impl WorkerSpawnRecord {
    fn kind_matches_drain(&self, token: &super::WorkerDrainToken) -> bool {
        self.agent_kind == token.target.agent_kind
            && self.room.host == token.target.room.host
            && self.socket == token.target.socket
    }
    pub(super) fn incomplete_at(&self, pane: &str, socket: Option<&str>) -> bool {
        !matches!(
            self.phase,
            WorkerSpawnPhase::Failed | WorkerSpawnPhase::SessionBound
        ) && self.pane.as_deref() == Some(pane)
            && self.socket.as_deref() == socket
    }

    pub(super) fn incomplete_for(&self, p: &Participant) -> bool {
        !matches!(
            self.phase,
            WorkerSpawnPhase::Failed | WorkerSpawnPhase::SessionBound
        ) && (self.owner.same_session(p) || self.incomplete_at(&p.pane, p.socket.as_deref()))
    }
    fn close(&mut self, detail: String) {
        self.phase = WorkerSpawnPhase::Failed;
        self.closed_at = Some(OffsetDateTime::now_utc());
        self.detail = Some(detail.chars().take(512).collect());
    }

    fn expired_history(&self) -> bool {
        self.phase == WorkerSpawnPhase::Failed
            && self.closed_at.unwrap_or(self.lease_until)
                < OffsetDateTime::now_utc() - time::Duration::days(CLOSED_RETENTION_DAYS)
    }
}

pub(super) fn load_spawns(
    connection: &Connection,
) -> Result<Vec<WorkerSpawnRecord>, CollaborationError> {
    let mut statement =
        connection.prepare("SELECT payload FROM collaboration_worker_spawns ORDER BY rowid")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let mut records: Vec<WorkerSpawnRecord> = rows
        .map(|row| Ok(serde_json::from_str(&row?)?))
        .collect::<Result<_, CollaborationError>>()?;
    drop(statement);
    for record in &mut records {
        if record.phase != WorkerSpawnPhase::Failed
            && record
                .fence
                .as_ref()
                .is_some_and(ProcessFence::definitely_exited)
        {
            record.close("observed worker process exited".into());
            connection.execute(
                "UPDATE collaboration_worker_spawns SET payload=?2 WHERE id=?1",
                params![record.launch_id, serde_json::to_string(record)?],
            )?;
        }
        if record.expired_history() {
            connection.execute(
                "DELETE FROM collaboration_worker_spawns WHERE id=?1",
                [&record.launch_id],
            )?;
        }
    }
    records.retain(|r| !r.expired_history());
    Ok(records)
}

impl CollaborationStore {
    pub(super) fn close_drained_spawn(
        &self,
        record: &mut WorkerSpawnRecord,
        token: &super::WorkerDrainToken,
    ) -> Result<(), CollaborationError> {
        if record.fence.as_ref() == Some(&token.process_fence) && record.kind_matches_drain(token) {
            let mut updated = record.clone();
            updated.close("fenced worker exited through graceful prune".into());
            self.persist_spawn(&updated)?;
            *record = updated;
        }
        Ok(())
    }
    pub async fn worker_spawn_records(&self) -> Vec<WorkerSpawnView> {
        self.worker_spawns
            .lock()
            .await
            .iter()
            .map(|r| WorkerSpawnView {
                launch_id: r.launch_id.clone(),
                room: r.room.clone(),
                agent_kind: r.agent_kind,
                role: r.role.clone(),
                phase: r.phase,
                pane: r.pane.clone(),
                socket: r.socket.clone(),
                detail: r.detail.clone(),
                lease_until: r.lease_until,
                closed_at: r.closed_at,
            })
            .collect()
    }

    fn prune_closed_spawns(
        &self,
        records: &mut Vec<WorkerSpawnRecord>,
    ) -> Result<(), CollaborationError> {
        if !records.iter().any(WorkerSpawnRecord::expired_history) {
            return Ok(());
        }
        if let Some(database) = &self.database {
            database.with_connection(|connection| {
                let tx = connection.transaction()?;
                for record in records.iter().filter(|r| r.expired_history()) {
                    tx.execute(
                        "DELETE FROM collaboration_worker_spawns WHERE id=?1",
                        [&record.launch_id],
                    )?;
                }
                tx.commit()?;
                Ok(())
            })?;
        }
        records.retain(|r| !r.expired_history());
        Ok(())
    }

    fn persist_spawn(&self, record: &WorkerSpawnRecord) -> Result<(), CollaborationError> {
        if let Some(database) = &self.database {
            database.with_connection(|connection| {
                connection.execute(
                    "INSERT OR REPLACE INTO collaboration_worker_spawns(id,payload) VALUES (?1,?2)",
                    params![record.launch_id, serde_json::to_string(record)?],
                )?;
                Ok(())
            })?;
        }
        Ok(())
    }

    /// The separate spawn mutex serializes acquisition across every IPC client.
    /// Never hold the mailbox transaction lock while observing a CLI process.
    pub async fn acquire_worker_spawn(
        &self,
        caller: &Participant,
        origin: CollaborationOrigin,
        spec: WorkerSpawnSpec,
        participants: &[Participant],
        panes: &[PaneInfo],
    ) -> Result<WorkerSpawnAcquisition, CollaborationError> {
        self.ensure_enabled()?;
        if caller.console || is_pending_session(&caller.agent_session_id) {
            return Err(CollaborationError::ConsoleIdentity);
        }
        let spec = spec.normalize()?;
        let mut records = self.worker_spawns.lock().await;
        {
            let _transaction = self.transaction_lock.lock().await;
            self.admit_worker_work(caller).await?;
        }
        self.prune_closed_spawns(&mut records)?;
        // Availability may have changed after this client's inventory scan.
        // Drain acquisition shares this spawn mutex, so refresh inside it.
        let mut current_participants = participants.to_vec();
        self.enrich_worker_drains(&mut current_participants).await;
        let participants = current_participants.as_slice();
        for record in records.iter_mut().filter(|r| {
            r.room == caller.room
                && r.agent_kind == spec.agent_kind
                && r.phase != WorkerSpawnPhase::Failed
        }) {
            let mut updated = record.clone();
            self.reconcile_worker_spawn(&mut updated, participants, panes)
                .await?;
            self.persist_spawn(&updated)?;
            *record = updated;
        }
        // Busy workers remain reusable: only Error/Stopped are excluded.
        let peer = participants
            .iter()
            .filter(|p| {
                p.room == caller.room
                    && !p.same_session(caller)
                    && p.agent_kind == spec.agent_kind
                    && p.launch_purpose == LaunchPurpose::Worker
                    && !p.draining
                    && !matches!(p.state, AgentState::Error | AgentState::Stopped)
                    && spec.accepts_role(&p.roles)
            })
            .min_by_key(|p| {
                (
                    state_rank(p.state),
                    p.pane
                        .rsplit_once('%')
                        .and_then(|(_, n)| n.parse::<u64>().ok())
                        .unwrap_or(u64::MAX),
                )
            });
        if let Some(peer) = peer {
            return Ok(WorkerSpawnAcquisition::Reuse { peer: peer.clone() });
        }
        if let Some(drain) = self
            .draining_worker_pool(caller, spec.agent_kind, spec.role.as_deref())
            .await
        {
            return Ok(WorkerSpawnAcquisition::Draining {
                drain_id: drain.token.drain_id,
                expires_at: drain.token.expires_at,
            });
        }
        if let Some(record) = records.iter().find(|r| {
            r.room == caller.room
                && r.agent_kind == spec.agent_kind
                && r.phase != WorkerSpawnPhase::Failed
                && r.bound_session.as_deref() != Some(&caller.agent_session_id)
                && spec.accepts_role(std::slice::from_ref(&r.role))
        }) {
            return Ok(WorkerSpawnAcquisition::Pending {
                launch_id: record.launch_id.clone(),
                phase: record.phase,
                pane: record.pane.clone(),
                socket: record.socket.clone(),
                detail: record.detail.clone(),
                peer: pending_worker(record, participants, panes),
            });
        }
        let launch_id = uuid::Uuid::new_v4().to_string();
        let record = WorkerSpawnRecord {
            launch_id: launch_id.clone(),
            owner: caller.clone(),
            room: caller.room.clone(),
            agent_kind: spec.agent_kind,
            role: spec.launch_role,
            phase: WorkerSpawnPhase::Requested,
            lease_until: OffsetDateTime::now_utc() + time::Duration::seconds(LEASE_SECONDS),
            pane: None,
            socket: None,
            fence: None,
            bound_session: None,
            detail: None,
            closed_at: None,
        };
        self.persist_spawn(&record)?;
        records.push(record);
        self.publish_change();
        Ok(WorkerSpawnAcquisition::LaunchToken {
            ticket: WorkerSpawnTicket { launch_id, origin },
        })
    }

    pub async fn update_worker_spawn(
        &self,
        caller: &Participant,
        launch_id: &str,
        action: WorkerSpawnAction,
        panes: &[PaneInfo],
    ) -> Result<(), CollaborationError> {
        self.ensure_enabled()?;
        if let WorkerSpawnAction::Release { reason } = action {
            return self
                .release_worker_spawn(caller, launch_id, &reason, panes)
                .await;
        }
        let mut records = self.worker_spawns.lock().await;
        let record = records
            .iter_mut()
            .find(|r| r.launch_id == launch_id)
            .ok_or_else(|| {
                CollaborationError::WorkerLaunch(
                    "no such launch id; list launches with `muxa workers`".into(),
                )
            })?;
        if !record.owner.same_session(caller) || record.room != caller.room {
            return Err(CollaborationError::ScopeDenied);
        }
        let mut updated = record.clone();
        let now = OffsetDateTime::now_utc();
        match action {
            WorkerSpawnAction::Begin => {
                // Begin is single-use, including retries. It fences an old
                // client after a Requested lease has expired or been replaced.
                if record.phase != WorkerSpawnPhase::Requested || record.lease_until <= now {
                    return Err(CollaborationError::DispatchConflict(launch_id.into()));
                }
                updated.phase = WorkerSpawnPhase::Launching;
                updated.lease_until = now + time::Duration::seconds(LEASE_SECONDS);
            }
            WorkerSpawnAction::Renew => {
                if !matches!(
                    record.phase,
                    WorkerSpawnPhase::Requested | WorkerSpawnPhase::Launching
                ) || record.lease_until <= now
                {
                    return Err(CollaborationError::DispatchConflict(launch_id.into()));
                }
                updated.lease_until = now + time::Duration::seconds(LEASE_SECONDS);
            }
            WorkerSpawnAction::Observe { pane, socket } => {
                if !matches!(
                    record.phase,
                    WorkerSpawnPhase::Launching
                        | WorkerSpawnPhase::Uncertain
                        | WorkerSpawnPhase::PaneObserved
                ) {
                    return Err(CollaborationError::DispatchConflict(launch_id.into()));
                }
                let observed = super::unique_pane(&pane, socket.as_deref(), panes)
                    .filter(|p| {
                        p.worker_launch_id.as_deref() == Some(launch_id)
                            && pane_room(&p.pane_id, p, p.socket.clone()) == record.room
                    })
                    .ok_or_else(|| {
                        CollaborationError::WorkerLaunch(
                            "worker launch stamp absent or ambiguous".into(),
                        )
                    })?;
                self.observe_spawn_pane(&mut updated, observed).await?;
            }
            WorkerSpawnAction::CreateRejected { detail } => {
                if !matches!(
                    record.phase,
                    WorkerSpawnPhase::Launching | WorkerSpawnPhase::Uncertain
                ) || record.fence.is_some()
                    || panes
                        .iter()
                        .any(|p| p.worker_launch_id.as_deref() == Some(launch_id))
                {
                    return Err(CollaborationError::DispatchConflict(launch_id.into()));
                }
                updated.close(format!("mux create rejected: {detail}"));
            }
            WorkerSpawnAction::Uncertain { detail } => {
                match record.phase {
                    // No Begin means no physical effect was authorized. Close
                    // the token so late Begin cannot resurrect it.
                    WorkerSpawnPhase::Requested => {
                        updated.close(format!("failed before Begin: {detail}"));
                    }
                    WorkerSpawnPhase::Launching | WorkerSpawnPhase::Uncertain => {
                        updated.phase = WorkerSpawnPhase::Uncertain;
                        updated.detail = Some(detail.chars().take(512).collect());
                    }
                    _ => {}
                }
            }
            WorkerSpawnAction::Release { .. } => {
                unreachable!("release dispatched before taking the lock")
            }
        }
        self.persist_spawn(&updated)?;
        *record = updated;
        self.publish_change();
        Ok(())
    }

    /// A console/operator or the exact originating session can abandon a
    /// reservation. This is explicit recovery, never an automatic lease expiry.
    pub async fn release_worker_spawn(
        &self,
        caller: &Participant,
        launch_id: &str,
        reason: &str,
        panes: &[PaneInfo],
    ) -> Result<(), CollaborationError> {
        self.ensure_enabled()?;
        let reason = reason.trim();
        if reason.is_empty() || reason.len() > 512 {
            return Err(CollaborationError::WorkerLaunch(
                "release needs a reason (1-512 bytes)".into(),
            ));
        }
        let mut records = self.worker_spawns.lock().await;
        let record = records
            .iter_mut()
            .find(|r| r.launch_id == launch_id)
            .ok_or_else(|| {
                CollaborationError::WorkerLaunch(
                    "no such launch id; list launches with `muxa workers`".into(),
                )
            })?;
        if !caller.console && !record.owner.same_session(caller) {
            return Err(CollaborationError::ScopeDenied);
        }
        if record.phase == WorkerSpawnPhase::Failed {
            return Ok(());
        }
        if record.phase == WorkerSpawnPhase::Launching
            && record.lease_until > OffsetDateTime::now_utc()
        {
            return Err(CollaborationError::WorkerLaunch(
                "launch is still active; stop the launcher and let its lease expire before release"
                    .into(),
            ));
        }
        let fence = match record.fence.clone() {
            Some(fence) => Some(fence),
            None => self.worker_launch_fence(launch_id).await,
        };
        if let Some(fence) = fence {
            let (exited, location) = observe_fence(
                fence,
                record.agent_kind,
                record.pane.clone(),
                record.socket.clone(),
                panes.to_vec(),
            )
            .await;
            let moved = location
                .as_ref()
                .is_some_and(|p| !same_spawn_location(record, p));
            if !exited && !moved {
                return Err(CollaborationError::WorkerLaunch("worker process still matches or cannot be disproved; stop it or verify its move before release".into()));
            }
        } else {
            // Without a committed grant, a stamp is still evidence of a
            // live effect; inspect it without minting worker authority.
            let kind = record.agent_kind;
            let stamped: Vec<_> = panes
                .iter()
                .filter(|p| p.worker_launch_id.as_deref() == Some(launch_id))
                .cloned()
                .collect();
            if tokio::task::spawn_blocking(move || {
                stamped
                    .iter()
                    .any(|p| ProcessFence::observe(p, kind).is_some())
            })
            .await
            .unwrap_or(true)
            {
                return Err(CollaborationError::WorkerLaunch(
                    "stamped worker process is still live; inspect or stop it before release"
                        .into(),
                ));
            }
        }
        let mut updated = record.clone();
        updated.close(format!("released: {reason}"));
        self.persist_spawn(&updated)?;
        *record = updated;
        self.publish_change();
        Ok(())
    }

    async fn observe_spawn_pane(
        &self,
        record: &mut WorkerSpawnRecord,
        pane: &PaneInfo,
    ) -> Result<(), CollaborationError> {
        // Once committed, removal of a grant can be an explicit Primary
        // decision. An Observe retry must never recreate that authority.
        if record.fence.is_some() && self.worker_launch_fence(&record.launch_id).await.is_none() {
            return Err(CollaborationError::DispatchConflict(
                record.launch_id.clone(),
            ));
        }
        // The ordinary single-use process-fenced grant remains the authority.
        self.register_worker_launch(record.launch_id.clone(), pane.clone(), record.agent_kind)
            .await?;
        record.fence = self.worker_launch_fence(&record.launch_id).await;
        record.pane = Some(pane.pane_id.clone());
        record.socket.clone_from(&pane.socket);
        record.bound_session = self.worker_launch_binding(&record.launch_id).await;
        record.phase = if record.bound_session.is_some() {
            WorkerSpawnPhase::SessionBound
        } else {
            WorkerSpawnPhase::PaneObserved
        };
        record.detail = None;
        Ok(())
    }

    async fn reconcile_worker_spawn(
        &self,
        record: &mut WorkerSpawnRecord,
        participants: &[Participant],
        panes: &[PaneInfo],
    ) -> Result<(), CollaborationError> {
        let now = OffsetDateTime::now_utc();
        if record.phase == WorkerSpawnPhase::Requested {
            if record.lease_until <= now {
                // No physical effect was authorized. Late Begin will refuse.
                record.close("reservation expired before Begin".into());
            }
            return Ok(());
        }
        if record.pane.is_none() {
            let matches: Vec<_> = panes
                .iter()
                .filter(|p| {
                    p.worker_launch_id.as_deref() == Some(&record.launch_id)
                        && pane_room(&p.pane_id, p, p.socket.clone()) == record.room
                })
                .collect();
            if let Some(pane) = matches.first().filter(|first| {
                matches.iter().all(|p| {
                    p.pane_id == first.pane_id
                        && p.socket == first.socket
                        && p.pane_pid == first.pane_pid
                })
            }) {
                // A crash after stamping but before reporting can be recovered.
                // Failure to observe the process is uncertainty, never permission
                // to spawn a replacement.
                // A pane mark can outlive its process. Recover only a grant
                // already registered for this launch; never mint authority
                // from a stale stamp after a crash.
                if let Some(fence) = self.worker_launch_fence(&record.launch_id).await {
                    record.fence = Some(fence);
                    record.pane = Some(pane.pane_id.clone());
                    record.socket.clone_from(&pane.socket);
                    record.phase = WorkerSpawnPhase::PaneObserved;
                } else {
                    record.phase = WorkerSpawnPhase::Uncertain;
                    record.detail = Some(
                        "pane stamped but grant not committed; explicit reconciliation required"
                            .into(),
                    );
                }
            } else if record.lease_until <= now {
                record.phase = WorkerSpawnPhase::Uncertain;
                record.detail = Some(
                    "launch lease expired without an unambiguous pane; reconciliation required"
                        .into(),
                );
            }
        }
        if let Some(session) = self.worker_launch_binding(&record.launch_id).await {
            record.bound_session = Some(session);
        }
        // Grant commit and record commit are separate writes. Recover the
        // existing fence even if its stamped pane has moved out of this room.
        if record.fence.is_none() {
            record.fence = self.worker_launch_fence(&record.launch_id).await;
        }
        let Some(fence) = record.fence.clone() else {
            return Ok(());
        };
        let kind = record.agent_kind;
        let (exited, location) = observe_fence(
            fence,
            kind,
            record.pane.clone(),
            record.socket.clone(),
            panes.to_vec(),
        )
        .await;
        if record.pane.is_none() {
            if let Some(pane) = location.as_ref().filter(|p| same_spawn_location(record, p)) {
                record.pane = Some(pane.pane_id.clone());
                record.socket.clone_from(&pane.socket);
                record.phase = WorkerSpawnPhase::PaneObserved;
            }
        }
        if exited {
            record.close("observed worker process exited".into());
        } else if location
            .as_ref()
            .is_some_and(|p| !same_spawn_location(record, p))
        {
            // Positive process evidence in another room/endpoint is a move,
            // not uncertainty about whether the old room still owns capacity.
            record.close("worker moved to another room or endpoint".into());
        } else if location.is_none() {
            record.phase = WorkerSpawnPhase::Uncertain;
            record.detail = Some("worker endpoint no longer matches its process fence".into());
        } else if let Some(peer) = participants.iter().find(|p| {
            Some(&p.pane) == record.pane.as_ref()
                && p.socket == record.socket
                && p.agent_kind == kind
                && p.room == record.room
                && p.launch_purpose == LaunchPurpose::Worker
                && !is_pending_session(&p.agent_session_id)
        }) {
            record.bound_session = Some(peer.agent_session_id.clone());
            if matches!(peer.state, AgentState::Error | AgentState::Stopped) {
                record.close("worker became ineligible for reuse".into());
            } else {
                record.phase = WorkerSpawnPhase::SessionBound;
            }
        }
        Ok(())
    }
}

fn same_spawn_location(record: &WorkerSpawnRecord, pane: &PaneInfo) -> bool {
    pane_room(&pane.pane_id, pane, pane.socket.clone()) == record.room
        && record.pane.as_deref().is_none_or(|id| id == pane.pane_id)
        && record
            .socket
            .as_deref()
            .is_none_or(|socket| pane.socket.as_deref() == Some(socket))
}

async fn observe_fence(
    fence: ProcessFence,
    kind: AgentKind,
    pane: Option<String>,
    socket: Option<String>,
    panes: Vec<PaneInfo>,
) -> (bool, Option<PaneInfo>) {
    tokio::task::spawn_blocking(move || {
        let exited = fence.definitely_exited();
        let matches = |p: &&PaneInfo| ProcessFence::observe(p, kind).as_ref() == Some(&fence);
        // The fast path avoids probing unrelated panes on ordinary calls.
        if let Some(found) = pane
            .as_deref()
            .and_then(|id| super::unique_pane(id, socket.as_deref(), &panes))
            .filter(|p| matches(p))
        {
            return (exited, Some(found.clone()));
        }
        let locations: Vec<_> = panes.iter().filter(matches).collect();
        let location = locations
            .first()
            .filter(|first| {
                locations.iter().all(|p| {
                    p.pane_id == first.pane_id
                        && p.socket == first.socket
                        && pane_room(&p.pane_id, p, p.socket.clone())
                            == pane_room(&first.pane_id, first, first.socket.clone())
                })
            })
            .map(|p| (*p).clone());
        (exited, location)
    })
    .await
    .unwrap_or((false, None))
}

fn state_rank(state: AgentState) -> u8 {
    match state {
        AgentState::Idle => 0,
        AgentState::Working => 1,
        AgentState::Starting => 2,
        _ => 3,
    }
}

fn pending_worker(
    record: &WorkerSpawnRecord,
    participants: &[Participant],
    panes: &[PaneInfo],
) -> Option<Participant> {
    if !matches!(
        record.phase,
        WorkerSpawnPhase::PaneObserved | WorkerSpawnPhase::SessionBound
    ) {
        return None;
    }
    // A consumed grant cannot bootstrap a second conversation in the same
    // still-running CLI (/new may not have emitted its first hook yet).
    if record.bound_session.is_some() {
        return None;
    }
    let pane = super::unique_pane(record.pane.as_deref()?, record.socket.as_deref(), panes)?;
    if pane_room(&pane.pane_id, pane, pane.socket.clone()) != record.room {
        return None;
    }
    // In particular do not elevate a paneless Codex thread assigned by cwd,
    // or a new primary conversation started in the worker's old CLI.
    if participants.iter().any(|p| {
        p.pane == pane.pane_id
            && p.socket == pane.socket
            && !is_pending_session(&p.agent_session_id)
            && p.launch_purpose != LaunchPurpose::Worker
    }) {
        return None;
    }
    let mut peer = pane_participant(pane, pane.socket.clone(), None);
    peer.agent_kind = record.agent_kind;
    peer.launch_purpose = LaunchPurpose::Worker;
    Some(peer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collaboration::CollaborationOptions;

    fn caller() -> Participant {
        serde_json::from_value(serde_json::json!({
            "agent_kind":"claude_code", "agent_session_id":"owner", "pane":"%1",
            "socket":"default", "room":{"host":"tmux","socket":"default","window_id":"@1"},
            "state":"idle"
        }))
        .unwrap()
    }

    fn origin() -> CollaborationOrigin {
        CollaborationOrigin {
            exact_session: false,
            pane: "%1".into(),
            socket: Some("default".into()),
            console: false,
            agent_session_id: Some("owner".into()),
        }
    }

    fn spec(role: Option<&str>) -> WorkerSpawnSpec {
        WorkerSpawnSpec {
            agent_kind: AgentKind::Codex,
            role: role.map(str::to_owned),
            launch_role: role.unwrap_or("reviewer").into(),
        }
    }

    async fn acquire(store: &CollaborationStore, role: Option<&str>) -> WorkerSpawnAcquisition {
        store
            .acquire_worker_spawn(&caller(), origin(), spec(role), &[], &[])
            .await
            .unwrap()
    }

    fn ticket(acquisition: WorkerSpawnAcquisition) -> WorkerSpawnTicket {
        let WorkerSpawnAcquisition::LaunchToken { ticket } = acquisition else {
            panic!("expected a launch token")
        };
        ticket
    }

    async fn expire(store: &CollaborationStore) {
        let mut records = store.worker_spawns.lock().await;
        for record in records.iter_mut() {
            record.lease_until = OffsetDateTime::now_utc() - time::Duration::seconds(1);
            store.persist_spawn(record).unwrap();
        }
    }

    #[tokio::test]
    async fn concurrent_provider_and_role_calls_share_one_launch() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let (provider, role) =
            tokio::join!(acquire(&store, None), acquire(&store, Some("REVIEWER")));
        let outcomes = [provider, role];
        assert_eq!(
            outcomes
                .iter()
                .filter(|o| matches!(o, WorkerSpawnAcquisition::LaunchToken { .. }))
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|o| matches!(o, WorkerSpawnAcquisition::Pending { peer: None, .. }))
                .count(),
            1
        );
        assert_eq!(store.worker_spawns.lock().await.len(), 1);
        // A genuinely different role is separate capacity, not an affinity key.
        assert!(matches!(
            acquire(&store, Some("implementer")).await,
            WorkerSpawnAcquisition::LaunchToken { .. }
        ));
        assert!(matches!(
            acquire(&store, None).await,
            WorkerSpawnAcquisition::Pending { .. }
        ));
    }

    #[tokio::test]
    async fn expired_requested_can_be_replaced_but_old_begin_is_fenced() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let old = ticket(acquire(&store, None).await);
        expire(&store).await;
        let new = ticket(acquire(&store, None).await);
        assert_ne!(old.launch_id, new.launch_id);
        assert!(store
            .update_worker_spawn(&caller(), &old.launch_id, WorkerSpawnAction::Begin, &[])
            .await
            .is_err());
        store
            .update_worker_spawn(&caller(), &new.launch_id, WorkerSpawnAction::Begin, &[])
            .await
            .unwrap();
        // Retrying Begin can never authorize a second physical launch.
        assert!(store
            .update_worker_spawn(&caller(), &new.launch_id, WorkerSpawnAction::Begin, &[])
            .await
            .is_err());
    }

    #[tokio::test]
    async fn expired_launching_survives_restart_as_uncertain_not_a_new_token() {
        let dir = tempfile::tempdir().unwrap();
        let opts = CollaborationOptions {
            path: Some(dir.path().join("launches.sqlite3")),
            ..Default::default()
        };
        let store = CollaborationStore::load(opts.clone()).await.unwrap();
        let first = ticket(acquire(&store, None).await);
        store
            .update_worker_spawn(&caller(), &first.launch_id, WorkerSpawnAction::Begin, &[])
            .await
            .unwrap();
        store
            .update_worker_spawn(&caller(), &first.launch_id, WorkerSpawnAction::Renew, &[])
            .await
            .unwrap();
        expire(&store).await;
        drop(store);
        let store = CollaborationStore::load(opts).await.unwrap();
        for _ in 0..3 {
            let WorkerSpawnAcquisition::Pending {
                launch_id,
                phase,
                peer,
                ..
            } = acquire(&store, None).await
            else {
                panic!("uncertain launch must occupy capacity")
            };
            assert_eq!(launch_id, first.launch_id);
            assert_eq!(phase, WorkerSpawnPhase::Uncertain);
            assert!(peer.is_none());
        }
        assert!(store
            .update_worker_spawn(&caller(), &first.launch_id, WorkerSpawnAction::Begin, &[])
            .await
            .is_err());
        assert!(store
            .update_worker_spawn(&caller(), &first.launch_id, WorkerSpawnAction::Renew, &[])
            .await
            .is_err());
    }

    #[tokio::test]
    async fn worker_busy_reuse_and_primary_protection_are_preserved() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let mut peer = caller();
        peer.agent_kind = AgentKind::Codex;
        peer.agent_session_id = "busy-worker".into();
        peer.pane = "%2".into();
        peer.roles = vec!["reviewer".into()];
        peer.state = AgentState::Working;
        peer.launch_purpose = LaunchPurpose::Worker;
        assert!(
            matches!(store.acquire_worker_spawn(&caller(), origin(), spec(None), &[peer.clone()], &[]).await.unwrap(), WorkerSpawnAcquisition::Reuse { peer:p } if p.agent_session_id == "busy-worker")
        );
        peer.launch_purpose = LaunchPurpose::Primary;
        assert!(matches!(
            store
                .acquire_worker_spawn(&caller(), origin(), spec(None), &[peer], &[])
                .await
                .unwrap(),
            WorkerSpawnAcquisition::LaunchToken { .. }
        ));
    }

    #[tokio::test]
    async fn only_the_owner_in_the_same_room_can_begin_or_renew() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let first = ticket(acquire(&store, None).await);
        let mut stranger = caller();
        stranger.agent_session_id = "stranger".into();
        assert!(store
            .update_worker_spawn(&stranger, &first.launch_id, WorkerSpawnAction::Begin, &[])
            .await
            .is_err());
        let mut moved = caller();
        moved.room.window_id = "@2".into();
        assert!(store
            .update_worker_spawn(&moved, &first.launch_id, WorkerSpawnAction::Renew, &[])
            .await
            .is_err());
    }

    #[cfg(target_os = "linux")]
    struct Child(std::process::Child);
    #[cfg(target_os = "linux")]
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[cfg(target_os = "linux")]
    fn worker_pane(id: &str) -> (Child, PaneInfo) {
        let child = Child(
            std::process::Command::new("bash")
                .args(["-c", "exec -a 'codex --no-daemon' sleep 60"])
                .spawn()
                .unwrap(),
        );
        let pane = serde_json::from_value(serde_json::json!({
            "pane_id":"%2", "session":"main", "window_id":"@1", "window_index":"0", "pane_index":"1",
            "tty":"", "current_command":"codex", "title":"", "pane_pid":child.0.id(), "socket":"default",
            "agent_program":"codex", "agent_role":"reviewer", "worker_launch_id":id
        })).unwrap();
        // Some tests inspect an uncommitted launch directly, without the
        // registration path's own startup wait. Synchronize the fixture's exec.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while ProcessFence::observe(&pane, AgentKind::Codex).is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "worker fixture did not exec"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        (child, pane)
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn stamped_pane_without_grant_never_mints_permission_during_recovery() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let first = ticket(acquire(&store, None).await);
        store
            .update_worker_spawn(&caller(), &first.launch_id, WorkerSpawnAction::Begin, &[])
            .await
            .unwrap();
        let (_child, pane) = worker_pane(&first.launch_id);
        let result = store
            .acquire_worker_spawn(&caller(), origin(), spec(None), &[], &[pane])
            .await
            .unwrap();
        assert!(matches!(
            result,
            WorkerSpawnAcquisition::Pending {
                phase: WorkerSpawnPhase::Uncertain,
                peer: None,
                ..
            }
        ));
        assert!(store.worker_launch_fence(&first.launch_id).await.is_none());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn observed_pane_bootstraps_before_hook_but_never_promotes_paneless_or_new_sessions() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let first = ticket(acquire(&store, None).await);
        store
            .update_worker_spawn(&caller(), &first.launch_id, WorkerSpawnAction::Begin, &[])
            .await
            .unwrap();
        let (_child, pane) = worker_pane(&first.launch_id);
        let panes = [pane.clone()];
        store
            .update_worker_spawn(
                &caller(),
                &first.launch_id,
                WorkerSpawnAction::Observe {
                    pane: pane.pane_id.clone(),
                    socket: pane.socket.clone(),
                },
                &panes,
            )
            .await
            .unwrap();
        expire(&store).await;
        let pending = store
            .acquire_worker_spawn(&caller(), origin(), spec(Some("reviewer")), &[], &panes)
            .await
            .unwrap();
        assert!(
            matches!(pending, WorkerSpawnAcquisition::Pending { peer:Some(p), phase:WorkerSpawnPhase::PaneObserved, .. } if is_pending_session(&p.agent_session_id) && p.launch_purpose == LaunchPurpose::Worker)
        );
        let mut misattributed = caller();
        misattributed.pane = "%2".into();
        misattributed.agent_kind = AgentKind::Codex;
        misattributed.agent_session_id = "paneless-human".into();
        let mut peers = vec![misattributed];
        store.enrich_launch_purposes(&mut peers, &panes).await;
        assert_eq!(peers[0].launch_purpose, LaunchPurpose::Primary);
        assert!(matches!(
            store
                .acquire_worker_spawn(&caller(), origin(), spec(None), &peers, &panes)
                .await
                .unwrap(),
            WorkerSpawnAcquisition::Pending { peer: None, .. }
        ));
        let mut hook = crate::event::AgentId {
            kind: AgentKind::Codex,
            session_id: "actual-worker".into(),
            surface: None,
            pane: Some("%2".into()),
            tmux_socket: Some("default".into()),
            cwd: None,
        };
        store.observe_worker_hook(&hook).await.unwrap();
        assert!(matches!(
            store
                .acquire_worker_spawn(&caller(), origin(), spec(None), &[], &panes)
                .await
                .unwrap(),
            WorkerSpawnAcquisition::Pending { peer: None, .. }
        ));
        peers[0].agent_session_id = "actual-worker".into();
        store.enrich_launch_purposes(&mut peers, &panes).await;
        assert_eq!(peers[0].launch_purpose, LaunchPurpose::Worker);
        assert!(matches!(
            store
                .acquire_worker_spawn(&caller(), origin(), spec(None), &peers, &panes)
                .await
                .unwrap(),
            WorkerSpawnAcquisition::Reuse { .. }
        ));
        hook.session_id = "new-conversation".into();
        store.observe_worker_hook(&hook).await.unwrap();
        peers[0].agent_session_id = hook.session_id;
        store.enrich_launch_purposes(&mut peers, &panes).await;
        assert_eq!(peers[0].launch_purpose, LaunchPurpose::Primary);
        assert!(matches!(
            store
                .acquire_worker_spawn(&caller(), origin(), spec(None), &peers, &panes)
                .await
                .unwrap(),
            WorkerSpawnAcquisition::Pending { peer: None, .. }
        ));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn grant_commit_crash_reconciles_and_dead_fence_allows_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let opts = CollaborationOptions {
            path: Some(dir.path().join("recovery.sqlite3")),
            ..Default::default()
        };
        let store = CollaborationStore::load(opts.clone()).await.unwrap();
        let first = ticket(acquire(&store, None).await);
        store
            .update_worker_spawn(&caller(), &first.launch_id, WorkerSpawnAction::Begin, &[])
            .await
            .unwrap();
        let (mut child, pane) = worker_pane(&first.launch_id);
        store
            .register_worker_launch(first.launch_id.clone(), pane.clone(), AgentKind::Codex)
            .await
            .unwrap();
        // Grant committed, but the launch record's PaneObserved write did not.
        drop(store);
        let store = CollaborationStore::load(opts).await.unwrap();
        assert!(matches!(
            store
                .acquire_worker_spawn(
                    &caller(),
                    origin(),
                    spec(None),
                    &[],
                    std::slice::from_ref(&pane)
                )
                .await
                .unwrap(),
            WorkerSpawnAcquisition::Pending { peer: Some(_), .. }
        ));
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        assert!(
            matches!(store.acquire_worker_spawn(&caller(), origin(), spec(None), &[], &[pane]).await.unwrap(), WorkerSpawnAcquisition::LaunchToken { ticket } if ticket.launch_id != first.launch_id)
        );
    }
    #[tokio::test]
    async fn pre_begin_failure_closes_capacity_and_fences_late_begin() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let first = ticket(acquire(&store, None).await);
        store
            .update_worker_spawn(
                &caller(),
                &first.launch_id,
                WorkerSpawnAction::Uncertain {
                    detail: "unsupported host / invalid account / prepare_launch failed".into(),
                },
                &[],
            )
            .await
            .unwrap();
        assert_eq!(
            store.worker_spawn_records().await[0].phase,
            WorkerSpawnPhase::Failed
        );
        assert!(store
            .update_worker_spawn(&caller(), &first.launch_id, WorkerSpawnAction::Begin, &[])
            .await
            .is_err());
        assert!(
            matches!(acquire(&store, None).await, WorkerSpawnAcquisition::LaunchToken { ticket } if ticket.launch_id != first.launch_id)
        );
    }

    #[tokio::test]
    async fn rejected_mux_create_closes_capacity_even_after_begin() {
        for lease_expired in [false, true] {
            let store = CollaborationStore::in_memory(CollaborationOptions::default());
            let first = ticket(acquire(&store, None).await);
            store
                .update_worker_spawn(&caller(), &first.launch_id, WorkerSpawnAction::Begin, &[])
                .await
                .unwrap();
            if lease_expired {
                expire(&store).await;
                let _ = acquire(&store, None).await;
                assert_eq!(
                    store.worker_spawn_records().await[0].phase,
                    WorkerSpawnPhase::Uncertain
                );
            }
            store
                .update_worker_spawn(
                    &caller(),
                    &first.launch_id,
                    WorkerSpawnAction::CreateRejected {
                        detail: "target pane vanished; command returned 1 without pane id".into(),
                    },
                    &[],
                )
                .await
                .unwrap();
            // The MCP's generic error report must not reopen a known failure.
            store
                .update_worker_spawn(
                    &caller(),
                    &first.launch_id,
                    WorkerSpawnAction::Uncertain {
                        detail: "spawn_peer returned an error".into(),
                    },
                    &[],
                )
                .await
                .unwrap();
            assert_eq!(
                store.worker_spawn_records().await[0].phase,
                WorkerSpawnPhase::Failed
            );
            assert!(matches!(
                acquire(&store, None).await,
                WorkerSpawnAcquisition::LaunchToken { .. }
            ));
        }
    }

    #[cfg(target_os = "linux")]
    async fn observed_worker() -> (
        std::sync::Arc<CollaborationStore>,
        WorkerSpawnTicket,
        Child,
        PaneInfo,
    ) {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let first = ticket(acquire(&store, None).await);
        store
            .update_worker_spawn(&caller(), &first.launch_id, WorkerSpawnAction::Begin, &[])
            .await
            .unwrap();
        let (child, pane) = worker_pane(&first.launch_id);
        store
            .update_worker_spawn(
                &caller(),
                &first.launch_id,
                WorkerSpawnAction::Observe {
                    pane: pane.pane_id.clone(),
                    socket: pane.socket.clone(),
                },
                std::slice::from_ref(&pane),
            )
            .await
            .unwrap();
        (store, first, child, pane)
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn positively_moved_live_worker_releases_original_capacity() {
        for change_room in [true, false] {
            let (store, first, _child, mut pane) = observed_worker().await;
            if change_room {
                pane.window_id = "@2".into();
            } else {
                pane.pane_id = "%9".into();
            }
            // Process fence stays identical: the worker moved, it did not die.
            assert!(
                matches!(store.acquire_worker_spawn(&caller(), origin(), spec(None), &[], &[pane]).await.unwrap(), WorkerSpawnAcquisition::LaunchToken { ticket } if ticket.launch_id != first.launch_id)
            );
            assert!(store
                .worker_spawn_records()
                .await
                .iter()
                .any(|r| r.launch_id == first.launch_id
                    && r.phase == WorkerSpawnPhase::Failed
                    && r.detail.as_ref().is_some_and(|d| d.contains("moved"))));
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn uncertain_release_checks_ownership_and_live_process_then_allows_recovery() {
        let (store, first, mut child, pane) = observed_worker().await;
        // An incomplete inventory makes this Uncertain, not dead.
        assert!(matches!(
            acquire(&store, None).await,
            WorkerSpawnAcquisition::Pending {
                phase: WorkerSpawnPhase::Uncertain,
                ..
            }
        ));
        let action = || WorkerSpawnAction::Release {
            reason: "inspected the launch and stopped its launcher".into(),
        };
        assert!(
            store
                .update_worker_spawn(
                    &caller(),
                    &first.launch_id,
                    action(),
                    std::slice::from_ref(&pane)
                )
                .await
                .is_err(),
            "a matching live fence must block release"
        );
        assert!(
            store
                .update_worker_spawn(&caller(), &first.launch_id, action(), &[])
                .await
                .is_err(),
            "missing inventory is not proof of death"
        );
        let mut stranger = caller();
        stranger.agent_session_id = "stranger".into();
        assert!(store
            .update_worker_spawn(&stranger, &first.launch_id, action(), &[])
            .await
            .is_err());
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        store
            .update_worker_spawn(&caller(), &first.launch_id, action(), &[pane])
            .await
            .unwrap();
        assert!(matches!(
            acquire(&store, None).await,
            WorkerSpawnAcquisition::LaunchToken { .. }
        ));
        assert!(store
            .update_worker_spawn(&caller(), &first.launch_id, WorkerSpawnAction::Begin, &[])
            .await
            .is_err());
    }

    #[tokio::test]
    async fn operator_can_release_unknown_effects_but_not_an_active_launch() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let first = ticket(acquire(&store, None).await);
        store
            .update_worker_spawn(&caller(), &first.launch_id, WorkerSpawnAction::Begin, &[])
            .await
            .unwrap();
        let console = Participant::console(caller().room);
        assert!(store
            .release_worker_spawn(
                &console,
                &first.launch_id,
                "confirmed old launcher stopped",
                &[]
            )
            .await
            .is_err());
        expire(&store).await;
        let _ = acquire(&store, None).await;
        assert!(store
            .release_worker_spawn(&console, &first.launch_id, " ", &[])
            .await
            .is_err());
        store
            .release_worker_spawn(
                &console,
                &first.launch_id,
                "confirmed old launcher stopped; no worker process exists",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(
            store.worker_spawn_records().await[0].phase,
            WorkerSpawnPhase::Failed
        );
        assert!(matches!(
            acquire(&store, None).await,
            WorkerSpawnAcquisition::LaunchToken { .. }
        ));
    }

    #[tokio::test]
    async fn old_closed_records_are_pruned_but_unknown_effects_survive_restart() {
        let dir = tempfile::tempdir().unwrap();
        let opts = CollaborationOptions {
            path: Some(dir.path().join("retention.sqlite3")),
            ..Default::default()
        };
        let store = CollaborationStore::load(opts.clone()).await.unwrap();
        let old = ticket(acquire(&store, None).await);
        store
            .update_worker_spawn(
                &caller(),
                &old.launch_id,
                WorkerSpawnAction::Uncertain {
                    detail: "failed before Begin".into(),
                },
                &[],
            )
            .await
            .unwrap();
        {
            let mut records = store.worker_spawns.lock().await;
            records[0].closed_at =
                Some(OffsetDateTime::now_utc() - time::Duration::days(CLOSED_RETENTION_DAYS + 1));
            store.persist_spawn(&records[0]).unwrap();
        }
        let pending = ticket(acquire(&store, None).await);
        assert!(store
            .worker_spawn_records()
            .await
            .iter()
            .all(|r| r.launch_id != old.launch_id));
        store
            .update_worker_spawn(&caller(), &pending.launch_id, WorkerSpawnAction::Begin, &[])
            .await
            .unwrap();
        {
            let mut records = store.worker_spawns.lock().await;
            records[0].phase = WorkerSpawnPhase::Uncertain;
            records[0].lease_until = OffsetDateTime::now_utc() - time::Duration::days(365);
            store.persist_spawn(&records[0]).unwrap();
        }
        {
            // Also exercise load-time retention, independent of acquire's
            // in-memory cleanup. Unknown effects remain alongside this tombstone.
            let mut closed = store.worker_spawns.lock().await[0].clone();
            closed.launch_id = uuid::Uuid::new_v4().to_string();
            closed.close("historical failure".into());
            closed.closed_at =
                Some(OffsetDateTime::now_utc() - time::Duration::days(CLOSED_RETENTION_DAYS + 1));
            store.persist_spawn(&closed).unwrap();
        }
        drop(store);
        let store = CollaborationStore::load(opts).await.unwrap();
        assert_eq!(store.worker_spawn_records().await.len(), 1);
        assert!(
            matches!(acquire(&store, None).await, WorkerSpawnAcquisition::Pending { launch_id, .. } if launch_id == pending.launch_id)
        );
        assert!(store
            .update_worker_spawn(&caller(), &old.launch_id, WorkerSpawnAction::Begin, &[])
            .await
            .is_err());
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn release_cannot_ignore_a_stamped_live_process_without_a_committed_grant() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let first = ticket(acquire(&store, None).await);
        store
            .update_worker_spawn(&caller(), &first.launch_id, WorkerSpawnAction::Begin, &[])
            .await
            .unwrap();
        let (mut child, pane) = worker_pane(&first.launch_id);
        store
            .update_worker_spawn(
                &caller(),
                &first.launch_id,
                WorkerSpawnAction::Uncertain {
                    detail: "crashed after stamp".into(),
                },
                &[],
            )
            .await
            .unwrap();
        assert!(store
            .release_worker_spawn(
                &caller(),
                &first.launch_id,
                "investigated",
                std::slice::from_ref(&pane)
            )
            .await
            .is_err());
        assert!(
            store.worker_launch_fence(&first.launch_id).await.is_none(),
            "release must not mint a grant to inspect a stamp"
        );
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        store
            .release_worker_spawn(
                &caller(),
                &first.launch_id,
                "stamped process was stopped",
                &[pane],
            )
            .await
            .unwrap();
        assert!(matches!(
            acquire(&store, None).await,
            WorkerSpawnAcquisition::LaunchToken { .. }
        ));
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn committed_grant_with_missing_record_endpoint_can_still_follow_a_move() {
        let (store, first, _child, mut pane) = observed_worker().await;
        {
            // Crash after the grant write but before the launch record write.
            let mut records = store.worker_spawns.lock().await;
            records[0].fence = None;
            records[0].pane = None;
            records[0].socket = None;
            records[0].phase = WorkerSpawnPhase::Launching;
        }
        pane.window_id = "@moved".into();
        assert!(
            matches!(store.acquire_worker_spawn(&caller(), origin(), spec(None), &[], &[pane]).await.unwrap(), WorkerSpawnAcquisition::LaunchToken { ticket } if ticket.launch_id != first.launch_id)
        );
    }
}
