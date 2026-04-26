//! In-memory agent registry.
//!
//! Events flow in, `Agent` rows are updated. No persistence — a daemon
//! restart drops state, and adapters re-announce on the next event.
//!
//! Concurrency: a single `tokio::sync::RwLock` guards the registry. This is
//! fine at the event rates we expect (tens/sec peak); revisit if profiling
//! shows contention.
//!
//! State-change fanout: the store owns a `tokio::sync::broadcast` channel
//! that emits a `Transition` on every `state`-field change. This is an
//! **in-process** signal only — it is not exposed over IPC — and is used
//! by the daemon's desktop-notifier task to wake users when an agent moves
//! into `WaitingInput` or `Error`.

use crate::event::{AgentEvent, AgentKind, AgentState, NotificationLevel};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use time::OffsetDateTime;
use tokio::sync::{broadcast, RwLock};

/// Prefix used by `muxa sync` / startup discovery for the `session_id` of a
/// synthesized `Started` event. The store recognizes this prefix to keep
/// dedup honest: a real hook event arriving for the same `(kind, pane)`
/// replaces the synthetic placeholder rather than racing it.
///
/// Kept here (not in the runtime crate) so the no-I/O store layer can dedup
/// without taking a cross-crate dependency on the discovery module.
pub const SYNTHETIC_SESSION_PREFIX: &str = "synthetic-";

fn is_synthetic(session_id: &str) -> bool {
    session_id.starts_with(SYNTHETIC_SESSION_PREFIX)
}

/// Capacity of the in-process state-transition broadcast. Slow subscribers
/// that lag past this will see `RecvError::Lagged` and should resync via
/// `Store::snapshot` — the notifier task logs and continues.
const TRANSITION_CHANNEL_CAPACITY: usize = 64;

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

/// In-process notification emitted when an agent's `state` field changes.
///
/// Not part of the IPC protocol — consumers must live in the daemon
/// process. `agent` is the post-transition snapshot, suitable for rendering
/// UI (desktop notification body, log line, etc.) without racing further
/// mutations.
#[derive(Debug, Clone, Serialize)]
pub struct Transition {
    pub from: AgentState,
    pub to: AgentState,
    pub agent: Agent,
}

#[derive(Debug)]
pub struct Store {
    agents: RwLock<HashMap<String, Agent>>,
    transitions: broadcast::Sender<Transition>,
}

impl Default for Store {
    fn default() -> Self {
        let (tx, _) = broadcast::channel(TRANSITION_CHANNEL_CAPACITY);
        Self {
            agents: RwLock::default(),
            transitions: tx,
        }
    }
}

pub type SharedStore = Arc<Store>;

/// Reconcile pane occupants for an incoming `Started` event.
///
/// Returns `false` when the event should be dropped (re-running `muxa sync`
/// against a pane that's already represented). Otherwise the map has been
/// updated to make room for the new agent:
///
/// * Synthetic placeholders for `pane` are removed when the incoming event
///   is real, so the real entry replaces them rather than coexisting.
/// * Older non-stopped sessions sharing the pane are flipped to `Stopped`
///   (the user launched a fresh agent in the same pane and the previous
///   session never emitted `SessionEnd`).
fn reconcile_pane_for_started(
    agents: &mut HashMap<String, Agent>,
    incoming_session: &str,
    pane: &str,
    at: OffsetDateTime,
) -> bool {
    if is_synthetic(incoming_session) {
        // Idempotent re-sync: any non-stopped occupant wins, real or not.
        let occupied = agents
            .values()
            .any(|a| a.pane.as_deref() == Some(pane) && a.state != AgentState::Stopped);
        if occupied {
            return false;
        }
    } else {
        // Real Started — drop synthetic placeholders for this pane outright.
        agents.retain(|_, a| !(a.pane.as_deref() == Some(pane) && is_synthetic(&a.session_id)));
    }

    for other in agents.values_mut() {
        if other.session_id != incoming_session
            && other.pane.as_deref() == Some(pane)
            && other.state != AgentState::Stopped
        {
            other.state = AgentState::Stopped;
            other.last_activity_at = at;
        }
    }
    true
}

impl Store {
    pub fn shared() -> SharedStore {
        Arc::new(Self::default())
    }

    /// Subscribe to in-process state transitions.
    ///
    /// Returns a fresh receiver; each subscriber has an independent cursor.
    /// Callers should handle `broadcast::error::RecvError::Lagged` by
    /// resyncing from `snapshot()` rather than treating it as fatal.
    pub fn subscribe(&self) -> broadcast::Receiver<Transition> {
        self.transitions.subscribe()
    }

    pub async fn apply(&self, ev: &AgentEvent) {
        let mut agents = self.agents.write().await;
        let id = ev.id();
        let at = ev.at();

        if matches!(ev, AgentEvent::Started { .. }) {
            if let Some(pane) = id.pane.as_deref() {
                if !reconcile_pane_for_started(&mut agents, &id.session_id, pane, at) {
                    return;
                }
            }
        }

        let agent = agents.entry(id.session_id.clone()).or_insert_with(|| {
            Agent::new(
                id.kind,
                id.session_id.clone(),
                id.pane.clone(),
                id.cwd.clone(),
                at,
            )
        });

        // Keep identity fields fresh — adapters may re-send with more info.
        if agent.pane.is_none() {
            agent.pane.clone_from(&id.pane);
        }
        if agent.cwd.is_none() {
            agent.cwd.clone_from(&id.cwd);
        }
        agent.last_activity_at = at;

        let prev_state = agent.state;

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

        if agent.state != prev_state {
            let transition = Transition {
                from: prev_state,
                to: agent.state,
                agent: agent.clone(),
            };
            // `send` errors only when there are zero subscribers — that's
            // the common case (notifier disabled) and not worth logging.
            let _ = self.transitions.send(transition);
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
            .apply(&AgentEvent::Started {
                id: id("s"),
                at: now,
            })
            .await;
        assert_eq!(store.by_session("s").await.unwrap().state, AgentState::Idle);

        store
            .apply(&AgentEvent::PromptSubmitted {
                id: id("s"),
                prompt: "hi".into(),
                at: now,
            })
            .await;
        assert_eq!(
            store.by_session("s").await.unwrap().state,
            AgentState::Working
        );

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
            .apply(&AgentEvent::TurnStopped {
                id: id("s"),
                at: now,
            })
            .await;
        assert_eq!(store.by_session("s").await.unwrap().state, AgentState::Idle);

        store
            .apply(&AgentEvent::SessionEnded {
                id: id("s"),
                at: now,
            })
            .await;
        assert_eq!(
            store.by_session("s").await.unwrap().state,
            AgentState::Stopped
        );
    }

    #[tokio::test]
    async fn started_dedupes_previous_session_in_same_pane() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-24 12:00:00 UTC);
        let t1 = datetime!(2026-04-24 12:05:00 UTC);

        // First session starts on pane %1 and is happily working.
        store
            .apply(&AgentEvent::Started {
                id: id("first"),
                at: t0,
            })
            .await;
        store
            .apply(&AgentEvent::PromptSubmitted {
                id: id("first"),
                prompt: "do a thing".into(),
                at: t0,
            })
            .await;
        assert_eq!(
            store.by_session("first").await.unwrap().state,
            AgentState::Working
        );

        // A fresh session opens in the same pane — e.g. user closed the old
        // agent and launched a new one without the adapter ever seeing a
        // SessionEnded. The old row must flip to Stopped.
        store
            .apply(&AgentEvent::Started {
                id: id("second"),
                at: t1,
            })
            .await;

        assert_eq!(
            store.by_session("first").await.unwrap().state,
            AgentState::Stopped
        );
        assert_eq!(
            store.by_session("second").await.unwrap().state,
            AgentState::Idle
        );
    }

    #[tokio::test]
    async fn gc_removes_old_stopped_agents() {
        let store = Store::shared();
        let stale = OffsetDateTime::now_utc() - time::Duration::hours(2);
        store
            .apply(&AgentEvent::Started {
                id: id("s"),
                at: stale,
            })
            .await;
        store
            .apply(&AgentEvent::SessionEnded {
                id: id("s"),
                at: stale,
            })
            .await;
        let removed = store.gc(time::Duration::hours(1)).await;
        assert_eq!(removed, 1);
        assert!(store.by_session("s").await.is_none());
    }

    #[tokio::test]
    async fn subscribe_receives_state_transition() {
        let store = Store::shared();
        let now = datetime!(2026-04-24 12:00:00 UTC);

        // Subscribe BEFORE applying; otherwise the event is missed.
        let mut rx = store.subscribe();

        // Seed the agent so its prior state is known (Starting -> Idle on
        // Started). Drain that transition so the assertion below targets
        // the one we care about.
        store
            .apply(&AgentEvent::Started {
                id: id("s"),
                at: now,
            })
            .await;
        let first = rx.recv().await.unwrap();
        assert_eq!(first.from, AgentState::Starting);
        assert_eq!(first.to, AgentState::Idle);

        store
            .apply(&AgentEvent::PromptSubmitted {
                id: id("s"),
                prompt: "hello".into(),
                at: now,
            })
            .await;

        let t = rx.recv().await.unwrap();
        assert_eq!(t.from, AgentState::Idle);
        assert_eq!(t.to, AgentState::Working);
        assert_eq!(t.agent.session_id, "s");
        assert_eq!(t.agent.last_prompt.as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn synthetic_started_idempotent_on_same_pane() {
        let store = Store::shared();
        let now = datetime!(2026-04-24 12:00:00 UTC);

        // First synthetic from `muxa sync` lands an Idle agent on %1.
        let synthetic = AgentId {
            kind: AgentKind::ClaudeCode,
            session_id: "synthetic-%1".into(),
            pane: Some("%1".into()),
            cwd: None,
        };
        store
            .apply(&AgentEvent::Started {
                id: synthetic.clone(),
                at: now,
            })
            .await;
        assert_eq!(store.snapshot().await.len(), 1);

        // Re-running discovery must not create a duplicate or wipe the
        // first entry's started_at — it's a no-op.
        let later = datetime!(2026-04-24 12:30:00 UTC);
        store
            .apply(&AgentEvent::Started {
                id: synthetic,
                at: later,
            })
            .await;
        let snap = store.snapshot().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].started_at, now);
    }

    #[tokio::test]
    async fn real_started_replaces_synthetic_on_same_pane() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-24 12:00:00 UTC);
        let t1 = datetime!(2026-04-24 12:01:00 UTC);

        // Discovery synthesizes a placeholder.
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::ClaudeCode,
                    session_id: "synthetic-%7".into(),
                    pane: Some("%7".into()),
                    cwd: None,
                },
                at: t0,
            })
            .await;

        // A real hook arrives — same pane, real session id. The synthetic
        // should be replaced (gone), leaving exactly one entry under the
        // canonical session id.
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::ClaudeCode,
                    session_id: "real-sess".into(),
                    pane: Some("%7".into()),
                    cwd: Some("/work".into()),
                },
                at: t1,
            })
            .await;

        let snap = store.snapshot().await;
        assert_eq!(snap.len(), 1, "synthetic should have been removed");
        assert_eq!(snap[0].session_id, "real-sess");
        assert_eq!(snap[0].cwd.as_deref(), Some("/work"));
        assert_eq!(snap[0].state, AgentState::Idle);
        assert!(store.by_session("synthetic-%7").await.is_none());
    }

    #[tokio::test]
    async fn synthetic_skipped_when_real_agent_present() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-24 12:00:00 UTC);

        // Real agent already known via a prior hook.
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::ClaudeCode,
                    session_id: "real-sess".into(),
                    pane: Some("%9".into()),
                    cwd: None,
                },
                at: t0,
            })
            .await;

        // `muxa sync` runs and tries to backfill the same pane.
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::ClaudeCode,
                    session_id: "synthetic-%9".into(),
                    pane: Some("%9".into()),
                    cwd: None,
                },
                at: t0,
            })
            .await;

        let snap = store.snapshot().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].session_id, "real-sess");
    }

    #[tokio::test]
    async fn no_transition_when_state_unchanged() {
        let store = Store::shared();
        let now = datetime!(2026-04-24 12:00:00 UTC);

        store
            .apply(&AgentEvent::Started {
                id: id("s"),
                at: now,
            })
            .await;

        let mut rx = store.subscribe();

        // Heartbeat updates metadata only — no state change, no broadcast.
        store
            .apply(&AgentEvent::Heartbeat {
                id: id("s"),
                model: Some("Opus".into()),
                context_used_pct: None,
                cost_usd: None,
                at: now,
            })
            .await;

        // A 50ms window is plenty — the send is synchronous-ish (tokio
        // broadcast send is non-blocking) and we're on a single runtime.
        let res = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
        assert!(res.is_err(), "expected no transition, got {res:?}");
    }
}
