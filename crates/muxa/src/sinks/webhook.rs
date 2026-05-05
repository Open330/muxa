//! Slack/Discord webhook sink — push state-transition alerts off-host.
//!
//! Subscribes to the daemon's `Transition` broadcast and POSTs a one-line
//! message to a Slack-incoming-webhook or Discord-webhook URL whenever an
//! agent transitions into a state the operator wants to be paged on
//! (defaults: `WaitingInput`, `Error`). Designed for the AFK case — push
//! a Slack message to your phone the moment Claude Code stops to ask for
//! permission, so you can either grant it from the couch or come back to
//! the desk knowing something needs you.
//!
//! ## Why this is intentionally lightweight
//!
//! - No queue, no retry backoff, no on-disk spool. A dropped notification
//!   matters less than a stalled sink task: Slack/Discord both have their
//!   own retry semantics on the receiving end, and we'd rather lose one
//!   alert than buffer 1000 of them while Slack is down and then page the
//!   operator with a thundering herd when service comes back.
//! - In-task per-agent rate limiter (`HashMap<(kind, session, state), Instant>`).
//!   `WaitingInput` ↔ `Working` flap-flap is common during permission
//!   loops; we don't want one flaky agent to send 30 push notifications a
//!   minute. One message per `(kind, session_id, state)` per
//!   `rate_limit_secs` window is plenty.
//! - Filter by `to`-state up front (`on_states`). Most transitions are
//!   routine `Idle ↔ Working` and would spam.

use crate::config::WebhookToml;
use crate::event::{AgentKind, AgentState};
use crate::state::Transition;
use serde::Serialize;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

/// Default per-agent rate-limit window. One alert per `(kind, session,
/// state)` tuple in this many seconds. Picked at "pageable, not spammy"
/// — long enough to absorb a flapping `Working`/`WaitingInput` loop,
/// short enough that a real second incident an hour later still pages.
pub const DEFAULT_RATE_LIMIT_SECS: u64 = 60;

/// HTTP timeout per POST. Short — Slack/Discord webhooks usually return
/// in well under a second, and a stalled request blocks no other work
/// (each notification fires from the sink loop and we drop on failure)
/// but slow timeouts add up if a host is offline.
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Cap on the message body. Slack truncates around 4 KB; Discord around
/// 2 KB. We stay well under both with the prompt clipped to 100 chars
/// — the goal is "is this worth looking at", not full transcript review.
const PROMPT_TRUNCATE_LEN: usize = 100;

#[derive(Debug, thiserror::Error)]
pub enum WebhookError {
    #[error(
        "webhook sink is enabled but neither [sinks.webhook].endpoint nor \
         [sinks.webhook].endpoint_env is set (one of the two is required)"
    )]
    MissingEndpoint,

    #[error(
        "webhook sink is enabled but env var {var} is empty/unset \
         (set the full webhook URL there, or use the `endpoint` field)"
    )]
    MissingEndpointEnv { var: String },

    #[error("invalid webhook URL {url:?}: {source}")]
    InvalidEndpoint {
        url: String,
        #[source]
        source: url::ParseError,
    },

    #[error("reqwest client init failed: {0}")]
    ReqwestInit(#[source] reqwest::Error),
}

/// Wire-format flavor for the outgoing POST body.
///
/// Slack and Discord both accept a "minimum viable" JSON shape with one
/// string field. We auto-detect from the URL host, with an explicit
/// override for the rare case (`generic`) where the operator points at a
/// custom HTTP receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebhookFlavor {
    Slack,
    Discord,
    /// Posts the full `Transition` JSON. Useful for n8n/Zapier-style
    /// receivers that want structured data, and for our own integration
    /// tests.
    Generic,
}

impl WebhookFlavor {
    /// Parse a config string. Empty / unknown values fall through to
    /// `None` so the caller can apply URL-based auto-detection.
    fn from_str_opt(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "slack" => Some(Self::Slack),
            "discord" => Some(Self::Discord),
            "generic" => Some(Self::Generic),
            _ => None,
        }
    }
}

/// Pick a flavor by inspecting the URL host. We use simple substring
/// matching rather than parsing the URL because Slack/Discord both
/// publish stable, well-known hostnames and a substring check sidesteps
/// quirks like `slack.com` vs `hooks.slack.com` vs proxied URLs.
pub fn infer_flavor(url: &str) -> WebhookFlavor {
    let lower = url.to_ascii_lowercase();
    if lower.contains("hooks.slack.com") {
        WebhookFlavor::Slack
    } else if lower.contains("discord.com/api/webhooks")
        || lower.contains("discordapp.com/api/webhooks")
    {
        WebhookFlavor::Discord
    } else {
        WebhookFlavor::Generic
    }
}

/// Resolved webhook sink — runtime state derived from the TOML config.
#[derive(Debug)]
pub struct WebhookSink {
    endpoint: String,
    flavor: WebhookFlavor,
    on_states: Vec<AgentState>,
    rate_limit: Duration,
    client: reqwest::Client,
}

impl WebhookSink {
    /// Resolve the TOML sub-table into a runtime sink, or `Ok(None)` if
    /// disabled. Errors when `enabled = true` but the URL cannot be
    /// determined or is malformed — better to fail loudly at startup
    /// than silently no-op while the operator wonders why no alerts
    /// arrive.
    pub fn resolve(toml: &WebhookToml) -> Result<Option<Self>, WebhookError> {
        if !toml.enabled.unwrap_or(false) {
            return Ok(None);
        }

        // `endpoint_env` wins when both are set. The expected workflow is
        // "TOML carries the harmless fields, env var carries the
        // secret-bearing URL", and an env override is the natural way to
        // shadow a default URL committed to a shared dotfile.
        let endpoint = if let Some(var) = toml.endpoint_env.as_deref().filter(|s| !s.is_empty()) {
            std::env::var(var)
                .ok()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| WebhookError::MissingEndpointEnv {
                    var: var.to_string(),
                })?
        } else {
            toml.endpoint
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or(WebhookError::MissingEndpoint)?
                .to_string()
        };

        // Validate the URL up front so a typo surfaces at startup rather
        // than on the first transition.
        url::Url::parse(&endpoint).map_err(|source| WebhookError::InvalidEndpoint {
            url: endpoint.clone(),
            source,
        })?;

        let flavor = toml
            .flavor
            .as_deref()
            .and_then(WebhookFlavor::from_str_opt)
            .unwrap_or_else(|| infer_flavor(&endpoint));

        let on_states = if let Some(raw) = toml.on_states.as_ref() {
            raw.iter().filter_map(|s| parse_state(s)).collect()
        } else {
            default_on_states()
        };

        let rate_limit =
            Duration::from_secs(toml.rate_limit_secs.unwrap_or(DEFAULT_RATE_LIMIT_SECS));

        let client = reqwest::Client::builder()
            .timeout(HTTP_REQUEST_TIMEOUT)
            .build()
            .map_err(WebhookError::ReqwestInit)?;

        Ok(Some(Self {
            endpoint,
            flavor,
            on_states,
            rate_limit,
            client,
        }))
    }

    /// Run the sink until the shutdown channel fires or the broadcast
    /// closes. Mirrors the ohmyprompt sink's shape: one task, no
    /// background workers, abort-on-shutdown. The rate limiter lives
    /// on the stack so there's no shared state with the rest of the
    /// daemon.
    pub async fn run(
        self,
        mut transition_rx: broadcast::Receiver<Transition>,
        mut shutdown_rx: broadcast::Receiver<()>,
    ) -> Result<(), WebhookError> {
        // Per-(kind, session_id, state) "last sent" timestamps. Bounded
        // implicitly by the pane lifecycle — entries for stopped agents
        // simply stop being looked up. We don't bother with explicit
        // GC: the map is small and the daemon process restarts on
        // upgrade.
        let mut last_sent: HashMap<(AgentKind, String, AgentState), Instant> = HashMap::new();

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    tracing::debug!("webhook sink received shutdown");
                    return Ok(());
                }
                next = transition_rx.recv() => {
                    match next {
                        Ok(transition) => {
                            self.handle_transition(&transition, &mut last_sent).await;
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            // Lagged transitions can only mean we missed
                            // pages we'd otherwise have sent — log and
                            // keep going. The alternative (resyncing
                            // state) doesn't help: by the time we
                            // notice, the agent is somewhere else.
                            tracing::warn!(
                                missed = n,
                                "webhook sink lagged behind transition stream; continuing"
                            );
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            tracing::debug!("transition broadcast closed; webhook sink exiting");
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    /// Decide whether to forward, then POST. Errors are logged at WARN
    /// and dropped — the run loop must never exit because of a 500.
    async fn handle_transition(
        &self,
        transition: &Transition,
        last_sent: &mut HashMap<(AgentKind, String, AgentState), Instant>,
    ) {
        if !should_forward(transition, &self.on_states) {
            return;
        }

        let key = (
            transition.agent.kind,
            transition.agent.session_id.clone(),
            transition.to,
        );
        let now = Instant::now();
        if let Some(prev) = last_sent.get(&key) {
            if now.duration_since(*prev) < self.rate_limit {
                tracing::debug!(
                    kind = %transition.agent.kind,
                    session = %transition.agent.session_id,
                    state = %transition.to,
                    "webhook: suppressed by rate limit"
                );
                return;
            }
        }
        last_sent.insert(key, now);

        let message = format_message(transition);
        let body = build_payload(self.flavor, &message, transition);

        let result = self
            .client
            .post(&self.endpoint)
            .json(&body)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status);

        match result {
            Ok(_) => {
                tracing::debug!(
                    kind = %transition.agent.kind,
                    session = %transition.agent.session_id,
                    state = %transition.to,
                    "webhook: notification posted"
                );
            }
            Err(e) => {
                // Best-effort: drop on failure. See module docs for why
                // we don't queue.
                tracing::warn!(
                    error = %e,
                    kind = %transition.agent.kind,
                    state = %transition.to,
                    "webhook: post failed; dropping notification"
                );
            }
        }
    }
}

/// Public spawn helper mirroring `sinks::ohmyprompt::spawn` (added in
/// the daemon main wire-up). Returns the `JoinHandle` so the daemon can
/// hold/await it during shutdown.
pub fn spawn(
    sink: WebhookSink,
    transition_rx: broadcast::Receiver<Transition>,
    shutdown_rx: broadcast::Receiver<()>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = sink.run(transition_rx, shutdown_rx).await {
            tracing::error!(error = %e, "webhook sink exited");
        }
    })
}

/// Parse a `[sinks.webhook].on_states` entry. Accepts both
/// `PascalCase` (`"WaitingInput"`) and `snake_case` (`"waiting_input"`)
/// forms because TOML readers tend to type either; `AgentState`'s serde
/// is `snake_case` and its strum Display matches, but the brief calls
/// out the `PascalCase` form so we accept both.
fn parse_state(s: &str) -> Option<AgentState> {
    match s {
        "Starting" | "starting" => Some(AgentState::Starting),
        "Working" | "working" => Some(AgentState::Working),
        "Idle" | "idle" => Some(AgentState::Idle),
        "WaitingInput" | "waiting_input" => Some(AgentState::WaitingInput),
        "Error" | "error" => Some(AgentState::Error),
        "Stopped" | "stopped" => Some(AgentState::Stopped),
        _ => {
            tracing::warn!(
                value = %s,
                "webhook: unknown on_states entry; ignoring"
            );
            None
        }
    }
}

fn default_on_states() -> Vec<AgentState> {
    vec![AgentState::WaitingInput, AgentState::Error]
}

/// Filter predicate. Pulled out as a free function so the unit tests can
/// drive it without instantiating a sink (which would need an HTTP
/// client).
fn should_forward(transition: &Transition, on_states: &[AgentState]) -> bool {
    on_states.contains(&transition.to)
}

/// Build the human-readable one-line message body.
///
/// Format: `{from_glyph} → {to_glyph} {kind} [{pane}] {tag} — "{snippet}"`
///
/// `kind` uses strum's Display impl so a kind change at the protocol
/// layer ripples here automatically. `pane` falls back to `-` when the
/// agent has no tmux pane bound (paneless agents). `snippet` prefers
/// `last_prompt` (what the user just asked) but falls back to
/// `last_notification` (the agent's own message) so an Error transition
/// without a fresh prompt still carries the failure tag.
fn format_message(transition: &Transition) -> String {
    let agent = &transition.agent;
    let pane = agent.pane.as_deref().unwrap_or("-");
    let from_glyph = state_glyph(transition.from);
    let to_glyph = state_glyph(transition.to);
    let tag = match transition.to {
        AgentState::WaitingInput => "needs input",
        AgentState::Error => "error",
        AgentState::Working => "working",
        AgentState::Idle => "idle",
        AgentState::Starting => "starting",
        AgentState::Stopped => "stopped",
    };

    let snippet = agent
        .last_prompt
        .as_deref()
        .or(agent.last_notification.as_deref())
        .map(truncate_snippet)
        .unwrap_or_default();

    if snippet.is_empty() {
        format!(
            "{from_glyph} → {to_glyph} {kind} [{pane}] {tag}",
            kind = agent.kind,
        )
    } else {
        format!(
            "{from_glyph} → {to_glyph} {kind} [{pane}] {tag} — \"{snippet}\"",
            kind = agent.kind,
        )
    }
}

/// Map a state to a glyph for the message header. Mirrors the watch UI's
/// glyphs so the Slack message visually matches what an operator sees on
/// their terminal — recognition memory is faster than reading.
fn state_glyph(state: AgentState) -> &'static str {
    match state {
        AgentState::Starting => "…",
        AgentState::Working => "⚙",
        AgentState::Idle => "·",
        AgentState::WaitingInput => "!",
        AgentState::Error => "✗",
        AgentState::Stopped => "■",
    }
}

/// Truncate at char (not byte) boundaries — `last_prompt` is user input
/// and likely contains UTF-8. Append a trailing `…` to make truncation
/// obvious in the alert.
fn truncate_snippet(s: &str) -> String {
    let trimmed: String = s.chars().take(PROMPT_TRUNCATE_LEN).collect();
    if trimmed.chars().count() < s.chars().count() {
        format!("{trimmed}…")
    } else {
        trimmed
    }
}

/// Slack-flavored payload: `{ "text": "..." }`.
#[derive(Debug, Serialize)]
struct SlackPayload<'a> {
    text: &'a str,
}

/// Discord-flavored payload: `{ "content": "..." }`.
#[derive(Debug, Serialize)]
struct DiscordPayload<'a> {
    content: &'a str,
}

/// Build a `serde_json::Value` for the chosen flavor. We materialize
/// to `Value` (rather than returning a `Box<dyn Serialize>`) so the
/// tests can assert the wire shape without re-parsing JSON, and so
/// `reqwest`'s `.json(&body)` path stays a single monomorphized call.
fn build_payload(
    flavor: WebhookFlavor,
    message: &str,
    transition: &Transition,
) -> serde_json::Value {
    match flavor {
        WebhookFlavor::Slack => serde_json::to_value(SlackPayload { text: message })
            .unwrap_or_else(|_| serde_json::json!({ "text": message })),
        WebhookFlavor::Discord => serde_json::to_value(DiscordPayload { content: message })
            .unwrap_or_else(|_| serde_json::json!({ "content": message })),
        WebhookFlavor::Generic => {
            // The full `Transition` is `Serialize` — the receiver gets
            // the structured payload. If serialization fails (it
            // shouldn't, every field is plain data), fall back to the
            // message string so we still send *something*.
            serde_json::to_value(transition)
                .unwrap_or_else(|_| serde_json::json!({ "text": message }))
        }
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::event::AgentKind;
    use crate::state::Agent;
    use time::macros::datetime;

    fn agent(kind: AgentKind, pane: Option<&str>, prompt: Option<&str>) -> Agent {
        Agent {
            kind,
            session_id: "sess-1".into(),
            pane: pane.map(str::to_string),
            cwd: None,
            state: AgentState::WaitingInput,
            last_prompt: prompt.map(str::to_string),
            last_response: None,
            last_notification: None,
            model: None,
            context_used_pct: None,
            cost_usd: None,
            rate_limit_5h_pct: None,
            rate_limit_5h_resets_at: None,
            rate_limit_7d_pct: None,
            rate_limit_7d_resets_at: None,
            rate_limited_until: None,
            rate_limit_scope: None,
            rate_limit_source: None,
            started_at: datetime!(2026-04-30 12:00:00 UTC),
            last_activity_at: datetime!(2026-04-30 12:00:00 UTC),
        }
    }

    fn transition(from: AgentState, to: AgentState, agent: Agent) -> Transition {
        Transition { from, to, agent }
    }

    #[test]
    fn format_message_for_waiting_input() {
        let mut a = agent(AgentKind::ClaudeCode, Some("main:2"), Some("do all"));
        a.state = AgentState::WaitingInput;
        let t = transition(AgentState::Working, AgentState::WaitingInput, a);
        assert_eq!(
            format_message(&t),
            "⚙ → ! claude_code [main:2] needs input — \"do all\""
        );
    }

    #[test]
    fn format_message_for_error() {
        let mut a = agent(AgentKind::Codex, Some("work:1"), Some("rate_limit"));
        a.state = AgentState::Error;
        let t = transition(AgentState::Working, AgentState::Error, a);
        assert_eq!(
            format_message(&t),
            "⚙ → ✗ codex [work:1] error — \"rate_limit\""
        );
    }

    #[test]
    fn format_message_falls_back_to_dash_for_paneless_agent() {
        let a = agent(AgentKind::ClaudeCode, None, Some("hi"));
        let t = transition(AgentState::Working, AgentState::WaitingInput, a);
        let msg = format_message(&t);
        assert!(msg.contains("[-]"), "got {msg:?}");
    }

    #[test]
    fn format_message_uses_last_notification_when_no_prompt() {
        let mut a = agent(AgentKind::ClaudeCode, Some("p:0"), None);
        a.last_notification = Some("permission requested".into());
        let t = transition(AgentState::Working, AgentState::WaitingInput, a);
        let msg = format_message(&t);
        assert!(msg.contains("permission requested"), "got {msg:?}");
    }

    #[test]
    fn format_message_truncates_long_prompts() {
        let long = "x".repeat(300);
        let a = agent(AgentKind::ClaudeCode, Some("p:0"), Some(&long));
        let t = transition(AgentState::Working, AgentState::WaitingInput, a);
        let msg = format_message(&t);
        // 100 truncated chars + ellipsis. Easier to assert the ellipsis
        // is present than to count UTF-8 bytes back out of the message.
        assert!(msg.contains('…'), "expected truncation marker in {msg:?}");
        assert!(msg.len() < long.len() + 50);
    }

    #[test]
    fn should_forward_filters_idle_transitions() {
        let a = agent(AgentKind::ClaudeCode, Some("p:0"), None);
        let t = transition(AgentState::Working, AgentState::Idle, a);
        let on = default_on_states();
        assert!(!should_forward(&t, &on));
    }

    #[test]
    fn should_forward_passes_waiting_input_and_error() {
        let a = agent(AgentKind::ClaudeCode, Some("p:0"), None);
        let on = default_on_states();
        let t = transition(AgentState::Working, AgentState::WaitingInput, a.clone());
        assert!(should_forward(&t, &on));
        let t = transition(AgentState::Working, AgentState::Error, a);
        assert!(should_forward(&t, &on));
    }

    /// The rate-limiter logic is the same shape as `handle_transition`
    /// uses internally, but we exercise it without `tokio::time::sleep`
    /// — manually subtracting from the stored Instant lets us run the
    /// "60 seconds later" branch in microseconds.
    fn check_and_record(
        last_sent: &mut HashMap<(AgentKind, String, AgentState), Instant>,
        key: (AgentKind, String, AgentState),
        now: Instant,
        window: Duration,
    ) -> bool {
        if let Some(prev) = last_sent.get(&key) {
            if now.duration_since(*prev) < window {
                return false;
            }
        }
        last_sent.insert(key, now);
        true
    }

    #[test]
    fn rate_limiter_suppresses_within_window() {
        let mut last = HashMap::new();
        let key = (
            AgentKind::ClaudeCode,
            "sess-1".to_string(),
            AgentState::WaitingInput,
        );
        let t0 = Instant::now();
        let window = Duration::from_secs(60);
        assert!(check_and_record(&mut last, key.clone(), t0, window));
        // Half a second later — well inside the window.
        let t1 = t0 + Duration::from_millis(500);
        assert!(!check_and_record(&mut last, key, t1, window));
    }

    #[test]
    fn rate_limiter_releases_after_window() {
        let mut last = HashMap::new();
        let key = (
            AgentKind::ClaudeCode,
            "sess-1".to_string(),
            AgentState::WaitingInput,
        );
        let window = Duration::from_secs(60);
        let t0 = Instant::now();
        assert!(check_and_record(&mut last, key.clone(), t0, window));
        // 61 seconds later — outside the window.
        let t1 = t0 + Duration::from_secs(61);
        assert!(check_and_record(&mut last, key, t1, window));
    }

    #[test]
    fn rate_limiter_keys_per_agent_and_state() {
        let mut last = HashMap::new();
        let window = Duration::from_secs(60);
        let now = Instant::now();
        let claude_waiting = (
            AgentKind::ClaudeCode,
            "sess-1".to_string(),
            AgentState::WaitingInput,
        );
        let codex_waiting = (
            AgentKind::Codex,
            "sess-1".to_string(),
            AgentState::WaitingInput,
        );
        let claude_error = (
            AgentKind::ClaudeCode,
            "sess-1".to_string(),
            AgentState::Error,
        );

        assert!(check_and_record(&mut last, claude_waiting, now, window));
        // Different kind / same session must NOT be suppressed.
        assert!(check_and_record(&mut last, codex_waiting, now, window));
        // Same kind / different state must NOT be suppressed.
        assert!(check_and_record(&mut last, claude_error, now, window));
    }

    #[test]
    fn flavor_inference_from_url() {
        assert_eq!(
            infer_flavor("https://hooks.slack.com/services/T0/B0/abc"),
            WebhookFlavor::Slack
        );
        assert_eq!(
            infer_flavor("https://discord.com/api/webhooks/123/abc"),
            WebhookFlavor::Discord
        );
        assert_eq!(
            infer_flavor("https://discordapp.com/api/webhooks/123/abc"),
            WebhookFlavor::Discord
        );
        assert_eq!(
            infer_flavor("https://example.com/hook"),
            WebhookFlavor::Generic
        );
    }

    #[test]
    fn payload_for_slack_uses_text_field() {
        let a = agent(AgentKind::ClaudeCode, Some("p:0"), Some("hi"));
        let t = transition(AgentState::Working, AgentState::WaitingInput, a);
        let v = build_payload(WebhookFlavor::Slack, "msg", &t);
        let obj = v.as_object().expect("object payload");
        assert!(obj.contains_key("text"));
        assert_eq!(obj.get("text").and_then(|v| v.as_str()), Some("msg"));
        assert!(!obj.contains_key("content"));
    }

    #[test]
    fn payload_for_discord_uses_content_field() {
        let a = agent(AgentKind::ClaudeCode, Some("p:0"), Some("hi"));
        let t = transition(AgentState::Working, AgentState::WaitingInput, a);
        let v = build_payload(WebhookFlavor::Discord, "msg", &t);
        let obj = v.as_object().expect("object payload");
        assert!(obj.contains_key("content"));
        assert_eq!(obj.get("content").and_then(|v| v.as_str()), Some("msg"));
        assert!(!obj.contains_key("text"));
    }

    #[test]
    fn payload_for_generic_serializes_full_transition() {
        let a = agent(AgentKind::ClaudeCode, Some("p:0"), Some("hi"));
        let t = transition(AgentState::Working, AgentState::WaitingInput, a);
        let v = build_payload(WebhookFlavor::Generic, "msg", &t);
        let obj = v.as_object().expect("object payload");
        // Generic flavor passes the full Transition through; assert the
        // fields are present so a future change to `Transition` (rename
        // / drop) breaks this test.
        assert!(obj.contains_key("from"));
        assert!(obj.contains_key("to"));
        assert!(obj.contains_key("agent"));
    }

    #[test]
    fn parse_state_accepts_both_cases() {
        assert_eq!(parse_state("WaitingInput"), Some(AgentState::WaitingInput));
        assert_eq!(parse_state("waiting_input"), Some(AgentState::WaitingInput));
        assert_eq!(parse_state("Error"), Some(AgentState::Error));
        assert_eq!(parse_state("error"), Some(AgentState::Error));
        assert_eq!(parse_state("nonsense"), None);
    }

    #[test]
    fn resolve_returns_none_when_disabled() {
        let toml = WebhookToml::default();
        let out = WebhookSink::resolve(&toml).unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn resolve_errors_when_enabled_without_endpoint_or_env() {
        let toml = WebhookToml {
            enabled: Some(true),
            ..WebhookToml::default()
        };
        let err = WebhookSink::resolve(&toml).unwrap_err();
        assert!(matches!(err, WebhookError::MissingEndpoint));
    }

    #[test]
    fn resolve_errors_when_endpoint_env_unset() {
        let var = "WEBHOOK_TEST_MISSING_URL_ENV";
        std::env::remove_var(var);
        let toml = WebhookToml {
            enabled: Some(true),
            endpoint_env: Some(var.into()),
            ..WebhookToml::default()
        };
        let err = WebhookSink::resolve(&toml).unwrap_err();
        assert!(matches!(err, WebhookError::MissingEndpointEnv { .. }));
    }

    #[test]
    fn resolve_prefers_endpoint_env_over_endpoint() {
        // Set both to distinguishable values; endpoint_env should win.
        let var = "WEBHOOK_TEST_PREFER_ENV_URL";
        std::env::set_var(var, "https://hooks.slack.com/services/from/env");
        let toml = WebhookToml {
            enabled: Some(true),
            endpoint: Some("https://example.com/from-toml".into()),
            endpoint_env: Some(var.into()),
            ..WebhookToml::default()
        };
        let sink = WebhookSink::resolve(&toml).unwrap().expect("resolved");
        assert_eq!(sink.endpoint, "https://hooks.slack.com/services/from/env");
        assert_eq!(sink.flavor, WebhookFlavor::Slack);
        std::env::remove_var(var);
    }

    #[test]
    fn resolve_uses_explicit_flavor_over_inference() {
        let toml = WebhookToml {
            enabled: Some(true),
            endpoint: Some("https://example.com/hook".into()),
            flavor: Some("slack".into()),
            ..WebhookToml::default()
        };
        let sink = WebhookSink::resolve(&toml).unwrap().expect("resolved");
        assert_eq!(sink.flavor, WebhookFlavor::Slack);
    }

    #[test]
    fn resolve_default_on_states_are_waiting_input_and_error() {
        let toml = WebhookToml {
            enabled: Some(true),
            endpoint: Some("https://example.com/hook".into()),
            ..WebhookToml::default()
        };
        let sink = WebhookSink::resolve(&toml).unwrap().expect("resolved");
        assert_eq!(sink.on_states, default_on_states());
    }

    #[test]
    fn resolve_rejects_invalid_endpoint() {
        let toml = WebhookToml {
            enabled: Some(true),
            endpoint: Some("not-a-url".into()),
            ..WebhookToml::default()
        };
        let err = WebhookSink::resolve(&toml).unwrap_err();
        assert!(matches!(err, WebhookError::InvalidEndpoint { .. }));
    }
}
