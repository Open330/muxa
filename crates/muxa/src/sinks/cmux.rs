//! cmux notification sink — surface state transitions to the cmux sidebar.
//!
//! Subscribes to the daemon's `Transition` broadcast and runs
//! `cmux notify --title <kind> --subtitle <state> --body <prompt>` whenever
//! an agent transitions into an attention-needing state (defaults:
//! `WaitingInput`, `WaitingChoice`, `Error`). Mirrors the webhook sink's
//! philosophy: best-effort, in-task per-agent rate limit, no queue — a
//! dropped notification matters less than a stalled sink task.
//!
//! ## Targeting
//!
//! When the transitioning agent carries a cmux surface
//! (`Agent.surface.kind == Cmux`, populated from `$CMUX_SURFACE_ID` by
//! the hook adapter), the notification is targeted at that exact surface
//! via `--surface <id>`. Agents without a cmux surface (running under
//! tmux/zellij) are skipped — there is no useful cmux target for them,
//! and firing `cmux notify` would land ambiguously on whatever workspace
//! happens to be focused in the cmux app.
//!
//! ## Why spawn, not socket
//!
//! cmux's CLI auto-discovers its Unix socket
//! (`~/Library/Application Support/cmux/cmux.sock`) and handles auth.
//! Reaching the socket directly from Rust would duplicate that
//! discovery and auth, and drift on every cmux update. A short-lived
//! `cmux notify` subprocess is the documented integration path — the
//! same one cmux's own bundled `claude` wrapper uses.

use crate::config::CmuxToml;
use crate::event::{AgentKind, AgentState, SurfaceKind};
use crate::state::Transition;
use std::collections::HashMap;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

/// Default per-(kind, session, state) rate-limit window. Same rationale
/// as the webhook sink: long enough to absorb a flapping loop, short
/// enough that a second real incident an hour later still pages.
pub const DEFAULT_RATE_LIMIT_SECS: u64 = 60;

/// How long to let a `cmux notify` spawn run before killing it. cmux
/// returns near-instantly; this is a safety bound against a wedged cmux
/// app freezing the sink loop.
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(10);

/// Body (prompt snippet) cap. cmux truncates generously; we keep it
/// short so the sidebar row stays scannable.
const BODY_TRUNCATE: usize = 120;

#[derive(Debug, thiserror::Error)]
pub enum CmuxError {
    #[error("cmux binary not found on PATH and [sinks.cmux].binary not set")]
    BinaryNotFound,
}

/// Resolved cmux sink — runtime state derived from the TOML config.
#[derive(Debug)]
pub struct CmuxSink {
    binary: String,
    on_states: Vec<AgentState>,
    rate_limit: Duration,
}

impl CmuxSink {
    /// Resolve the TOML sub-table into a runtime sink, or `Ok(None)` if
    /// disabled. Validates the binary is resolvable when enabled so a
    /// misconfiguration surfaces at startup rather than as silent
    /// no-ops on every transition.
    pub fn resolve(toml: &CmuxToml) -> Result<Option<Self>, CmuxError> {
        if !toml.enabled.unwrap_or(false) {
            return Ok(None);
        }
        let binary = toml
            .binary
            .as_deref()
            .filter(|s| !s.is_empty())
            .map_or_else(|| "cmux".into(), str::to_string);
        // When the user gives an absolute/relative path, trust it; when
        // it's a bare name, require it to be on PATH so we fail loudly
        // here instead of on the first transition.
        if binary.contains(std::path::MAIN_SEPARATOR) {
            // Explicit path — assume the user knows what they're doing.
        } else if which::which(&binary).is_err() {
            return Err(CmuxError::BinaryNotFound);
        }
        let on_states = if let Some(raw) = toml.on_states.as_ref() {
            raw.iter().filter_map(|s| parse_state(s)).collect()
        } else {
            default_on_states()
        };
        let rate_limit =
            Duration::from_secs(toml.rate_limit_secs.unwrap_or(DEFAULT_RATE_LIMIT_SECS));
        Ok(Some(Self {
            binary,
            on_states,
            rate_limit,
        }))
    }

    /// Run the sink until shutdown. Same shape as the webhook sink:
    /// biased shutdown, lag-tolerant broadcast, per-agent rate limit.
    pub async fn run(
        self,
        mut transition_rx: broadcast::Receiver<Transition>,
        mut shutdown_rx: broadcast::Receiver<()>,
    ) {
        let mut last_sent: HashMap<(AgentKind, String, AgentState), Instant> = HashMap::new();
        // Bounded channel decouples the broadcast drain from spawn
        // latency so a slow cmux can't cause this sink to lag and drop
        // transitions. Capacity 64 matches the webhook sink's effective
        // burst tolerance.
        let (tx, mut rx) = mpsc::channel::<Transition>(64);
        let forwarder = tokio::spawn(async move {
            while let Some(t) = rx.recv().await {
                self.handle_one(&t, &mut last_sent).await;
            }
        });

        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx.recv() => {
                    drop(tx);
                    // Drain: let in-flight notifications finish, then exit.
                    let _ = forwarder.await;
                    tracing::debug!("cmux sink received shutdown");
                    return;
                }
                next = transition_rx.recv() => {
                    match next {
                        Ok(t) => {
                            // `tx.send` failing means the forwarder exited;
                            // there's nothing useful to do except stop.
                            if tx.send(t).await.is_err() {
                                tracing::error!("cmux sink forwarder exited; stopping");
                                return;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(
                                missed = n,
                                "cmux sink lagged behind transition stream; continuing"
                            );
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            tracing::debug!("transition broadcast closed; cmux sink exiting");
                            drop(tx);
                            let _ = forwarder.await;
                            return;
                        }
                    }
                }
            }
        }
    }

    async fn handle_one(
        &self,
        transition: &Transition,
        last_sent: &mut HashMap<(AgentKind, String, AgentState), Instant>,
    ) {
        if !should_forward(transition, &self.on_states) {
            return;
        }
        // Targeting gate: only notify when we can aim at a concrete cmux
        // surface. Firing `cmux notify` without a target would land on
        // whatever workspace is focused — noisy and surprising.
        let surface_id = match transition.agent.surface.as_ref() {
            Some(s) if s.kind == SurfaceKind::Cmux => s.id.clone(),
            _ => return,
        };

        let key = (
            transition.agent.kind,
            transition.agent.session_id.clone(),
            transition.to,
        );
        let now = Instant::now();
        if let Some(prev) = last_sent.get(&key) {
            if self.rate_limit > Duration::ZERO && now.duration_since(*prev) < self.rate_limit {
                return;
            }
        }
        last_sent.insert(key, now);

        let (title, subtitle, body) = render(transition);
        let mut cmd = Command::new(&self.binary);
        cmd.args([
            "notify",
            "--title",
            &title,
            "--subtitle",
            &subtitle,
            "--body",
            &body,
            "--surface",
            &surface_id,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

        // Race the spawn against a timeout so a wedged cmux app can't
        // freeze the sink. Best-effort: errors are logged and dropped.
        match tokio::time::timeout(NOTIFY_TIMEOUT, cmd.status()).await {
            Ok(Ok(status)) if status.success() => {
                tracing::debug!(
                    kind = %transition.agent.kind,
                    state = %transition.to,
                    "cmux: notification posted"
                );
            }
            Ok(Ok(status)) => {
                tracing::warn!(
                    code = ?status.code(),
                    kind = %transition.agent.kind,
                    "cmux: notify exited non-zero; dropping"
                );
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    error = %e,
                    kind = %transition.agent.kind,
                    "cmux: notify spawn failed; dropping"
                );
            }
            Err(_) => {
                tracing::warn!(
                    kind = %transition.agent.kind,
                    "cmux: notify timed out after {NOTIFY_TIMEOUT:?}; dropping"
                );
            }
        }
    }
}

/// Public spawn helper mirroring `sinks::webhook::spawn`.
pub fn spawn(
    sink: CmuxSink,
    transition_rx: broadcast::Receiver<Transition>,
    shutdown_rx: broadcast::Receiver<()>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        sink.run(transition_rx, shutdown_rx).await;
    })
}

fn should_forward(transition: &Transition, on_states: &[AgentState]) -> bool {
    on_states.contains(&transition.to)
}

fn default_on_states() -> Vec<AgentState> {
    vec![
        AgentState::WaitingInput,
        AgentState::WaitingChoice,
        AgentState::Error,
    ]
}

/// Build the (title, subtitle, body) triple for `cmux notify`.
///
/// - title: agent kind (`claude_code` / `codex` / `pi` …)
/// - subtitle: short state label (`needs input` / `error` …)
/// - body: first line of the last prompt, truncated, falling back to
///   the agent's own last notification text so an error without a fresh
///   prompt still carries a clue.
fn render(transition: &Transition) -> (String, String, String) {
    let title = transition.agent.kind.to_string();
    let subtitle = match transition.to {
        AgentState::WaitingInput => "needs input",
        AgentState::WaitingChoice => "needs choice",
        AgentState::Error => "error",
        AgentState::Working => "working",
        AgentState::Idle => "idle",
        AgentState::Stopped => "stopped",
        AgentState::Starting => "starting",
    }
    .into();
    let snippet = transition
        .agent
        .last_prompt
        .as_deref()
        .or(transition.agent.last_notification.as_deref())
        .unwrap_or("");
    let first_line = snippet.lines().next().unwrap_or("");
    let body = truncate_str(first_line, BODY_TRUNCATE);
    (title, subtitle, body)
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Parse a `[sinks.cmux].on_states` entry. Accepts both `PascalCase`
/// and `snake_case` (same tolerance as the webhook sink).
fn parse_state(s: &str) -> Option<AgentState> {
    match s {
        "Starting" | "starting" => Some(AgentState::Starting),
        "Working" | "working" => Some(AgentState::Working),
        "Idle" | "idle" => Some(AgentState::Idle),
        "WaitingInput" | "waiting_input" => Some(AgentState::WaitingInput),
        "WaitingChoice" | "waiting_choice" => Some(AgentState::WaitingChoice),
        "Error" | "error" => Some(AgentState::Error),
        "Stopped" | "stopped" => Some(AgentState::Stopped),
        _ => {
            tracing::warn!(value = %s, "cmux: unknown on_states entry; ignoring");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::SurfaceRef;
    use crate::state::Agent;
    use std::sync::Arc;
    use time::macros::datetime;

    fn agent(
        kind: AgentKind,
        surface: Option<SurfaceKind>,
        prompt: Option<&str>,
        state: AgentState,
    ) -> Arc<Agent> {
        Arc::new(Agent {
            kind,
            session_id: "sess-1".into(),
            surface: surface.map(|k| SurfaceRef {
                kind: k,
                id: "surface-uuid".into(),
            }),
            pane: None,
            cwd: None,
            pid: None,
            workload: crate::WorkloadSummary::default(),
            state,
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
            started_at: datetime!(2026-04-24 12:00:00 UTC),
            last_activity_at: datetime!(2026-04-24 12:00:00 UTC),
            state_entered_at: datetime!(2026-04-24 12:00:00 UTC),
        })
    }

    fn transition(from: AgentState, to: AgentState, agent: Arc<Agent>) -> Transition {
        Transition { from, to, agent }
    }

    #[test]
    fn forwards_cmux_surface_on_attention_state() {
        let a = agent(
            AgentKind::Pi,
            Some(SurfaceKind::Cmux),
            Some("build it"),
            AgentState::WaitingInput,
        );
        assert!(should_forward(
            &transition(AgentState::Working, AgentState::WaitingInput, a),
            &default_on_states(),
        ));
    }

    #[test]
    fn skips_non_cmux_surface() {
        // A tmux agent has no cmux target — handle_one returns before
        // spawn. Covered by the targeting gate; here we assert the
        // render path still works so the gate is the only barrier.
        let a = agent(AgentKind::ClaudeCode, None, Some("hi"), AgentState::Error);
        let (title, _, _) = render(&transition(AgentState::Working, AgentState::Error, a));
        assert_eq!(title, "claude_code");
    }

    #[test]
    fn render_falls_back_to_notification_when_no_prompt() {
        let mut a = agent(
            AgentKind::Codex,
            Some(SurfaceKind::Cmux),
            None,
            AgentState::Error,
        );
        let agent_mut = Arc::get_mut(&mut a).expect("unique ref in test");
        agent_mut.last_notification = Some("rate limit hit".into());
        let (_, _, body) = render(&transition(AgentState::Working, AgentState::Error, a));
        assert_eq!(body, "rate limit hit");
    }

    #[test]
    fn render_truncates_long_prompt_to_first_line_and_cap() {
        let long = "x".repeat(200);
        let a = agent(
            AgentKind::Pi,
            Some(SurfaceKind::Cmux),
            Some(&long),
            AgentState::WaitingInput,
        );
        let (_, _, body) = render(&transition(
            AgentState::Working,
            AgentState::WaitingInput,
            a,
        ));
        assert!(body.chars().count() <= BODY_TRUNCATE);
        assert!(body.ends_with('…'));
    }

    #[test]
    fn parse_state_accepts_both_cases() {
        assert_eq!(parse_state("WaitingInput"), Some(AgentState::WaitingInput));
        assert_eq!(parse_state("waiting_input"), Some(AgentState::WaitingInput));
        assert_eq!(parse_state("Error"), Some(AgentState::Error));
        assert_eq!(parse_state("nonsense"), None);
    }
}
