//! In-memory agent registry.
//!
//! Events flow in, `Agent` rows are updated. No persistence — a daemon
//! restart drops state, and adapters re-announce on the next event.
//!
//! Concurrency: a single `tokio::sync::RwLock` guards the registry. This is
//! fine at the event rates we expect (tens/sec peak); revisit if profiling
//! shows contention.

use crate::event::{AgentEvent, AgentKind, AgentState, NotificationLevel};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use time::OffsetDateTime;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
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
        let at = ev.at();
        let agent = agents.entry(id.session_id.clone()).or_insert_with(|| {
            Agent::new(id.kind, id.session_id.clone(), id.pane.clone(), id.cwd.clone(), at)
        });

        // Keep identity fields fresh — adapters may re-send with more info.
        if agent.pane.is_none() {
            agent.pane.clone_from(&id.pane);
        }
        if agent.cwd.is_none() {
            agent.cwd.clone_from(&id.cwd);
        }
        agent.last_activity_at = at;

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
                match level {
                    NotificationLevel::NeedsInput => agent.state = AgentState::WaitingInput,
                    NotificationLevel::Error => agent.state = AgentState::Error,
                    NotificationLevel::Info | NotificationLevel::Warning => {}
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

    /// Remove agents that ended more than `max_age` ago. Caller decides
    /// cadence; daemon runs this on a timer.
    pub async fn gc(&self, max_age: time::Duration) -> usize {
        let cutoff = OffsetDateTime::now_utc() - max_age;
        let mut agents = self.agents.write().await;
        let before = agents.len();
        agents.retain(|_, a| a.state != AgentState::Stopped || a.last_activity_at >= cutoff);
        before - agents.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AgentEvent, AgentId, AgentKind, NotificationLevel};
    use time::macros::datetime;

    fn id(session: &str) -> AgentId {
        AgentId {
            kind: AgentKind::ClaudeCode,
            session_id: session.into(),
            pane: Some("%1".into()),
            cwd: None,
        }
    }

    #[tokio::test]
    async fn lifecycle_transitions() {
        let store = Store::shared();
        let now = datetime!(2026-04-24 12:00:00 UTC);

        store
            .apply(&AgentEvent::Started { id: id("s"), at: now })
            .await;
        assert_eq!(store.by_session("s").await.unwrap().state, AgentState::Idle);

        store
            .apply(&AgentEvent::PromptSubmitted {
                id: id("s"),
                prompt: "hi".into(),
                at: now,
            })
            .await;
        assert_eq!(store.by_session("s").await.unwrap().state, AgentState::Working);

        store
            .apply(&AgentEvent::NotificationFired {
                id: id("s"),
                level: NotificationLevel::NeedsInput,
                message: "?".into(),
                at: now,
            })
            .await;
        assert_eq!(
            store.by_session("s").await.unwrap().state,
            AgentState::WaitingInput
        );

        store
            .apply(&AgentEvent::TurnStopped { id: id("s"), at: now })
            .await;
        assert_eq!(store.by_session("s").await.unwrap().state, AgentState::Idle);

        store
            .apply(&AgentEvent::SessionEnded { id: id("s"), at: now })
            .await;
        assert_eq!(
            store.by_session("s").await.unwrap().state,
            AgentState::Stopped
        );
    }

    #[tokio::test]
    async fn gc_removes_old_stopped_agents() {
        let store = Store::shared();
        let stale = OffsetDateTime::now_utc() - time::Duration::hours(2);
        store
            .apply(&AgentEvent::Started { id: id("s"), at: stale })
            .await;
        store
            .apply(&AgentEvent::SessionEnded { id: id("s"), at: stale })
            .await;
        let removed = store.gc(time::Duration::hours(1)).await;
        assert_eq!(removed, 1);
        assert!(store.by_session("s").await.is_none());
    }
}
