//! Unix-domain-socket IPC.
//!
//! **Wire format.** Line-delimited JSON (one request per line, one
//! response per line). `serde_json` never emits newlines inside a value, so
//! embedded `\n` in strings is safely escaped — see the round-trip test.
//!
//! Every request carries a `protocol` field set to `PROTOCOL_VERSION`. The
//! server rejects mismatched versions to prevent schema drift from silently
//! corrupting state.
//!
//! **Socket permissions.** The server chmods the socket file to `0600` after
//! binding so only the owning user can send events.
//!
//! **Shutdown.** The server accepts a `CancellationToken`-style signal via
//! the `shutdown` channel and stops accepting new connections, then drains
//! its tracked in-flight handlers (with a bounded timeout) before
//! returning. The drain is what gives the snapshotter task its
//! "last-to-die" guarantee: by the time `Server::run` returns, no handler
//! can call `Store::apply` afterwards, so the daemon's final flush
//! captures every state change the user actually triggered.

mod mailbox_cache;
mod response_cost;
#[cfg(all(test, unix))]
mod snapshot_tests;
#[path = "worker_drain_control.rs"]
mod worker_drain_control;

use crate::ask::{
    AskConversation, AskCredential, AskEntry, AskProviderAdd, AskProviderEdit, AskProviderInfo,
    AskStore,
};
use crate::automation::{
    AutomationLedgerEntry, AutomationRule, AutomationRules, AutomationStore, AutomationSubject,
    AutomationTestReport,
};
use crate::backend::{default_backend, HostKind, SharedBackend};
use crate::collaboration::{
    self, AirArtifactReference, CollaborationClientKind, CollaborationOptions, CollaborationOrigin,
    CollaborationOriginMatch, CollaborationPaneEvidence, CollaborationProvenance,
    CollaborationRequest, CollaborationStore, MailboxScope, NewRequest, Participant,
    RequestMailbox, RequestStatus, RoomContext,
};
use crate::collaboration_audit::{
    CollaborationAuditContext, CollaborationAuditLog, CollaborationAuditOperation,
};
use crate::event::{AgentEvent, AgentKind, PROTOCOL_VERSION};
use crate::fleet::{
    FleetCommandResult, FleetOperation, FleetRuntime, FleetSnapshot, FleetUpdate, LabelSelector,
};
use crate::pipeline_run::{
    PipelineAliasStatus, PipelineClaim, PipelineRun, PipelineRunRegistration, PipelineRunStore,
};
use crate::session::{
    PtySessionBackend, SessionBackend, SessionOutput, SessionRef, SharedSessionBackend,
    SpawnSession, TerminalSnapshot,
};
use crate::state::{Agent, SharedStore};
use crate::tmux::PaneInfo;
use crate::transport::{self, Listener, ReadHalf, Stream, WriteHalf};
use crate::work::WorkIdentity;
use crate::work_compose::{self, WorkComposeOutput, WorkComposeRequest};
use crate::work_control::{
    self, RemoteWorkRunner, WorkCommandLimits, WorkCommandOutput, WorkCommandSurface, WorkUpRequest,
};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinSet;

/// Maximum time `Server::run` will wait for in-flight handlers to finish
/// after the shutdown signal lands. Sized for the longest plausible
/// handler — a `recap_all` query reading several MB of NDJSON — plus
/// generous slack. If a handler hangs past this we abort it rather than
/// blocking the daemon's exit indefinitely.
const HANDLER_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_IPC_LINE_BYTES: usize = 8 * 1024 * 1024;

/// Upper bound on concurrent connection handlers. Kept well under the
/// daemon's file-descriptor budget so a burst — or a leak — of handlers can
/// never drive `accept()` into `EMFILE` and wedge the whole listener the way
/// a runaway of hung hook connections once did. The server reserves a permit
/// before `accept()`, so over-budget connections wait in the OS backlog or
/// time out client-side instead of consuming another daemon fd.
const MAX_INFLIGHT_HANDLERS: usize = 256;

/// How often the server confirms its socket file is still the one it bound.
/// Anything that unlinks `/tmp/muxa-<uid>.sock` (a tmp cleaner, a stray
/// script) otherwise leaves the daemon running but unreachable until restart.
const ENDPOINT_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// How long a starting server waits for another holder to release the
/// endpoint claim before deciding a daemon already owns it.
const CLAIM_WAIT: Duration = Duration::from_secs(2);

/// How long a connection may sit between requests without sending a complete
/// line before the handler closes it. Generous enough that a legitimately
/// idle persistent client is never dropped mid-session, tight enough that a
/// client which connects and never sends EOF cannot pin a file descriptor
/// indefinitely. Does not apply to the streaming pump — a `Subscribe`
/// connection leaves the request loop entirely (see [`stream_transitions`]).
const IDLE_CONN_TIMEOUT: Duration = Duration::from_secs(10);

/// Cadence of keepalive writes on a `Subscribe` stream. A dead watch client
/// that has stopped reading is detected on the next keepalive write (broken
/// pipe) instead of lingering until the next real transition — bounding a
/// dead stream's fd lifetime to roughly one interval.
const STREAM_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

const fn default_session_wait_ms() -> u64 {
    15_000
}

/// A snapshot plan or restore leaves existing sessions alone unless the
/// client says otherwise (`mux_snapshot_plan_v1`).
const fn mux_snapshot_only_missing() -> bool {
    true
}

/// Overall deadline for a client request/response round trip (connect +
/// hello + write + read). No caller should ever block forever against a
/// wedged or half-dead daemon.
const CLIENT_CALL_TIMEOUT: Duration = Duration::from_secs(3);

/// Default observed-idle time past which a worker is flagged as long idle.
pub const DEFAULT_WORKER_IDLE_SECS: u64 = 24 * 60 * 60;

fn default_worker_idle_secs() -> u64 {
    DEFAULT_WORKER_IDLE_SECS
}
/// Backoff between attempts of a collaboration wait interrupted by a
/// daemon restart, doubling up to the cap.
const WAIT_RETRY_INITIAL: Duration = Duration::from_millis(200);
const WAIT_RETRY_MAX: Duration = Duration::from_secs(2);
/// A `work_command` waits for a 30 s child plus Fleet transport slack.
const WORK_COMMAND_CLIENT_TIMEOUT: Duration = Duration::from_secs(50);

/// A collaboration wait occupies one bounded IPC handler and client
/// connection. Keep the ceiling aligned with the MCP surface so a caller
/// cannot pin daemon resources indefinitely.
const MAX_COLLABORATION_WAIT_SECS: u64 = 600;

/// Compatibility cadence used only when an older daemon explicitly rejects
/// the event-driven `collaboration_wait` request kind. This loop lives wholly
/// inside one CLI/MCP tool call, so it never consumes model turns; current
/// daemons always take the revision-driven path instead.
const LEGACY_COLLABORATION_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Tighter deadline for hook ingest, which runs on the agent's critical path
/// (every prompt / every tool call). A wedged daemon must never stall the
/// agent, so fail fast and let `best_effort_ingest` treat it as a no-op.
const HOOK_CALL_TIMEOUT: Duration = Duration::from_millis(750);
mod protocol;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod work_command_tests;
pub use protocol::*;

mod lifecycle;
pub use lifecycle::*;

mod work;
pub use work::*;

mod server;
pub use server::*;

mod framing;
use framing::{
    encode_line, is_client_disconnect, read_limited_line, stream_agent_changes,
    stream_fleet_updates, stream_revision_updates, stream_revision_updates_filtered,
    stream_transitions, until_server_shutdown, write_line_or_closed, write_subscription_ack,
};

mod topology;
use topology::{resolve_backend, unique_pane_endpoint, CollaborationTopology};

mod automation;
use automation::{automation_judge_test, automation_subjects};

mod identity;
use identity::{
    collaboration_participants, observe_collaboration_actor, promote_purpose_targets,
    represented_participant, CollaborationConnectionActor,
};

mod config;
use config::{
    collaboration_audit_entry, mux_snapshot_response, orchestration_config_response,
    SWAP_CLIENT_TIMEOUT,
};

mod swap;
pub use swap::*;

mod dispatch;
use dispatch::handle;

mod blocking;
pub use blocking::*;

mod client;
pub use client::*;
