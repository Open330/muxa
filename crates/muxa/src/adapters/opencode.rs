//! **Status:** not yet implemented. See README "Agent support" table.
//!
//! opencode — deferred.
//!
//! opencode does not expose a shell-hook surface. Its integration approach
//! will be one of:
//!
//!   1. TS plugin at `~/.config/opencode/plugins/muxa.ts` subscribing to
//!      the in-process event bus and forwarding normalized events to muxa.
//!   2. muxa spawning `opencode serve` and consuming the SSE `/event`
//!      stream directly.
//!
//! Both are on the roadmap. For now this module is intentionally empty; the
//! user-facing `muxa hook opencode` subcommand is disabled at the CLI layer.
