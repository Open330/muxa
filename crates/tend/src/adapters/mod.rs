//! Adapters translate each agent CLI's native events into `AgentEvent`.
//!
//! Three of the supported agents (Claude Code, Codex, and the Antigravity
//! CLI) expose a shell-hook system that delivers JSON on stdin. They all
//! implement the [`HookAdapter`] trait, and callers invoke [`run_hook`] to do
//! the boilerplate. Their payload shapes are not interchangeable.
//!
//! GitHub Copilot CLI has shell hooks too, but some of its hooks carry no
//! state change, so [`copilot::parse`] returns an optional event instead.
//!
//! opencode does not expose shell hooks; its integration ships as an
//! in-process TS plugin that forwards a filtered subset of opencode's own
//! events, which [`opencode::parse`] maps — see the [`opencode`] module.

pub mod antigravity;
pub mod antigravity_transcript;
pub mod claude;
pub mod codex;
pub mod codex_rollout;
pub mod copilot;
pub mod copilot_session;
pub mod hook;
pub mod opencode;
pub mod proc_ancestry;
pub mod transcript;

pub use hook::{run_hook, AdapterError, HookAdapter};

pub use antigravity::AntigravityAdapter;
pub use claude::ClaudeAdapter;
pub use codex::CodexAdapter;
