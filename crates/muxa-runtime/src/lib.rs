//! Runtime I/O for muxa: the IPC protocol and the tmux CLI wrapper.

pub mod discovery;
pub mod ipc;
pub mod notify;
pub mod tmux;

pub use discovery::{run_discovery, scan_panes, Discovered, DiscoveryReport};
pub use ipc::{Client, RuntimeError, Server};
pub use notify::{Notifier, NotifyError};
