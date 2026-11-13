//! **Internal:** `#[doc(hidden)]`, for `tendd` only.
//!
//! Account swaps as daemon-owned jobs. A request is matched against the
//! jobs already accepted *before* anything is planned; a new one is planned
//! with the daemon's own configuration, accounts and panes, written down
//! whole — every entry in report order, the settled ones with their outcome —
//! and only then acknowledged. From then on the job belongs to the service:
//! it runs on its own thread, every effect journalled before it happens,
//! whatever becomes of the caller.
//!
//! The service stops taking work the moment it cannot write: a journal or
//! store failure halts the job before its next target and marks the service
//! faulted. Started or uncertain attempts are left for a person after restart;
//! untouched public targets may get fresh attempts under a bounded resume window.
//! No effect is automatically replayed (`swap_job` recovery).

mod resume;
#[cfg(test)]
mod tests;

use crate::account_swap::effects::{EffectObserver, NoObserver};
use crate::account_swap::{
    self, AccountChoice, BareMode, ContinueEvidence, ContinueWhen, PreparedTarget, Step,
    SwapConfig, SwapOutcome, SwapRequest, SwapScope, SwapStatus, TargetIdentity,
};
use crate::swap_job::{
    request_digest, AcceptedPlan, Boundary, Disposition, JobError, Lease, Liveness, OwnerIdentity,
    PlannedTarget, ResumePolicy, SettledEntry, SwapJobStore, TextRef,
};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tend::backend::SharedBackend;
use tend::event::AgentKind;
use tend::state::Agent;
pub use tend::swap_api::{
    validate_key, Accepted, EntryView, JobRef, JobView, PlanView, RetryOverrides, StillDue,
    SwapHealth, SwapJobs, SwapOverrides, SwapReadiness, UnopenedSwap,
};

/// The job store schema this build writes and reads.
pub const JOB_SCHEMA: i64 = crate::swap_job::SCHEMA_VERSION;

/// [`SwapOverrides`] lives in [`tend::swap_api`]; the module completes it.
pub trait ResolveOverrides {
    /// The request as `config` completes it.
    fn resolve(&self, config: &SwapConfig) -> Result<SwapRequest, String>;
}

impl ResolveOverrides for SwapOverrides {
    fn resolve(&self, config: &SwapConfig) -> Result<SwapRequest, String> {
        self.validate()?;
        let scope = if !self.targets.is_empty() {
            SwapScope::Targets(self.targets.clone())
        } else if self.all {
            SwapScope::All
        } else {
            SwapScope::Limited
        };
        let mut request = SwapRequest::new(scope, config);
        request.provider = match self.provider.as_deref() {
            None => None,
            Some("claude") => Some(AgentKind::ClaudeCode),
            Some("codex") => Some(AgentKind::Codex),
            Some(other) => return Err(format!("unknown provider {other:?}")),
        };
        request.to = if self.spread {
            AccountChoice::Spread
        } else {
            AccountChoice::parse(self.to.as_deref().unwrap_or("auto"))
        };
        request.force = self.force;
        request.when_idle = self.when_idle;
        if let Some(timeout) = &self.idle_timeout {
            request.idle_timeout = tend::automation::parse_duration(timeout)?
                .try_into()
                .map_err(|_| "idle_timeout must be nonnegative")?;
        }
        request.bare = match self.bare.as_deref() {
            None => request.bare,
            Some("skip") => BareMode::Skip,
            Some("wrap") => BareMode::Wrap,
            Some(other) => return Err(format!("unknown bare mode {other:?}")),
        };
        if let Some(text) = &self.continue_text {
            request.continue_text = Some(text.clone()).filter(|text| !text.trim().is_empty());
        }
        request.continue_when = if self.no_continue {
            ContinueWhen::Never
        } else {
            match self.continue_when.as_deref() {
                None => request.continue_when,
                Some("limited") => ContinueWhen::Limited,
                Some("always") => ContinueWhen::Always,
                Some("never") => ContinueWhen::Never,
                Some(other) => return Err(format!("unknown continue policy {other:?}")),
            }
        };
        request.continue_evidence = match self.continue_evidence.as_deref() {
            None => request.continue_evidence,
            Some("binding") => ContinueEvidence::Binding,
            Some("verified") => ContinueEvidence::Verified,
            Some(other) => return Err(format!("unknown continue evidence {other:?}")),
        };
        request.exclude.clone_from(&self.exclude);
        request.dry_run = false;
        Ok(request)
    }
}

/// What the daemon knows right now, as one consistent snapshot.
pub struct Snapshot {
    pub node_id: String,
    pub agents: Vec<Agent>,
    pub backends: Vec<SharedBackend>,
    pub config: SwapConfig,
    pub accounts: tend::accounts::AccountsConfig,
    /// The configuration file the daemon loaded.
    pub config_path: Option<PathBuf>,
}

/// Held while a target's pane is being acted on, so no other writer types
/// into it meanwhile.
pub trait TargetGuard: Send {
    /// Whether this guard protects `endpoint` (`pane@server`).
    fn covers(&self, endpoint: &str) -> bool;
}

/// The daemon's side: its live state and its input exclusion.
pub trait ContextProvider: Send + Sync {
    fn snapshot(&self) -> Result<Snapshot, String>;
    /// The same exclusion every other writer to `pane` takes.
    fn acquire_guard(
        &self,
        pane: &str,
        socket: Option<&str>,
    ) -> Result<Box<dyn TargetGuard>, String>;
    /// The agents' prompt hooks as they arrive, if this daemon reads them.
    fn turn_probe(&self) -> Option<Arc<dyn account_swap::TurnProbe>> {
        None
    }
}

/// The agents' prompt hooks as the daemon receives them, kept only as long
/// as a follow-up could wait for one and without their text: kind,
/// conversation and pane, in arrival order. The daemon feeds it from its
/// store; the swap executor reads it as a [`account_swap::TurnProbe`].
pub struct PromptReceipts {
    /// The store's own count of accepted prompt hooks: the cursor.
    source: Arc<std::sync::atomic::AtomicU64>,
    seen: Mutex<std::collections::VecDeque<Receipt>>,
    /// The daemon's expected-prompt registry, when it lends one.
    registry: Option<Arc<dyn PromptRegistry>>,
}

/// The daemon's expected-prompt registry (`Store::expect_prompt`): the
/// guarded prompt path registers each prompt before typing it, so the hook
/// that reports it is attributed to tend.
pub trait PromptRegistry: Send + Sync {
    fn expect(&self, pane: &str, socket: Option<&str>, text: &str) -> Option<u64>;
    fn forget(&self, token: u64);
    /// Whether `token` still waits for its hook (`Store::expected_prompt_pending`).
    fn pending(&self, token: u64) -> bool;
}

struct Receipt {
    seq: u64,
    kind: AgentKind,
    conversation: String,
    endpoint: String,
}

/// More than enough for every pane's hooks within one follow-up's wait.
const RECEIPTS_KEPT: usize = 512;

/// Ended jobs whose rich outcomes stay in memory, newest kept. An older
/// one reads as it would after a restart: its journal, without them.
const LIVE_ENDED_KEPT: usize = 64;

/// How long a cleanly ended job keeps its effects and observations. Its job
/// and target rows are kept for good.
const EVIDENCE_KEPT: time::Duration = time::Duration::days(30);

/// Jobs pruned per transaction, so pruning never holds the store long.
const PRUNE_BATCH: usize = 32;

/// Transactions per pruning run; what is left waits for the next one.
const PRUNE_BATCHES: usize = 64;

/// How often ended jobs prompt a pruning run.
const PRUNE_EVERY: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

impl PromptReceipts {
    /// Receipts numbered by `source`, the store's [`crate::Store::prompt_sequence`]:
    /// a hook the store accepted before a cursor was read is never after it,
    /// however late it is received here.
    #[must_use]
    pub fn new(source: Arc<std::sync::atomic::AtomicU64>) -> Self {
        Self {
            source,
            seen: Mutex::new(std::collections::VecDeque::new()),
            registry: None,
        }
    }

    /// The same, registering follow-ups with the daemon's `registry`.
    #[must_use]
    pub fn with_registry(mut self, registry: Arc<dyn PromptRegistry>) -> Self {
        self.registry = Some(registry);
        self
    }

    /// Note one prompt hook, by who sent it and from where, under the number
    /// the store gave it; its text is not kept.
    pub fn record(&self, record: &tend::state::PromptRecord) {
        let Ok(mut seen) = self.seen.lock() else {
            return;
        };
        let receipt = Receipt {
            seq: record.seq,
            kind: record.id.kind,
            conversation: record.id.session_id.clone(),
            endpoint: record.id.pane.as_deref().map_or_else(String::new, |pane| {
                account_swap::endpoint_key(pane, record.id.tmux_socket.as_deref())
            }),
        };
        seen.push_back(receipt);
        while seen.len() > RECEIPTS_KEPT {
            seen.pop_front();
        }
    }
}

impl account_swap::TurnProbe for PromptReceipts {
    fn cursor(&self) -> u64 {
        self.source.load(Ordering::SeqCst)
    }

    fn prompt_after(
        &self,
        cursor: u64,
        kind: AgentKind,
        conversation: &str,
        endpoint: &str,
    ) -> bool {
        self.seen.lock().is_ok_and(|seen| {
            seen.iter().any(|receipt| {
                receipt.seq > cursor
                    && receipt.kind == kind
                    && receipt.conversation == conversation
                    && receipt.endpoint == endpoint
            })
        })
    }

    fn expect_prompt(&self, pane: &str, socket: Option<&str>, text: &str) -> Option<u64> {
        self.registry.as_ref()?.expect(pane, socket, text)
    }

    fn forget_prompt(&self, token: u64) {
        if let Some(registry) = &self.registry {
            registry.forget(token);
        }
    }

    fn registration_consumed(&self, token: u64) -> bool {
        self.registry
            .as_ref()
            .is_some_and(|registry| !registry.pending(token))
    }
}

#[derive(Serialize)]
struct RetryDigest<'a> {
    retry_of: &'a str,
    overrides: &'a RetryOverrides,
}

/// A planned step: settled already, or a target to run.
#[allow(clippy::large_enum_variant)] // a plan's steps, held only while it is made
pub(crate) enum EngineStep<T> {
    Done(SwapOutcome),
    Run(T),
}

/// Planning and executing, injected so the service can be tested without a
/// pane or a process.
pub(crate) trait Engine: Send + Sync + 'static {
    type Target: Send + 'static;
    fn prepare(
        &self,
        request: &SwapRequest,
        snapshot: &Snapshot,
    ) -> (SwapRequest, Vec<EngineStep<Self::Target>>);
    fn identity(&self, target: &Self::Target) -> TargetIdentity;
    fn refresh_agent(&self, _target: &mut Self::Target, _agent: &Agent) {}
    /// Plan one target of a retry, as its parent planned it.
    fn prepare_retry(
        &self,
        request: &SwapRequest,
        planned: &PlannedTarget,
        snapshot: &Snapshot,
    ) -> (SwapRequest, EngineStep<Self::Target>);
    /// A changed binding is final; an unavailable read must remain deferred.
    fn prepare_resume(
        &self,
        request: &SwapRequest,
        planned: &PlannedTarget,
        snapshot: &Snapshot,
    ) -> (SwapRequest, EngineStep<Self::Target>, bool) {
        let (request, step) = self.prepare_retry(request, planned, snapshot);
        (request, step, false)
    }
    /// Only a complete backend scan can prove that its pane has gone.
    fn pane_gone(&self, _planned: &PlannedTarget, _snapshot: &Snapshot) -> bool {
        false
    }
    /// Look at a target's pane now, read-only: a reason code.
    fn observe(&self, planned: &PlannedTarget, snapshot: &Snapshot) -> &'static str;
    fn execute(
        &self,
        request: &SwapRequest,
        target: &Self::Target,
        observer: &mut dyn EffectObserver,
    ) -> SwapOutcome;
}

fn registered_target<'a>(
    agents: &'a [Agent],
    conversation: &str,
    pane: &str,
    endpoint: &str,
) -> Option<&'a Agent> {
    agents.iter().find(|agent| {
        agent.session_id == conversation
            && agent.state != tend::event::AgentState::Stopped
            && agent.pane.as_deref() == Some(pane)
            && agent
                .tmux_socket
                .as_deref()
                .is_none_or(|socket| account_swap::endpoint_key(pane, Some(socket)) == endpoint)
    })
}

fn pane_absent(
    pane: &str,
    socket: Option<&str>,
    observation: &tend::backend::PaneObservation,
) -> bool {
    observation.is_complete()
        && !observation.panes.iter().any(|found| {
            found.pane_id == pane
                && found.socket.as_deref().is_none_or(|found_socket| {
                    socket.is_none_or(|socket| {
                        account_swap::endpoint_key(pane, Some(socket))
                            == account_swap::endpoint_key(pane, Some(found_socket))
                    })
                })
        })
}

/// The real thing: `account_swap`.
#[derive(Default)]
pub(crate) struct RealEngine {
    /// The daemon's prompt hooks, for a follow-up's evidence.
    probe: Option<Arc<dyn account_swap::TurnProbe>>,
}

impl Engine for RealEngine {
    type Target = Box<PreparedTarget>;

    fn prepare(
        &self,
        request: &SwapRequest,
        snapshot: &Snapshot,
    ) -> (SwapRequest, Vec<EngineStep<Self::Target>>) {
        let (resolved, steps) = account_swap::prepare(
            request,
            &snapshot.agents,
            &snapshot.backends,
            &snapshot.config,
            &snapshot.accounts,
        );
        let steps = steps
            .into_iter()
            .map(|step| match step {
                Step::Done(outcome) => EngineStep::Done(outcome),
                Step::Run(target) => EngineStep::Run(target),
            })
            .collect();
        (resolved, steps)
    }

    fn identity(&self, target: &Self::Target) -> TargetIdentity {
        target.identity()
    }

    fn refresh_agent(&self, target: &mut Self::Target, agent: &Agent) {
        target.refresh_agent(agent);
    }

    fn prepare_retry(
        &self,
        request: &SwapRequest,
        planned: &PlannedTarget,
        snapshot: &Snapshot,
    ) -> (SwapRequest, EngineStep<Self::Target>) {
        let (resolved, step) = account_swap::prepare_retry(
            request,
            &account_swap::RetryTarget {
                endpoint: &planned.endpoint,
                conversation_id: &planned.conversation_id,
                process_pid: planned.process_pid,
                process_start: planned.process_start.as_deref(),
            },
            &snapshot.agents,
            &snapshot.backends,
            &snapshot.config,
            &snapshot.accounts,
        );
        let step = match step {
            Step::Done(outcome) => EngineStep::Done(outcome),
            Step::Run(target) => EngineStep::Run(target),
        };
        (resolved, step)
    }

    fn prepare_resume(
        &self,
        request: &SwapRequest,
        planned: &PlannedTarget,
        snapshot: &Snapshot,
    ) -> (SwapRequest, EngineStep<Self::Target>, bool) {
        let (resolved, step, changed) = account_swap::prepare_retry_classified(
            request,
            &account_swap::RetryTarget {
                endpoint: &planned.endpoint,
                conversation_id: &planned.conversation_id,
                process_pid: planned.process_pid,
                process_start: planned.process_start.as_deref(),
            },
            &snapshot.agents,
            &snapshot.backends,
            &snapshot.config,
            &snapshot.accounts,
        );
        let step = match step {
            Step::Done(outcome) => EngineStep::Done(outcome),
            Step::Run(target) => EngineStep::Run(target),
        };
        (resolved, step, changed)
    }

    fn pane_gone(&self, planned: &PlannedTarget, snapshot: &Snapshot) -> bool {
        let kind = tend::backend::pane_id_host_kind(&planned.pane);
        snapshot
            .backends
            .iter()
            .filter(|backend| Some(backend.kind()) == kind)
            .any(|backend| {
                pane_absent(
                    &planned.pane,
                    planned.socket.as_deref(),
                    &backend.observe_panes(),
                )
            })
    }

    fn observe(&self, planned: &PlannedTarget, snapshot: &Snapshot) -> &'static str {
        account_swap::observe_target(
            &planned.pane,
            planned.socket.as_deref(),
            provider_kind(&planned.provider),
            &planned.conversation_id,
            planned.assigned_account.as_deref(),
            &snapshot.backends,
            &snapshot.accounts,
        )
    }

    fn execute(
        &self,
        request: &SwapRequest,
        target: &Self::Target,
        observer: &mut dyn EffectObserver,
    ) -> SwapOutcome {
        account_swap::execute_target(request, target, observer, self.probe.as_deref())
    }
}

fn provider_kind(provider: &str) -> AgentKind {
    match provider {
        "codex" => AgentKind::Codex,
        "claude" => AgentKind::ClaudeCode,
        _ => AgentKind::Unknown,
    }
}

/// What a [`SwapService`] runs: `account_swap`, or, for a daemon's own
/// tests, a stand-in that touches nothing.
pub(crate) enum DaemonEngine {
    Real(RealEngine),
    #[cfg(feature = "test-support")]
    Inert(testing::InertEngine),
}

pub(crate) enum DaemonTarget {
    Real(Box<PreparedTarget>),
    #[cfg(feature = "test-support")]
    Inert(TargetIdentity),
}

impl Engine for DaemonEngine {
    type Target = DaemonTarget;

    fn prepare(
        &self,
        request: &SwapRequest,
        snapshot: &Snapshot,
    ) -> (SwapRequest, Vec<EngineStep<DaemonTarget>>) {
        match self {
            Self::Real(engine) => {
                let (resolved, steps) = engine.prepare(request, snapshot);
                let steps = steps
                    .into_iter()
                    .map(|step| match step {
                        EngineStep::Done(outcome) => EngineStep::Done(outcome),
                        EngineStep::Run(target) => EngineStep::Run(DaemonTarget::Real(target)),
                    })
                    .collect();
                (resolved, steps)
            }
            #[cfg(feature = "test-support")]
            Self::Inert(_) => testing::InertEngine::prepare(request, snapshot),
        }
    }

    fn identity(&self, target: &DaemonTarget) -> TargetIdentity {
        match target {
            DaemonTarget::Real(target) => target.identity(),
            #[cfg(feature = "test-support")]
            DaemonTarget::Inert(identity) => identity.clone(),
        }
    }

    fn refresh_agent(&self, target: &mut Self::Target, agent: &Agent) {
        match (self, target) {
            (Self::Real(engine), DaemonTarget::Real(target)) => engine.refresh_agent(target, agent),
            #[cfg(feature = "test-support")]
            _ => {}
        }
    }

    fn prepare_retry(
        &self,
        request: &SwapRequest,
        planned: &PlannedTarget,
        snapshot: &Snapshot,
    ) -> (SwapRequest, EngineStep<DaemonTarget>) {
        match self {
            Self::Real(engine) => {
                let (resolved, step) = engine.prepare_retry(request, planned, snapshot);
                let step = match step {
                    EngineStep::Done(outcome) => EngineStep::Done(outcome),
                    EngineStep::Run(target) => EngineStep::Run(DaemonTarget::Real(target)),
                };
                (resolved, step)
            }
            #[cfg(feature = "test-support")]
            Self::Inert(_) => testing::InertEngine::prepare_retry(request, planned, snapshot),
        }
    }

    fn prepare_resume(
        &self,
        request: &SwapRequest,
        planned: &PlannedTarget,
        snapshot: &Snapshot,
    ) -> (SwapRequest, EngineStep<DaemonTarget>, bool) {
        match self {
            Self::Real(engine) => {
                let (resolved, step, changed) = engine.prepare_resume(request, planned, snapshot);
                let step = match step {
                    EngineStep::Done(outcome) => EngineStep::Done(outcome),
                    EngineStep::Run(target) => EngineStep::Run(DaemonTarget::Real(target)),
                };
                (resolved, step, changed)
            }
            #[cfg(feature = "test-support")]
            Self::Inert(_) => {
                let (request, step) =
                    testing::InertEngine::prepare_retry(request, planned, snapshot);
                (request, step, false)
            }
        }
    }

    fn pane_gone(&self, planned: &PlannedTarget, snapshot: &Snapshot) -> bool {
        match self {
            Self::Real(engine) => engine.pane_gone(planned, snapshot),
            #[cfg(feature = "test-support")]
            Self::Inert(_) => false,
        }
    }

    fn observe(&self, planned: &PlannedTarget, snapshot: &Snapshot) -> &'static str {
        match self {
            Self::Real(engine) => engine.observe(planned, snapshot),
            #[cfg(feature = "test-support")]
            Self::Inert(_) => "observed_unknown",
        }
    }

    fn execute(
        &self,
        request: &SwapRequest,
        target: &DaemonTarget,
        observer: &mut dyn EffectObserver,
    ) -> SwapOutcome {
        match (self, target) {
            (Self::Real(engine), DaemonTarget::Real(target)) => {
                engine.execute(request, target, observer)
            }
            #[cfg(feature = "test-support")]
            (Self::Inert(engine), DaemonTarget::Inert(identity)) => engine.execute(identity),
            #[cfg(feature = "test-support")]
            _ => unreachable!("a target is only ever run by the engine that planned it"),
        }
    }
}

/// The daemon's handle.
pub struct SwapService {
    inner: Arc<Service<DaemonEngine>>,
}

impl SwapService {
    /// Open the default store, become its one manager, and recover what a
    /// previous manager left. `None` path: the default.
    pub fn open(path: Option<&Path>, provider: Arc<dyn ContextProvider>) -> Result<Self, String> {
        let path = path
            .map(Path::to_path_buf)
            .or_else(SwapJobStore::default_path)
            .ok_or("no data directory for the swap job store")?;
        let engine = RealEngine {
            probe: provider.turn_probe(),
        };
        Self::open_with(&path, provider, DaemonEngine::Real(engine))
    }

    fn open_with(
        path: &Path,
        provider: Arc<dyn ContextProvider>,
        engine: DaemonEngine,
    ) -> Result<Self, String> {
        let lock = ownership_lock(path)?;
        let identity = OwnerIdentity {
            nonce: uuid::Uuid::new_v4().to_string(),
            pid: std::process::id(),
            process_start: owner_start(std::process::id()),
        };
        // Holding the lock, whoever the store names as owner is gone.
        let inner = Service::open(path, identity, &|_, _| Liveness::Gone, engine, provider)
            .map_err(|error| error.to_string())?
            .holding(lock);
        let inner = Arc::new(inner);
        // Only the dispatcher can fail to launch here, before any resumed
        // effect. Later admission failures fault the visible service handle.
        Service::start_resumer(&inner)?;
        Ok(Self { inner })
    }

    /// Whether a submit would be taken now: the store is still this
    /// manager's, its file is still the one opened, and nothing faulted.
    pub fn active(&self) -> Result<(), String> {
        self.inner.active()
    }

    /// The daemon's own automation: a swap it decided on and already holds
    /// the target's input guard for. Unlike [`SwapJobs::submit`] it may act
    /// on a pane that runs its agent directly, exactly as automation always
    /// has, since it runs in the daemon's own environment either way.
    ///
    /// `still_due` is the automation's own word that the swap is still
    /// wanted (same rule revision, same agent episode, condition still
    /// holding). It is asked again once the plan is made, before the job is
    /// accepted, and once more right before its target is started: a queue
    /// or a slow plan never turns a stale firing into a stop.
    pub fn submit_automation(
        &self,
        request_key: &str,
        overrides: &SwapOverrides,
        guard: Option<Box<dyn TargetGuard>>,
        still_due: Option<StillDue>,
    ) -> Result<Accepted, String> {
        Service::submit_checked(
            &self.inner,
            request_key,
            overrides,
            Origin::Automation,
            guard,
            still_due,
        )
    }

    /// Wait for a job to end and read it, all within `timeout`. A timeout
    /// only ends the wait: the job runs on, and its status says where it is.
    pub async fn wait(&self, job: &str, timeout: std::time::Duration) -> Result<JobView, String> {
        Service::wait(Arc::clone(&self.inner), job, timeout).await
    }

    /// Stop taking new jobs and wait, within `timeout`, for every submission
    /// already past the gate and every running job to end. Nothing running
    /// is interrupted. `false` if the deadline came first.
    pub async fn drain(&self, timeout: std::time::Duration) -> bool {
        Service::drain(Arc::clone(&self.inner), timeout).await
    }
}

impl SwapJobs for SwapService {
    fn accounts(&self) -> Result<Vec<crate::account_provider::ProviderCandidates>, String> {
        let snapshot = self.inner.provider.snapshot()?;
        Ok(["claude", "codex"]
            .into_iter()
            .map(|provider| {
                crate::account_provider::candidates(
                    provider,
                    &snapshot.accounts,
                    &snapshot.config.aas,
                )
            })
            .collect())
    }

    fn active(&self) -> Result<(), String> {
        self.inner.active()
    }

    fn plan(&self, overrides: &SwapOverrides) -> Result<PlanView, String> {
        self.inner.plan(overrides, Origin::Public)
    }

    fn submit(&self, request_key: &str, overrides: &SwapOverrides) -> Result<Accepted, String> {
        Service::submit(&self.inner, request_key, overrides, Origin::Public, None)
    }

    fn retry(
        &self,
        request_key: &str,
        parent: &JobRef,
        overrides: &RetryOverrides,
    ) -> Result<Accepted, String> {
        Service::retry(&self.inner, request_key, parent, overrides)
    }

    fn cancel(&self, job: &JobRef) -> Result<JobView, String> {
        self.inner.cancel(job)
    }

    fn health(&self) -> SwapHealth {
        self.inner.health()
    }

    fn status(&self, job: &JobRef) -> Result<JobView, String> {
        self.inner.status(job)
    }
}

/// Who asked for a swap. Minted by the daemon, never taken from a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    /// Anyone over IPC.
    Public,
    /// The daemon's own automation.
    Automation,
}

/// A pane on its server, socket spellings normalized: what a
/// [`TargetGuard`] is asked to cover.
#[must_use]
pub fn endpoint(pane: &str, socket: Option<&str>) -> String {
    account_swap::endpoint_key(pane, socket)
}

/// The key one automation firing submits under: the same logical firing —
/// its node, pane, conversation, rule and episode — is the same key however
/// often it is delivered or re-armed, and a later episode is another.
#[must_use]
pub fn automation_request_key(
    node_id: &str,
    pane: &str,
    socket: Option<&str>,
    agent_session_id: &str,
    rule: &str,
    episode: &str,
) -> String {
    let identity = serde_json::json!([
        node_id,
        account_swap::endpoint_key(pane, socket),
        agent_session_id,
        rule,
        episode,
    ]);
    let digest = crate::swap_job::request_digest(&identity);
    format!("automation-{}", &digest[..32])
}

/// This process's start identity, recorded with its ownership for the
/// record only: who may take the store over is decided by the lock.
#[cfg(target_os = "linux")]
fn owner_start(pid: u32) -> Option<String> {
    // Counted from boot, so scoped to that boot.
    let start = tend::agent_conversation::process_start(pid)?;
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    Some(format!("{}:{start}", boot.trim()))
}

#[cfg(not(target_os = "linux"))]
fn owner_start(pid: u32) -> Option<String> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let wanted = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[wanted]),
        true,
        ProcessRefreshKind::nothing(),
    );
    // A process that could not be read comes back with a zero start: that
    // is no identity at all.
    system
        .process(wanted)
        .map(sysinfo::Process::start_time)
        .filter(|start| *start > 0)
        .map(|start| format!("epoch:{start}"))
}

/// Hold the store's ownership lock: an exclusive lock on `<store>.lock`,
/// kept for the manager's whole life. The OS drops it when the manager
/// exits however it exits, so holding it is the proof that no other
/// manager is alive — the one thing a process-table lookup cannot prove.
fn ownership_lock(store: &Path) -> Result<std::fs::File, String> {
    if let Some(parent) = store.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options
        .open(store.with_extension("lock"))
        .map_err(|error| error.to_string())?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(std::fs::TryLockError::WouldBlock) => {
            Err("swap jobs are owned by another live manager".into())
        }
        Err(std::fs::TryLockError::Error(error)) => Err(format!(
            "could not take the swap job ownership lock: {error}"
        )),
    }
}

/// Starts a job's thread; injected so a failure to start is testable.
type Spawner = Box<dyn Fn(String, Box<dyn FnOnce() + Send>) -> std::io::Result<()> + Send + Sync>;

// Each job owns an OS thread: idle polling and blocking pane I/O never run
// on a Tokio worker. The manager lease belongs to the service, not one job.
fn thread_spawner() -> Spawner {
    Box::new(|name, work| {
        std::thread::Builder::new()
            .name(name)
            .spawn(work)
            .map(|_| ())
    })
}

#[derive(Debug, Default)]
struct LiveJob {
    outcomes: BTreeMap<usize, SwapOutcome>,
}

pub(crate) struct Service<E: Engine> {
    store: SwapJobStore,
    lease: Lease,
    engine: E,
    provider: Arc<dyn ContextProvider>,
    /// Held from the key lookup to the spawn, so two sends of one request
    /// make one job and one executor. Never held while a job runs.
    submit: Mutex<()>,
    /// Endpoints a job is acting on or will act on. An endpoint whose last
    /// effect is uncertain stays reserved until the daemon restarts.
    reserved: Mutex<HashMap<String, String>>,
    live: Mutex<HashMap<String, LiveJob>>,
    /// Ended jobs still in `live`, oldest first.
    ended_live: Mutex<std::collections::VecDeque<String>>,
    /// When evidence was last pruned; held by the one run doing it.
    pruned_at: Mutex<Option<std::time::Instant>>,
    /// Why the last pruning failed, if it did.
    maintenance: Mutex<Option<String>>,
    fault: Mutex<Option<String>>,
    spawner: Spawner,
    /// The ownership lock ([`ownership_lock`]). Owned here, where every
    /// running job holds a reference, so it is released only once the
    /// manager and its last executor are both gone — never while work
    /// that was accepted under it still runs.
    #[allow(dead_code)] // held for its lifetime, never read
    ownership: Option<std::fs::File>,
    /// Per running job: flips to `true` when it ends, however it ends.
    done: Mutex<HashMap<String, tokio::sync::watch::Sender<bool>>>,
    /// Shutting down: no new job is taken.
    draining: AtomicBool,
    /// One dispatcher at a time; workers restart it when they defer again.
    resumer_running: AtomicBool,
}

/// A persisted attempt, independent of its position in a resumed queue.
struct AttemptRun<T> {
    target_id: u32,
    attempt: u32,
    report_index: usize,
    identity: TargetIdentity,
    target: T,
}

impl<E: Engine> Service<E> {
    pub(crate) fn open(
        path: &Path,
        me: OwnerIdentity,
        liveness: &dyn Fn(u32, Option<&str>) -> Liveness,
        engine: E,
        provider: Arc<dyn ContextProvider>,
    ) -> Result<Self, JobError> {
        let store = SwapJobStore::open(path)?;
        let lease = store.acquire(&me, liveness)?;
        store.recover(&lease)?;
        let service = Self {
            store,
            lease,
            engine,
            provider,
            submit: Mutex::new(()),
            reserved: Mutex::new(HashMap::new()),
            live: Mutex::new(HashMap::new()),
            ended_live: Mutex::new(std::collections::VecDeque::new()),
            pruned_at: Mutex::new(Some(std::time::Instant::now())),
            maintenance: Mutex::new(None),
            fault: Mutex::new(None),
            spawner: thread_spawner(),
            ownership: None,
            done: Mutex::new(HashMap::new()),
            draining: AtomicBool::new(false),
            resumer_running: AtomicBool::new(false),
        };
        // Upkeep a restart brings due at once: old evidence goes before any
        // job is taken.
        service.prune_evidence();
        Ok(service)
    }

    /// Keep `lock` for as long as this service, and any job running under
    /// it, lives.
    pub(crate) fn holding(mut self, lock: std::fs::File) -> Self {
        self.ownership = Some(lock);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_spawner(mut self, spawner: Spawner) -> Self {
        self.spawner = spawner;
        self
    }

    pub(crate) fn active(&self) -> Result<(), String> {
        if self.draining.load(Ordering::SeqCst) {
            return Err("swap jobs are shutting down".into());
        }
        if let Some(fault) = self.fault() {
            return Err(format!("swap jobs are halted: {fault}"));
        }
        self.store
            .check(&self.lease)
            .map_err(|error| format!("swap jobs are unavailable: {error}"))
    }

    /// How the service is now, read-only.
    pub(crate) fn health(&self) -> SwapHealth {
        let fault = self.fault();
        let (readiness, reason) = if self.draining.load(Ordering::SeqCst) {
            (SwapReadiness::Draining, Some("shutting down".to_owned()))
        } else if let Some(fault) = &fault {
            (SwapReadiness::Halted, Some(fault.clone()))
        } else if let Err(error) = self.store.check(&self.lease) {
            (SwapReadiness::Unavailable, Some(error.to_string()))
        } else {
            (SwapReadiness::Ready, None)
        };
        SwapHealth {
            readiness,
            reason,
            schema: Some(crate::swap_job::SCHEMA_VERSION),
            expected_schema: crate::swap_job::SCHEMA_VERSION,
            fault,
            jobs_running: self.done.lock().map_or(0, |done| done.len()),
            maintenance: self.maintenance.lock().ok().and_then(|m| m.clone()),
        }
    }

    pub(crate) async fn wait(
        this: Arc<Self>,
        job: &str,
        timeout: std::time::Duration,
    ) -> Result<JobView, String> {
        let deadline = tokio::time::Instant::now() + timeout;
        let ended = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&ended);
        let job_ref = JobRef::Id(job.to_owned());
        let read = tokio::time::timeout_at(deadline, async move {
            if let Some(mut done) = this.receiver(job) {
                let _ = done.wait_for(|ended| *ended).await;
            }
            seen.store(true, Ordering::SeqCst);
            tokio::task::spawn_blocking(move || this.status(&job_ref))
                .await
                .map_err(|error| error.to_string())?
        })
        .await;
        match read {
            Ok(view) => view,
            Err(_) if ended.load(Ordering::SeqCst) => Err(format!(
                "job {job} has ended, but its status could not be read within {}s",
                timeout.as_secs()
            )),
            Err(_) => Err(format!(
                "job {job} is still running after {}s",
                timeout.as_secs()
            )),
        }
    }

    pub(crate) async fn drain(this: Arc<Self>, timeout: std::time::Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        this.draining.store(true, Ordering::SeqCst);
        let draining = Arc::clone(&this);
        let begin =
            tokio::task::spawn_blocking(move || draining.store.begin_drain(&draining.lease));
        if !matches!(
            tokio::time::timeout_at(deadline, begin).await,
            Ok(Ok(Ok(())))
        ) {
            this.set_fault("could not durably defer pending swap targets on shutdown".into());
            return false;
        }
        // A submission holds the submit lock from its gate to its worker's
        // registration; taking the lock once means every one that passed the
        // gate before the flag has registered or given up.
        let gate = Arc::clone(&this);
        let barrier = tokio::task::spawn_blocking(move || {
            drop(gate.submit.lock());
        });
        if tokio::time::timeout_at(deadline, barrier).await.is_err() {
            return false;
        }
        // Catch submissions that passed the gate before the flag and registered after the first sweep.
        let draining = Arc::clone(&this);
        let sweep =
            tokio::task::spawn_blocking(move || draining.store.begin_drain(&draining.lease));
        if !matches!(
            tokio::time::timeout_at(deadline, sweep).await,
            Ok(Ok(Ok(())))
        ) {
            this.set_fault("could not settle in-flight submissions on shutdown".into());
            return false;
        }
        let waiting = this.receivers();
        tokio::time::timeout_at(deadline, async move {
            for mut done in waiting {
                let _ = done.wait_for(|ended| *ended).await;
            }
        })
        .await
        .is_ok()
    }

    /// A watch on a running job's end; `None` once it has ended or was
    /// never run here.
    pub(crate) fn receiver(&self, job: &str) -> Option<tokio::sync::watch::Receiver<bool>> {
        self.done
            .lock()
            .ok()
            .and_then(|done| done.get(job).map(tokio::sync::watch::Sender::subscribe))
    }

    fn receivers(&self) -> Vec<tokio::sync::watch::Receiver<bool>> {
        self.done.lock().map_or_else(
            |_| Vec::new(),
            |done| {
                done.values()
                    .map(tokio::sync::watch::Sender::subscribe)
                    .collect()
            },
        )
    }

    /// The job ended, however it ended: wake whoever waits on it, and let
    /// the oldest ended job's outcomes go once more than
    /// [`LIVE_ENDED_KEPT`] have.
    fn ended(&self, job: &str) {
        // The cache settles first: whoever is woken reads it as it stays.
        if let (Ok(mut ended), Ok(mut live)) = (self.ended_live.lock(), self.live.lock()) {
            if live.contains_key(job) && !ended.iter().any(|kept| kept == job) {
                ended.push_back(job.to_owned());
            }
            while ended.len() > LIVE_ENDED_KEPT {
                if let Some(oldest) = ended.pop_front() {
                    live.remove(&oldest);
                }
            }
        }
        if let Ok(mut done) = self.done.lock() {
            if let Some(sender) = done.remove(job) {
                let _ = sender.send(true);
            }
        }
        // After the wake: upkeep never delays whoever waits on the job.
        self.prune_when_due();
    }

    /// Prune old evidence if a day has passed since the last run and no
    /// other run holds the claim.
    fn prune_when_due(&self) {
        let Ok(mut last) = self.pruned_at.try_lock() else {
            return;
        };
        if last.is_some_and(|at| at.elapsed() < PRUNE_EVERY) {
            return;
        }
        *last = Some(std::time::Instant::now());
        self.prune_evidence();
    }

    /// Prune the evidence of jobs that ended cleanly more than
    /// [`EVIDENCE_KEPT`] ago, in short batches. A failure is kept as the
    /// service's maintenance health, never as a fault.
    fn prune_evidence(&self) -> usize {
        let cutoff = (time::OffsetDateTime::now_utc() - EVIDENCE_KEPT)
            .format(&time::format_description::well_known::Rfc3339);
        let mut pruned = 0;
        let failure = match cutoff {
            Err(error) => Some(error.to_string()),
            Ok(cutoff) => (0..PRUNE_BATCHES)
                .find_map(|_| {
                    match self.store.prune_evidence(&self.lease, &cutoff, PRUNE_BATCH) {
                        Ok(count) => {
                            pruned += count;
                            // A short batch: nothing is left.
                            (count < PRUNE_BATCH).then_some(None)
                        }
                        Err(error) => Some(Some(error.to_string())),
                    }
                })
                .flatten(),
        };
        if let Ok(mut maintenance) = self.maintenance.lock() {
            *maintenance =
                failure.map(|error| format!("pruning old swap job evidence failed: {error}"));
        }
        pruned
    }

    fn fault(&self) -> Option<String> {
        self.fault.lock().ok().and_then(|fault| fault.clone())
    }

    fn set_fault(&self, why: String) {
        if let Ok(mut fault) = self.fault.lock() {
            fault.get_or_insert(why);
        }
    }

    /// A snapshot planned against the configuration the caller loaded.
    fn snapshot_for(&self, overrides: &SwapOverrides) -> Result<Snapshot, String> {
        self.snapshot_with(overrides.caller_config.as_deref())
    }

    /// The daemon's snapshot, refused if the caller planned with another
    /// configuration file than the daemon runs.
    fn snapshot_with(&self, caller_config: Option<&Path>) -> Result<Snapshot, String> {
        let snapshot = self.provider.snapshot()?;
        if let Some(caller) = caller_config {
            let same = snapshot
                .config_path
                .as_deref()
                .is_some_and(|daemon| same_path(caller, daemon));
            if !same {
                return Err(format!(
                    "this request was made with {}, but the daemon runs {}; \
                     swaps run with the daemon's configuration",
                    caller.display(),
                    snapshot.config_path.as_deref().map_or_else(
                        || "no configuration file".into(),
                        |p| p.display().to_string()
                    )
                ));
            }
        }
        Ok(snapshot)
    }

    /// What a submit would do, without doing any of it.
    pub(crate) fn plan(
        &self,
        overrides: &SwapOverrides,
        origin: Origin,
    ) -> Result<PlanView, String> {
        let snapshot = self.snapshot_for(overrides)?;
        let mut request = overrides.resolve(&snapshot.config)?;
        request.dry_run = true;
        request.allow_direct_respawn = origin == Origin::Automation;
        let (resolved, steps) = self.engine.prepare(&request, &snapshot);
        // Plan-only whatever was asked: the dry run returns before any effect.
        let mut resolved = resolved;
        resolved.dry_run = true;
        let mut direct_targets = Vec::new();
        let outcomes = steps
            .into_iter()
            .map(|step| match step {
                EngineStep::Done(outcome) => outcome,
                EngineStep::Run(target) => {
                    let identity = self.engine.identity(&target);
                    match refusal(&identity, origin) {
                        Some((code, note)) => {
                            if code == DIRECT_PANE_UNSUPPORTED
                                && (!resolved.when_idle || resolved.force)
                            {
                                direct_targets.push(account_swap::DirectTarget {
                                    pane: identity.pane.clone(),
                                    socket: identity.socket.clone(),
                                    endpoint: identity.endpoint.clone(),
                                    conversation_id: identity.conversation_id.clone(),
                                });
                            }
                            skipped_outcome(&identity, note)
                        }
                        None => self.engine.execute(&resolved, &target, &mut NoObserver),
                    }
                }
            })
            .collect();
        let unavailable = self.active().err();
        Ok(PlanView {
            outcomes,
            direct_targets,
            executable: unavailable.is_none(),
            unavailable,
        })
    }

    pub(crate) fn submit(
        this: &Arc<Self>,
        request_key: &str,
        overrides: &SwapOverrides,
        origin: Origin,
        guard: Option<Box<dyn TargetGuard>>,
    ) -> Result<Accepted, String> {
        Self::submit_checked(this, request_key, overrides, origin, guard, None)
    }

    /// [`Self::submit`], with the caller's `still_due` asked once the plan
    /// is made and again before the target starts.
    pub(crate) fn submit_checked(
        this: &Arc<Self>,
        request_key: &str,
        overrides: &SwapOverrides,
        origin: Origin,
        guard: Option<Box<dyn TargetGuard>>,
        still_due: Option<StillDue>,
    ) -> Result<Accepted, String> {
        validate_key(request_key)?;
        overrides.validate()?;
        let digest = request_digest(overrides);
        let _one_at_a_time = this
            .submit
            .lock()
            .map_err(|_| "swap job submission lock poisoned".to_owned())?;
        this.active()?;
        if let Some(job) = this
            .store
            .lookup(request_key, &digest)
            .map_err(|error| error.to_string())?
        {
            let view = this.status(&JobRef::Id(job.clone()))?;
            return Ok(Accepted {
                job_id: job,
                existing: true,
                view,
            });
        }
        let snapshot = this.snapshot_for(overrides)?;
        let mut request = overrides.resolve(&snapshot.config)?;
        request.allow_direct_respawn = origin == Origin::Automation;
        let (resolved, steps) = this.engine.prepare(&request, &snapshot);

        let (settled, notes, runs, taken) = this.split(request_key, steps, origin);
        // Planning may have waited behind other submissions: still wanted?
        if let Some(check) = &still_due {
            if let Err(why) = check() {
                this.release_all(&taken);
                return Err(format!("no longer due: {why}"));
            }
        }
        // An adopted guard is for one pane: a job it does not cover exactly
        // would have to take that pane's guard again while holding it.
        if let Some(guard) = &guard {
            let covered =
                matches!(runs.as_slice(), [(_, identity, _)] if guard.covers(&identity.endpoint));
            if !covered {
                this.release_all(&taken);
                return Err(
                    "the input guard held for this request does not cover its one target pane"
                        .into(),
                );
            }
        }
        let plan = AcceptedPlan {
            resume: Some(ResumePolicy {
                public: origin == Origin::Public,
                when_idle: resolved.when_idle,
                idle_deadline: time::OffsetDateTime::now_utc()
                    .unix_timestamp()
                    .saturating_add(
                        i64::try_from(resolved.idle_timeout.as_secs()).unwrap_or(i64::MAX),
                    ),
                restore_draft: resolved.restore_draft,
                continue_when: resolved.continue_when,
                continue_evidence: resolved.continue_evidence,
                bare: resolved.bare,
                force: resolved.force,
                configured_text: overrides.continue_text.is_none(),
            }),
            node_id: snapshot.node_id.clone(),
            continue_when: resolved.continue_when.to_string(),
            continue_text: resolved.continue_text.as_deref().map(TextRef::of),
            targets: runs
                .iter()
                .map(|(report_index, identity, _)| planned_target(*report_index, identity))
                .collect(),
            settled,
        };
        let created = match this
            .store
            .create_job(&this.lease, request_key, &digest, &plan)
        {
            Ok(created) => created,
            Err(error) => {
                this.release_all(&taken);
                return Err(error.to_string());
            }
        };
        if created.existing {
            // The same request accepted meanwhile: that job, not a second one.
            this.release_all(&taken);
            let view = this.status(&JobRef::Id(created.job_id.clone()))?;
            return Ok(Accepted {
                job_id: created.job_id,
                existing: true,
                view,
            });
        }
        Self::run_accepted(
            this,
            created.job_id,
            resolved,
            runs,
            guard,
            plan.targets.len(),
            &taken,
            notes,
            still_due,
        )
    }

    /// Start a job just written and hand it to its executor.
    #[allow(clippy::too_many_arguments)] // one accepted job's whole hand-over
    fn run_accepted(
        this: &Arc<Self>,
        job: String,
        resolved: SwapRequest,
        runs: Vec<(usize, TargetIdentity, E::Target)>,
        guard: Option<Box<dyn TargetGuard>>,
        targets: usize,
        taken: &[String],
        notes: Vec<(usize, SwapOutcome)>,
        still_due: Option<StillDue>,
    ) -> Result<Accepted, String> {
        if let Err(error) = this.store.start(&this.lease, &job) {
            this.release_all(taken);
            this.set_fault(format!("could not start job {job}: {error}"));
            return Err(error.to_string());
        }
        if let Ok(mut done) = this.done.lock() {
            done.insert(job.clone(), tokio::sync::watch::channel(false).0);
        }
        if let Ok(mut live) = this.live.lock() {
            live.insert(
                job.clone(),
                LiveJob {
                    outcomes: notes.into_iter().collect(),
                },
            );
        }
        let runs = runs
            .into_iter()
            .enumerate()
            .map(|(id, (report_index, identity, target))| AttemptRun {
                target_id: u32::try_from(id).unwrap_or(u32::MAX),
                attempt: 1,
                report_index,
                identity,
                target,
            })
            .collect();
        Self::spawn(this, &job, resolved, runs, guard, targets, taken, still_due)?;
        let view = this.status(&JobRef::Id(job.clone()))?;
        Ok(Accepted {
            job_id: job,
            existing: false,
            view,
        })
    }

    /// Retry the targets of an ended job that can be retried safely, as a
    /// new job under `request_key`.
    ///
    /// A target is run again only if it failed before anything at or past
    /// its relaunch was even intended, with no effect left uncertain, and
    /// the very process its job planned (same pid and start) still runs on
    /// the same endpoint and conversation: then nothing replaced it, and
    /// the swap starts from where it was. Its account is the one first
    /// assigned. No follow-up is typed and no draft restored: both were
    /// another run's. Every other target that did not end swapped, kept or
    /// skipped is only looked at, read-only, and reported as found.
    #[allow(clippy::too_many_lines)] // the whole acceptance, in order
    pub(crate) fn retry(
        this: &Arc<Self>,
        request_key: &str,
        parent: &JobRef,
        overrides: &RetryOverrides,
    ) -> Result<Accepted, String> {
        validate_key(request_key)?;
        overrides.validate()?;
        let parent = this.status(parent)?.job_id;
        // What makes a resend the same retry: the parent and what was typed,
        // never the eligibility read now, which a first send changes.
        let digest = request_digest(&RetryDigest {
            retry_of: &parent,
            overrides,
        });
        let _one_at_a_time = this
            .submit
            .lock()
            .map_err(|_| "swap job submission lock poisoned".to_owned())?;
        this.active()?;
        if let Some(job) = this
            .store
            .lookup(request_key, &digest)
            .map_err(|error| error.to_string())?
        {
            let view = this.status(&JobRef::Id(job.clone()))?;
            return Ok(Accepted {
                job_id: job,
                existing: true,
                view,
            });
        }
        let sources = this
            .store
            .retry_sources(&parent)
            .map_err(|error| error.to_string())?;
        let plan = this
            .store
            .plan(&parent)
            .map_err(|error| error.to_string())?;
        let snapshot = this.snapshot_with(overrides.caller_config.as_deref())?;
        let mut settled: Vec<SettledEntry> = plan
            .settled
            .iter()
            .map(|entry| SettledEntry {
                disposition: Disposition::Skipped,
                reason: "not_retried".into(),
                ..entry.clone()
            })
            .collect();
        let mut notes = Vec::new();
        let mut runs = Vec::new();
        let mut links = Vec::new();
        let mut taken = Vec::new();
        let mut resolved = None;
        for source in &sources {
            let Some(planned) = usize::try_from(source.target_id)
                .ok()
                .and_then(|index| plan.targets.get(index))
            else {
                continue;
            };
            let settle = |reason: &str| SettledEntry {
                report_index: planned.report_index,
                pane: Some(planned.pane.clone()),
                agent_session_id: planned.conversation_id.clone(),
                disposition: Disposition::Skipped,
                reason: reason.into(),
            };
            if let Some(by) = &source.retried_by {
                let _ = by;
                settled.push(settle("retried_elsewhere"));
                continue;
            }
            let done = matches!(
                source.disposition.as_deref(),
                Some("swapped" | "kept" | "skipped")
            );
            if done {
                settled.push(settle("not_retried"));
                continue;
            }
            let rerun = matches!(source.disposition.as_deref(), Some("failed" | "deferred"))
                && !source.launched
                && source.uncertain_effects == 0;
            if !this.reserve(&planned.endpoint, request_key) {
                settled.push(settle("busy"));
                continue;
            }
            if !rerun {
                // Looked at under the reservation, so no job acting on it
                // can make this read misleading; nothing is done to it.
                let found = this.engine.observe(planned, &snapshot);
                this.release(&planned.endpoint);
                settled.push(settle(found));
                continue;
            }
            let Some(account) = planned.assigned_account.clone() else {
                this.release(&planned.endpoint);
                settled.push(settle("no_assigned_account"));
                continue;
            };
            let mut request = SwapRequest::new(
                SwapScope::Targets(vec![planned.conversation_id.clone()]),
                &snapshot.config,
            );
            request.to = AccountChoice::Named(account);
            request.force = overrides.force;
            request.continue_when = ContinueWhen::Never;
            request.continue_text = None;
            request.restore_draft = false;
            request.allow_direct_respawn = false;
            if let Some(evidence) = overrides.evidence()? {
                request.continue_evidence = evidence;
            }
            let (request, step) = this.engine.prepare_retry(&request, planned, &snapshot);
            match step {
                EngineStep::Run(target) => {
                    let identity = this.engine.identity(&target);
                    if let Some((code, note)) = refusal(&identity, Origin::Public) {
                        this.release(&planned.endpoint);
                        settled.push(settle(code));
                        notes.push((planned.report_index, skipped_outcome(&identity, note)));
                        continue;
                    }
                    links.push(crate::swap_job::RetryLink {
                        target_index: runs.len(),
                        parent_target: source.target_id,
                        parent_attempt: source.attempt,
                        lineage: source.lineage.clone(),
                    });
                    taken.push(planned.endpoint.clone());
                    runs.push((planned.report_index, identity, target));
                    resolved.get_or_insert(request);
                }
                EngineStep::Done(outcome) => {
                    this.release(&planned.endpoint);
                    settled.push(settled_entry(planned.report_index, &outcome));
                    notes.push((planned.report_index, outcome));
                }
            }
        }
        settled.sort_by_key(|entry| entry.report_index);
        let accepted = AcceptedPlan {
            resume: resolved.as_ref().map(|request: &SwapRequest| ResumePolicy {
                public: true,
                when_idle: request.when_idle,
                idle_deadline: time::OffsetDateTime::now_utc()
                    .unix_timestamp()
                    .saturating_add(
                        i64::try_from(request.idle_timeout.as_secs()).unwrap_or(i64::MAX),
                    ),
                restore_draft: request.restore_draft,
                continue_when: request.continue_when,
                continue_evidence: request.continue_evidence,
                bare: request.bare,
                force: request.force,
                configured_text: true,
            }),
            node_id: snapshot.node_id.clone(),
            continue_when: ContinueWhen::Never.to_string(),
            continue_text: None,
            targets: runs
                .iter()
                .map(|(report_index, identity, _)| planned_target(*report_index, identity))
                .collect(),
            settled,
        };
        let created = match this.store.create_retry_job(
            &this.lease,
            request_key,
            &digest,
            &accepted,
            &parent,
            &links,
        ) {
            Ok(created) => created,
            Err(error) => {
                this.release_all(&taken);
                return Err(error.to_string());
            }
        };
        if created.existing {
            this.release_all(&taken);
            let view = this.status(&JobRef::Id(created.job_id.clone()))?;
            return Ok(Accepted {
                job_id: created.job_id,
                existing: true,
                view,
            });
        }
        let resolved = resolved.unwrap_or_else(|| {
            let mut request = SwapRequest::new(SwapScope::Targets(Vec::new()), &snapshot.config);
            request.continue_when = ContinueWhen::Never;
            request
        });
        Self::run_accepted(
            this,
            created.job_id,
            resolved,
            runs,
            None,
            accepted.targets.len(),
            &taken,
            notes,
            None,
        )
    }

    /// Settled steps and the targets to run, each target's endpoint
    /// reserved; a pane already reserved settles as busy.
    /// Settled entries (a reason code each, their notes kept apart for
    /// memory only) and the targets to run, each target's endpoint reserved;
    /// a pane already reserved, or on a server that cannot be told, settles.
    #[allow(clippy::type_complexity)] // the parts of one split
    fn split(
        &self,
        owner: &str,
        steps: Vec<EngineStep<E::Target>>,
        origin: Origin,
    ) -> (
        Vec<SettledEntry>,
        Vec<(usize, SwapOutcome)>,
        Vec<(usize, TargetIdentity, E::Target)>,
        Vec<String>,
    ) {
        let mut settled = Vec::new();
        let mut notes = Vec::new();
        let mut runs = Vec::new();
        let mut taken = Vec::new();
        for (report_index, step) in steps.into_iter().enumerate() {
            match step {
                EngineStep::Done(outcome) => {
                    settled.push(settled_entry(report_index, &outcome));
                    notes.push((report_index, outcome));
                }
                EngineStep::Run(target) => {
                    let identity = self.engine.identity(&target);
                    let refused = if let Some(refused) = refusal(&identity, origin) {
                        Some(refused)
                    } else if self.reserve(&identity.endpoint, owner) {
                        None
                    } else {
                        Some((
                            "busy",
                            "another swap job has reserved this pane (possibly waiting for idle)",
                        ))
                    };
                    if let Some((code, note)) = refused {
                        settled.push(SettledEntry {
                            report_index,
                            pane: Some(identity.pane.clone()),
                            agent_session_id: identity.conversation_id.clone(),
                            disposition: Disposition::Skipped,
                            reason: code.into(),
                        });
                        notes.push((report_index, skipped_outcome(&identity, note)));
                    } else {
                        taken.push(identity.endpoint.clone());
                        runs.push((report_index, identity, target));
                    }
                }
            }
        }
        (settled, notes, runs, taken)
    }

    /// Hand an accepted job to its own thread. If the thread cannot start,
    /// every target ends with no effect and the job is recorded as ended.
    #[allow(clippy::too_many_arguments)] // one accepted job's whole hand-over
    fn spawn(
        this: &Arc<Self>,
        job: &str,
        request: SwapRequest,
        runs: Vec<AttemptRun<E::Target>>,
        guard: Option<Box<dyn TargetGuard>>,
        _targets: usize,
        taken: &[String],
        still_due: Option<StillDue>,
    ) -> Result<(), String> {
        let attempts: Vec<_> = runs
            .iter()
            .map(|run| (run.target_id, run.attempt))
            .collect();
        let worker = Arc::clone(this);
        let worker_job = job.to_owned();
        let work: Box<dyn FnOnce() + Send> = Box::new(move || {
            // A panic must not leave a job running under a manager that
            // still reports itself healthy.
            let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                worker.run(&worker_job, &request, runs, guard, still_due.as_ref());
            }));
            if ran.is_err() {
                worker.set_fault(format!(
                    "job {worker_job}: the swap executor panicked; no further target is started"
                ));
            }
            // Ended — finished, halted or panicked — whoever waits is woken.
            worker.ended(&worker_job);
            if let Err(error) = Self::start_resumer(&worker) {
                worker.set_fault(format!("could not restart swap resumer: {error}"));
            }
        });
        let name = format!("swap-{}", &job[..job.len().min(13)]);
        if let Err(error) = (this.spawner)(name, work) {
            // Nothing ran: every target ends with no effect.
            let mut recorded = Ok(());
            for (target, attempt) in attempts {
                let settled = if attempt > 1 {
                    this.store.defer_attempt(
                        &this.lease,
                        job,
                        target,
                        attempt,
                        "worker_unavailable",
                    )
                } else {
                    this.store
                        .finish_target(&this.lease, job, target, attempt, Disposition::Failed)
                };
                if let Err(error) = settled {
                    recorded = Err(error);
                }
            }
            if recorded.is_ok() {
                recorded = this.store.finish(&this.lease, job);
            }
            match recorded {
                Ok(()) => this.release_all(taken),
                Err(store) => this.set_fault(format!(
                    "job {job}: the executor could not start and its end could not be recorded: {store}"
                )),
            }
            this.ended(job);
            return Err(format!("the swap executor could not start: {error}"));
        }
        Ok(())
    }

    /// Run targets in report order, rotating busy targets while waiting for idle.
    /// Halt the whole job on the first failure to record.
    #[allow(clippy::too_many_lines)] // boundaries, idle waiting, execution and journalling in order
    fn run(
        &self,
        job: &str,
        request: &SwapRequest,
        runs: Vec<AttemptRun<E::Target>>,
        adopted: Option<Box<dyn TargetGuard>>,
        still_due: Option<&StillDue>,
    ) {
        let mut adopted = adopted;
        let began = std::time::Instant::now();
        let mut pending: std::collections::VecDeque<_> = runs.into();
        let accepted = match self.store.plan(job) {
            Ok(plan) => plan,
            Err(error) => {
                self.set_fault(format!(
                    "job {job}: could not read its accepted policy: {error}"
                ));
                return;
            }
        };
        let deadline = accepted.resume.as_ref().map(|policy| policy.idle_deadline);
        let mut next_poll = std::time::Instant::now();
        while let Some(AttemptRun {
            target_id,
            attempt,
            report_index,
            identity,
            mut target,
        }) = pending.pop_front()
        {
            if self.fault().is_some() {
                // Halted: later targets are never reached, their pane stays
                // reserved until the daemon restarts and recovers the job.
                break;
            }
            // A cancelled job starts nothing more, without waiting on a pane.
            match self.boundary(job, target_id, attempt, false) {
                Some(Boundary::Deferred) => {
                    self.release(&identity.endpoint);
                    continue;
                }
                Some(Boundary::Cancelled) => {
                    self.cancelled(job, report_index, &identity);
                    continue;
                }
                Some(_) => {}
                None => break,
            }
            if request.when_idle
                && !request.force
                && deadline
                    .is_some_and(|until| time::OffsetDateTime::now_utc().unix_timestamp() >= until)
            {
                self.timed_out(job, target_id, attempt, report_index, &identity);
                continue;
            }
            // An adopted guard protects the one endpoint it was taken for.
            let held = match adopted.take() {
                Some(guard) if guard.covers(&identity.endpoint) => Ok(guard),
                other => {
                    adopted = other;
                    self.provider
                        .acquire_guard(&identity.pane, identity.socket.as_deref())
                }
            };
            let guard = match held {
                Ok(guard) => guard,
                Err(_) if attempt > 1 => {
                    self.defer_run(job, target_id, attempt, &identity, "guard_unavailable");
                    continue;
                }
                Err(why) => {
                    self.settle(
                        job,
                        target_id,
                        attempt,
                        report_index,
                        &identity,
                        Disposition::Skipped,
                        None,
                        &why,
                    );
                    continue;
                }
            };
            if request.when_idle && !request.force {
                let fresh = self.provider.snapshot().map(|snapshot| {
                    if attempt > 1
                        && usize::try_from(target_id)
                            .ok()
                            .and_then(|id| accepted.targets.get(id))
                            .is_some_and(|planned| self.engine.pane_gone(planned, &snapshot))
                    {
                        return None;
                    }
                    registered_target(
                        &snapshot.agents,
                        &identity.conversation_id,
                        &identity.pane,
                        &identity.endpoint,
                    )
                    .cloned()
                });
                match fresh {
                    Ok(Some(agent)) if account_swap::waiting_for_idle(&agent) => {
                        if deadline.map_or_else(
                            || began.elapsed() >= request.idle_timeout,
                            |until| time::OffsetDateTime::now_utc().unix_timestamp() >= until,
                        ) {
                            self.timed_out(job, target_id, attempt, report_index, &identity);
                        } else {
                            // Release the input exclusion while waiting. Keep only
                            // the per-endpoint reservation, to reject duplicate swaps;
                            // neither unrelated endpoints nor submissions are held.
                            drop(guard);
                            pending.push_back(AttemptRun {
                                target_id,
                                attempt,
                                report_index,
                                identity,
                                target,
                            });
                            // Rotate the queue, so a busy target never blocks ready targets.
                            // Bound CPU use and cancellation latency to one second per round.
                            if pending
                                .front()
                                .is_some_and(|run| run.target_id <= target_id)
                            {
                                let now = std::time::Instant::now();
                                if now < next_poll {
                                    std::thread::sleep(next_poll - now);
                                }
                                next_poll =
                                    std::time::Instant::now() + std::time::Duration::from_secs(1);
                            }
                        }
                        continue;
                    }
                    Ok(Some(agent)) => self.engine.refresh_agent(&mut target, &agent),
                    Ok(None) => {
                        self.target_gone(job, target_id, attempt, report_index, &identity);
                        continue;
                    }
                    Err(_) if attempt > 1 => {
                        self.defer_run(job, target_id, attempt, &identity, "snapshot_unavailable");
                        continue;
                    }
                    Err(why) => {
                        self.settle(
                            job,
                            target_id,
                            attempt,
                            report_index,
                            &identity,
                            Disposition::Skipped,
                            None,
                            &why,
                        );
                        continue;
                    }
                }
            }
            // The pane is held: the last moment a stale automation firing
            // can still step back, with nothing done.
            if let Some(Err(why)) = still_due.map(|check| check()) {
                self.settle(
                    job,
                    target_id,
                    attempt,
                    report_index,
                    &identity,
                    Disposition::Skipped,
                    None,
                    &format!("no longer due: {why}; nothing was done"),
                );
                continue;
            }
            // The pane is held: start it, unless a cancel came first. From
            // here on it runs to its end.
            match self.boundary(job, target_id, attempt, true) {
                Some(Boundary::Started) => {}
                Some(Boundary::Deferred) => {
                    self.release(&identity.endpoint);
                    continue;
                }
                Some(_) => {
                    self.cancelled(job, report_index, &identity);
                    continue;
                }
                None => break,
            }
            let mut journal = self.store.journal(&self.lease, job, target_id, attempt);
            let outcome = self.engine.execute(request, &target, &mut journal);
            let uncertain = journal.uncertain();
            let failed = journal.failed();
            let effects = journal.effects();
            drop(journal);
            let disposition = if uncertain {
                Disposition::NeedsReconciliation
            } else {
                match outcome.status {
                    SwapStatus::Swapped => Disposition::Swapped,
                    SwapStatus::Failed => Disposition::Failed,
                    // A skip after effects is a session deliberately kept,
                    // which may still have been typed into.
                    SwapStatus::Skipped if effects > 0 => Disposition::Kept,
                    SwapStatus::Skipped | SwapStatus::Planned => Disposition::Skipped,
                }
            };
            if failed || uncertain {
                self.set_fault(format!(
                    "job {job}: the journal could not record target {target_id}; \
                     no further target is started"
                ));
            }
            let recorded =
                self.store
                    .finish_target(&self.lease, job, target_id, attempt, disposition);
            if let Err(error) = &recorded {
                self.set_fault(format!(
                    "job {job}: could not record target {target_id}: {error}"
                ));
            }
            self.remember(job, report_index, outcome);
            if recorded.is_ok() && !uncertain && !failed {
                self.release(&identity.endpoint);
            }
        }
        if self.fault().is_none() {
            if let Err(error) = self.store.finish(&self.lease, job) {
                self.set_fault(format!("job {job}: could not record its end: {error}"));
            }
        }
    }

    fn target_gone(
        &self,
        job: &str,
        target: u32,
        attempt: u32,
        report_index: usize,
        identity: &TargetIdentity,
    ) {
        if let Err(error) = self.store.finish_target_with_reason(
            &self.lease,
            job,
            target,
            attempt,
            Disposition::Skipped,
            Some("target_gone"),
        ) {
            self.set_fault(format!(
                "job {job}: could not settle absent target {target}: {error}"
            ));
            return;
        }
        self.remember(
            job,
            report_index,
            skipped_outcome(identity, "target gone; nothing was done"),
        );
        self.release(&identity.endpoint);
    }

    fn defer_run(
        &self,
        job: &str,
        target: u32,
        attempt: u32,
        identity: &TargetIdentity,
        reason: &str,
    ) {
        match self
            .store
            .defer_attempt(&self.lease, job, target, attempt, reason)
        {
            Ok(()) => self.release(&identity.endpoint),
            Err(error) => self.set_fault(format!(
                "job {job}: could not defer target {target}: {error}"
            )),
        }
    }

    fn timed_out(
        &self,
        job: &str,
        target_id: u32,
        attempt: u32,
        report_index: usize,
        identity: &TargetIdentity,
    ) {
        if let Err(error) = self.store.finish_target_with_reason(
            &self.lease,
            job,
            target_id,
            attempt,
            Disposition::Skipped,
            Some("idle_timeout"),
        ) {
            self.set_fault(format!("job {job}: could not record idle timeout: {error}"));
            return;
        }
        self.remember(
            job,
            report_index,
            skipped_outcome(
                identity,
                "idle timeout; target still pending; nothing was done",
            ),
        );
        self.release(&identity.endpoint);
    }

    /// One target boundary; `None` (the service now halted) if it could not
    /// be recorded: a cancel that cannot be read is never taken as none.
    fn boundary(&self, job: &str, target_id: u32, attempt: u32, start: bool) -> Option<Boundary> {
        match self
            .store
            .start_or_cancel(&self.lease, job, target_id, attempt, start)
        {
            Ok(boundary) => Some(boundary),
            Err(error) => {
                self.set_fault(format!(
                    "job {job}: could not record the boundary of target {target_id}: {error}"
                ));
                None
            }
        }
    }

    /// A target a cancel kept from starting: reported, its pane let go.
    fn cancelled(&self, job: &str, report_index: usize, identity: &TargetIdentity) {
        self.remember(
            job,
            report_index,
            SwapOutcome {
                note: "cancelled before it started; nothing was done".into(),
                ..skipped_outcome(identity, "")
            },
        );
        self.release(&identity.endpoint);
    }

    /// Ask `job` to start no further target. A target already started runs
    /// to its end; nothing running is interrupted or signalled. Accepted
    /// whatever the service's health, so a halted or draining daemon can
    /// still be told; an ended job is left as it is.
    pub(crate) fn cancel(&self, job: &JobRef) -> Result<JobView, String> {
        let job = self.status(job)?.job_id;
        self.store
            .request_cancel(&self.lease, &job)
            .map_err(|error| error.to_string())?;
        self.status(&JobRef::Id(job))
    }

    #[allow(clippy::too_many_arguments)] // one settled target, all of its identity
    fn settle(
        &self,
        job: &str,
        target_id: u32,
        attempt: u32,
        report_index: usize,
        identity: &TargetIdentity,
        disposition: Disposition,
        outcome: Option<SwapOutcome>,
        why: &str,
    ) {
        if let Err(error) =
            self.store
                .finish_target(&self.lease, job, target_id, attempt, disposition)
        {
            self.set_fault(format!(
                "job {job}: could not record target {target_id}: {error}"
            ));
            return;
        }
        let outcome = outcome.unwrap_or_else(|| SwapOutcome {
            pane: Some(identity.pane.clone()),
            agent_session_id: identity.conversation_id.clone(),
            kind: AgentKind::Unknown,
            from: identity.from_account.clone(),
            to: identity.assigned_account.clone(),
            status: SwapStatus::Skipped,
            command: None,
            note: why.to_owned(),
            continue_reason: None,
            session_id_source: Some(identity.session_id_source),
            launch_id: None,
            conversation_verified: Some(identity.conversation_verified),
            conversation_unverified_reason: None,
            registered: None,
            continue_withheld: None,
            follow_up: None,
        });
        self.remember(job, report_index, outcome);
        self.release(&identity.endpoint);
    }

    fn remember(&self, job: &str, report_index: usize, outcome: SwapOutcome) {
        if let Ok(mut live) = self.live.lock() {
            live.entry(job.to_owned())
                .or_default()
                .outcomes
                .insert(report_index, outcome);
        }
    }

    fn reserve(&self, endpoint: &str, owner: &str) -> bool {
        let Ok(mut reserved) = self.reserved.lock() else {
            return false;
        };
        if reserved.contains_key(endpoint) {
            return false;
        }
        reserved.insert(endpoint.to_owned(), owner.to_owned());
        true
    }

    fn release(&self, endpoint: &str) {
        if let Ok(mut reserved) = self.reserved.lock() {
            reserved.remove(endpoint);
        }
    }

    fn release_all(&self, endpoints: &[String]) {
        for endpoint in endpoints {
            self.release(endpoint);
        }
    }

    pub(crate) fn status(&self, job: &JobRef) -> Result<JobView, String> {
        let status = match job {
            JobRef::Id(id) => self.store.status(id),
            JobRef::Key(key) => self.store.status_by_key(key),
        }
        .map_err(|error| error.to_string())?;
        let plan = self
            .store
            .plan(&status.job_id)
            .map_err(|error| error.to_string())?;
        let live = self
            .live
            .lock()
            .ok()
            .and_then(|live| live.get(&status.job_id).map(|job| job.outcomes.clone()))
            .unwrap_or_default();
        let mut entries: Vec<EntryView> = plan
            .settled
            .iter()
            .map(|entry| EntryView {
                report_index: entry.report_index,
                pane: entry.pane.clone(),
                disposition: Some(entry.disposition.to_string()),
                reason: Some(entry.reason.clone()),
                uncertain_effects: 0,
                outcome: live.get(&entry.report_index).cloned(),
                continuation: None,
                follow_up: None,
            })
            .collect();
        for target in &status.targets {
            let Some(planned) = usize::try_from(target.target_id)
                .ok()
                .and_then(|id| plan.targets.get(id))
            else {
                continue;
            };
            entries.push(EntryView {
                report_index: planned.report_index,
                pane: Some(planned.pane.clone()),
                disposition: target.disposition.clone(),
                reason: target.reason.clone(),
                uncertain_effects: target.uncertain_effects,
                outcome: live.get(&planned.report_index).cloned(),
                continuation: target.continuation.clone(),
                follow_up: target.follow_up.clone(),
            });
        }
        entries.sort_by_key(|entry| entry.report_index);
        let unfinished = matches!(status.state.as_str(), "accepted" | "running" | "deferred")
            || entries
                .iter()
                .any(|entry| entry.disposition.as_deref() == Some("deferred"));
        Ok(JobView {
            job_id: status.job_id,
            node_id: status.node_id,
            state: status.state,
            revision: status.revision,
            reason: status.reason,
            entries,
            fault: self.fault().filter(|_| unfinished),
            service_fault: self.fault(),
            cancel_requested: status.cancel_requested,
            evidence_pruned_at: status.evidence_pruned_at,
        })
    }
}

/// Why a target is refused before anything is reserved: a server that
/// cannot be told, or — for a public request — a pane running its agent
/// directly, whose respawn would run in the daemon's environment instead of
/// the shell the caller had.
const DIRECT_PANE_UNSUPPORTED: &str = "direct_pane_unsupported";

fn refusal(identity: &TargetIdentity, origin: Origin) -> Option<(&'static str, &'static str)> {
    if identity.socket.is_none() {
        return Some(("unknown_server", "cannot tell which server this pane is on"));
    }
    if identity.direct && origin == Origin::Public {
        return Some((
            DIRECT_PANE_UNSUPPORTED,
            "this pane runs its agent directly; swaps through the daemon do not respawn such \
             panes yet",
        ));
    }
    None
}

fn settled_entry(report_index: usize, outcome: &SwapOutcome) -> SettledEntry {
    SettledEntry {
        report_index,
        pane: outcome.pane.clone(),
        agent_session_id: outcome.agent_session_id.clone(),
        disposition: match outcome.status {
            SwapStatus::Failed => Disposition::Failed,
            _ => Disposition::Skipped,
        },
        reason: reason_code(outcome).into(),
    }
}

/// A bounded code for a planning outcome. The note itself can quote an
/// external tool's output, so it never goes to disk.
fn reason_code(outcome: &SwapOutcome) -> &'static str {
    const CODES: &[(&str, &str)] = &[
        ("excluded", "excluded"),
        ("mid-turn", "mid_turn"),
        ("active in the last", "recently_active"),
        ("not registered by its own hooks", "unregistered"),
        ("identity conflict", "identity_conflict"),
        ("changed while it was being read", "identity_unstable"),
        ("no provider session id", "no_session_id"),
        ("more than one live pane", "ambiguous_target"),
        ("no live pane", "unresolved_target"),
        ("no agent tend knows", "no_agent"),
        ("account switching is not supported", "provider_unsupported"),
        ("not in a multiplexer pane", "no_pane"),
    ];
    if outcome.status == SwapStatus::Failed {
        return "candidates_unavailable";
    }
    CODES
        .iter()
        .find(|(needle, _)| outcome.note.contains(needle))
        .map_or("other", |(_, code)| code)
}

/// The report row for a target refused before it ran.
fn skipped_outcome(identity: &TargetIdentity, note: &str) -> SwapOutcome {
    SwapOutcome {
        pane: Some(identity.pane.clone()),
        agent_session_id: identity.conversation_id.clone(),
        kind: AgentKind::Unknown,
        from: identity.from_account.clone(),
        to: identity.assigned_account.clone(),
        status: SwapStatus::Skipped,
        command: None,
        note: note.to_owned(),
        continue_reason: None,
        session_id_source: Some(identity.session_id_source),
        launch_id: None,
        conversation_verified: Some(identity.conversation_verified),
        conversation_unverified_reason: None,
        registered: None,
        continue_withheld: None,
        follow_up: None,
    }
}

fn planned_target(report_index: usize, identity: &TargetIdentity) -> PlannedTarget {
    PlannedTarget {
        report_index,
        endpoint: identity.endpoint.clone(),
        host: tend::backend::pane_id_host_kind(&identity.pane)
            .map_or_else(String::new, |host| host.to_string()),
        socket: identity.socket.clone(),
        pane: identity.pane.clone(),
        provider: identity.provider.clone(),
        conversation_id: identity.conversation_id.clone(),
        session_id_source: identity.session_id_source.to_string(),
        conversation_verified: identity.conversation_verified,
        process_pid: identity.process_pid,
        process_start: identity.process_start.clone(),
        from_account: identity.from_account.clone(),
        assigned_account: identity.assigned_account.clone(),
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    let canonical =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canonical(left) == canonical(right)
}

/// For a daemon's own tests: a real service, journal and guard handling
/// included, over an engine that plans and "runs" targets without touching
/// any pane, process or account.
#[cfg(feature = "test-support")]
pub mod testing {
    use super::{
        ContextProvider, DaemonTarget, EngineStep, Path, Snapshot, SwapRequest, SwapService,
    };
    use crate::account_swap::{
        endpoint_key, SessionIdSource, SwapOutcome, SwapScope, SwapStatus, TargetIdentity,
    };
    use std::sync::Arc;

    pub(crate) struct InertEngine {
        run: Arc<dyn Fn() + Send + Sync>,
    }

    impl InertEngine {
        /// Every named target the snapshot knows (by session id or pane)
        /// is one runnable target; anything else is skipped.
        pub(crate) fn prepare(
            request: &SwapRequest,
            snapshot: &Snapshot,
        ) -> (SwapRequest, Vec<EngineStep<DaemonTarget>>) {
            let SwapScope::Targets(targets) = &request.scope else {
                return (request.clone(), Vec::new());
            };
            let steps = targets
                .iter()
                .map(|wanted| {
                    let found = snapshot.agents.iter().find(|agent| {
                        agent.session_id == *wanted || agent.pane.as_deref() == Some(wanted)
                    });
                    match found.and_then(|agent| Some((agent, agent.pane.clone()?))) {
                        Some((agent, pane)) => {
                            let socket = agent.tmux_socket.clone();
                            EngineStep::Run(DaemonTarget::Inert(TargetIdentity {
                                endpoint: endpoint_key(&pane, socket.as_deref()),
                                pane,
                                socket,
                                provider: agent.kind.to_string(),
                                conversation_id: agent.session_id.clone(),
                                session_id_source: SessionIdSource::Registry,
                                conversation_verified: true,
                                process_pid: 0,
                                process_start: None,
                                from_account: None,
                                assigned_account: None,
                                direct: false,
                            }))
                        }
                        None => EngineStep::Done(outcome(
                            None,
                            wanted,
                            SwapStatus::Skipped,
                            "no such agent",
                        )),
                    }
                })
                .collect();
            (request.clone(), steps)
        }

        /// A retried target runs again if the snapshot still has its agent on
        /// its pane.
        pub(crate) fn prepare_retry(
            request: &SwapRequest,
            planned: &super::PlannedTarget,
            snapshot: &Snapshot,
        ) -> (SwapRequest, EngineStep<DaemonTarget>) {
            let mut request = request.clone();
            request.scope = SwapScope::Targets(vec![planned.conversation_id.clone()]);
            let (request, mut steps) = Self::prepare(&request, snapshot);
            let step = steps.pop().unwrap_or_else(|| {
                EngineStep::Done(outcome(
                    Some(&planned.pane),
                    &planned.conversation_id,
                    SwapStatus::Skipped,
                    "no such agent",
                ))
            });
            (request, step)
        }

        /// Calls `run`, then reports the target swapped. No effect is made.
        pub(crate) fn execute(&self, identity: &TargetIdentity) -> SwapOutcome {
            (self.run)();
            outcome(
                Some(&identity.pane),
                &identity.conversation_id,
                SwapStatus::Swapped,
                "inert",
            )
        }
    }

    fn outcome(pane: Option<&str>, session: &str, status: SwapStatus, note: &str) -> SwapOutcome {
        SwapOutcome {
            pane: pane.map(str::to_owned),
            agent_session_id: session.to_owned(),
            kind: tend::event::AgentKind::ClaudeCode,
            from: None,
            to: None,
            status,
            command: None,
            note: note.into(),
            continue_reason: None,
            session_id_source: None,
            launch_id: None,
            conversation_verified: None,
            conversation_unverified_reason: None,
            registered: None,
            continue_withheld: None,
            follow_up: None,
        }
    }

    /// A service on `path` whose every target runs `run` on the job's own
    /// thread and nothing else.
    ///
    /// # Errors
    /// As [`SwapService::open`].
    pub fn open_inert(
        path: &Path,
        provider: Arc<dyn ContextProvider>,
        run: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<SwapService, String> {
        SwapService::open_with(
            path,
            provider,
            super::DaemonEngine::Inert(InertEngine { run }),
        )
    }
}
