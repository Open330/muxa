use super::*;
use crate::account_swap::effects::Key;

fn me(nonce: &str, pid: u32) -> OwnerIdentity {
    OwnerIdentity {
        nonce: nonce.into(),
        pid,
        process_start: Some("100".into()),
    }
}

fn alive(_: u32, _: Option<&str>) -> Liveness {
    Liveness::Alive
}

fn gone(_: u32, _: Option<&str>) -> Liveness {
    Liveness::Gone
}

fn plan(targets: usize, text: Option<&str>) -> AcceptedPlan {
    AcceptedPlan {
        node_id: "node-1".into(),
        continue_when: "limited".into(),
        continue_text: text.map(TextRef::of),
        targets: (0..targets)
            .map(|i| PlannedTarget {
                host: "rmux".into(),
                socket: Some("/tmp/rmux-1/default".into()),
                pane: format!("rmux:%{i}"),
                provider: "claude".into(),
                conversation_id: format!("conv-{i}"),
                session_id_source: "claude_session_file".into(),
                conversation_verified: true,
                process_pid: 1000 + u32::try_from(i).unwrap(),
                process_start: Some("42".into()),
                from_account: Some("a@claude".into()),
                assigned_account: "b@claude".into(),
            })
            .collect(),
    }
}

fn effect(purpose: Purpose, op: Op) -> Effect {
    Effect {
        group: 0,
        purpose,
        op,
    }
}

fn open(dir: &tempfile::TempDir) -> SwapJobStore {
    SwapJobStore::open(&dir.path().join("muxa/swap-jobs.sqlite3")).unwrap()
}

/// A running job with `targets` targets under `lease`.
fn running(store: &SwapJobStore, lease: &Lease, key: &str, targets: usize) -> String {
    let plan = plan(targets, Some("continue"));
    let job = store
        .create_job(lease, key, &digest(&[key], Some("continue")), &plan)
        .unwrap()
        .job_id;
    store.start(lease, &job).unwrap();
    job
}

#[cfg(unix)]
#[test]
fn the_store_is_private_and_versioned() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("muxa/swap-jobs.sqlite3");
    let store = SwapJobStore::open(&path).unwrap();
    drop(store);
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(path.parent().unwrap()), 0o700);
    // Reopening a current store is fine.
    SwapJobStore::open(&path).unwrap();
}

/// A store from another schema is refused, and left exactly as it was.
#[test]
fn an_unknown_schema_is_refused_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("swap-jobs.sqlite3");
    {
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch("CREATE TABLE future (x);")
            .unwrap();
        connection.pragma_update(None, "user_version", 9).unwrap();
    }
    assert_eq!(
        SwapJobStore::open(&path).err(),
        Some(JobError::UnsupportedSchema { found: 9 })
    );
    let connection = Connection::open(&path).unwrap();
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 9);
    let tables: u32 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('jobs', 'owner')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tables, 0);
}

/// A second manager cannot take a store whose owner is alive; once that
/// owner is gone it can, and the first one is fenced out of every write.
#[test]
fn one_manager_owns_the_store_and_a_replaced_one_is_fenced() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    let first = store.acquire(&me("a", 1), &alive).unwrap();
    let other = open(&dir);
    assert_eq!(
        other.acquire(&me("b", 2), &alive).err(),
        Some(JobError::OwnedElsewhere { pid: 1 })
    );
    // Re-acquiring under the same nonce is the same manager.
    store.acquire(&me("a", 1), &alive).unwrap();

    let job = running(&store, &first, "k1", 1);
    let second = other.acquire(&me("b", 2), &gone).unwrap();
    assert_eq!(
        store.create_job(&first, "k2", "f", &plan(1, None)).err(),
        Some(JobError::Fenced)
    );
    let mut journal = store.journal(&first, &job, 0, 1);
    assert!(journal
        .before(&effect(Purpose::Stop, Op::Key(Key::Esc)))
        .is_err());
    // The new owner can write.
    other
        .create_job(&second, "k3", "f", &plan(1, None))
        .unwrap();
}

/// A request as a caller makes it, for digesting.
#[derive(serde::Serialize)]
struct Request<'a> {
    targets: &'a [&'a str],
    continue_text: Option<&'a str>,
}

fn digest(targets: &[&str], text: Option<&str>) -> String {
    request_digest(&Request {
        targets,
        continue_text: text,
    })
}

#[test]
fn a_repeated_request_is_the_same_job_and_a_changed_one_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    let lease = store.acquire(&me("a", 1), &alive).unwrap();
    let request = digest(&["%1", "%2"], Some("continue"));
    let accepted = plan(2, Some("continue"));
    assert_eq!(store.lookup("key", &request).unwrap(), None);
    let first = store
        .create_job(&lease, "key", &request, &accepted)
        .unwrap();
    assert!(!first.existing);

    // Sent again after the first one moved the agents: the live plan would
    // differ now (new pids, new accounts), the request does not.
    assert_eq!(
        store.lookup("key", &request).unwrap(),
        Some(first.job_id.clone())
    );
    let mut moved = plan(2, Some("continue"));
    moved.targets[0].process_pid = 4242;
    moved.targets[0].assigned_account = "c@claude".into();
    let again = store.create_job(&lease, "key", &request, &moved).unwrap();
    assert_eq!(
        again,
        Created {
            job_id: first.job_id.clone(),
            existing: true
        }
    );
    assert_eq!(
        store.plan(&first.job_id).unwrap(),
        accepted,
        "the first plan stands"
    );
    assert_eq!(store.status_by_key("key").unwrap().job_id, first.job_id);

    // Same length, different words: a different request.
    assert_ne!(digest(&["%1"], Some("abc")), digest(&["%1"], Some("xyz")));
    let reworded = digest(&["%1", "%2"], Some("go on, ok"));
    assert_eq!(
        store.lookup("key", &reworded).err(),
        Some(JobError::KeyReused("key".into()))
    );
    assert_eq!(
        store.create_job(&lease, "key", &reworded, &accepted).err(),
        Some(JobError::KeyReused("key".into()))
    );
    // The plan is stored without the text.
    assert!(!serde_json::to_string(&store.plan(&first.job_id).unwrap())
        .unwrap()
        .contains("continue\""));
}

/// The intent is in the file before the effect happens: another connection
/// sees it between `before` and `after`.
#[test]
fn an_effect_is_written_before_it_happens_and_settled_after() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    let lease = store.acquire(&me("a", 1), &alive).unwrap();
    let job = running(&store, &lease, "k", 1);
    let mut journal = store.journal(&lease, &job, 0, 1);
    let typed = effect(Purpose::FollowUp, Op::Prompt { chars: 8 });
    journal.before(&typed).unwrap();

    let reader = open(&dir);
    let seen = reader.status(&job).unwrap();
    assert_eq!(seen.targets[0].uncertain_effects, 1);

    journal.after(&typed, Transport::Accepted).unwrap();
    let seen = reader.status(&job).unwrap();
    assert_eq!(
        (seen.targets[0].uncertain_effects, seen.targets[0].effects),
        (0, 1)
    );
    // Only the length went into the journal.
    let raw: String = Connection::open(dir.path().join("muxa/swap-jobs.sqlite3"))
        .unwrap()
        .query_row("SELECT op_json FROM effects", [], |row| row.get(0))
        .unwrap();
    assert_eq!(raw, r#"{"prompt":{"chars":8}}"#);
}

/// A result with no intent, or a second result for one intent, is refused
/// rather than overwriting anything.
#[test]
fn a_stray_or_repeated_result_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    let lease = store.acquire(&me("a", 1), &alive).unwrap();
    let job = running(&store, &lease, "k", 1);
    let mut journal = store.journal(&lease, &job, 0, 1);
    let key = effect(Purpose::Interrupt, Op::Key(Key::Esc));
    assert!(journal.after(&key, Transport::Accepted).is_err());
    journal.before(&key).unwrap();
    // A second intent before the first has a result is refused too.
    assert!(journal.before(&key).is_err());
    journal.after(&key, Transport::Accepted).unwrap();
    assert!(journal.after(&key, Transport::Refused).is_err());
}

/// An intent that cannot be written refuses the effect; a result that
/// cannot be written leaves the intent, which reads as uncertain.
#[test]
fn a_store_that_stops_accepting_writes_never_loses_an_effect() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    let lease = store.acquire(&me("a", 1), &alive).unwrap();
    let job = running(&store, &lease, "k", 1);
    let mut journal = store.journal(&lease, &job, 0, 1);
    let stop = effect(
        Purpose::Stop,
        Op::Signal {
            signal: "TERM".into(),
            pids: vec![1000],
        },
    );
    journal.before(&stop).unwrap();
    // Another manager takes over between the effect and its result.
    let other = open(&dir);
    other.acquire(&me("b", 2), &gone).unwrap();
    assert!(journal.after(&stop, Transport::Unconfirmed).is_err());
    assert_eq!(store.status(&job).unwrap().targets[0].uncertain_effects, 1);
    // And nothing more can be started under the old lease.
    let mut journal = store.journal(&lease, &job, 0, 1);
    assert!(journal
        .before(&effect(Purpose::Relaunch, Op::Text { chars: 10 }))
        .is_err());
}

/// Recovery after the previous manager died: an effect without a result is
/// uncertain; one that completed is "interrupted after" it; a job that never
/// did anything is abandoned. Nothing is replayed, and the live owner's own
/// jobs are left alone.
#[test]
fn recovery_marks_every_unfinished_job_without_replaying_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    let dead = store.acquire(&me("dead", 1), &alive).unwrap();

    let uncertain = running(&store, &dead, "k1", 1);
    let mut journal = store.journal(&dead, &uncertain, 0, 1);
    journal
        .before(&effect(Purpose::Relaunch, Op::Text { chars: 10 }))
        .unwrap();

    let between = running(&store, &dead, "k2", 2);
    let mut journal = store.journal(&dead, &between, 0, 1);
    let stop = effect(
        Purpose::Stop,
        Op::Signal {
            signal: "TERM".into(),
            pids: vec![1000],
        },
    );
    journal.before(&stop).unwrap();
    journal.after(&stop, Transport::Unconfirmed).unwrap();
    store
        .finish_target(&dead, &between, 1, 1, Disposition::Skipped)
        .unwrap();

    let never = store
        .create_job(&dead, "k3", "f3", &plan(1, None))
        .unwrap()
        .job_id;

    let next = open(&dir);
    let lease = next.acquire(&me("next", 2), &gone).unwrap();
    let live = running(&next, &lease, "k4", 1);
    let mut recovered = next.recover(&lease).unwrap();
    recovered.sort();
    let mut expected = vec![uncertain.clone(), between.clone(), never.clone()];
    expected.sort();
    assert_eq!(recovered, expected);

    let status = next.status(&uncertain).unwrap();
    assert_eq!(status.state, "needs_reconciliation");
    assert_eq!(
        status.targets[0].disposition.as_deref(),
        Some("needs_reconciliation")
    );
    assert_eq!(
        status.targets[0].reason.as_deref(),
        Some("uncertain:relaunch")
    );
    assert_eq!(status.targets[0].uncertain_effects, 1);

    let status = next.status(&between).unwrap();
    assert_eq!(status.state, "needs_reconciliation");
    assert_eq!(status.targets[0].reason.as_deref(), Some("after:stop"));
    // A target that had already finished keeps its outcome.
    assert_eq!(status.targets[1].disposition.as_deref(), Some("skipped"));

    let status = next.status(&never).unwrap();
    assert_eq!(status.state, "abandoned");
    assert_eq!(status.targets[0].reason.as_deref(), Some("not_started"));

    assert_eq!(next.status(&live).unwrap().state, "running");
    // Recovering again changes nothing.
    assert!(next.recover(&lease).unwrap().is_empty());
}

#[test]
fn a_job_is_done_only_once_every_target_is_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    let lease = store.acquire(&me("a", 1), &alive).unwrap();
    let job = running(&store, &lease, "k", 2);
    store
        .observe(
            &lease,
            &job,
            0,
            1,
            &Observation::Phase {
                phase: Phase::Stopping,
            },
        )
        .unwrap();
    store
        .observe(
            &lease,
            &job,
            0,
            1,
            &Observation::AccountCheck {
                result: AccountCheck::Unreadable,
                source: "process_environ".into(),
            },
        )
        .unwrap();
    assert_eq!(store.status(&job).unwrap().targets[0].phase, "stopping");
    store
        .finish_target(&lease, &job, 0, 1, Disposition::Swapped)
        .unwrap();
    assert!(store.finish(&lease, &job).is_err());
    // An outcome is recorded once.
    assert!(store
        .finish_target(&lease, &job, 0, 1, Disposition::Failed)
        .is_err());
    store
        .finish_target(&lease, &job, 1, 1, Disposition::Kept)
        .unwrap();
    store.finish(&lease, &job).unwrap();
    let status = store.status(&job).unwrap();
    assert_eq!(status.state, "done");
    assert_eq!(status.targets[0].phase, "finished");
}

#[test]
fn the_store_lives_in_the_data_dir() {
    if let Some(path) = SwapJobStore::default_path() {
        assert!(
            path.ends_with("muxa/swap-jobs.sqlite3"),
            "{}",
            path.display()
        );
    }
}

/// Not a gate: prints what one intent + result pair costs with
/// `synchronous=FULL` on this machine's temporary directory.
#[test]
#[ignore = "measurement, run by hand"]
fn measure_journal_cost() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    let lease = store.acquire(&me("a", 1), &alive).unwrap();
    let job = running(&store, &lease, "k", 1);
    let mut journal = store.journal(&lease, &job, 0, 1);
    let key = effect(Purpose::ExitDialog, Op::Key(Key::Enter));
    let rounds = 48;
    let started = std::time::Instant::now();
    let mut worst = std::time::Duration::ZERO;
    for _ in 0..rounds {
        let one = std::time::Instant::now();
        journal.before(&key).unwrap();
        journal.after(&key, Transport::Accepted).unwrap();
        worst = worst.max(one.elapsed());
    }
    let total = started.elapsed();
    println!(
        "{rounds} effects: total {total:?}, mean {:?}, worst {worst:?}",
        total / rounds
    );
}

/// An intent committed after the database was renamed away would be one a
/// restarted manager never sees: refused, and latched even once the path
/// comes back.
#[test]
fn a_moved_or_replaced_database_takes_no_further_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("muxa/swap-jobs.sqlite3");
    let store = SwapJobStore::open(&path).unwrap();
    let lease = store.acquire(&me("a", 1), &alive).unwrap();
    let job = running(&store, &lease, "k", 1);
    let aside = dir.path().join("aside.sqlite3");
    std::fs::rename(&path, &aside).unwrap();
    let mut journal = store.journal(&lease, &job, 0, 1);
    let esc = effect(Purpose::Interrupt, Op::Key(Key::Esc));
    assert!(journal.before(&esc).is_err());
    std::fs::rename(&aside, &path).unwrap();
    assert!(
        journal.before(&esc).is_err(),
        "latched after the path came back"
    );
    assert!(store.status(&job).is_err());

    // A file put in its place is not the store either.
    let other_dir = tempfile::tempdir().unwrap();
    let other_path = other_dir.path().join("swap-jobs.sqlite3");
    let other = SwapJobStore::open(&other_path).unwrap();
    let lease = other.acquire(&me("b", 2), &alive).unwrap();
    std::fs::remove_file(&other_path).unwrap();
    std::fs::write(&other_path, b"").unwrap();
    assert!(other.create_job(&lease, "k", "f", &plan(1, None)).is_err());
}

/// A target that has ended takes no new effect or observation, even while
/// another target of the job is still running.
#[test]
fn an_ended_target_takes_nothing_new() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    let lease = store.acquire(&me("a", 1), &alive).unwrap();
    let job = running(&store, &lease, "k", 2);
    store
        .finish_target(&lease, &job, 0, 1, Disposition::Swapped)
        .unwrap();
    let mut journal = store.journal(&lease, &job, 0, 1);
    assert!(journal
        .before(&effect(Purpose::FollowUp, Op::Prompt { chars: 3 }))
        .is_err());
    assert!(store
        .observe(
            &lease,
            &job,
            0,
            1,
            &Observation::Phase {
                phase: Phase::Stopping
            }
        )
        .is_err());
    assert_eq!(store.status(&job).unwrap().targets[0].phase, "finished");
    // The other target is still open.
    let mut journal = store.journal(&lease, &job, 1, 1);
    journal
        .before(&effect(Purpose::Interrupt, Op::Key(Key::Esc)))
        .unwrap();
}

/// The open intent lives in the file: a second journal for the same attempt,
/// or one made after reopening, cannot start another effect.
#[test]
fn an_open_intent_blocks_every_journal_for_that_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(&dir);
    let lease = store.acquire(&me("a", 1), &alive).unwrap();
    let job = running(&store, &lease, "k", 1);
    let stop = effect(
        Purpose::Stop,
        Op::Signal {
            signal: "TERM".into(),
            pids: vec![1000],
        },
    );
    let mut first = store.journal(&lease, &job, 0, 1);
    first.before(&stop).unwrap();
    let mut second = store.journal(&lease, &job, 0, 1);
    assert!(second
        .before(&effect(Purpose::Relaunch, Op::Text { chars: 8 }))
        .is_err());

    // A result for another effect does not settle this one, and keeps it
    // pending.
    let other = effect(Purpose::FollowUp, Op::Prompt { chars: 8 });
    assert!(first.after(&other, Transport::Accepted).is_err());
    assert_eq!(store.status(&job).unwrap().targets[0].uncertain_effects, 1);
    first.after(&stop, Transport::Unconfirmed).unwrap();
    assert_eq!(store.status(&job).unwrap().targets[0].uncertain_effects, 0);

    // Reopened with an intent left open: nothing new on that attempt.
    first.before(&other).unwrap();
    drop(first);
    drop(second);
    drop(store);
    let reopened = open(&dir);
    let lease = reopened.acquire(&me("a", 1), &alive).unwrap();
    let mut journal = reopened.journal(&lease, &job, 0, 1);
    assert!(journal
        .before(&effect(Purpose::RestoreDraft, Op::Prompt { chars: 2 }))
        .is_err());
    // It can only end as needs_reconciliation.
    assert!(reopened
        .finish_target(&lease, &job, 0, 1, Disposition::Swapped)
        .is_err());
    reopened
        .finish_target(&lease, &job, 0, 1, Disposition::NeedsReconciliation)
        .unwrap();
}
