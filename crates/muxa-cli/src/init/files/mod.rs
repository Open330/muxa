//! Per-target file editors.
//!
//! Each submodule is a pure-string content layer for one external
//! file. They produce `(new_content, Outcome)` pairs and never touch
//! the disk — `apply.rs` owns I/O so the same edits can be unit-tested
//! and dry-run-rendered.

pub mod claude;
pub mod codex;
pub mod collaboration;
pub mod dashboard;
pub mod gemini;
pub mod launchd;
pub mod opencode;
pub mod shellrc;
pub mod systemd;
pub mod tmux;
