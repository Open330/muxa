//! `[automation]` — rules that watch agent state and act on it.
//!
//! The motivating case is the session cap: an agent hits "you've hit your
//! limit", the row goes [`AgentState::Error`] with a
//! [`RateLimitScope`], and the operator has to remember to come back and
//! type `continue` once the window rolls over. A rule does that for them.
//!
//! The mechanism is deliberately general — an event, filters, a delay, an
//! action — because the cap is only the first of these an operator will
//! want. Both vocabularies are **closed sets** ([`AutomationEvent`],
//! [`AutomationAction`]) so adding one is a compile error everywhere it has
//! to be handled, not a silent no-op.
//!
//! # Layering
//!
//! * [`AutomationConfig`] / [`AutomationRule`] — the serde types behind
//!   `[automation]` and `[[automation.rule]]`. Pure data; validated by
//!   [`AutomationConfig::validate`].
//! * [`Scheduler`] — pure decisions. Given a rule, a trigger, an
//!   [`AutomationSubject`] and an injected `now`, it answers "fire, and
//!   when" or "skip, because". No clock reads, no I/O; this is where the
//!   tests live.
//! * [`AutomationLedger`] — the durable record of what fired. It backs both
//!   the rate guards and `muxa automation log`.
//! * [`AutomationStore`] — the live handle the daemon and the IPC layer
//!   share: the effective config (master switch, pause, per-rule enable),
//!   the ledger, and the `config.toml` write-back for edits made at runtime.
//!
//! The task that turns decisions into keystrokes lives in `muxad`, next to
//! the collaboration waker it is modelled on.
//!
//! # Safety
//!
//! An automation types into a live agent, so every firing passes:
//! the master switch, `paused_until`, the rule's own `enabled`, its
//! `cooldown` and `max_per_hour` (per rule per pane), an unconfigurable
//! global hourly ceiling ([`GLOBAL_MAX_PER_HOUR`]), the
//! one-firing-per-episode key, and a re-check of `only_if_still` against
//! the live store at fire time. Every decision — fired, skipped, failed —
//! is appended to the ledger.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use time::OffsetDateTime;
use tokio::sync::{watch, Mutex, RwLock};

use crate::backend::HostKind;
use crate::config::Config;
use crate::event::{AgentKind, AgentState, RateLimitScope, RateLimitSource};
use crate::state::Agent;

/// Filename of the firing ledger under `$XDG_DATA_HOME/muxa/`.
pub const AUTOMATION_FILENAME: &str = "automation.json";

/// Firings retained in the ledger. Old entries are dropped from the front,
/// so the file stays a bounded, appendable audit trail rather than a log
/// that grows without limit.
pub const MAX_LEDGER_ENTRIES: usize = 500;

/// Hard ceiling on firings per hour across every rule and every pane. Not
/// configurable on purpose: `max_per_hour` bounds one rule against one
/// pane, and this bounds the whole engine against a pathological fan-out
/// (a dozen agents capping at once, a rule whose `only_if_still` never
/// clears). Reaching it is a bug, and the skip is logged as one.
pub const GLOBAL_MAX_PER_HOUR: u32 = 30;

/// Longest `text` an automation may type into a live agent.
const MAX_ACTION_TEXT_BYTES: usize = 4096;
/// Upper bound on any configured duration. A rule that wants to wait longer
/// than a day is almost certainly a typo (`2` meaning minutes, `2h` meant).
const MAX_CONFIGURED_SECONDS: i64 = 24 * 60 * 60;

const DEFAULT_JITTER_SECONDS: i64 = 15;
const DEFAULT_COOLDOWN_SECONDS: i64 = 120;
const DEFAULT_FALLBACK_SECONDS: i64 = 15 * 60;
const DEFAULT_MAX_PER_HOUR: u32 = 3;

/// Default firing ledger: `$XDG_DATA_HOME/muxa/automation.json`, beside the
/// ask history and collaboration mailbox it sits alongside.
#[must_use]
pub fn default_automation_file() -> Option<PathBuf> {
    dirs::data_dir().map(|dir| {
        dir.join(crate::paths::CONFIG_DIRNAME)
            .join(AUTOMATION_FILENAME)
    })
}

// ---------------------------------------------------------------------------
// Durations: `30s` / `5m` / `2h`, and `reset+2m`
// ---------------------------------------------------------------------------

/// A duration written the way `config.toml` writes it: `45s`, `5m`, `2h`,
/// `1d`. Serialized back in the same shape, so a rule round-trips through
/// [`AutomationStore::upsert_rule`] without being rewritten in seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DurationSpec(time::Duration);

impl DurationSpec {
    #[must_use]
    pub fn seconds(value: i64) -> Self {
        Self(time::Duration::seconds(value))
    }

    #[must_use]
    pub fn duration(self) -> time::Duration {
        self.0
    }

    fn validate(self, field: &str) -> Result<(), String> {
        if self.0.is_negative() {
            return Err(format!("{field} cannot be negative"));
        }
        if self.0.whole_seconds() > MAX_CONFIGURED_SECONDS {
            return Err(format!("{field} cannot exceed 24h"));
        }
        Ok(())
    }
}

impl std::fmt::Display for DurationSpec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&render_duration(self.0))
    }
}

impl Serialize for DurationSpec {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for DurationSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        parse_duration(&text)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

/// Parse `45s` / `5m` / `2h` / `1d`, plus a bare `0`. A unit is required
/// otherwise: a bare number reads as seconds to one operator and minutes to
/// the next, and the difference between those two is a runaway.
pub fn parse_duration(text: &str) -> Result<time::Duration, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("expected a duration like `30s`, `5m`, `2h`".into());
    }
    if trimmed == "0" {
        return Ok(time::Duration::ZERO);
    }
    let (digits, unit) = trimmed.split_at(
        trimmed
            .find(|c: char| !c.is_ascii_digit())
            .ok_or_else(|| format!("{trimmed:?} needs a unit: `s`, `m`, `h`, or `d`"))?,
    );
    if digits.is_empty() {
        return Err(format!("{trimmed:?} does not start with a number"));
    }
    let amount: i64 = digits
        .parse()
        .map_err(|_| format!("{digits:?} is not a whole number"))?;
    let seconds = match unit {
        "s" => amount,
        "m" => amount.saturating_mul(60),
        "h" => amount.saturating_mul(3600),
        "d" => amount.saturating_mul(86_400),
        other => {
            return Err(format!(
                "unknown duration unit {other:?}; use s, m, h, or d"
            ))
        }
    };
    Ok(time::Duration::seconds(seconds))
}

/// Render a duration back into the compact config spelling, choosing the
/// largest unit that divides it exactly.
fn render_duration(duration: time::Duration) -> String {
    let seconds = duration.whole_seconds();
    let (magnitude, unit) = if seconds == 0 {
        (0, "s")
    } else if seconds % 86_400 == 0 {
        (seconds / 86_400, "d")
    } else if seconds % 3_600 == 0 {
        (seconds / 3_600, "h")
    } else if seconds % 60 == 0 {
        (seconds / 60, "m")
    } else {
        (seconds, "s")
    };
    format!("{magnitude}{unit}")
}

/// What a rule's `wait` counts from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitAnchor {
    /// The moment the rule was armed — a plain `5m`.
    Event,
    /// The cap's own reset time, when the source carried one. Only
    /// meaningful for `on = "rate_limited"`.
    Reset,
}

/// A rule's `wait`: `5m` (from the event) or `reset+2m` / `reset` /
/// `reset-30s` (from the cap's reset time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitSpec {
    pub anchor: WaitAnchor,
    pub offset: time::Duration,
}

impl WaitSpec {
    #[must_use]
    pub fn after_event(offset: time::Duration) -> Self {
        Self {
            anchor: WaitAnchor::Event,
            offset,
        }
    }

    #[must_use]
    pub fn after_reset(offset: time::Duration) -> Self {
        Self {
            anchor: WaitAnchor::Reset,
            offset,
        }
    }

    fn validate(self, field: &str) -> Result<(), String> {
        if self.offset.whole_seconds().abs() > MAX_CONFIGURED_SECONDS {
            return Err(format!("{field} offset cannot exceed 24h"));
        }
        if self.anchor == WaitAnchor::Event && self.offset.is_negative() {
            return Err(format!(
                "{field} cannot be negative without a `reset` anchor"
            ));
        }
        Ok(())
    }
}

impl std::fmt::Display for WaitSpec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.anchor {
            WaitAnchor::Event => formatter.write_str(&render_duration(self.offset)),
            WaitAnchor::Reset if self.offset.is_zero() => formatter.write_str("reset"),
            WaitAnchor::Reset if self.offset.is_negative() => {
                write!(formatter, "reset-{}", render_duration(-self.offset))
            }
            WaitAnchor::Reset => write!(formatter, "reset+{}", render_duration(self.offset)),
        }
    }
}

impl Serialize for WaitSpec {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for WaitSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        parse_wait(&text).map_err(serde::de::Error::custom)
    }
}

/// Parse a `wait` value: `reset`, `reset+2m`, `reset-30s`, or a plain
/// duration.
pub fn parse_wait(text: &str) -> Result<WaitSpec, String> {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("reset") else {
        return parse_duration(trimmed).map(WaitSpec::after_event);
    };
    let rest = rest.trim();
    if rest.is_empty() {
        return Ok(WaitSpec::after_reset(time::Duration::ZERO));
    }
    let (sign, magnitude) = match rest.split_at(1) {
        ("+", tail) => (1, tail),
        ("-", tail) => (-1, tail),
        _ => {
            return Err(format!(
                "{trimmed:?} is not a wait: use `reset`, `reset+2m`, `reset-30s`, or `5m`"
            ))
        }
    };
    let offset = parse_duration(magnitude)?;
    Ok(WaitSpec::after_reset(if sign < 0 {
        -offset
    } else {
        offset
    }))
}

// ---------------------------------------------------------------------------
// The closed vocabularies
// ---------------------------------------------------------------------------

/// What a rule watches for.
///
/// Closed set. Adding a variant means: a `match` arm in
/// [`AutomationSubject::current_event`] saying which live agent state
/// produces it, an arm in [`AutomationCondition::from_event`] naming the
/// condition it re-checks as, and — where the event needs an extra key
/// (`idle_for` needs `for`) — an arm in [`AutomationRule::validate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum AutomationEvent {
    /// The agent hit a usage cap: `Error` **with** a rate-limit scope.
    RateLimited,
    /// The agent is blocked on the operator (`WaitingInput` /
    /// `WaitingChoice`).
    WaitingInput,
    /// The agent has been `Idle` for the rule's `for` duration.
    IdleFor,
    /// The agent is in `Error` for a reason that is *not* a usage cap.
    Error,
}

/// What a rule does when it fires.
///
/// Closed set. Adding a variant means: the keys it reads
/// (validated in [`AutomationRule::validate`]), an arm in the daemon's
/// `perform_automation_action`, and a line in `docs/AUTOMATION.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum AutomationAction {
    /// Type `text` into the pane, then Enter when `submit` (the default).
    SendPrompt,
    /// Emit `message` as a daemon-side notice. Types nothing.
    Notify,
    /// Send the pane's interrupt key. Types no prompt.
    Interrupt,
}

/// The live-state predicate re-checked at fire time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum AutomationCondition {
    RateLimited,
    WaitingInput,
    Idle,
    Error,
    /// Fire regardless of what the agent is doing now. Opt-in, and the only
    /// way to defeat the fire-time re-check.
    Any,
}

impl AutomationCondition {
    /// The condition a rule re-checks by default: the one that armed it.
    #[must_use]
    pub fn from_event(event: AutomationEvent) -> Self {
        match event {
            AutomationEvent::RateLimited => Self::RateLimited,
            AutomationEvent::WaitingInput => Self::WaitingInput,
            AutomationEvent::IdleFor => Self::Idle,
            AutomationEvent::Error => Self::Error,
        }
    }
}

/// An [`AgentKind`] as an operator writes it. Accepts the canonical
/// `snake_case` name and the everyday one (`claude`, `gemini`, `agy`);
/// always serializes canonically so a rule written back to `config.toml`
/// names the same kind the registry does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AgentKindSpec(pub AgentKind);

impl Serialize for AgentKindSpec {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for AgentKindSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        parse_agent_kind(&text)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

/// Map an operator-written agent name onto an [`AgentKind`].
pub fn parse_agent_kind(raw: &str) -> Result<AgentKind, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "claude" | "claude_code" | "claude-code" => Ok(AgentKind::ClaudeCode),
        "codex" => Ok(AgentKind::Codex),
        "opencode" => Ok(AgentKind::Opencode),
        "gemini" | "gemini_cli" | "gemini-cli" => Ok(AgentKind::GeminiCli),
        "agy" | "antigravity" => Ok(AgentKind::Antigravity),
        "task" => Ok(AgentKind::Task),
        "unknown" => Ok(AgentKind::Unknown),
        other => Err(format!(
            "unknown agent {other:?}; use claude, codex, opencode, gemini, agy, task, or unknown"
        )),
    }
}

// ---------------------------------------------------------------------------
// `[automation]`
// ---------------------------------------------------------------------------

/// `[automation]` — the master switch, the pause, and the rules.
///
/// Defaults to enabled **with no rules**: a fresh install runs the engine's
/// bookkeeping and does nothing, until the operator (or Muxa.app) writes a
/// rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AutomationConfig {
    /// Master switch. `false` stops every rule without editing any of them.
    pub enabled: bool,
    /// Written by `muxa automation pause`. While `now` is before this,
    /// nothing fires; expiry needs no further action.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub paused_until: Option<OffsetDateTime>,
    /// `[[automation.rule]]`, in file order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rule: Vec<AutomationRule>,
}

impl Default for AutomationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            paused_until: None,
            rule: Vec::new(),
        }
    }
}

impl AutomationConfig {
    /// Hard checks run at config load and before any runtime edit is
    /// written. Names must be unique and TOML-safe; every rule must be
    /// self-consistent.
    pub fn validate(&self) -> Result<(), String> {
        let mut seen: Vec<&str> = Vec::with_capacity(self.rule.len());
        for rule in &self.rule {
            rule.validate()?;
            if seen.contains(&rule.name.as_str()) {
                return Err(format!(
                    "automation.rule: {:?} is defined more than once",
                    rule.name
                ));
            }
            seen.push(&rule.name);
        }
        Ok(())
    }

    /// Is the engine allowed to fire at all right now?
    #[must_use]
    pub fn active_at(&self, now: OffsetDateTime) -> bool {
        self.enabled && self.paused_until.is_none_or(|until| now >= until)
    }

    #[must_use]
    pub fn rule_named(&self, name: &str) -> Option<&AutomationRule> {
        self.rule.iter().find(|rule| rule.name == name)
    }
}

/// One `[[automation.rule]]`.
///
/// Every tuning key is `Option`: absent means "the default", and absent is
/// what gets written back, so a rule edited through IPC keeps the shape the
/// operator gave it. Read the effective values through the accessors
/// ([`Self::wait`], [`Self::cooldown`], …), never the raw fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutomationRule {
    /// Unique, TOML-safe identity. Used by `enable`/`disable`, by the
    /// ledger, and as part of the one-firing-per-episode key.
    pub name: String,
    /// The event this rule watches.
    pub on: AutomationEvent,
    #[serde(default = "default_true")]
    pub enabled: bool,

    // --- filters: every one is optional, and all present ones must match ---
    /// Restrict to these agent kinds. Empty means any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent: Vec<AgentKindSpec>,
    /// Exact workspace id stamped on the agent's pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Regex matched against the Work id stamped on the agent's pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work: Option<String>,
    /// Exact pane id (`%42`, `herdr:7`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
    /// `local` (this daemon's own node, which is every pane it can act on)
    /// or a pane-host namespace: `tmux`, `cmux`, `rmux`, `zellij`, `herdr`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Which rate-limit windows count. Only valid with `on = "rate_limited"`;
    /// empty means any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scope: Vec<RateLimitScope>,
    /// How long the agent must have been idle. Required by — and only valid
    /// for — `on = "idle_for"`.
    #[serde(default, rename = "for", skip_serializing_if = "Option::is_none")]
    pub for_: Option<DurationSpec>,

    // --- timing ---
    /// When to act. `reset+2m` anchors on the cap's own reset time;
    /// a plain duration counts from the event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<WaitSpec>,
    /// Used instead of a `reset` anchor when the cap carried no reset time
    /// (a `StopFailure` 429 does not). Default 15m.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<DurationSpec>,
    /// A random `0..jitter` is added to every fire time, so a dozen agents
    /// capped in the same window do not all resume on the same second.
    /// Default 15s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jitter: Option<DurationSpec>,

    // --- action ---
    pub action: AutomationAction,
    /// `send_prompt` payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// `notify` payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Whether `send_prompt` commits the line with Enter. Default `true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submit: Option<bool>,

    // --- guards: a rule cannot opt out of these, only tune them ---
    /// Firings allowed per hour, per pane. Default 3, capped at 60, and
    /// bounded further by [`GLOBAL_MAX_PER_HOUR`] across all rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_per_hour: Option<u32>,
    /// Minimum gap between firings of this rule against one pane.
    /// Default 2m.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown: Option<DurationSpec>,
    /// Re-checked against the live store at fire time. Defaults to the
    /// condition that armed the rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub only_if_still: Option<AutomationCondition>,
}

fn default_true() -> bool {
    true
}

impl AutomationRule {
    /// A minimal rule, for tests and for building one programmatically.
    #[must_use]
    pub fn new(name: impl Into<String>, on: AutomationEvent, action: AutomationAction) -> Self {
        Self {
            name: name.into(),
            on,
            enabled: true,
            agent: Vec::new(),
            workspace: None,
            work: None,
            pane: None,
            host: None,
            scope: Vec::new(),
            for_: None,
            wait: None,
            fallback: None,
            jitter: None,
            action,
            text: None,
            message: None,
            submit: None,
            max_per_hour: None,
            cooldown: None,
            only_if_still: None,
        }
    }

    #[must_use]
    pub fn wait(&self) -> WaitSpec {
        self.wait.unwrap_or(match self.on {
            // A cap that names its reset time is the whole point of the
            // anchor, so that is the default for `rate_limited`.
            AutomationEvent::RateLimited => WaitSpec::after_reset(time::Duration::ZERO),
            _ => WaitSpec::after_event(time::Duration::ZERO),
        })
    }

    #[must_use]
    pub fn fallback(&self) -> time::Duration {
        self.fallback.map_or_else(
            || time::Duration::seconds(DEFAULT_FALLBACK_SECONDS),
            DurationSpec::duration,
        )
    }

    #[must_use]
    pub fn jitter(&self) -> time::Duration {
        self.jitter.map_or_else(
            || time::Duration::seconds(DEFAULT_JITTER_SECONDS),
            DurationSpec::duration,
        )
    }

    #[must_use]
    pub fn cooldown(&self) -> time::Duration {
        self.cooldown.map_or_else(
            || time::Duration::seconds(DEFAULT_COOLDOWN_SECONDS),
            DurationSpec::duration,
        )
    }

    #[must_use]
    pub fn max_per_hour(&self) -> u32 {
        self.max_per_hour.unwrap_or(DEFAULT_MAX_PER_HOUR)
    }

    #[must_use]
    pub fn submit(&self) -> bool {
        self.submit.unwrap_or(true)
    }

    #[must_use]
    pub fn only_if_still(&self) -> AutomationCondition {
        self.only_if_still
            .unwrap_or_else(|| AutomationCondition::from_event(self.on))
    }

    /// Does this rule read pane-scoped tmux metadata? The daemon only pays
    /// for a pane scan when at least one enabled rule says yes.
    #[must_use]
    pub fn needs_pane_metadata(&self) -> bool {
        self.workspace.is_some() || self.work.is_some() || self.host.is_some()
    }

    /// The compiled `work` regex, if the rule has one. Compilation is
    /// checked by [`Self::validate`], so a validated rule never returns
    /// `Some(Err(_))` here.
    fn work_regex(&self) -> Option<Result<regex::Regex, regex::Error>> {
        self.work.as_deref().map(regex::Regex::new)
    }

    /// Hard checks: a rule that fails one is never loaded and never
    /// written.
    #[allow(clippy::too_many_lines)] // one branch per key; a table would hide which key failed
    pub fn validate(&self) -> Result<(), String> {
        let named = |message: String| format!("automation.rule {:?}: {message}", self.name);

        if self.name.trim().is_empty() {
            return Err("automation.rule: name cannot be empty".into());
        }
        if self.name.len() > 64 {
            return Err(named("name must be at most 64 characters".into()));
        }
        if !self
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(named(
                "name may only contain letters, digits, `-`, `_`, and `.`".into(),
            ));
        }

        if let Some(work) = &self.work {
            if let Some(Err(error)) = self.work_regex() {
                return Err(named(format!(
                    "work {work:?} is not a valid regex: {error}"
                )));
            }
        }
        if let Some(host) = &self.host {
            if parse_host_filter(host).is_none() {
                return Err(named(format!(
                    "host {host:?} is not `local` or a pane host \
                     (tmux, cmux, rmux, zellij, herdr)"
                )));
            }
        }
        if let Some(pane) = &self.pane {
            if pane.trim().is_empty() {
                return Err(named("pane cannot be empty".into()));
            }
        }
        if let Some(workspace) = &self.workspace {
            if workspace.trim().is_empty() {
                return Err(named("workspace cannot be empty".into()));
            }
        }

        if !self.scope.is_empty() && self.on != AutomationEvent::RateLimited {
            return Err(named(
                "scope only applies to `on = \"rate_limited\"`".into(),
            ));
        }
        match (self.on, self.for_) {
            (AutomationEvent::IdleFor, None) => {
                return Err(named("`on = \"idle_for\"` requires `for`".into()))
            }
            (AutomationEvent::IdleFor, Some(value)) => {
                value.validate("for").map_err(&named)?;
                if value.duration().is_zero() {
                    return Err(named("`for` must be greater than zero".into()));
                }
            }
            (_, Some(_)) => return Err(named("`for` only applies to `on = \"idle_for\"`".into())),
            (_, None) => {}
        }

        let wait = self.wait();
        wait.validate("wait").map_err(&named)?;
        if wait.anchor == WaitAnchor::Reset && self.on != AutomationEvent::RateLimited {
            return Err(named(
                "a `reset` wait only applies to `on = \"rate_limited\"`; \
                 nothing else carries a reset time"
                    .into(),
            ));
        }
        if let Some(fallback) = self.fallback {
            fallback.validate("fallback").map_err(&named)?;
        }
        if let Some(jitter) = self.jitter {
            jitter.validate("jitter").map_err(&named)?;
        }
        if let Some(cooldown) = self.cooldown {
            cooldown.validate("cooldown").map_err(&named)?;
        }
        if let Some(max) = self.max_per_hour {
            if max == 0 {
                return Err(named(
                    "max_per_hour must be at least 1; use `enabled = false` to stop a rule".into(),
                ));
            }
            if max > 60 {
                return Err(named("max_per_hour cannot exceed 60".into()));
            }
        }

        match self.action {
            AutomationAction::SendPrompt => {
                let text = self
                    .text
                    .as_deref()
                    .ok_or_else(|| named("`action = \"send_prompt\"` requires `text`".into()))?;
                if text.is_empty() {
                    return Err(named("text cannot be empty".into()));
                }
                if text.len() > MAX_ACTION_TEXT_BYTES {
                    return Err(named(format!(
                        "text cannot exceed {MAX_ACTION_TEXT_BYTES} bytes"
                    )));
                }
                if !text_is_terminal_safe(text) {
                    return Err(named(
                        "text contains terminal control characters; an automation types \
                         this into a live agent, so only printable text, tabs, and \
                         newlines are accepted"
                            .into(),
                    ));
                }
                if self.message.is_some() {
                    return Err(named("`message` belongs to `action = \"notify\"`".into()));
                }
            }
            AutomationAction::Notify => {
                let message = self
                    .message
                    .as_deref()
                    .ok_or_else(|| named("`action = \"notify\"` requires `message`".into()))?;
                if message.trim().is_empty() {
                    return Err(named("message cannot be empty".into()));
                }
                if self.text.is_some() || self.submit.is_some() {
                    return Err(named(
                        "`text` and `submit` belong to `action = \"send_prompt\"`".into(),
                    ));
                }
            }
            AutomationAction::Interrupt => {
                if self.text.is_some() || self.message.is_some() || self.submit.is_some() {
                    return Err(named(
                        "`action = \"interrupt\"` takes no `text`, `message`, or `submit`".into(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// One-line human summary of the rule's filters, for `automation list`
    /// and the macOS table. `any` when the rule is unfiltered.
    #[must_use]
    pub fn filter_summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.agent.is_empty() {
            let kinds: Vec<String> = self.agent.iter().map(|spec| spec.0.to_string()).collect();
            parts.push(format!("agent={}", kinds.join(",")));
        }
        if let Some(workspace) = &self.workspace {
            parts.push(format!("workspace={workspace}"));
        }
        if let Some(work) = &self.work {
            parts.push(format!("work={work}"));
        }
        if let Some(pane) = &self.pane {
            parts.push(format!("pane={pane}"));
        }
        if let Some(host) = &self.host {
            parts.push(format!("host={host}"));
        }
        if !self.scope.is_empty() {
            let scopes: Vec<String> = self
                .scope
                .iter()
                .map(|scope| scope_name(*scope).to_string())
                .collect();
            parts.push(format!("scope={}", scopes.join(",")));
        }
        if let Some(value) = self.for_ {
            parts.push(format!("for={value}"));
        }
        if parts.is_empty() {
            "any".into()
        } else {
            parts.join(" ")
        }
    }
}

/// Same rule the collaboration direct-wake path uses: an automation may
/// type printable text, tabs, and newlines, and nothing else. Escape
/// sequences reaching a live TUI are how a "resume" turns into arbitrary
/// key bindings.
#[must_use]
pub fn text_is_terminal_safe(text: &str) -> bool {
    text.chars()
        .all(|character| matches!(character, '\n' | '\t') || !character.is_control())
}

fn scope_name(scope: RateLimitScope) -> &'static str {
    match scope {
        RateLimitScope::FiveHour => "five_hour",
        RateLimitScope::SevenDay => "seven_day",
        RateLimitScope::Unknown => "unknown",
    }
}

/// `local` matches every pane this daemon can act on — it only ever sees
/// its own node. A backend name narrows to one pane-id namespace.
///
/// The nesting is the point and not a smell: the outer level says whether
/// the filter parsed at all, the inner says whether it narrows to a host
/// kind or (for `local`) matches everything.
#[allow(clippy::option_option)]
fn parse_host_filter(raw: &str) -> Option<Option<HostKind>> {
    match raw.trim().to_ascii_lowercase().as_str() {
        crate::fleet::LOCAL_HOST_ALIAS => Some(None),
        "tmux" => Some(Some(HostKind::Tmux)),
        "cmux" => Some(Some(HostKind::Cmux)),
        "rmux" => Some(Some(HostKind::Rmux)),
        "zellij" => Some(Some(HostKind::Zellij)),
        "herdr" => Some(Some(HostKind::Herdr)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// The subject a rule is evaluated against
// ---------------------------------------------------------------------------

/// Everything a rule can match on, flattened out of the live registry row
/// plus (optionally) the tmux metadata stamped on its pane.
///
/// Separated from [`Agent`] on purpose: it is what makes [`Scheduler`]
/// pure and what lets the tests state a scenario in ten lines.
#[derive(Debug, Clone, PartialEq)]
pub struct AutomationSubject {
    pub agent_session_id: String,
    pub kind: AgentKind,
    pub pane: Option<String>,
    pub socket: Option<String>,
    pub host: Option<HostKind>,
    pub workspace: Option<String>,
    pub work: Option<String>,
    pub state: AgentState,
    pub rate_limit_scope: Option<RateLimitScope>,
    pub rate_limit_source: Option<RateLimitSource>,
    pub rate_limited_until: Option<OffsetDateTime>,
    pub state_entered_at: OffsetDateTime,
    pub last_activity_at: OffsetDateTime,
}

impl AutomationSubject {
    #[must_use]
    pub fn from_agent(agent: &Agent) -> Self {
        Self {
            agent_session_id: agent.session_id.clone(),
            kind: agent.kind,
            pane: agent.pane.clone(),
            socket: agent.tmux_socket.clone(),
            host: agent
                .pane
                .as_deref()
                .and_then(crate::backend::pane_id_host_kind),
            workspace: None,
            work: None,
            state: agent.state,
            rate_limit_scope: agent.rate_limit_scope,
            rate_limit_source: agent.rate_limit_source,
            rate_limited_until: agent.rate_limited_until,
            state_entered_at: agent.state_entered_at,
            last_activity_at: agent.last_activity_at,
        }
    }

    /// Layer the tmux user options stamped on the pane (`workspace_id` /
    /// `work_id`) onto the subject. Panes muxa did not launch carry none,
    /// which is why the workspace/work filters simply do not match them.
    #[must_use]
    pub fn with_pane(mut self, pane: &crate::tmux::PaneInfo) -> Self {
        self.workspace.clone_from(&pane.workspace_id);
        self.work.clone_from(&pane.work_id);
        self
    }

    /// The event this subject *currently is*, if any. Both the transition
    /// path and the reconcile scan go through this, so a row armed by a
    /// broadcast and a row found by a scan are classified identically.
    #[must_use]
    pub fn current_event(&self) -> Option<AutomationEvent> {
        match self.state {
            AgentState::Error if self.rate_limit_scope.is_some() => {
                Some(AutomationEvent::RateLimited)
            }
            AgentState::Error => Some(AutomationEvent::Error),
            AgentState::WaitingInput | AgentState::WaitingChoice => {
                Some(AutomationEvent::WaitingInput)
            }
            AgentState::Idle => Some(AutomationEvent::IdleFor),
            _ => None,
        }
    }

    #[must_use]
    pub fn satisfies(&self, condition: AutomationCondition) -> bool {
        match condition {
            AutomationCondition::Any => true,
            AutomationCondition::RateLimited => {
                self.state == AgentState::Error && self.rate_limit_scope.is_some()
            }
            AutomationCondition::Error => self.state == AgentState::Error,
            AutomationCondition::WaitingInput => {
                matches!(
                    self.state,
                    AgentState::WaitingInput | AgentState::WaitingChoice
                )
            }
            AutomationCondition::Idle => self.state == AgentState::Idle,
        }
    }

    /// Identity of the current *episode* — one uninterrupted stay in the
    /// state that armed the rule. `state_entered_at` only moves when the
    /// state actually changes, so a soft cap upgrading to a hard one, or a
    /// second `RateLimited` landing on an already-`Error` row, stays the
    /// same episode and cannot produce a second firing.
    #[must_use]
    pub fn episode(&self) -> String {
        format!(
            "{}@{}",
            self.state,
            self.state_entered_at
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_else(|_| self.state_entered_at.unix_timestamp().to_string())
        )
    }

    fn matches_filters(&self, rule: &AutomationRule) -> bool {
        if !rule.agent.is_empty() && !rule.agent.iter().any(|spec| spec.0 == self.kind) {
            return false;
        }
        if let Some(pane) = &rule.pane {
            if self.pane.as_deref() != Some(pane.as_str()) {
                return false;
            }
        }
        if let Some(host) = &rule.host {
            // `local` is every pane this daemon governs, so it only has to
            // parse — an unparseable value never reaches here (validation).
            match parse_host_filter(host) {
                Some(Some(kind)) if self.host != Some(kind) => return false,
                None => return false,
                _ => {}
            }
        }
        if let Some(workspace) = &rule.workspace {
            if self.workspace.as_deref() != Some(workspace.as_str()) {
                return false;
            }
        }
        if let Some(Ok(pattern)) = rule.work_regex() {
            match self.work.as_deref() {
                Some(work) if pattern.is_match(work) => {}
                _ => return false,
            }
        }
        if !rule.scope.is_empty() {
            match self.rate_limit_scope {
                Some(scope) if rule.scope.contains(&scope) => {}
                _ => return false,
            }
        }
        true
    }
}

/// Build one [`AutomationSubject`] per live agent, layering on the tmux
/// user options stamped on each row's pane.
///
/// The single place agents and panes are joined, so the daemon's task,
/// `automation_test`, and any future caller evaluate identical inputs.
#[must_use]
pub fn subjects_from(agents: &[Agent], panes: &[crate::tmux::PaneInfo]) -> Vec<AutomationSubject> {
    agents
        .iter()
        .map(|agent| {
            let subject = AutomationSubject::from_agent(agent);
            match subject.pane.as_deref() {
                Some(pane_id) => {
                    match panes.iter().find(|pane| {
                        pane.pane_id == pane_id
                            // Pane ids are only unique per server, so a row
                            // that names its socket has to match on it too.
                            && agent
                                .tmux_socket
                                .as_deref()
                                .is_none_or(|socket| {
                                    pane.socket.as_deref().is_none_or(|value| value == socket)
                                })
                    }) {
                        Some(pane) => subject.with_pane(pane),
                        None => subject,
                    }
                }
                None => subject,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The scheduler
// ---------------------------------------------------------------------------

/// Why a rule did not fire. Recorded in the ledger and printed by
/// `muxa automation test`, so "nothing happened" always has an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum SkipReason {
    /// `[automation] enabled = false`.
    EngineDisabled,
    /// Inside the `paused_until` window.
    Paused,
    /// The rule's own `enabled = false`.
    RuleDisabled,
    /// The agent is not in the state this rule watches.
    EventMismatch,
    /// A filter (agent/workspace/work/pane/host/scope) did not match.
    FilterMismatch,
    /// The row has no pane, so there is nothing to type into.
    NoPane,
    /// This rule already acted on this episode.
    EpisodeAlreadyHandled,
    /// Within `cooldown` of this rule's last firing against this pane.
    Cooldown,
    /// `max_per_hour` reached for this rule and pane.
    HourlyCap,
    /// [`GLOBAL_MAX_PER_HOUR`] reached across every rule.
    GlobalCap,
    /// `only_if_still` no longer holds — the agent recovered while waiting.
    ConditionCleared,
    /// The pane went away between arming and firing.
    PaneGone,
    /// The backend refused the injection.
    ActionFailed,
}

/// The guard state the scheduler needs, read off the ledger by the caller
/// so the decision itself stays pure.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GuardState {
    /// Most recent *successful* firing of this rule against this pane.
    pub last_fired_at: Option<OffsetDateTime>,
    /// Successful firings of this rule against this pane in the last hour.
    pub fired_last_hour: u32,
    /// Successful firings across every rule and pane in the last hour.
    pub global_fired_last_hour: u32,
    /// This rule already acted on this exact episode.
    pub episode_handled: bool,
}

/// A firing the engine has committed to, waiting on its fire time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedFiring {
    pub rule: String,
    pub pane: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<String>,
    pub agent_session_id: String,
    pub agent: AgentKind,
    /// The episode this firing belongs to. Keyed with `(rule, pane)` it is
    /// what makes "never twice for one cap" hold across a daemon restart.
    pub episode: String,
    #[serde(with = "time::serde::rfc3339")]
    pub fire_at: OffsetDateTime,
    pub action: AutomationAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub submit: bool,
    pub only_if_still: AutomationCondition,
}

/// Ordered by fire time so a `BinaryHeap<Reverse<_>>` pops the next one
/// due. The tail keys keep the order total (and therefore stable).
impl Ord for PlannedFiring {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.fire_at
            .cmp(&other.fire_at)
            .then_with(|| self.rule.cmp(&other.rule))
            .then_with(|| self.pane.cmp(&other.pane))
            .then_with(|| self.episode.cmp(&other.episode))
    }
}

impl PartialOrd for PlannedFiring {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// The scheduler's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Fire(Box<PlannedFiring>),
    Skip(SkipReason),
}

impl Decision {
    #[must_use]
    pub fn firing(&self) -> Option<&PlannedFiring> {
        match self {
            Self::Fire(firing) => Some(firing),
            Self::Skip(_) => None,
        }
    }

    #[must_use]
    pub fn skip_reason(&self) -> Option<SkipReason> {
        match self {
            Self::Skip(reason) => Some(*reason),
            Self::Fire(_) => None,
        }
    }
}

/// Pure rule evaluation. No clock reads, no I/O, no interior state — every
/// input is an argument, which is what makes the guards testable to the
/// second.
#[derive(Debug, Clone, Copy)]
pub struct Scheduler;

impl Scheduler {
    /// Decide whether `rule` should act on `subject`, and when.
    ///
    /// `jitter_ratio` is the caller's random number in `[0, 1)`; tests pass
    /// `0.0` and get an exact fire time. `now` is the injected clock.
    #[must_use]
    pub fn arm(
        config: &AutomationConfig,
        rule: &AutomationRule,
        subject: &AutomationSubject,
        guards: GuardState,
        now: OffsetDateTime,
        jitter_ratio: f64,
    ) -> Decision {
        if !config.enabled {
            return Decision::Skip(SkipReason::EngineDisabled);
        }
        if config.paused_until.is_some_and(|until| now < until) {
            return Decision::Skip(SkipReason::Paused);
        }
        if !rule.enabled {
            return Decision::Skip(SkipReason::RuleDisabled);
        }
        if subject.current_event() != Some(rule.on) {
            return Decision::Skip(SkipReason::EventMismatch);
        }
        if !subject.matches_filters(rule) {
            return Decision::Skip(SkipReason::FilterMismatch);
        }
        let Some(pane) = subject.pane.clone() else {
            return Decision::Skip(SkipReason::NoPane);
        };
        if guards.episode_handled {
            return Decision::Skip(SkipReason::EpisodeAlreadyHandled);
        }
        if let Some(reason) = Self::guard_breach(rule, guards, now) {
            return Decision::Skip(reason);
        }

        let fire_at = Self::fire_at(rule, subject, now, jitter_ratio);
        Decision::Fire(Box::new(PlannedFiring {
            rule: rule.name.clone(),
            pane,
            socket: subject.socket.clone(),
            agent_session_id: subject.agent_session_id.clone(),
            agent: subject.kind,
            episode: subject.episode(),
            fire_at,
            action: rule.action,
            text: match rule.action {
                AutomationAction::SendPrompt => rule.text.clone(),
                AutomationAction::Notify => rule.message.clone(),
                AutomationAction::Interrupt => None,
            },
            submit: rule.submit(),
            only_if_still: rule.only_if_still(),
        }))
    }

    /// The fire-time re-check. `subject` is the row as the live store has
    /// it *now* — `None` when the pane or session is gone.
    #[must_use]
    pub fn confirm(
        config: &AutomationConfig,
        rule: &AutomationRule,
        firing: &PlannedFiring,
        subject: Option<&AutomationSubject>,
        guards: GuardState,
        now: OffsetDateTime,
    ) -> Decision {
        if !config.enabled {
            return Decision::Skip(SkipReason::EngineDisabled);
        }
        if config.paused_until.is_some_and(|until| now < until) {
            return Decision::Skip(SkipReason::Paused);
        }
        if !rule.enabled {
            return Decision::Skip(SkipReason::RuleDisabled);
        }
        let Some(subject) = subject else {
            return Decision::Skip(SkipReason::PaneGone);
        };
        if subject.pane.as_deref() != Some(firing.pane.as_str()) {
            return Decision::Skip(SkipReason::PaneGone);
        }
        // A row that left and re-entered the state is a different episode;
        // this firing was armed for the old one and must not carry over.
        if subject.episode() != firing.episode {
            return Decision::Skip(SkipReason::ConditionCleared);
        }
        if !subject.satisfies(firing.only_if_still) {
            return Decision::Skip(SkipReason::ConditionCleared);
        }
        if guards.episode_handled {
            return Decision::Skip(SkipReason::EpisodeAlreadyHandled);
        }
        if let Some(reason) = Self::guard_breach(rule, guards, now) {
            return Decision::Skip(reason);
        }
        Decision::Fire(Box::new(firing.clone()))
    }

    fn guard_breach(
        rule: &AutomationRule,
        guards: GuardState,
        now: OffsetDateTime,
    ) -> Option<SkipReason> {
        if guards.global_fired_last_hour >= GLOBAL_MAX_PER_HOUR {
            return Some(SkipReason::GlobalCap);
        }
        if guards.fired_last_hour >= rule.max_per_hour() {
            return Some(SkipReason::HourlyCap);
        }
        if let Some(last) = guards.last_fired_at {
            if now - last < rule.cooldown() {
                return Some(SkipReason::Cooldown);
            }
        }
        None
    }

    /// When the firing is due. Never in the past: a cap whose reset time has
    /// already gone by fires now, not retroactively.
    fn fire_at(
        rule: &AutomationRule,
        subject: &AutomationSubject,
        now: OffsetDateTime,
        jitter_ratio: f64,
    ) -> OffsetDateTime {
        let jitter = jitter_offset(rule.jitter(), jitter_ratio);
        let base = match rule.on {
            // `for` is the delay; it counts from when the row went idle,
            // not from when we noticed.
            AutomationEvent::IdleFor => {
                subject.state_entered_at
                    + rule
                        .for_
                        .map_or(time::Duration::ZERO, DurationSpec::duration)
            }
            _ => match rule.wait().anchor {
                WaitAnchor::Event => now + rule.wait().offset,
                WaitAnchor::Reset => match subject.rate_limited_until {
                    Some(reset) => reset + rule.wait().offset,
                    // No reset time on this cap (a `StopFailure` 429 carries
                    // none): the offset had nothing to anchor to, so the
                    // fallback replaces the whole expression.
                    None => now + rule.fallback(),
                },
            },
        };
        let planned = base + jitter;
        if planned < now {
            now
        } else {
            planned
        }
    }
}

/// `ratio * jitter`, clamped into `[0, jitter]`.
fn jitter_offset(jitter: time::Duration, ratio: f64) -> time::Duration {
    if jitter.is_zero() {
        return time::Duration::ZERO;
    }
    let ratio = ratio.clamp(0.0, 1.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    let seconds = (jitter.whole_seconds() as f64 * ratio) as i64;
    time::Duration::seconds(seconds)
}

// ---------------------------------------------------------------------------
// Ledger
// ---------------------------------------------------------------------------

/// What became of one evaluated firing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum AutomationOutcome {
    /// The action reached the pane.
    Fired,
    /// A guard or the fire-time re-check stopped it.
    Skipped,
    /// The action was attempted and the backend refused it.
    Failed,
}

/// One line of the durable firing record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationLedgerEntry {
    pub rule: String,
    pub pane: String,
    pub agent: AgentKind,
    #[serde(with = "time::serde::rfc3339")]
    pub fired_at: OffsetDateTime,
    pub action: AutomationAction,
    pub outcome: AutomationOutcome,
    /// The skip reason, the failure, or the text that was sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The episode this entry belongs to. Additive on top of the shape the
    /// brief specified: it is what lets the one-firing-per-episode guard
    /// survive a daemon restart, since the in-memory in-flight set does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct LedgerSnapshot {
    #[serde(default)]
    entries: Vec<AutomationLedgerEntry>,
}

/// Durable, bounded record of every automation decision.
///
/// Doubles as the guard input: `cooldown` and `max_per_hour` are read from
/// here, so restarting the daemon does not reset an agent's budget.
#[derive(Debug)]
pub struct AutomationLedger {
    path: Option<PathBuf>,
    entries: RwLock<Vec<AutomationLedgerEntry>>,
    /// Serializes each mutation with its snapshot write, so a reader never
    /// sees an entry the file does not have.
    write_lock: Mutex<()>,
}

impl AutomationLedger {
    #[must_use]
    pub fn in_memory() -> Arc<Self> {
        Arc::new(Self {
            path: None,
            entries: RwLock::new(Vec::new()),
            write_lock: Mutex::new(()),
        })
    }

    /// Read the ledger back. A missing or unreadable file starts empty —
    /// the guards then run from a clean slate, which is conservative in the
    /// direction of firing, so the global cap remains the backstop.
    #[must_use]
    pub fn load(path: Option<PathBuf>) -> Arc<Self> {
        let mut entries = path
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str::<LedgerSnapshot>(&text).ok())
            .unwrap_or_default()
            .entries;
        trim(&mut entries);
        Arc::new(Self {
            path,
            entries: RwLock::new(entries),
            write_lock: Mutex::new(()),
        })
    }

    pub async fn append(&self, entry: AutomationLedgerEntry) {
        let _guard = self.write_lock.lock().await;
        {
            let mut entries = self.entries.write().await;
            entries.push(entry);
            trim(&mut entries);
        }
        self.persist().await;
    }

    /// Newest first, capped at `limit`.
    pub async fn recent(&self, limit: usize) -> Vec<AutomationLedgerEntry> {
        let entries = self.entries.read().await;
        entries.iter().rev().take(limit).cloned().collect()
    }

    pub async fn all(&self) -> Vec<AutomationLedgerEntry> {
        self.entries.read().await.clone()
    }

    /// Everything the scheduler's guards need for one `(rule, pane,
    /// episode)`, computed in a single pass over the ledger.
    pub async fn guard_state(
        &self,
        rule: &str,
        pane: &str,
        episode: &str,
        now: OffsetDateTime,
    ) -> GuardState {
        let entries = self.entries.read().await;
        let hour_ago = now - time::Duration::hours(1);
        let mut state = GuardState::default();
        for entry in entries.iter() {
            let fired = entry.outcome == AutomationOutcome::Fired;
            if fired && entry.fired_at >= hour_ago {
                state.global_fired_last_hour = state.global_fired_last_hour.saturating_add(1);
            }
            if entry.rule != rule || entry.pane != pane {
                continue;
            }
            // A failed attempt still consumed the episode: retrying it would
            // re-send the same keystrokes into a pane that may have taken
            // the first ones.
            if entry.outcome != AutomationOutcome::Skipped
                && entry.episode.as_deref() == Some(episode)
            {
                state.episode_handled = true;
            }
            if !fired {
                continue;
            }
            if entry.fired_at >= hour_ago {
                state.fired_last_hour = state.fired_last_hour.saturating_add(1);
            }
            state.last_fired_at = Some(
                state
                    .last_fired_at
                    .map_or(entry.fired_at, |current| current.max(entry.fired_at)),
            );
        }
        state
    }

    /// `(fired-in-the-last-hour, last-fired-at)` per rule, for the list view.
    pub async fn rule_activity(
        &self,
        now: OffsetDateTime,
    ) -> HashMap<String, (u32, OffsetDateTime)> {
        let entries = self.entries.read().await;
        let hour_ago = now - time::Duration::hours(1);
        let mut activity: HashMap<String, (u32, OffsetDateTime)> = HashMap::new();
        for entry in entries.iter() {
            if entry.outcome != AutomationOutcome::Fired {
                continue;
            }
            let slot = activity
                .entry(entry.rule.clone())
                .or_insert((0, entry.fired_at));
            if entry.fired_at >= hour_ago {
                slot.0 = slot.0.saturating_add(1);
            }
            slot.1 = slot.1.max(entry.fired_at);
        }
        activity
    }

    /// Snapshot to disk. Best-effort: an unwritable path degrades to an
    /// in-memory ledger rather than blocking the action the operator asked
    /// for. Write-then-rename so a reader never catches a half-written file.
    async fn persist(&self) {
        let Some(path) = self.path.as_ref() else {
            return;
        };
        let snapshot = LedgerSnapshot {
            entries: self.entries.read().await.clone(),
        };
        let Ok(text) = serde_json::to_string_pretty(&snapshot) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

fn trim(entries: &mut Vec<AutomationLedgerEntry>) {
    if entries.len() > MAX_LEDGER_ENTRIES {
        entries.drain(..entries.len() - MAX_LEDGER_ENTRIES);
    }
}

// ---------------------------------------------------------------------------
// Views published over IPC
// ---------------------------------------------------------------------------

/// One row of `automation_rules`.
///
/// Deliberately flat, and deliberately *complete*: it carries the rule's
/// own filters and payload verbatim alongside the effective timing and
/// guards. A visual editor can therefore load a rule, change one field,
/// and hand the whole thing back to `automation_set_rule` without losing
/// anything it did not render — while a table view reads the pre-resolved
/// `wait` / `cooldown` / `filters` strings and never has to know the
/// duration grammar or the defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutomationRuleView {
    pub name: String,
    pub on: AutomationEvent,
    pub enabled: bool,
    pub action: AutomationAction,

    // --- the rule's filters, verbatim ---
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent: Vec<AgentKindSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scope: Vec<RateLimitScope>,
    #[serde(default, rename = "for", skip_serializing_if = "Option::is_none")]
    pub for_: Option<DurationSpec>,

    // --- the action's payload, verbatim ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub submit: bool,

    // --- effective timing and guards, defaults already resolved ---
    /// `reset+2m` / `5m`.
    pub wait: String,
    pub fallback: String,
    pub jitter: String,
    pub cooldown: String,
    pub max_per_hour: u32,
    pub only_if_still: AutomationCondition,

    // --- derived ---
    /// One-line filter summary, or `any`.
    pub filters: String,
    pub fired_last_hour: u32,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub last_fired_at: Option<OffsetDateTime>,
}

/// The whole `automation_list` answer: engine state plus the rules.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutomationRules {
    pub enabled: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub paused_until: Option<OffsetDateTime>,
    pub rules: Vec<AutomationRuleView>,
    /// The ceiling every rule shares, so a client can state the guard it is
    /// actually bound by instead of hard-coding the number.
    #[serde(default = "default_global_max_per_hour")]
    pub global_max_per_hour: u32,
}

fn default_global_max_per_hour() -> u32 {
    GLOBAL_MAX_PER_HOUR
}

/// What `muxa automation test <name>` found, firing nothing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutomationTestReport {
    pub rule: String,
    pub enabled: bool,
    pub engine_enabled: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub paused_until: Option<OffsetDateTime>,
    pub candidates: Vec<AutomationTestCandidate>,
}

/// One agent the rule was evaluated against.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutomationTestCandidate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
    pub agent_session_id: String,
    pub agent: AgentKind,
    pub state: AgentState,
    /// `fire`, or the [`SkipReason`].
    pub decision: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub fire_at: Option<OffsetDateTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

// ---------------------------------------------------------------------------
// The live store
// ---------------------------------------------------------------------------

/// The handle the daemon task and the IPC layer share: effective config,
/// the ledger, and write-back to `config.toml` for edits made at runtime.
///
/// The daemon reads config once at startup and does not watch the file, so
/// a runtime edit has to change both the live copy (immediate effect) and
/// the file (survives a restart). Every mutator does exactly that, and
/// refuses the edit if the merged file would not load.
#[derive(Debug)]
pub struct AutomationStore {
    config: RwLock<AutomationConfig>,
    ledger: Arc<AutomationLedger>,
    config_path: Option<PathBuf>,
    changes: watch::Sender<u64>,
}

impl AutomationStore {
    #[must_use]
    pub fn new(
        config: AutomationConfig,
        config_path: Option<PathBuf>,
        ledger: Arc<AutomationLedger>,
    ) -> Arc<Self> {
        let (changes, _) = watch::channel(0);
        Arc::new(Self {
            config: RwLock::new(config),
            ledger,
            config_path,
            changes,
        })
    }

    #[must_use]
    pub fn in_memory(config: AutomationConfig) -> Arc<Self> {
        Self::new(config, None, AutomationLedger::in_memory())
    }

    #[must_use]
    pub fn ledger(&self) -> Arc<AutomationLedger> {
        self.ledger.clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    fn publish_change(&self) {
        self.changes
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }

    pub async fn config(&self) -> AutomationConfig {
        self.config.read().await.clone()
    }

    pub async fn rules(&self) -> Vec<AutomationRule> {
        self.config.read().await.rule.clone()
    }

    /// Rules that could fire, and whether the engine is on. Both are read
    /// under one lock so the task never acts on a half-applied edit.
    pub async fn active_rules(&self, now: OffsetDateTime) -> (bool, Vec<AutomationRule>) {
        let config = self.config.read().await;
        (
            config.active_at(now),
            config
                .rule
                .iter()
                .filter(|rule| rule.enabled)
                .cloned()
                .collect(),
        )
    }

    /// The `automation_list` payload.
    pub async fn views(&self, now: OffsetDateTime) -> AutomationRules {
        let config = self.config.read().await.clone();
        let activity = self.ledger.rule_activity(now).await;
        let rules = config
            .rule
            .iter()
            .map(|rule| {
                let (fired_last_hour, last_fired_at) = activity
                    .get(&rule.name)
                    .map_or((0, None), |(count, at)| (*count, Some(*at)));
                AutomationRuleView {
                    name: rule.name.clone(),
                    on: rule.on,
                    enabled: rule.enabled,
                    action: rule.action,
                    agent: rule.agent.clone(),
                    workspace: rule.workspace.clone(),
                    work: rule.work.clone(),
                    pane: rule.pane.clone(),
                    host: rule.host.clone(),
                    scope: rule.scope.clone(),
                    for_: rule.for_,
                    text: rule.text.clone(),
                    message: rule.message.clone(),
                    submit: rule.submit(),
                    wait: rule.wait().to_string(),
                    fallback: render_duration(rule.fallback()),
                    jitter: render_duration(rule.jitter()),
                    cooldown: render_duration(rule.cooldown()),
                    max_per_hour: rule.max_per_hour(),
                    only_if_still: rule.only_if_still(),
                    filters: rule.filter_summary(),
                    fired_last_hour,
                    last_fired_at,
                }
            })
            .collect();
        AutomationRules {
            enabled: config.enabled,
            paused_until: config.paused_until,
            rules,
            global_max_per_hour: GLOBAL_MAX_PER_HOUR,
        }
    }

    /// Flip one rule's `enabled`. Takes effect immediately and is written
    /// back to `config.toml`.
    pub async fn set_rule_enabled(&self, name: &str, enabled: bool) -> Result<(), String> {
        let mut config = self.config.write().await;
        let rule = config
            .rule
            .iter_mut()
            .find(|rule| rule.name == name)
            .ok_or_else(|| format!("no automation rule named {name:?}"))?;
        if rule.enabled == enabled {
            return Ok(());
        }
        rule.enabled = enabled;
        let persisted = self.persist_rule_enabled(name, enabled);
        drop(config);
        self.publish_change();
        persisted
    }

    /// Turn the whole engine on or off. Unlike `paused_until`, this is the
    /// operator's standing decision, so it is written back to
    /// `[automation] enabled` and survives a restart.
    pub async fn set_master_enabled(&self, enabled: bool) -> Result<(), String> {
        let mut config = self.config.write().await;
        if config.enabled == enabled {
            return Ok(());
        }
        config.enabled = enabled;
        let persisted = self.persist_master_enabled(enabled);
        drop(config);
        self.publish_change();
        persisted
    }

    /// Hold every rule until `until` (or lift the hold with `None`).
    pub async fn set_paused_until(&self, until: Option<OffsetDateTime>) -> Result<(), String> {
        let mut config = self.config.write().await;
        config.paused_until = until;
        let persisted = self.persist_paused_until(until);
        drop(config);
        self.publish_change();
        persisted
    }

    /// Write a rule, replacing the one with the same name in place or
    /// appending it. The merged document has to read back as a full
    /// [`Config`] before anything touches disk.
    pub async fn upsert_rule(&self, rule: AutomationRule) -> Result<(), String> {
        rule.validate()?;
        let mut config = self.config.write().await;
        // Validate the whole set as the loader would, so a duplicate name
        // or a cross-rule invariant fails here rather than at next startup.
        let mut candidate = config.clone();
        match candidate.rule.iter().position(|r| r.name == rule.name) {
            Some(index) => candidate.rule[index] = rule.clone(),
            None => candidate.rule.push(rule.clone()),
        }
        candidate.validate()?;
        self.persist_rule(&rule)?;
        *config = candidate;
        drop(config);
        self.publish_change();
        Ok(())
    }

    /// Remove a rule. An unknown name is refused rather than silently
    /// succeeding — a GUI that lost sync should be told.
    pub async fn remove_rule(&self, name: &str) -> Result<(), String> {
        let mut config = self.config.write().await;
        let index = config
            .rule
            .iter()
            .position(|rule| rule.name == name)
            .ok_or_else(|| format!("no automation rule named {name:?}"))?;
        self.persist_remove_rule(name)?;
        config.rule.remove(index);
        drop(config);
        self.publish_change();
        Ok(())
    }

    /// Evaluate one rule against a set of live subjects without acting.
    /// Guards are read exactly as a real firing would read them.
    pub async fn test_rule(
        &self,
        name: &str,
        subjects: &[AutomationSubject],
        now: OffsetDateTime,
    ) -> Result<AutomationTestReport, String> {
        let config = self.config.read().await.clone();
        let rule = config
            .rule_named(name)
            .ok_or_else(|| format!("no automation rule named {name:?}"))?
            .clone();
        let mut candidates = Vec::new();
        for subject in subjects {
            let guards = match subject.pane.as_deref() {
                Some(pane) => {
                    self.ledger
                        .guard_state(&rule.name, pane, &subject.episode(), now)
                        .await
                }
                None => GuardState::default(),
            };
            // Ratio 0 so the report names the earliest time it could fire;
            // the real firing adds up to `jitter` on top.
            let decision = Scheduler::arm(&config, &rule, subject, guards, now, 0.0);
            candidates.push(AutomationTestCandidate {
                pane: subject.pane.clone(),
                agent_session_id: subject.agent_session_id.clone(),
                agent: subject.kind,
                state: subject.state,
                decision: decision
                    .skip_reason()
                    .map_or_else(|| "fire".to_string(), |reason| reason.to_string()),
                fire_at: decision.firing().map(|firing| firing.fire_at),
                detail: decision.firing().and_then(|firing| firing.text.clone()),
            });
        }
        Ok(AutomationTestReport {
            rule: rule.name,
            enabled: rule.enabled,
            engine_enabled: config.enabled,
            paused_until: config.paused_until,
            candidates,
        })
    }

    // --- config.toml write-back -------------------------------------------

    fn persist_rule_enabled(&self, name: &str, enabled: bool) -> Result<(), String> {
        self.edit_config(|document| {
            let rules = automation_rules_mut(document)?;
            let table = rules
                .iter_mut()
                .find(|table| table_name(table).as_deref() == Some(name))
                .ok_or_else(|| format!("no [[automation.rule]] named {name:?} in config.toml"))?;
            table.insert("enabled", toml_edit::value(enabled));
            Ok(())
        })
    }

    fn persist_master_enabled(&self, enabled: bool) -> Result<(), String> {
        self.edit_config(|document| {
            let automation = implicit_table(document.as_table_mut(), "automation")?;
            automation.set_implicit(false);
            automation.insert("enabled", toml_edit::value(enabled));
            Ok(())
        })
    }

    fn persist_paused_until(&self, until: Option<OffsetDateTime>) -> Result<(), String> {
        self.edit_config(|document| {
            let automation = implicit_table(document.as_table_mut(), "automation")?;
            automation.set_implicit(false);
            match until {
                Some(until) => {
                    let text = until
                        .format(&time::format_description::well_known::Rfc3339)
                        .map_err(|error| format!("formatting paused_until: {error}"))?;
                    automation.insert("paused_until", toml_edit::value(text));
                }
                None => {
                    automation.remove("paused_until");
                }
            }
            Ok(())
        })
    }

    fn persist_rule(&self, rule: &AutomationRule) -> Result<(), String> {
        self.edit_config(|document| {
            let rules = automation_rules_mut(document)?;
            let mut table = rule_table(rule)?;
            let existing = rules
                .iter()
                .position(|existing| table_name(existing).as_deref() == Some(rule.name.as_str()));
            match existing {
                // Replace in place so the operator's ordering survives.
                Some(index) => {
                    let position = rules.get(index).and_then(toml_edit::Table::position);
                    if let Some(position) = position {
                        table.set_position(position);
                    }
                    let slot = rules
                        .get_mut(index)
                        .ok_or_else(|| "automation.rule vanished mid-edit".to_string())?;
                    *slot = table;
                }
                None => rules.push(table),
            }
            Ok(())
        })
    }

    fn persist_remove_rule(&self, name: &str) -> Result<(), String> {
        self.edit_config(|document| {
            let rules = automation_rules_mut(document)?;
            let Some(index) = rules
                .iter()
                .position(|table| table_name(table).as_deref() == Some(name))
            else {
                // Already absent from the file (a hand edit raced us). The
                // live removal still stands; nothing to write.
                return Ok(());
            };
            rules.remove(index);
            if rules.is_empty() {
                if let Some(automation) = document
                    .get_mut("automation")
                    .and_then(toml_edit::Item::as_table_mut)
                {
                    automation.remove("rule");
                    if automation.is_empty() {
                        document.remove("automation");
                    }
                }
            }
            Ok(())
        })
    }

    /// Apply `edit` to `config.toml`, keeping every other byte, and refuse
    /// the write unless the result reads back as a full [`Config`].
    /// A store with no config path (tests, embedders) edits memory only.
    fn edit_config(
        &self,
        edit: impl FnOnce(&mut toml_edit::DocumentMut) -> Result<(), String>,
    ) -> Result<(), String> {
        let Some(path) = self.config_path.as_ref() else {
            return Ok(());
        };
        let mut document = load_document(path)?;
        edit(&mut document)?;
        let text = document.to_string();
        let config: Config = toml::from_str(&text).map_err(|error| {
            format!("the updated config would not parse, so it was not written: {error}")
        })?;
        config.validate().map_err(|error| {
            format!("the updated config is invalid, so it was not written: {error}")
        })?;
        atomic_write(path, &text)
    }
}

fn table_name(table: &toml_edit::Table) -> Option<String> {
    table.get("name")?.as_str().map(str::to_string)
}

/// The `automation.rule` array of tables, created (with an implicit
/// `[automation]` parent) when absent.
fn automation_rules_mut(
    document: &mut toml_edit::DocumentMut,
) -> Result<&mut toml_edit::ArrayOfTables, String> {
    let automation = implicit_table(document.as_table_mut(), "automation")?;
    if automation.get("rule").is_none() {
        automation.insert(
            "rule",
            toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()),
        );
    }
    automation
        .get_mut("rule")
        .and_then(toml_edit::Item::as_array_of_tables_mut)
        .ok_or_else(|| "`automation.rule` in config.toml is not an array of tables".into())
}

/// Render a rule as the `[[automation.rule]]` table the loader reads back,
/// key for key. Built by serializing the rule itself, so a new field cannot
/// be silently dropped on write-back.
fn rule_table(rule: &AutomationRule) -> Result<toml_edit::Table, String> {
    let text = toml::to_string(rule).map_err(|error| format!("rendering rule: {error}"))?;
    let document: toml_edit::DocumentMut = text
        .parse()
        .map_err(|error| format!("rendering rule: {error}"))?;
    let mut table = document.as_table().clone();
    table.set_implicit(false);
    Ok(table)
}

fn implicit_table<'a>(
    table: &'a mut toml_edit::Table,
    key: &str,
) -> Result<&'a mut toml_edit::Table, String> {
    if table.get(key).is_none() {
        let mut fresh = toml_edit::Table::new();
        fresh.set_implicit(true);
        table.insert(key, toml_edit::Item::Table(fresh));
    }
    table
        .get_mut(key)
        .and_then(toml_edit::Item::as_table_mut)
        .ok_or_else(|| format!("`{key}` in config.toml is not a table"))
}

fn load_document(path: &Path) -> Result<toml_edit::DocumentMut, String> {
    match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => text
            .parse::<toml_edit::DocumentMut>()
            .map_err(|error| format!("parsing {}: {error}", path.display())),
        Ok(_) => Ok(toml_edit::DocumentMut::new()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(toml_edit::DocumentMut::new())
        }
        Err(error) => Err(format!("reading {}: {error}", path.display())),
    }
}

/// Write-then-rename in the target's directory, keeping the mode of the
/// file being replaced (a fresh file is owner-only).
fn atomic_write(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write as _;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("creating {}: {error}", parent.display()))?;
    }
    let permissions = match std::fs::metadata(path) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("reading mode of {}: {error}", path.display())),
    };
    let tmp = path.with_extension(format!("toml.{}.tmp", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        match permissions {
            Some(permissions) => file.set_permissions(permissions)?,
            #[cfg(unix)]
            None => {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
            #[cfg(not(unix))]
            None => {}
        }
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("writing {}: {error}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-09-03 12:00:00 UTC);

    fn resume_rule() -> AutomationRule {
        let mut rule = AutomationRule::new(
            "resume-after-limit",
            AutomationEvent::RateLimited,
            AutomationAction::SendPrompt,
        );
        rule.text = Some("continue".into());
        rule.wait = Some(WaitSpec::after_reset(time::Duration::minutes(2)));
        rule.fallback = Some(DurationSpec::seconds(20 * 60));
        rule.jitter = Some(DurationSpec::seconds(0));
        rule
    }

    fn capped_subject() -> AutomationSubject {
        AutomationSubject {
            agent_session_id: "sess-1".into(),
            kind: AgentKind::ClaudeCode,
            pane: Some("%42".into()),
            socket: Some("default".into()),
            host: Some(HostKind::Tmux),
            workspace: Some("callabo".into()),
            work: Some("CAL-1234".into()),
            state: AgentState::Error,
            rate_limit_scope: Some(RateLimitScope::FiveHour),
            rate_limit_source: Some(RateLimitSource::Statusline),
            rate_limited_until: Some(NOW + time::Duration::hours(1)),
            state_entered_at: NOW,
            last_activity_at: NOW,
        }
    }

    // --- duration / wait parsing ------------------------------------------

    #[test]
    fn durations_parse_every_unit() {
        assert_eq!(parse_duration("30s").unwrap(), time::Duration::seconds(30));
        assert_eq!(parse_duration("5m").unwrap(), time::Duration::minutes(5));
        assert_eq!(parse_duration("2h").unwrap(), time::Duration::hours(2));
        assert_eq!(parse_duration("1d").unwrap(), time::Duration::days(1));
        assert_eq!(parse_duration("0").unwrap(), time::Duration::ZERO);
    }

    #[test]
    fn durations_require_a_unit() {
        // A bare number reads as seconds to one operator and minutes to the
        // next; the difference between those two is a runaway.
        assert!(parse_duration("20").is_err());
        assert!(parse_duration("").is_err());
        assert!(parse_duration("5x").is_err());
        assert!(parse_duration("m").is_err());
    }

    #[test]
    fn durations_render_in_the_largest_exact_unit() {
        assert_eq!(render_duration(time::Duration::seconds(90)), "90s");
        assert_eq!(render_duration(time::Duration::seconds(120)), "2m");
        assert_eq!(render_duration(time::Duration::seconds(7200)), "2h");
        assert_eq!(render_duration(time::Duration::ZERO), "0s");
    }

    #[test]
    fn wait_parses_reset_and_offsets() {
        assert_eq!(
            parse_wait("reset").unwrap(),
            WaitSpec::after_reset(time::Duration::ZERO)
        );
        assert_eq!(
            parse_wait("reset+2m").unwrap(),
            WaitSpec::after_reset(time::Duration::minutes(2))
        );
        assert_eq!(
            parse_wait("reset-30s").unwrap(),
            WaitSpec::after_reset(time::Duration::seconds(-30))
        );
        assert_eq!(
            parse_wait("20m").unwrap(),
            WaitSpec::after_event(time::Duration::minutes(20))
        );
        assert!(parse_wait("reset*2m").is_err());
    }

    #[test]
    fn wait_round_trips_through_its_rendering() {
        for text in ["reset", "reset+2m", "reset-30s", "20m", "45s"] {
            assert_eq!(parse_wait(text).unwrap().to_string(), text);
        }
    }

    // --- config shape ------------------------------------------------------

    #[test]
    fn a_fresh_install_is_enabled_with_no_rules() {
        let config = AutomationConfig::default();
        assert!(config.enabled);
        assert!(config.rule.is_empty());
        assert!(config.active_at(NOW));
    }

    #[test]
    fn the_documented_rule_parses() {
        let toml = r#"
enabled = true

[[rule]]
name = "resume-after-limit"
on = "rate_limited"
agent = ["claude", "codex"]
workspace = "callabo"
work = "^CAL-"
scope = ["five_hour"]
wait = "reset+2m"
fallback = "20m"
jitter = "30s"
action = "send_prompt"
text = "continue"
max_per_hour = 2
cooldown = "5m"
only_if_still = "rate_limited"
"#;
        let config: AutomationConfig = toml::from_str(toml).unwrap();
        config.validate().unwrap();
        let rule = &config.rule[0];
        assert_eq!(rule.on, AutomationEvent::RateLimited);
        assert_eq!(
            rule.agent,
            vec![
                AgentKindSpec(AgentKind::ClaudeCode),
                AgentKindSpec(AgentKind::Codex)
            ]
        );
        assert_eq!(
            rule.wait(),
            WaitSpec::after_reset(time::Duration::minutes(2))
        );
        assert_eq!(rule.max_per_hour(), 2);
        assert_eq!(rule.cooldown(), time::Duration::minutes(5));
        assert!(rule.submit());
    }

    #[test]
    fn unknown_keys_are_refused() {
        let toml = r#"
[[rule]]
name = "typo"
on = "rate_limited"
action = "send_prompt"
text = "continue"
waaait = "5m"
"#;
        assert!(toml::from_str::<AutomationConfig>(toml).is_err());
    }

    #[test]
    fn validation_rejects_inconsistent_rules() {
        let mut missing_text = AutomationRule::new(
            "a",
            AutomationEvent::RateLimited,
            AutomationAction::SendPrompt,
        );
        assert!(missing_text.validate().is_err());
        missing_text.text = Some("continue".into());
        missing_text.validate().unwrap();

        let mut idle =
            AutomationRule::new("b", AutomationEvent::IdleFor, AutomationAction::Interrupt);
        assert!(idle.validate().is_err(), "idle_for needs `for`");
        idle.for_ = Some(DurationSpec::seconds(600));
        idle.validate().unwrap();

        let mut reset_on_idle = idle.clone();
        reset_on_idle.wait = Some(WaitSpec::after_reset(time::Duration::ZERO));
        assert!(
            reset_on_idle.validate().is_err(),
            "only a cap carries a reset time"
        );

        let mut scoped =
            AutomationRule::new("c", AutomationEvent::WaitingInput, AutomationAction::Notify);
        scoped.message = Some("blocked".into());
        scoped.validate().unwrap();
        scoped.scope = vec![RateLimitScope::FiveHour];
        assert!(scoped.validate().is_err());
    }

    #[test]
    fn validation_rejects_terminal_control_sequences_in_text() {
        let mut rule = resume_rule();
        rule.text = Some("continue\u{1b}[2J".into());
        let error = rule.validate().unwrap_err();
        assert!(error.contains("terminal control"), "{error}");
    }

    #[test]
    fn validation_rejects_duplicate_names() {
        let config = AutomationConfig {
            rule: vec![resume_rule(), resume_rule()],
            ..AutomationConfig::default()
        };
        assert!(config.validate().unwrap_err().contains("more than once"));
    }

    #[test]
    fn validation_rejects_a_bad_work_regex_and_host() {
        let mut rule = resume_rule();
        rule.work = Some("^CAL-[".into());
        assert!(rule.validate().is_err());
        let mut rule = resume_rule();
        rule.host = Some("mars".into());
        assert!(rule.validate().is_err());
        rule.host = Some("local".into());
        rule.validate().unwrap();
        rule.host = Some("tmux".into());
        rule.validate().unwrap();
    }

    // --- scheduler: the producers -----------------------------------------

    #[test]
    fn a_cap_with_a_reset_time_fires_at_reset_plus_offset() {
        let config = AutomationConfig::default();
        let decision = Scheduler::arm(
            &config,
            &resume_rule(),
            &capped_subject(),
            GuardState::default(),
            NOW,
            0.0,
        );
        let firing = decision.firing().expect("should fire");
        assert_eq!(firing.fire_at, NOW + time::Duration::minutes(62));
        assert_eq!(firing.text.as_deref(), Some("continue"));
        assert!(firing.submit);
        assert_eq!(firing.only_if_still, AutomationCondition::RateLimited);
    }

    #[test]
    fn a_cap_without_a_reset_time_uses_the_fallback() {
        // `StopFailure` (a live 429) and the transcript scan carry no reset
        // time — the fallback is the only thing standing between them and a
        // firing that never happens.
        let mut subject = capped_subject();
        subject.rate_limited_until = None;
        subject.rate_limit_source = Some(RateLimitSource::StopFailure);
        let decision = Scheduler::arm(
            &AutomationConfig::default(),
            &resume_rule(),
            &subject,
            GuardState::default(),
            NOW,
            0.0,
        );
        assert_eq!(
            decision.firing().unwrap().fire_at,
            NOW + time::Duration::minutes(20)
        );
    }

    #[test]
    fn a_codex_rollout_cap_uses_its_own_reset_time() {
        let mut subject = capped_subject();
        subject.kind = AgentKind::Codex;
        subject.rate_limit_source = Some(RateLimitSource::CodexRollout);
        subject.rate_limited_until = Some(NOW + time::Duration::minutes(30));
        let decision = Scheduler::arm(
            &AutomationConfig::default(),
            &resume_rule(),
            &subject,
            GuardState::default(),
            NOW,
            0.0,
        );
        assert_eq!(
            decision.firing().unwrap().fire_at,
            NOW + time::Duration::minutes(32)
        );
    }

    #[test]
    fn a_reset_time_already_in_the_past_fires_now_not_retroactively() {
        let mut subject = capped_subject();
        subject.rate_limited_until = Some(NOW - time::Duration::hours(3));
        let decision = Scheduler::arm(
            &AutomationConfig::default(),
            &resume_rule(),
            &subject,
            GuardState::default(),
            NOW,
            0.0,
        );
        assert_eq!(decision.firing().unwrap().fire_at, NOW);
    }

    #[test]
    fn jitter_is_added_between_zero_and_the_configured_span() {
        let mut rule = resume_rule();
        rule.jitter = Some(DurationSpec::seconds(60));
        let config = AutomationConfig::default();
        let subject = capped_subject();
        let base = NOW + time::Duration::minutes(62);
        for (ratio, expected) in [(0.0, 0), (0.5, 30), (1.0, 60)] {
            let decision =
                Scheduler::arm(&config, &rule, &subject, GuardState::default(), NOW, ratio);
            assert_eq!(
                decision.firing().unwrap().fire_at,
                base + time::Duration::seconds(expected),
                "ratio {ratio}",
            );
        }
    }

    #[test]
    fn an_idle_rule_counts_from_when_the_row_went_idle() {
        let mut rule = AutomationRule::new(
            "nudge",
            AutomationEvent::IdleFor,
            AutomationAction::SendPrompt,
        );
        rule.for_ = Some(DurationSpec::seconds(600));
        rule.text = Some("status?".into());
        rule.jitter = Some(DurationSpec::seconds(0));
        let mut subject = capped_subject();
        subject.state = AgentState::Idle;
        subject.rate_limit_scope = None;
        subject.state_entered_at = NOW - time::Duration::minutes(4);
        let decision = Scheduler::arm(
            &AutomationConfig::default(),
            &rule,
            &subject,
            GuardState::default(),
            NOW,
            0.0,
        );
        assert_eq!(
            decision.firing().unwrap().fire_at,
            NOW + time::Duration::minutes(6)
        );
    }

    #[test]
    fn a_plain_error_is_not_a_cap_and_vice_versa() {
        let mut error_only = capped_subject();
        error_only.rate_limit_scope = None;
        assert_eq!(
            error_only.current_event(),
            Some(AutomationEvent::Error),
            "an Error row with no scope is a plain error",
        );
        assert_eq!(
            Scheduler::arm(
                &AutomationConfig::default(),
                &resume_rule(),
                &error_only,
                GuardState::default(),
                NOW,
                0.0,
            )
            .skip_reason(),
            Some(SkipReason::EventMismatch),
        );
        assert_eq!(
            capped_subject().current_event(),
            Some(AutomationEvent::RateLimited),
        );
    }

    // --- scheduler: filters ------------------------------------------------

    #[test]
    fn filters_all_have_to_match() {
        let config = AutomationConfig::default();
        let subject = capped_subject();
        let fires = |rule: &AutomationRule| {
            Scheduler::arm(&config, rule, &subject, GuardState::default(), NOW, 0.0)
                .firing()
                .is_some()
        };

        let mut rule = resume_rule();
        rule.agent = vec![AgentKindSpec(AgentKind::ClaudeCode)];
        rule.workspace = Some("callabo".into());
        rule.work = Some("^CAL-".into());
        rule.pane = Some("%42".into());
        rule.host = Some("tmux".into());
        rule.scope = vec![RateLimitScope::FiveHour];
        assert!(fires(&rule));

        for mutate in [
            (|r: &mut AutomationRule| r.agent = vec![AgentKindSpec(AgentKind::Codex)])
                as fn(&mut AutomationRule),
            |r: &mut AutomationRule| r.workspace = Some("other".into()),
            |r: &mut AutomationRule| r.work = Some("^JIRA-".into()),
            |r: &mut AutomationRule| r.pane = Some("%7".into()),
            |r: &mut AutomationRule| r.host = Some("herdr".into()),
            |r: &mut AutomationRule| r.scope = vec![RateLimitScope::SevenDay],
        ] {
            let mut narrowed = rule.clone();
            mutate(&mut narrowed);
            assert!(!fires(&narrowed), "a mismatched filter must stop the rule");
        }
    }

    #[test]
    fn host_local_matches_every_pane_this_daemon_governs() {
        let mut rule = resume_rule();
        rule.host = Some("local".into());
        let mut subject = capped_subject();
        subject.host = Some(HostKind::Herdr);
        subject.pane = Some("herdr:9".into());
        assert!(Scheduler::arm(
            &AutomationConfig::default(),
            &rule,
            &subject,
            GuardState::default(),
            NOW,
            0.0
        )
        .firing()
        .is_some());
    }

    #[test]
    fn a_paneless_row_is_skipped() {
        let mut subject = capped_subject();
        subject.pane = None;
        assert_eq!(
            Scheduler::arm(
                &AutomationConfig::default(),
                &resume_rule(),
                &subject,
                GuardState::default(),
                NOW,
                0.0
            )
            .skip_reason(),
            Some(SkipReason::NoPane),
        );
    }

    // --- scheduler: guards -------------------------------------------------

    #[test]
    fn the_master_switch_and_pause_stop_everything() {
        let subject = capped_subject();
        let off = AutomationConfig {
            enabled: false,
            ..AutomationConfig::default()
        };
        assert_eq!(
            Scheduler::arm(
                &off,
                &resume_rule(),
                &subject,
                GuardState::default(),
                NOW,
                0.0
            )
            .skip_reason(),
            Some(SkipReason::EngineDisabled),
        );
        let paused = AutomationConfig {
            paused_until: Some(NOW + time::Duration::hours(1)),
            ..AutomationConfig::default()
        };
        assert_eq!(
            Scheduler::arm(
                &paused,
                &resume_rule(),
                &subject,
                GuardState::default(),
                NOW,
                0.0
            )
            .skip_reason(),
            Some(SkipReason::Paused),
        );
        // The pause needs no lifting: once `now` reaches it, rules resume.
        assert!(paused.active_at(NOW + time::Duration::hours(2)));
    }

    #[test]
    fn a_disabled_rule_does_not_fire() {
        let mut rule = resume_rule();
        rule.enabled = false;
        assert_eq!(
            Scheduler::arm(
                &AutomationConfig::default(),
                &rule,
                &capped_subject(),
                GuardState::default(),
                NOW,
                0.0
            )
            .skip_reason(),
            Some(SkipReason::RuleDisabled),
        );
    }

    #[test]
    fn the_cooldown_and_hourly_cap_hold() {
        let rule = resume_rule(); // max_per_hour default 3, cooldown default 2m
        let config = AutomationConfig::default();
        let subject = capped_subject();

        let inside_cooldown = GuardState {
            last_fired_at: Some(NOW - time::Duration::seconds(30)),
            fired_last_hour: 1,
            ..GuardState::default()
        };
        assert_eq!(
            Scheduler::arm(&config, &rule, &subject, inside_cooldown, NOW, 0.0).skip_reason(),
            Some(SkipReason::Cooldown),
        );

        let past_cooldown = GuardState {
            last_fired_at: Some(NOW - time::Duration::minutes(5)),
            fired_last_hour: 1,
            ..GuardState::default()
        };
        assert!(
            Scheduler::arm(&config, &rule, &subject, past_cooldown, NOW, 0.0)
                .firing()
                .is_some()
        );

        let capped = GuardState {
            last_fired_at: Some(NOW - time::Duration::minutes(30)),
            fired_last_hour: 3,
            ..GuardState::default()
        };
        assert_eq!(
            Scheduler::arm(&config, &rule, &subject, capped, NOW, 0.0).skip_reason(),
            Some(SkipReason::HourlyCap),
        );
    }

    #[test]
    fn the_global_cap_stops_a_fan_out_no_rule_can_opt_out_of() {
        let mut rule = resume_rule();
        rule.max_per_hour = Some(60);
        let guards = GuardState {
            global_fired_last_hour: GLOBAL_MAX_PER_HOUR,
            ..GuardState::default()
        };
        assert_eq!(
            Scheduler::arm(
                &AutomationConfig::default(),
                &rule,
                &capped_subject(),
                guards,
                NOW,
                0.0
            )
            .skip_reason(),
            Some(SkipReason::GlobalCap),
        );
    }

    #[test]
    fn one_episode_gets_one_firing() {
        let guards = GuardState {
            episode_handled: true,
            ..GuardState::default()
        };
        assert_eq!(
            Scheduler::arm(
                &AutomationConfig::default(),
                &resume_rule(),
                &capped_subject(),
                guards,
                NOW,
                0.0
            )
            .skip_reason(),
            Some(SkipReason::EpisodeAlreadyHandled),
        );
    }

    #[test]
    fn the_episode_key_changes_only_when_the_state_does() {
        let first = capped_subject();
        // A soft cap upgrading to a hard one leaves the row in Error, so
        // `state_entered_at` — and the episode — does not move.
        let mut upgraded = first.clone();
        upgraded.rate_limit_source = Some(RateLimitSource::StopFailure);
        assert_eq!(first.episode(), upgraded.episode());

        let mut recovered_then_capped_again = first.clone();
        recovered_then_capped_again.state_entered_at = NOW + time::Duration::hours(6);
        assert_ne!(first.episode(), recovered_then_capped_again.episode());
    }

    // --- scheduler: the fire-time re-check ---------------------------------

    #[test]
    fn confirm_rejects_an_agent_that_recovered_while_waiting() {
        let config = AutomationConfig::default();
        let rule = resume_rule();
        let firing = Scheduler::arm(
            &config,
            &rule,
            &capped_subject(),
            GuardState::default(),
            NOW,
            0.0,
        )
        .firing()
        .unwrap()
        .clone();

        let later = NOW + time::Duration::hours(1);
        let mut recovered = capped_subject();
        recovered.state = AgentState::Idle;
        recovered.rate_limit_scope = None;
        recovered.state_entered_at = later;
        assert_eq!(
            Scheduler::confirm(
                &config,
                &rule,
                &firing,
                Some(&recovered),
                GuardState::default(),
                later
            )
            .skip_reason(),
            Some(SkipReason::ConditionCleared),
        );

        // Still capped, same episode → the firing stands.
        assert!(Scheduler::confirm(
            &config,
            &rule,
            &firing,
            Some(&capped_subject()),
            GuardState::default(),
            later
        )
        .firing()
        .is_some());
    }

    #[test]
    fn confirm_rejects_a_pane_that_went_away() {
        let config = AutomationConfig::default();
        let rule = resume_rule();
        let firing = Scheduler::arm(
            &config,
            &rule,
            &capped_subject(),
            GuardState::default(),
            NOW,
            0.0,
        )
        .firing()
        .unwrap()
        .clone();
        assert_eq!(
            Scheduler::confirm(&config, &rule, &firing, None, GuardState::default(), NOW)
                .skip_reason(),
            Some(SkipReason::PaneGone),
        );
        let mut moved = capped_subject();
        moved.pane = Some("%99".into());
        assert_eq!(
            Scheduler::confirm(
                &config,
                &rule,
                &firing,
                Some(&moved),
                GuardState::default(),
                NOW
            )
            .skip_reason(),
            Some(SkipReason::PaneGone),
        );
    }

    #[test]
    fn confirm_honours_a_pause_that_landed_while_waiting() {
        let rule = resume_rule();
        let firing = Scheduler::arm(
            &AutomationConfig::default(),
            &rule,
            &capped_subject(),
            GuardState::default(),
            NOW,
            0.0,
        )
        .firing()
        .unwrap()
        .clone();
        let paused = AutomationConfig {
            paused_until: Some(NOW + time::Duration::hours(4)),
            ..AutomationConfig::default()
        };
        assert_eq!(
            Scheduler::confirm(
                &paused,
                &rule,
                &firing,
                Some(&capped_subject()),
                GuardState::default(),
                NOW + time::Duration::hours(1)
            )
            .skip_reason(),
            Some(SkipReason::Paused),
        );
    }

    #[test]
    fn only_if_still_any_defeats_the_state_check_but_not_the_episode_check() {
        let config = AutomationConfig::default();
        let mut rule = resume_rule();
        rule.only_if_still = Some(AutomationCondition::Any);
        let firing = Scheduler::arm(
            &config,
            &rule,
            &capped_subject(),
            GuardState::default(),
            NOW,
            0.0,
        )
        .firing()
        .unwrap()
        .clone();
        let mut working = capped_subject();
        working.state = AgentState::Working;
        // Same episode timestamp, different state → `any` lets it through.
        working.state_entered_at = capped_subject().state_entered_at;
        working.rate_limit_scope = None;
        assert_eq!(
            Scheduler::confirm(
                &config,
                &rule,
                &firing,
                Some(&working),
                GuardState::default(),
                NOW
            )
            .skip_reason(),
            // The episode key embeds the state, so a state change is still
            // a different episode — `any` relaxes the predicate, not the
            // one-firing-per-episode guarantee.
            Some(SkipReason::ConditionCleared),
        );
    }

    // --- ordering ----------------------------------------------------------

    #[test]
    fn planned_firings_order_by_fire_time() {
        let base = Scheduler::arm(
            &AutomationConfig::default(),
            &resume_rule(),
            &capped_subject(),
            GuardState::default(),
            NOW,
            0.0,
        )
        .firing()
        .unwrap()
        .clone();
        let mut later = base.clone();
        later.fire_at = base.fire_at + time::Duration::minutes(5);
        let mut heap = std::collections::BinaryHeap::new();
        heap.push(std::cmp::Reverse(later.clone()));
        heap.push(std::cmp::Reverse(base.clone()));
        assert_eq!(heap.pop().unwrap().0.fire_at, base.fire_at);
        assert_eq!(heap.pop().unwrap().0.fire_at, later.fire_at);
    }

    // --- ledger ------------------------------------------------------------

    fn entry(
        rule: &str,
        pane: &str,
        at: OffsetDateTime,
        outcome: AutomationOutcome,
    ) -> AutomationLedgerEntry {
        AutomationLedgerEntry {
            rule: rule.into(),
            pane: pane.into(),
            agent: AgentKind::ClaudeCode,
            fired_at: at,
            action: AutomationAction::SendPrompt,
            outcome,
            detail: None,
            episode: Some("Error@ep".into()),
        }
    }

    #[tokio::test]
    async fn ledger_round_trips_and_trims() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("automation.json");
        let ledger = AutomationLedger::load(Some(path.clone()));
        for index in 0..(MAX_LEDGER_ENTRIES + 25) {
            ledger
                .append(entry(
                    "resume-after-limit",
                    "%42",
                    NOW + time::Duration::seconds(index.try_into().unwrap()),
                    AutomationOutcome::Fired,
                ))
                .await;
        }
        let reloaded = AutomationLedger::load(Some(path));
        let all = reloaded.all().await;
        assert_eq!(all.len(), MAX_LEDGER_ENTRIES);
        // The oldest are the ones dropped.
        assert_eq!(all[0].fired_at, NOW + time::Duration::seconds(25));
    }

    #[tokio::test]
    async fn guard_state_reads_cooldown_cap_and_episode_off_the_ledger() {
        let ledger = AutomationLedger::in_memory();
        ledger
            .append(entry(
                "resume-after-limit",
                "%42",
                NOW - time::Duration::minutes(90),
                AutomationOutcome::Fired,
            ))
            .await;
        ledger
            .append(entry(
                "resume-after-limit",
                "%42",
                NOW - time::Duration::minutes(30),
                AutomationOutcome::Fired,
            ))
            .await;
        ledger
            .append(entry(
                "resume-after-limit",
                "%7",
                NOW - time::Duration::minutes(10),
                AutomationOutcome::Fired,
            ))
            .await;
        // A skip consumes neither the budget nor the episode.
        ledger
            .append(entry(
                "resume-after-limit",
                "%42",
                NOW - time::Duration::minutes(1),
                AutomationOutcome::Skipped,
            ))
            .await;

        let guards = ledger
            .guard_state("resume-after-limit", "%42", "Error@ep", NOW)
            .await;
        assert_eq!(guards.fired_last_hour, 1, "the 90m-old firing has aged out");
        assert_eq!(guards.global_fired_last_hour, 2);
        assert_eq!(
            guards.last_fired_at,
            Some(NOW - time::Duration::minutes(30))
        );
        assert!(guards.episode_handled);

        let other_episode = ledger
            .guard_state("resume-after-limit", "%42", "Error@other", NOW)
            .await;
        assert!(!other_episode.episode_handled);
    }

    #[tokio::test]
    async fn a_failed_attempt_still_consumes_its_episode() {
        // Retrying would re-send keystrokes into a pane that may have taken
        // the first ones; a failure is a stop, not a reason to try again.
        let ledger = AutomationLedger::in_memory();
        ledger
            .append(entry(
                "resume-after-limit",
                "%42",
                NOW,
                AutomationOutcome::Failed,
            ))
            .await;
        let guards = ledger
            .guard_state("resume-after-limit", "%42", "Error@ep", NOW)
            .await;
        assert!(guards.episode_handled);
        assert_eq!(guards.fired_last_hour, 0);
    }

    // --- store -------------------------------------------------------------

    fn store_with_rule() -> (tempfile::TempDir, Arc<AutomationStore>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "# muxa config\n[ui]\nicons = \"unicode\"\n").unwrap();
        let store = AutomationStore::new(
            AutomationConfig::default(),
            Some(path),
            AutomationLedger::in_memory(),
        );
        (dir, store)
    }

    #[tokio::test]
    async fn upsert_writes_the_rule_and_replaces_it_in_place() {
        let (dir, store) = store_with_rule();
        let path = dir.path().join("config.toml");
        store.upsert_rule(resume_rule()).await.unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[[automation.rule]]"), "{text}");
        assert!(text.contains("# muxa config"), "unrelated bytes survive");

        let mut edited = resume_rule();
        edited.text = Some("keep going".into());
        store.upsert_rule(edited).await.unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text.matches("[[automation.rule]]").count(),
            1,
            "a same-name rule is replaced, not appended: {text}"
        );
        assert!(text.contains("keep going"), "{text}");

        // What was written is what the loader reads back.
        let config: Config = toml::from_str(&text).unwrap();
        assert_eq!(config.automation.rule.len(), 1);
        assert_eq!(
            config.automation.rule[0].text.as_deref(),
            Some("keep going")
        );
    }

    #[tokio::test]
    async fn upsert_refuses_an_invalid_rule_and_writes_nothing() {
        let (dir, store) = store_with_rule();
        let path = dir.path().join("config.toml");
        let before = std::fs::read_to_string(&path).unwrap();
        let mut bad = resume_rule();
        bad.text = None;
        assert!(store.upsert_rule(bad).await.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert!(store.rules().await.is_empty());
    }

    #[tokio::test]
    async fn remove_refuses_an_unknown_name() {
        let (_dir, store) = store_with_rule();
        let error = store.remove_rule("nope").await.unwrap_err();
        assert!(error.contains("no automation rule named"), "{error}");
    }

    #[tokio::test]
    async fn enable_disable_and_pause_change_the_live_copy_and_the_file() {
        let (dir, store) = store_with_rule();
        let path = dir.path().join("config.toml");
        store.upsert_rule(resume_rule()).await.unwrap();

        store
            .set_rule_enabled("resume-after-limit", false)
            .await
            .unwrap();
        assert!(!store.rules().await[0].enabled);
        let config: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(!config.automation.rule[0].enabled);

        let until = NOW + time::Duration::hours(1);
        store.set_paused_until(Some(until)).await.unwrap();
        assert_eq!(store.config().await.paused_until, Some(until));
        let config: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(config.automation.paused_until, Some(until));

        store.set_paused_until(None).await.unwrap();
        let config: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(config.automation.paused_until, None);
    }

    #[tokio::test]
    async fn remove_clears_the_section_when_the_last_rule_goes() {
        let (dir, store) = store_with_rule();
        let path = dir.path().join("config.toml");
        store.upsert_rule(resume_rule()).await.unwrap();
        store.remove_rule("resume-after-limit").await.unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("automation"), "{text}");
        assert!(store.rules().await.is_empty());
    }

    #[tokio::test]
    async fn views_carry_the_rendered_durations_and_recent_activity() {
        let (_dir, store) = store_with_rule();
        store.upsert_rule(resume_rule()).await.unwrap();
        store
            .ledger()
            .append(entry(
                "resume-after-limit",
                "%42",
                NOW - time::Duration::minutes(5),
                AutomationOutcome::Fired,
            ))
            .await;
        let views = store.views(NOW).await;
        assert!(views.enabled);
        let view = &views.rules[0];
        assert_eq!(view.wait, "reset+2m");
        assert_eq!(view.fallback, "20m");
        assert_eq!(view.cooldown, "2m");
        assert_eq!(view.max_per_hour, 3);
        assert_eq!(view.text.as_deref(), Some("continue"));
        assert!(view.submit);
        assert_eq!(view.fired_last_hour, 1);
        assert_eq!(view.filters, "any");
    }

    #[tokio::test]
    async fn test_rule_explains_each_candidate_without_firing() {
        let (_dir, store) = store_with_rule();
        store.upsert_rule(resume_rule()).await.unwrap();
        let mut idle = capped_subject();
        idle.agent_session_id = "sess-2".into();
        idle.pane = Some("%7".into());
        idle.state = AgentState::Idle;
        idle.rate_limit_scope = None;

        let report = store
            .test_rule("resume-after-limit", &[capped_subject(), idle], NOW)
            .await
            .unwrap();
        assert_eq!(report.candidates[0].decision, "fire");
        assert_eq!(
            report.candidates[0].fire_at,
            Some(NOW + time::Duration::minutes(62))
        );
        assert_eq!(report.candidates[1].decision, "event_mismatch");
        // Nothing was recorded: `test` is a dry run.
        assert!(store.ledger().all().await.is_empty());
    }

    #[tokio::test]
    async fn test_rule_refuses_an_unknown_name() {
        let (_dir, store) = store_with_rule();
        assert!(store.test_rule("nope", &[], NOW).await.is_err());
    }

    #[test]
    fn views_and_reports_serialize_flat() {
        let view = AutomationRuleView {
            name: "resume-after-limit".into(),
            on: AutomationEvent::RateLimited,
            enabled: true,
            action: AutomationAction::SendPrompt,
            agent: vec![AgentKindSpec(AgentKind::ClaudeCode)],
            workspace: None,
            work: Some("^CAL-".into()),
            pane: None,
            host: None,
            scope: vec![RateLimitScope::FiveHour],
            for_: None,
            text: Some("continue".into()),
            message: None,
            submit: true,
            wait: "reset+2m".into(),
            fallback: "20m".into(),
            jitter: "30s".into(),
            cooldown: "5m".into(),
            max_per_hour: 2,
            only_if_still: AutomationCondition::RateLimited,
            filters: "agent=claude_code work=^CAL-".into(),
            fired_last_hour: 0,
            last_fired_at: None,
        };
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["on"], "rate_limited");
        assert_eq!(json["action"], "send_prompt");
        assert_eq!(json["only_if_still"], "rate_limited");
        assert_eq!(json["agent"], serde_json::json!(["claude_code"]));
        assert_eq!(json["scope"], serde_json::json!(["five_hour"]));
        assert!(json["last_fired_at"].is_null());
        let back: AutomationRuleView = serde_json::from_value(json).unwrap();
        assert_eq!(back, view);
    }

    #[test]
    fn ledger_entries_serialize_flat() {
        let entry = entry("r", "%1", NOW, AutomationOutcome::Skipped);
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["outcome"], "skipped");
        assert_eq!(json["agent"], "claude_code");
        assert_eq!(json["fired_at"], "2026-09-03T12:00:00Z");
        let back: AutomationLedgerEntry = serde_json::from_value(json).unwrap();
        assert_eq!(back, entry);
    }
}
