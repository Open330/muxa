//! Mechanical preparation of an explicitly launched worker, before any task is sent.
//! The caller must fence the new process, pane and authorized working directory.
//! No conversation prompt, login choice or tool permission is answered here.
use crate::agent_tui::{
    self as io, composer_draft, option_rows, pending_dialog, turn_running, type_keys,
    wait_for_resume_answering_with_probe, Clock, Consent, Resume, READY_QUIET, STARTUP_DIALOGS,
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

// Custom statusLine replaces Claude's default prompt hints. This probe is
// worker-only: account-swap resume must never accept an old composer's frame.
// Restrict the actual composer to the bottom of the screen, leaving at most
// three footer lines and rejecting shell/relaunch output underneath it.
fn claude_composer_visible(screen: &str) -> bool {
    let tail = crate::agent_tui::screen_tail(screen, 12);
    let lines: Vec<_> = tail.lines().collect();
    let rules: Vec<_> = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            let line = line.trim();
            (line.chars().count() >= 5 && line.chars().all(|c| c == '─')).then_some(index)
        })
        .collect();
    let [.., top, bottom] = rules[..] else {
        return false;
    };
    let footer: Vec<_> = lines[bottom + 1..]
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect();
    footer.len() <= 3
        && !footer.iter().any(|line| {
            line.starts_with(['$', '❯', '›', '>', '#', '%'])
                || line.starts_with("claude ")
                || line.starts_with("exec ")
                || line.contains(" $ ")
                || line.contains(" % ")
                || line.contains(" --resume")
                || line.contains(" --session-id")
        })
        && lines[top + 1..bottom]
            .first()
            .is_some_and(|line| line.trim_start().starts_with('❯'))
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
    let resume = wait_for_resume_answering_with_probe(
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
        &|screen| {
            crate::agent_tui::agent_prompt_visible(screen)
                || (kind == AgentKind::ClaudeCode && claude_composer_visible(screen))
        },
    );
    let (state, reason) = match resume {
        // Codex asks before running hooks that are new or changed. Accepting
        // lets them run outside the sandbox, so it stays the user's call.
        _ if kind == AgentKind::Codex && codex_hook_review(&last.borrow()) => (
            State::Blocked,
            Some(
                "Codex asks to review new or changed hooks before it starts; answer it in the \
                 pane (tend does not trust hooks for you), then prepare the worker again"
                    .into(),
            ),
        ),
        Resume::Ready
            if turn_running(&last.borrow())
                || composer_draft(&last.borrow(), kind)
                    .is_some_and(|text| !(kind == AgentKind::Codex && cut_placeholder(&text))) =>
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

/// Codex's startup review of new or changed hooks, as the live menu at the
/// bottom of the screen: its title, the numbered options with one selected,
/// and the key-hint footer last. Text that merely mentions it (a transcript
/// line above a normal composer) is not the menu.
fn codex_hook_review(screen: &str) -> bool {
    let tail = crate::agent_tui::screen_tail(screen, 20);
    let lines: Vec<&str> = tail
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines
        .iter()
        .any(|line| line.starts_with('›') && is_codex_composer_line(line))
    {
        return false;
    }
    let Some(title) = lines.iter().rposition(|line| *line == "Hooks need review") else {
        return false;
    };
    // The footer is the last line or two; Codex wraps it in a narrow pane,
    // and older builds word it "Press enter to confirm or esc to go back".
    let footer_at = lines.len().saturating_sub(2).max(title + 1);
    let footer = lines[footer_at..].join(" ").to_lowercase();
    let footer_lines = if footer.contains("enter") && footer.contains("confirm") {
        lines[footer_at..]
            .iter()
            .position(|line| line.to_lowercase().contains("enter"))
            .map_or(0, |start| lines.len() - footer_at - start)
    } else {
        return false;
    };
    // Numbered rows; a wrapped label continues on the next lines.
    let mut rows: Vec<(bool, String)> = Vec::new();
    for line in &lines[title + 1..lines.len() - footer_lines] {
        let selected = line.starts_with('›');
        let body = line.trim_start_matches('›').trim_start();
        match body.split_once(". ") {
            Some((n, label)) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => {
                rows.push((selected, label.to_owned()));
            }
            _ => {
                if let Some((_, label)) = rows.last_mut() {
                    label.push(' ');
                    label.push_str(body);
                }
            }
        }
    }
    let position = |label: &str| rows.iter().position(|(_, row)| row == label);
    matches!(
        (position("Review hooks"), position("Trust all and continue")),
        (Some(review), Some(trust)) if review < trust
    ) && rows.iter().any(|(selected, _)| *selected)
}

/// An empty Codex composer or one showing a placeholder.
fn is_codex_composer_line(line: &str) -> bool {
    let text = line.trim_start_matches('›').trim();
    text.is_empty() || crate::agent_tui::CODEX_PLACEHOLDERS.contains(&text)
}

/// A known Codex placeholder cut at a word boundary by a narrow pane
/// (`› Ask Codex to`). Only for a worker launched a moment ago, which
/// nobody has typed into; account-swap draft capture keeps such text.
fn cut_placeholder(text: &str) -> bool {
    let text = text.trim_end_matches('…');
    text.chars().count() >= 8
        && crate::agent_tui::CODEX_PLACEHOLDERS
            .iter()
            .any(|placeholder| {
                placeholder
                    .strip_prefix(text)
                    .is_some_and(|rest| rest.starts_with(' '))
            })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_tui::{DOWN, ENTER};
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
        let screen = "Claude Code v2.1.322\n─────────────────\n❯ \u{a0}\n─────────────────\n ~/p/tend Opus 5.5 0 0\n ⎇ main 453ms $0.00";
        assert_eq!(
            run(|| Some(screen.into()), || true, |_| panic!()).state,
            State::Ready
        );
        for unproven in ["Claude Code\n❯ ", "Claude Code\n─────\nLoading\n─────"]
        {
            assert_eq!(
                run(|| Some(unproven.into()), || true, |_| panic!()).state,
                State::Unsettled
            );
        }
        for suffix in [
            "\n$ claude --resume old",
            "\n❯ exec claude",
            "\nuser@host % claude --session-id new",
            "\nline1\nline2\nline3\nline4",
        ] {
            assert_eq!(
                run(|| Some(format!("{screen}{suffix}")), || true, |_| panic!()).state,
                State::Unsettled
            );
        }
        // The shared account-swap probe must retain its old policy: a dead
        // composer's frame can remain above a fresh relaunch.
        assert!(!crate::agent_tui::agent_prompt_visible(screen));
        let without_header = screen.lines().skip(1).collect::<Vec<_>>().join("\n");
        assert_eq!(
            run(|| Some(without_header.clone()), || true, |_| panic!()).state,
            State::Ready
        );
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
    fn codex_hook_review_is_a_named_blocker_and_never_answered() {
        let screen = "  Hooks need review\n  6 hooks are new or changed.\n  Hooks can run outside the\n  sandbox after you trust\n  them.\n› 1. Review hooks\n  2. Trust all and continue\n  3. Continue without trusting\n     (hooks won't run)\n  enter confirm · esc skip";
        let clock = FakeClock {
            base: Instant::now(),
            elapsed: Cell::new(Duration::ZERO),
        };
        let result = prepare_with(
            &clock,
            AgentKind::Codex,
            || Some(screen.into()),
            || true,
            |_| panic!("must not answer a hook review"),
            true,
            Duration::from_secs(1),
            Duration::from_millis(100),
            Duration::from_millis(25),
        );
        assert_eq!(result.state, State::Blocked);
        assert!(
            result
                .reason
                .as_deref()
                .unwrap()
                .contains("review new or changed hooks"),
            "{result:?}"
        );
        assert!(result.answered.is_empty());
    }

    #[test]
    fn a_narrow_codex_placeholder_is_not_a_draft_for_a_fresh_worker_only() {
        assert!(cut_placeholder("Ask Codex to"));
        assert!(cut_placeholder("Find and fix…"));
        assert!(!cut_placeholder("Ask Codex to fix it"));
        assert!(!cut_placeholder("Ask"));
        // Account-swap draft capture keeps such text: it may be typed.
        assert_eq!(
            crate::agent_tui::composer_draft(
                "› Find and fix\n  100% context left",
                AgentKind::Codex
            )
            .as_deref(),
            Some("Find and fix")
        );
        let clock = FakeClock {
            base: Instant::now(),
            elapsed: Cell::new(Duration::ZERO),
        };
        let result = prepare_with(
            &clock,
            AgentKind::Codex,
            || Some("  permissions: YOLO mode\n› Ask Codex to\n  G…  ⚠ 1 · f2".into()),
            || true,
            |_| true,
            false,
            Duration::from_secs(1),
            Duration::from_millis(100),
            Duration::from_millis(25),
        );
        assert_eq!(result.state, State::Ready, "{result:?}");
    }

    #[test]
    fn a_wrapped_hook_menu_and_older_footer_are_still_the_menu() {
        let wrapped = "  Hooks need review\n  6 hooks are new or changed.\n› 1. Review hooks\n  2. Trust all and\n     continue\n  3. Continue without\n     trusting (hooks won't run)\n  enter confirm ·\n  esc skip";
        assert!(codex_hook_review(wrapped));
        let older = "  Hooks need review\n› 1. Review hooks\n  2. Trust all and continue\n  3. Continue without trusting\n  Press enter to confirm or esc to go back";
        assert!(codex_hook_review(older));
        let unselected = "  Hooks need review\n  1. Review hooks\n  2. Trust all and continue\n  enter confirm · esc skip";
        assert!(!codex_hook_review(unselected));
    }

    #[test]
    fn a_transcript_mentioning_the_hook_review_is_not_the_menu() {
        let idle = "• The startup dialog says Hooks need review. Choose Review hooks to inspect it.\n› Ask Codex to do anything\n  100% context left";
        assert!(!codex_hook_review(idle));
        let quoted = "  Hooks need review\n  1. Review hooks\n  2. Trust all and continue\n› Ask Codex to do anything\n  100% context left";
        assert!(!codex_hook_review(quoted));
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
