//! Runtime I/O for muxa: the IPC protocol and the tmux CLI wrapper.

pub mod ipc;
pub mod tmux;

pub use ipc::{Client, RuntimeError, Server};
