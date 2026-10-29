//! The executor against a scripted pane: virtual time, a screen, processes
//! that die or appear when the right keys arrive. Each test pins the exact
//! sequence of operations a branch performs, failure and keep branches
//! included, and that every one of them went through the [`Runner`].

use super::effects::{
    Effect, EffectObserver, Key, NoObserver, ObserverError, Op, Purpose, Runner, Transport,
};
use super::io::{Clock, SwapIo};
use super::{execute, LaunchLine, PaneLaunch, Refusal, Target};
use crate::event::AgentKind;
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
    let runner = Runner::new(io, observer);
    let verified = |_: &Target| Ok(String::new());
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
