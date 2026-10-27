//! Daemon-wide worker capacity reservations. A lease expiry is not evidence
//! that a physical launch failed: uncertain launches continue to occupy capacity.
use super::purpose::ProcessFence;
use super::{
    is_pending_session, open_database, pane_participant, pane_room, secure_database_files,
    CollaborationError, CollaborationOrigin, CollaborationStore, LaunchPurpose, Participant,
    RoomId,
};
use crate::{
    event::{AgentKind, AgentState},
    tmux::PaneInfo,
};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::path::Path;
use time::OffsetDateTime;

pub const WORKER_SPAWN_CAPABILITY: &str = "collaboration_worker_spawn_v1";
const LEASE_SECONDS: i64 = 30;

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
            return Err(CollaborationError::UnknownTarget(
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
            return Err(CollaborationError::UnknownTarget(
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
    Uncertain {
        detail: String,
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
}

pub(super) fn load_spawns(
    path: Option<&Path>,
) -> Result<Vec<WorkerSpawnRecord>, CollaborationError> {
    let Some(path) = path else {
        return Ok(Vec::new());
    };
    let connection = open_database(path)?;
    let mut statement =
        connection.prepare("SELECT payload FROM collaboration_worker_spawns ORDER BY rowid")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
}

impl CollaborationStore {
    fn persist_spawn(&self, record: &WorkerSpawnRecord) -> Result<(), CollaborationError> {
        if let Some(path) = &self.database_path {
            open_database(path)?.execute(
                "INSERT OR REPLACE INTO collaboration_worker_spawns(id,payload) VALUES (?1,?2)",
                params![record.launch_id, serde_json::to_string(record)?],
            )?;
            secure_database_files(path)?;
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
        let mut records = self.worker_spawns.lock().await;
        let record = records
            .iter_mut()
            .find(|r| r.launch_id == launch_id)
            .ok_or_else(|| CollaborationError::UnknownTarget("unknown worker launch".into()))?;
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
                        CollaborationError::UnknownTarget(
                            "worker launch stamp absent or ambiguous".into(),
                        )
                    })?;
                self.observe_spawn_pane(&mut updated, observed).await?;
            }
            WorkerSpawnAction::Uncertain { detail } => {
                if matches!(
                    record.phase,
                    WorkerSpawnPhase::Requested
                        | WorkerSpawnPhase::Launching
                        | WorkerSpawnPhase::Uncertain
                ) {
                    updated.phase = WorkerSpawnPhase::Uncertain;
                    updated.detail = Some(detail.chars().take(512).collect());
                }
            }
        }
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
                record.phase = WorkerSpawnPhase::Failed;
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
        let Some(fence) = record.fence.clone() else {
            return Ok(());
        };
        let pane = record.pane.clone();
        let socket = record.socket.clone();
        let kind = record.agent_kind;
        let observed = panes.to_vec();
        let room = record.room.clone();
        let (exited, matches) = tokio::task::spawn_blocking(move || {
            (
                fence.definitely_exited(),
                pane.as_deref()
                    .and_then(|id| super::unique_pane(id, socket.as_deref(), &observed))
                    .is_some_and(|p| {
                        pane_room(&p.pane_id, p, p.socket.clone()) == room
                            && ProcessFence::observe(p, kind).as_ref() == Some(&fence)
                    }),
            )
        })
        .await
        .unwrap_or((false, false));
        if exited {
            record.phase = WorkerSpawnPhase::Failed;
            record.detail = Some("observed worker process exited".into());
        } else if !matches {
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
            record.phase = if matches!(peer.state, AgentState::Error | AgentState::Stopped) {
                WorkerSpawnPhase::Failed
            } else {
                WorkerSpawnPhase::SessionBound
            };
        }
        Ok(())
    }
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
}
