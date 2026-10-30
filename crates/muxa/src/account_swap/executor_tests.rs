//! The executor against a scripted pane: virtual time, a screen, processes
//! that die or appear when the right keys arrive. Each test pins the exact
//! sequence of operations a branch performs, failure and keep branches
//! included, and that every one of them went through the [`Runner`].

use super::effects::{
    Effect, EffectObserver, Key, NoObserver, ObserverError, Op, Purpose, Runner, Transport,
};
use super::io::{AgentArgv, Clock, SwapIo};
use super::{execute, AccountVerdict, LaunchLine, PaneLaunch, Refusal, Target};
use crate::event::AgentKind;
use crate::swap_job::{
    AccountCheck, ContinueDecision, Observation, Phase, Readiness, WithheldReason,
};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const AGENT: u32 = 10;
const WRAPPER: u32 = 11;

type React = Box<dyn Fn(&FakeIo, &str)>;

/// A pane and its processes on a virtual clock. Every operation is logged
/// as one line and handed to `react`, which plays the agent's part.
struct FakeIo {
    base: Instant,
    now: Cell<Duration>,
    screen: RefCell<String>,
    /// When each pid exits; absent means it keeps running.
    dies_at: RefCell<HashMap<u32, Duration>>,
    /// When the relaunched agent shows up in the pane.
    agent_from: Cell<Option<Duration>>,
    pane_pid: Cell<Option<u32>>,
    remain_ok: Cell<bool>,
    respawn_ok: Cell<bool>,
    enter_ok: Cell<bool>,
    sent: RefCell<Vec<String>>,
    react: React,
    /// The relaunched agent processes' arguments; by default one process
    /// that resumes the target's conversation the provider's way.
    argvs: RefCell<Option<Vec<AgentArgv>>>,
    kind: Cell<AgentKind>,
}

impl FakeIo {
    fn new(screen: &str, react: React) -> Self {
        Self {
            base: Instant::now(),
            now: Cell::new(Duration::ZERO),
            screen: RefCell::new(screen.to_owned()),
            dies_at: RefCell::new(HashMap::new()),
            agent_from: Cell::new(None),
            pane_pid: Cell::new(Some(200)),
            remain_ok: Cell::new(true),
            respawn_ok: Cell::new(true),
            enter_ok: Cell::new(true),
            sent: RefCell::new(Vec::new()),
            react,
            argvs: RefCell::new(None),
            kind: Cell::new(AgentKind::Codex),
        }
    }

    fn show(&self, screen: &str) {
        *self.screen.borrow_mut() = screen.to_owned();
    }

    fn kill(&self, pid: u32) {
        self.dies_at.borrow_mut().insert(pid, self.now.get());
    }

    fn agent_appears_in(&self, after: Duration) {
        self.agent_from.set(Some(self.now.get() + after));
    }

    fn previous(&self) -> Option<String> {
        let sent = self.sent.borrow();
        sent.len().checked_sub(2).map(|i| sent[i].clone())
    }

    fn record(&self, line: String) {
        self.sent.borrow_mut().push(line.clone());
        (self.react)(self, &line);
    }

    fn sent(&self) -> Vec<String> {
        self.sent.borrow().clone()
    }
}

impl Clock for FakeIo {
    fn now(&self) -> Instant {
        self.base + self.now.get()
    }

    fn sleep(&self, duration: Duration) {
        self.now.set(self.now.get() + duration);
    }
}

impl SwapIo for FakeIo {
    fn capture(&self) -> Option<String> {
        Some(self.screen.borrow().clone())
    }

    fn send_text(&self, text: &str) -> bool {
        self.record(format!("text {text:?}"));
        text != "\r" || self.enter_ok.get()
    }

    fn send_prompt_text(&self, text: &str) -> bool {
        self.record(format!("prompt {text:?}"));
        true
    }

    fn set_remain_on_exit(&self, on: bool) -> bool {
        self.record(format!("remain {on}"));
        self.remain_ok.get()
    }

    fn respawn(&self, _cwd: &str, _line: &str) -> bool {
        self.record("respawn".into());
        self.respawn_ok.get()
    }

    fn pane_pid_now(&self) -> Option<u32> {
        self.pane_pid.get()
    }

    fn agent_running(&self, _pane_pid: u32) -> bool {
        self.agent_from
            .get()
            .is_some_and(|from| self.now.get() >= from)
    }

    fn agent_argvs(&self, pane_pid: u32) -> Vec<AgentArgv> {
        if let Some(argvs) = self.argvs.borrow().clone() {
            return argvs;
        }
        if !self.agent_running(pane_pid) {
            return Vec::new();
        }
        let argv = match self.kind.get() {
            AgentKind::Codex => ["codex", "resume", "S"],
            _ => ["claude", "--resume", "S"],
        };
        vec![(20, Some(argv.iter().map(|w| (*w).to_owned()).collect()))]
    }

    fn alive(&self, pid: u32) -> bool {
        self.dies_at
            .borrow()
            .get(&pid)
            .is_none_or(|at| self.now.get() < *at)
    }

    fn signal(&self, signal: &str, pids: &[u32]) {
        self.record(format!("signal {signal} {pids:?}"));
    }

    fn writer_owners(&self, _pane_pid: u32) -> String {
        String::new()
    }

    fn submit_grace(&self) -> Duration {
        Duration::from_millis(100)
    }
}

fn target(kind: AgentKind, direct: bool, busy: bool) -> Target {
    Target {
        socket: None,
        pane: "%1".into(),
        pane_pid: Cell::new(100),
        direct,
        cwd: "/work".into(),
        kind,
        session_id: "S".into(),
        busy,
        conversation_verified: true,
        evidence: super::ContinueEvidence::Binding,
    }
}

fn launch() -> PaneLaunch {
    PaneLaunch {
        agent_pid: AGENT,
        top_pid: AGENT,
        line: LaunchLine::bare("codex"),
        resumed_id: None,
    }
}

fn claude_launch() -> PaneLaunch {
    PaneLaunch {
        agent_pid: AGENT,
        top_pid: WRAPPER,
        line: LaunchLine::bare("claude"),
        resumed_id: None,
    }
}

const CODEX_IDLE: &str = "› Ask Codex to do anything\n";
const CODEX_WORKING: &str = "• Working (3s • esc to interrupt)\n› Ask Codex to do anything\n";
const CLAUDE_WITH_DRAFT: &str = "──────────\n❯ half typed\n──────────\n  ⏵⏵ auto mode on\n";
const SCHEDULED: &str = "Exiting will stop 1 scheduled task\n❯ Exit and stop tasks\n  Cancel\n";

/// A Codex agent that stops on TERM, comes back on the relaunch and starts a
/// turn on the follow-up.
fn codex_agent() -> React {
    Box::new(|io: &FakeIo, line: &str| {
        if line.starts_with("signal TERM") {
            io.kill(AGENT);
        }
        if line == "text \"\\r\"" {
            match io.previous().as_deref() {
                Some("text \"RELAUNCH\"") => {
                    io.agent_appears_in(Duration::from_secs(1));
                    io.show(CODEX_IDLE);
                }
                Some("prompt \"continue\"") => io.show(CODEX_WORKING),
                _ => {}
            }
        }
    })
}

/// What a run returned, every effect it made, and why it halted.
type Ran = (
    Result<String, Refusal>,
    Vec<(Effect, Transport)>,
    Option<String>,
);

/// Run the executor with an account check that always passes.
fn run(
    io: &FakeIo,
    target: &Target,
    launch: &PaneLaunch,
    follow_up: Option<&str>,
    observer: &mut dyn EffectObserver,
) -> Ran {
    io.kind.set(target.kind);
    let runner = Runner::new(io, observer);
    let verified = |_: &Target, _: Option<u32>| {
        Ok(AccountVerdict {
            check: AccountCheck::Matched,
            observed: Some("b".into()),
            source: "test",
            note: String::new(),
        })
    };
    let result = execute(&runner, target, launch, "RELAUNCH", follow_up, &verified);
    (result, runner.log(), runner.halted())
}

#[test]
fn a_codex_swap_stops_relaunches_and_sends_the_follow_up_in_order() {
    let io = FakeIo::new(CODEX_IDLE, codex_agent());
    let (result, log, halted) = run(
        &io,
        &target(AgentKind::Codex, false, false),
        &launch(),
        Some("continue"),
        &mut NoObserver,
    );
    assert_eq!(
        result.unwrap(),
        "sent \"continue\", and the agent picked the turn back up"
    );
    assert_eq!(
        io.sent(),
        [
            "signal TERM [10]",
            "text \"RELAUNCH\"",
            "text \"\\r\"",
            "prompt \"continue\"",
            "text \"\\r\"",
        ]
    );
    assert!(halted.is_none());
    // The prompt and its Enter are two effects of one act.
    let follow: Vec<_> = log
        .iter()
        .filter(|(effect, _)| effect.purpose == Purpose::FollowUp)
        .collect();
    assert_eq!(follow.len(), 2);
    assert_eq!(follow[0].0.group, follow[1].0.group);
    assert_eq!(follow[0].0.op, Op::Prompt { chars: 8 });
    assert_eq!(follow[1].0.op, Op::Key(Key::Enter));
    // The relaunch line is recorded by length, never by content.
    assert!(log.iter().any(
        |(effect, _)| effect.op == Op::Text { chars: 8 } && effect.purpose == Purpose::Relaunch
    ));
}

/// The scheduled-task refusal is a skip that still typed into the pane:
/// the interrupted turn's follow-up and the draft go back on the original
/// account, and both are recorded.
#[test]
fn a_session_with_scheduled_tasks_is_kept_and_what_was_typed_is_recorded() {
    let io = FakeIo::new(
        CLAUDE_WITH_DRAFT,
        Box::new(|io: &FakeIo, line: &str| {
            if line == "text \"\\r\"" && io.previous().as_deref() == Some("prompt \"/exit\"") {
                io.show(SCHEDULED);
            }
        }),
    );
    let (result, log, _) = run(
        &io,
        &target(AgentKind::ClaudeCode, false, true),
        &claude_launch(),
        Some("continue"),
        &mut NoObserver,
    );
    let Err(Refusal::Kept(note)) = result else {
        panic!("a session owning scheduled tasks must be kept");
    };
    assert!(note.contains("scheduled tasks"), "{note}");
    assert_eq!(
        io.sent(),
        [
            "text \"\\u{1b}\"",
            "text \"\\u{3}\"",
            "prompt \"/exit\"",
            "text \"\\r\"",
            "text \"\\u{1b}\"",
            "prompt \"continue\"",
            "text \"\\r\"",
            "prompt \"half typed\"",
        ]
    );
    let purposes: Vec<Purpose> = log.iter().map(|(effect, _)| effect.purpose).collect();
    assert_eq!(
        purposes,
        [
            Purpose::Interrupt,
            Purpose::ClearPrompt,
            Purpose::Exit,
            Purpose::Exit,
            Purpose::CancelExit,
            Purpose::KeptFollowUp,
            Purpose::KeptFollowUp,
            Purpose::KeptDraft,
        ]
    );
}

/// A relaunched agent that runs but never reaches its prompt is a failure,
/// and nothing is typed into it.
#[test]
fn a_readiness_timeout_with_the_agent_running_sends_nothing() {
    let io = FakeIo::new(
        CODEX_IDLE,
        Box::new(|io: &FakeIo, line: &str| {
            if line.starts_with("signal TERM") {
                io.kill(AGENT);
            }
            if line == "text \"\\r\"" {
                io.agent_appears_in(Duration::from_secs(1));
                io.show("loading…\n");
            }
        }),
    );
    let (result, _, _) = run(
        &io,
        &target(AgentKind::Codex, false, false),
        &launch(),
        Some("continue"),
        &mut NoObserver,
    );
    let Err(Refusal::Failed(note)) = result else {
        panic!("an unsettled resume must fail");
    };
    assert!(note.contains("did not become ready within 30s"), "{note}");
    assert!(!io.sent().iter().any(|line| line.contains("continue")));
}

/// A respawn the multiplexer accepted, followed by a pane that cannot be
/// found again: the effect happened, the observation failed, the pane stays
/// open.
#[test]
fn a_respawn_whose_pane_cannot_be_found_again_keeps_it_open() {
    let io = FakeIo::new(
        CODEX_IDLE,
        Box::new(|io: &FakeIo, line: &str| {
            if line.starts_with("signal TERM") {
                io.kill(AGENT);
            }
        }),
    );
    io.pane_pid.set(None);
    let (result, log, _) = run(
        &io,
        &target(AgentKind::Codex, true, false),
        &launch(),
        None,
        &mut NoObserver,
    );
    let Err(Refusal::Failed(note)) = result else {
        panic!("a lost pane must fail");
    };
    assert!(note.contains("could not find it again"), "{note}");
    assert_eq!(io.sent(), ["remain true", "signal TERM [10]", "respawn"]);
    assert!(log
        .iter()
        .any(|(effect, transport)| effect.op == Op::Respawn && *transport == Transport::Accepted));
}

/// An agent that will not die: TERM, TERM, KILL, then the pane kept open
/// for the swap is released again.
#[test]
fn an_agent_that_will_not_exit_is_signalled_then_its_pane_released() {
    let io = FakeIo::new(CODEX_IDLE, Box::new(|_: &FakeIo, _: &str| {}));
    let (result, log, _) = run(
        &io,
        &target(AgentKind::Codex, true, false),
        &launch(),
        None,
        &mut NoObserver,
    );
    let Err(Refusal::Failed(note)) = result else {
        panic!("an agent that does not exit must fail");
    };
    assert_eq!(note, "the agent did not exit");
    assert_eq!(
        io.sent(),
        [
            "remain true",
            "signal TERM [10]",
            "signal TERM [10]",
            "signal KILL [10]",
            "remain false",
        ]
    );
    // A signal is attempted, never confirmed; liveness is read separately.
    assert!(log
        .iter()
        .filter(|(effect, _)| matches!(effect.op, Op::Signal { .. }))
        .all(|(_, transport)| *transport == Transport::Unconfirmed));
}

/// The prompt reached the pane but its Enter did not: two effects, the
/// second refused, and the report says the follow-up was not typed.
#[test]
fn a_follow_up_whose_enter_fails_is_reported_and_recorded_apart() {
    let io = FakeIo::new(
        CODEX_IDLE,
        Box::new(|io: &FakeIo, line: &str| {
            if line.starts_with("signal TERM") {
                io.kill(AGENT);
            }
            if line == "text \"\\r\"" && io.previous().as_deref() == Some("text \"RELAUNCH\"") {
                io.agent_appears_in(Duration::from_secs(1));
            }
            if line == "prompt \"continue\"" {
                io.enter_ok.set(false);
            }
        }),
    );
    let (result, log, _) = run(
        &io,
        &target(AgentKind::Codex, false, false),
        &launch(),
        Some("continue"),
        &mut NoObserver,
    );
    assert_eq!(result.unwrap(), "resumed; follow-up could not be typed");
    let follow: Vec<_> = log
        .iter()
        .filter(|(effect, _)| effect.purpose == Purpose::FollowUp)
        .map(|(_, transport)| *transport)
        .collect();
    assert_eq!(follow, [Transport::Accepted, Transport::Refused]);
}

/// Fails `before` or `after` on the nth effect.
struct Failing {
    before_at: Option<usize>,
    after_at: Option<usize>,
    seen: usize,
}

/// Fails the observation of entering `phase`.
struct FailsAt(Phase);

impl EffectObserver for FailsAt {
    fn before(&mut self, _: &Effect) -> Result<(), ObserverError> {
        Ok(())
    }

    fn after(&mut self, _: &Effect, _: Transport) -> Result<(), ObserverError> {
        Ok(())
    }

    fn observe(&mut self, observation: &Observation) -> Result<(), ObserverError> {
        match observation {
            Observation::Phase { phase } if *phase == self.0 => {
                Err(ObserverError("journal full".into()))
            }
            _ => Ok(()),
        }
    }
}

impl EffectObserver for Failing {
    fn before(&mut self, _: &Effect) -> Result<(), ObserverError> {
        if self.before_at == Some(self.seen) {
            return Err(ObserverError("journal full".into()));
        }
        Ok(())
    }

    fn after(&mut self, _: &Effect, _: Transport) -> Result<(), ObserverError> {
        let at = self.seen;
        self.seen += 1;
        if self.after_at == Some(at) {
            return Err(ObserverError("journal gone".into()));
        }
        Ok(())
    }
}

/// An intent that cannot be recorded is never acted on, nor anything after.
#[test]
fn an_unrecorded_intent_does_nothing_at_all() {
    let io = FakeIo::new(CODEX_IDLE, codex_agent());
    let mut observer = Failing {
        before_at: Some(0),
        after_at: None,
        seen: 0,
    };
    let (_, log, halted) = run(
        &io,
        &target(AgentKind::Codex, false, false),
        &launch(),
        Some("continue"),
        &mut observer,
    );
    assert!(io.sent().is_empty(), "{:?}", io.sent());
    assert!(log.is_empty());
    assert!(halted.unwrap().contains("could not be recorded"));
}

/// A result that cannot be recorded leaves the effect done exactly once and
/// stops everything after it.
#[test]
fn an_unrecorded_result_is_done_once_and_stops_the_run() {
    let io = FakeIo::new(CODEX_IDLE, codex_agent());
    let mut observer = Failing {
        before_at: None,
        after_at: Some(0),
        seen: 0,
    };
    let (_, log, halted) = run(
        &io,
        &target(AgentKind::Codex, false, false),
        &launch(),
        Some("continue"),
        &mut observer,
    );
    assert_eq!(io.sent(), ["signal TERM [10]"]);
    assert_eq!(log.len(), 1);
    assert!(halted
        .unwrap()
        .contains("attempted but its result could not be recorded"));
}

/// Every phase, the readiness seen and the account found are reported, in
/// the order they happen.
#[test]
fn a_swap_reports_what_it_saw_in_order() {
    let io = FakeIo::new(CODEX_IDLE, codex_agent());
    let mut observer = NoObserver;
    let runner = Runner::new(&io, &mut observer);
    let verified = |_: &Target, _: Option<u32>| {
        Ok(AccountVerdict {
            check: AccountCheck::Matched,
            observed: Some("b".into()),
            source: "test",
            note: String::new(),
        })
    };
    execute(
        &runner,
        &target(AgentKind::Codex, false, false),
        &launch(),
        "RELAUNCH",
        Some("continue"),
        &verified,
    )
    .unwrap();
    assert_eq!(
        runner.observations(),
        [
            Observation::Phase {
                phase: Phase::Stopping
            },
            Observation::Phase {
                phase: Phase::Launching
            },
            Observation::Phase {
                phase: Phase::Readiness
            },
            Observation::Readiness {
                result: Readiness::Ready
            },
            Observation::Phase {
                phase: Phase::Verifying
            },
            Observation::AccountCheck {
                result: AccountCheck::Matched,
                observed: Some("b".into()),
                source: "test".into(),
            },
            Observation::Continuation {
                decision: ContinueDecision::Allowed,
                withheld: None,
                draft_allowed: true,
            },
            Observation::Phase {
                phase: Phase::FollowThrough
            },
        ]
    );
}

/// A phase that cannot be recorded stops the run before its first effect:
/// the agent was stopped, the relaunch is never typed.
#[test]
fn an_unrecorded_phase_stops_the_run_before_its_effects() {
    let io = FakeIo::new(CODEX_IDLE, codex_agent());
    let (result, _, halted) = run(
        &io,
        &target(AgentKind::Codex, false, false),
        &launch(),
        Some("continue"),
        &mut FailsAt(Phase::Launching),
    );
    assert!(result.is_err());
    assert!(halted
        .unwrap()
        .contains("observation could not be recorded"));
    assert_eq!(io.sent(), ["signal TERM [10]"]);
}

/// A mismatched account is reported as such, with the account found, and
/// refuses the follow-up.
#[test]
fn a_mismatched_account_is_reported_with_what_was_found() {
    let io = FakeIo::new(CODEX_IDLE, codex_agent());
    let mut observer = NoObserver;
    let runner = Runner::new(&io, &mut observer);
    let wrong = |_: &Target, _: Option<u32>| {
        Err(AccountVerdict {
            check: AccountCheck::Mismatched,
            observed: Some("c".into()),
            source: "test",
            note: "the resumed agent runs on c".into(),
        })
    };
    let result = execute(
        &runner,
        &target(AgentKind::Codex, false, false),
        &launch(),
        "RELAUNCH",
        Some("continue"),
        &wrong,
    );
    let Err(Refusal::Failed(note)) = result else {
        panic!("a mismatched account must refuse");
    };
    assert_eq!(
        note,
        "relaunched, but the resumed agent runs on c; nothing was sent to the pane"
    );
    assert!(runner.observations().contains(&Observation::AccountCheck {
        result: AccountCheck::Mismatched,
        observed: Some("c".into()),
        source: "test".into(),
    }));
    assert!(!io.sent().iter().any(|line| line.contains("continue")));
}

// --- continuation evidence ------------------------------------------------

const CODEX_WITH_DRAFT: &str = "› half typed\n";

fn words(argv: &[&str]) -> Vec<String> {
    argv.iter().map(|w| (*w).to_owned()).collect()
}

/// What a run returned, what was typed, what was observed, and which bound
/// process each account check was asked about.
type EvidenceRun = (
    Result<String, Refusal>,
    Vec<String>,
    Vec<Observation>,
    Vec<Option<u32>>,
);

/// Run a Codex swap with a draft in the composer under `evidence`, the
/// relaunch's processes as given, and an account check that says `account`.
fn run_with_evidence(
    evidence: super::ContinueEvidence,
    verified: bool,
    argvs: Option<Vec<AgentArgv>>,
    account: AccountCheck,
    observer: &mut dyn EffectObserver,
) -> EvidenceRun {
    let io = FakeIo::new(CODEX_WITH_DRAFT, codex_agent());
    *io.argvs.borrow_mut() = argvs;
    let mut target = target(AgentKind::Codex, false, false);
    target.evidence = evidence;
    target.conversation_verified = verified;
    let asked = RefCell::new(Vec::new());
    let check = |_: &Target, bound: Option<u32>| {
        asked.borrow_mut().push(bound);
        Ok(AccountVerdict {
            check: account,
            observed: None,
            source: "test",
            note: String::new(),
        })
    };
    let runner = Runner::new(&io, observer);
    let result = execute(
        &runner,
        &target,
        &launch(),
        "RELAUNCH",
        Some("continue"),
        &check,
    );
    let observations = runner.observations();
    drop(runner);
    (result, io.sent(), observations, asked.into_inner())
}

fn decision(
    observations: &[Observation],
) -> Option<(ContinueDecision, Option<WithheldReason>, bool)> {
    observations.iter().find_map(|o| match o {
        Observation::Continuation {
            decision,
            withheld,
            draft_allowed,
        } => Some((*decision, *withheld, *draft_allowed)),
        _ => None,
    })
}

/// The account is read on the very process bound to the conversation.
#[test]
fn a_bound_relaunch_gets_its_follow_up_and_its_draft_back() {
    let (result, sent, observations, asked) = run_with_evidence(
        super::ContinueEvidence::Binding,
        false,
        Some(vec![(31, Some(words(&["codex", "resume", "S"])))]),
        AccountCheck::Unchecked,
        &mut NoObserver,
    );
    let note = result.unwrap();
    assert!(sent.contains(&"prompt \"continue\"".to_owned()), "{sent:?}");
    assert!(
        sent.contains(&"prompt \"half typed\"".to_owned()),
        "{sent:?}"
    );
    assert!(note.contains("restored the half-typed prompt"), "{note}");
    assert_eq!(
        decision(&observations),
        Some((ContinueDecision::Allowed, None, true))
    );
    assert_eq!(asked, [Some(31)]);
}

/// No process resuming exactly the conversation: neither the follow-up nor
/// the draft is typed, and the report does not claim a resume.
#[test]
fn a_relaunch_without_the_resume_binding_gets_neither_follow_up_nor_draft() {
    for argvs in [
        vec![(31, Some(words(&["codex", "resume", "S-other"])))],
        vec![(31, Some(words(&["codex", "resume", "--last"])))],
        vec![],
    ] {
        let (result, sent, observations, asked) = run_with_evidence(
            super::ContinueEvidence::Binding,
            true,
            Some(argvs),
            AccountCheck::Matched,
            &mut NoObserver,
        );
        let note = result.unwrap();
        assert!(!sent.iter().any(|l| l.contains("continue")), "{sent:?}");
        assert!(!sent.iter().any(|l| l.contains("half typed")), "{sent:?}");
        assert!(
            note.starts_with("relaunched; resume binding unconfirmed"),
            "{note}"
        );
        assert!(
            note.contains("follow-up withheld (binding_missing)"),
            "{note}"
        );
        assert!(note.contains("not put back"), "{note}");
        assert_eq!(
            decision(&observations),
            Some((
                ContinueDecision::Withheld,
                Some(WithheldReason::BindingMissing),
                false
            ))
        );
        assert_eq!(asked, [None]);
    }
}

/// Two processes resuming it, or one that cannot be read, is not a binding.
#[test]
fn an_ambiguous_or_unreadable_relaunch_is_not_a_binding() {
    let (_, sent, observations, _) = run_with_evidence(
        super::ContinueEvidence::Binding,
        true,
        Some(vec![
            (31, Some(words(&["codex", "resume", "S"]))),
            (32, Some(words(&["codex", "resume", "S"]))),
        ]),
        AccountCheck::Matched,
        &mut NoObserver,
    );
    assert!(!sent.iter().any(|l| l.contains("continue")), "{sent:?}");
    assert_eq!(
        decision(&observations).map(|d| d.1),
        Some(Some(WithheldReason::BindingAmbiguous))
    );
    let (_, sent, observations, _) = run_with_evidence(
        super::ContinueEvidence::Binding,
        true,
        Some(vec![(31, None)]),
        AccountCheck::Matched,
        &mut NoObserver,
    );
    assert!(!sent.iter().any(|l| l.contains("continue")), "{sent:?}");
    assert_eq!(
        decision(&observations).map(|d| d.1),
        Some(Some(WithheldReason::BindingUnreadable))
    );
    // A match beside an unreadable agent process is no binding either.
    let (_, sent, observations, _) = run_with_evidence(
        super::ContinueEvidence::Binding,
        true,
        Some(vec![
            (31, Some(words(&["codex", "resume", "S"]))),
            (32, None),
        ]),
        AccountCheck::Matched,
        &mut NoObserver,
    );
    assert!(
        !sent
            .iter()
            .any(|l| l.contains("continue") || l.contains("half typed")),
        "{sent:?}"
    );
    assert_eq!(
        decision(&observations),
        Some((
            ContinueDecision::Withheld,
            Some(WithheldReason::BindingUnreadable),
            false
        ))
    );
}

/// `verified` also needs the stopped process read and the account matched;
/// the draft still goes back into a bound relaunch.
#[test]
fn verified_evidence_withholds_the_follow_up_but_not_the_draft() {
    let bound = || Some(vec![(31, Some(words(&["codex", "resume", "S"])))]);
    let cases = [
        (
            false,
            AccountCheck::Matched,
            Some(WithheldReason::UnverifiedConversation),
        ),
        (
            true,
            AccountCheck::Unchecked,
            Some(WithheldReason::AccountUnconfirmed),
        ),
        (
            true,
            AccountCheck::Unreadable,
            Some(WithheldReason::AccountUnconfirmed),
        ),
        (true, AccountCheck::Matched, None),
    ];
    for (verified, account, withheld) in cases {
        let (_, sent, observations, _) = run_with_evidence(
            super::ContinueEvidence::Verified,
            verified,
            bound(),
            account,
            &mut NoObserver,
        );
        assert_eq!(
            sent.iter().any(|l| l == "prompt \"continue\""),
            withheld.is_none(),
            "{verified} {account:?}: {sent:?}"
        );
        assert!(
            sent.contains(&"prompt \"half typed\"".to_owned()),
            "{sent:?}"
        );
        assert_eq!(decision(&observations).map(|d| d.1), Some(withheld));
    }
    // `binding` asks for none of that.
    for (verified, account) in [
        (false, AccountCheck::Unchecked),
        (true, AccountCheck::Unreadable),
    ] {
        let (_, sent, _, _) = run_with_evidence(
            super::ContinueEvidence::Binding,
            verified,
            bound(),
            account,
            &mut NoObserver,
        );
        assert!(sent.contains(&"prompt \"continue\"".to_owned()), "{sent:?}");
    }
}

/// A decision that cannot be recorded stops the run: nothing more is typed.
#[test]
fn an_unrecorded_continuation_decision_types_nothing_more() {
    struct FailsOnDecision;
    impl EffectObserver for FailsOnDecision {
        fn before(&mut self, _: &Effect) -> Result<(), ObserverError> {
            Ok(())
        }
        fn after(&mut self, _: &Effect, _: Transport) -> Result<(), ObserverError> {
            Ok(())
        }
        fn observe(&mut self, observation: &Observation) -> Result<(), ObserverError> {
            match observation {
                Observation::Continuation { .. } => Err(ObserverError("journal full".into())),
                _ => Ok(()),
            }
        }
    }
    let (result, sent, _, _) = run_with_evidence(
        super::ContinueEvidence::Binding,
        true,
        Some(vec![(31, Some(words(&["codex", "resume", "S"])))]),
        AccountCheck::Matched,
        &mut FailsOnDecision,
    );
    assert!(result.is_err());
    assert!(
        !sent
            .iter()
            .any(|l| l.contains("continue") || l.contains("half typed")),
        "{sent:?}"
    );
}

/// No follow-up due is not missing evidence: the draft still goes back.
#[test]
fn a_swap_with_no_follow_up_due_still_restores_the_draft() {
    let io = FakeIo::new(CODEX_WITH_DRAFT, codex_agent());
    let (result, _, _) = run(
        &io,
        &target(AgentKind::Codex, false, false),
        &launch(),
        None,
        &mut NoObserver,
    );
    assert!(result.unwrap().contains("restored the half-typed prompt"));
    assert!(io.sent().contains(&"prompt \"half typed\"".to_owned()));
}
