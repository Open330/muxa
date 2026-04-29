//! Normalized event schema shared by all adapters and the daemon.
//!
//! The wire format is stable within a major protocol version; breaking
//! changes bump `PROTOCOL_VERSION`.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Current muxa IPC protocol version. Bump on any breaking schema change.
///
/// **Unstable wire format** — this value may change between minor releases
/// when the IPC envelope schema evolves. Pinning to a specific value across
/// muxa upgrades is not supported; treat it as a runtime negotiation token,
/// not a stable API constant.
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum AgentKind {
    ClaudeCode,
    Opencode,
    Codex,
    GeminiCli,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum AgentState {
    Starting,
    Working,
    Idle,
    WaitingInput,
    Error,
    Stopped,
}

/// Identity of an agent instance.
///
/// `session_id` is the source of truth when present. `pane` correlates back
/// to tmux for UI. `cwd` is informational.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentId {
    pub kind: AgentKind,
    pub session_id: String,
    pub pane: Option<String>,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NotificationLevel {
    Info,
    NeedsInput,
    Warning,
    Error,
}

/// Which Claude Code rate-limit window was hit. Pro/Max plans expose two:
/// a 5-hour rolling session window and a 7-day weekly window. Sources that
/// don't distinguish (e.g., the `StopFailure` hook reports `error:"rate_limit"`
/// without a window tag) emit [`RateLimitScope::Unknown`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitScope {
    FiveHour,
    SevenDay,
    Unknown,
}

/// Which signal in the Claude Code surface uncovered the rate-limit hit.
/// Tracked so operators can tell why a row went red and so log scrapers
/// can pivot on the root cause.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitSource {
    /// Picked up from the documented `rate_limits` object in Claude
    /// Code's statusline JSON. Fires whenever the statusline refreshes,
    /// so this is the primary "you're approaching / past the limit"
    /// signal even before the user sees the in-TUI banner.
    Statusline,
    /// Picked up from the `StopFailure` hook firing with
    /// `error == "rate_limit"`. Indicates an in-flight 429.
    StopFailure,
    /// Picked up by parsing the transcript JSONL — fallback for cases
    /// the hooks didn't catch (e.g., older Claude Code versions, sub-agent
    /// rate limits surfaced as `tool_result` text).
    Transcript,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    Started {
        id: AgentId,
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },
    PromptSubmitted {
        id: AgentId,
        prompt: String,
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },
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
    NotificationFired {
        id: AgentId,
        level: NotificationLevel,
        message: String,
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },
    TurnStopped {
        id: AgentId,
        /// Assistant's response text for the turn that just ended, when
        /// the adapter was able to read it from the transcript. Optional
        /// (and `#[serde(default)]`) so adapters that can't capture a
        /// response — and older protocol peers that predate this field —
        /// stay wire-compatible.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response: Option<String>,
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },
    SessionEnded {
        id: AgentId,
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },
    Heartbeat {
        id: AgentId,
        model: Option<String>,
        context_used_pct: Option<f32>,
        cost_usd: Option<f64>,
        /// 5-hour rolling rate-limit window utilization (0–100), when the
        /// adapter was able to read it. Optional + `#[serde(default)]` so
        /// older peers stay wire-compatible and adapters that don't carry
        /// the field (Codex/Gemini today) emit `null`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rate_limit_5h_pct: Option<f32>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            with = "time::serde::rfc3339::option"
        )]
        rate_limit_5h_resets_at: Option<OffsetDateTime>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rate_limit_7d_pct: Option<f32>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            with = "time::serde::rfc3339::option"
        )]
        rate_limit_7d_resets_at: Option<OffsetDateTime>,
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },
    /// User has been told they've hit a usage cap — surfaced separately
    /// from `Heartbeat` so the watch UI can flip the row red and the
    /// notifier can wake the user. `resets_at` is best-effort: present
    /// when the source carries it (statusline, transcript message),
    /// `None` when not (StopFailure 429).
    RateLimited {
        id: AgentId,
        scope: RateLimitScope,
        source: RateLimitSource,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            with = "time::serde::rfc3339::option"
        )]
        resets_at: Option<OffsetDateTime>,
        /// Verbatim user-facing text from the source, when one exists
        /// (e.g., transcript "You've hit your limit · resets 2:40pm
        /// (Asia/Seoul)"). Useful for log lines and the dashboard tooltip.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },
}

impl AgentEvent {
    pub fn id(&self) -> &AgentId {
        match self {
            Self::Started { id, .. }
            | Self::PromptSubmitted { id, .. }
            | Self::ToolStarted { id, .. }
            | Self::ToolCompleted { id, .. }
            | Self::NotificationFired { id, .. }
            | Self::TurnStopped { id, .. }
            | Self::SessionEnded { id, .. }
            | Self::Heartbeat { id, .. }
            | Self::RateLimited { id, .. } => id,
        }
    }

    pub fn at(&self) -> OffsetDateTime {
        match self {
            Self::Started { at, .. }
            | Self::PromptSubmitted { at, .. }
            | Self::ToolStarted { at, .. }
            | Self::ToolCompleted { at, .. }
            | Self::NotificationFired { at, .. }
            | Self::TurnStopped { at, .. }
            | Self::SessionEnded { at, .. }
            | Self::Heartbeat { at, .. }
            | Self::RateLimited { at, .. } => *at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn round_trip_prompt_submitted() {
        let ev = AgentEvent::PromptSubmitted {
            id: AgentId {
                kind: AgentKind::ClaudeCode,
                session_id: "sess-1".into(),
                pane: Some("%10".into()),
                cwd: None,
            },
            prompt: "hello".into(),
            at: datetime!(2026-04-24 12:00:00 UTC),
        };
        let json = serde_json::to_string(&ev).unwrap();
        let back: AgentEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev.id().session_id, back.id().session_id);
    }

    #[test]
    fn kind_serializes_snake_case() {
        let json = serde_json::to_string(&AgentKind::GeminiCli).unwrap();
        assert_eq!(json, "\"gemini_cli\"");
    }

    /// Older muxa peers emit `TurnStopped` without the `response` field.
    /// `#[serde(default)]` must let the new client deserialize them
    /// instead of erroring out — otherwise a daemon and CLI on different
    /// versions can't talk.
    #[test]
    fn turn_stopped_deserializes_without_response_field() {
        let json = r#"{
            "type": "turn_stopped",
            "id": {"kind": "claude_code", "session_id": "s", "pane": null, "cwd": null},
            "at": "2026-04-24T12:00:00Z"
        }"#;
        let ev: AgentEvent = serde_json::from_str(json).unwrap();
        match ev {
            AgentEvent::TurnStopped { response, .. } => assert_eq!(response, None),
            _ => panic!("expected TurnStopped"),
        }
    }

    #[test]
    fn turn_stopped_without_response_omits_field_in_json() {
        let ev = AgentEvent::TurnStopped {
            id: AgentId {
                kind: AgentKind::ClaudeCode,
                session_id: "s".into(),
                pane: None,
                cwd: None,
            },
            response: None,
            at: datetime!(2026-04-24 12:00:00 UTC),
        };
        let json = serde_json::to_string(&ev).unwrap();
        // `skip_serializing_if = "Option::is_none"` keeps the wire payload
        // identical to the pre-`response` schema for adapters that don't
        // capture a response.
        assert!(!json.contains("response"), "json was: {json}");
    }
}
