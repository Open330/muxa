//! Desktop notifications on attention-worthy state transitions.
//!
//! The daemon subscribes to `Store::subscribe()` and, when the user has
//! opted in via `[notifier] enabled = true, backend = "libnotify"`, posts a
//! desktop notification for transitions that need the user's attention:
//!
//! - `* -> WaitingInput` — agent is blocked on the human
//! - `* -> Error`        — agent hit a terminal failure
//! - `Working -> Stopped` — session completed mid-turn (rare but useful)
//!
//! Delivery is best-effort. If the notification backend is unavailable
//! (e.g., headless CI, `DBus` not running), each failed post is logged at
//! `warn` and the task keeps running. Backpressure from the broadcast
//! channel is handled by `recv`'s `Lagged` variant — we log and continue
//! from the new cursor.
//!
//! The transport is `notify-rust`, which on Linux/BSD talks `DBus` via
//! `org.freedesktop.Notifications` (libnotify-compatible), on macOS uses
//! `NSUserNotification`, and on Windows uses `WinRT` toasts. No platform
//! gating is needed at this layer.

use crate::state::Transition;
use crate::{AgentKind, AgentState};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

/// Max body length we hand to the notification backend. Longer prompts are
/// truncated with an ellipsis. Most desktop environments clip past ~100
/// chars anyway, and libnotify treats the body as markup on some backends.
const BODY_TRUNCATE: usize = 80;

/// Per-agent re-notify debounce window. A flapping agent that bounces in
/// and out of `WaitingInput` (or an error that re-fires as the agent
/// retries) would otherwise spam a desktop notification on every
/// transition. Within this window we suppress a *repeat of the same
/// state* for the same agent; a genuine state change (e.g. `WaitingInput`
/// → `Error`) always gets through immediately.
const RENOTIFY_WINDOW: Duration = Duration::from_secs(30);

/// Identifies one agent across transitions for debounce bookkeeping.
type NotifyKey = (AgentKind, String);

#[derive(Debug, thiserror::Error)]
pub enum NotifyError {
    /// The broadcast sender was dropped — the store is gone and so is the
    /// daemon; the task should exit cleanly.
    #[error("store broadcast channel closed")]
    Closed,
}

/// Desktop-notifier task.
///
/// Construct with `Notifier::new()` and spawn its `run` future on the
/// tokio runtime. It owns no shared state; one receiver is enough.
#[derive(Debug, Default)]
pub struct Notifier {
    /// App name shown in the notification (GNOME, KDE, macOS all display
    /// this). Kept short and stable so multiple muxa notifications group.
    app_name: String,
}

impl Notifier {
    pub fn new() -> Self {
        Self {
            app_name: "muxa".into(),
        }
    }

    /// Subscribe-and-dispatch loop. Terminates when the broadcast channel
    /// is closed (i.e., the `Store` is dropped, which only happens at
    /// daemon shutdown).
    pub async fn run(self, mut rx: broadcast::Receiver<Transition>) -> Result<(), NotifyError> {
        // Last (target state, fire instant) we posted per agent, so a
        // flapping agent can't spam the desktop. Bounded by the number of
        // live agents; entries are simply overwritten, never leaked to
        // unbounded growth in practice (one per (kind, session_id)).
        let mut last_fired: HashMap<NotifyKey, (AgentState, Instant)> = HashMap::new();
        loop {
            match rx.recv().await {
                Ok(t) => {
                    if !should_notify(&t) {
                        continue;
                    }
                    let key = (t.agent.kind, t.agent.session_id.clone());
                    let now = Instant::now();
                    let last = last_fired
                        .get(&key)
                        .map(|(state, at)| (*state, now.saturating_duration_since(*at)));
                    if !should_fire(last, t.to, RENOTIFY_WINDOW) {
                        continue;
                    }
                    last_fired.insert(key, (t.to, now));

                    let (title, body) = render(&self.app_name, &t);
                    // Errors are the one state a user must not miss — mark
                    // them Critical so backends that honor urgency (XDG)
                    // keep them on screen until dismissed.
                    let critical = t.to == AgentState::Error;
                    if let Err(e) = post(&self.app_name, &title, &body, critical) {
                        tracing::warn!(error = %e, "desktop notification post failed");
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        missed = n,
                        "notifier lagged behind state transitions; continuing"
                    );
                }
                Err(broadcast::error::RecvError::Closed) => return Err(NotifyError::Closed),
            }
        }
    }
}

/// Predicate: does this transition warrant a desktop pop?
fn should_notify(t: &Transition) -> bool {
    matches!(
        (t.from, t.to),
        (
            _,
            AgentState::WaitingInput | AgentState::WaitingChoice | AgentState::Error
        ) | (AgentState::Working, AgentState::Stopped)
    )
}

/// Debounce decision. `last` is `Some((previously-notified target state,
/// elapsed since that notification))` for this agent, or `None` if we've
/// never notified it. Fire when it's a new agent, when the target state
/// differs from what we last posted, or when the re-notify window has
/// elapsed for a repeat of the same state.
fn should_fire(last: Option<(AgentState, Duration)>, to: AgentState, window: Duration) -> bool {
    match last {
        Some((prev_state, elapsed)) => prev_state != to || elapsed >= window,
        None => true,
    }
}

/// Human-readable label for the states we notify on — the machine
/// `snake_case` `Display` (`waiting_input`) reads like a log line, not a
/// message a person wants popped on their desktop.
fn humanize_state(state: AgentState) -> &'static str {
    match state {
        AgentState::WaitingInput => "needs input",
        AgentState::WaitingChoice => "needs choice",
        AgentState::Error => "error",
        AgentState::Stopped => "stopped",
        // Only the four above ever reach `render` (see `should_notify`);
        // keep a sane fallback in case that filter widens later.
        AgentState::Working => "working",
        AgentState::Idle => "idle",
        AgentState::Starting => "starting",
    }
}

/// Build (title, body) strings for a given transition.
fn render(_app: &str, t: &Transition) -> (String, String) {
    let title = format!("muxa · {} · {}", t.agent.kind, humanize_state(t.to));

    let pane = t.agent.pane.as_deref().unwrap_or("-");
    // Prefer the notification message (it's the one the agent explicitly
    // pushed), fall back to last prompt for context.
    let detail = t
        .agent
        .last_notification
        .as_deref()
        .or(t.agent.last_prompt.as_deref())
        .unwrap_or("");
    let detail = truncate(detail, BODY_TRUNCATE);
    let body = if detail.is_empty() {
        format!("pane {pane}")
    } else {
        format!("pane {pane}: {detail}")
    };
    (title, body)
}

/// Post a notification. Factored out so tests can stay hermetic — this is
/// the only function that actually touches `notify-rust`. `critical`
/// escalates urgency on backends that support it (see
/// [`set_critical_urgency`]).
fn post(
    app_name: &str,
    title: &str,
    body: &str,
    critical: bool,
) -> Result<(), notify_rust::error::Error> {
    let mut builder = notify_rust::Notification::new();
    builder.appname(app_name).summary(title).body(body);
    if critical {
        set_critical_urgency(&mut builder);
    }
    builder.show()?;
    Ok(())
}

/// Escalate a notification to Critical urgency where the backend supports
/// it. `notify-rust` only exposes `.urgency()` on XDG (Linux/BSD) and
/// Windows — macOS has no urgency concept — so this is a platform-gated
/// no-op elsewhere rather than a compile error.
#[cfg(any(all(unix, not(target_os = "macos")), target_os = "windows"))]
fn set_critical_urgency(builder: &mut notify_rust::Notification) {
    builder.urgency(notify_rust::Urgency::Critical);
}

#[cfg(not(any(all(unix, not(target_os = "macos")), target_os = "windows")))]
fn set_critical_urgency(_builder: &mut notify_rust::Notification) {}

/// Char-boundary-safe truncation with an ellipsis.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Agent;
    use crate::AgentKind;
    use std::sync::Arc;
    use time::macros::datetime;

    fn agent(state: AgentState) -> Agent {
        Agent {
            tmux_socket: None,
            tmux_session: None,
            kind: AgentKind::ClaudeCode,
            session_id: "s".into(),
            surface: None,
            pane: Some("%7".into()),
            pid: None,
            workload: crate::WorkloadSummary::default(),
            cwd: None,
            state,
            last_prompt: Some("refactor the ipc module to use tokio io_uring".into()),
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
            started_at: datetime!(2026-04-24 12:00:00 UTC),
            last_activity_at: datetime!(2026-04-24 12:00:00 UTC),
            state_entered_at: datetime!(2026-04-24 12:00:00 UTC),
        }
    }

    #[test]
    fn filter_matches_attention_transitions() {
        let cases = [
            (AgentState::Working, AgentState::WaitingInput, true),
            (AgentState::Idle, AgentState::WaitingInput, true),
            (AgentState::Working, AgentState::WaitingChoice, true),
            (AgentState::Idle, AgentState::WaitingChoice, true),
            (AgentState::Working, AgentState::Error, true),
            (AgentState::Working, AgentState::Stopped, true),
            // Not attention-worthy:
            (AgentState::Starting, AgentState::Idle, false),
            (AgentState::Idle, AgentState::Working, false),
            (AgentState::Working, AgentState::Idle, false),
            (AgentState::Idle, AgentState::Stopped, false),
        ];
        for (from, to, expected) in cases {
            let t = Transition {
                from,
                to,
                agent: Arc::new(agent(to)),
            };
            assert_eq!(should_notify(&t), expected, "{from:?} -> {to:?}");
        }
    }

    #[test]
    fn render_uses_last_prompt_when_no_notification() {
        let t = Transition {
            from: AgentState::Working,
            to: AgentState::WaitingInput,
            agent: Arc::new(agent(AgentState::WaitingInput)),
        };
        let (title, body) = render("muxa", &t);
        assert_eq!(title, "muxa · claude_code · needs input");
        assert!(body.starts_with("pane %7: refactor"), "body = {body:?}");
    }

    #[test]
    fn humanize_state_is_human_readable() {
        assert_eq!(humanize_state(AgentState::WaitingInput), "needs input");
        assert_eq!(humanize_state(AgentState::WaitingChoice), "needs choice");
        assert_eq!(humanize_state(AgentState::Error), "error");
        assert_eq!(humanize_state(AgentState::Stopped), "stopped");
    }

    #[test]
    fn should_fire_debounces_same_state_within_window() {
        let window = Duration::from_secs(30);
        // Never notified before → fire.
        assert!(should_fire(None, AgentState::WaitingInput, window));
        // Same state, still inside the window → suppress (flapping agent).
        assert!(!should_fire(
            Some((AgentState::WaitingInput, Duration::from_secs(5))),
            AgentState::WaitingInput,
            window,
        ));
        // Same state, window elapsed → fire again (still stuck; remind).
        assert!(should_fire(
            Some((AgentState::WaitingInput, Duration::from_secs(31))),
            AgentState::WaitingInput,
            window,
        ));
        // Different target state inside the window → always fire (a real
        // escalation like input → error must not be swallowed).
        assert!(should_fire(
            Some((AgentState::WaitingInput, Duration::from_secs(1))),
            AgentState::Error,
            window,
        ));
    }

    #[test]
    fn render_truncates_long_body() {
        let mut a = agent(AgentState::WaitingInput);
        a.last_notification = Some("x".repeat(500));
        let t = Transition {
            from: AgentState::Working,
            to: AgentState::WaitingInput,
            agent: Arc::new(a),
        };
        let (_, body) = render("muxa", &t);
        // "pane %7: " is 9 chars; body then up to BODY_TRUNCATE chars incl. ellipsis.
        let suffix = body.strip_prefix("pane %7: ").unwrap();
        assert!(suffix.chars().count() <= BODY_TRUNCATE);
        assert!(suffix.ends_with('…'));
    }

    #[test]
    fn truncate_is_char_boundary_safe() {
        // A multi-byte char right at the boundary must not panic or split.
        let s = "a".repeat(79) + "é";
        let out = truncate(&s, 80);
        assert_eq!(out.chars().count(), 80);
    }
}
