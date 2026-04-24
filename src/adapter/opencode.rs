//! opencode adapter — NOTE: opencode does NOT have shell-hook support.
//!
//! Its integration surface is:
//!
//!   1. Built-in HTTP + SSE server (`GET /event` firehose), OR
//!   2. An in-process TS plugin loaded from
//!      `~/.config/opencode/plugins/*.ts` or `.opencode/plugins/*.ts`.
//!
//! We ship a plugin at `examples/opencode-muxa-plugin.ts` that subscribes
//! to the wildcard `event` hook and forwards a small whitelist of events
//! to the muxa daemon socket.
//!
//! For stdin-JSON symmetry with the other adapters, we also accept a
//! pre-built `AgentEvent` on stdin (see `muxa hook opencode --event raw`)
//! — this is what the TS plugin calls into.

use crate::event::AgentEvent;
use anyhow::{Context, Result};

pub fn parse_stdin_agent_event() -> Result<AgentEvent> {
    let mut buf = String::new();
    use std::io::Read;
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("reading AgentEvent JSON from stdin")?;
    serde_json::from_str(&buf).context("parsing AgentEvent JSON")
}
