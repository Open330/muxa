//! **Internal:** `#[doc(hidden)]`, for `muxad` only.
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
//! faulted. Nothing is retried or replayed; a restarted daemon finds the job
//! unfinished and leaves it for a person (`swap_job` recovery).

#[cfg(test)]
mod tests;

use crate::account_swap::effects::{EffectObserver, NoObserver};
use crate::account_swap::{
    self, AccountChoice, BareMode, ContinueWhen, PreparedTarget, Step, SwapConfig, SwapOutcome,
    SwapRequest, SwapScope, SwapStatus, TargetIdentity,
};
use crate::backend::SharedBackend;
use crate::event::AgentKind;
use crate::state::Agent;
use crate::swap_job::{
    request_digest, AcceptedPlan, Disposition, JobError, Lease, Liveness, OwnerIdentity,
    PlannedTarget, SettledEntry, SwapJobStore, TextRef,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A swap request exactly as the caller made it: only what it set, nothing
/// filled in from configuration. Its digest identifies a repeated request,
/// so changing a default between two sends of the same request does not
/// make it a different one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)] // independent request switches, as on the CLI
pub struct SwapOverrides {
    pub targets: Vec<String>,
    pub all: bool,
    pub limited: bool,
    pub provider: Option<String>,
    pub to: Option<String>,
    pub spread: bool,
    pub force: bool,
    pub bare: Option<String>,
    pub continue_text: Option<String>,
    pub continue_when: Option<String>,
    pub no_continue: bool,
    pub exclude: Vec<String>,
    /// The configuration file the caller loaded, if it named one. A request
    /// planned against a different file than the daemon's is refused rather
    /// than silently run on the daemon's.
    pub caller_config: Option<PathBuf>,
}

impl SwapOverrides {
    /// The request as `config` completes it.
    pub fn resolve(&self, config: &SwapConfig) -> Result<SwapRequest, String> {
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
    pub accounts: crate::accounts::AccountsConfig,
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
}

/// A job by id, or by the request key that made it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobRef {
    Id(String),
    Key(String),
}

/// One report row of a job, in the order the request reported it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryView {
    pub report_index: usize,
    pub pane: Option<String>,
    /// `None` while the target is still to run, or was never reached.
    pub disposition: Option<String>,
    pub reason: Option<String>,
    /// Effects whose result was never recorded: they may or may not have
    /// happened.
    pub uncertain_effects: u32,
    /// The executor's own report, while the daemon that ran it holds it.
    pub outcome: Option<SwapOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobView {
    pub job_id: String,
    pub node_id: String,
    pub state: String,
    pub revision: u64,
    pub reason: Option<String>,
    pub entries: Vec<EntryView>,
    /// Why the service stopped taking work, if it did.
    pub fault: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Accepted {
    pub job_id: String,
    /// The request key was seen before: this is the job it made then.
    pub existing: bool,
    pub view: JobView,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanView {
    pub outcomes: Vec<SwapOutcome>,
    /// Whether a submit would be taken now.
    pub executable: bool,
    pub unavailable: Option<String>,
}

/// A planned step: settled already, or a target to run.
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
    fn execute(
        &self,
        request: &SwapRequest,
        target: &Self::Target,
        observer: &mut dyn EffectObserver,
    ) -> SwapOutcome;
}

/// The real thing: `account_swap`.
pub(crate) struct RealEngine;

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

    fn execute(
        &self,
        request: &SwapRequest,
        target: &Self::Target,
        observer: &mut dyn EffectObserver,
    ) -> SwapOutcome {
        account_swap::execute_target(request, target, observer)
    }
}

/// The daemon's handle.
pub struct SwapService {
    inner: Arc<Service<RealEngine>>,
}

impl SwapService {
    /// Open the default store, become its one manager, and recover what a
    /// previous manager left. `None` path: the default.
    pub fn open(path: Option<&Path>, provider: Arc<dyn ContextProvider>) -> Result<Self, String> {
        let path = path
            .map(Path::to_path_buf)
            .or_else(SwapJobStore::default_path)
            .ok_or("no data directory for the swap job store")?;
        let identity = OwnerIdentity {
            nonce: uuid::Uuid::new_v4().to_string(),
            pid: std::process::id(),
            process_start: owner_start(std::process::id()),
        };
        let inner = Service::open(&path, identity, &owner_liveness, RealEngine, provider)
            .map_err(|error| error.to_string())?;
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// Whether a submit would be taken now: the store is still this
    /// manager's, its file is still the one opened, and nothing faulted.
    pub fn active(&self) -> Result<(), String> {
        self.inner.active()
    }

    pub fn plan(&self, overrides: &SwapOverrides) -> Result<PlanView, String> {
        self.inner.plan(overrides)
    }

    /// Accept a request as a job, or return the job its key already made.
    /// `guard` is one the caller already holds for its single target (an
    /// automation firing); the job adopts it for that target only.
    pub fn submit(
        &self,
        request_key: &str,
        overrides: &SwapOverrides,
        guard: Option<Box<dyn TargetGuard>>,
    ) -> Result<Accepted, String> {
        Service::submit(&self.inner, request_key, overrides, guard)
    }

    pub fn status(&self, job: &JobRef) -> Result<JobView, String> {
        self.inner.status(job)
    }
}

/// A process's start in a form that also names the boot it belongs to: a
/// start time counted from boot means nothing across a reboot.
fn owner_start(pid: u32) -> Option<String> {
    let start = crate::agent_conversation::process_start(pid)?;
    let boot = boot_id()?;
    Some(format!("{boot}:{start}"))
}

fn boot_id() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .ok()
            .map(|id| id.trim().to_owned())
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Whether a recorded owner still runs. Gone only on positive evidence: its
/// pid absent from a readable process table, or both start identities known
/// and different. Anything unreadable counts as alive.
fn owner_liveness(pid: u32, recorded: Option<&str>) -> Liveness {
    #[cfg(target_os = "linux")]
    {
        let proc_root = Path::new("/proc");
        if proc_root.join("self").exists() && !proc_root.join(pid.to_string()).exists() {
            return Liveness::Gone;
        }
    }
    match (recorded, owner_start(pid)) {
        (Some(recorded), Some(current)) if recorded != current => Liveness::Gone,
        _ => Liveness::Alive,
    }
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
    fault: Mutex<Option<String>>,
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
        Ok(Self {
            store,
            lease,
            engine,
            provider,
            submit: Mutex::new(()),
            reserved: Mutex::new(HashMap::new()),
            live: Mutex::new(HashMap::new()),
            fault: Mutex::new(None),
        })
    }

    pub(crate) fn active(&self) -> Result<(), String> {
        if let Some(fault) = self.fault() {
            return Err(format!("swap jobs are halted: {fault}"));
        }
        self.store
            .check(&self.lease)
            .map_err(|error| format!("swap jobs are unavailable: {error}"))
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
        let snapshot = self.provider.snapshot()?;
        if let Some(caller) = &overrides.caller_config {
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
    pub(crate) fn plan(&self, overrides: &SwapOverrides) -> Result<PlanView, String> {
        let snapshot = self.snapshot_for(overrides)?;
        let mut request = overrides.resolve(&snapshot.config)?;
        request.dry_run = true;
        let (resolved, steps) = self.engine.prepare(&request, &snapshot);
        // Plan-only whatever was asked: the dry run returns before any effect.
        let mut resolved = resolved;
        resolved.dry_run = true;
        let outcomes = steps
            .into_iter()
            .map(|step| match step {
                EngineStep::Done(outcome) => outcome,
                EngineStep::Run(target) => self.engine.execute(&resolved, &target, &mut NoObserver),
            })
            .collect();
        let unavailable = self.active().err();
        Ok(PlanView {
            outcomes,
            executable: unavailable.is_none(),
            unavailable,
        })
    }

    pub(crate) fn submit(
        this: &Arc<Self>,
        request_key: &str,
        overrides: &SwapOverrides,
        guard: Option<Box<dyn TargetGuard>>,
    ) -> Result<Accepted, String> {
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
        let request = overrides.resolve(&snapshot.config)?;
        let (resolved, steps) = this.engine.prepare(&request, &snapshot);

        let (settled, runs, taken) = this.split(request_key, steps);
        let plan = AcceptedPlan {
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
        let job = created.job_id;
        if let Err(error) = this.store.start(&this.lease, &job) {
            this.release_all(&taken);
            this.set_fault(format!("could not start job {job}: {error}"));
            return Err(error.to_string());
        }
        if let Ok(mut live) = this.live.lock() {
            live.insert(job.clone(), LiveJob::default());
        }
        Self::spawn(
            this,
            &job,
            resolved,
            runs,
            guard,
            plan.targets.len(),
            &taken,
        )?;
        let view = this.status(&JobRef::Id(job.clone()))?;
        Ok(Accepted {
            job_id: job,
            existing: false,
            view,
        })
    }

    /// Settled steps and the targets to run, each target's endpoint
    /// reserved; a pane already reserved settles as busy.
    #[allow(clippy::type_complexity)] // the three parts of one split
    fn split(
        &self,
        owner: &str,
        steps: Vec<EngineStep<E::Target>>,
    ) -> (
        Vec<SettledEntry>,
        Vec<(usize, TargetIdentity, E::Target)>,
        Vec<String>,
    ) {
        let mut settled = Vec::new();
        let mut runs = Vec::new();
        let mut taken = Vec::new();
        for (report_index, step) in steps.into_iter().enumerate() {
            match step {
                EngineStep::Done(outcome) => settled.push(settled_entry(report_index, &outcome)),
                EngineStep::Run(target) => {
                    let identity = self.engine.identity(&target);
                    if self.reserve(&identity.endpoint, owner) {
                        taken.push(identity.endpoint.clone());
                        runs.push((report_index, identity, target));
                    } else {
                        settled.push(SettledEntry {
                            report_index,
                            pane: Some(identity.pane.clone()),
                            agent_session_id: identity.conversation_id.clone(),
                            disposition: Disposition::Skipped,
                            reason: "another swap job is acting on this pane".into(),
                        });
                    }
                }
            }
        }
        (settled, runs, taken)
    }

    /// Hand an accepted job to its own thread. If the thread cannot start,
    /// every target ends with no effect and the job is recorded as ended.
    fn spawn(
        this: &Arc<Self>,
        job: &str,
        request: SwapRequest,
        runs: Vec<(usize, TargetIdentity, E::Target)>,
        guard: Option<Box<dyn TargetGuard>>,
        targets: usize,
        taken: &[String],
    ) -> Result<(), String> {
        let worker = Arc::clone(this);
        let worker_job = job.to_owned();
        let spawned = std::thread::Builder::new()
            .name(format!("swap-{}", &job[..job.len().min(13)]))
            .spawn(move || worker.run(&worker_job, &request, runs, guard));
        if let Err(error) = spawned {
            for target in 0..targets {
                let _ = this.store.finish_target(
                    &this.lease,
                    job,
                    u32::try_from(target).unwrap_or(u32::MAX),
                    1,
                    Disposition::Failed,
                );
            }
            let _ = this.store.finish(&this.lease, job);
            this.release_all(taken);
            return Err(format!("the swap executor could not start: {error}"));
        }
        Ok(())
    }

    /// Run a job's targets in order, halting the whole job on the first
    /// failure to record.
    fn run(
        &self,
        job: &str,
        request: &SwapRequest,
        runs: Vec<(usize, TargetIdentity, E::Target)>,
        adopted: Option<Box<dyn TargetGuard>>,
    ) {
        let mut adopted = adopted;
        for (target_id, (report_index, identity, target)) in runs.into_iter().enumerate() {
            let target_id = u32::try_from(target_id).unwrap_or(u32::MAX);
            if self.fault().is_some() {
                // Halted: later targets are never reached, their pane stays
                // reserved until the daemon restarts and recovers the job.
                break;
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
            let _guard = match held {
                Ok(guard) => guard,
                Err(why) => {
                    self.settle(
                        job,
                        target_id,
                        report_index,
                        &identity,
                        Disposition::Skipped,
                        None,
                        &why,
                    );
                    continue;
                }
            };
            let mut journal = self.store.journal(&self.lease, job, target_id, 1);
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
            let recorded = self
                .store
                .finish_target(&self.lease, job, target_id, 1, disposition);
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

    #[allow(clippy::too_many_arguments)] // one settled target, all of its identity
    fn settle(
        &self,
        job: &str,
        target_id: u32,
        report_index: usize,
        identity: &TargetIdentity,
        disposition: Disposition,
        outcome: Option<SwapOutcome>,
        why: &str,
    ) {
        if let Err(error) = self
            .store
            .finish_target(&self.lease, job, target_id, 1, disposition)
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
            conversation_verified: Some(identity.conversation_verified),
            conversation_unverified_reason: None,
            registered: None,
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
                outcome: None,
            })
            .collect();
        for (target, planned) in status.targets.iter().zip(&plan.targets) {
            entries.push(EntryView {
                report_index: planned.report_index,
                pane: Some(planned.pane.clone()),
                disposition: target.disposition.clone(),
                reason: target.reason.clone(),
                uncertain_effects: target.uncertain_effects,
                outcome: live.get(&planned.report_index).cloned(),
            });
        }
        entries.sort_by_key(|entry| entry.report_index);
        Ok(JobView {
            job_id: status.job_id,
            node_id: status.node_id,
            state: status.state,
            revision: status.revision,
            reason: status.reason,
            entries,
            fault: self.fault(),
        })
    }
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
        reason: outcome.note.clone(),
    }
}

fn planned_target(report_index: usize, identity: &TargetIdentity) -> PlannedTarget {
    PlannedTarget {
        report_index,
        endpoint: identity.endpoint.clone(),
        host: crate::backend::pane_id_host_kind(&identity.pane)
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
