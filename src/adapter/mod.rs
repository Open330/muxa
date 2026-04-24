//! Adapters translate each agent CLI's native events into `AgentEvent`.
//!
//! Layout: one file per agent. The `claude` adapter is the reference
//! implementation — it reads JSON-over-stdin (from a Claude Code hook) and
//! emits a normalized event to the daemon.
//!
//! Other adapters (opencode, codex, gemini) are stubs pending upstream
//! research — they'll likely share the stdin-JSON pattern for agents that
//! support hooks, and use a `pipe-wrap` subcommand for those that don't.

pub mod claude;
pub mod opencode;
pub mod codex;
pub mod gemini;
