//! The service with a scripted engine and daemon: no pane, no process, no
//! live swap. Targets record what they were asked to do and can be held
//! mid-run to test what happens around a running job.

use super::*;
use crate::account_swap::effects::{Effect, Key, Op, Purpose, Transport};
use crate::account_swap::{endpoint_key, SessionIdSource};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// Blocks a target until opened.
#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
}

impl Gate {
    fn open(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }

    fn wait(&self) {
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.changed.wait(open).unwrap();
        }
    }
}

struct FakeTarget {
    identity: TargetIdentity,
    gate: Option<Arc<Gate>>,
}

/// What the engine plans, per call to `prepare`.
type Script = Box<dyn Fn() -> Vec<EngineStep<FakeTarget>> + Send + Sync>;

struct FakeEngine {
    script: Script,
    executed: Arc<Mutex<Vec<String>>>,
    dry_runs: Arc<AtomicUsize>,
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

struct FakeProvider {
    config_path: Option<PathBuf>,
    config: Mutex<SwapConfig>,
    acquired: Mutex<Vec<String>>,
}

impl ContextProvider for FakeProvider {
    fn snapshot(&self) -> Result<Snapshot, String> {
        Ok(Snapshot {
            node_id: "node-1".into(),
            agents: Vec::new(),
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
        let endpoint = endpoint_key(pane, socket);
        self.acquired.lock().unwrap().push(endpoint.clone());
        Ok(Box::new(FakeGuard(endpoint)))
    }
}

struct Harness {
    dir: tempfile::TempDir,
    service: Arc<Service<FakeEngine>>,
    provider: Arc<FakeProvider>,
    executed: Arc<Mutex<Vec<String>>>,
    dry_runs: Arc<AtomicUsize>,
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
        config_path: Some(config_path),
        config: Mutex::new(SwapConfig::default()),
        acquired: Mutex::new(Vec::new()),
    });
    let executed = Arc::new(Mutex::new(Vec::new()));
    let dry_runs = Arc::new(AtomicUsize::new(0));
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
    // Another manager takes the store while the first target is held.
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
    // The daemon "restarts" while the target is held, before it runs.
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
        },
        Arc::clone(&h.provider) as Arc<dyn ContextProvider>,
    )
    .unwrap();
    let view = restarted.status(&JobRef::Key("key".into())).unwrap();
    assert_eq!(view.job_id, accepted.job_id);
    assert_eq!(view.state, "abandoned");
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
            config_path: None,
            config: Mutex::new(SwapConfig::default()),
            acquired: Mutex::new(Vec::new()),
        }) as Arc<dyn ContextProvider>
    };
    let first = SwapService::open(Some(&path), provider()).unwrap();
    assert!(SwapService::open(Some(&path), provider())
        .err()
        .unwrap()
        .contains("another live manager"));
    assert!(first.active().is_ok());
    drop(first);
    let second = SwapService::open(Some(&path), provider()).unwrap();
    assert!(second.active().is_ok());
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
        },
        Arc::new(FakeProvider {
            config_path: None,
            config: Mutex::new(SwapConfig::default()),
            acquired: Mutex::new(Vec::new()),
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
            config_path: None,
            config: Mutex::new(SwapConfig::default()),
            acquired: Mutex::new(Vec::new()),
        }) as Arc<dyn ContextProvider>
    };

    // A worker's reference keeps the manager's lock.
    let first = SwapService::open(Some(&path), provider()).unwrap();
    let worker = Arc::clone(&first.inner);
    drop(first);
    assert!(SwapService::open(Some(&path), provider()).is_err());
    drop(worker);
    drop(SwapService::open(Some(&path), provider()).unwrap());

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
    assert_eq!(settled(&h.service, &accepted.job_id).state, "done");
    assert_eq!(*h.executed.lock().unwrap(), ["%1"]);
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
