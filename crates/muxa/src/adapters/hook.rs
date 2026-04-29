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
/// `pane` resolution, in order:
/// 1. `$TMUX_PANE` (tmux sets this on every shell inside a pane).
/// 2. `$ZELLIJ_PANE_ID` (zellij's analog).
/// 3. Walk the parent-pid chain and match against the active backend's
///    `pane_pid_map()`. Useful when a SDK sub-process didn't inherit
///    the host env var. Skipped when the backend's `caps().pane_pid_map`
///    reports the lookup is structurally unsupported (zellij CLI today)
///    rather than transiently empty — saves a fruitless walk on every
///    sub-process hook.
///
/// Any failure (no host, /proc unreadable, no match) yields
/// `pane: None`. The daemon's IPC layer accepts paneless events; agent
/// state still flows, the watch UI just hides them by default.
pub fn run_hook<A, R>(event_flag: &str, stdin: &mut R) -> Result<AgentEvent, AdapterError>
where
    A: HookAdapter,
    R: Read,
{
    let event = A::parse_event(event_flag)?;
    let mut buf = String::new();
    stdin.read_to_string(&mut buf)?;
    let input: A::Input = serde_json::from_str(&buf)?;
    let pane = host_pane_env()
        .or_else(|| resolve_pane_via_ancestry(crate::backend::default_backend().as_ref()));
    Ok(A::normalize(event, input, pane))
}

/// Read whichever host-set "this pane" env var is present, in
/// `TMUX_PANE` then `ZELLIJ_PANE_ID` order. Empty string is treated as
/// unset; see also `crate::backend::detect_host_env` for the host-
/// selection precedence.
fn host_pane_env() -> Option<String> {
    for name in ["TMUX_PANE", "ZELLIJ_PANE_ID"] {
        if let Ok(v) = std::env::var(name) {
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

/// Walk our parent PID chain and look each ancestor up in the
/// backend's pane-pid map. Returns the matching `pane_id` string
/// when an ancestor is the shell of a known pane.
///
/// Skips entirely when the backend reports `caps().pane_pid_map ==
/// false` — for zellij CLI-only the map is structurally never going
/// to populate, so walking the chain is just `/proc` traffic for no
/// reward. Tmux backends keep the existing behaviour.
fn resolve_pane_via_ancestry(backend: &dyn crate::backend::PaneBackend) -> Option<String> {
    use crate::adapters::proc_ancestry::{ancestor_in_set, parent_pid};
    if !backend.caps().pane_pid_map {
        return None;
    }
    let pid_map = backend.pane_pid_map();
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
