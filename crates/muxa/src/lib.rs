//! `muxa` — observability for AI coding agents in tmux.
//!
//! See README.md for the high-level model. Library consumers reach the
//! main types through this facade; internal cross-module access uses the
//! full `muxa::ipc::Server` form.
//!
//! # Public API surface
//!
//! The following items are considered the **stable** library surface and
//! follow semver from the beta release onward:
//!
//! - [`Config`], [`Error`]
//! - [`Agent`], [`PromptRecord`], [`Store`], [`SharedStore`], [`Transition`],
//!   [`ReconcileReport`]
//! - [`AgentEvent`], [`AgentId`], [`AgentKind`], [`AgentState`],
//!   [`NotificationLevel`]
//! - [`PromptHistory`], [`HistoryEntry`] (return / argument types of stable
//!   `Store` methods such as [`Store::with_history`] and
//!   [`Store::recent_prompts`])
//! - The pane backend abstraction: [`PaneBackend`], [`SharedBackend`],
//!   [`BackendCaps`], [`HostKind`], [`TmuxBackend`], [`ZellijBackend`],
//!   [`default_backend`]
//!
//! Everything else re-exported from this crate is internal task wiring used
//! by the `muxad` and `muxa` binaries in this workspace. Those items are
//! marked `#[doc(hidden)]` and may move or change shape between minor
//! releases without notice — please do not depend on them from external
//! crates.
//!
//! The schema version constants ([`PROTOCOL_VERSION`],
//! [`HISTORY_SCHEMA_VERSION`], [`STATE_SCHEMA_VERSION`]) remain `pub` so the
//! daemon and on-disk readers can negotiate compatibility, but they describe
//! an **unstable wire format** — the values may bump on any minor release
//! when the underlying schema changes.

pub mod activity;
pub mod adapters;
pub mod backend;
pub mod config;
pub mod dashboard;
pub mod discovery;
pub mod error;
pub mod event;
pub mod history;
pub mod ipc;
pub mod metrics;
pub mod notify;
pub mod paths;
pub mod reconcile;
pub mod session;
pub mod session_activity;
pub mod sinks;
pub mod snapshot;
pub mod state;
pub mod timeline;
pub mod tmux;

// ---------------------------------------------------------------------------
// Stable public surface.
// ---------------------------------------------------------------------------

pub use backend::{
    default_backend, tmux::TmuxBackend, zellij::ZellijBackend, BackendCaps, HostKind, PaneBackend,
    SharedBackend,
};
pub use config::Config;
pub use error::CoreError as Error;
pub use event::{
    AgentEvent, AgentId, AgentKind, AgentState, NotificationLevel, SurfaceKind, SurfaceRef,
};
pub use history::{HistoryEntry, PromptHistory};
pub use state::{Agent, PromptRecord, ReconcileReport, SharedStore, Store, Transition};

// ---------------------------------------------------------------------------
// Unstable wire-format constants — `pub` so daemon/readers can negotiate
// compatibility, but the value may change on any minor release.
// ---------------------------------------------------------------------------

#[doc(inline)]
pub use event::PROTOCOL_VERSION;
#[doc(inline)]
pub use history::HISTORY_SCHEMA_VERSION;
#[doc(inline)]
pub use snapshot::STATE_SCHEMA_VERSION;

// ---------------------------------------------------------------------------
// Internal task wiring. Still `pub` so the workspace binaries can reach
// them, but `#[doc(hidden)]` to keep them off the documented surface and
// signal "do not depend on this from outside the workspace".
// ---------------------------------------------------------------------------

#[doc(hidden)]
pub use activity::{
    ActivityEntry, ActivityLog, ActivityOptions, HumanInteractionEntry, HumanInteractionInput,
    HumanInteractionKind, SessionForegroundEntry, StateTransitionEntry, StateTransitionInput,
    ACTIVITY_SCHEMA_VERSION,
};
#[doc(hidden)]
pub use discovery::{run_discovery, scan_panes, Discovered, DiscoveryReport};
#[doc(hidden)]
pub use error::{CoreError, Result};
#[doc(hidden)]
pub use history::{CompactReport, HistoryOptions, PaneSessionCache};
#[doc(hidden)]
pub use metrics::{Metrics, MetricsSnapshot};
#[doc(hidden)]
pub use reconcile::{LivenessSource, Reconciler};
#[doc(hidden)]
pub use session::{
    PtySessionBackend, SessionBackend, SessionBackendCaps, SessionBackendKind, SessionError,
    SessionOutput, SessionRef, SharedSessionBackend, SpawnSession, TerminalSnapshot,
};
#[doc(hidden)]
pub use session_activity::{
    SessionActivity, SessionActivityTracker, SESSION_ACTIVITY_SCHEMA_VERSION,
};
#[doc(hidden)]
pub use snapshot::{Snapshotter, SnapshotterOptions};
#[doc(hidden)]
pub use timeline::{
    TimelineBuildInput, TimelineDocument, TimelineFilters, TimelineInterval,
    TimelineIntervalSource, TimelineLane, TimelineLaneKind, TimelineRange, TimelineTotals,
};
