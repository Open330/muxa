//! Normalized event schema.
//!
//! All adapters translate their agent's native events into this shape before
//! sending to the daemon. The daemon only understands `AgentEvent`.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    ClaudeCode,
    Opencode,
    Codex,
    GeminiCli,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Starting,
    Working,
    Idle,
    WaitingInput,
    Error,
    Stopped,
}

/// Identity of an agent instance. `session_id` is the source of truth when
/// present; `pane` is the correlation key for tmux-side UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentId {
    pub kind: AgentKind,
    pub session_id: String,
    pub pane: Option<String>,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// Session started. Adapter should send this as early as possible.
    Started {
        id: AgentId,
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },
    /// User submitted a prompt. `prompt` may be truncated by the adapter.
    PromptSubmitted {
        id: AgentId,
        prompt: String,
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },
    /// Tool invocation starting. Optional — adapters may skip.
    ToolStarted {
        id: AgentId,
        tool: String,
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },
    ToolCompleted {
        id: AgentId,
        tool: String,
        success: bool,
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },
    /// Agent needs user attention (permission prompt, idle, etc.).
    NotificationFired {
        id: AgentId,
        level: NotificationLevel,
        message: String,
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },
    /// Agent finished responding to the current turn.
    TurnStopped {
        id: AgentId,
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },
    /// Session ended.
    SessionEnded {
        id: AgentId,
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },
    /// Periodic metadata update (model, context %, cost...).
    Heartbeat {
        id: AgentId,
        model: Option<String>,
        context_used_pct: Option<f32>,
        cost_usd: Option<f64>,
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },
}

impl AgentEvent {
    pub fn id(&self) -> &AgentId {
        match self {
            AgentEvent::Started { id, .. }
            | AgentEvent::PromptSubmitted { id, .. }
            | AgentEvent::ToolStarted { id, .. }
            | AgentEvent::ToolCompleted { id, .. }
            | AgentEvent::NotificationFired { id, .. }
            | AgentEvent::TurnStopped { id, .. }
            | AgentEvent::SessionEnded { id, .. }
            | AgentEvent::Heartbeat { id, .. } => id,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NotificationLevel {
    Info,
    NeedsInput,
    Warning,
    Error,
}
