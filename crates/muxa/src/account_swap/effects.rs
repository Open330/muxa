//! Every change the swap executor makes to a target — a key, a typed line,
//! a prompt, a signal, keeping the pane open, respawning it — as one
//! recorded effect.
//!
//! [`Runner`] is the only way the executor mutates a target. Each physical
//! operation is offered to an [`EffectObserver`] before it happens and
//! reported after, so a journal can hold the intent before the terminal or
//! the process sees anything. A prompt and the Enter that submits it are two
//! effects in one group: a failure between them is representable.
//!
//! An observer that cannot record the intent stops the run before that
//! effect and every later one; one that cannot record the result stops the
//! run after it, the effect having happened exactly once. The default
//! [`NoObserver`] records nothing and never fails, so a swap behaves as it
//! did before this seam existed.
//!
//! Typed text never enters an effect, only its length: follow-ups and drafts
//! are the user's words, and a relaunch line may carry credentials.

use super::io::SwapIo;
use std::cell::{Cell, RefCell};

/// Why the executor did this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Purpose {
    /// Keep a directly-run pane open across the agent's exit.
    KeepPaneOpen,
    /// Let a directly-run pane close with its agent again.
    ReleasePane,
    /// Esc to interrupt a turn before `/exit`.
    Interrupt,
    /// Ctrl-C to clear a half-typed draft before `/exit`.
    ClearPrompt,
    /// `/exit`.
    Exit,
    /// A key answering one of Claude's exit dialogs.
    ExitDialog,
    /// Esc to back out of an exit the swap will not finish.
    CancelExit,
    /// The follow-up sent to a session kept on its original account.
    KeptFollowUp,
    /// The half-typed prompt put back into a kept session.
    KeptDraft,
    /// A signal stopping the agent.
    Stop,
    /// The relaunch line typed into the pane's shell.
    Relaunch,
    /// The pane respawned with the relaunch.
    Respawn,
    /// A key answering a startup dialog of the relaunched agent.
    StartupDialog,
    /// The follow-up sent on the new account.
    FollowUp,
    /// The half-typed prompt put back after the follow-up.
    RestoreDraft,
}

/// A key the executor types, by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Key {
    Esc,
    CtrlC,
    Enter,
    Down,
}

/// The physical operation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Op {
    Key(Key),
    /// Raw text of this many characters.
    Text {
        chars: usize,
    },
    /// A prompt of this many characters, typed the composer's way.
    Prompt {
        chars: usize,
    },
    RemainOnExit(bool),
    Respawn,
    Signal {
        signal: String,
        pids: Vec<u32>,
    },
}

/// One recorded effect. `group` ties together the operations of one act
/// (a prompt and its Enter, the keys of one dialog answer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Effect {
    pub(crate) group: u32,
    pub(crate) purpose: Purpose,
    pub(crate) op: Op,
}

/// What the transport said. Not proof of what the target did: a refused
/// write may still have reached the pane, and a signal is never confirmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Transport {
    Accepted,
    Refused,
    Unconfirmed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ObserverError(pub(crate) String);

pub(crate) trait EffectObserver {
    /// Called before the effect; an error means it must not happen.
    fn before(&mut self, effect: &Effect) -> Result<(), ObserverError>;
    /// Called after it; an error means it happened but its result is not
    /// recorded.
    fn after(&mut self, effect: &Effect, transport: Transport) -> Result<(), ObserverError>;
    /// Called with what the executor saw — a phase entered, readiness, the
    /// account found. An error stops the run before its next effect.
    fn observe(
        &mut self,
        _observation: &crate::swap_job::Observation,
    ) -> Result<(), ObserverError> {
        Ok(())
    }
}

/// Records nothing.
pub(crate) struct NoObserver;

impl EffectObserver for NoObserver {
    fn before(&mut self, _: &Effect) -> Result<(), ObserverError> {
        Ok(())
    }

    fn after(&mut self, _: &Effect, _: Transport) -> Result<(), ObserverError> {
        Ok(())
    }
}

/// The executor's handle on a target: reads go straight to [`SwapIo`],
/// mutations through the observer.
pub(super) struct Runner<'a> {
    pub(super) io: &'a dyn SwapIo,
    observer: RefCell<&'a mut dyn EffectObserver>,
    log: RefCell<Vec<(Effect, Transport)>>,
    seen: RefCell<Vec<crate::swap_job::Observation>>,
    next_group: Cell<u32>,
    halted: RefCell<Option<String>>,
}

impl<'a> Runner<'a> {
    pub(super) fn new(io: &'a dyn SwapIo, observer: &'a mut dyn EffectObserver) -> Self {
        Self {
            io,
            observer: RefCell::new(observer),
            log: RefCell::new(Vec::new()),
            seen: RefCell::new(Vec::new()),
            next_group: Cell::new(0),
            halted: RefCell::new(None),
        }
    }

    /// A fresh group id for the operations of one act.
    pub(super) fn group(&self) -> u32 {
        let group = self.next_group.get();
        self.next_group.set(group + 1);
        group
    }

    /// Why the run stopped making effects, if it did.
    pub(super) fn halted(&self) -> Option<String> {
        self.halted.borrow().clone()
    }

    /// Every effect that happened, in order, with what the transport said.
    #[cfg(test)]
    pub(super) fn log(&self) -> Vec<(Effect, Transport)> {
        self.log.borrow().clone()
    }

    /// Every observation reported, in order.
    pub(super) fn observations(&self) -> Vec<crate::swap_job::Observation> {
        self.seen.borrow().clone()
    }

    /// Report what the executor saw. `false` once the run has halted, or if
    /// it could not be recorded, which halts it: what comes next may depend
    /// on it.
    pub(super) fn observe(&self, observation: crate::swap_job::Observation) -> bool {
        if self.halted.borrow().is_some() {
            return false;
        }
        if let Err(ObserverError(why)) = self.observer.borrow_mut().observe(&observation) {
            *self.halted.borrow_mut() = Some(format!(
                "stopped: an observation could not be recorded ({why})"
            ));
            return false;
        }
        self.seen.borrow_mut().push(observation);
        true
    }

    /// Type one key or raw `text` (a relaunch line).
    pub(super) fn send_text(&self, group: u32, purpose: Purpose, text: &str) -> bool {
        let op = match text {
            "\x1b" => Op::Key(Key::Esc),
            "\x03" => Op::Key(Key::CtrlC),
            "\r" => Op::Key(Key::Enter),
            "\x1b[B" => Op::Key(Key::Down),
            _ => Op::Text {
                chars: text.chars().count(),
            },
        };
        self.effect(group, purpose, op, || accepted(self.io.send_text(text)))
    }

    pub(super) fn send_prompt(&self, group: u32, purpose: Purpose, text: &str) -> bool {
        let op = Op::Prompt {
            chars: text.chars().count(),
        };
        self.effect(group, purpose, op, || {
            accepted(self.io.send_prompt_text(text))
        })
    }

    pub(super) fn set_remain_on_exit(&self, purpose: Purpose, on: bool) -> bool {
        let group = self.group();
        self.effect(group, purpose, Op::RemainOnExit(on), || {
            accepted(self.io.set_remain_on_exit(on))
        })
    }

    pub(super) fn respawn(&self, purpose: Purpose, cwd: &str, line: &str) -> bool {
        let group = self.group();
        self.effect(group, purpose, Op::Respawn, || {
            accepted(self.io.respawn(cwd, line))
        })
    }

    pub(super) fn signal(&self, purpose: Purpose, signal: &str, pids: &[u32]) {
        let group = self.group();
        let op = Op::Signal {
            signal: signal.to_owned(),
            pids: pids.to_vec(),
        };
        self.effect(group, purpose, op, || {
            self.io.signal(signal, pids);
            Transport::Unconfirmed
        });
    }

    /// Offer `op` to the observer, do it, and report it. `false` when it was
    /// not done, or the transport refused it.
    fn effect(
        &self,
        group: u32,
        purpose: Purpose,
        op: Op,
        act: impl FnOnce() -> Transport,
    ) -> bool {
        if self.halted.borrow().is_some() {
            return false;
        }
        let effect = Effect { group, purpose, op };
        if let Err(ObserverError(why)) = self.observer.borrow_mut().before(&effect) {
            *self.halted.borrow_mut() = Some(format!(
                "stopped before {purpose:?}: its intent could not be recorded ({why})"
            ));
            return false;
        }
        let transport = act();
        self.log.borrow_mut().push((effect.clone(), transport));
        if let Err(ObserverError(why)) = self.observer.borrow_mut().after(&effect, transport) {
            *self.halted.borrow_mut() = Some(format!(
                "stopped after {purpose:?}: it was attempted but its result could not be recorded ({why})"
            ));
        }
        transport != Transport::Refused
    }
}

fn accepted(ok: bool) -> Transport {
    if ok {
        Transport::Accepted
    } else {
        Transport::Refused
    }
}
