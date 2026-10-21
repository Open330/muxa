//! Explicit session purposes and single-use, process-fenced worker launches.
use super::{
    is_pending_session, open_database, pane_participant, pane_room, secure_database_files,
    unique_pane, CollaborationError, CollaborationStore, Participant, RoomId,
};
use crate::{event::AgentKind, tmux::PaneInfo};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, path::Path};

pub const HUMAN_PRIORITY_CAPABILITY: &str = "collaboration_human_priority_v1";

/// `hello` capability: the operator may set the purpose of exact live agent
/// sessions it names, from a terminal that is not itself an agent pane.
pub const PURPOSE_PROMOTE_CAPABILITY: &str = "collaboration_purpose_promote_v1";

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LaunchPurpose {
    #[default]
    Primary,
    Worker,
}

impl std::str::FromStr for LaunchPurpose {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "primary" => Ok(Self::Primary),
            "worker" => Ok(Self::Worker),
            _ => Err("purpose must be primary or worker".into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ProcessFence {
    pid: u32,
    birth: String,
}

impl ProcessFence {
    fn observe(pane: &PaneInfo, kind: AgentKind) -> Option<Self> {
        #[cfg(target_os = "linux")]
        let root = crate::process_tree::read_proc(pane.pane_pid, 0);
        #[cfg(not(target_os = "linux"))]
        let root = crate::process_snapshot::read_current_process_table()
            .get(pane.pane_pid)
            .cloned();
        let pid = if root
            .as_ref()
            .is_some_and(|root| crate::account_swap::is_agent_process(root, kind))
        {
            pane.pane_pid
        } else {
            let processes = crate::process_tree::read_descendants(pane.pane_pid);
            crate::account_swap::find_launch(&processes, pane.pane_pid, kind)?.agent_pid
        };
        if kind == AgentKind::Codex {
            #[cfg(target_os = "linux")]
            let process = crate::process_tree::read_proc(pid, 0)?;
            #[cfg(not(target_os = "linux"))]
            let process = crate::process_snapshot::read_current_process_table()
                .get(pid)?
                .clone();
            if !process
                .cmdline
                .split_whitespace()
                .any(|arg| arg == "--no-daemon")
            {
                return None;
            }
        }
        Some(Self {
            pid,
            birth: process_birth(pid)?,
        })
    }
}

fn process_birth(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let ticks = stat.rsplit_once(')')?.1.split_whitespace().nth(19)?;
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
        Some(format!("{}:{ticks}", boot.trim()))
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let output = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "lstart="])
            .output()
            .ok()?;
        let birth = String::from_utf8(output.stdout).ok()?;
        (output.status.success() && !birth.trim().is_empty()).then(|| birth.trim().to_owned())
    }
    #[cfg(windows)]
    {
        crate::process_snapshot::start_time(pid).map(|value| value.to_string())
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct PurposeGrant {
    id: String,
    room: RoomId,
    pane: String,
    socket: Option<String>,
    kind: AgentKind,
    purpose: LaunchPurpose,
    fence: Option<ProcessFence>,
    consumed_by: Option<String>,
}

impl PurposeGrant {
    fn owns_session(&self, participant: &Participant) -> bool {
        self.consumed_by.as_deref() == Some(&participant.agent_session_id)
            && self.room.host == participant.room.host
            && self.socket == participant.socket
            && self.kind == participant.agent_kind
    }
}

pub(super) fn load_purposes(path: Option<&Path>) -> Result<Vec<PurposeGrant>, CollaborationError> {
    let Some(path) = path else {
        return Ok(Vec::new());
    };
    let connection = open_database(path)?;
    let mut statement =
        connection.prepare("SELECT payload FROM collaboration_purposes ORDER BY rowid")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
}

fn verify_pending_grants(
    pending: Vec<PurposeGrant>,
    observed_panes: &[PaneInfo],
) -> (HashSet<String>, HashSet<String>) {
    let (pending, expired): (Vec<_>, Vec<_>) = pending.into_iter().partition(|grant| {
        grant
            .fence
            .as_ref()
            .is_some_and(|fence| process_birth(fence.pid).as_ref() == Some(&fence.birth))
    });
    let verified = pending
        .into_iter()
        .filter(|grant| {
            let Some(pane) = unique_pane(&grant.pane, grant.socket.as_deref(), observed_panes)
            else {
                return false;
            };
            pane_room(&pane.pane_id, pane, pane.socket.clone()) == grant.room
                && ProcessFence::observe(pane, grant.kind).as_ref() == grant.fence.as_ref()
        })
        .map(|grant| grant.id)
        .collect::<HashSet<_>>();
    (
        verified,
        expired
            .into_iter()
            .map(|grant| grant.id)
            .collect::<HashSet<_>>(),
    )
}

impl CollaborationStore {
    fn persist_purposes(
        &self,
        grants: &[PurposeGrant],
        previous: &[PurposeGrant],
    ) -> Result<(), CollaborationError> {
        let Some(path) = &self.database_path else {
            return Ok(());
        };
        let mut connection = open_database(path)?;
        let tx = connection.transaction()?;
        for old in previous
            .iter()
            .filter(|old| !grants.iter().any(|grant| grant.id == old.id))
        {
            tx.execute(
                "DELETE FROM collaboration_purposes WHERE id = ?1",
                [&old.id],
            )?;
        }
        for grant in grants.iter().filter(|grant| !previous.contains(grant)) {
            tx.execute(
                "INSERT OR REPLACE INTO collaboration_purposes(id,payload) VALUES (?1,?2)",
                params![grant.id, serde_json::to_string(grant)?],
            )?;
        }
        tx.commit()?;
        secure_database_files(path)?;
        Ok(())
    }

    /// Register an explicit worker launch, observing its actual CLI process.
    /// A retry with the same id is idempotent and cannot grant a second session.
    pub async fn register_worker_launch(
        &self,
        id: String,
        pane: PaneInfo,
        kind: AgentKind,
    ) -> Result<(), CollaborationError> {
        self.ensure_enabled()?;
        uuid::Uuid::parse_str(&id).map_err(|_| {
            CollaborationError::UnknownTarget("worker launch id must be a UUID".into())
        })?;
        let observed = pane.clone();
        let fence = tokio::task::spawn_blocking(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            loop {
                if let Some(fence) = ProcessFence::observe(&observed, kind) {
                    return Some(fence);
                }
                if std::time::Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        })
        .await
        .ok()
        .flatten()
        .ok_or_else(|| {
            CollaborationError::UnknownTarget(
                "worker CLI process is not observable yet; no worker permission granted".into(),
            )
        })?;
        let _transaction = self.transaction_lock.lock().await;
        let mut grants = self.purposes.write().await;
        if let Some(existing) = grants.iter().find(|grant| grant.id == id) {
            return if existing.pane == pane.pane_id
                && existing.socket == pane.socket
                && existing.fence.as_ref() == Some(&fence)
                && existing.kind == kind
            {
                Ok(())
            } else {
                Err(CollaborationError::DispatchConflict(id))
            };
        }
        let previous = grants.clone();
        grants.push(PurposeGrant {
            id,
            room: pane_room(&pane.pane_id, &pane, pane.socket.clone()),
            pane: pane.pane_id,
            socket: pane.socket,
            kind,
            purpose: LaunchPurpose::Worker,
            fence: Some(fence),
            consumed_by: None,
        });
        if let Err(error) = self.persist_purposes(&grants, &previous) {
            *grants = previous;
            return Err(error);
        }
        drop(grants);
        self.publish_change();
        Ok(())
    }

    /// Explicit self-session purpose changes never affect a later /new session.
    pub async fn set_purpose(
        &self,
        caller: &Participant,
        purpose: LaunchPurpose,
    ) -> Result<(), CollaborationError> {
        self.set_purposes(std::slice::from_ref(caller), purpose)
            .await
    }

    /// Set the purpose of each listed live agent session in one write. The
    /// caller decides who may do this: an agent for its own session, or the
    /// operator for exact sessions it named (see
    /// [`PURPOSE_PROMOTE_CAPABILITY`]).
    pub async fn set_purposes(
        &self,
        sessions: &[Participant],
        purpose: LaunchPurpose,
    ) -> Result<(), CollaborationError> {
        self.ensure_enabled()?;
        if sessions
            .iter()
            .any(|session| session.console || is_pending_session(&session.agent_session_id))
        {
            return Err(CollaborationError::ConsoleIdentity);
        }
        let _transaction = self.transaction_lock.lock().await;
        let mut grants = self.purposes.write().await;
        let previous = grants.clone();
        for caller in sessions {
            grants.retain(|grant| {
                !(grant.owns_session(caller)
                    || grant.consumed_by.is_none()
                        && grant.pane == caller.pane
                        && grant.socket == caller.socket)
            });
            grants.push(PurposeGrant {
                id: uuid::Uuid::new_v4().to_string(),
                room: caller.room.clone(),
                pane: caller.pane.clone(),
                socket: caller.socket.clone(),
                kind: caller.agent_kind,
                purpose,
                fence: None,
                consumed_by: Some(caller.agent_session_id.clone()),
            });
        }
        if let Err(error) = self.persist_purposes(&grants, &previous) {
            *grants = previous;
            return Err(error);
        }
        drop(grants);
        self.publish_change();
        Ok(())
    }

    /// Only an isolated Codex CLI's raw pane-bearing hook can consume its
    /// launch grant. A paneless thread assigned by cwd elimination cannot.
    pub async fn observe_worker_hook(
        &self,
        id: &crate::event::AgentId,
    ) -> Result<(), CollaborationError> {
        let (Some(pane), Some(socket)) = (&id.pane, &id.tmux_socket) else {
            return Ok(());
        };
        if id.kind != AgentKind::Codex
            || id.session_id.is_empty()
            || is_pending_session(&id.session_id)
            || id
                .session_id
                .starts_with(crate::state::SYNTHETIC_SESSION_PREFIX)
        {
            return Ok(());
        }
        let candidates: Vec<_> = self
            .purposes
            .read()
            .await
            .iter()
            .filter(|grant| {
                grant.consumed_by.is_none()
                    && grant.kind == id.kind
                    && grant.pane == *pane
                    && grant.socket.as_deref().is_some_and(|endpoint| {
                        crate::backend::pane_endpoints_match(Some(pane), endpoint, socket)
                    })
            })
            .cloned()
            .collect();
        if candidates.is_empty() {
            return Ok(());
        }
        let valid = tokio::task::spawn_blocking(move || {
            candidates
                .into_iter()
                .filter(|grant| {
                    grant.fence.as_ref().is_some_and(|fence| {
                        process_birth(fence.pid).as_ref() == Some(&fence.birth)
                    })
                })
                .map(|grant| grant.id)
                .collect::<HashSet<_>>()
        })
        .await
        .unwrap_or_default();
        let _transaction = self.transaction_lock.lock().await;
        let mut grants = self.purposes.write().await;
        let previous = grants.clone();
        if let Some(grant) = grants
            .iter_mut()
            .find(|grant| grant.consumed_by.is_none() && valid.contains(&grant.id))
        {
            grant.consumed_by = Some(id.session_id.clone());
            if let Err(error) = self.persist_purposes(&grants, &previous) {
                *grants = previous;
                return Err(error);
            }
            drop(grants);
            self.publish_change();
        }
        Ok(())
    }

    #[cfg(test)]
    async fn has_pending_worker_launches(&self) -> bool {
        self.purposes
            .read()
            .await
            .iter()
            .any(|grant| grant.consumed_by.is_none())
    }

    pub async fn enrich_launch_purposes(
        &self,
        participants: &mut Vec<Participant>,
        panes: &[PaneInfo],
    ) {
        // Probe only unconsumed launch grants, outside the store transaction.
        let pending: Vec<_> = self
            .purposes
            .read()
            .await
            .iter()
            .filter(|grant| grant.consumed_by.is_none())
            .cloned()
            .collect();
        if pending.is_empty() {
            let grants = self.purposes.read().await;
            for participant in participants {
                participant.launch_purpose = grants
                    .iter()
                    .rev()
                    .find(|grant| grant.owns_session(participant))
                    .map_or(LaunchPurpose::Primary, |grant| grant.purpose);
            }
            return;
        }
        let observed_panes = panes.to_vec();
        let (verified, expired) =
            tokio::task::spawn_blocking(move || verify_pending_grants(pending, &observed_panes))
                .await
                .unwrap_or_default();
        let _transaction = self.transaction_lock.lock().await;
        let mut grants = self.purposes.write().await;
        let previous = grants.clone();
        grants.retain(|grant| !expired.contains(&grant.id));
        let mut changed = !expired.is_empty();
        // The process-fenced launch itself is sufficient evidence for a pending
        // worker, even before ordinary discovery has classified a slow wrapper.
        for grant in grants
            .iter()
            .filter(|grant| grant.consumed_by.is_none() && verified.contains(&grant.id))
        {
            if !participants
                .iter()
                .any(|p| p.pane == grant.pane && p.socket == grant.socket)
            {
                if let Some(pane) = unique_pane(&grant.pane, grant.socket.as_deref(), panes) {
                    let mut pending = pane_participant(pane, pane.socket.clone(), None);
                    pending.agent_kind = grant.kind;
                    participants.push(pending);
                }
            }
        }
        for participant in participants.iter_mut() {
            participant.launch_purpose = LaunchPurpose::Primary;
            if let Some(grant) = grants
                .iter()
                .rev()
                .find(|grant| grant.owns_session(participant))
            {
                participant.launch_purpose = grant.purpose;
                continue;
            }
            if let Some(grant) = grants.iter_mut().find(|grant| {
                grant.consumed_by.is_none()
                    && verified.contains(&grant.id)
                    && grant.pane == participant.pane
                    && grant.socket == participant.socket
                    && grant.room == participant.room
                    && grant.kind == participant.agent_kind
            }) {
                if grant.kind == AgentKind::Codex
                    && !is_pending_session(&participant.agent_session_id)
                {
                    continue;
                }
                participant.launch_purpose = LaunchPurpose::Worker;
                if !is_pending_session(&participant.agent_session_id)
                    && !participant
                        .agent_session_id
                        .starts_with(crate::state::SYNTHETIC_SESSION_PREFIX)
                {
                    grant.consumed_by = Some(participant.agent_session_id.clone());
                    changed = true;
                }
            }
        }
        if changed {
            if let Err(error) = self.persist_purposes(&grants, &previous) {
                *grants = previous;
                for participant in participants {
                    participant.launch_purpose = LaunchPurpose::Primary;
                }
                tracing::warn!(%error, "worker session binding failed; keeping sessions protected");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collaboration::{CollaborationOptions, NewRequest, RequestStatus};

    fn participant(session: &str) -> Participant {
        serde_json::from_value(serde_json::json!({
            "agent_kind":"codex", "agent_session_id":session, "pane":"%2",
            "socket":"default", "room":{"host":"tmux","socket":"default","window_id":"@1"},
            "state":"idle"
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn purpose_survives_restart_but_never_a_new_session() {
        let dir = tempfile::tempdir().unwrap();
        let options = CollaborationOptions {
            path: Some(dir.path().join("mail.sqlite3")),
            ..Default::default()
        };
        let store = CollaborationStore::load(options.clone()).await.unwrap();
        let current = participant("worker-session");
        store
            .set_purpose(&current, LaunchPurpose::Worker)
            .await
            .unwrap();
        drop(store);
        let store = CollaborationStore::load(options).await.unwrap();
        let mut moved = current.clone();
        moved.pane = "%9".into();
        moved.room.window_id = "@99".into();
        let mut participants = vec![moved, participant("new-session")];
        store.enrich_launch_purposes(&mut participants, &[]).await;
        assert_eq!(participants[0].launch_purpose, LaunchPurpose::Worker);
        assert_eq!(participants[1].launch_purpose, LaunchPurpose::Primary);
        store
            .set_purpose(&current, LaunchPurpose::Primary)
            .await
            .unwrap();
        store.enrich_launch_purposes(&mut participants, &[]).await;
        assert!(participants
            .iter()
            .all(|p| p.launch_purpose == LaunchPurpose::Primary));
    }

    #[tokio::test]
    async fn protected_inbox_requires_exact_acceptance_and_keeps_claimed_work() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let sender = participant("sender");
        let recipient = participant("recipient");
        let first = store
            .create(
                sender.clone(),
                recipient.clone(),
                NewRequest {
                    body: "first".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let second = store
            .create(
                sender,
                recipient.clone(),
                NewRequest {
                    body: "second".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(store.claim_for(&recipient).await.unwrap().is_empty());
        assert!(store
            .prepare_direct_wake(&recipient, &first.id)
            .await
            .unwrap()
            .is_none());
        let claimed = store.claim_by_id(&recipient, &first.id).await.unwrap();
        assert_eq!(claimed.status, RequestStatus::Claimed);
        let inbox = store.claim_for(&recipient).await.unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].id, first.id);
        assert_eq!(
            store.peek_for(&recipient, &second.id).await.unwrap().status,
            RequestStatus::Queued
        );
        assert!(store
            .claim_by_id(&participant("stranger"), &second.id)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn console_work_remains_deliverable_to_primary() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let recipient = participant("recipient");
        let sender = Participant::console(recipient.room.clone());
        let request = store
            .create(
                sender,
                recipient.clone(),
                NewRequest {
                    body: "human task".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(store.claim_for(&recipient).await.unwrap()[0].id, request.id);
    }

    #[test]
    fn process_identity_is_stable_and_missing_process_has_no_grant() {
        assert!(process_birth(0).is_none());
        assert!(process_birth(u32::MAX).is_none());
        assert_eq!(
            process_birth(std::process::id()),
            process_birth(std::process::id())
        );
        assert!(process_birth(std::process::id()).is_some());
    }
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn delayed_worker_is_pending_then_binds_once_across_restart() {
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let child = Child(
            std::process::Command::new("bash")
                .args(["-c", "sleep 0.3; exec -a 'codex --no-daemon' sleep 30"])
                .spawn()
                .unwrap(),
        );
        let pane: PaneInfo = serde_json::from_value(serde_json::json!({
            "pane_id":"%2", "session":"main", "window_id":"@1", "window_index":"0",
            "pane_index":"1", "tty":"", "current_command":"bash", "title":"",
            "pane_pid":child.0.id(), "socket":"default", "agent_program":"codex"
        }))
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let options = CollaborationOptions {
            path: Some(dir.path().join("purpose.sqlite3")),
            ..Default::default()
        };
        let store = CollaborationStore::load(options.clone()).await.unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        store
            .register_worker_launch(id.clone(), pane.clone(), AgentKind::Codex)
            .await
            .unwrap();
        drop(store);
        let store = CollaborationStore::load(options.clone()).await.unwrap();
        let mut pending = Vec::new();
        store
            .enrich_launch_purposes(&mut pending, std::slice::from_ref(&pane))
            .await;
        assert_eq!(
            pending.len(),
            1,
            "a launch grant works before ordinary discovery"
        );
        assert!(is_pending_session(&pending[0].agent_session_id));
        assert_eq!(pending[0].launch_purpose, LaunchPurpose::Worker);
        let mut sessions = vec![participant("human-paneless-session")];
        store
            .enrich_launch_purposes(&mut sessions, std::slice::from_ref(&pane))
            .await;
        assert_eq!(
            sessions[0].launch_purpose,
            LaunchPurpose::Primary,
            "cwd attribution cannot consume a worker grant"
        );
        let mut hook = crate::event::AgentId {
            kind: AgentKind::Codex,
            session_id: "human-paneless-session".into(),
            surface: None,
            pane: None,
            tmux_socket: Some("default".into()),
            cwd: None,
        };
        store.observe_worker_hook(&hook).await.unwrap();
        assert!(store.has_pending_worker_launches().await);
        hook.pane = Some("%2".into());
        hook.session_id = "first-session".into();
        store.observe_worker_hook(&hook).await.unwrap();
        let mut sessions = vec![participant("first-session")];
        store
            .enrich_launch_purposes(&mut sessions, std::slice::from_ref(&pane))
            .await;
        assert_eq!(sessions[0].launch_purpose, LaunchPurpose::Worker);
        assert!(!store.has_pending_worker_launches().await);
        // Replaying a successful launch registration cannot create a second grant.
        store
            .register_worker_launch(id, pane.clone(), AgentKind::Codex)
            .await
            .unwrap();
        drop(store);
        let store = CollaborationStore::load(options).await.unwrap();
        let mut sessions = vec![participant("first-session"), participant("new-session")];
        store
            .enrich_launch_purposes(&mut sessions, std::slice::from_ref(&pane))
            .await;
        assert_eq!(sessions[0].launch_purpose, LaunchPurpose::Worker);
        assert_eq!(sessions[1].launch_purpose, LaunchPurpose::Primary);
    }
}
