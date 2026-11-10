//! Mechanical preparation of an explicitly launched worker, before any task is sent.
//! The caller must fence the new process, pane and authorized working directory.
//! No conversation prompt, login choice or tool permission is answered here.
use super::{
    composer_draft,
    io::{self, Clock},
    option_rows, pending_dialog, turn_running, type_keys, wait_for_resume_answering, Consent,
    Resume, READY_QUIET, STARTUP_DIALOGS,
};
use crate::event::AgentKind;
use serde::Serialize;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Ready,
    Blocked,
    Exited,
    Unsettled,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Report {
    pub state: State,
    pub answered: Vec<String>,
    pub reason: Option<String>,
}

// Claude inserts hard line breaks in a narrow pane, so tmux's -J cannot
// repair the two wrapped option labels. Normalize only an exact two-row
// folder-trust menu and its exact footer, never arbitrary prose or approvals.
fn normalize_capture(screen: String) -> String {
    let lines: Vec<_> = screen.lines().collect();
    let compact = |text: &str| {
        text.chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
    };
    let Some(footer) = lines
        .iter()
        .rposition(|line| line.trim_start().starts_with("Enter"))
    else {
        return screen;
    };
    if compact(&lines[footer..].join("")) != "Entertoconfirm·Esctocancel" {
        return screen;
    }
    let mut end = footer;
    while end > 0 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    let start = lines[..end]
        .iter()
        .rposition(|line| line.trim().is_empty())
        .map_or(0, |i| i + 1);
    let menu = compact(&lines[start..end].join(""));
    let yes = "Yes, I trust this folder";
    for no in ["No, exit", "No, exit Claude Code"] {
        for (first, second) in [(no, yes), (yes, no)] {
            for selected_first in [true, false] {
                let expected = if selected_first {
                    format!("❯{first}{second}")
                } else {
                    format!("{first}❯{second}")
                };
                if menu == compact(&expected) {
                    return if selected_first {
                        format!(" ❯ {first}\n   {second}\n Enter to confirm · Esc to cancel")
                    } else {
                        format!("   {first}\n ❯ {second}\n Enter to confirm · Esc to cancel")
                    };
                }
            }
        }
    }
    screen
}

/// Reuses the account-switch startup parser and settling rules. `alive` must
/// check the original launch identity, not merely that a pane still exists.
/// The permission profile is retained: bypass consent is possible only when
/// the caller explicitly launched with it. Folder settings remain a blocker.
pub fn prepare(
    kind: AgentKind,
    capture: impl FnMut() -> Option<String>,
    alive: impl FnMut() -> bool,
    send_key: impl FnMut(&str) -> bool,
    bypass_requested: bool,
    timeout: Duration,
) -> Report {
    if !matches!(kind, AgentKind::ClaudeCode | AgentKind::Codex) {
        return Report {
            state: State::Blocked,
            answered: vec![],
            reason: Some("provider has no verified startup/composer probe".into()),
        };
    }
    prepare_with(
        &io::RealClock,
        kind,
        capture,
        alive,
        send_key,
        bypass_requested,
        timeout,
        READY_QUIET,
        Duration::from_millis(250),
    )
}

#[allow(clippy::too_many_arguments)]
fn prepare_with(
    clock: &dyn Clock,
    kind: AgentKind,
    mut capture: impl FnMut() -> Option<String>,
    mut alive: impl FnMut() -> bool,
    mut send_key: impl FnMut(&str) -> bool,
    bypass_requested: bool,
    timeout: Duration,
    quiet: Duration,
    tick: Duration,
) -> Report {
    let last = std::cell::RefCell::new(String::new());
    let mut answered = Vec::new();
    let resume = wait_for_resume_answering(
        clock,
        || {
            let raw = capture()?;
            let screen = if kind == AgentKind::ClaudeCode {
                normalize_capture(raw)
            } else {
                raw
            };
            last.replace(screen.clone());
            Some(screen)
        },
        &mut alive,
        |keys| {
            // Only initial setup, never resume an old goal or answer a task's
            // approval. A quoted/ambiguous dialog is rejected by the parser.
            let screen = last.borrow();
            let dialog = pending_dialog(&screen, STARTUP_DIALOGS, &[]);
            let allowed = dialog.is_some_and(|d| {
                (d.label == "trusted the folder"
                    // Codex trust can alter sandbox defaults; keep it blocked
                    // until its cold trust flow is independently verified.
                    && kind == AgentKind::ClaudeCode
                    && d.pick == Some("Yes, I trust this folder")
                    && option_rows(&screen).is_some_and(|(rows, _)| {
                        rows.len() == 2
                            && rows.iter().any(|row| {
                                matches!(row.as_str(), "No, exit" | "No, exit Claude Code" | "Quit")
                            })
                    }))
                    || (kind == AgentKind::Codex && d.label == "skipped the Codex update prompt")
                    || (bypass_requested && d.consent == Consent::Bypass)
            });
            allowed && type_keys(clock, &mut send_key, keys)
        },
        &|consent| consent == Consent::None || (bypass_requested && consent == Consent::Bypass),
        timeout,
        quiet,
        tick,
        &mut answered,
    );
    let (state, reason) = match resume {
        Resume::Ready
            if turn_running(&last.borrow()) || composer_draft(&last.borrow(), kind).is_some() =>
        {
            (
                State::Blocked,
                Some("worker is busy or its composer contains a draft; nothing was cleared".into()),
            )
        }
        Resume::Ready => (State::Ready, None),
        Resume::Blocked(reason) => (State::Blocked, Some(reason)),
        Resume::Exited => (
            State::Exited,
            Some("the original worker process or working directory changed".into()),
        ),
        Resume::Unsettled => (
            State::Unsettled,
            Some("no stable agent input prompt observed before the deadline".into()),
        ),
    };
    Report {
        state,
        answered: answered.into_iter().map(str::to_owned).collect(),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account_swap::{DOWN, ENTER};
    use std::cell::Cell;
    use std::time::Instant;
    struct FakeClock {
        base: Instant,
        elapsed: Cell<Duration>,
    }
    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            self.base + self.elapsed.get()
        }
        fn sleep(&self, d: Duration) {
            self.elapsed.set(self.elapsed.get() + d);
        }
    }
    #[test]
    fn codex_folder_trust_remains_blocked_without_a_verified_sandbox_flow() {
        let clock = FakeClock {
            base: Instant::now(),
            elapsed: Cell::new(Duration::ZERO),
        };
        let report = prepare_with(
            &clock,
            AgentKind::Codex,
            || {
                Some("Trust this folder\n› 1. Trust and continue\n  2. Quit\n  enter continue · esc quit".into())
            },
            || true,
            |_| panic!("must not accept Codex trust"),
            false,
            Duration::from_secs(1),
            Duration::from_millis(100),
            Duration::from_millis(25),
        );
        assert_ne!(report.state, State::Ready);
        assert!(report.answered.is_empty());
    }
    const TRUST: &str =
        " ❯ No, exit\n   Yes, I trust this folder\n Enter to confirm · Esc to cancel";
    const READY: &str = "Claude Code\n❯ \n ? for shortcuts";
    fn run(
        screen: impl FnMut() -> Option<String>,
        alive: impl FnMut() -> bool,
        send: impl FnMut(&str) -> bool,
    ) -> Report {
        prepare_with(
            &FakeClock {
                base: Instant::now(),
                elapsed: Cell::new(Duration::ZERO),
            },
            AgentKind::ClaudeCode,
            screen,
            alive,
            send,
            false,
            Duration::from_secs(3),
            Duration::from_millis(100),
            Duration::from_millis(25),
        )
    }
    #[test]
    fn custom_claude_statusline_does_not_hide_its_framed_composer() {
        let screen = "Claude Code v2.1.322\n─────────────────\n❯ \u{a0}\n─────────────────\n ~/p/muxa Opus 5.5 0 0\n ⎇ main 453ms $0.00";
        assert_eq!(
            run(|| Some(screen.into()), || true, |_| panic!()).state,
            State::Ready
        );
        for unproven in [
            "Claude Code\n❯ ",
            "$ claude\n─────\n❯ \n─────",
            "Claude Code\n─────\nLoading\n─────",
        ] {
            assert_eq!(
                run(|| Some(unproven.into()), || true, |_| panic!()).state,
                State::Unsettled
            );
        }
        let draft = screen.replace("❯ \u{a0}", "❯ unsent draft");
        assert_eq!(
            run(|| Some(draft.clone()), || true, |_| panic!()).state,
            State::Blocked
        );
    }
    #[test]
    fn trust_is_answered_then_a_real_prompt_is_required() {
        let keys = std::cell::RefCell::new(Vec::new());
        let result = run(
            || {
                Some(
                    if keys.borrow().len() == 2 {
                        READY
                    } else {
                        TRUST
                    }
                    .into(),
                )
            },
            || true,
            |key| {
                keys.borrow_mut().push(key.to_owned());
                true
            },
        );
        assert_eq!(result.state, State::Ready);
        assert_eq!(*keys.borrow(), [DOWN, ENTER]);
        assert_eq!(result.answered, ["trusted the folder"]);
    }
    #[test]
    fn settings_tools_and_bypass_are_not_startup_authority() {
        for screen in [
            " ❯ No, exit\n   Yes, I trust these settings\n Enter to confirm · Esc to cancel",
            "Bypass Permissions mode\n ❯ No, exit\n   Yes, I accept\n Enter to confirm · Esc to cancel",
            "Allow this read outside the working directories?\n ❯ 1. Yes, always\n   2. No\n Enter to confirm · Esc to cancel",
        ] {
            let result = run(|| Some(screen.into()), || true, |_| panic!("must not answer a permission dialog"));
            assert_eq!(result.state, State::Blocked, "{screen}");
        }
    }
    #[test]
    fn process_exit_and_shell_echo_are_never_ready() {
        assert_eq!(
            run(|| Some(TRUST.into()), || false, |_| panic!()).state,
            State::Exited
        );
        assert_eq!(
            run(
                || Some("$ claude --session-id new".into()),
                || true,
                |_| panic!()
            )
            .state,
            State::Unsettled
        );
    }
    #[test]
    fn a_failed_write_does_not_repeat_an_answer() {
        let count = Cell::new(0);
        let result = run(
            || Some(TRUST.into()),
            || true,
            |_| {
                count.set(count.get() + 1);
                false
            },
        );
        assert_eq!(result.state, State::Blocked);
        assert_eq!(count.get(), 1);
    }
    #[test]
    fn a_lingering_gate_is_not_answered_twice() {
        let count = Cell::new(0);
        let result = run(
            || Some(TRUST.into()),
            || true,
            |_| {
                count.set(count.get() + 1);
                true
            },
        );
        assert_ne!(result.state, State::Ready);
        assert_eq!(
            count.get(),
            2,
            "only Down and Enter, never a repeated confirmation"
        );
    }
    #[test]
    fn a_draft_or_running_turn_is_not_an_empty_composer() {
        for screen in [
            "──────\n❯ existing draft\n──────\n? for shortcuts",
            "✻ Thinking… (12s · ↓ 300 tokens)\n──────\n❯ \n? for shortcuts",
        ] {
            let result = run(
                || Some(screen.into()),
                || true,
                |_| panic!("must not clear another input"),
            );
            assert_eq!(result.state, State::Blocked);
        }
    }
    #[test]
    fn narrow_claude_hard_wrapping_is_handled_without_guessing_other_dialogs() {
        let count = Cell::new(0);
        let narrow = "Security guide\n\n ❯ No, exit\n   Yes, I trust this\n   folder\n\n Enter to confirm ·\n Esc to cancel\n";
        let result = run(
            || Some(if count.get() == 2 { READY } else { narrow }.into()),
            || true,
            |_| {
                count.set(count.get() + 1);
                true
            },
        );
        assert_eq!(result.state, State::Ready);
        assert_eq!(count.get(), 2);
    }
}
