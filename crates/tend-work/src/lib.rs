//! The Work model and agent launch, shared by the `tend` CLI and `tendd`.
//!
//! These modules used to live in the CLI binary, so the daemon could only
//! reach them by running the sibling `tend` executable. They are a library
//! now; the CLI keeps its command-line views (`tend work list|show|done`,
//! the `work up` entry point and the watch popup) on top of them.

pub mod account_module;
pub mod agent_launch;
pub mod backlog;
pub mod caller;
pub mod mux_control;
pub mod spawn_provenance;
pub mod tmux_work;
pub mod work_up;
pub mod worker_startup;
pub mod worktrees;
