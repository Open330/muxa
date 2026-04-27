//! Shared machinery for stdin-JSON hook adapters.

use crate::event::{AgentEvent, AgentKind};
use serde::de::DeserializeOwned;
use std::io::Read;

#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("unknown hook event: {0}")]
    UnknownEvent(String),
    #[error("i/o error reading stdin: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid hook JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// Contract implemented by each stdin-JSON adapter.
pub trait HookAdapter {
    /// Per-adapter typed hook-event enum.
    type Event;
    /// Per-adapter stdin payload shape.
    type Input: DeserializeOwned;

    /// Which agent CLI this adapter targets.
    const KIND: AgentKind;

    /// Parse the `--event` flag value into a typed event variant.
    fn parse_event(flag: &str) -> Result<Self::Event, AdapterError>;

    /// Translate one typed event + parsed stdin payload into an `AgentEvent`.
    /// `pane` is `$TMUX_PANE` if the hook was invoked inside tmux.
    fn normalize(event: Self::Event, input: Self::Input, pane: Option<String>) -> AgentEvent;
}

/// Shared hook entrypoint. Binaries call this after parsing `--event`.
///
/// Reads stdin to EOF, parses as `A::Input`, normalizes to `AgentEvent`.
///
/// `pane` resolution: prefer the `TMUX_PANE` env var (set by tmux for
/// any process running inside a pane). When that's missing — most
/// commonly because the hook fired from a Claude Code SDK sub-process
/// whose env didn't inherit it — fall back to walking the process
/// ancestry and matching against `tmux list-panes`'s `pane_pid` map.
/// The fallback is best-effort: any failure (no tmux, /proc unreadable,
/// no match) yields `pane: None` exactly as before.
pub fn run_hook<A, R>(event_flag: &str, stdin: &mut R) -> Result<AgentEvent, AdapterError>
where
    A: HookAdapter,
    R: Read,
{
    let event = A::parse_event(event_flag)?;
    let mut buf = String::new();
    stdin.read_to_string(&mut buf)?;
    let input: A::Input = serde_json::from_str(&buf)?;
    let pane = std::env::var("TMUX_PANE")
        .ok()
        .or_else(resolve_pane_via_ancestry);
    Ok(A::normalize(event, input, pane))
}

/// Walk our parent PID chain and look each ancestor up in the tmux
/// pane-pid map. Returns the matching `pane_id` string when an
/// ancestor is the shell of a known tmux pane.
///
/// Skips entirely when tmux returns no panes (no server running, etc).
fn resolve_pane_via_ancestry() -> Option<String> {
    use crate::adapters::proc_ancestry::{ancestor_in_set, parent_pid};
    use crate::tmux::pane_pid_map;
    let pid_map = pane_pid_map();
    if pid_map.is_empty() {
        return None;
    }
    let pids: std::collections::HashSet<u32> = pid_map.keys().copied().collect();
    let me = std::process::id();
    let matched = ancestor_in_set(me, &pids, parent_pid)?;
    pid_map.get(&matched).cloned()
}

/// Utility: truncate a prompt/message to `max` bytes, appending a single
/// ellipsis. Used by every adapter so long prompts don't blow out the event.
pub(crate) fn truncate(mut s: String, max: usize) -> String {
    if s.len() > max {
        // Truncate to a char boundary <= max, preserving UTF-8 validity.
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s.push('…');
    }
    s
}
