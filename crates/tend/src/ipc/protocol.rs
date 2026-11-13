use super::*;

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("socket already exists and is in use at {0}; another daemon may be running")]
    SocketInUse(PathBuf),

    #[error(
        "daemon not reachable at {} — is `tendd` running? (start `tendd`, run `tend doctor`, or set TEND_SOCKET)",
        .0.display()
    )]
    NotConnected(PathBuf),

    #[error("ipc message exceeds {0} bytes")]
    MessageTooLarge(usize),

    #[error("ipc request timed out after {0:?}")]
    Timeout(Duration),

    /// The daemon answered, and said no. Carries the server's own message so
    /// a refusal reaches the user as a refusal instead of being flattened
    /// into an empty result — see [`decode_agents`].
    #[error("{0}")]
    Daemon(String),

    /// The connection closed before any answer arrived: tendd stopped or
    /// restarted mid-request. Distinct from a malformed answer, so a caller
    /// can tell "ask again" from "something is broken".
    #[error("tendd closed the connection before answering (it may be restarting); retry")]
    Disconnected,
}

impl RuntimeError {
    pub(super) fn is_client_disconnect(&self) -> bool {
        matches!(self, Self::Io(e) if is_client_disconnect(e))
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum RequestBody {
    Ingest {
        event: AgentEvent,
        #[serde(default)]
        launch_id: Option<String>,
        #[serde(default)]
        ask_entry: Option<String>,
    },
    Snapshot {
        #[serde(default)]
        launch_identity: bool,
    },
    InvalidateAgentDirectory,
    AgentFind {
        query: String,
        limit: usize,
        #[serde(default)]
        exclude_sessions: Vec<String>,
        #[serde(default)]
        refresh: bool,
    },
    /// Ask the room's namespace arbiter for a handle. The daemon is the only
    /// place that sees pane options, registered identities, and handles
    /// promised to callers that have not written them yet.
    CollaborationIssueHandle {
        pane: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        socket: Option<String>,
        request: collaboration::HandleRequest,
    },
    /// `turn_evidence_v1`: the calls `session_id` made since `since`
    /// (RFC 3339, the reviewed prompt's time) and their results, from this
    /// host's own records.
    TurnEvidence {
        session_id: String,
        /// When the prompt being reviewed was submitted; required.
        since: String,
    },
    /// Record a claim that a result needs review (`review_items_v1`).
    ReviewCreate {
        review: Box<crate::review::NewReview>,
    },
    ReviewGet {
        id: String,
    },
    /// Newest first, at most `review::MAX_LIST_LIMIT`.
    ReviewList {
        #[serde(default)]
        open_only: bool,
        #[serde(default)]
        oldest_first: bool,
        #[serde(default = "default_review_list_limit")]
        limit: usize,
        #[serde(default)]
        offset: usize,
    },
    /// Open count and oldest open request time, for status surfaces.
    ReviewSummary,
    ReviewDispose {
        id: String,
        disposition: crate::review::DispositionKind,
        by: crate::review::ReviewActor,
        #[serde(default)]
        note: Option<String>,
    },
    /// Record a claim and its disposition together (`review_record_v1`):
    /// `review.requested_by` both asks and decides. Needs an idempotency key.
    ReviewRecord {
        review: Box<crate::review::NewReview>,
        disposition: crate::review::DispositionKind,
        #[serde(default)]
        note: Option<String>,
    },
    /// Everything pending on the person, in one ordered list
    /// (`decision_queue_v1`).
    ///
    /// Human requests come only to an operator-console origin, as with any
    /// listing past the caller's own mailbox; other callers get reviews and
    /// waiting agents, with the human source named as unavailable.
    DecisionQueue {
        #[serde(default = "default_review_list_limit")]
        limit: usize,
        #[serde(default)]
        origin: Option<collaboration::CollaborationOrigin>,
    },
    /// Decided review items and answered human requests, newest first
    /// (`decision_queue_v1`).
    DecisionHistory {
        #[serde(default = "default_review_list_limit")]
        limit: usize,
        #[serde(default)]
        origin: Option<collaboration::CollaborationOrigin>,
    },
    /// Durable Work records, backlog order (`work_backlog_v1`). With
    /// `stage`, only records at that stage.
    WorkBacklog {
        #[serde(default)]
        stage: Option<crate::work::WorkStage>,
    },
    /// Create or partially edit one Work record without launching it.
    WorkEdit {
        identity: crate::work::WorkIdentity,
        edit: crate::work::WorkEdit,
    },
    /// One Work's record, its result records and its durable Run: what was
    /// asked, done and decided, whether or not a window is open.
    WorkRecordShow {
        identity: crate::work::WorkIdentity,
        /// Only the newest `newest` result records (still oldest first in
        /// the reply), for surfaces that show a recent slice.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        newest: Option<usize>,
    },
    /// Attach or refresh one external issue reference on a Work record
    /// (`work_external_v1`). The client fetched it; tendd only stores it.
    WorkExternalUpsert {
        identity: crate::work::WorkIdentity,
        item: crate::work::ExternalItemRef,
    },
    /// Durable desired graph and per-alias execution state for every Work Run.
    PipelineRuns,
    PipelineSubscribe,
    /// Start the canonical `tend work up` implementation as a bounded daemon
    /// operation. The initial call returns immediately; native clients poll
    /// `work_up_status` so a ticket lookup never freezes their state stream.
    WorkUp {
        request: WorkUpRequest,
    },
    WorkUpStatus {
        operation_id: String,
    },
    /// Run one allowlisted `tend work options|preset|pipeline|route …` argv
    /// on the local host or a Fleet host and return its exit code and
    /// streams. Bounded to 30 s and 1 MiB of output.
    /// `swap_jobs_v1`: accept a swap as a daemon-owned job, or return the
    /// job its request key already made.
    SwapJobStart {
        request_key: String,
        overrides: crate::swap_api::SwapOverrides,
    },
    /// `swap_jobs_v1`: a job by id or request key, readable even while the
    /// service is halted.
    SwapJobStatus {
        job: crate::swap_api::JobRef,
    },
    /// `swap_plan_v1`: what a start would do, doing none of it.
    SwapPlan {
        overrides: crate::swap_api::SwapOverrides,
    },
    /// `swap_health_v1`: how the swap service is, read-only.
    SwapHealth,
    /// `swap_accounts_v1`: configured accounts first, then optional aas.
    SwapAccounts,
    /// `swap_cancel_v1`: ask a job to start no further target.
    SwapJobCancel {
        job: crate::swap_api::JobRef,
    },
    /// `swap_retry_v1`: retry an ended job's safely retryable targets as a
    /// new job under `request_key`.
    SwapJobRetry {
        request_key: String,
        parent: crate::swap_api::JobRef,
        #[serde(default)]
        overrides: crate::swap_api::RetryOverrides,
    },
    WorkCommand {
        #[serde(default)]
        host: Option<String>,
        args: Vec<String>,
        #[serde(default)]
        stdin: Option<String>,
    },
    // WS-B: new agent
    /// Start one allowlisted agent through `tend agent start --json`, on
    /// this host or a control-mode Fleet host. Synchronous, bounded to 30 s.
    AgentStart {
        request: crate::agent_control::AgentStartRequest,
    },
    // WS-F: snapshot
    /// Workspace snapshots, run through `tend snapshot` / `tend restore
    /// --json` (`mux_snapshot_v1`). Local host only. Newest first.
    MuxSnapshotList,
    /// Take a manual snapshot of the server tend's agents are on.
    MuxSnapshotSave,
    /// Dry-run a restore of one snapshot: per session "create" or "skip",
    /// and (`mux_snapshot_plan_v1`) what is missing, present or new since
    /// the snapshot on the server now.
    MuxSnapshotPlan {
        id: String,
        /// `mux_snapshot_plan_v1`: false plans existing sessions to get
        /// the windows and panes they lack. Absent means true, which is
        /// all a `mux_snapshot_v1` daemon does.
        #[serde(default = "mux_snapshot_only_missing")]
        only_missing: bool,
    },
    /// Start restoring the sessions of one snapshot that the server does
    /// not have; unless `only_missing` is false (`mux_snapshot_plan_v1`),
    /// existing sessions are never touched. Poll with
    /// `mux_snapshot_restore_status`.
    MuxSnapshotRestore {
        id: String,
        #[serde(default)]
        layout_only: bool,
        #[serde(default = "mux_snapshot_only_missing")]
        only_missing: bool,
        /// `mux_snapshot_continue_v1`: after the relaunch, type this into
        /// every agent the snapshot caught mid-turn (`tend restore
        /// --continue`). Absent or blank sends nothing.
        #[serde(default, rename = "continue")]
        continue_text: Option<String>,
    },
    MuxSnapshotRestoreStatus {
        operation_id: String,
    },
    MuxSnapshotDelete {
        id: String,
    },
    // END WS-F: snapshot
    /// Draft one pipeline from a description with a read-only headless
    /// turn, validated with the `pipeline set` rules and retried once on a
    /// draft that would not launch. Writes nothing.
    WorkCompose {
        description: String,
        /// Provider to draft with; absent means the ask store's selection.
        #[serde(default)]
        agent: Option<String>,
        /// A previous draft to refine; `description` is then the change.
        #[serde(default)]
        current: Option<crate::work_pipeline_spec::PipelineSpec>,
        /// Same shape and handling as `ask_send`'s.
        #[serde(default)]
        credential: Option<AskCredential>,
    },
    /// `ask_one_shot_v1`: one read-only turn of a configured Ask provider,
    /// answered to the caller and recorded nowhere (no conversation, no
    /// history). Tend.app uses it to organize review results and to tailor
    /// one reply into a message per agent.
    AskOneShot {
        prompt: String,
        /// Provider to ask; absent means the ask store's selection.
        #[serde(default)]
        agent: Option<String>,
        /// Same shape and handling as `ask_send`'s.
        #[serde(default)]
        credential: Option<AskCredential>,
    },
    /// Create or update one desired Run and reconcile live pane evidence.
    PipelineRegister {
        registration: PipelineRunRegistration,
    },
    /// Atomic, generation-checked completion event.
    PipelineDone {
        identity: WorkIdentity,
        alias: String,
        generation: u64,
    },
    /// Start a new generation for an alias and its downstream closure.
    PipelineInvalidate {
        identity: WorkIdentity,
        alias: String,
        generation: u64,
    },
    /// Atomically reserve every dependency-ready pending alias.
    PipelineClaim {
        identity: WorkIdentity,
        generation: u64,
    },
    /// Report the physical outcome of a claimed launch or re-prompt.
    PipelineReport {
        identity: WorkIdentity,
        alias: String,
        generation: u64,
        status: PipelineAliasStatus,
        #[serde(default)]
        pane: Option<String>,
        #[serde(default)]
        error: Option<String>,
        #[serde(default)]
        window_id: Option<String>,
    },
    /// Snapshot of every configured physical SSH host. `selector` follows
    /// Kubernetes label-selector syntax and is evaluated against central
    /// inventory metadata, never against untrusted remote data.
    FleetSnapshot {
        #[serde(default)]
        selector: Option<String>,
    },
    /// Long-lived notification stream for changes to the central Fleet cache.
    /// Payloads are deliberately tiny; clients fetch one coherent filtered
    /// snapshot after coalescing notifications.
    FleetSubscribe {
        #[serde(default)]
        selector: Option<String>,
    },
    /// Route one exact operation through the per-host persistent SSH relay.
    #[serde(alias = "fleet_command_contract")]
    FleetCommand {
        host: String,
        operation: FleetOperation,
    },
    FleetWaitReply {
        #[serde(default)]
        after_update: Option<u64>,
        host: String,
        pane: crate::topology::PaneKey,
        request_id: String,
        timeout_secs: u64,
    },
    ByPane {
        pane: String,
    },
    BySession {
        session_id: String,
    },
    BySurface {
        surface_id: String,
    },
    /// Disk-backed prompt audit log. `pane = None` returns prompts across
    /// every tracked pane, sorted newest-first; otherwise filtered to one
    /// pane. `limit = 0` (or absent) returns everything available, capped
    /// by the daemon's in-memory retention.
    RecentPrompts {
        #[serde(default)]
        pane: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    Health,
    /// Ask the real daemon to drain and stop cleanly. Like `restart`, this is
    /// opt-in so embedded IPC servers used by tests and integrations cannot
    /// be terminated by an ordinary client connection.
    Stop,
    /// Ask the daemon to drain and re-exec itself onto the binary currently
    /// installed at its argv[0]. Opt-in: only the real daemon installs a
    /// restart controller; embedders refuse rather than shutting down with no
    /// way to come back.
    Restart,
    /// Capability handshake. Optional first message; opts the connection
    /// into negotiated-protocol mode. The server replies with its
    /// `[min, max]` supported range and a list of capability tags, then
    /// downgrades wire-visible enum variants for the rest of the
    /// connection so a client pinned to an older protocol stays usable.
    Hello {
        #[serde(default)]
        client: Option<String>,
    },
    /// Coalesced registry invalidations, including same-state activity.
    SubscribeAgentChanges,

    /// Long-lived streaming subscribe. Server replies with a one-shot
    /// `ok` ack, then writes one JSON-encoded `Transition` per
    /// state change (newline-delimited) until the client closes the
    /// socket. Used by `tend watch` to switch from 500 ms polling to
    /// push-based updates, and by the `tend mcp` server's
    /// `tend_wait_for_change` tool.
    ///
    /// `lagged_markers` (default `false`) opts the connection into receiving
    /// the `{"event":"lagged","dropped":N}` control frame after a broadcast
    /// overflow. It defaults OFF so a pre-marker client (whose `Transition`
    /// parser would choke on the frame and abandon push mode) keeps the
    /// historical behavior — the server silently continues after a lag, and the
    /// client reconciles the gap via its fallback snapshot poll. tend's own
    /// `TransitionStream` reader understands the frame and opts in.
    Subscribe {
        #[serde(default)]
        lagged_markers: bool,
    },

    /// Control action: inject `text` into `pane` as literal keystrokes,
    /// resolving the backend from the pane-id namespace. When `submit`,
    /// a trailing carriage return is sent as a second injection so the
    /// agent's current line is committed. Refused with a structured
    /// error (never a panic) when the target backend lacks the
    /// `send_text` capability (e.g. zellij). Backs `tend mcp`'s
    /// `tend_send_prompt` tool.
    SendPrompt {
        pane: String,
        /// Explicit server endpoint; permits an untracked pane on that server.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        socket: Option<String>,
        /// Refuse unless this agent session still occupies the target pane.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        text: String,
        #[serde(default)]
        submit: bool,
        /// Who asked for it (`prompt_origin_v1`); recorded on the prompt the
        /// agent reports. Only a person, peer or automation can be declared;
        /// anything else is recorded as tend.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<crate::PromptOrigin>,
        /// `send_prompt_state_gate_v1`. `Some(false)` refuses unless the
        /// target is Idle; `Some(true)` types regardless. Absent keeps the
        /// pre-gate behaviour for older operator clients (watch, Mac app),
        /// which type on a person's explicit action.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        force: Option<bool>,
    },

    /// `prompt_origin_v1`: a caller that types into `pane` itself (a Fleet
    /// relay) registers the prompt first, so the hook reporting it is
    /// stamped with `origin`.
    ExpectPrompt {
        pane: String,
        #[serde(default)]
        socket: Option<String>,
        text: String,
        origin: crate::PromptOrigin,
    },

    /// Drops the registration `token` named after typing failed.
    ForgetExpectedPrompt {
        token: u64,
    },

    /// Control/observation: capture the visible contents of `pane` via
    /// the namespace-resolved backend. Backs `tend mcp`'s
    /// `tend_capture_pane` tool. Returns `capture = null` when the pane
    /// is gone or the backend can't capture (best-effort).
    Capture {
        pane: String,
    },

    CollaborationContext {
        origin: CollaborationOrigin,
    },
    CollaborationSetIdentity {
        origin: CollaborationOrigin,
        #[serde(default)]
        alias: Option<String>,
        #[serde(default)]
        roles: Vec<String>,
    },
    #[serde(alias = "collaboration_send_contract")]
    CollaborationSend {
        origin: CollaborationOrigin,
        target: String,
        request: Box<NewRequest>,
        #[serde(default)]
        view: collaboration::CollaborationReadView,
    },
    /// Queue a headless question. Returns the pending entry immediately;
    /// the answer lands in the store when the agent exits.
    AskSend {
        prompt: String,
        /// Optional one-turn credential. The socket is owner-only (0600);
        /// the daemon moves this directly into the child environment and the
        /// Ask store never persists it.
        #[serde(default)]
        credential: Option<AskCredential>,
    },
    /// Queue the first turn of a fresh conversation atomically. This is a
    /// distinct request kind so an older daemon rejects it instead of
    /// silently ignoring a new flag and appending to the active conversation.
    AskSendNew {
        prompt: String,
        #[serde(default)]
        credential: Option<AskCredential>,
    },
    AskSubscribe,
    /// Report whether the daemon accepted the explicit `[ask].enabled`
    /// grant at startup. This lets native clients present setup before a
    /// typed question fails with a configuration error.
    AskStatus {},
    AskList {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        conversation_id: Option<String>,
    },
    /// List durable conversations and identify the selected one for the
    /// current provider.
    AskConversationList {},
    /// Resume a prior tend conversation, switching provider when needed.
    AskConversationSelect {
        conversation_id: String,
    },
    /// Point the next question at a different agent, or read back which
    /// one is selected when `agent` is omitted.
    AskAgent {
        #[serde(default)]
        agent: Option<String>,
    },
    /// Start a fresh conversation. History is untouched.
    AskReset {},
    /// Remove completed ask history. Running asks and conversation ids stay.
    AskClear {},
    /// Remove one completed ask history entry by opaque id.
    AskDelete {
        id: String,
    },
    /// Every provider instance the daemon can ask — the ones the operator
    /// composed plus the built-ins — with the engine behind each, the
    /// effective model, and which one is selected.
    AskProviders {},
    /// Hand out the daemon's `config.toml` as text, so a client can edit
    /// the sections that have no typed request of their own.
    ConfigRead {},
    ConfigOrchestrationRead {},
    ConfigOrchestrationWrite {
        expected_text: String,
        settings: crate::orchestration::OrchestrationConfig,
    },
    ConfigLaunchRead {},
    ConfigLaunchWrite {
        expected_text: String,
        edits: Vec<crate::config_file::LaunchEdit>,
    },
    /// Replace `config.toml` with `text`. Refused unless the document
    /// parses and validates, and unless `expected_text` (when given) still
    /// matches what is on disk, so two editors cannot clobber each other.
    ConfigWrite {
        text: String,
        #[serde(default)]
        expected_text: Option<String>,
    },
    /// Edit `[ask.providers.<provider>]`. Each key is tri-state: absent
    /// from the request leaves it unchanged, `null` clears it, a string
    /// sets it — so a client sends only what it changed. `engine` is not
    /// among them: it is what the instance is, not a setting. Answers with
    /// the updated provider list.
    AskProviderConfigure {
        provider: String,
        // `Option<Option<_>>` is the point: the outer level is "was the key
        // sent", the inner is "null or a value", and clients rely on both.
        #[allow(clippy::option_option)]
        #[serde(default, deserialize_with = "double_option")]
        title: Option<Option<String>>,
        #[allow(clippy::option_option)]
        #[serde(default, deserialize_with = "double_option")]
        model: Option<Option<String>>,
        #[allow(clippy::option_option)]
        #[serde(default, deserialize_with = "double_option")]
        api_key_env: Option<Option<String>>,
        #[allow(clippy::option_option)]
        #[serde(default, deserialize_with = "double_option")]
        executable: Option<Option<String>>,
    },
    /// Add an `[ask.providers.<id>]` instance driven by `engine`, so the
    /// operator can keep two accounts of one provider side by side.
    /// Answers with the updated provider list.
    AskProviderAdd {
        id: String,
        engine: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        model: Option<String>,
        /// The *name* of an environment variable holding the key, never
        /// the key: this is written to config.toml.
        #[serde(default)]
        api_key_env: Option<String>,
        /// Binary a CLI engine spawns for this instance.
        #[serde(default)]
        executable: Option<String>,
    },
    /// Remove an `[ask.providers.<id>]` instance. A built-in id keeps its
    /// row and only loses its overrides. Answers with the updated list.
    AskProviderRemove {
        id: String,
    },
    /// Select `provider` and write it to `[ask] agent`, so the choice
    /// survives a restart. Answers with the updated list.
    AskProviderDefault {
        provider: String,
    },
    // --- automation_v1 ---------------------------------------------------
    /// Every `[[automation.rule]]` with its effective timing, its guards,
    /// and how often it has fired lately, plus the engine's master switch
    /// and pause. Answers in `automation_rules`.
    AutomationList {},
    /// The firing ledger, newest first. Answers in `automation_log`.
    AutomationLog {
        #[serde(default)]
        limit: Option<usize>,
    },
    /// Flip one rule's `enabled`, or the whole engine's when `name` is
    /// absent or null. Takes effect immediately and is written back to
    /// `config.toml`. Answers with the refreshed rule list.
    AutomationSetEnabled {
        #[serde(default)]
        name: Option<String>,
        enabled: bool,
    },
    /// Hold every rule until `until`, or lift the hold with `null`.
    /// Answers with the refreshed rule list.
    AutomationPause {
        #[serde(default, with = "time::serde::rfc3339::option")]
        until: Option<time::OffsetDateTime>,
    },
    /// Write one rule into `config.toml`, replacing the `[[automation.rule]]`
    /// with the same `name` in place or appending it. Validated exactly as
    /// the loader validates it, and the merged document has to read back as
    /// a full `Config` before anything touches disk. Answers with the
    /// refreshed rule list.
    AutomationSetRule {
        rule: AutomationRule,
    },
    /// Remove one rule. An unknown name is refused rather than silently
    /// succeeding — an editor that lost sync should be told. Answers with
    /// the refreshed rule list.
    AutomationRemoveRule {
        name: String,
    },
    /// Evaluate one rule against the live registry and report what it
    /// *would* do, firing nothing and recording nothing. Answers in
    /// `automation_test`.
    AutomationTest {
        name: String,
    },
    AutomationJudgeTest {
        rule: AutomationRule,
        pane: String,
    },
    /// Offer queued user-authorized work to a primary at a turn boundary;
    /// see [`collaboration::turn_offer`]. Answers in `turn_boundary_offers`.
    CollaborationTurnBoundaryOffer {
        origin: CollaborationOrigin,
        event: collaboration::turn_offer::TurnBoundaryEvent,
        #[serde(default)]
        max_requests: Option<usize>,
        #[serde(default)]
        max_bytes: Option<usize>,
        /// Offer kinds the calling hook renders (`reply_ready`); absent from
        /// older hooks.
        #[serde(default)]
        supports: Vec<String>,
    },
    #[serde(alias = "collaboration_accept")]
    CollaborationInbox {
        origin: CollaborationOrigin,
        #[serde(default)]
        request_id: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    CollaborationPurpose {
        origin: CollaborationOrigin,
        purpose: collaboration::LaunchPurpose,
    },
    /// Set the purpose of exact live agent sessions, on the operator's
    /// instruction. Audited with the caller's provenance.
    CollaborationPromotePurpose {
        session_ids: Vec<String>,
        purpose: collaboration::LaunchPurpose,
    },
    /// Daemon-owned worker capacity. Kept separate from inventory reads.
    /// Operator recovery; the supplied origin must resolve to the console.
    CollaborationWorkerSpawnRelease {
        origin: CollaborationOrigin,
        launch_id: String,
        reason: String,
    },
    /// Body-free launch lifecycle records, with the same trust model as snapshot.
    WorkerDrainPlan {
        origin: CollaborationOrigin,
        policy: collaboration::WorkerDrainPolicy,
    },
    WorkerDrainAcquire {
        origin: CollaborationOrigin,
        target: collaboration::WorkerDrainTarget,
        policy: collaboration::WorkerDrainPolicy,
    },
    WorkerDrainRenew {
        origin: CollaborationOrigin,
        token: collaboration::WorkerDrainToken,
    },
    WorkerDrainStop {
        origin: CollaborationOrigin,
        token: collaboration::WorkerDrainToken,
        operation_id: String,
    },
    WorkerDrainFinish {
        origin: CollaborationOrigin,
        token: collaboration::WorkerDrainToken,
        outcome: collaboration::WorkerDrainFinish,
        detail: String,
    },
    WorkerDrains {
        origin: CollaborationOrigin,
    },
    CollaborationWorkerSpawns,
    CollaborationWorkerAcquire {
        origin: CollaborationOrigin,
        spec: collaboration::WorkerSpawnSpec,
    },
    CollaborationWorkerSpawn {
        ticket: collaboration::WorkerSpawnTicket,
        action: collaboration::WorkerSpawnAction,
    },
    CollaborationWorkerLaunch {
        pane: String,
        socket: Option<String>,
        agent_kind: AgentKind,
        launch_id: String,
    },
    /// Long-lived, content-free durable-mailbox invalidation stream. Reading
    /// actual requests still goes through the normal participant/operator
    /// authorization paths.
    CollaborationSubscribe {
        #[serde(default)]
        requests_only: bool,
    },
    CollaborationOverview {
        origin: CollaborationOrigin,
    },
    /// Every worker tend launched that is still live, host-wide. Read-only,
    /// like `snapshot`: it names agents and counts open requests without
    /// reading any body, and changes nothing.
    CollaborationWorkers {
        #[serde(default = "default_worker_idle_secs")]
        idle_threshold_secs: u64,
    },
    CollaborationOpenCounts {
        origin: CollaborationOrigin,
        #[serde(default)]
        scope: MailboxScope,
    },
    CollaborationMailboxSnapshot {
        origin: CollaborationOrigin,
        #[serde(default)]
        operator_only: bool,
        #[serde(default)]
        if_unchanged: Option<String>,
    },
    CollaborationList {
        origin: CollaborationOrigin,
        #[serde(default)]
        mailbox: RequestMailbox,
        /// Absent from every client built before scoped listing existed, and
        /// defaulting to the caller's own mailbox is exactly what those
        /// clients asked for.
        #[serde(default)]
        scope: MailboxScope,
        #[serde(default)]
        query: Option<collaboration::CollaborationQuery>,
        #[serde(default)]
        offset: usize,
        #[serde(default)]
        metadata_only: bool,
        #[serde(default)]
        view: collaboration::CollaborationReadView,
        #[serde(default)]
        auto_scope: bool,
    },
    #[serde(alias = "collaboration_reply_contract")]
    CollaborationReply {
        origin: CollaborationOrigin,
        request_id: String,
        status: RequestStatus,
        #[serde(default)]
        body: String,
        #[serde(default)]
        artifacts: Vec<String>,
        #[serde(default)]
        #[serde(deserialize_with = "collaboration::deserialize_report_input")]
        report: Option<collaboration::TaskReport>,
        #[serde(default)]
        view: collaboration::CollaborationReadView,
    },
    CollaborationGet {
        origin: CollaborationOrigin,
        request_id: String,
        #[serde(default)]
        view: collaboration::CollaborationReadView,
    },
    /// Exact non-acknowledging read for instructions and incremental progress.
    CollaborationPeek {
        origin: CollaborationOrigin,
        request_id: String,
        #[serde(default)]
        view: collaboration::CollaborationReadView,
    },
    CollaborationUpdate {
        origin: CollaborationOrigin,
        request_id: String,
        body: String,
        #[serde(default)]
        view: collaboration::CollaborationReadView,
    },
    /// Block on the mailbox's durable revision signal until this exact
    /// participant-visible request becomes terminal, or return its latest
    /// state at the bounded deadline. Unlike `collaboration_get` loops, one
    /// request occupies no client/model polling turns.
    CollaborationWait {
        origin: CollaborationOrigin,
        request_id: String,
        timeout_secs: u64,
        #[serde(default)]
        view: collaboration::CollaborationReadView,
    },
    CollaborationCancel {
        origin: CollaborationOrigin,
        request_id: String,
        #[serde(default)]
        view: collaboration::CollaborationReadView,
    },
    /// Stop waking the caller for replies to its own listed requests.
    /// Releases notification interest only; see
    /// [`crate::collaboration::CollaborationStore::withdraw_reply_wakes`].
    CollaborationWithdrawReplyWakes {
        origin: CollaborationOrigin,
        request_ids: Vec<String>,
    },
    /// Body-free diagnostics over the daemon's cached projections.
    CollaborationHealth,
    /// Explicit exact-ID operator retirement; no completion or process stop.
    CollaborationRetire {
        origin: CollaborationOrigin,
        request_id: String,
        reason: String,
        evidence: String,
        dry_run: bool,
    },
    /// Legacy operator expiry. Human decisions are protected on this server.
    CollaborationExpire {
        older_than_secs: u64,
        #[serde(default)]
        quiet_only: bool,
        #[serde(default)]
        request_id: Option<String>,
        #[serde(default)]
        dry_run: bool,
    },
    /// A separate operation prevents legacy parsers from ignoring filters.
    CollaborationExpireFiltered {
        older_than_secs: u64,
        quiet_only: bool,
        request_id: Option<String>,
        #[serde(default)]
        dry_run: bool,
    },

    /// Filtered expiry with a bounded count and ID sample response.
    CollaborationExpireSummary {
        older_than_secs: u64,
        quiet_only: bool,
        request_id: Option<String>,
        #[serde(default)]
        dry_run: bool,
    },

    /// Wholesale pane-metadata snapshot pushed by an out-of-process
    /// backend source. Designed for a zellij WASM plugin that never shipped
    /// (the stub crate was removed); no in-tree sender exists today, and the
    /// command stays for wire compatibility. The daemon hands the panes to its
    /// `SharedBackend::ingest_pane_snapshot`, which the zellij backend
    /// caches so `list_panes` / `resolve_pane` answer from real data. A
    /// no-op on tmux (that backend enumerates panes itself).
    BackendPaneSnapshot {
        panes: Vec<PaneInfo>,
    },
    /// Observe locations after a move, without changing agent activity/state.
    RefreshPaneLocations {
        pane: String,
    },
    SpawnSession {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: Vec<(String, String)>,
        #[serde(default)]
        cwd: Option<PathBuf>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        cols: Option<u16>,
        #[serde(default)]
        rows: Option<u16>,
    },
    ListSessions,
    CaptureSession {
        session_id: String,
    },
    ReadSession {
        session_id: String,
        offset: u64,
    },
    /// Bounded event-driven terminal read. The daemon waits on the PTY
    /// session's output/exit signal instead of requiring native clients to
    /// issue 20-125 empty reads per second.
    ReadSessionWait {
        session_id: String,
        offset: u64,
        #[serde(default = "default_session_wait_ms")]
        timeout_ms: u64,
    },
    WriteSession {
        session_id: String,
        data: String,
    },
    /// Byte-safe input for native terminal clients. The legacy
    /// `write_session` request remains available for text-only clients.
    WriteSessionBytes {
        session_id: String,
        data_base64: String,
    },
    ResizeSession {
        session_id: String,
        cols: u16,
        rows: u16,
    },
    SetSessionAttached {
        session_id: String,
        #[serde(default)]
        client_id: Option<String>,
        attached: bool,
    },
    TerminateSession {
        session_id: String,
    },
    /// Register an arbitrary background process as a pid-tracked `Task` row
    /// so it shows up in `tend status`/`tend watch`. Backs `tend register`.
    Register {
        name: String,
        #[serde(default)]
        pid: Option<u32>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        pane: Option<String>,
        #[serde(default)]
        command: Option<String>,
    },
    /// Explicitly forget one exact agent session without terminating its host.
    ForgetAgent {
        session_id: String,
    },
    /// Delete fully orphaned rows (no pane, surface, or pid) idle longer than
    /// `max_age_secs`. Backs `tend prune` — the on-demand cleanup of
    /// remote/detached ghost rows the reconciler would otherwise only age out
    /// after its 24h sweep. `max_age_secs = 0` (or absent) removes every
    /// orphan regardless of age. Replies with `pruned` = rows removed.
    Prune {
        #[serde(default)]
        max_age_secs: u64,
    },
}

#[derive(Debug, Deserialize)]
pub(super) struct Request {
    /// Wire protocol version the client expects. Must equal `PROTOCOL_VERSION`.
    #[serde(default)]
    pub(super) protocol: u32,
    #[serde(flatten)]
    pub(super) body: RequestBody,
}

/// `Option<Option<T>>` from JSON: a present key — `null` included — is
/// `Some(...)`, so `#[serde(default)]` alone marks the absent case.
#[allow(clippy::option_option)]
pub(super) fn double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// Oldest protocol the server can still serve via the negotiated regime
/// (i.e. with v1-compat enum downgrade). Bumped when we drop the
/// downgrade path for an older variant.
pub const MIN_PROTOCOL_VERSION: u32 = 1;

/// Stable feature tags advertised by `hello`. Each token names a
/// semver-additive capability the server supports; clients use the list
/// to feature-gate behaviour without re-reading `protocol`.
pub(super) const CAPABILITIES: &[&str] = &[
    crate::launch_identity::CAPABILITY,
    crate::provenance::STATE_SOURCE_CAPABILITY,
    "agent_session_id",
    "waiting_choice",
    "needs_choice",
    "rate_limited",
    "collaboration_mailbox",
    collaboration::MAILBOX_SNAPSHOT_CAPABILITY,
    collaboration::OPEN_COUNTS_CAPABILITY,
    collaboration::HUMAN_QUERY_CAPABILITY,
    collaboration::HUMAN_PRIORITY_CAPABILITY,
    collaboration::WORKER_SPAWN_CAPABILITY,
    collaboration::WORKER_DRAIN_CAPABILITY,
    collaboration::WORKER_SPAWN_RELEASE_CAPABILITY,
    collaboration::EXACT_SESSION_CAPABILITY,
    collaboration::INBOX_LIMIT_CAPABILITY,
    "collaboration_overview_v1",
    "collaboration_expire_v1",
    collaboration::COLLABORATION_HEALTH_CAPABILITY,
    "collaboration_expire_filtered_v1",
    collaboration::FILTERED_EXPIRE_CAPABILITY,
    collaboration::EXPIRE_SUMMARY_CAPABILITY,
    collaboration::RETIRE_CAPABILITY,
    "collaboration_lifecycle",
    "collaboration_wait",
    "collaboration_peek",
    "collaboration_subscribe",
    "collaboration_dispatch_v1",
    "orchestration_v1",
    "collaboration_identity",
    "collaboration_provenance",
    "collaboration_scope",
    "fleet_v1",
    "fleet_raw_capture_v1",
    "fleet_subscribe",
    "fleet_wait_reply",
    "collaboration_update",
    "collaboration_peer_recovery",
    collaboration::TASK_CONTRACT_CAPABILITY,
    collaboration::HUMAN_ACTIONS_CAPABILITY,
    collaboration::EXECUTE_OVERLAP_CAPABILITY,
    collaboration::PURPOSE_PROMOTE_CAPABILITY,
    collaboration::PIPELINE_PURPOSE_CAPABILITY,
    collaboration::REPLY_WAKE_WITHDRAW_CAPABILITY,
    collaboration::USER_AUTHORIZED_DELIVERY_CAPABILITY,
    "pipeline_runs_v1",
    crate::review::REVIEW_CAPABILITY,
    "turn_evidence_v1",
    crate::review::REVIEW_RECORD_CAPABILITY,
    crate::decision::REVIEW_DECISION_CAPABILITY,
    crate::decision::HUMAN_DECISION_CAPABILITY,
    crate::decision_queue::DECISION_QUEUE_CAPABILITY,
    crate::work::WORK_BACKLOG_CAPABILITY,
    crate::work::WORK_RECENT_RESULTS_CAPABILITY,
    crate::work::WORK_EXTERNAL_CAPABILITY,
    "pipeline_subscribe",
    "work_control_v1",
    "work_command_v1",
    "agent_start_v1",  // WS-B: new agent
    "mux_snapshot_v1", // WS-F: snapshot
    "mux_snapshot_plan_v1",
    "mux_snapshot_continue_v1",
    "handle_namespace_v1",
    "session_bytes_v1",
    "session_attachment_identity_v1",
    "session_wait_v1",
    "ask_one_turn_credential_v1",
    "ask_status_v1",
    "ask_conversations_v1",
    "ask_send_new_v1",
    "ask_subscribe",
    "ask_providers_v1",
    crate::agent_directory::CAPABILITY,
    "ask_provider_default_v1",
    "work_compose_v1",
    "ask_one_shot_v1",
    "fleet_keys_v1",
    "fleet_multiselect_v1",
    "fleet_worktrees_v1",
    "prompt_origin_v1",
    crate::state::AWAITING_REPLY_CAPABILITY,
    "fleet_worktree_change_v1",
    "fleet_servers_v1",
    "automation_v1",
    "automation_ask_v1",
    "config_launch_v1",
    "config_orchestration_v1",
    "config_edit_v1",
    DAEMON_BOOT_ID_CAPABILITY,
    crate::prompt_delivery::SEND_PROMPT_STATE_GATE_CAPABILITY,
    crate::fleet::FLEET_SEND_PROMPT_GATE_CAPABILITY,
];

/// Advertised by a daemon whose `hello` and successful config read/write
/// responses carry [`daemon_boot_id`]. A client that sees it may treat a
/// changed `boot_id` as proof of a different daemon process; without it, the
/// client must fall back to weaker evidence.
pub const DAEMON_BOOT_ID_CAPABILITY: &str = "daemon_boot_id_v1";

/// A random identity drawn once per daemon process image.
///
/// `generation` cannot tell daemons apart: every fresh process starts at 0 and
/// only a self-reexec increments it, so an external stop/start looks
/// identical. This id is 128 random bits, generated on first use and fixed for
/// the life of the process image. A self-reexec loads a new image and so
/// draws a new id; the generation changes in the same step, so the pair stays
/// consistent and no state has to survive `exec`.
#[must_use]
pub fn daemon_boot_id() -> &'static str {
    static BOOT_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BOOT_ID.get_or_init(|| uuid::Uuid::new_v4().simple().to_string())
}

/// Advertised only when the server has the controller required to come back
/// after draining. A server without one refuses `restart`.
pub(super) const RESTART_CAPABILITY: &str = "restart";
/// Refusal for the config requests when tendd was started without a config
/// file path (`--config` absent and no default location).
pub(super) const NO_CONFIG_PATH: &str =
    "this daemon has no config file path; start tendd with --config or a default config location";
/// Advertised only by a server with a lifecycle controller, which can flush
/// durable writers and remove the socket before exiting.
pub(super) const STOP_CAPABILITY: &str = "stop";

#[derive(Debug, Serialize)]
pub struct Response {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launch_identities: Option<Arc<BTreeMap<String, crate::launch_identity::LaunchIdentity>>>,
    pub ok: bool,
    pub protocol: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<Agent>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_locations: Option<crate::agent_locator::LocationResults>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompts: Option<Vec<crate::history::HistoryEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<HealthInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collaboration_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_protocol: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_protocol: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<&'static str>>,
    /// The daemon's own crate version, sent on `hello`. Protocol numbers move
    /// only when the wire format changes, so two builds can agree on the
    /// protocol and still disagree on everything else; this is what lets a
    /// client notice a stale daemon during the window where the protocol
    /// still matches. Absent from a daemon that predates the field, which is
    /// itself evidence the daemon is old.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<&'static str>,
    /// The listing breadth the daemon actually applied. A daemon that predates
    /// scoped listing simply drops the request field, so its absence here is
    /// how a new client detects that its `--scope` was ignored rather than
    /// silently reading a caller-scoped answer as a fleet-wide one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collaboration_scope: Option<MailboxScope>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collaboration_page: Option<collaboration::CollaborationPage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collaboration_mailbox_snapshot: Option<collaboration::MailboxSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collaboration_open_counts: Option<collaboration::CollaborationOpenCounts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collaboration_byte_budget: Option<usize>,
    /// Present only when the daemon can restart itself. It increments across
    /// each re-exec so a client can distinguish the replacement image from
    /// the old daemon still finishing an in-flight response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    /// Per-process daemon identity from [`daemon_boot_id`], present on every
    /// `hello` and successful config read/write from a daemon advertising
    /// [`DAEMON_BOOT_ID_CAPABILITY`]. Unlike `generation` it differs across an
    /// external stop/start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<&'static str>,
    /// With `turn_evidence`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_evidence: Option<Box<crate::agent_records::evidence::TurnEvidence>>,
    /// The registration an `expect_prompt` made; absent when tend could not
    /// bind it to exactly one agent session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_token: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sessions: Option<Vec<SessionRef>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal: Option<TerminalSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<SessionOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pruned: Option<usize>,
    /// Visible pane contents for a `capture` request. `Some("")` is a
    /// real empty capture; `None` means the field is absent (any other
    /// response kind, or a `capture` whose backend returned nothing).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture: Option<String>,
    /// Whether a `send_prompt`'s text injection landed. Present only on a
    /// `send_prompt` response. `true` means the text is already in the pane
    /// — a caller MUST NOT resend it, even if `submitted` is `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sent: Option<bool>,
    /// Whether a `send_prompt`'s submit CR landed. Present only on a
    /// `send_prompt` response. `false` with `sent:true` and a requested
    /// `submit:true` is a PARTIAL success: the text is in the pane but the
    /// host rejected Enter. Inspect the current input before an Enter-only
    /// retry; never resend the whole prompt. `true` is transport acceptance,
    /// not an application acknowledgement (see `submission_confirmed`). `false` is also the
    /// normal value when `submit:false` was requested (nothing to submit).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submitted: Option<bool>,
    /// Application-level `PromptSubmitted` evidence, separate from transport
    /// acceptance. None means unobserved (including hosts without hooks),
    /// never a reason to resend the body or blindly retry Enter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submission_confirmed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub room: Option<RoomContext>,
    /// Handle issued by the room's namespace arbiter. Absent when the room
    /// had no free name, or when the pane could not be placed in one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collaboration_requests: Option<Vec<CollaborationRequest>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_boundary_offers: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collaboration_overview: Option<collaboration::CollaborationOverview>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collaboration_health: Option<collaboration::CollaborationHealth>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collaboration_expire_summary: Option<collaboration::ExpireSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_drain_plan: Option<collaboration::WorkerDrainPlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_drain_acquire: Option<collaboration::WorkerDrainAcquire>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_drain_token: Option<collaboration::WorkerDrainToken>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_drain_stop: Option<collaboration::WorkerDrainStop>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_drain: Option<collaboration::WorkerDrainView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_drains: Option<Vec<collaboration::WorkerDrainView>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_spawn: Option<collaboration::WorkerSpawnAcquisition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_spawns: Option<Vec<collaboration::WorkerSpawnView>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swap_accounts: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_inventory: Option<collaboration::WorkerInventory>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collaboration_request: Option<CollaborationRequest>,
    /// Set on the answer to a long poll that tendd cut short to restart:
    /// nothing changed, and the same request may be asked again.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restarting: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ask_entries: Option<Vec<AskEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ask_entry: Option<AskEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ask_conversations: Option<Vec<AskConversation>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ask_conversation: Option<AskConversation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ask_agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ask_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ask_providers: Option<Vec<AskProviderInfo>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<crate::config_file::ConfigDocument>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fleet: Option<FleetSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fleet_result: Option<FleetCommandResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pipeline_runs: Option<Vec<PipelineRun>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pipeline_run: Option<PipelineRun>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_item: Option<Box<crate::review::ReviewItem>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_items: Option<Vec<crate::review::ReviewItem>>,
    /// With `review_items`: another page follows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_has_more: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_summary: Option<crate::review::ReviewSummary>,
    /// `work_backlog_v1`: Work records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_records: Option<Vec<crate::work::WorkRecord>>,
    /// `work_backlog_v1`: one Work record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_record: Option<Box<crate::work::WorkRecord>>,
    /// `decision_queue_v1`: everything pending on the person.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_queue: Option<Box<crate::decision_queue::PendingQueue>>,
    /// `decision_queue_v1`: past decisions, newest first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_history: Option<Box<crate::decision_queue::DecisionHistory>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pipeline_claims: Option<Vec<PipelineClaim>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_operation: Option<WorkUpOperation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_command: Option<WorkCommandOutput>,
    /// `swap_jobs_v1` / `swap_plan_v1`: the job, accepted job or plan.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swap: Option<serde_json::Value>,
    /// `agent_start_v1`: the started agent's surface. WS-B: new agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_start: Option<crate::agent_control::AgentStartResult>,
    // WS-F: snapshot
    /// `mux_snapshot_v1`: the CLI's own JSON — the listing, a saved
    /// snapshot, or a restore plan — passed through untouched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mux_snapshot: Option<serde_json::Value>,
    /// `mux_snapshot_v1`: a restore operation and, once done, its report.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mux_snapshot_operation: Option<crate::mux_snapshot_control::MuxSnapshotOperation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_compose: Option<WorkComposeOutput>,
    /// `ask_one_shot_v1`: the provider's answer text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ask_one_shot: Option<String>,
    /// `automation_v1`: the rule list plus the engine's switch/pause.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub automation_rules: Option<AutomationRules>,
    /// `automation_v1`: the firing ledger, newest first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub automation_log: Option<Vec<AutomationLedgerEntry>>,
    /// `automation_v1`: what one rule would do right now.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub automation_test: Option<AutomationTestReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub automation_judgment: Option<crate::automation_judge::AutomationJudgment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launch: Option<crate::config_file::LaunchSettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orchestration: Option<crate::orchestration::OrchestrationConfig>,
}

#[derive(Debug, Serialize)]
pub struct HealthInfo {
    pub version: &'static str,
    pub protocol: u32,
}

impl Response {
    pub(super) fn ok() -> Self {
        Self {
            ok: true,
            protocol: PROTOCOL_VERSION,
            error: None,
            agents: None,
            launch_identities: None,
            agent_locations: None,
            prompts: None,
            health: None,
            collaboration_error: None,
            min_protocol: None,
            max_protocol: None,
            capabilities: None,
            version: None,
            collaboration_scope: None,
            collaboration_page: None,
            collaboration_mailbox_snapshot: None,
            collaboration_open_counts: None,
            collaboration_byte_budget: None,
            generation: None,
            boot_id: None,
            turn_evidence: None,
            prompt_token: None,
            sessions: None,
            session: None,
            terminal: None,
            output: None,
            pruned: None,
            capture: None,
            sent: None,
            submitted: None,
            submission_confirmed: None,
            room: None,
            handle: None,
            collaboration_requests: None,
            turn_boundary_offers: None,
            collaboration_overview: None,
            collaboration_health: None,
            collaboration_expire_summary: None,
            worker_drain_plan: None,
            worker_drain_acquire: None,
            worker_drain_token: None,
            worker_drain_stop: None,
            worker_drain: None,
            worker_drains: None,
            worker_spawn: None,
            worker_spawns: None,
            swap_accounts: None,
            worker_inventory: None,
            collaboration_request: None,
            restarting: None,
            ask_entries: None,
            ask_entry: None,
            ask_conversations: None,
            ask_conversation: None,
            ask_agent: None,
            ask_enabled: None,
            ask_providers: None,
            config: None,
            fleet: None,
            fleet_result: None,
            pipeline_runs: None,
            pipeline_run: None,
            review_item: None,
            review_items: None,
            review_has_more: None,
            review_summary: None,
            decision_queue: None,
            decision_history: None,
            work_records: None,
            work_record: None,
            pipeline_claims: None,
            work_operation: None,
            work_command: None,
            swap: None,
            agent_start: None,
            mux_snapshot: None,
            mux_snapshot_operation: None,
            work_compose: None,
            ask_one_shot: None,
            automation_rules: None,
            automation_log: None,
            automation_test: None,
            automation_judgment: None,
            launch: None,
            orchestration: None,
        }
    }
    pub(super) fn err(msg: impl Into<String>) -> Self {
        let mut r = Self::ok();
        r.ok = false;
        r.error = Some(msg.into());
        r
    }
    pub(super) fn with_agents(agents: Vec<Agent>) -> Self {
        let mut r = Self::ok();
        r.agents = Some(agents);
        r
    }
    pub(super) fn with_fleet(fleet: FleetSnapshot) -> Self {
        let mut response = Self::ok();
        response.fleet = Some(fleet);
        response
    }
    pub(super) fn with_fleet_result(result: FleetCommandResult) -> Self {
        let mut response = Self::ok();
        response.fleet_result = Some(result);
        response
    }
    pub(super) fn with_review_item(item: crate::review::ReviewItem) -> Self {
        let mut response = Self::ok();
        response.review_item = Some(Box::new(item));
        response
    }
    pub(super) fn with_review_page(page: crate::review::ReviewPage) -> Self {
        let mut response = Self::ok();
        response.review_items = Some(page.items);
        response.review_has_more = Some(page.has_more);
        response
    }
    pub(super) fn with_review_summary(summary: crate::review::ReviewSummary) -> Self {
        let mut response = Self::ok();
        response.review_summary = Some(summary);
        response
    }
    pub(super) fn with_pipeline_runs(runs: Vec<PipelineRun>) -> Self {
        let mut response = Self::ok();
        response.pipeline_runs = Some(runs);
        response
    }
    pub(super) fn with_pipeline_run(run: PipelineRun) -> Self {
        let mut response = Self::ok();
        response.pipeline_run = Some(run);
        response
    }
    pub(super) fn with_pipeline_claims(claims: Vec<PipelineClaim>) -> Self {
        let mut response = Self::ok();
        response.pipeline_claims = Some(claims);
        response
    }
    pub(super) fn with_work_operation(operation: WorkUpOperation) -> Self {
        let mut response = Self::ok();
        response.work_operation = Some(operation);
        response
    }
    pub(super) fn with_swap(value: serde_json::Value) -> Self {
        let mut response = Self::ok();
        response.swap = Some(value);
        response
    }
    pub(super) fn with_work_command(output: WorkCommandOutput) -> Self {
        let mut response = Self::ok();
        response.work_command = Some(output);
        response
    }
    pub(super) fn with_agent_start(result: crate::agent_control::AgentStartResult) -> Self {
        let mut response = Self::ok();
        response.agent_start = Some(result);
        response
    }
    // WS-F: snapshot
    pub(super) fn with_mux_snapshot(value: serde_json::Value) -> Self {
        let mut response = Self::ok();
        response.mux_snapshot = Some(value);
        response
    }
    pub(super) fn with_mux_snapshot_operation(
        operation: crate::mux_snapshot_control::MuxSnapshotOperation,
    ) -> Self {
        let mut response = Self::ok();
        response.mux_snapshot_operation = Some(operation);
        response
    }
    pub(super) fn with_ask_one_shot(text: String) -> Self {
        let mut response = Self::ok();
        response.ask_one_shot = Some(text);
        response
    }
    pub(super) fn with_work_compose(output: WorkComposeOutput) -> Self {
        let mut response = Self::ok();
        response.work_compose = Some(output);
        response
    }
    pub(super) fn with_prompts(prompts: Vec<crate::history::HistoryEntry>) -> Self {
        let mut r = Self::ok();
        r.prompts = Some(prompts);
        r
    }
    pub(super) fn health() -> Self {
        let mut r = Self::ok();
        r.health = Some(HealthInfo {
            version: env!("CARGO_PKG_VERSION"),
            protocol: PROTOCOL_VERSION,
        });
        r
    }
    pub(super) fn with_sessions(sessions: Vec<SessionRef>) -> Self {
        let mut r = Self::ok();
        r.sessions = Some(sessions);
        r
    }
    pub(super) fn with_session(session: SessionRef) -> Self {
        let mut r = Self::ok();
        r.session = Some(session);
        r
    }
    pub(super) fn with_terminal(terminal: TerminalSnapshot) -> Self {
        let mut r = Self::ok();
        r.terminal = Some(terminal);
        r
    }
    pub(super) fn with_output(output: SessionOutput) -> Self {
        let mut r = Self::ok();
        r.output = Some(output);
        r
    }
    pub(super) fn with_pruned(pruned: usize) -> Self {
        let mut r = Self::ok();
        r.pruned = Some(pruned);
        r
    }
    pub(super) fn with_capture(capture: Option<String>) -> Self {
        let mut r = Self::ok();
        r.capture = capture;
        r
    }
    /// A `send_prompt` success carrying the two non-atomic outcomes distinctly
    /// (Fix: honest partial-failure signal). Only built when the text landed
    /// (`sent = true`), so `ok = true`; `submitted` reflects whether the
    /// follow-up submit CR also landed.
    pub(super) fn with_send_result(sent: bool, submitted: bool) -> Self {
        let mut r = Self::ok();
        r.sent = Some(sent);
        r.submitted = Some(submitted);
        r
    }
    pub(super) fn with_handle(handle: Option<String>) -> Self {
        let mut r = Self::ok();
        r.handle = handle;
        r
    }
    pub(super) fn with_room(room: RoomContext) -> Self {
        let mut r = Self::ok();
        r.room = Some(room);
        r
    }
    pub(super) fn with_turn_boundary_offers(offers: Vec<serde_json::Value>) -> Self {
        let mut r = Self::ok();
        r.turn_boundary_offers = Some(offers);
        r
    }
    pub(super) fn with_collaboration_requests(requests: Vec<CollaborationRequest>) -> Self {
        let mut r = Self::ok();
        r.collaboration_requests = Some(requests);
        r
    }
    pub(super) fn with_scoped_collaboration_requests(
        requests: Vec<CollaborationRequest>,
        scope: MailboxScope,
    ) -> Self {
        let mut r = Self::with_collaboration_requests(requests);
        r.collaboration_scope = Some(scope);
        r
    }
    /// A read-only long poll ended early because tendd is restarting. It is
    /// still answered, so the client hears "ask again" instead of a closed
    /// socket.
    pub(super) fn restarting() -> Self {
        let mut r = Self::err("tendd is restarting; retry the wait");
        r.restarting = Some(true);
        r
    }
    pub(super) fn with_collaboration_request(request: CollaborationRequest) -> Self {
        let mut r = Self::ok();
        r.collaboration_request = Some(request);
        r
    }
    pub(super) fn with_ask_entries(entries: Vec<AskEntry>) -> Self {
        let mut r = Self::ok();
        r.ask_entries = Some(entries);
        r
    }
    pub(super) fn with_ask_status(enabled: bool) -> Self {
        let mut response = Self::ok();
        response.ask_enabled = Some(enabled);
        response
    }
    pub(super) fn with_ask_agent(agent: String) -> Self {
        let mut r = Self::ok();
        r.ask_agent = Some(agent);
        r
    }
    pub(super) fn with_ask_entry(entry: AskEntry) -> Self {
        let mut r = Self::ok();
        r.ask_entry = Some(entry);
        r
    }
    pub(super) fn with_ask_conversations(
        conversations: Vec<AskConversation>,
        active: Option<AskConversation>,
    ) -> Self {
        let mut response = Self::ok();
        response.ask_conversations = Some(conversations);
        response.ask_conversation = active;
        response
    }
    pub(super) fn with_ask_conversation(conversation: AskConversation) -> Self {
        let mut response = Self::ok();
        response.ask_conversation = Some(conversation);
        response
    }
    /// Every successful config read or write names the daemon process that
    /// served it, so a client can anchor "saved, applies on restart" to the
    /// daemon that actually took the write rather than to a cached hello.
    pub(super) fn with_config(document: crate::config_file::ConfigDocument) -> Self {
        let mut response = Self::ok();
        response.config = Some(document);
        response.boot_id = Some(daemon_boot_id());
        response
    }

    pub(super) fn with_ask_providers(providers: Vec<AskProviderInfo>) -> Self {
        let mut response = Self::ok();
        response.ask_providers = Some(providers);
        response
    }
    pub(super) fn with_automation_rules(rules: AutomationRules) -> Self {
        let mut response = Self::ok();
        response.automation_rules = Some(rules);
        response
    }
    pub(super) fn with_automation_log(entries: Vec<AutomationLedgerEntry>) -> Self {
        let mut response = Self::ok();
        response.automation_log = Some(entries);
        response
    }
    pub(super) fn with_automation_test(report: AutomationTestReport) -> Self {
        let mut response = Self::ok();
        response.automation_test = Some(report);
        response
    }
    pub(super) fn hello(restart: Option<&RestartController>) -> Self {
        let mut r = Self::ok();
        r.min_protocol = Some(MIN_PROTOCOL_VERSION);
        r.max_protocol = Some(PROTOCOL_VERSION);
        let mut capabilities = CAPABILITIES.to_vec();
        if restart.is_some() {
            capabilities.push(RESTART_CAPABILITY);
            capabilities.push(STOP_CAPABILITY);
        }
        r.capabilities = Some(capabilities);
        r.version = Some(env!("CARGO_PKG_VERSION"));
        r.generation = restart.map(RestartController::generation);
        r.boot_id = Some(daemon_boot_id());
        r
    }
}

pub(super) const RESTART_RUNNING: u8 = 0;
pub(super) const RESTART_REQUESTED: u8 = 1;
pub(super) const RESTART_STOPPING: u8 = 2;

#[cfg(test)]
mod ask_one_shot_tests {
    use super::*;

    #[test]
    fn ask_one_shot_parses_with_optional_agent_and_credential() {
        let request: Request = serde_json::from_value(serde_json::json!({
            "protocol": crate::event::PROTOCOL_VERSION,
            "kind": "ask_one_shot",
            "prompt": "Summarize these results",
        }))
        .unwrap();
        match request.body {
            RequestBody::AskOneShot {
                prompt,
                agent,
                credential,
            } => {
                assert_eq!(prompt, "Summarize these results");
                assert!(agent.is_none() && credential.is_none());
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
