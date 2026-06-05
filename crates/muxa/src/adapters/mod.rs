//! Adapters translate each agent CLI's native events into `AgentEvent`.
//!
//! Three of the supported agents (Claude Code, Codex, Gemini CLI) expose a
//! shell-hook system that delivers JSON on stdin. They all implement the
//! [`HookAdapter`] trait, and callers invoke [`run_hook`] to do the
//! boilerplate.
//!
//! opencode does not expose shell hooks; its integration ships as an
//! in-process TS plugin that forwards pre-normalized `AgentEvent`s — see the
//! [`opencode`] module.

pub mod claude;
pub mod codex;
pub mod gemini;
pub mod hook;
pub mod opencode;
pub mod proc_ancestry;
pub mod transcript;

pub use hook::{run_hook, AdapterError, HookAdapter};

pub use claude::ClaudeAdapter;
pub use codex::CodexAdapter;
pub use gemini::GeminiAdapter;
pub use opencode::OpencodeAdapter;
