//! The service with a scripted engine and daemon: no pane, no process, no
//! live swap. Targets record what they were asked to do and can be held
//! mid-run to test what happens around a running job.

use super::*;
use crate::account_swap::effects::{Effect, Key, Op, Purpose, Transport};
use crate::account_swap::{endpoint_key, SessionIdSource};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// Blocks a target until opened, and tells whether one has reached it.
#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
    reached: std::sync::atomic::AtomicBool,
}

impl Gate {
    fn open(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }

    fn wait(&self) {
        self.reached.store(true, Ordering::SeqCst);
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.changed.wait(open).unwrap();
        }
    }

    /// Wait until a target is held at this gate: past every boundary before
    /// its execution.
    fn until_reached(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.reached.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "no target reached the gate");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

struct FakeTarget {
    identity: TargetIdentity,
    gate: Option<Arc<Gate>>,
}

/// What the engine plans, per call to `prepare`.
type Script = Box<dyn Fn() -> Vec<EngineStep<FakeTarget>> + Send + Sync>;

/// How a scripted target fails, once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fail {
    /// Before anything at or past the relaunch.
    BeforeLaunch,
    /// After the relaunch was typed.
    AfterLaunch,
}

struct FakeEngine {
    script: Script,
    executed: Arc<Mutex<Vec<String>>>,
    dry_runs: Arc<AtomicUsize>,
    /// Panes to fail, each once.
    failing: Arc<Mutex<HashMap<String, Fail>>>,
    /// Every pane a retry looked at, read-only.
    observed: Arc<Mutex<Vec<String>>>,
    /// The request each executed target ran under.
    requests: Arc<Mutex<Vec<SwapRequest>>>,
}

impl Engine for FakeEngine {
    type Target = FakeTarget;

    fn prepare(
        &self,
        request: &SwapRequest,
        _snapshot: &Snapshot,
    ) -> (SwapRequest, Vec<EngineStep<FakeTarget>>) {
        (request.clone(), (self.script)())
    }

    fn identity(&self, target: &FakeTarget) -> TargetIdentity {
        target.identity.clone()
    }

    fn prepare_retry(
        &self,
        request: &SwapRequest,
        planned: &PlannedTarget,
        _snapshot: &Snapshot,
    ) -> (SwapRequest, EngineStep<FakeTarget>) {
        let mut identity = identity(&planned.pane, "/tmp/tmux-1/default");
        identity
            .assigned_account
            .clone_from(&planned.assigned_account);
        (
            request.clone(),
            EngineStep::Run(FakeTarget {
                identity,
                gate: None,
            }),
        )
    }

    fn prepare_resume(
        &self,
        request: &SwapRequest,
        planned: &PlannedTarget,
        snapshot: &Snapshot,
    ) -> (SwapRequest, EngineStep<Self::Target>, bool) {
        if planned.process_pid != 100 || planned.process_start.as_deref() != Some("1") {
            return (
                request.clone(),
                EngineStep::Done(outcome(
                    &planned.pane,
                    SwapStatus::Skipped,
                    "binding unavailable or changed",
                )),
                planned.process_pid != 100 || planned.process_start.is_some(),
            );
        }
        let (request, step) = self.prepare_retry(request, planned, snapshot);
        (request, step, false)
    }

    fn observe(&self, planned: &PlannedTarget, _snapshot: &Snapshot) -> &'static str {
        self.observed.lock().unwrap().push(planned.pane.clone());
        "observed_unbound"
    }

    fn execute(
        &self,
        request: &SwapRequest,
        target: &FakeTarget,
        observer: &mut dyn EffectObserver,
    ) -> SwapOutcome {
        let mut outcome = outcome(&target.identity.pane, SwapStatus::Planned, "");
        if request.dry_run {
            self.dry_runs.fetch_add(1, Ordering::SeqCst);
            return outcome;
        }
        if let Some(gate) = &target.gate {
            gate.wait();
        }
        assert!(target.identity.pane != "%panic", "the engine blew up");
        self.requests.lock().unwrap().push(request.clone());
        let fail = self.failing.lock().unwrap().remove(&target.identity.pane);
        if let Some(fail) = fail {
            let (purpose, op) = match fail {
                Fail::BeforeLaunch => (Purpose::Interrupt, Op::Key(Key::Esc)),
                Fail::AfterLaunch => (Purpose::Relaunch, Op::Text { chars: 8 }),
            };
            let effect = Effect {
                group: 0,
                purpose,
                op,
            };
            let _ = observer.before(&effect);
            let _ = observer.after(&effect, Transport::Accepted);
            outcome.status = SwapStatus::Failed;
            outcome.note = "failed".into();
            return outcome;
        }
        self.executed
            .lock()
            .unwrap()
            .push(target.identity.pane.clone());
        let effect = Effect {
            group: 0,
            purpose: Purpose::Interrupt,
            op: Op::Key(Key::Esc),
        };
        if observer.before(&effect).is_err() {
            outcome.status = SwapStatus::Failed;
            outcome.note = "stopped: journal".into();
            return outcome;
        }
        let _ = observer.after(&effect, Transport::Accepted);
        outcome.status = SwapStatus::Swapped;
        outcome.note = "resumed".into();
        outcome
    }
}

fn outcome(pane: &str, status: SwapStatus, note: &str) -> SwapOutcome {
    SwapOutcome {
        pane: Some(pane.into()),
        agent_session_id: format!("conv-{pane}"),
        kind: AgentKind::ClaudeCode,
        from: None,
        to: None,
        status,
        command: None,
        note: note.into(),
        continue_reason: None,
        session_id_source: None,
        conversation_verified: None,
        conversation_unverified_reason: None,
        registered: None,
        continue_withheld: None,
        follow_up: None,
    }
}

fn identity(pane: &str, socket: &str) -> TargetIdentity {
    TargetIdentity {
        pane: pane.into(),
        socket: Some(socket.into()),
        endpoint: endpoint_key(pane, Some(socket)),
        provider: "claude".into(),
        conversation_id: format!("conv-{pane}"),
        session_id_source: SessionIdSource::Registry,
        conversation_verified: true,
        process_pid: 100,
        process_start: Some("1".into()),
        from_account: Some("a".into()),
        assigned_account: Some("b".into()),
        direct: false,
    }
}

fn run(pane: &str) -> EngineStep<FakeTarget> {
    EngineStep::Run(FakeTarget {
        identity: identity(pane, "/tmp/tmux-1/default"),
        gate: None,
    })
}

struct FakeGuard(String);

impl TargetGuard for FakeGuard {
    fn covers(&self, endpoint: &str) -> bool {
        endpoint == self.0
    }
}

struct TrackedGuard {
    endpoint: String,
    held: Arc<AtomicUsize>,
    released: Arc<AtomicUsize>,
}

impl TargetGuard for TrackedGuard {
    fn covers(&self, endpoint: &str) -> bool {
        self.endpoint == endpoint
    }
}

impl Drop for TrackedGuard {
    fn drop(&mut self) {
        self.held.fetch_sub(1, Ordering::SeqCst);
        self.released.fetch_add(1, Ordering::SeqCst);
    }
}

struct FakeProvider {
    snapshot_gate: Mutex<Option<Arc<Gate>>>,
    snapshot_calls: AtomicUsize,
    snapshot_failure: AtomicBool,
    guard_failure: AtomicBool,
    config_path: Option<PathBuf>,
    config: Mutex<SwapConfig>,
    agents: Mutex<Vec<Agent>>,
    acquired: Mutex<Vec<String>>,
    held_guards: Arc<AtomicUsize>,
    released_guards: Arc<AtomicUsize>,
    /// Holds every guard acquisition until opened, when set.
    guard_gate: Mutex<Option<Arc<Gate>>>,
}

impl ContextProvider for FakeProvider {
    fn snapshot(&self) -> Result<Snapshot, String> {
        self.snapshot_calls.fetch_add(1, Ordering::SeqCst);
        let gate = self.snapshot_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.wait();
        }
        if self.snapshot_failure.load(Ordering::SeqCst) {
            return Err("temporarily unavailable".into());
        }
        Ok(Snapshot {
            node_id: "node-1".into(),
            agents: self.agents.lock().unwrap().clone(),
            backends: Vec::new(),
            config: self.config.lock().unwrap().clone(),
            accounts: crate::accounts::AccountsConfig::default(),
            config_path: self.config_path.clone(),
        })
    }

    fn acquire_guard(
        &self,
        pane: &str,
        socket: Option<&str>,
    ) -> Result<Box<dyn TargetGuard>, String> {
        if self.guard_failure.load(Ordering::SeqCst) {
            return Err("temporarily held".into());
        }
        let endpoint = endpoint_key(pane, socket);
        self.acquired.lock().unwrap().push(endpoint.clone());
        let gate = self.guard_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.wait();
        }
        self.held_guards.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(TrackedGuard {
            endpoint,
            held: Arc::clone(&self.held_guards),
            released: Arc::clone(&self.released_guards),
        }))
    }
}

struct Harness {
    dir: tempfile::TempDir,
    service: Arc<Service<FakeEngine>>,
    provider: Arc<FakeProvider>,
    executed: Arc<Mutex<Vec<String>>>,
    dry_runs: Arc<AtomicUsize>,
    failing: Arc<Mutex<HashMap<String, Fail>>>,
    observed: Arc<Mutex<Vec<String>>>,
    requests: Arc<Mutex<Vec<SwapRequest>>>,
}

fn alive(_: u32, _: Option<&str>) -> Liveness {
    Liveness::Alive
}

fn gone(_: u32, _: Option<&str>) -> Liveness {
    Liveness::Gone
}

fn harness(script: Script) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "").unwrap();
    let provider = Arc::new(FakeProvider {
        snapshot_gate: Mutex::new(None),
        snapshot_calls: AtomicUsize::new(0),
        snapshot_failure: AtomicBool::new(false),
        guard_failure: AtomicBool::new(false),
        config_path: Some(config_path),
        agents: Mutex::new(Vec::new()),
        config: Mutex::new(SwapConfig::default()),
        acquired: Mutex::new(Vec::new()),
        held_guards: Arc::new(AtomicUsize::new(0)),
        released_guards: Arc::new(AtomicUsize::new(0)),
        guard_gate: Mutex::new(None),
    });
    let executed = Arc::new(Mutex::new(Vec::new()));
    let dry_runs = Arc::new(AtomicUsize::new(0));
    let failing = Arc::new(Mutex::new(HashMap::new()));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let service = Service::open(
        &dir.path().join("swap-jobs.sqlite3"),
        OwnerIdentity {
            nonce: "first".into(),
            pid: 1,
            process_start: Some("1".into()),
        },
        &alive,
        FakeEngine {
            script,
            executed: Arc::clone(&executed),
            dry_runs: Arc::clone(&dry_runs),
            failing: Arc::clone(&failing),
            observed: Arc::clone(&observed),
            requests: Arc::clone(&requests),
        },
        Arc::clone(&provider) as Arc<dyn ContextProvider>,
    )
    .unwrap();
    Harness {
        dir,
        service: Arc::new(service),
        provider,
        executed,
        dry_runs,
        failing,
        observed,
        requests,
    }
}

fn overrides(targets: &[&str]) -> SwapOverrides {
    SwapOverrides {
        targets: targets.iter().map(|t| (*t).to_owned()).collect(),
        ..SwapOverrides::default()
    }
}

/// Poll until the job leaves `running`.
fn settled(service: &Service<FakeEngine>, job: &str) -> JobView {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let view = service.status(&JobRef::Id(job.into())).unwrap();
        if view.state != "running" || view.fault.is_some() || Instant::now() > deadline {
            return view;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_job_runs_its_targets_and_reports_in_the_requested_order() {
    let h = harness(Box::new(|| {
        vec![
            run("%1"),
            EngineStep::Done(outcome("%2", SwapStatus::Skipped, "mid-turn")),
            run("%3"),
        ]
    }));
    let accepted = Service::submit(
        &h.service,
        "key",
        &overrides(&["%1", "%2", "%3"]),
        Origin::Public,
        None,
    )
    .unwrap();
    assert!(!accepted.existing);
    let view = settled(&h.service, &accepted.job_id);
    assert_eq!(view.state, "done");
    let order: Vec<_> = view
        .entries
        .iter()
        .map(|e| {
            (
                e.report_index,
                e.pane.clone().unwrap(),
                e.disposition.clone().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        order,
        [
            (0, "%1".into(), "swapped".into()),
            (1, "%2".into(), "skipped".into()),
            (2, "%3".into(), "swapped".into()),
        ]
    );
    assert_eq!(*h.executed.lock().unwrap(), ["%1", "%3"]);
    assert_eq!(view.entries[0].outcome.as_ref().unwrap().note, "resumed");
}

/// The same request again — even after a default it did not set changed —
/// is the job it already made, never a second run.
#[test]
fn a_repeated_request_attaches_to_its_job() {
    let h = harness(Box::new(|| vec![run("%1")]));
    let first =
        Service::submit(&h.service, "key", &overrides(&["%1"]), Origin::Public, None).unwrap();
    settled(&h.service, &first.job_id);
    h.provider.config.lock().unwrap().continue_text = "changed default".into();
    let again =
        Service::submit(&h.service, "key", &overrides(&["%1"]), Origin::Public, None).unwrap();
    assert!(again.existing);
    assert_eq!(again.job_id, first.job_id);
    assert_eq!(h.executed.lock().unwrap().len(), 1);
    // The same key for a different request is refused.
    assert!(Service::submit(&h.service, "key", &overrides(&["%9"]), Origin::Public, None).is_err());
}

#[test]
fn concurrent_sends_of_one_request_make_one_executor() {
    let h = harness(Box::new(|| vec![run("%1")]));
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let service = Arc::clone(&h.service);
            std::thread::spawn(move || {
                Service::submit(&service, "key", &overrides(&["%1"]), Origin::Public, None)
                    .unwrap()
                    .job_id
            })
        })
        .collect();
    let jobs: Vec<String> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert!(jobs.iter().all(|job| *job == jobs[0]));
    settled(&h.service, &jobs[0]);
    assert_eq!(h.executed.lock().unwrap().len(), 1);
}

/// A pane a running job holds is busy for another job, even named through
/// another spelling of the same server; the caller is free to go while the
/// job runs on.
#[test]
fn a_busy_pane_is_refused_and_a_job_outlives_its_caller() {
    let gate = Arc::new(Gate::default());
    let first_gate = Arc::clone(&gate);
    let calls = Arc::new(AtomicUsize::new(0));
    let h = harness(Box::new(move || {
        let call = calls.fetch_add(1, Ordering::SeqCst);
        let socket = if call == 0 {
            "/tmp/tmux-1/default"
        } else {
            "default"
        };
        vec![EngineStep::Run(FakeTarget {
            identity: identity("%1", socket),
            gate: (call == 0).then(|| Arc::clone(&first_gate)),
        })]
    }));
    let first =
        Service::submit(&h.service, "one", &overrides(&["%1"]), Origin::Public, None).unwrap();
    // `submit` returned while the target is still held: the caller is gone.
    let second =
        Service::submit(&h.service, "two", &overrides(&["%1"]), Origin::Public, None).unwrap();
    let view = settled(&h.service, &second.job_id);
    assert_eq!(view.entries[0].disposition.as_deref(), Some("skipped"));
    assert_eq!(view.entries[0].reason.as_deref(), Some("busy"));
    assert!(view.entries[0]
        .outcome
        .as_ref()
        .unwrap()
        .note
        .contains("another swap job"));
    gate.open();
    assert_eq!(settled(&h.service, &first.job_id).state, "done");
    assert_eq!(*h.executed.lock().unwrap(), ["%1"]);
}

/// An automation's guard is used for its one pane and no other guard is
/// taken; a guard that does not cover the job's single target exactly is
/// refused before anything is accepted, rather than waited on again.
#[test]
fn an_adopted_guard_must_cover_its_one_target() {
    let h = harness(Box::new(|| vec![run("%1")]));
    let mine = endpoint_key("%1", Some("/tmp/tmux-1/default"));
    let accepted = Service::submit(
        &h.service,
        "key",
        &overrides(&["%1"]),
        Origin::Automation,
        Some(Box::new(FakeGuard(mine))),
    )
    .unwrap();
    assert_eq!(settled(&h.service, &accepted.job_id).state, "done");
    assert!(h.provider.acquired.lock().unwrap().is_empty());

    let other = endpoint_key("%9", Some("/tmp/tmux-1/default"));
    assert!(Service::submit(
        &h.service,
        "other",
        &overrides(&["%1"]),
        Origin::Automation,
        Some(Box::new(FakeGuard(other))),
    )
    .unwrap_err()
    .contains("does not cover"));
    assert!(h.service.status(&JobRef::Key("other".into())).is_err());
    assert!(h.service.reserved.lock().unwrap().is_empty());
}

#[test]
fn an_adopted_guard_is_refused_for_more_than_one_target() {
    let h = harness(Box::new(|| vec![run("%1"), run("%2")]));
    let mine = endpoint_key("%1", Some("/tmp/tmux-1/default"));
    assert!(Service::submit(
        &h.service,
        "key",
        &overrides(&["%1", "%2"]),
        Origin::Automation,
        Some(Box::new(FakeGuard(mine))),
    )
    .is_err());
    assert!(h.executed.lock().unwrap().is_empty());
}

/// A journal that stops accepting writes halts the whole job before its
/// next target, and the service takes no more work.
#[test]
fn a_journal_failure_halts_the_job_and_the_service() {
    let gate = Arc::new(Gate::default());
    let held = Arc::clone(&gate);
    let h = harness(Box::new(move || {
        vec![
            EngineStep::Run(FakeTarget {
                identity: identity("%1", "/tmp/tmux-1/default"),
                gate: Some(Arc::clone(&held)),
            }),
            run("%2"),
        ]
    }));
    let accepted = Service::submit(
        &h.service,
        "key",
        &overrides(&["%1", "%2"]),
        Origin::Public,
        None,
    )
    .unwrap();
    // Another manager takes the store while the first target is held (in
    // its execution, past its start boundary, which is fenced too).
    gate.until_reached();
    let usurper = SwapJobStore::open(&h.dir.path().join("swap-jobs.sqlite3")).unwrap();
    usurper
        .acquire(
            &OwnerIdentity {
                nonce: "second".into(),
                pid: 2,
                process_start: None,
            },
            &gone,
        )
        .unwrap();
    gate.open();
    let view = settled(&h.service, &accepted.job_id);
    assert!(view.fault.is_some(), "{view:?}");
    assert_eq!(
        *h.executed.lock().unwrap(),
        ["%1"],
        "the second target never ran"
    );
    assert!(h.service.active().is_err());
    assert!(Service::submit(
        &h.service,
        "next",
        &overrides(&["%3"]),
        Origin::Public,
        None
    )
    .is_err());
}

#[test]
fn a_request_made_with_another_configuration_is_refused() {
    let h = harness(Box::new(|| vec![run("%1")]));
    let mut request = overrides(&["%1"]);
    request.caller_config = Some(h.dir.path().join("other.toml"));
    assert!(
        Service::submit(&h.service, "key", &request, Origin::Public, None)
            .unwrap_err()
            .contains("daemon's configuration")
    );
    // The daemon's file under another name is the same file.
    #[cfg(unix)]
    {
        let link = h.dir.path().join("link.toml");
        std::os::unix::fs::symlink(h.provider.config_path.as_ref().unwrap(), &link).unwrap();
        request.caller_config = Some(link);
        Service::submit(&h.service, "key", &request, Origin::Public, None).unwrap();
    }
    assert!(h.executed.lock().unwrap().len() <= 1);
}

/// A plan touches nothing: no job, no reservation, every target dry.
#[test]
fn a_plan_writes_nothing() {
    let h = harness(Box::new(|| vec![run("%1"), run("%2")]));
    let plan = h
        .service
        .plan(&overrides(&["%1", "%2"]), Origin::Public)
        .unwrap();
    assert_eq!(plan.outcomes.len(), 2);
    assert!(plan.executable);
    assert_eq!(h.dry_runs.load(Ordering::SeqCst), 2);
    assert!(h.executed.lock().unwrap().is_empty());
    assert!(h.service.status(&JobRef::Key("anything".into())).is_err());
    // Its pane is free for a real submit afterwards.
    let accepted = Service::submit(
        &h.service,
        "key",
        &overrides(&["%1", "%2"]),
        Origin::Public,
        None,
    )
    .unwrap();
    assert_eq!(settled(&h.service, &accepted.job_id).state, "done");
}

/// After a restart the job reads back in its order, from what was recorded.
#[test]
fn a_restarted_service_reads_an_interrupted_job_back_in_order() {
    let gate = Arc::new(Gate::default());
    let _release = OpenOnDrop(Arc::clone(&gate));
    let held = Arc::clone(&gate);
    let h = harness(Box::new(move || {
        vec![
            EngineStep::Done(outcome("%0", SwapStatus::Skipped, "excluded")),
            EngineStep::Run(FakeTarget {
                identity: identity("%1", "/tmp/tmux-1/default"),
                gate: Some(Arc::clone(&held)),
            }),
        ]
    }));
    let accepted = Service::submit(
        &h.service,
        "key",
        &overrides(&["%0", "%1"]),
        Origin::Public,
        None,
    )
    .unwrap();
    gate.until_reached();
    // The daemon restarts after the start boundary, before the first effect.
    let restarted = Service::open(
        &h.dir.path().join("swap-jobs.sqlite3"),
        OwnerIdentity {
            nonce: "second".into(),
            pid: 2,
            process_start: None,
        },
        &gone,
        FakeEngine {
            script: Box::new(Vec::new),
            executed: Arc::new(Mutex::new(Vec::new())),
            dry_runs: Arc::new(AtomicUsize::new(0)),
            failing: Arc::default(),
            observed: Arc::default(),
            requests: Arc::default(),
        },
        Arc::clone(&h.provider) as Arc<dyn ContextProvider>,
    )
    .unwrap();
    let view = restarted.status(&JobRef::Key("key".into())).unwrap();
    assert_eq!(view.job_id, accepted.job_id);
    assert_eq!(view.state, "needs_reconciliation");
    assert_eq!(view.entries[0].pane.as_deref(), Some("%0"));
    assert_eq!(view.entries[0].reason.as_deref(), Some("excluded"));
    assert_eq!(view.entries[1].pane.as_deref(), Some("%1"));
    assert!(
        view.entries.iter().all(|e| e.outcome.is_none()),
        "notes are gone"
    );
    gate.open();
}

#[test]
fn overrides_resolve_like_the_cli() {
    let config = SwapConfig::default();
    let request = SwapOverrides {
        targets: vec!["%5".into()],
        continue_text: Some("go".into()),
        ..SwapOverrides::default()
    }
    .resolve(&config)
    .unwrap();
    assert_eq!(request.scope, SwapScope::Targets(vec!["%5".into()]));
    assert_eq!(request.continue_text.as_deref(), Some("go"));
    assert_eq!(request.continue_when, config.continue_when);
    let never = SwapOverrides {
        no_continue: true,
        ..SwapOverrides::default()
    }
    .resolve(&config)
    .unwrap();
    assert_eq!(never.continue_when, ContinueWhen::Never);
    assert_eq!(never.scope, SwapScope::Limited);
    assert!(SwapOverrides {
        provider: Some("gpt".into()),
        ..SwapOverrides::default()
    }
    .resolve(&config)
    .is_err());
}

/// One live manager holds the store; a second is refused while it lives,
/// and takes over once it is gone — the OS releases the lock however the
/// first one ends — recovering what it left.
#[test]
fn one_live_manager_holds_the_store_until_it_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("swap-jobs.sqlite3");
    let provider = || {
        Arc::new(FakeProvider {
            snapshot_gate: Mutex::new(None),
            snapshot_calls: AtomicUsize::new(0),
            snapshot_failure: AtomicBool::new(false),
            guard_failure: AtomicBool::new(false),
            config_path: None,
            agents: Mutex::new(Vec::new()),
            config: Mutex::new(SwapConfig::default()),
            acquired: Mutex::new(Vec::new()),
            held_guards: Arc::new(AtomicUsize::new(0)),
            released_guards: Arc::new(AtomicUsize::new(0)),
            guard_gate: Mutex::new(None),
        }) as Arc<dyn ContextProvider>
    };
    let first = SwapService::open(Some(&path), provider()).unwrap();
    assert!(SwapService::open(Some(&path), provider())
        .err()
        .unwrap()
        .contains("another live manager"));
    assert!(first.active().is_ok());
    drop(first);
    let second = open_when_free(&path, provider);
    assert!(second.active().is_ok());
}

/// Open the store once its lock is free. A process another test forks at
/// the wrong moment holds an inherited copy of the lock until it execs, so
/// release can lag a drop by a moment.
fn open_when_free(path: &Path, provider: impl Fn() -> Arc<dyn ContextProvider>) -> SwapService {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match SwapService::open(Some(path), provider()) {
            Ok(service) => return service,
            Err(error) if Instant::now() < deadline && error.contains("another live manager") => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("{error}"),
        }
    }
}

/// A planning note can quote an external tool; only a code goes to disk.
#[test]
fn a_settled_note_is_kept_in_memory_and_only_its_code_is_written() {
    let h = harness(Box::new(|| {
        vec![EngineStep::Done(outcome(
            "%1",
            SwapStatus::Failed,
            "aas said: secret-marker token=abc",
        ))]
    }));
    let accepted =
        Service::submit(&h.service, "key", &overrides(&["%1"]), Origin::Public, None).unwrap();
    let view = settled(&h.service, &accepted.job_id);
    assert_eq!(
        view.entries[0].reason.as_deref(),
        Some("candidates_unavailable")
    );
    assert!(view.entries[0]
        .outcome
        .as_ref()
        .unwrap()
        .note
        .contains("secret-marker"));
    let raw: String = rusqlite::Connection::open(h.dir.path().join("swap-jobs.sqlite3"))
        .unwrap()
        .query_row("SELECT plan_json FROM jobs", [], |row| row.get(0))
        .unwrap();
    assert!(!raw.contains("secret-marker"), "{raw}");
}

/// Conflicting options are refused before anything is planned or accepted.
#[test]
fn conflicting_options_are_refused_before_planning() {
    let h = harness(Box::new(|| vec![run("%1")]));
    let cases = [
        SwapOverrides {
            all: true,
            limited: true,
            ..SwapOverrides::default()
        },
        SwapOverrides {
            targets: vec!["%1".into()],
            all: true,
            ..SwapOverrides::default()
        },
        SwapOverrides {
            spread: true,
            to: Some("x".into()),
            ..SwapOverrides::default()
        },
        SwapOverrides {
            no_continue: true,
            continue_when: Some("bogus".into()),
            ..SwapOverrides::default()
        },
        SwapOverrides {
            no_continue: true,
            continue_text: Some("go".into()),
            ..SwapOverrides::default()
        },
    ];
    for (i, request) in cases.iter().enumerate() {
        assert!(h.service.plan(request, Origin::Public).is_err(), "case {i}");
        assert!(
            Service::submit(&h.service, &format!("k{i}"), request, Origin::Public, None).is_err(),
            "case {i}"
        );
        assert!(h.service.status(&JobRef::Key(format!("k{i}"))).is_err());
    }
    assert!(h.executed.lock().unwrap().is_empty());
    assert_eq!(h.dry_runs.load(Ordering::SeqCst), 0);
}

/// A pane whose server cannot be told is not reserved as some other one.
#[test]
fn a_target_on_an_unknown_server_is_refused() {
    let h = harness(Box::new(|| {
        let mut unknown = identity("%1", "default");
        unknown.socket = None;
        unknown.endpoint = endpoint_key("%1", None);
        vec![EngineStep::Run(FakeTarget {
            identity: unknown,
            gate: None,
        })]
    }));
    let accepted =
        Service::submit(&h.service, "key", &overrides(&["%1"]), Origin::Public, None).unwrap();
    let view = settled(&h.service, &accepted.job_id);
    assert_eq!(view.entries[0].reason.as_deref(), Some("unknown_server"));
    assert!(h.executed.lock().unwrap().is_empty());
    // Two spellings of one tmux server are one endpoint.
    assert_eq!(
        endpoint_key("%1", Some("/tmp/tmux-1/default")),
        endpoint_key("%1", Some("default"))
    );
}

/// A thread that cannot start ends the job durably with no effect and frees
/// its panes.
#[test]
fn an_executor_that_cannot_start_ends_the_job_without_effects() {
    let dir = tempfile::tempdir().unwrap();
    let executed = Arc::new(Mutex::new(Vec::new()));
    let service = Service::open(
        &dir.path().join("swap-jobs.sqlite3"),
        OwnerIdentity {
            nonce: "a".into(),
            pid: 1,
            process_start: None,
        },
        &alive,
        FakeEngine {
            script: Box::new(|| vec![run("%1"), run("%2")]),
            executed: Arc::clone(&executed),
            dry_runs: Arc::new(AtomicUsize::new(0)),
            failing: Arc::default(),
            observed: Arc::default(),
            requests: Arc::default(),
        },
        Arc::new(FakeProvider {
            snapshot_gate: Mutex::new(None),
            snapshot_calls: AtomicUsize::new(0),
            snapshot_failure: AtomicBool::new(false),
            guard_failure: AtomicBool::new(false),
            config_path: None,
            agents: Mutex::new(Vec::new()),
            config: Mutex::new(SwapConfig::default()),
            acquired: Mutex::new(Vec::new()),
            held_guards: Arc::new(AtomicUsize::new(0)),
            released_guards: Arc::new(AtomicUsize::new(0)),
            guard_gate: Mutex::new(None),
        }) as Arc<dyn ContextProvider>,
    )
    .unwrap()
    .with_spawner(Box::new(|_, _| Err(std::io::Error::other("no threads"))));
    let service = Arc::new(service);
    assert!(Service::submit(
        &service,
        "key",
        &overrides(&["%1", "%2"]),
        Origin::Public,
        None
    )
    .unwrap_err()
    .contains("could not start"));
    let view = service.status(&JobRef::Key("key".into())).unwrap();
    assert_eq!(view.state, "done");
    assert!(view
        .entries
        .iter()
        .all(|e| e.disposition.as_deref() == Some("failed")));
    assert!(executed.lock().unwrap().is_empty());
    assert!(service.reserved.lock().unwrap().is_empty());
    assert!(service.active().is_ok());
}

/// A panicking executor faults the service instead of leaving a running
/// job behind a healthy one.
#[test]
fn a_panicking_executor_faults_the_service() {
    let h = harness(Box::new(|| vec![run("%panic"), run("%2")]));
    let accepted = Service::submit(
        &h.service,
        "key",
        &overrides(&["%panic", "%2"]),
        Origin::Public,
        None,
    )
    .unwrap();
    let view = settled(&h.service, &accepted.job_id);
    assert!(
        view.fault
            .as_deref()
            .unwrap_or_default()
            .contains("panicked"),
        "{view:?}"
    );
    assert_ne!(view.state, "done");
    assert!(h.service.active().is_err());
    assert!(h.executed.lock().unwrap().is_empty());
}

/// The lock lives as long as anything still running under it: dropping the
/// public handle while a job runs does not let another manager in.
#[test]
fn ownership_outlives_the_handle_while_a_job_runs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("swap-jobs.sqlite3");
    let provider = || {
        Arc::new(FakeProvider {
            snapshot_gate: Mutex::new(None),
            snapshot_calls: AtomicUsize::new(0),
            snapshot_failure: AtomicBool::new(false),
            guard_failure: AtomicBool::new(false),
            config_path: None,
            agents: Mutex::new(Vec::new()),
            config: Mutex::new(SwapConfig::default()),
            acquired: Mutex::new(Vec::new()),
            held_guards: Arc::new(AtomicUsize::new(0)),
            released_guards: Arc::new(AtomicUsize::new(0)),
            guard_gate: Mutex::new(None),
        }) as Arc<dyn ContextProvider>
    };

    // A worker's reference keeps the manager's lock.
    let first = SwapService::open(Some(&path), provider()).unwrap();
    let worker = Arc::clone(&first.inner);
    drop(first);
    assert!(SwapService::open(Some(&path), provider()).is_err());
    drop(worker);
    drop(open_when_free(&path, provider));

    // A job held mid-target keeps it too, until it finishes.
    let gate = Arc::new(Gate::default());
    let held = Arc::clone(&gate);
    let lock = ownership_lock(&path).unwrap();
    let service = Arc::new(
        Service::open(
            &path,
            OwnerIdentity {
                nonce: "running".into(),
                pid: std::process::id(),
                process_start: None,
            },
            &gone,
            FakeEngine {
                script: Box::new(move || {
                    vec![EngineStep::Run(FakeTarget {
                        identity: identity("%1", "/tmp/tmux-1/default"),
                        gate: Some(Arc::clone(&held)),
                    })]
                }),
                executed: Arc::new(Mutex::new(Vec::new())),
                dry_runs: Arc::new(AtomicUsize::new(0)),
                failing: Arc::default(),
                observed: Arc::default(),
                requests: Arc::default(),
            },
            provider(),
        )
        .unwrap()
        .holding(lock),
    );
    Service::submit(&service, "key", &overrides(&["%1"]), Origin::Public, None).unwrap();
    drop(service);
    assert!(
        ownership_lock(&path).is_err(),
        "the running job still owns the store"
    );
    gate.open();
    let deadline = Instant::now() + Duration::from_secs(10);
    let freed = loop {
        match ownership_lock(&path) {
            Ok(lock) => break Some(lock),
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => break None,
        }
    };
    assert!(freed.is_some(), "released once the job ended");
}

fn direct(pane: &str) -> EngineStep<FakeTarget> {
    let mut identity = identity(pane, "/tmp/tmux-1/default");
    identity.direct = true;
    EngineStep::Run(FakeTarget {
        identity,
        gate: None,
    })
}

/// A public request does not respawn a pane that runs its agent directly —
/// in plan and in submit alike — while the daemon's own automation still
/// does, as it always has.
#[test]
fn a_direct_pane_is_refused_publicly_and_kept_for_automation() {
    let h = harness(Box::new(|| vec![direct("%1")]));
    let plan = h.service.plan(&overrides(&["%1"]), Origin::Public).unwrap();
    assert!(plan.outcomes[0].note.contains("runs its agent directly"));
    // Named for the caller to act on itself, as exactly that agent.
    assert_eq!(
        plan.direct_targets,
        [crate::account_swap::DirectTarget {
            pane: "%1".into(),
            socket: Some("/tmp/tmux-1/default".into()),
            endpoint: endpoint_key("%1", Some("/tmp/tmux-1/default")),
            conversation_id: "conv-%1".into(),
        }]
    );
    let waiting = h
        .service
        .plan(
            &SwapOverrides {
                when_idle: true,
                ..overrides(&["%1"])
            },
            Origin::Public,
        )
        .unwrap();
    assert!(waiting.direct_targets.is_empty());
    assert!(waiting.outcomes[0].note.contains("runs its agent directly"));
    let shell = harness(Box::new(|| vec![run("%2")]));
    assert!(shell
        .service
        .plan(&overrides(&["%2"]), Origin::Public)
        .unwrap()
        .direct_targets
        .is_empty());
    let public = Service::submit(
        &h.service,
        "public",
        &overrides(&["%1"]),
        Origin::Public,
        None,
    )
    .unwrap();
    let view = settled(&h.service, &public.job_id);
    assert_eq!(
        view.entries[0].reason.as_deref(),
        Some("direct_pane_unsupported")
    );
    assert!(h.executed.lock().unwrap().is_empty());

    let mine = endpoint_key("%1", Some("/tmp/tmux-1/default"));
    let automation = Service::submit(
        &h.service,
        "automation",
        &overrides(&["%1"]),
        Origin::Automation,
        Some(Box::new(FakeGuard(mine))),
    )
    .unwrap();
    assert_eq!(settled(&h.service, &automation.job_id).state, "done");
    assert_eq!(*h.executed.lock().unwrap(), ["%1"]);
}

/// Whoever waits on a job is woken when it ends — finished, halted or
/// panicked — and a job that already ended has nothing to wait on.
#[test]
fn a_job_wakes_its_waiters_however_it_ends() {
    let gate = Arc::new(Gate::default());
    let held = Arc::clone(&gate);
    let h = harness(Box::new(move || {
        vec![EngineStep::Run(FakeTarget {
            identity: identity("%1", "/tmp/tmux-1/default"),
            gate: Some(Arc::clone(&held)),
        })]
    }));
    let accepted =
        Service::submit(&h.service, "key", &overrides(&["%1"]), Origin::Public, None).unwrap();
    let done = h.service.receiver(&accepted.job_id).unwrap();
    assert!(!*done.borrow());
    gate.open();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !*done.borrow() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(*done.borrow());
    assert!(h.service.receiver(&accepted.job_id).is_none());

    let panicking = harness(Box::new(|| vec![run("%panic")]));
    let job = Service::submit(
        &panicking.service,
        "key",
        &overrides(&["%panic"]),
        Origin::Public,
        None,
    )
    .unwrap()
    .job_id;
    let deadline = Instant::now() + Duration::from_secs(10);
    while panicking.service.receiver(&job).is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        panicking.service.receiver(&job).is_none(),
        "a panic still wakes waiters"
    );
    // A halted service is still inspectable.
    assert!(panicking.service.active().is_err());
    assert!(panicking.service.status(&JobRef::Id(job)).is_ok());
}

/// Opens a gate when dropped, panic or not.
struct OpenOnDrop(Arc<Gate>);

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        self.0.open();
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_time()
        .build()
        .unwrap()
}

/// Draining waits for a submission already past the gate — here held in
/// planning — to register its job, then for that job, and takes no new
/// one; nothing is interrupted.
#[test]
fn draining_waits_for_submissions_in_flight_and_their_jobs() {
    let planning = Arc::new(Gate::default());
    let held = Arc::clone(&planning);
    let h = harness(Box::new(move || {
        held.wait();
        vec![run("%1")]
    }));
    // Declared before the release below, so on a failed assertion the gate
    // opens first and the runtime's blocked barrier can then finish.
    let rt = runtime();
    // However this test ends, the planning submission is let go, so a
    // failing assertion cannot leave a thread blocked forever.
    let _release = OpenOnDrop(Arc::clone(&planning));
    let service = Arc::clone(&h.service);
    let submitting = std::thread::spawn(move || {
        Service::submit(&service, "key", &overrides(&["%1"]), Origin::Public, None)
    });
    // Let the submission pass the gate and block in planning.
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        !rt.block_on(Service::drain(
            Arc::clone(&h.service),
            Duration::from_millis(100)
        )),
        "a submission still planning keeps the drain from finishing"
    );
    planning.open();
    let accepted = submitting.join().unwrap().unwrap();
    assert!(rt.block_on(Service::drain(
        Arc::clone(&h.service),
        Duration::from_secs(10)
    )));
    assert_eq!(settled(&h.service, &accepted.job_id).state, "deferred");
    assert!(h.executed.lock().unwrap().is_empty());
    assert!(Service::submit(
        &h.service,
        "next",
        &overrides(&["%2"]),
        Origin::Public,
        None
    )
    .unwrap_err()
    .contains("shutting down"));
}

/// `wait` reads a finished job within its bound, and a status read that
/// cannot finish in time is reported as such, not as a running job.
#[test]
fn waiting_covers_reading_the_result() {
    let h = harness(Box::new(|| vec![run("%1")]));
    let accepted =
        Service::submit(&h.service, "key", &overrides(&["%1"]), Origin::Public, None).unwrap();
    let rt = runtime();
    let view = rt
        .block_on(Service::wait(
            Arc::clone(&h.service),
            &accepted.job_id,
            Duration::from_secs(10),
        ))
        .unwrap();
    assert_eq!(view.state, "done");

    let held = h.service.store.hold();
    let late = rt
        .block_on(Service::wait(
            Arc::clone(&h.service),
            &accepted.job_id,
            Duration::from_millis(100),
        ))
        .unwrap_err();
    drop(held);
    assert!(late.contains("has ended"), "{late}");
}

#[test]
fn oversized_requests_and_keys_are_refused() {
    let h = harness(Box::new(|| vec![run("%1")]));
    let many = SwapOverrides {
        targets: (0..300).map(|i| format!("%{i}")).collect(),
        ..SwapOverrides::default()
    };
    assert!(Service::submit(&h.service, "key", &many, Origin::Public, None).is_err());
    let long = SwapOverrides {
        continue_text: Some("x".repeat(5000)),
        ..SwapOverrides::default()
    };
    assert!(Service::submit(&h.service, "key", &long, Origin::Public, None).is_err());
    for key in ["", "has space", &"k".repeat(200)] {
        assert!(
            Service::submit(&h.service, key, &overrides(&["%1"]), Origin::Public, None).is_err()
        );
    }
    assert!(h.executed.lock().unwrap().is_empty());
}

/// One logical firing is one key, however it is delivered; a later episode
/// is another; two spellings of the pane's server are the same firing.
#[test]
fn an_automation_firing_has_one_key_per_episode() {
    let key = |socket: Option<&str>, episode: &str| {
        automation_request_key("node", "%1", socket, "sid", "swap-on-limit", episode)
    };
    assert_eq!(key(Some("default"), "e1"), key(Some("default"), "e1"));
    assert_eq!(
        key(Some("/tmp/tmux-1/default"), "e1"),
        key(Some("default"), "e1")
    );
    assert_ne!(key(Some("default"), "e1"), key(Some("default"), "e2"));
    validate_key(&key(None, "e1")).unwrap();
}

// --- retry -----------------------------------------------------------------

fn retry(h: &Harness, key: &str, parent: &str) -> Result<Accepted, String> {
    Service::retry(
        &h.service,
        key,
        &JobRef::Id(parent.into()),
        &RetryOverrides::default(),
    )
}

fn reasons(view: &JobView) -> Vec<(String, Option<String>, Option<String>)> {
    view.entries
        .iter()
        .map(|e| {
            (
                e.pane.clone().unwrap_or_default(),
                e.disposition.clone(),
                e.reason.clone(),
            )
        })
        .collect()
}

/// A parent whose targets ended three ways: %a failed before its
/// relaunch, %b after it, %c swapped. (An uncertain effect halts a live
/// service; the store's own tests cover it.)
fn mixed_parent(h: &Harness) -> String {
    h.failing.lock().unwrap().extend([
        ("%a".to_owned(), Fail::BeforeLaunch),
        ("%b".to_owned(), Fail::AfterLaunch),
    ]);
    let job = Service::submit(
        &h.service,
        "parent",
        &overrides(&["%a", "%b", "%c"]),
        Origin::Public,
        None,
    )
    .unwrap()
    .job_id;
    settled(&h.service, &job);
    job
}

/// Only the target that failed before its relaunch runs again, on its
/// first account, with no follow-up and no draft; the others are looked at
/// and nothing is done to them.
#[test]
fn a_retry_runs_again_only_what_failed_before_its_relaunch() {
    let h = harness(Box::new(|| vec![run("%a"), run("%b"), run("%c")]));
    let parent = mixed_parent(&h);
    h.executed.lock().unwrap().clear();
    h.requests.lock().unwrap().clear();
    let accepted = retry(&h, "retry-1", &parent).unwrap();
    let view = settled(&h.service, &accepted.job_id);
    assert_eq!(view.state, "done");
    assert_eq!(*h.executed.lock().unwrap(), ["%a"]);
    let observed = h.observed.lock().unwrap().clone();
    assert_eq!(observed, ["%b"]);
    let rows = reasons(&view);
    let row = |pane: &str| rows.iter().find(|r| r.0 == pane).cloned().unwrap();
    assert_eq!(row("%a").1.as_deref(), Some("swapped"));
    assert_eq!(row("%b").2.as_deref(), Some("observed_unbound"));
    assert_eq!(row("%c").2.as_deref(), Some("not_retried"));
    let requests = h.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].continue_when, ContinueWhen::Never);
    assert_eq!(requests[0].continue_text, None);
    assert!(!requests[0].restore_draft);
    assert!(!requests[0].allow_direct_respawn);
    assert_eq!(requests[0].to, AccountChoice::Named("b".into()));
}

/// A resend after a lost answer gets the job it made, even once a later
/// retry has moved on.
#[test]
fn a_resent_retry_gets_its_own_job_back() {
    let h = harness(Box::new(|| vec![run("%a")]));
    h.failing
        .lock()
        .unwrap()
        .insert("%a".into(), Fail::BeforeLaunch);
    let parent = Service::submit(&h.service, "p", &overrides(&["%a"]), Origin::Public, None)
        .unwrap()
        .job_id;
    settled(&h.service, &parent);
    let first = retry(&h, "r1", &parent).unwrap();
    settled(&h.service, &first.job_id);
    let again = retry(&h, "r1", &parent).unwrap();
    assert!(again.existing);
    assert_eq!(again.job_id, first.job_id);
    // A different key on the same parent finds the target already retried.
    let sibling = retry(&h, "r2", &parent).unwrap();
    let view = settled(&h.service, &sibling.job_id);
    assert_eq!(reasons(&view)[0].2.as_deref(), Some("retried_elsewhere"));
    assert_eq!(*h.executed.lock().unwrap(), ["%a"]);
}

/// A retried target that failed again before its relaunch is retried from
/// its retry, and the root it came from is not retried a second time.
#[test]
fn a_lineage_is_retried_from_its_newest_attempt_only() {
    let h = harness(Box::new(|| vec![run("%a")]));
    h.failing
        .lock()
        .unwrap()
        .insert("%a".into(), Fail::BeforeLaunch);
    let root = Service::submit(&h.service, "p", &overrides(&["%a"]), Origin::Public, None)
        .unwrap()
        .job_id;
    settled(&h.service, &root);
    h.failing
        .lock()
        .unwrap()
        .insert("%a".into(), Fail::BeforeLaunch);
    let child = retry(&h, "r1", &root).unwrap();
    assert_eq!(
        settled(&h.service, &child.job_id).entries[0]
            .disposition
            .as_deref(),
        Some("failed")
    );
    let again = retry(&h, "r2", &root).unwrap();
    assert_eq!(
        reasons(&settled(&h.service, &again.job_id))[0].2.as_deref(),
        Some("retried_elsewhere")
    );
    let grandchild = retry(&h, "r3", &child.job_id).unwrap();
    assert_eq!(
        settled(&h.service, &grandchild.job_id).entries[0]
            .disposition
            .as_deref(),
        Some("swapped")
    );
}

#[test]
fn a_running_job_or_an_unknown_one_is_not_retried() {
    let gate = Arc::new(Gate::default());
    let held = Arc::clone(&gate);
    let h = harness(Box::new(move || {
        vec![EngineStep::Run(FakeTarget {
            identity: identity("%1", "/tmp/tmux-1/default"),
            gate: Some(Arc::clone(&held)),
        })]
    }));
    let _open = OpenOnDrop(Arc::clone(&gate));
    let running = Service::submit(&h.service, "p", &overrides(&["%1"]), Origin::Public, None)
        .unwrap()
        .job_id;
    assert!(retry(&h, "r", &running)
        .unwrap_err()
        .contains("still running"));
    assert!(retry(&h, "r", "swap-nope")
        .unwrap_err()
        .contains("not found"));
    gate.open();
    settled(&h.service, &running);
}

/// A pane another job is acting on is only reported busy.
#[test]
fn a_retry_leaves_a_busy_pane_alone() {
    let h = harness(Box::new(|| vec![run("%a")]));
    h.failing
        .lock()
        .unwrap()
        .insert("%a".into(), Fail::AfterLaunch);
    let parent = Service::submit(&h.service, "p", &overrides(&["%a"]), Origin::Public, None)
        .unwrap()
        .job_id;
    settled(&h.service, &parent);
    let endpoint = endpoint_key("%a", Some("/tmp/tmux-1/default"));
    assert!(h.service.reserve(&endpoint, "someone"));
    let accepted = retry(&h, "r", &parent).unwrap();
    assert_eq!(
        reasons(&settled(&h.service, &accepted.job_id))[0]
            .2
            .as_deref(),
        Some("busy")
    );
    assert!(h.observed.lock().unwrap().is_empty());
}

/// A retry made with another configuration file than the daemon's is
/// refused before anything; the daemon's own file, however spelled, is not.
#[test]
fn a_retry_with_another_configuration_is_refused() {
    let h = harness(Box::new(|| vec![run("%a")]));
    h.failing
        .lock()
        .unwrap()
        .insert("%a".into(), Fail::BeforeLaunch);
    let parent = Service::submit(&h.service, "p", &overrides(&["%a"]), Origin::Public, None)
        .unwrap()
        .job_id;
    settled(&h.service, &parent);
    h.executed.lock().unwrap().clear();
    let elsewhere = h.dir.path().join("other.toml");
    std::fs::write(&elsewhere, "").unwrap();
    let refused = Service::retry(
        &h.service,
        "r1",
        &JobRef::Id(parent.clone()),
        &RetryOverrides {
            caller_config: Some(elsewhere),
            ..RetryOverrides::default()
        },
    );
    assert!(refused
        .unwrap_err()
        .contains("swaps run with the daemon's configuration"));
    assert!(h.executed.lock().unwrap().is_empty());
    let same = h.dir.path().join(".").join("config.toml");
    let accepted = Service::retry(
        &h.service,
        "r2",
        &JobRef::Id(parent),
        &RetryOverrides {
            caller_config: Some(same),
            ..RetryOverrides::default()
        },
    )
    .unwrap();
    settled(&h.service, &accepted.job_id);
    assert_eq!(*h.executed.lock().unwrap(), ["%a"]);
}

// --- prompt receipts ---------------------------------------------------------

fn prompt(kind: AgentKind, session: &str, pane: &str, seq: u64) -> crate::state::PromptRecord {
    crate::state::PromptRecord {
        id: crate::event::AgentId {
            kind,
            session_id: session.into(),
            surface: None,
            pane: Some(pane.into()),
            tmux_socket: Some("/tmp/tmux-1/default".into()),
            cwd: None,
        },
        prompt: "secret text".into(),
        at: time::OffsetDateTime::now_utc(),
        model: None,
        seq,
        origin: crate::state::PromptOrigin::Unknown,
    }
}

/// Only a hook after the cursor, from this kind's conversation on this
/// pane, is evidence; an earlier one, or another pane's, is not.
#[test]
fn a_prompt_receipt_counts_only_after_its_cursor_and_on_its_pane() {
    use crate::account_swap::TurnProbe as _;
    let accepted = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let receipts = PromptReceipts::new(Arc::clone(&accepted));
    let endpoint = endpoint_key("%1", Some("/tmp/tmux-1/default"));
    // Accepted by the store (number 1) before the cursor, received after it.
    accepted.store(1, Ordering::SeqCst);
    let cursor = receipts.cursor();
    receipts.record(&prompt(AgentKind::ClaudeCode, "c", "%1", 1));
    assert!(!receipts.prompt_after(cursor, AgentKind::ClaudeCode, "c", &endpoint));
    receipts.record(&prompt(AgentKind::ClaudeCode, "c", "%2", 2));
    receipts.record(&prompt(AgentKind::ClaudeCode, "other", "%1", 3));
    receipts.record(&prompt(AgentKind::Codex, "c", "%1", 4));
    assert!(!receipts.prompt_after(cursor, AgentKind::ClaudeCode, "c", &endpoint));
    receipts.record(&prompt(AgentKind::ClaudeCode, "c", "%1", 5));
    assert!(receipts.prompt_after(cursor, AgentKind::ClaudeCode, "c", &endpoint));
}

// --- cancel ------------------------------------------------------------------

fn gated(pane: &str, gate: &Arc<Gate>) -> EngineStep<FakeTarget> {
    EngineStep::Run(FakeTarget {
        identity: identity(pane, "/tmp/tmux-1/default"),
        gate: Some(Arc::clone(gate)),
    })
}

/// A cancel lets the target already started finish and starts no other;
/// the panes it never touched are let go.
#[test]
fn a_cancel_stops_at_the_next_target() {
    let gate = Arc::new(Gate::default());
    let held = Arc::clone(&gate);
    let h = harness(Box::new(move || {
        vec![gated("%1", &held), run("%2"), run("%3")]
    }));
    let _open = OpenOnDrop(Arc::clone(&gate));
    let job = Service::submit(
        &h.service,
        "k",
        &overrides(&["%1", "%2", "%3"]),
        Origin::Public,
        None,
    )
    .unwrap()
    .job_id;
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.provider.acquired.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::sleep(Duration::from_millis(50));
    let view = h.service.cancel(&JobRef::Id(job.clone())).unwrap();
    assert!(view.cancel_requested);
    gate.open();
    let view = settled(&h.service, &job);
    assert_eq!(view.state, "done");
    assert_eq!(*h.executed.lock().unwrap(), ["%1"]);
    let reasons: Vec<_> = view
        .entries
        .iter()
        .map(|e| (e.disposition.clone(), e.reason.clone()))
        .collect();
    assert_eq!(reasons[0].0.as_deref(), Some("swapped"));
    assert_eq!(
        reasons[1],
        (Some("skipped".into()), Some("cancelled".into()))
    );
    assert_eq!(
        reasons[2],
        (Some("skipped".into()), Some("cancelled".into()))
    );
    // Every pane is free again.
    for pane in ["%1", "%2", "%3"] {
        assert!(h
            .service
            .reserve(&endpoint_key(pane, Some("/tmp/tmux-1/default")), "next"));
    }
    // Repeating it, on the ended job, changes nothing.
    let again = h.service.cancel(&JobRef::Id(job)).unwrap();
    assert_eq!(again.state, "done");
}

/// A cancel that lands while the target waits for its pane wins: the target
/// never starts, though the pane was taken.
#[test]
fn a_cancel_while_waiting_for_the_pane_keeps_the_target_from_starting() {
    let h = harness(Box::new(|| vec![run("%1")]));
    let gate = Arc::new(Gate::default());
    *h.provider.guard_gate.lock().unwrap() = Some(Arc::clone(&gate));
    let _open = OpenOnDrop(Arc::clone(&gate));
    let job = Service::submit(&h.service, "k", &overrides(&["%1"]), Origin::Public, None)
        .unwrap()
        .job_id;
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.provider.acquired.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    h.service.cancel(&JobRef::Id(job.clone())).unwrap();
    gate.open();
    let view = settled(&h.service, &job);
    assert_eq!(view.entries[0].reason.as_deref(), Some("cancelled"));
    assert!(h.executed.lock().unwrap().is_empty());
}

/// A service that takes no new job (draining) can still be told to stop
/// one, and a job it cannot find is an error.
#[test]
fn a_cancel_is_taken_while_draining() {
    let gate = Arc::new(Gate::default());
    let held = Arc::clone(&gate);
    let h = harness(Box::new(move || vec![gated("%1", &held), run("%2")]));
    let _open = OpenOnDrop(Arc::clone(&gate));
    let job = Service::submit(
        &h.service,
        "k",
        &overrides(&["%1", "%2"]),
        Origin::Public,
        None,
    )
    .unwrap()
    .job_id;
    h.service.draining.store(true, Ordering::SeqCst);
    assert!(h.service.active().is_err());
    assert!(
        h.service
            .cancel(&JobRef::Id(job.clone()))
            .unwrap()
            .cancel_requested
    );
    gate.open();
    let view = settled(&h.service, &job);
    assert_eq!(view.entries[1].reason.as_deref(), Some("cancelled"));
    assert!(h.service.cancel(&JobRef::Id("swap-nope".into())).is_err());
}

// --- automation still due ----------------------------------------------------

/// A firing that went stale while its swap was planned is refused before
/// any job exists, and its pane is let go.
#[test]
fn a_stale_automation_swap_is_refused_once_planned() {
    let h = harness(Box::new(|| vec![run("%1")]));
    let refused = Service::submit_checked(
        &h.service,
        "auto",
        &overrides(&["%1"]),
        Origin::Automation,
        None,
        Some(Box::new(|| Err("the agent's state changed".into()))),
    )
    .unwrap_err();
    assert!(refused.contains("no longer due"), "{refused}");
    assert!(h.service.status(&JobRef::Key("auto".into())).is_err());
    assert!(h
        .service
        .reserve(&endpoint_key("%1", Some("/tmp/tmux-1/default")), "next"));
    assert!(h.executed.lock().unwrap().is_empty());
}

/// One that goes stale after acceptance steps back at its target, with
/// nothing done.
#[test]
fn a_stale_automation_swap_steps_back_before_its_target_starts() {
    let h = harness(Box::new(|| vec![run("%1")]));
    let asked = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&asked);
    let job = Service::submit_checked(
        &h.service,
        "auto",
        &overrides(&["%1"]),
        Origin::Automation,
        None,
        Some(Box::new(move || {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(())
            } else {
                Err("its condition no longer holds".into())
            }
        })),
    )
    .unwrap()
    .job_id;
    let view = settled(&h.service, &job);
    assert_eq!(view.state, "done");
    assert_eq!(view.entries[0].disposition.as_deref(), Some("skipped"));
    assert!(h.executed.lock().unwrap().is_empty());
    assert_eq!(asked.load(Ordering::SeqCst), 2);
}

// --- health -------------------------------------------------------------------

/// Health reads the service's state and nothing else.
#[test]
fn health_reports_ready_draining_and_halted() {
    let h = harness(Box::new(Vec::new));
    let health = h.service.health();
    assert_eq!(health.readiness, SwapReadiness::Ready);
    assert_eq!(health.schema, Some(health.expected_schema));
    assert_eq!(health.jobs_running, 0);
    h.service.set_fault("journal".into());
    let halted = h.service.health();
    assert_eq!(halted.readiness, SwapReadiness::Halted);
    assert_eq!(halted.fault.as_deref(), Some("journal"));
    h.service.draining.store(true, Ordering::SeqCst);
    assert_eq!(h.service.health().readiness, SwapReadiness::Draining);
    assert_eq!(h.dry_runs.load(Ordering::SeqCst), 0);
}

/// A service that never opened keeps its reason: refused with it, and
/// reported by health, with no store schema to claim.
#[test]
fn an_unopened_service_reports_why() {
    let unopened = UnopenedSwap::new(
        "swap jobs are owned by another live manager".into(),
        JOB_SCHEMA,
    );
    assert!(!unopened.exists());
    let health = unopened.health();
    assert_eq!(health.readiness, SwapReadiness::Unavailable);
    assert_eq!(health.schema, None);
    assert!(health.reason.unwrap().contains("another live manager"));
    assert!(unopened
        .status(&JobRef::Key("k".into()))
        .unwrap_err()
        .contains("another live manager"));
}

// --- fault scoping -----------------------------------------------------------

/// A fault after a job ended changes nothing about it: its view carries no
/// fault of its own, only the service's health beside it. The job the
/// fault stopped still carries it.
#[test]
fn a_later_fault_does_not_rewrite_an_ended_job() {
    let gate = Arc::new(Gate::default());
    let held = Arc::clone(&gate);
    let h = harness(Box::new(move || {
        vec![EngineStep::Run(FakeTarget {
            identity: identity("%9", "/tmp/tmux-1/default"),
            gate: Some(Arc::clone(&held)),
        })]
    }));
    let _open = OpenOnDrop(Arc::clone(&gate));
    // A: its own pane, run to its end first.
    gate.open();
    let a = Service::submit(&h.service, "a", &overrides(&["%9"]), Origin::Public, None)
        .unwrap()
        .job_id;
    let a_view = settled(&h.service, &a);
    assert_eq!(a_view.state, "done");
    // B faults the service.
    h.service
        .set_fault("job b: the journal could not record target 0".into());
    let a_after = h.service.status(&JobRef::Id(a.clone())).unwrap();
    assert_eq!(a_after.fault, None);
    assert!(a_after.service_fault.is_some());
    assert_eq!(a_after.state, "done");
    assert_eq!(a_after.entries, a_view.entries);
}

/// A view from a daemon before the field reads as no service fault.
#[test]
fn a_job_view_without_service_fault_still_reads() {
    let old = r#"{"job_id":"swap-1","node_id":"n","state":"done","revision":1,
        "reason":null,"entries":[],"fault":null}"#;
    let view: JobView = serde_json::from_str(old).unwrap();
    assert_eq!(view.service_fault, None);
    assert!(!view.cancel_requested);
}

/// Only the newest ended jobs keep their rich outcomes in memory; an older
/// one still reads from its journal, as it would after a restart.
#[test]
fn ended_jobs_let_their_outcomes_go_beyond_a_bound() {
    let h = harness(Box::new(|| vec![run("%1")]));
    let mut jobs = Vec::new();
    for n in 0..=LIVE_ENDED_KEPT {
        let accepted = Service::submit(
            &h.service,
            &format!("key-{n}"),
            &overrides(&["%1"]),
            Origin::Public,
            None,
        )
        .unwrap();
        assert_eq!(settled(&h.service, &accepted.job_id).state, "done");
        // Ended, its waiters woken, before the next one.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !h.service.done.lock().unwrap().is_empty() {
            assert!(Instant::now() < deadline, "the job never ended");
            std::thread::sleep(Duration::from_millis(5));
        }
        jobs.push(accepted.job_id);
    }
    assert_eq!(h.service.live.lock().unwrap().len(), LIVE_ENDED_KEPT);
    let oldest = h.service.status(&JobRef::Id(jobs[0].clone())).unwrap();
    assert_eq!(oldest.state, "done");
    assert_eq!(oldest.entries[0].disposition.as_deref(), Some("swapped"));
    assert!(oldest.entries[0].outcome.is_none());
    let newest = h
        .service
        .status(&JobRef::Id(jobs.last().unwrap().clone()))
        .unwrap();
    assert!(newest.entries[0].outcome.is_some());
}

/// A job still running keeps its place however many others end meanwhile:
/// only ended jobs are ever let go.
#[test]
fn a_running_job_keeps_its_outcomes_while_others_end() {
    let gate = Arc::new(Gate::default());
    let held = Arc::clone(&gate);
    let calls = Arc::new(AtomicUsize::new(0));
    let h = harness(Box::new(move || {
        let call = calls.fetch_add(1, Ordering::SeqCst);
        vec![EngineStep::Run(FakeTarget {
            identity: identity(&format!("%{}", call + 100), "default"),
            gate: (call == 0).then(|| Arc::clone(&held)),
        })]
    }));
    let running = Service::submit(
        &h.service,
        "held",
        &overrides(&["%100"]),
        Origin::Public,
        None,
    )
    .unwrap();
    gate.until_reached();
    for n in 0..=LIVE_ENDED_KEPT {
        let accepted = Service::submit(
            &h.service,
            &format!("key-{n}"),
            &overrides(&[&format!("%{}", n + 101)]),
            Origin::Public,
            None,
        )
        .unwrap();
        assert_eq!(settled(&h.service, &accepted.job_id).state, "done");
        let deadline = Instant::now() + Duration::from_secs(10);
        while h
            .service
            .done
            .lock()
            .unwrap()
            .contains_key(&accepted.job_id)
        {
            assert!(Instant::now() < deadline, "the job never ended");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    assert!(h.service.live.lock().unwrap().contains_key(&running.job_id));
    assert_eq!(h.service.live.lock().unwrap().len(), LIVE_ENDED_KEPT + 1);
    gate.open();
    let view = settled(&h.service, &running.job_id);
    assert_eq!(view.state, "done");
    assert!(view.entries[0].outcome.is_some());
}

/// Old evidence is pruned by the service in upkeep: the job still reads
/// done and swapped, says its detail was pruned, and a failed run is told
/// as maintenance health, never a fault; a run is due once a day.
#[test]
fn the_service_prunes_old_evidence_as_upkeep() {
    let h = harness(Box::new(|| vec![run("%1")]));
    let accepted =
        Service::submit(&h.service, "key", &overrides(&["%1"]), Origin::Public, None).unwrap();
    assert_eq!(settled(&h.service, &accepted.job_id).state, "done");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !h.service.done.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "the job never ended");
        std::thread::sleep(Duration::from_millis(5));
    }
    h.service
        .store
        .hold()
        .execute(
            "UPDATE jobs SET updated_at = '2001-01-01T00:00:00Z' WHERE job_id = ?1",
            [&accepted.job_id],
        )
        .unwrap();
    // Not due yet: it ran when the service opened.
    h.service.prune_when_due();
    assert!(h
        .service
        .status(&JobRef::Id(accepted.job_id.clone()))
        .unwrap()
        .evidence_pruned_at
        .is_none());
    assert_eq!(h.service.prune_evidence(), 1);
    let view = h
        .service
        .status(&JobRef::Id(accepted.job_id.clone()))
        .unwrap();
    assert_eq!(view.state, "done");
    assert_eq!(view.entries[0].disposition.as_deref(), Some("swapped"));
    assert!(view.evidence_pruned_at.is_some());
    assert!(h.service.health().maintenance.is_none());
    // Another manager takes the store: pruning fails, and says so.
    let other =
        crate::swap_job::SwapJobStore::open(&h.dir.path().join("swap-jobs.sqlite3")).unwrap();
    other
        .acquire(
            &OwnerIdentity {
                nonce: "second".into(),
                pid: 2,
                process_start: Some("2".into()),
            },
            &gone,
        )
        .unwrap();
    h.service.prune_evidence();
    let health = h.service.health();
    assert!(
        health
            .maintenance
            .as_deref()
            .is_some_and(|m| m.contains("pruning")),
        "{health:?}"
    );
    assert!(health.fault.is_none());
}

fn test_agent(pane: &str, sid: &str, kind: AgentKind, state: crate::event::AgentState) -> Agent {
    let at = time::macros::datetime!(2026-10-15 12:00:00 UTC);
    Agent {
        ask_origin: None,
        tmux_socket: None,
        tmux_session: None,
        kind,
        session_id: sid.into(),
        surface: None,
        pid: None,
        workload: crate::WorkloadSummary::default(),
        subagents: Vec::new(),
        pane: Some(pane.into()),
        cwd: None,
        state,
        last_prompt: None,
        last_prompt_at: None,
        last_response: None,
        recap: None,
        ai_title: None,
        pending_command: None,
        last_notification: None,
        model: None,
        context_used_pct: None,
        cost_usd: None,
        rate_limit_5h_pct: None,
        rate_limit_5h_resets_at: None,
        rate_limit_7d_pct: None,
        rate_limit_7d_resets_at: None,
        rate_limited_until: None,
        rate_limit_scope: None,
        rate_limit_source: None,
        started_at: at,
        last_activity_at: at,
        state_entered_at: at,
        last_prompt_origin: None,
        last_event_at: None,
        last_response_at: None,
        awaiting_reply: None,
    }
}

fn waiting_agent(pane: &str, busy: bool) -> Agent {
    let mut agent = test_agent(
        pane,
        &format!("conv-{pane}"),
        AgentKind::ClaudeCode,
        crate::event::AgentState::Idle,
    );
    agent.pane = Some(pane.into());
    agent.tmux_socket = Some("/tmp/tmux-1/default".into());
    agent.state = if busy {
        crate::event::AgentState::Working
    } else {
        crate::event::AgentState::Idle
    };
    agent.last_activity_at = time::OffsetDateTime::now_utc() - time::Duration::minutes(1);
    agent
}

#[test]
fn pending_targets_rotate_then_use_fresh_idle_state() {
    let h = harness(Box::new(|| vec![run("%1"), run("%2")]));
    *h.provider.agents.lock().unwrap() =
        vec![waiting_agent("%1", true), waiting_agent("%2", false)];
    let request = SwapOverrides {
        when_idle: true,
        ..overrides(&["%1", "%2"])
    };
    let accepted = Service::submit(&h.service, "idle", &request, Origin::Public, None).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.executed.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(*h.executed.lock().unwrap(), vec!["%2"]);
    let view = h
        .service
        .status(&JobRef::Id(accepted.job_id.clone()))
        .unwrap();
    assert!(view.entries[0].disposition.is_none());
    assert_eq!(view.state, "running");
    h.provider.agents.lock().unwrap()[0] = waiting_agent("%1", false);
    let view = settled(&h.service, &accepted.job_id);
    assert_eq!(view.state, "done");
    assert_eq!(*h.executed.lock().unwrap(), vec!["%2", "%1"]);
}

#[test]
fn idle_timeout_reports_without_executing() {
    let h = harness(Box::new(|| vec![run("%1")]));
    *h.provider.agents.lock().unwrap() = vec![waiting_agent("%1", true)];
    let request = SwapOverrides {
        when_idle: true,
        idle_timeout: Some("0".into()),
        ..overrides(&["%1"])
    };
    let accepted = Service::submit(&h.service, "timeout", &request, Origin::Public, None).unwrap();
    let view = settled(&h.service, &accepted.job_id);
    assert_eq!(view.state, "done");
    assert!(h.executed.lock().unwrap().is_empty());
    assert!(view.entries[0]
        .outcome
        .as_ref()
        .unwrap()
        .note
        .contains("idle timeout"));
    assert_eq!(view.entries[0].reason.as_deref(), Some("idle_timeout"));
    // The reason survives without the in-memory outcome text.
    h.service.live.lock().unwrap().clear();
    let durable = h.service.status(&JobRef::Id(accepted.job_id)).unwrap();
    assert_eq!(durable.entries[0].reason.as_deref(), Some("idle_timeout"));
}

#[test]
fn cancelling_idle_wait_releases_target_without_executing() {
    let h = harness(Box::new(|| vec![run("%1")]));
    *h.provider.agents.lock().unwrap() = vec![waiting_agent("%1", true)];
    let request = SwapOverrides {
        when_idle: true,
        ..overrides(&["%1"])
    };
    let accepted =
        Service::submit(&h.service, "cancel-idle", &request, Origin::Public, None).unwrap();
    h.service
        .cancel(&JobRef::Id(accepted.job_id.clone()))
        .unwrap();
    let view = settled(&h.service, &accepted.job_id);
    assert!(view.cancel_requested);
    assert!(h.executed.lock().unwrap().is_empty());
    assert_eq!(view.entries[0].disposition.as_deref(), Some("skipped"));
    assert_eq!(view.entries[0].reason.as_deref(), Some("cancelled"));
}

#[test]
fn force_takes_precedence_over_idle_wait() {
    let h = harness(Box::new(|| vec![run("%1")]));
    *h.provider.agents.lock().unwrap() = vec![waiting_agent("%1", true)];
    let request = SwapOverrides {
        when_idle: true,
        force: true,
        ..overrides(&["%1"])
    };
    let accepted =
        Service::submit(&h.service, "force-idle", &request, Origin::Public, None).unwrap();
    let view = settled(&h.service, &accepted.job_id);
    assert_eq!(view.entries[0].disposition.as_deref(), Some("swapped"));
}

#[test]
fn idle_wait_releases_input_guards_and_allows_other_jobs_but_reserves_its_target() {
    let calls = Arc::new(AtomicUsize::new(0));
    let next = Arc::clone(&calls);
    let h = harness(Box::new(move || {
        let pane = ["%1", "%2", "%3", "%1"][next.fetch_add(1, Ordering::SeqCst)];
        vec![run(pane)]
    }));
    *h.provider.agents.lock().unwrap() = vec![waiting_agent("%1", true)];
    let request = SwapOverrides {
        when_idle: true,
        ..overrides(&["%1"])
    };
    let waiting = Service::submit(&h.service, "waiting", &request, Origin::Public, None).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let checked = h.provider.released_guards.load(Ordering::SeqCst) > 0;
        if checked && h.provider.held_guards.load(Ordering::SeqCst) == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "idle wait retained its input guard"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // Public requests (including Fleet relay) and automation use the same
    // manager while the first job remains pending for another endpoint.
    for (key, pane, origin) in [
        ("public", "%2", Origin::Public),
        ("automation", "%3", Origin::Automation),
    ] {
        let guard = (origin == Origin::Automation).then(|| {
            Box::new(FakeGuard(endpoint_key(pane, Some("/tmp/tmux-1/default"))))
                as Box<dyn TargetGuard>
        });
        let accepted =
            Service::submit(&h.service, key, &overrides(&[pane]), origin, guard).unwrap();
        assert_eq!(
            settled(&h.service, &accepted.job_id).entries[0]
                .disposition
                .as_deref(),
            Some("swapped")
        );
    }
    assert_eq!(*h.executed.lock().unwrap(), ["%2", "%3"]);
    assert_eq!(
        h.service
            .status(&JobRef::Id(waiting.job_id.clone()))
            .unwrap()
            .state,
        "running"
    );
    let duplicate = Service::submit(
        &h.service,
        "duplicate",
        &overrides(&["%1"]),
        Origin::Public,
        None,
    )
    .unwrap();
    let duplicate = settled(&h.service, &duplicate.job_id);
    assert_eq!(duplicate.entries[0].reason.as_deref(), Some("busy"));
    assert!(duplicate.entries[0]
        .outcome
        .as_ref()
        .unwrap()
        .note
        .contains("reserved"));
    let repeated = Service::submit(&h.service, "waiting", &request, Origin::Public, None).unwrap();
    assert!(repeated.existing);
    assert_eq!(repeated.job_id, waiting.job_id);
    h.service
        .cancel(&JobRef::Id(waiting.job_id.clone()))
        .unwrap();
    assert_eq!(settled(&h.service, &waiting.job_id).state, "done");
    assert!(h.service.reserved.lock().unwrap().is_empty());
    assert!(h.executed.lock().unwrap().iter().all(|pane| pane != "%1"));
}

fn resume_fixture(h: &Harness, count: usize) -> AcceptedPlan {
    *h.provider.agents.lock().unwrap() = (0..count)
        .map(|id| waiting_agent(&format!("%{id}"), false))
        .collect();
    AcceptedPlan {
        node_id: "node-1".into(),
        continue_when: "always".into(),
        continue_text: Some(TextRef::of(
            &h.provider.config.lock().unwrap().continue_text,
        )),
        targets: (0..count)
            .map(|id| planned_target(id, &identity(&format!("%{id}"), "/tmp/tmux-1/default")))
            .collect(),
        settled: Vec::new(),
        resume: Some(ResumePolicy {
            public: true,
            when_idle: false,
            idle_deadline: time::OffsetDateTime::now_utc().unix_timestamp() + 3600,
            restore_draft: true,
            continue_when: ContinueWhen::Always,
            continue_evidence: ContinueEvidence::Verified,
            bare: BareMode::Skip,
            force: false,
            configured_text: true,
        }),
    }
}

fn write_fixture(h: &Harness, plan: &AcceptedPlan) -> String {
    let job = h
        .service
        .store
        .create_job(&h.service.lease, "resume-key", "digest", plan)
        .unwrap()
        .job_id;
    h.service.store.start(&h.service.lease, &job).unwrap();
    job
}

fn reopen_fixture(h: &Harness) -> Arc<Service<FakeEngine>> {
    Arc::new(
        Service::open(
            &h.dir.path().join("swap-jobs.sqlite3"),
            OwnerIdentity {
                nonce: "restarted".into(),
                pid: 2,
                process_start: Some("2".into()),
            },
            &gone,
            FakeEngine {
                script: Box::new(Vec::new),
                executed: Arc::clone(&h.executed),
                dry_runs: Arc::clone(&h.dry_runs),
                failing: Arc::clone(&h.failing),
                observed: Arc::clone(&h.observed),
                requests: Arc::clone(&h.requests),
            },
            Arc::clone(&h.provider) as Arc<dyn ContextProvider>,
        )
        .unwrap(),
    )
}

#[test]
fn resumed_noncontiguous_targets_keep_their_job_key_and_report_positions() {
    let h = harness(Box::new(Vec::new));
    let plan = resume_fixture(&h, 4);
    let job = write_fixture(&h, &plan);
    for id in [0, 2] {
        h.service
            .store
            .finish_target(&h.service.lease, &job, id, 1, Disposition::Swapped)
            .unwrap();
    }
    h.service.store.begin_drain(&h.service.lease).unwrap();
    let next = reopen_fixture(&h);
    Service::resume_pending(&next).unwrap();
    let view = settled(&next, &job);
    assert_eq!(view.state, "done");
    assert_eq!(view.entries.len(), 4);
    for (id, entry) in view.entries.iter().enumerate() {
        assert_eq!(entry.pane.as_deref(), Some(format!("%{id}").as_str()));
        assert_eq!(entry.disposition.as_deref(), Some("swapped"));
    }
    let journal = next.store.status(&job).unwrap();
    assert_eq!(
        journal
            .targets
            .iter()
            .map(|t| t.attempt)
            .collect::<Vec<_>>(),
        [1, 2, 1, 2]
    );
    assert_eq!(
        next.status(&JobRef::Key("resume-key".into()))
            .unwrap()
            .job_id,
        job
    );
    Service::resume_pending(&next).unwrap();
    assert_eq!(*h.executed.lock().unwrap(), ["%1", "%3"]);
    let requests = h.requests.lock().unwrap();
    assert!(requests
        .iter()
        .all(|r| r.to == AccountChoice::Named("b".into())
            && r.restore_draft
            && r.continue_evidence == ContinueEvidence::Verified));
}

#[test]
fn resume_waits_for_transient_snapshots_guards_and_reservations_before_claiming() {
    let h = harness(Box::new(Vec::new));
    let plan = resume_fixture(&h, 1);
    let job = write_fixture(&h, &plan);
    h.service.store.begin_drain(&h.service.lease).unwrap();
    let next = reopen_fixture(&h);
    h.provider.snapshot_failure.store(true, Ordering::SeqCst);
    Service::resume_pending(&next).unwrap();
    assert_eq!(next.store.status(&job).unwrap().targets[0].attempt, 1);
    h.provider.snapshot_failure.store(false, Ordering::SeqCst);
    h.provider.guard_failure.store(true, Ordering::SeqCst);
    Service::resume_pending(&next).unwrap();
    assert_eq!(next.store.status(&job).unwrap().targets[0].attempt, 1);
    h.provider.guard_failure.store(false, Ordering::SeqCst);
    assert!(next.reserve(&plan.targets[0].endpoint, "other"));
    Service::resume_pending(&next).unwrap();
    assert_eq!(next.store.status(&job).unwrap().targets[0].attempt, 1);
    next.release(&plan.targets[0].endpoint);
    Service::resume_pending(&next).unwrap();
    assert_eq!(settled(&next, &job).state, "done");
    assert_eq!(*h.executed.lock().unwrap(), ["%0"]);
}

#[test]
fn resumed_text_requires_the_accepted_digest_and_configured_source() {
    for custom in [false, true] {
        let h = harness(Box::new(Vec::new));
        let mut plan = resume_fixture(&h, 1);
        if custom {
            plan.resume.as_mut().unwrap().configured_text = false;
        } else {
            h.provider.config.lock().unwrap().continue_text = "different config text".into();
        }
        let job = write_fixture(&h, &plan);
        h.service.store.begin_drain(&h.service.lease).unwrap();
        let next = reopen_fixture(&h);
        Service::resume_pending(&next).unwrap();
        let view = settled(&next, &job);
        assert_eq!(view.state, "done");
        let requests = h.requests.lock().unwrap();
        assert_eq!(requests[0].continue_when, ContinueWhen::Never);
        assert!(requests[0].continue_text.is_none());
        assert_eq!(
            view.entries[0].continuation.as_deref(),
            Some("text_unavailable")
        );
        next.live.lock().unwrap().clear();
        assert_eq!(
            next.status(&JobRef::Id(job)).unwrap().entries[0]
                .continuation
                .as_deref(),
            Some("text_unavailable")
        );
    }
}

#[test]
fn automatic_resume_respects_automation_legacy_node_deadline_and_changed_identity() {
    for reason in [
        "automation_not_resumed",
        "resume_policy_unavailable",
        "resume_node_changed",
        "resume_window_expired",
        "changed_while_deferred",
    ] {
        let h = harness(Box::new(Vec::new));
        let mut plan = resume_fixture(&h, 1);
        match reason {
            "automation_not_resumed" => plan.resume.as_mut().unwrap().public = false,
            "resume_policy_unavailable" => plan.resume = None,
            "resume_node_changed" => plan.node_id = "another-node".into(),
            "resume_window_expired" => {
                let p = plan.resume.as_mut().unwrap();
                p.when_idle = true;
                p.idle_deadline = 0;
            }
            "changed_while_deferred" => plan.targets[0].process_pid += 1,
            _ => unreachable!(),
        }
        let job = write_fixture(&h, &plan);
        h.service.store.begin_drain(&h.service.lease).unwrap();
        let next = reopen_fixture(&h);
        Service::resume_pending(&next).unwrap();
        let status = next.store.status(&job).unwrap();
        assert_eq!(status.targets[0].reason.as_deref(), Some(reason));
        assert_eq!(status.targets[0].attempt, 1);
        assert!(h.executed.lock().unwrap().is_empty());
    }
}

#[test]
fn draining_defers_idle_targets_and_pending_siblings_of_a_stuck_executor() {
    let gate = Arc::new(Gate::default());
    let held = Arc::clone(&gate);
    let h = harness(Box::new(move || {
        vec![
            EngineStep::Run(FakeTarget {
                identity: identity("%0", "/tmp/tmux-1/default"),
                gate: Some(Arc::clone(&held)),
            }),
            run("%1"),
        ]
    }));
    let _release = OpenOnDrop(Arc::clone(&gate));
    let accepted = Service::submit(
        &h.service,
        "mixed-drain",
        &overrides(&["%0", "%1"]),
        Origin::Public,
        None,
    )
    .unwrap();
    gate.until_reached();
    assert!(!runtime().block_on(Service::drain(
        Arc::clone(&h.service),
        Duration::from_millis(50)
    )));
    let view = h
        .service
        .status(&JobRef::Id(accepted.job_id.clone()))
        .unwrap();
    assert_eq!(view.entries[1].disposition.as_deref(), Some("deferred"));
    assert_eq!(view.entries[1].reason.as_deref(), Some("daemon_shutdown"));
    gate.open();
    assert_eq!(settled(&h.service, &accepted.job_id).state, "deferred");
    assert_eq!(*h.executed.lock().unwrap(), ["%0"]);

    let idle = harness(Box::new(|| vec![run("%1")]));
    *idle.provider.agents.lock().unwrap() = vec![waiting_agent("%1", true)];
    let accepted = Service::submit(
        &idle.service,
        "idle-drain",
        &SwapOverrides {
            when_idle: true,
            ..overrides(&["%1"])
        },
        Origin::Public,
        None,
    )
    .unwrap();
    assert!(runtime().block_on(Service::drain(
        Arc::clone(&idle.service),
        Duration::from_secs(3)
    )));
    assert_eq!(settled(&idle.service, &accepted.job_id).state, "deferred");
    assert!(idle.executed.lock().unwrap().is_empty());
}

#[test]
fn resumer_does_not_treat_unreadable_binding_as_a_changed_process() {
    let h = harness(Box::new(Vec::new));
    let mut plan = resume_fixture(&h, 1);
    plan.targets[0].process_start = None;
    let job = write_fixture(&h, &plan);
    h.service.store.begin_drain(&h.service.lease).unwrap();
    let next = reopen_fixture(&h);
    Service::resume_pending(&next).unwrap();
    let status = next.store.status(&job).unwrap();
    assert_eq!(status.state, "deferred");
    assert_eq!(status.targets[0].attempt, 1);
    assert!(h.executed.lock().unwrap().is_empty());
}

#[test]
fn a_refused_resumed_worker_leaves_its_new_attempt_deferred() {
    let h = harness(Box::new(Vec::new));
    let job = write_fixture(&h, &resume_fixture(&h, 1));
    h.service.store.begin_drain(&h.service.lease).unwrap();
    let next = reopen_fixture(&h);
    // No dispatcher/worker has cloned this new service yet.
    let mut next = Arc::try_unwrap(next).ok().unwrap();
    next.spawner = Box::new(|_, _| Err(std::io::Error::other("temporarily no threads")));
    let next = Arc::new(next);
    Service::resume_pending(&next).unwrap();
    let status = next.store.status(&job).unwrap();
    assert_eq!(status.state, "deferred");
    assert_eq!(status.targets[0].attempt, 2);
    assert_eq!(
        status.targets[0].reason.as_deref(),
        Some("worker_unavailable")
    );
    assert!(next.fault().is_none());
    assert!(h.executed.lock().unwrap().is_empty());
    assert!(next.receiver(&job).is_none());
}

#[test]
fn startup_dispatcher_resumes_without_a_new_submission() {
    let h = harness(Box::new(Vec::new));
    let job = write_fixture(&h, &resume_fixture(&h, 1));
    h.service.store.begin_drain(&h.service.lease).unwrap();
    let next = reopen_fixture(&h);
    Service::start_resumer(&next).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let view = next.status(&JobRef::Key("resume-key".into())).unwrap();
        if view.state == "done" {
            assert_eq!(view.job_id, job);
            assert_eq!(*h.executed.lock().unwrap(), ["%0"]);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "dispatcher did not resume: {view:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn expire_resume_window(service: &Service<FakeEngine>) {
    service
        .store
        .hold()
        .execute("UPDATE resume_windows SET deadline = 0", [])
        .unwrap();
}

#[test]
fn an_agent_closed_while_the_daemon_was_down_settles_as_target_gone() {
    for stopped_row in [false, true] {
        let h = harness(Box::new(Vec::new));
        let job = write_fixture(&h, &resume_fixture(&h, 1));
        h.service.store.begin_drain(&h.service.lease).unwrap();
        if stopped_row {
            h.provider.agents.lock().unwrap()[0].state = crate::event::AgentState::Stopped;
        } else {
            h.provider.agents.lock().unwrap().clear();
        }
        let next = reopen_fixture(&h);
        assert_eq!(Service::resume_pending(&next).unwrap(), 0);
        let view = next.status(&JobRef::Id(job)).unwrap();
        assert_eq!(view.state, "done");
        assert_eq!(view.entries[0].disposition.as_deref(), Some("skipped"));
        assert_eq!(view.entries[0].reason.as_deref(), Some("target_gone"));
        assert!(h.executed.lock().unwrap().is_empty());
    }
}

#[test]
fn unreadable_binding_and_snapshot_errors_expire_without_resetting_the_window() {
    for provider_error in [false, true] {
        let h = harness(Box::new(Vec::new));
        let mut plan = resume_fixture(&h, 1);
        plan.targets[0].process_start = None;
        let job = write_fixture(&h, &plan);
        h.service.store.begin_drain(&h.service.lease).unwrap();
        let next = reopen_fixture(&h);
        h.provider
            .snapshot_failure
            .store(provider_error, Ordering::SeqCst);
        Service::resume_pending(&next).unwrap();
        assert_eq!(next.store.status(&job).unwrap().state, "deferred");
        let deadline = next.store.resume_deadline(&job, 0, 1).unwrap();
        Service::resume_pending(&next).unwrap();
        assert_eq!(next.store.resume_deadline(&job, 0, 1).unwrap(), deadline);
        expire_resume_window(&next);
        Service::resume_pending(&next).unwrap();
        let view = next.status(&JobRef::Id(job)).unwrap();
        assert_eq!(view.state, "done");
        assert_eq!(
            view.entries[0].reason.as_deref(),
            Some("resume_window_expired")
        );
        assert!(h.executed.lock().unwrap().is_empty());
    }
}

#[test]
fn a_slow_resume_snapshot_does_not_hold_the_submission_gate_and_cancel_is_rechecked() {
    let h = harness(Box::new(Vec::new));
    let job = write_fixture(&h, &resume_fixture(&h, 1));
    h.service.store.begin_drain(&h.service.lease).unwrap();
    let next = reopen_fixture(&h);
    let gate = Arc::new(Gate::default());
    let _release = OpenOnDrop(Arc::clone(&gate));
    *h.provider.snapshot_gate.lock().unwrap() = Some(Arc::clone(&gate));
    let worker = Arc::clone(&next);
    let pass = std::thread::spawn(move || Service::resume_pending(&worker));
    gate.until_reached();
    assert!(
        next.submit.try_lock().is_ok(),
        "snapshot blocked new submissions"
    );
    next.cancel(&JobRef::Id(job.clone())).unwrap();
    gate.open();
    assert_eq!(pass.join().unwrap().unwrap(), 0);
    assert_eq!(
        next.store.status(&job).unwrap().targets[0]
            .reason
            .as_deref(),
        Some("cancelled")
    );
    assert!(h.executed.lock().unwrap().is_empty());
}

#[test]
fn resumed_worker_skips_a_successfully_observed_missing_registration() {
    let h = harness(Box::new(Vec::new));
    let mut plan = resume_fixture(&h, 1);
    plan.resume.as_mut().unwrap().when_idle = true;
    let job = write_fixture(&h, &plan);
    h.service.store.begin_drain(&h.service.lease).unwrap();
    let next = reopen_fixture(&h);
    let attempt = next
        .store
        .claim_resume(&next.lease, &job, 0, 1, false)
        .unwrap()
        .unwrap();
    h.provider.agents.lock().unwrap().clear();
    let id = identity("%0", "/tmp/tmux-1/default");
    let request = SwapRequest {
        when_idle: true,
        ..SwapRequest::new(
            SwapScope::Targets(vec!["conv-%0".into()]),
            &SwapConfig::default(),
        )
    };
    next.run(
        &job,
        &request,
        vec![AttemptRun {
            target_id: 0,
            attempt,
            report_index: 0,
            identity: id.clone(),
            target: FakeTarget {
                identity: id,
                gate: None,
            },
        }],
        None,
        None,
    );
    let view = next.status(&JobRef::Id(job)).unwrap();
    assert_eq!(view.state, "done");
    assert_eq!(view.entries[0].reason.as_deref(), Some("target_gone"));
    assert!(h.executed.lock().unwrap().is_empty());
}

#[test]
fn a_resumer_stops_when_no_targets_are_deferred() {
    let h = harness(Box::new(Vec::new));
    let calls = h.provider.snapshot_calls.load(Ordering::SeqCst);
    Service::start_resumer(&h.service).unwrap();
    assert!(!h.service.resumer_running.load(Ordering::SeqCst));
    assert_eq!(h.provider.snapshot_calls.load(Ordering::SeqCst), calls);

    let job = write_fixture(&h, &resume_fixture(&h, 1));
    h.service.store.begin_drain(&h.service.lease).unwrap();
    h.provider.agents.lock().unwrap().clear();
    let next = reopen_fixture(&h);
    Service::start_resumer(&next).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while next.resumer_running.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "resumer did not stop");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(next.store.status(&job).unwrap().state, "done");
    let calls = h.provider.snapshot_calls.load(Ordering::SeqCst);
    Service::start_resumer(&next).unwrap();
    assert!(!next.resumer_running.load(Ordering::SeqCst));
    assert_eq!(h.provider.snapshot_calls.load(Ordering::SeqCst), calls);
}

#[test]
fn backend_absence_requires_a_complete_scan() {
    use crate::backend::PaneObservation;
    assert!(pane_absent(
        "%1",
        Some("/tmp/default"),
        &PaneObservation::complete(Vec::new())
    ));
    assert!(!pane_absent(
        "%1",
        Some("/tmp/default"),
        &PaneObservation::incomplete(Vec::new())
    ));
    assert!(!pane_absent(
        "%1",
        Some("/tmp/default"),
        &PaneObservation::partial(Vec::new())
    ));
}

#[test]
fn resume_backoff_reaches_thirty_seconds_and_resets_after_admission() {
    let mut backoff = super::resume::ResumeBackoff::default();
    for seconds in [1, 2, 4, 8, 16, 30, 30] {
        assert_eq!(backoff.after_pass(0), Duration::from_secs(seconds));
    }
    assert_eq!(backoff.after_pass(1), Duration::from_secs(1));
    assert_eq!(backoff.after_pass(0), Duration::from_secs(2));
}

#[test]
fn a_worker_deferral_restarts_admission_after_the_dispatcher_stopped() {
    let h = harness(Box::new(Vec::new));
    let job = write_fixture(&h, &resume_fixture(&h, 1));
    h.service.store.begin_drain(&h.service.lease).unwrap();
    let next = reopen_fixture(&h);
    let attempt = next
        .store
        .claim_resume(&next.lease, &job, 0, 1, false)
        .unwrap()
        .unwrap();
    assert!(!next.resumer_running.load(Ordering::SeqCst));
    h.provider.guard_failure.store(true, Ordering::SeqCst);
    h.provider.snapshot_failure.store(true, Ordering::SeqCst);
    next.done
        .lock()
        .unwrap()
        .insert(job.clone(), tokio::sync::watch::channel(false).0);
    let id = identity("%0", "/tmp/tmux-1/default");
    let request = SwapRequest::new(
        SwapScope::Targets(vec!["conv-%0".into()]),
        &SwapConfig::default(),
    );
    Service::spawn(
        &next,
        &job,
        request,
        vec![AttemptRun {
            target_id: 0,
            attempt,
            report_index: 0,
            identity: id.clone(),
            target: FakeTarget {
                identity: id.clone(),
                gate: None,
            },
        }],
        None,
        1,
        std::slice::from_ref(&id.endpoint),
        None,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !next.resumer_running.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "worker did not restart the dispatcher"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(next.store.status(&job).unwrap().state, "deferred");
    h.provider.guard_failure.store(false, Ordering::SeqCst);
    h.provider.snapshot_failure.store(false, Ordering::SeqCst);
    loop {
        let view = next.status(&JobRef::Id(job.clone())).unwrap();
        if view.state == "done" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "restarted dispatcher did not admit: {view:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(*h.executed.lock().unwrap(), ["%0"]);
}
