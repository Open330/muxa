//! Fixtures the daemon's test modules share.

use super::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tend::backend::{BackendCaps, HostKind, PaneBackend};
use tend::event::{AgentEvent, AgentId, AgentKind};
use tend::tmux::PaneInfo;
use time::OffsetDateTime;

pub(crate) fn room_participant(
    pane: &str,
    session: &str,
    window: &str,
) -> tend::collaboration::Participant {
    tend::collaboration::Participant {
        launch_purpose: tend::collaboration::LaunchPurpose::Primary,
        draining: false,
        agent_kind: AgentKind::Codex,
        agent_session_id: session.into(),
        pane: pane.into(),
        socket: Some("/tmp/rmux-1/default".into()),
        room: tend::collaboration::RoomId {
            host: "rmux".into(),
            socket: Some("/tmp/rmux-1/default".into()),
            window_id: window.into(),
        },
        tmux_session_id: None,
        tmux_session_name: None,
        window_name: None,
        state: tend::AgentState::Idle,
        cwd: None,
        alias: None,
        roles: Vec::new(),
        console: false,
    }
}

pub(crate) struct CollaborationWakeBackend {
    pub(crate) panes: Vec<PaneInfo>,
    pub(crate) sends: Arc<Mutex<Vec<(String, String)>>>,
}

impl PaneBackend for CollaborationWakeBackend {
    fn kind(&self) -> HostKind {
        HostKind::Tmux
    }

    fn list_panes(&self) -> Vec<PaneInfo> {
        self.panes.clone()
    }

    fn resolve_pane(&self, pane_id: &str) -> Option<PaneInfo> {
        self.panes
            .iter()
            .find(|pane| pane.pane_id == pane_id)
            .cloned()
    }

    fn capture_pane(&self, _pane_id: &str) -> Option<String> {
        None
    }

    fn pane_pid_map(&self) -> HashMap<u32, String> {
        HashMap::new()
    }

    fn current_pane(&self) -> Option<String> {
        None
    }

    fn focus_pane(&self, _pane_id: &str) -> bool {
        false
    }

    fn send_text(&self, pane_id: &str, text: &str) -> bool {
        self.sends
            .lock()
            .unwrap()
            .push((pane_id.into(), text.into()));
        true
    }

    fn caps(&self) -> BackendCaps {
        BackendCaps::default()
    }
}

/// Records every attempt and accepts input only while `accepting` is set,
/// like a pane whose application is not taking keystrokes.
pub(crate) struct RefusingWakeBackend {
    pub(crate) panes: Vec<PaneInfo>,
    pub(crate) attempts: Arc<Mutex<Vec<(String, String)>>>,
    pub(crate) accepting: Arc<std::sync::atomic::AtomicBool>,
}

impl PaneBackend for RefusingWakeBackend {
    fn kind(&self) -> HostKind {
        HostKind::Tmux
    }

    fn list_panes(&self) -> Vec<PaneInfo> {
        self.panes.clone()
    }

    fn resolve_pane(&self, pane_id: &str) -> Option<PaneInfo> {
        self.panes
            .iter()
            .find(|pane| pane.pane_id == pane_id)
            .cloned()
    }

    fn capture_pane(&self, _pane_id: &str) -> Option<String> {
        None
    }

    fn pane_pid_map(&self) -> HashMap<u32, String> {
        HashMap::new()
    }

    fn current_pane(&self) -> Option<String> {
        None
    }

    fn focus_pane(&self, _pane_id: &str) -> bool {
        false
    }

    fn send_text(&self, pane_id: &str, text: &str) -> bool {
        self.attempts
            .lock()
            .unwrap()
            .push((pane_id.into(), text.into()));
        self.accepting.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn caps(&self) -> BackendCaps {
        BackendCaps::default()
    }
}

pub(crate) fn collaboration_pane(pane_id: &str, pane_index: &str) -> PaneInfo {
    PaneInfo {
        session_group: None,
        agent_program: None,
        worker_launch_id: None,
        agent_role: None,
        agent_alias: None,
        workspace_id: None,
        work_id: None,
        pane_id: pane_id.into(),
        session_id: "$1".into(),
        session: "collaboration".into(),
        window_id: "@1".into(),
        window_name: "agents".into(),
        window_index: "0".into(),
        pane_index: pane_index.into(),
        tty: String::new(),
        current_command: "agent".into(),
        title: String::new(),
        current_path: "/repo".into(),
        pane_pid: 0,
        socket: Some("default".into()),
    }
}

pub(crate) async fn add_agent(
    store: &tend::SharedStore,
    pane: &str,
    session_id: &str,
    kind: AgentKind,
) {
    store
        .apply(&AgentEvent::Started {
            id: AgentId {
                kind,
                session_id: session_id.into(),
                surface: None,
                pane: Some(pane.into()),
                tmux_socket: Some("default".into()),
                cwd: Some("/repo".into()),
            },
            at: OffsetDateTime::now_utc(),
        })
        .await;
}

pub(crate) fn worker_participants(
    agents: &[tend::Agent],
    panes: &[PaneInfo],
) -> Vec<tend::collaboration::Participant> {
    let mut participants = tend::collaboration::participants_from(agents, panes);
    for participant in &mut participants {
        participant.launch_purpose = tend::collaboration::LaunchPurpose::Worker;
    }
    participants
}

pub(crate) async fn authorize_test_workers(
    mailbox: &CollaborationStore,
    participants: &[tend::collaboration::Participant],
) {
    for participant in participants
        .iter()
        .filter(|p| !tend::collaboration::is_pending_session(&p.agent_session_id))
    {
        mailbox
            .set_purpose(participant, tend::collaboration::LaunchPurpose::Worker)
            .await
            .unwrap();
    }
}

/// A capped agent: `Started`, then a `RateLimited` whose reset time has
/// already passed, so a `wait = "reset"` rule is due immediately.
pub(crate) async fn add_capped_agent(store: &tend::SharedStore, pane: &str, session_id: &str) {
    add_agent(store, pane, session_id, AgentKind::ClaudeCode).await;
    store
        .apply(&AgentEvent::RateLimited {
            id: AgentId {
                kind: AgentKind::ClaudeCode,
                session_id: session_id.into(),
                surface: None,
                pane: Some(pane.into()),
                tmux_socket: Some("default".into()),
                cwd: Some("/repo".into()),
            },
            scope: tend::event::RateLimitScope::FiveHour,
            source: tend::event::RateLimitSource::Statusline,
            resets_at: Some(OffsetDateTime::now_utc() - time::Duration::minutes(1)),
            message: Some("You've hit your limit".into()),
            at: OffsetDateTime::now_utc(),
        })
        .await;
}
