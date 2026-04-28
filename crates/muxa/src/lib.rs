//! `muxa` — observability for AI coding agents in tmux.
//!
//! See README.md for the high-level model. Library consumers reach the
//! main types through this facade; internal cross-module access uses the
//! full `muxa::ipc::Server` form.

pub mod adapters;
pub mod backend;
pub mod config;
pub mod dashboard;
pub mod discovery;
pub mod error;
pub mod event;
pub mod history;
pub mod ipc;
pub mod notify;
pub mod paths;
pub mod reconcile;
pub mod sinks;
pub mod snapshot;
pub mod state;
pub mod tmux;

pub use backend::{
    default_backend, tmux::TmuxBackend, zellij::ZellijBackend, BackendCaps, HostKind, PaneBackend,
    SharedBackend,
};
pub use config::Config;
pub use discovery::{run_discovery, scan_panes, Discovered, DiscoveryReport};
pub use error::{CoreError, Result};
pub use event::{AgentEvent, AgentId, AgentKind, AgentState, NotificationLevel, PROTOCOL_VERSION};
pub use history::{
    CompactReport, HistoryEntry, HistoryOptions, PromptHistory, HISTORY_SCHEMA_VERSION,
};
pub use reconcile::{LivenessSource, Reconciler};
pub use snapshot::{Snapshotter, SnapshotterOptions, STATE_SCHEMA_VERSION};
pub use state::{Agent, PromptRecord, ReconcileReport, SharedStore, Store, Transition};
