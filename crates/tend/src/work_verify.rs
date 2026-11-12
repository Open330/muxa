//! Acceptance criteria and declared checks on a Work.
//!
//! A person (`tend work plan`/`work up`) or the requesting agent (the MCP
//! start tool) declares what done means and which commands prove it. tend
//! runs those commands itself when the Work reports a result, so the person
//! is not the first verifier. Commands come only from this declaration,
//! never from repository content.

use serde::{Deserialize, Serialize};

/// Most criteria one Work carries.
pub const MAX_ACCEPTANCE: usize = 16;
/// Most declared checks one Work carries.
pub const MAX_CHECKS: usize = 8;
/// Default and largest per-check time limit.
pub const DEFAULT_CHECK_TIMEOUT_SECS: u32 = 300;
pub const MAX_CHECK_TIMEOUT_SECS: u32 = 1_200;
/// Wall-clock budget for one verification, every check together. A check
/// that would start past it is not run and counts as failed.
pub const MAX_VERIFY_SECS: u64 = 1_200;
/// How long a client waits for a request that may run checks: the budget
/// plus a margin for the daemon's own work.
pub const VERIFY_CALL_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(MAX_VERIFY_SECS + 60);
/// Output kept per stream while a check runs; the rest is drained unread.
const OUTPUT_BUDGET: usize = 256 * 1024;
/// Lines of output kept as evidence.
const TAIL_LINES: usize = 40;

/// Who declared a Work's acceptance criteria and checks, as tendd saw the
/// caller: the person (any caller not running in a tracked agent's pane),
/// or the agent session whose pane the caller runs in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Declarer {
    Operator,
    Agent { session_id: String },
}

impl Declarer {
    /// Whether `self` may change a declaration `current` made. The person
    /// may always; an agent only its own, or one nobody has made yet. This
    /// keeps the agent whose work is checked from rewriting its own checks;
    /// it is not authentication, since every caller runs as the same user.
    #[must_use]
    pub fn may_replace(&self, current: Option<&Self>) -> bool {
        match (self, current) {
            (Self::Operator, _) | (_, None) => true,
            (Self::Agent { session_id }, Some(Self::Agent { session_id: owner })) => {
                session_id == owner
            }
            (Self::Agent { .. }, Some(Self::Operator)) => false,
        }
    }
}

/// One command tend runs to verify a result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkCheck {
    /// Shell command line, run in the Work's directory.
    pub command: String,
    /// Short label shown with the result; defaults to the command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Seconds before the check is stopped and counted as failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u32>,
}

impl WorkCheck {
    #[must_use]
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            name: None,
            timeout_secs: None,
        }
    }

    /// The label to show: the name, else the command.
    #[must_use]
    pub fn label(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.command)
    }

    #[must_use]
    pub fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(u64::from(
            self.timeout_secs.unwrap_or(DEFAULT_CHECK_TIMEOUT_SECS),
        ))
    }
}

fn clean_line(value: &str, max: usize, label: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("{label} cannot be empty"));
    }
    if value.len() > max {
        return Err(format!("{label} is too long (max {max} bytes)"));
    }
    if value.chars().any(|ch| ch.is_control() && ch != '\t') {
        return Err(format!(
            "{label} cannot contain control characters or newlines"
        ));
    }
    Ok(value.to_string())
}

/// Trimmed, bounded criteria; duplicates dropped, order kept.
pub fn validate_acceptance(criteria: Vec<String>) -> Result<Vec<String>, String> {
    if criteria.len() > MAX_ACCEPTANCE {
        return Err(format!("at most {MAX_ACCEPTANCE} acceptance criteria"));
    }
    let mut out: Vec<String> = Vec::with_capacity(criteria.len());
    for criterion in criteria {
        let criterion = clean_line(&criterion, 1_024, "acceptance criterion")?;
        if !out.contains(&criterion) {
            out.push(criterion);
        }
    }
    Ok(out)
}

/// Bounded checks with a sane timeout each.
pub fn validate_checks(checks: Vec<WorkCheck>) -> Result<Vec<WorkCheck>, String> {
    if checks.len() > MAX_CHECKS {
        return Err(format!("at most {MAX_CHECKS} checks"));
    }
    checks
        .into_iter()
        .map(|check| {
            let command = clean_line(&check.command, 2_048, "check command")?;
            let name = check
                .name
                .as_deref()
                .map(|name| clean_line(name, 120, "check name"))
                .transpose()?;
            if let Some(timeout) = check.timeout_secs {
                if timeout == 0 || timeout > MAX_CHECK_TIMEOUT_SECS {
                    return Err(format!(
                        "check timeout must be 1-{MAX_CHECK_TIMEOUT_SECS} seconds"
                    ));
                }
            }
            Ok(WorkCheck {
                command,
                name,
                timeout_secs: check.timeout_secs,
            })
        })
        .collect()
}

/// How one declared check ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckOutcome {
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub passed: bool,
    /// The exit status, when the process exited by itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Stopped at its time limit (or not started: the budget ran out).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub timed_out: bool,
    /// The last lines of its combined output, cleaned for display.
    pub tail: String,
}

impl CheckOutcome {
    /// The result as decision evidence, marked as run by tend.
    #[must_use]
    pub fn evidence(&self) -> crate::decision::Evidence {
        crate::decision::Evidence::Test {
            command: self.command.clone(),
            passed: Some(self.passed),
            tail: (!self.tail.is_empty()).then(|| self.tail.clone()),
            verified: true,
        }
    }

    /// One line plus the tail, for an agent told its result was refused.
    #[must_use]
    pub fn report(&self) -> String {
        let how = if self.timed_out {
            "timed out".to_string()
        } else {
            match self.exit_code {
                Some(code) => format!("exit {code}"),
                None => "killed".to_string(),
            }
        };
        let label = self.name.as_deref().unwrap_or(&self.command);
        let mut out = format!(
            "{} {label} ({how})",
            if self.passed { "PASS" } else { "FAIL" }
        );
        for line in self.tail.lines() {
            out.push_str("\n    ");
            out.push_str(line);
        }
        out
    }
}

/// Run `checks` one after another in `dir`, within [`MAX_VERIFY_SECS`].
/// Blocking: call it from a blocking thread.
#[must_use]
pub fn run_checks(checks: &[WorkCheck], dir: &std::path::Path) -> Vec<CheckOutcome> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(MAX_VERIFY_SECS);
    checks
        .iter()
        .map(|check| {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return CheckOutcome {
                    command: check.command.clone(),
                    name: check.name.clone(),
                    passed: false,
                    exit_code: None,
                    timed_out: true,
                    tail: "not run: the verification time budget was used up".into(),
                };
            }
            run_check(check, dir, check.timeout().min(left))
        })
        .collect()
}

/// Keep the newest [`OUTPUT_BUDGET`] bytes of `stream` on its own thread.
fn drain(stream: Option<Box<dyn std::io::Read + Send>>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        if let Some(mut stream) = stream {
            let mut chunk = [0_u8; 8192];
            while let Ok(n) = std::io::Read::read(&mut stream, &mut chunk) {
                if n == 0 {
                    break;
                }
                kept.extend_from_slice(&chunk[..n]);
                if kept.len() > OUTPUT_BUDGET {
                    let excess = kept.len() - OUTPUT_BUDGET;
                    kept.drain(..excess);
                }
            }
        }
        kept
    })
}

/// stdout, then stderr on a fresh line.
fn combined(mut output: Vec<u8>, err: &[u8]) -> Vec<u8> {
    if !err.is_empty() {
        if !output.is_empty() && !output.ends_with(b"\n") {
            output.push(b'\n');
        }
        output.extend_from_slice(err);
    }
    output
}

/// The last [`TAIL_LINES`] lines, cleaned, plus a note when the check was
/// stopped at `stopped_after`.
fn tail_of(output: &[u8], stopped_after: Option<std::time::Duration>) -> String {
    let text = String::from_utf8_lossy(output);
    let lines: Vec<&str> = text.lines().collect();
    let mut tail = lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n");
    if let Some(limit) = stopped_after {
        if !tail.is_empty() {
            tail.push('\n');
        }
        tail.push_str("stopped after ");
        tail.push_str(&limit.as_secs().to_string());
        tail.push('s');
    }
    crate::decision::clean_tail(&tail)
}

fn run_check(check: &WorkCheck, dir: &std::path::Path, limit: std::time::Duration) -> CheckOutcome {
    let outcome = |passed, exit_code, timed_out, tail: String| CheckOutcome {
        command: check.command.clone(),
        name: check.name.clone(),
        passed,
        exit_code,
        timed_out,
        tail,
    };
    if !dir.is_dir() {
        return outcome(
            false,
            None,
            false,
            format!("not run: {} is not a directory", dir.display()),
        );
    }
    let mut command = crate::shell::script_command(&check.command);
    command
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // A check is not part of any agent session tend tracks.
    for name in ["TEND_SESSION_ID", "TEND_LAUNCH_ID", "TMUX_PANE"] {
        command.env_remove(name);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own process group, so a timeout stops what it started too.
        command.process_group(0);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return outcome(false, None, false, format!("could not start: {error}")),
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
    );
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() >= limit => break None,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(_) => break None,
        }
    };
    let timed_out = status.is_none();
    if timed_out {
        // The whole process group the check started (see `process_group`).
        #[cfg(unix)]
        let _ = std::process::Command::new("kill")
            .args(["-KILL", "--", &format!("-{}", child.id())])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let _ = child.kill();
        let _ = child.wait();
    }
    let output = combined(
        stdout.join().unwrap_or_default(),
        &stderr.join().unwrap_or_default(),
    );
    let tail = tail_of(&output, timed_out.then_some(limit));
    let exit_code = status.and_then(|status| status.code());
    let passed = status.is_some_and(|status| status.success());
    outcome(passed, exit_code, timed_out, tail)
}

/// A verification's verdict: the Work declared no checks, or every check
/// passed, or at least one failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    NoChecks,
    Passed(Vec<CheckOutcome>),
    Failed(Vec<CheckOutcome>),
}

/// Run `identity`'s declared checks off the async runtime, in its durable
/// Run's directory when it has one (where `work up` put it: a route
/// worktree, `--cwd`), else the directory the checks were declared for. A
/// Work with no checks (or no record) is [`Gate::NoChecks`].
pub async fn verify(
    store: &crate::dashboard::work_store::SharedWorkStore,
    runs: &crate::pipeline_run::PipelineRunStore,
    identity: &crate::work::WorkIdentity,
) -> Gate {
    let plan = store
        .lock()
        .await
        .get(identity)
        .and_then(|record| record.plan);
    let Some(plan) = plan.filter(|plan| !plan.checks.is_empty()) else {
        return Gate::NoChecks;
    };
    let run_dir = runs
        .get(identity)
        .await
        .map(|run| run.cwd.to_string_lossy().into_owned());
    let Some(dir) = run_dir.or_else(|| plan.checks_dir().map(str::to_owned)) else {
        // Refused at edit time; kept as a failure rather than a pass.
        return Gate::Failed(vec![CheckOutcome {
            command: plan.checks[0].command.clone(),
            name: None,
            passed: false,
            exit_code: None,
            timed_out: false,
            tail: "not run: the Work has no directory".into(),
        }]);
    };
    let checks = plan.checks;
    let outcomes =
        tokio::task::spawn_blocking(move || run_checks(&checks, std::path::Path::new(&dir)))
            .await
            .unwrap_or_default();
    if !outcomes.is_empty() && outcomes.iter().all(|outcome| outcome.passed) {
        remember_pass(identity);
        Gate::Passed(outcomes)
    } else {
        Gate::Failed(outcomes)
    }
}

/// How long a passing verification of a Work covers a following
/// completion, so `tend work done --summary` runs the checks once.
const PASS_REUSE: std::time::Duration = std::time::Duration::from_secs(600);

fn passes() -> &'static std::sync::Mutex<
    std::collections::HashMap<crate::work::WorkIdentity, std::time::Instant>,
> {
    static PASSES: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<crate::work::WorkIdentity, std::time::Instant>>,
    > = std::sync::OnceLock::new();
    PASSES.get_or_init(Default::default)
}

fn remember_pass(identity: &crate::work::WorkIdentity) {
    let mut passes = passes()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    passes.retain(|_, at| at.elapsed() < PASS_REUSE);
    passes.insert(identity.clone(), std::time::Instant::now());
}

/// [`verify`], unless the same Work passed within [`PASS_REUSE`].
pub async fn verify_unless_recent(
    store: &crate::dashboard::work_store::SharedWorkStore,
    runs: &crate::pipeline_run::PipelineRunStore,
    identity: &crate::work::WorkIdentity,
) -> Gate {
    let recent = passes()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(identity)
        .is_some_and(|at| at.elapsed() < PASS_REUSE);
    if recent {
        return Gate::Passed(Vec::new());
    }
    verify(store, runs, identity).await
}

/// Put tend's own check results at the head of a result record's
/// evidence, and clear any client's claim to have been run by tend. The
/// agent's own evidence follows, trimmed to fit.
pub fn attach(decision: &mut Option<crate::decision::Decision>, outcomes: &[CheckOutcome]) {
    let decision = decision.get_or_insert_with(Default::default);
    let mut evidence: Vec<crate::decision::Evidence> =
        outcomes.iter().map(CheckOutcome::evidence).collect();
    for mut theirs in std::mem::take(&mut decision.evidence) {
        if let crate::decision::Evidence::Test { verified, .. } = &mut theirs {
            *verified = false;
        }
        evidence.push(theirs);
    }
    evidence.truncate(crate::decision::MAX_EVIDENCE);
    decision.evidence = evidence;
}

/// Clear any client's claim that tend ran its evidence.
pub fn strip_claims(decision: &mut Option<crate::decision::Decision>) {
    if let Some(decision) = decision {
        for evidence in &mut decision.evidence {
            if let crate::decision::Evidence::Test { verified, .. } = evidence {
                *verified = false;
            }
        }
    }
}

/// The message an agent gets when its result is held back by a failing
/// check.
#[must_use]
pub fn refusal(outcomes: &[CheckOutcome]) -> String {
    let mut out = String::from(
        "checks_failed: the Work's declared checks did not pass, so the result was not filed and the Work stays in progress. Fix and report again.",
    );
    for outcome in outcomes {
        out.push('\n');
        out.push_str(&outcome.report());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acceptance_is_trimmed_deduplicated_and_bounded() {
        assert_eq!(
            validate_acceptance(vec![
                " tests pass ".into(),
                "tests pass".into(),
                "docs".into()
            ])
            .unwrap(),
            ["tests pass", "docs"]
        );
        assert!(validate_acceptance(vec![String::new()]).is_err());
        assert!(validate_acceptance(vec!["a\nb".into()]).is_err());
        assert!(validate_acceptance((0..17).map(|i| i.to_string()).collect()).is_err());
    }

    #[test]
    fn checks_are_bounded_and_need_a_sane_timeout() {
        assert!(validate_checks(vec![WorkCheck::new("cargo test")]).is_ok());
        assert!(validate_checks(vec![WorkCheck::new("  ")]).is_err());
        let slow = WorkCheck {
            timeout_secs: Some(0),
            ..WorkCheck::new("x")
        };
        assert!(validate_checks(vec![slow]).is_err());
        assert!(validate_checks((0..9).map(|_| WorkCheck::new("x")).collect()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn checks_run_in_the_work_directory_and_report_how_they_ended() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker"), b"here").unwrap();
        let outcomes = run_checks(
            &[
                WorkCheck::new("cat marker"),
                WorkCheck::new("echo boom >&2; exit 3"),
            ],
            dir.path(),
        );
        assert!(outcomes[0].passed);
        assert_eq!(outcomes[0].tail, "here");
        assert!(!outcomes[1].passed);
        assert_eq!(outcomes[1].exit_code, Some(3));
        assert_eq!(outcomes[1].tail, "boom");
        assert!(outcomes[1]
            .report()
            .starts_with("FAIL echo boom >&2; exit 3 (exit 3)"));
        let evidence = outcomes[1].evidence();
        assert!(matches!(
            evidence,
            crate::decision::Evidence::Test {
                verified: true,
                passed: Some(false),
                ..
            }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_check_past_its_time_limit_is_stopped_with_its_children() {
        let dir = tempfile::tempdir().unwrap();
        let slow = WorkCheck {
            timeout_secs: Some(1),
            ..WorkCheck::new("echo started; sleep 30 & wait")
        };
        let started = std::time::Instant::now();
        let outcome = &run_checks(&[slow], dir.path())[0];
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        assert!(outcome.timed_out && !outcome.passed);
        assert!(outcome.tail.contains("started"), "{}", outcome.tail);
        assert!(outcome.tail.contains("stopped after 1s"));
    }

    #[test]
    fn output_is_bounded_to_its_newest_lines() {
        let dir = tempfile::tempdir().unwrap();
        let check = if cfg!(windows) {
            WorkCheck::new("1..100 | ForEach-Object { \"line $_\" }")
        } else {
            WorkCheck::new("i=1; while [ $i -le 100 ]; do echo line $i; i=$((i+1)); done")
        };
        let outcome = &run_checks(&[check], dir.path())[0];
        assert!(outcome.passed, "{}", outcome.tail);
        assert_eq!(outcome.tail.lines().count(), TAIL_LINES);
        assert!(outcome.tail.ends_with("line 100"));
    }

    #[test]
    fn tends_results_lead_and_a_clients_claim_is_cleared() {
        let ran = CheckOutcome {
            command: "cargo test".into(),
            name: None,
            passed: true,
            exit_code: Some(0),
            timed_out: false,
            tail: "ok".into(),
        };
        let forged = crate::decision::Evidence::Test {
            command: "cargo test".into(),
            passed: Some(true),
            tail: None,
            verified: true,
        };
        let mut decision = Some(crate::decision::Decision {
            evidence: vec![forged],
            ..Default::default()
        });
        attach(&mut decision, &[ran]);
        let evidence = decision.unwrap().evidence;
        assert!(matches!(
            evidence[0],
            crate::decision::Evidence::Test { verified: true, .. }
        ));
        assert!(matches!(
            evidence[1],
            crate::decision::Evidence::Test {
                verified: false,
                ..
            }
        ));

        let mut only_theirs = Some(crate::decision::Decision {
            evidence: vec![crate::decision::Evidence::Test {
                command: "x".into(),
                passed: None,
                tail: None,
                verified: true,
            }],
            ..Default::default()
        });
        strip_claims(&mut only_theirs);
        assert!(matches!(
            only_theirs.unwrap().evidence[0],
            crate::decision::Evidence::Test {
                verified: false,
                ..
            }
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_work_is_verified_with_its_own_checks_in_its_own_directory() {
        use crate::work::{WorkEdit, WorkIdentity};
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::dashboard::work_store::WorkStore::memory(),
        ));
        let runs = crate::pipeline_run::PipelineRunStore::in_memory();
        let identity = WorkIdentity::new("verify-test", "ENG-1");
        assert_eq!(verify(&store, &runs, &identity).await, Gate::NoChecks);
        store
            .lock()
            .await
            .edit_as(
                identity.clone(),
                WorkEdit {
                    title: Some("x".into()),
                    cwd: Some(dir.path().to_string_lossy().into_owned()),
                    checks: Some(vec![WorkCheck::new("test -f ready")]),
                    ..WorkEdit::default()
                },
                Some(&Declarer::Operator),
            )
            .await
            .unwrap();
        assert!(matches!(
            verify(&store, &runs, &identity).await,
            Gate::Failed(_)
        ));
        std::fs::write(dir.path().join("ready"), b"").unwrap();
        assert!(matches!(
            verify(&store, &runs, &identity).await,
            Gate::Passed(_)
        ));
        // A completion right after reuses the pass instead of rerunning.
        std::fs::remove_file(dir.path().join("ready")).unwrap();
        assert!(matches!(
            verify_unless_recent(&store, &runs, &identity).await,
            Gate::Passed(outcomes) if outcomes.is_empty()
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn checks_stay_where_they_were_declared_when_the_plan_moves() {
        use crate::work::{WorkEdit, WorkIdentity};
        let planned = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        std::fs::write(worktree.path().join("result"), b"").unwrap();
        let store = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::dashboard::work_store::WorkStore::memory(),
        ));
        let runs = crate::pipeline_run::PipelineRunStore::in_memory();
        let identity = WorkIdentity::new("verify-test", "ENG-9");
        store
            .lock()
            .await
            .edit_as(
                identity.clone(),
                WorkEdit {
                    title: Some("x".into()),
                    cwd: Some(worktree.path().to_string_lossy().into_owned()),
                    checks: Some(vec![WorkCheck::new("test -f result")]),
                    ..WorkEdit::default()
                },
                Some(&Declarer::Operator),
            )
            .await
            .unwrap();
        // Someone moves the plan elsewhere; the checks stay put.
        store
            .lock()
            .await
            .edit(
                identity.clone(),
                WorkEdit {
                    cwd: Some(planned.path().to_string_lossy().into_owned()),
                    ..WorkEdit::default()
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            verify(&store, &runs, &identity).await,
            Gate::Passed(_)
        ));
    }
}
