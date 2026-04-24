//! In-memory registry of active agents.
//!
//! MVP scope: no persistence. Losing the daemon means losing state —
//! adapters will re-announce on the next event.

use crate::event::{AgentEvent, AgentKind, AgentState, NotificationLevel};
use std::collections::HashMap;
use std::sync::Arc;
use time::OffsetDateTime;
use tokio::sync::RwLock;

#[derive(Debug, Clone)]
pub struct Agent {
    pub kind: AgentKind,
    pub session_id: String,
    pub pane: Option<String>,
    pub cwd: Option<String>,
    pub state: AgentState,
    pub last_prompt: Option<String>,
    pub last_notification: Option<String>,
    pub model: Option<String>,
    pub context_used_pct: Option<f32>,
    pub cost_usd: Option<f64>,
    pub started_at: OffsetDateTime,
    pub last_activity_at: OffsetDateTime,
}

impl Agent {
    fn new(
        kind: AgentKind,
        session_id: String,
        pane: Option<String>,
        cwd: Option<String>,
        at: OffsetDateTime,
    ) -> Self {
        Self {
            kind,
            session_id,
            pane,
            cwd,
            state: AgentState::Starting,
            last_prompt: None,
            last_notification: None,
            model: None,
            context_used_pct: None,
            cost_usd: None,
            started_at: at,
            last_activity_at: at,
        }
    }
}

#[derive(Debug, Default)]
pub struct Store {
    // key: session_id
    agents: RwLock<HashMap<String, Agent>>,
}

pub type SharedStore = Arc<Store>;

impl Store {
    pub fn shared() -> SharedStore {
        Arc::new(Self::default())
    }

    pub async fn apply(&self, ev: &AgentEvent) {
        let mut agents = self.agents.write().await;
        let id = ev.id();
        let agent = agents
            .entry(id.session_id.clone())
            .or_insert_with(|| Agent::new(id.kind, id.session_id.clone(), id.pane.clone(), id.cwd.clone(), event_at(ev)));

        // Keep identity fields fresh — adapters may re-send with more info.
        if agent.pane.is_none() {
            agent.pane = id.pane.clone();
        }
        if agent.cwd.is_none() {
            agent.cwd = id.cwd.clone();
        }
        agent.last_activity_at = event_at(ev);

        match ev {
            AgentEvent::Started { .. } => {
                agent.state = AgentState::Idle;
            }
            AgentEvent::PromptSubmitted { prompt, .. } => {
                agent.last_prompt = Some(prompt.clone());
                agent.state = AgentState::Working;
            }
            AgentEvent::ToolStarted { .. } => {
                agent.state = AgentState::Working;
            }
            AgentEvent::ToolCompleted { .. } => { /* state unchanged */ }
            AgentEvent::NotificationFired { level, message, .. } => {
                agent.last_notification = Some(message.clone());
                if *level == NotificationLevel::NeedsInput {
                    agent.state = AgentState::WaitingInput;
                } else if *level == NotificationLevel::Error {
                    agent.state = AgentState::Error;
                }
            }
            AgentEvent::TurnStopped { .. } => {
                if agent.state != AgentState::Error {
                    agent.state = AgentState::Idle;
                }
            }
            AgentEvent::SessionEnded { .. } => {
                agent.state = AgentState::Stopped;
            }
            AgentEvent::Heartbeat {
                model,
                context_used_pct,
                cost_usd,
                ..
            } => {
                if let Some(m) = model {
                    agent.model = Some(m.clone());
                }
                if let Some(p) = context_used_pct {
                    agent.context_used_pct = Some(*p);
                }
                if let Some(c) = cost_usd {
                    agent.cost_usd = Some(*c);
                }
            }
        }
    }

    pub async fn snapshot(&self) -> Vec<Agent> {
        self.agents.read().await.values().cloned().collect()
    }

    pub async fn by_pane(&self, pane: &str) -> Vec<Agent> {
        self.agents
            .read()
            .await
            .values()
            .filter(|a| a.pane.as_deref() == Some(pane))
            .cloned()
            .collect()
    }

    pub async fn by_session(&self, session_id: &str) -> Option<Agent> {
        self.agents.read().await.get(session_id).cloned()
    }
}

fn event_at(ev: &AgentEvent) -> OffsetDateTime {
    match ev {
        AgentEvent::Started { at, .. }
        | AgentEvent::PromptSubmitted { at, .. }
        | AgentEvent::ToolStarted { at, .. }
        | AgentEvent::ToolCompleted { at, .. }
        | AgentEvent::NotificationFired { at, .. }
        | AgentEvent::TurnStopped { at, .. }
        | AgentEvent::SessionEnded { at, .. }
        | AgentEvent::Heartbeat { at, .. } => *at,
    }
}
