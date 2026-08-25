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
pub const PROTOCOL_VERSION: u32 = 4;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum AgentKind {
    ClaudeCode,
    Opencode,
    Codex,
    /// The legacy Gemini CLI (`gemini`). Superseded by [`AgentKind::Antigravity`]
    /// upstream, but kept first-class: its hook contract still works and
    /// installs predate the switch.
    GeminiCli,
    /// Google's Antigravity CLI (`agy`), the Gemini CLI's successor. A
    /// separate kind rather than a rename — the two ship different hook
    /// formats, different config locations, and can be installed side by side.
    Antigravity,
    /// A non-agent background process (shell script, game, automation loop,
    /// or a `muxa run` PTY child) registered via `muxa register` / the
    /// `Register` IPC. Tracked by pid liveness rather than tmux pane
    /// presence; it has no attention states, only `Working` (alive) and
    /// `Stopped` (exited).
    Task,
    Unknown,
}

impl AgentKind {
    /// Name of the bundled screen-detection manifest for this kind, if one
    /// exists.
    ///
    /// This is the registry's answer to "which agent occupies the pane", and
    /// it beats `pane_current_command`: an npm-installed codex runs as `node`,
    /// which names no manifest, so command-only selection silently skipped
    /// every such pane. Kinds whose hooks cover attention entirely (Claude,
    /// opencode, the Gemini CLI) ship no manifest and return `None`.
    #[must_use]
    pub fn screen_manifest_name(self) -> Option<&'static str> {
        match self {
            Self::Codex => Some("codex"),
            Self::Antigravity => Some("agy"),
            Self::ClaudeCode | Self::Opencode | Self::GeminiCli | Self::Task | Self::Unknown => {
                None
            }
        }
    }

    /// Can this agent's hook stream tell muxa it is waiting on the operator?
    ///
    /// Claude Code (`Notification`), Codex (`PermissionRequest`), the Gemini
    /// CLI (`Notification`) and opencode (`permission.asked`) all can, so their
    /// rows reach [`AgentState::WaitingInput`] from hooks alone and screen
    /// inference must stay out of the way.
    ///
    /// The Antigravity CLI cannot — it exposes no permission or notification
    /// hook at all (see `docs/ANTIGRAVITY.md`), so for an agy row that one
    /// signal is only ever available from the pane's screen. `muxad`'s
    /// synthetic layer keys its attention-refinement path off this.
    // The two `true` arms stay separate on purpose: they are true for
    // opposite reasons (one has an attention hook, one is not an agent at
    // all), and spelling every kind out is what makes a new `AgentKind` a
    // compile error here rather than a silent default.
    #[allow(clippy::match_same_arms)]
    #[must_use]
    pub fn hooks_report_attention(self) -> bool {
        match self {
            Self::ClaudeCode | Self::Codex | Self::GeminiCli | Self::Opencode => true,
            Self::Antigravity => false,
            // Neither is a hook-driven agent: `Task` rows have no attention
            // states at all (only Working/Stopped), and `Unknown` is the kind
            // synthetic rows themselves carry. `true` keeps both out of the
            // refinement path, which is exactly where they belong.
            Self::Task | Self::Unknown => true,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum AgentState {
    Starting,
    Working,
    Idle,
    WaitingInput,
    /// Agent is blocked on a multiple-choice menu (e.g., Claude Code's
    /// `AskUserQuestion` / `ExitPlanMode`). Distinct from `WaitingInput`
    /// so the operator can tell at a glance "pick an option" vs
    /// "type a reply / approve permission". Treated like `WaitingInput`
    /// for "needs me" purposes (notifications, sink filters).
    WaitingChoice,
    Error,
    Stopped,
}

/// Normalized execution surface an agent is associated with.
///
/// `pane` remains the legacy host-pane field used by tmux/zellij liveness.
/// `surface` is the additive identity for newer runtimes, including
/// muxa-owned PTY sessions. Keeping these separate prevents a `pty:*`
/// identifier from being mistaken for a tmux/zellij pane and reaped by the
/// host-pane reconciler.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum SurfaceKind {
    Tmux,
    Zellij,
    Pty,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct SurfaceRef {
    pub kind: SurfaceKind,
    pub id: String,
}

/// Identity of an agent instance.
///
/// `session_id` is the source of truth when present. `pane` correlates back
/// to tmux for UI. `cwd` is informational.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentId {
    pub kind: AgentKind,
    /// Agent-runtime session identity, distinct from a tmux session id.
    #[serde(rename = "agent_session_id", alias = "session_id")]
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<SurfaceRef>,
    pub pane: Option<String>,
    /// Host control endpoint captured at hook time: the first field of `$TMUX`
    /// for tmux or `$RMUX` for rmux. Pane ids are only unique per server, so
    /// this disambiguates endpoints. The historical field name remains for
    /// wire compatibility; old adapters simply never send it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux_socket: Option<String>,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NotificationLevel {
    Info,
    NeedsInput,
    /// Like `NeedsInput`, but specifically signals a numbered/menu-style
    /// prompt rather than free-text or permission yes/no. Routes to
    /// `AgentState::WaitingChoice`.
    NeedsChoice,
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
    /// Picked up by parsing a Codex rollout JSONL's `rate_limits` record
    /// (`payload.rate_limits.rate_limit_reached_type`). Codex exposes no
    /// error/rate-limit hook, so the on-disk rollout is the only signal —
    /// the reconciler polls it. Treated as a *hard* source: a reached cap
    /// persists until the next `Started`, same as `StopFailure`/`Transcript`.
    CodexRollout,
}

/// Identifies a subagent spawned via Claude's `Task` tool, carried on
/// `ToolStarted`. `kind` is the Task `subagent_type` (e.g. `"Explore"`,
/// `"general-purpose"`); `description` is the short label the parent gave
/// the task, when present. Additive/optional so non-Task tools, other
/// adapters, and older wire peers stay compatible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentSpec {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
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
        /// Present only for Claude `Task` tool calls: the subagent being
        /// spawned. `#[serde(default)]` keeps the field additive on the
        /// wire for every other tool/adapter and for older peers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subagent: Option<SubagentSpec>,
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
        /// Claude Code's session "recap" (`※ recap: …`), scraped from the
        /// transcript on the same pass that reads `response`. It is not in
        /// any hook payload — the transcript is the only stable read path.
        /// Sparse by nature (only written when the user returns after being
        /// away), so consumers fall back to `ai_title`/`last_prompt`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        recap: Option<String>,
        /// Claude Code's rolling short session title (the string it also
        /// puts in the tmux pane title). Rewritten far more often than a
        /// recap, so it's the practical steady-state summary.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ai_title: Option<String>,
        /// Marker set ONLY by the SYNTHETIC detection producers (screen
        /// inference / the herdr bridge) when they have OBSERVED the pane go
        /// idle — the approval prompt / spinner is gone from the screen. A real
        /// hook's turn-boundary `Stop` always leaves this `false`.
        ///
        /// It exists to distinguish "a stop that is positive evidence the agent
        /// is now idle" (set it) from "a bare response-less stop that proves
        /// nothing about a pending wait" (leave it). The latter is the Codex
        /// quirk — Codex fires a response-less `Stop` while a permission prompt
        /// is still on screen — so a *markerless* response-less stop keeps a
        /// `WaitingInput`/`WaitingChoice` row waiting, while a marked one clears
        /// it to `Idle`. See `mutate_for_event`'s `TurnStopped` arm.
        /// `#[serde(default)]` keeps the field additive for older wire peers.
        #[serde(default)]
        idle_confirmed: bool,
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
    /// `None` when not (`StopFailure` 429).
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

    pub fn id_mut(&mut self) -> &mut AgentId {
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
                tmux_socket: None,
                kind: AgentKind::ClaudeCode,
                session_id: "sess-1".into(),
                surface: None,
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
        let json = serde_json::to_string(&AgentKind::Antigravity).unwrap();
        assert_eq!(json, "\"antigravity\"");
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
                tmux_socket: None,
                kind: AgentKind::ClaudeCode,
                session_id: "s".into(),
                surface: None,
                pane: None,
                cwd: None,
            },
            response: None,
            recap: None,
            ai_title: None,
            idle_confirmed: false,
            at: datetime!(2026-04-24 12:00:00 UTC),
        };
        let json = serde_json::to_string(&ev).unwrap();
        // `skip_serializing_if = "Option::is_none"` keeps the wire payload
        // identical to the pre-`response` schema for adapters that don't
        // capture a response.
        assert!(!json.contains("response"), "json was: {json}");
    }
}
