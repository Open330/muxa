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

use muxa_core::state::Transition;
use muxa_core::AgentState;
use tokio::sync::broadcast;

/// Max body length we hand to the notification backend. Longer prompts are
/// truncated with an ellipsis. Most desktop environments clip past ~100
/// chars anyway, and libnotify treats the body as markup on some backends.
const BODY_TRUNCATE: usize = 80;

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
        loop {
            match rx.recv().await {
                Ok(t) => {
                    if !should_notify(&t) {
                        continue;
                    }
                    let (title, body) = render(&self.app_name, &t);
                    if let Err(e) = post(&self.app_name, &title, &body) {
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
        (_, AgentState::WaitingInput | AgentState::Error)
            | (AgentState::Working, AgentState::Stopped)
    )
}

/// Build (title, body) strings for a given transition.
fn render(_app: &str, t: &Transition) -> (String, String) {
    let title = format!("muxa · {} · {}", t.agent.kind, t.to);

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
/// the only function that actually touches `notify-rust`.
fn post(app_name: &str, title: &str, body: &str) -> Result<(), notify_rust::error::Error> {
    notify_rust::Notification::new()
        .appname(app_name)
        .summary(title)
        .body(body)
        .show()?;
    Ok(())
}

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
    use muxa_core::state::Agent;
    use muxa_core::AgentKind;
    use time::macros::datetime;

    fn agent(state: AgentState) -> Agent {
        Agent {
            kind: AgentKind::ClaudeCode,
            session_id: "s".into(),
            pane: Some("%7".into()),
            cwd: None,
            state,
            last_prompt: Some("refactor the ipc module to use tokio io_uring".into()),
            last_notification: None,
            model: None,
            context_used_pct: None,
            cost_usd: None,
            started_at: datetime!(2026-04-24 12:00:00 UTC),
            last_activity_at: datetime!(2026-04-24 12:00:00 UTC),
        }
    }

    #[test]
    fn filter_matches_attention_transitions() {
        let cases = [
            (AgentState::Working, AgentState::WaitingInput, true),
            (AgentState::Idle, AgentState::WaitingInput, true),
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
                agent: agent(to),
            };
            assert_eq!(should_notify(&t), expected, "{from:?} -> {to:?}");
        }
    }

    #[test]
    fn render_uses_last_prompt_when_no_notification() {
        let t = Transition {
            from: AgentState::Working,
            to: AgentState::WaitingInput,
            agent: agent(AgentState::WaitingInput),
        };
        let (title, body) = render("muxa", &t);
        assert_eq!(title, "muxa · claude_code · waiting_input");
        assert!(body.starts_with("pane %7: refactor"), "body = {body:?}");
    }

    #[test]
    fn render_truncates_long_body() {
        let mut a = agent(AgentState::WaitingInput);
        a.last_notification = Some("x".repeat(500));
        let t = Transition {
            from: AgentState::Working,
            to: AgentState::WaitingInput,
            agent: a,
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
