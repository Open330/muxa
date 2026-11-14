//! Reading and answering a provider TUI through its pane: whether a turn is
//! running, which dialog is open, what was half-typed, and waiting for a
//! relaunched agent to settle while answering only the startup dialogs that
//! need no consent from the user.
//!
//! **Internal:** `#[doc(hidden)]`. This is the screen half shared by worker
//! startup (core) and the optional account module's swap executor. Every
//! check here reads the screen, so none of it alone may trigger a
//! destructive act (see `docs/DIRECTION.md`, "Deterministic first").

use crate::event::AgentKind;
use std::time::{Duration, Instant};

/// The screen must hold still this long to count as drawn.
pub const READY_QUIET: Duration = Duration::from_millis(1500);

/// Time, so a fake can advance it instead of waiting.
pub trait Clock {
    fn now(&self) -> Instant;
    fn sleep(&self, duration: Duration);
}

/// The wall clock, for tests that drive a probe in real time.
pub struct RealClock;

impl Clock for RealClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Does the bottom of `screen` show a turn in progress? Both TUIs print an
/// interrupt hint while working; Claude's spinner line also counts up
/// (`✻ Thinking… (12s · ↓ 300 tokens)`).
pub fn turn_running(screen: &str) -> bool {
    let tail = screen_tail(screen, 12);
    tail.contains("esc to interrupt")
        || tail.lines().any(|line| {
            // Claude's spinner: `… (12s ·`, `… (1m 3s ·`.
            line.split_once("… (").is_some_and(|(_, rest)| {
                let digits = rest.chars().take_while(char::is_ascii_digit).count();
                digits > 0 && matches!(rest[digits..].chars().next(), Some('s' | 'm'))
            })
        })
}

/// Placeholder text an empty Codex composer shows.
pub const CODEX_PLACEHOLDERS: &[&str] = &[
    "Ask Codex to do anything",
    "Find and fix a bug",
    "Explain this codebase",
    "Summarize recent commits",
    "Improve documentation",
    "Write tests",
    "Implement {feature}",
    "Run /review",
    "Use /skills",
];

/// What was half-typed into the agent's prompt, if anything. Claude's prompt
/// sits between the last two horizontal rules, after `❯`; Codex's is the
/// last `› ` line that is not its placeholder or a menu row.
pub fn composer_draft(screen: &str, kind: AgentKind) -> Option<String> {
    let lines: Vec<&str> = screen.lines().collect();
    let text = match kind {
        AgentKind::ClaudeCode => {
            let rules: Vec<usize> = lines
                .iter()
                .enumerate()
                .filter(|(_, line)| {
                    let line = line.trim();
                    line.chars().count() >= 5 && line.chars().all(|c| c == '─')
                })
                .map(|(i, _)| i)
                .collect();
            let [.., top, bottom] = rules[..] else {
                return None;
            };
            lines[top + 1..bottom]
                .iter()
                .map(|line| line.trim())
                .collect::<Vec<_>>()
                .join(" ")
                .trim_start_matches('❯')
                .trim()
                .to_owned()
        }
        AgentKind::Codex => {
            let line = lines.iter().rev().find(|line| line.starts_with("› "))?;
            let text = line["› ".len()..].trim().to_owned();
            // A menu row (`› 1. Trust and continue`) sits above key hints.
            let menu_row = open_question(screen).is_some()
                && text
                    .split_once(". ")
                    .is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
            if menu_row || CODEX_PLACEHOLDERS.contains(&text.as_str()) {
                return None;
            }
            text
        }
        _ => return None,
    };
    (!text.is_empty()).then_some(text)
}

/// What a resumed pane turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resume {
    /// Settled on a screen with no ownership or startup error, agent alive.
    Ready,
    /// Settled on an ownership or startup-error screen.
    Blocked(String),
    /// The agent process went away before the screen was usable.
    Exited,
    /// Never settled within the timeout.
    Unsettled,
}

/// Wait for the resumed TUI to stop redrawing, then accept it only if the
/// agent is still running and the screen is not an ownership or startup
/// error. A stable screen alone proves nothing: "This conversation is open
/// in another app" is stable too (Open330/tend#275).
#[cfg(test)]
pub fn wait_for_resume(
    capture: impl FnMut() -> Option<String>,
    alive: impl FnMut() -> bool,
    timeout: Duration,
    quiet: Duration,
    tick: Duration,
) -> Resume {
    wait_for_resume_answering(
        &RealClock,
        capture,
        alive,
        |_| false,
        &|_| false,
        timeout,
        quiet,
        tick,
        &mut Vec::new(),
    )
}

/// [`wait_for_resume`], answering the startup dialogs in
/// [`STARTUP_DIALOGS`] through `answer` (which types keys into the pane and
/// reports whether it could) and recording each answer in `answered`. A
/// settled screen that still asks a question is never `Ready`: a follow-up
/// typed into Claude's folder-trust prompt picks its default, "No, exit".
#[allow(clippy::too_many_arguments)] // the probes and timings tests drive one by one
pub fn wait_for_resume_answering(
    clock: &dyn Clock,
    capture: impl FnMut() -> Option<String>,
    alive: impl FnMut() -> bool,
    answer: impl FnMut(&[&str]) -> bool,
    granted: &dyn Fn(Consent) -> bool,
    timeout: Duration,
    quiet: Duration,
    tick: Duration,
    answered: &mut Vec<&'static str>,
) -> Resume {
    wait_for_resume_answering_with_probe(
        clock,
        capture,
        alive,
        answer,
        granted,
        timeout,
        quiet,
        tick,
        answered,
        &agent_prompt_visible,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn wait_for_resume_answering_with_probe(
    clock: &dyn Clock,
    mut capture: impl FnMut() -> Option<String>,
    mut alive: impl FnMut() -> bool,
    mut answer: impl FnMut(&[&str]) -> bool,
    granted: &dyn Fn(Consent) -> bool,
    timeout: Duration,
    quiet: Duration,
    tick: Duration,
    answered: &mut Vec<&'static str>,
    prompt_visible: &dyn Fn(&str) -> bool,
) -> Resume {
    let started = clock.now();
    let mut last = String::new();
    let mut quiet_since = clock.now();
    loop {
        let screen = capture().unwrap_or_default();
        if screen != last {
            last = screen;
            quiet_since = clock.now();
        } else if !last.trim().is_empty()
            && clock.now().saturating_duration_since(quiet_since) >= quiet
        {
            if answered_dialog_lingers(&last, answered)
                && open_question(&last).is_some()
                && clock.now().saturating_duration_since(started) < timeout
            {
                // Answered already; the agent has not redrawn yet.
                quiet_since = clock.now();
                clock.sleep(tick);
                continue;
            }
            if let Some(dialog) = pending_dialog(&last, STARTUP_DIALOGS, answered) {
                // Agreeing for the user needs evidence; a dialog drawn by an
                // agent that is gone is a stale screen.
                if dialog.consent != Consent::None && !granted(dialog.consent) {
                    return Resume::Blocked(format!(
                        "a startup dialog needs your answer ({}); nothing was typed",
                        dialog.label
                    ));
                }
                if !alive() {
                    return Resume::Exited;
                }
                if dialog.keys_on(&last).is_some_and(|keys| answer(&keys)) {
                    answered.push(dialog.label);
                    // The answer redraws the screen; wait for it to settle again.
                    last.clear();
                    quiet_since = clock.now();
                    // Bounded by the overall timeout so retries cannot outlast it.
                    clock.sleep(dialog.pause.min(timeout / 8));
                    continue;
                }
                return Resume::Blocked(format!("a startup dialog is waiting ({})", dialog.label));
            }
            if let Some(reason) = blocked_screen(&last) {
                return Resume::Blocked(reason.to_owned());
            }
            if let Some(line) = open_question(&last) {
                return Resume::Blocked(format!("a dialog is waiting for an answer ({line})"));
            }
            if !alive() {
                return Resume::Exited;
            }
            // Quiet is not ready: a wrapper or the agent itself can sit on
            // the shell's echo of the relaunch for seconds before the TUI
            // (or a trust dialog) draws. Wait for the agent's own prompt.
            if !prompt_visible(&last) {
                if clock.now().saturating_duration_since(started) >= timeout {
                    return Resume::Unsettled;
                }
                quiet_since = clock.now();
                clock.sleep(tick);
                continue;
            }
            return Resume::Ready;
        }
        if clock.now().saturating_duration_since(started) >= timeout {
            // A blocker can sit on a screen that keeps redrawing (a clock,
            // a spinner): report it rather than a bare timeout.
            if let Some(reason) = blocked_screen(&last) {
                return Resume::Blocked(reason.to_owned());
            }
            if let Some(line) = open_question(&last) {
                return Resume::Blocked(format!("a dialog is waiting for an answer ({line})"));
            }
            return if alive() {
                Resume::Unsettled
            } else {
                Resume::Exited
            };
        }
        clock.sleep(tick);
    }
}

/// Marks of an agent waiting at its own prompt, on the last lines of the
/// screen: Claude's mode and shortcut line, Codex's `›` composer. Claude's
/// `❯` is left out: shell prompts use it too.
pub const PROMPT_MARKS: &[&str] = &[
    "⏵⏵",
    "for shortcuts",
    "bypass permissions",
    "auto mode on",
    "accept edits on",
    "plan mode on",
    "Ask Codex",
];

/// Is the resumed agent's prompt on screen (not the shell's, not a dialog)?
pub fn agent_prompt_visible(screen: &str) -> bool {
    let tail = screen_tail(screen, 10);
    PROMPT_MARKS.iter().any(|mark| tail.contains(mark))
        || tail.lines().any(|line| line.starts_with("› "))
}

/// How many non-blank lines at the bottom of the screen a startup banner or
/// error can occupy. Only these are read, so a resumed conversation's own
/// history (which may quote the same words) cannot trip the check.
pub const BLOCKER_TAIL_LINES: usize = 8;

/// Line openings that mean the resumed agent cannot take input, with the
/// note shown to the operator. Matched case-insensitively at the start of a
/// line, after any leading box-drawing, bullet or `error:` decoration.
pub const BLOCKERS: &[(&str, &str)] = &[
    (
        "this conversation is open in another app",
        "another app still owns this conversation (\"This conversation is open in another app\")",
    ),
    (
        "conversation is open in another",
        "another app still owns this conversation",
    ),
    (
        "no conversation found",
        "the agent could not find the conversation to resume",
    ),
    (
        "no session found",
        "the agent could not find the session to resume",
    ),
    (
        "session not found",
        "the agent could not find the session to resume",
    ),
    (
        "failed to resume",
        "the agent failed to resume the conversation",
    ),
    (
        "unable to resume",
        "the agent failed to resume the conversation",
    ),
    (
        "too many open files",
        "the agent hit the open-file limit while starting",
    ),
    (
        "unexpected argument",
        "the agent rejected the relaunch arguments",
    ),
    (
        "command not found",
        "the relaunch command was not found in the pane's shell",
    ),
];

/// The reason a settled screen cannot take input, if it shows one.
pub fn blocked_screen(screen: &str) -> Option<&'static str> {
    let lines: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = &lines[lines.len().saturating_sub(BLOCKER_TAIL_LINES)..];
    tail.iter().find_map(|line| {
        let text = line.trim_start_matches(|c: char| {
            c.is_whitespace()
                || matches!(
                    c,
                    '│' | '┃'
                        | '║'
                        | '╭'
                        | '╰'
                        | '■'
                        | '▌'
                        | '⚠'
                        | '🔒'
                        | '✗'
                        | '×'
                        | '•'
                        | '›'
                        | '>'
                        | '!'
                        | '-'
                        | '*'
                )
        });
        // Work on the lowercased copy only: its byte offsets need not match
        // the original's once non-ASCII text is lowercased.
        let mut text = text.to_lowercase();
        // `zsh: command not found: aas` — the shell names itself first.
        for prefixes in [
            &["zsh:", "bash:", "sh:", "fish:"][..],
            &["error:", "warning:", "fatal:", "error"][..],
        ] {
            if let Some(rest) = prefixes.iter().find_map(|p| text.strip_prefix(p)) {
                text = rest.trim_start().to_owned();
            }
        }
        BLOCKERS
            .iter()
            .find(|(opening, _)| text.starts_with(opening))
            .map(|(_, reason)| *reason)
    })
}

/// A dialog a resumed or exiting agent can stop on, and the keys that answer
/// it the way a swap needs.
pub struct Dialog {
    /// Every one of these must be on the screen's last
    /// [`DIALOG_TAIL_LINES`] non-blank lines.
    pub needles: &'static [&'static str],
    /// Keys typed to answer it, when [`Dialog::pick`] cannot place the
    /// cursor (or there is no row to pick). Empty: refuse instead.
    pub keys: &'static [&'static str],
    /// The row to choose, reached from wherever the dialog's `❯` cursor
    /// sits: Claude orders and focuses some confirm rows case by case.
    pub pick: Option<&'static str>,
    /// For a dialog answered with fixed keys: a row its own menu must show,
    /// or a word its own last lines must show. With neither (and no
    /// `pick`), tend cannot tell it is the dialog on screen now, and it is
    /// never answered.
    pub row: Option<&'static str>,
    pub footer: Option<&'static str>,
    /// Named in the report.
    pub label: &'static str,
    /// How many times one swap may answer it.
    pub max: usize,
    /// Time for the agent to act on the answer before the screen is read again.
    pub pause: Duration,
    /// What answering it agrees to on the user's behalf, if anything.
    pub consent: Consent,
}

/// A startup answer that agrees to something for the user. Only given with
/// evidence the agent already ran that way; otherwise the swap stops at the
/// dialog and types nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consent {
    /// Nothing is agreed to (a tour dismissed, an update skipped).
    None,
    /// Bypass-permissions mode: only when the stopped agent ran with it and
    /// the relaunch keeps it.
    Bypass,
    /// Trusting a folder's settings (tools, hooks): never on the user's
    /// behalf.
    Settings,
}

/// Down arrow and Enter, as typed into a pane. A dialog's keys go one at a
/// time: Claude's input layer reads `ESC [ B CR` arriving in one write as
/// a single unknown sequence, not as Down then Enter.
pub const DOWN: &str = "\x1b[B";
pub const UP: &str = "\x1b[A";
pub const ENTER: &str = "\r";
pub const ESC: &str = "\x1b";
/// Pause between the keys of one answer.
pub const KEY_GAP: Duration = Duration::from_millis(300);

impl Dialog {
    /// The keys that answer this dialog as `screen` draws it, or `None` when
    /// tend cannot tell which row a key would pick.
    pub fn keys_on(&self, screen: &str) -> Option<Vec<&'static str>> {
        let Some(row) = self.pick else {
            return (!self.keys.is_empty()).then(|| self.keys.to_vec());
        };
        // Only the current menu's own rows: a row's text further up (a
        // quoted warning, an earlier dialog) is not one of its options.
        let (rows, cursor) = option_rows(screen)?;
        let target = rows.iter().position(|line| names(line, row))?;
        let step = if target > cursor { DOWN } else { UP };
        let mut keys = vec![step; target.abs_diff(cursor)];
        keys.push(ENTER);
        Some(keys)
    }
}

/// Claude's switch for bypass-permissions mode.
pub const BYPASS_SWITCH: &str = "--dangerously-skip-permissions";

/// Whether a Claude process's own arguments (lossless, as the process holds
/// them) carry `switch` as an option of its own: not inside a prompt, not
/// as another option's value, not after `--`. Unreadable is no.
pub fn claude_switch(argv: Option<&[String]>, switch: &str) -> bool {
    let Some(argv) = argv else {
        return false;
    };
    let Some(program) = argv
        .iter()
        .take(2)
        .position(|word| crate::discovery::classify_command(word) == Some(AgentKind::ClaudeCode))
    else {
        return false;
    };
    let mut words = argv[program + 1..].iter().map(String::as_str);
    while let Some(word) = words.next() {
        if word == "--" {
            return false;
        }
        if word == switch {
            return true;
        }
        if CLAUDE_VALUE_OPTIONS.contains(&word) {
            words.next();
        }
    }
    false
}

/// Whether a menu row is `label`: exactly, or followed by its description
/// (`Keep worktree    Stays at …`, `Use session directory (/x)`).
pub fn names(row: &str, label: &str) -> bool {
    row == label
        || row
            .strip_prefix(label)
            .is_some_and(|rest| rest.starts_with(' '))
}

/// How many lines above a dialog's footer can be its option rows.
pub const OPTION_ROWS: usize = 6;

/// The current dialog's options, and which one its cursor is on: the
/// contiguous rows around its one cursor (`❯`/`›`) that start at the cursor
/// row's text column, ending directly above the dialog's own footer.
/// Explanatory text, a quoted earlier dialog, or a second cursor make it
/// `None`: tend cannot tell which menu a key would move in.
pub fn option_rows(screen: &str) -> Option<(Vec<String>, usize)> {
    if !dialog_is_active(screen) {
        return None;
    }
    let lines: Vec<&str> = screen
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect();
    let footer = lines.iter().rposition(|line| {
        let line = line.to_lowercase();
        QUESTION_FOOTERS.iter().any(|footer| line.contains(footer))
    })?;
    let window = &lines[footer.saturating_sub(OPTION_ROWS)..footer];
    let is_cursor = |line: &str| line.trim_start().starts_with(['❯', '›']);
    let mut cursors = (0..window.len()).filter(|&i| is_cursor(window[i]));
    let cursor = cursors.next()?;
    if cursors.next().is_some() {
        return None;
    }
    let indent = |line: &str| line.chars().take_while(|c| *c == ' ').count();
    let marked = window[cursor];
    let after_marker = marked
        .trim_start()
        .chars()
        .skip(1)
        .take_while(|c| *c == ' ')
        .count();
    let column = indent(marked) + 1 + after_marker;
    let is_row = |line: &str| !is_cursor(line) && indent(line) == column;
    let mut first = cursor;
    while first > 0 && is_row(window[first - 1]) {
        first -= 1;
    }
    let mut last = cursor;
    while last + 1 < window.len() && is_row(window[last + 1]) {
        last += 1;
    }
    // The menu runs down to the footer; anything after it is not a menu.
    if last + 1 != window.len() || first == last {
        return None;
    }
    let text = |line: &str| {
        line.trim_start()
            .trim_start_matches(['❯', '›'])
            .trim_start()
            .trim_start_matches(|c: char| c.is_ascii_digit() || c == '.')
            .trim_start()
            .to_owned()
    };
    Some((
        window[first..=last].iter().map(|line| text(line)).collect(),
        cursor - first,
    ))
}

/// Whether the screen ends in a dialog still waiting for an answer, and not
/// in the agent's own prompt (whose mode line can quote a dialog's words).
pub fn dialog_is_active(screen: &str) -> bool {
    open_question(screen).is_some()
        && !PROMPT_MARKS
            .iter()
            .any(|mark| screen_tail(screen, 2).contains(mark))
}

/// Type `keys` one by one, [`KEY_GAP`] apart. `false` if any could not be.
pub fn type_keys(clock: &dyn Clock, mut send: impl FnMut(&str) -> bool, keys: &[&str]) -> bool {
    keys.iter().enumerate().all(|(i, key)| {
        if i > 0 {
            clock.sleep(KEY_GAP);
        }
        send(key)
    })
}

/// How many non-blank lines at the bottom of the screen a dialog occupies.
pub const DIALOG_TAIL_LINES: usize = 16;

/// Dialogs a relaunched agent shows before its conversation takes input.
/// The account changed, not the folder or the conversation, so each is
/// answered the way the agent was already running.
pub const STARTUP_DIALOGS: &[Dialog] = &[
    // Claude, on a profile that has not seen this folder. The default row is
    // "No, exit".
    Dialog {
        needles: &["Yes, I trust this folder", "Enter to confirm"],
        keys: &[DOWN, ENTER],
        pick: Some("Yes, I trust this folder"),
        row: None,
        footer: None,
        label: "trusted the folder",
        max: 1,
        pause: Duration::from_millis(500),
        consent: Consent::None,
    },
    // Claude, when the folder's settings pre-approve tools or hooks. Its row
    // order and focus vary, so the cursor decides the keys.
    Dialog {
        needles: &["Yes, I trust these settings", "Enter to confirm"],
        keys: &[],
        pick: Some("Yes, I trust these settings"),
        row: None,
        footer: None,
        label: "trust the folder's settings",
        max: 1,
        pause: Duration::from_millis(500),
        consent: Consent::Settings,
    },
    // Claude, relaunched with --dangerously-skip-permissions on a profile
    // that has not accepted it yet; the old process already ran that way.
    Dialog {
        needles: &["Yes, I accept", "Bypass Permissions mode"],
        keys: &[DOWN, ENTER],
        pick: Some("Yes, I accept"),
        row: None,
        footer: None,
        label: "accepted bypass-permissions mode",
        max: 1,
        pause: Duration::from_millis(500),
        consent: Consent::Bypass,
    },
    Dialog {
        needles: &["Teach auto mode", "Enter to confirm"],
        keys: &[],
        pick: None,
        row: None,
        footer: None,
        label: "dismissed the auto-mode tour",
        max: 1,
        pause: Duration::from_millis(500),
        consent: Consent::None,
    },
    // Codex, when a newer release is out: "1. Update now" is the default
    // row; Esc skips without installing anything. A swap relaunch never
    // meets it (it starts with `-c check_for_update_on_startup=false`); a
    // worker launch still can, and an older relaunch line read back from a
    // retried job too, so the entry stays as their fallback.
    Dialog {
        needles: &["Update available", "Skip until next version"],
        keys: &[ESC],
        pick: None,
        row: Some("Update now"),
        footer: None,
        label: "skipped the Codex update prompt",
        // Once: a second Esc on Codex's composer opens its backtrack view.
        max: 1,
        pause: Duration::from_millis(800),
        consent: Consent::None,
    },
    // Codex: "1. Trust and continue" is the default row.
    Dialog {
        needles: &["Trust this folder", "Trust and continue"],
        keys: &[ENTER],
        pick: Some("Trust and continue"),
        row: None,
        footer: None,
        label: "trusted the folder",
        max: 1,
        pause: Duration::from_millis(500),
        consent: Consent::None,
    },
    // Codex's "Working directory · resume" menu is not here: a swap
    // relaunch sets `tui.resume_cwd="session"` and never meets it, and a
    // worker launch does not answer it. Met anyway, it is an open question
    // and the swap stops there without typing.
    Dialog {
        needles: &["1. Resume goal", "enter select"],
        keys: &[ENTER],
        pick: Some("Resume goal"),
        row: None,
        footer: None,
        label: "resumed the paused goal",
        max: 1,
        pause: Duration::from_millis(500),
        consent: Consent::None,
    },
    // Codex: the old process's app-server can still hold the thread for a
    // few seconds after it exits.
    Dialog {
        needles: &["open in another app", "r to retry"],
        keys: &["r"],
        pick: None,
        row: None,
        footer: Some("f fork"),
        label: "retried a conversation another app still held",
        max: 5,
        pause: Duration::from_secs(3),
        consent: Consent::None,
    },
];

/// A startup dialog a freshly launched or restored agent is sitting on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupBlocker {
    /// What it is, for a report (`skipped the Codex update prompt`, …).
    pub label: &'static str,
    /// The keys that answer it, when it needs no consent from the user and
    /// tend can tell which row a key picks. Type them one by one with
    /// [`type_keys`], once. `None`: never typed; report the pane as
    /// waiting on this dialog instead.
    pub keys: Option<Vec<&'static str>>,
    /// What answering would agree to for the user.
    pub consent: Consent,
}

/// Startup dialogs any caller may answer: dismissing them installs,
/// trusts and resumes nothing.
pub const SAFE_DISMISSALS: &[&str] = &["skipped the Codex update prompt"];

/// The startup dialog `screen` shows, if any, so a restore or launch never
/// leaves a pane silently stuck on one. Built on [`STARTUP_DIALOGS`]; a
/// dialog tend must not answer (folder-settings trust, Codex's "Hooks need
/// review", bypass acceptance without evidence) comes back without keys.
/// Codex's update menu answers with Esc ("skip"), which installs nothing.
#[must_use]
pub fn startup_blocker(screen: &str) -> Option<StartupBlocker> {
    if crate::worker_startup::codex_hook_review(screen) {
        return Some(StartupBlocker {
            label: "Codex hooks need review",
            keys: None,
            consent: Consent::Settings,
        });
    }
    let Some(dialog) = pending_dialog(screen, STARTUP_DIALOGS, &[]) else {
        // A question tend has no entry for: never typed into, but never
        // left silent either.
        return open_question(screen)
            .filter(|_| dialog_is_active(screen))
            .map(|_| StartupBlocker {
                label: "an unrecognised startup dialog",
                keys: None,
                consent: Consent::None,
            });
    };
    // Only a dismissal that agrees to nothing and starts no work: the update
    // menu's Esc ("skip"). Folder trust, a paused goal's "Resume goal" and
    // the rest need evidence only a swap or a worker start has, so a
    // generic caller reports them instead.
    let keys = (dialog.consent == Consent::None && SAFE_DISMISSALS.contains(&dialog.label))
        .then(|| dialog.keys_on(screen))
        .flatten();
    Some(StartupBlocker {
        label: dialog.label,
        keys,
        consent: dialog.consent,
    })
}

/// What `/exit` may ask before Claude quits, and what a swap answers.
pub const EXIT_DIALOGS: &[Dialog] = &[
    // Background shells or agents: keep them running past the exit.
    Dialog {
        needles: &["Move to background and exit", "Enter to confirm"],
        keys: &[DOWN, ENTER],
        pick: Some("Move to background and exit"),
        row: None,
        footer: None,
        label: "moved background work out of the way",
        max: 1,
        pause: Duration::from_millis(500),
        consent: Consent::None,
    },
    Dialog {
        needles: &["Keep worktree", "Enter to confirm"],
        keys: &[ENTER],
        pick: Some("Keep worktree"),
        row: None,
        footer: None,
        label: "kept the worktree",
        max: 1,
        pause: Duration::from_millis(500),
        consent: Consent::None,
    },
];

/// The first dialog of `table` on `screen` that this swap may still answer.
pub fn pending_dialog<'a>(
    screen: &str,
    table: &'a [Dialog],
    answered: &[&'static str],
) -> Option<&'a Dialog> {
    if !dialog_is_active(screen) {
        return None;
    }
    let tail = screen_tail(screen, DIALOG_TAIL_LINES);
    let options = option_rows(screen)
        .map(|(rows, _)| rows)
        .unwrap_or_default();
    let offers = |dialog: &Dialog| {
        dialog
            .pick
            .is_some_and(|row| options.iter().any(|o| names(o, row)))
    };
    // Which dialog this is, shown by the dialog itself: its row in the
    // current menu, or its own words on its last lines. Words further up
    // (an earlier dialog, the conversation) prove nothing.
    let current = |dialog: &Dialog| match (dialog.pick, dialog.row, dialog.footer) {
        (Some(_), _, _) => offers(dialog),
        (None, Some(row), _) => options.iter().any(|o| names(o, row)),
        (None, None, Some(word)) => screen_tail(screen, 2).contains(word),
        // Nothing to tell it by: matched only to be named, never answered.
        (None, None, None) => true,
    };
    // A menu whose rows answer two dialogs (a folder trust and a settings
    // trust) is not one tend can tell apart: none.
    if table.iter().filter(|dialog| offers(dialog)).count() > 1 {
        return None;
    }
    table.iter().find(|dialog| {
        dialog.needles.iter().all(|needle| tail.contains(needle))
            && current(dialog)
            && answered
                .iter()
                .filter(|label| **label == dialog.label)
                .count()
                < dialog.max
    })
}

/// Is `screen` still showing a startup dialog this swap has already answered
/// as often as it may, other than a lock it gave up retrying? Paired with
/// [`open_question`]: output below an answered dialog means it is gone.
pub fn answered_dialog_lingers(screen: &str, answered: &[&'static str]) -> bool {
    let tail = screen_tail(screen, DIALOG_TAIL_LINES);
    STARTUP_DIALOGS.iter().any(|dialog| {
        dialog.max == 1
            && answered.contains(&dialog.label)
            && dialog.needles.iter().all(|needle| tail.contains(needle))
    })
}

/// The last `lines` non-blank lines of `screen`, joined.
pub fn screen_tail(screen: &str, lines: usize) -> String {
    let rows: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    rows[rows.len().saturating_sub(lines)..].join("\n")
}

/// Footers of a selection dialog, read case-insensitively on the last few
/// lines. One means the agent waits for a choice tend did not make.
pub const QUESTION_FOOTERS: &[&str] = &[
    "enter to confirm",
    "esc to cancel",
    "enter select",
    "enter continue",
    // Older Codex releases' menus (the update menu and folder trust of
    // 0.154): `Press enter to continue`.
    "press enter to continue",
    "r to retry",
    // Codex's lock screen: `r retry   f fork   esc/⌃c/q exit`.
    "r retry",
];

/// The footer line of a dialog still waiting for an answer, if any. A
/// dialog's key hints are its last line (two when a narrow pane wraps
/// them); once answered, output below them means it is gone.
pub fn open_question(screen: &str) -> Option<String> {
    let tail = screen_tail(screen, 2);
    tail.lines()
        .find(|line| {
            let line = line.to_lowercase();
            QUESTION_FOOTERS.iter().any(|footer| line.contains(footer))
        })
        .map(|line| line.trim().chars().take(80).collect())
}

/// Claude options that take one value, whose value is never an option even
/// if it reads like one.
pub const CLAUDE_VALUE_OPTIONS: &[&str] = &[
    "--agent",
    "--agents",
    "--append-system-prompt",
    "--append-system-prompt-file",
    "--autocompact",
    "--debug-file",
    "--effort",
    "--environment",
    "--fallback-model",
    "--input-format",
    "--json-schema",
    "--max-budget-usd",
    "--max-turns",
    "--model",
    "-n",
    "--name",
    "--output-format",
    "--permission-mode",
    "--permission-prompt-tool",
    "--permission-prompts",
    "--plugin-dir",
    "--plugin-url",
    "--remote-control-session-name-prefix",
    "--session-id",
    "--setting-sources",
    "--settings",
    "--system-prompt",
    "--system-prompt-file",
];

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Open330/tend#275: a running, settled TUI is not a resumed one ----

    const OWNED_ELSEWHERE: &str =
        "\n\n  ■ This conversation is open in another app.\n    Close it there to continue here.\n";
    const RESUMED: &str = "› Plan rate limiting\n\n• Edited src/limit.ts\n\n› Ask Codex to do anything\n  ⏎ send   ⌃J newline\n";

    fn quick(screens: Vec<&'static str>, alive: bool) -> Resume {
        let mut screens = screens.into_iter().map(str::to_owned);
        let mut last = String::new();
        wait_for_resume(
            || {
                if let Some(next) = screens.next() {
                    last = next;
                }
                Some(last.clone())
            },
            || alive,
            Duration::from_millis(300),
            Duration::from_millis(20),
            Duration::from_millis(5),
        )
    }

    #[test]
    fn a_stable_ownership_banner_is_blocked_not_ready() {
        assert!(
            matches!(quick(vec![OWNED_ELSEWHERE], true), Resume::Blocked(r) if r.contains("another app"))
        );
    }

    #[test]
    fn startup_errors_are_blocked() {
        for screen in [
            "Error: No conversation found with session ID 019a\n$ ",
            "error: unexpected argument '--resume' found\n",
            "⚠ Too many open files (os error 24)\n",
            "zsh: command not found: aas\n$ ",
        ] {
            assert!(blocked_screen(screen).is_some(), "{screen:?}");
        }
    }

    #[test]
    fn a_settled_resumed_conversation_with_a_live_agent_is_ready() {
        assert_eq!(quick(vec!["", "loading", RESUMED], true), Resume::Ready);
    }

    #[test]
    fn a_settled_screen_whose_agent_exited_is_not_ready() {
        assert_eq!(quick(vec![RESUMED], false), Resume::Exited);
    }

    #[test]
    fn history_that_quotes_a_blocker_does_not_trip_the_check() {
        let mut screen = String::from(
            "› why did the swap fail?\n• It said: This conversation is open in another app.\n",
        );
        screen.push_str(&"• more history\n".repeat(BLOCKER_TAIL_LINES));
        screen.push_str(RESUMED);
        assert_eq!(blocked_screen(&screen), None);
        // Mid-line mentions are not banners either.
        assert_eq!(
            blocked_screen("• the log said no conversation found yesterday\n› Ask Codex\n"),
            None
        );
    }

    // ---- startup dialogs a relaunch must answer, never type a follow-up into ----

    /// Claude on a profile that has not seen the folder (captured live).
    const CLAUDE_TRUST: &str = " Accessing workspace:\n /Users/june/personal/barshelf\n \
        Quick safety check: Is this a project you created or one you trust?\n \
        ❯ No, exit\n   Yes, I trust this folder\n Enter to confirm · Esc to cancel\n";
    const CLAUDE_READY: &str = "⏺ Resumed.\n────────\n❯ \n────────\n  \
        ⏵⏵ bypass permissions on (shift+tab to cycle)\n";
    const CODEX_LOCKED: &str =
        "  🔒  This conversation is open in another app        r to retry\n      \
        Close it there and press R to continue here.\n   r retry   f fork   esc/⌃c/q exit\n";

    /// Drive `wait_for_resume_answering` over a screen that becomes `after`
    /// once it has been answered `flips_after` times.
    fn answering(
        before: &'static str,
        after: &'static str,
        flips_after: usize,
        can_answer: bool,
    ) -> (Resume, Vec<&'static str>, Vec<String>) {
        let keys = std::cell::RefCell::new(Vec::<String>::new());
        let mut answered = Vec::new();
        let resume = wait_for_resume_answering(
            &RealClock,
            || {
                let n = keys.borrow().len();
                Some(if n >= flips_after { after } else { before }.to_owned())
            },
            || true,
            |k| {
                if can_answer {
                    keys.borrow_mut().push(k.concat());
                }
                can_answer
            },
            &|consent| consent == Consent::Bypass,
            Duration::from_secs(10),
            Duration::from_millis(20),
            Duration::from_millis(5),
            &mut answered,
        );
        (resume, answered, keys.into_inner())
    }

    #[test]
    fn a_coloured_trust_prompt_is_still_recognised() {
        // Claude colours each footer word apart; `capture-pane -e` keeps it.
        let raw = " ❯ No, exit\n   Yes, I trust this folder\n \
            \x1b[38;5;246mEnter\x1b[39m \x1b[38;5;246mto\x1b[39m \x1b[38;5;246mconfirm\x1b[39m \
            \x1b[38;5;246m·\x1b[39m \x1b[38;5;246mEsc\x1b[39m \x1b[38;5;246mto\x1b[39m \x1b[38;5;246mcancel\x1b[39m\n";
        assert!(
            pending_dialog(raw, STARTUP_DIALOGS, &[]).is_none(),
            "raw escapes defeat it"
        );
        let plain = crate::screen::strip_ansi(raw);
        assert_eq!(
            pending_dialog(&plain, STARTUP_DIALOGS, &[]).map(|d| d.label),
            Some("trusted the folder")
        );
    }

    #[test]
    fn a_quiet_screen_before_the_agent_draws_is_not_ready() {
        // Seen live: `aas e …` printed nothing for seconds, the swap took the
        // shell's echo for a ready agent, and the trust prompt drew after.
        let echo = "~ ❯ aas e k-june@claude --dangerously-skip-permissions --resume c63f\n";
        let mut captures = 0usize;
        let mut answered = Vec::new();
        let keys = std::cell::RefCell::new(Vec::<String>::new());
        let resume = wait_for_resume_answering(
            &RealClock,
            || {
                captures += 1;
                Some(
                    match (captures, keys.borrow().len()) {
                        (0..=30, _) => echo,
                        (_, 0) => CLAUDE_TRUST,
                        _ => CLAUDE_READY,
                    }
                    .to_owned(),
                )
            },
            || true,
            |k| {
                keys.borrow_mut().push(k.concat());
                true
            },
            &|consent| consent == Consent::Bypass,
            Duration::from_secs(10),
            Duration::from_millis(20),
            Duration::from_millis(5),
            &mut answered,
        );
        assert_eq!(resume, Resume::Ready);
        assert_eq!(answered, vec!["trusted the folder"]);
    }

    #[test]
    fn an_agent_that_never_draws_its_prompt_is_unsettled_not_ready() {
        let echo = "~ ❯ aas e k-june@claude --resume c63f\n";
        let resume = wait_for_resume(
            || Some(echo.to_owned()),
            || true,
            Duration::from_millis(150),
            Duration::from_millis(20),
            Duration::from_millis(5),
        );
        assert_eq!(resume, Resume::Unsettled);
    }

    #[test]
    fn claudes_trust_prompt_is_answered_yes_before_the_session_counts_as_ready() {
        let (resume, answered, keys) = answering(CLAUDE_TRUST, CLAUDE_READY, 1, true);
        assert_eq!(resume, Resume::Ready);
        assert_eq!(answered, vec!["trusted the folder"]);
        // Down to "Yes", then Enter: Enter alone would pick "No, exit".
        assert_eq!(keys, vec!["\x1b[B\r".to_owned()]);
    }

    /// Drive readiness over `before` (then `after` once answered) with the
    /// consents `granted` and an agent that is `alive`.
    fn answering_with(
        before: &str,
        after: &str,
        granted: &[Consent],
        alive: bool,
    ) -> (Resume, Vec<&'static str>, Vec<String>) {
        let keys = std::cell::RefCell::new(Vec::<String>::new());
        let mut answered = Vec::new();
        let resume = wait_for_resume_answering(
            &RealClock,
            || {
                Some(
                    if keys.borrow().is_empty() {
                        before
                    } else {
                        after
                    }
                    .to_owned(),
                )
            },
            || alive,
            |k| {
                keys.borrow_mut().push(k.concat());
                true
            },
            &|consent| granted.contains(&consent),
            Duration::from_secs(10),
            Duration::from_millis(20),
            Duration::from_millis(5),
            &mut answered,
        );
        (resume, answered, keys.into_inner())
    }

    /// Trusting a folder's settings is never done for the user: the swap
    /// stops at it with nothing typed, wherever its cursor sits.
    #[test]
    fn claudes_settings_trust_is_left_to_the_user() {
        let cancel_first = " This folder pre-approves 2 tool permissions in .claude/settings.json:\n \
            ❯ No, exit Claude Code\n   Yes, I trust these settings\n Enter to confirm · Esc to cancel\n";
        let confirm_first = " This folder pre-approves 2 tool permissions in .claude/settings.json:\n \
            ❯ Yes, I trust these settings\n   No, exit Claude Code\n Enter to confirm · Esc to cancel\n";
        for screen in [cancel_first, confirm_first] {
            let dialog = pending_dialog(screen, STARTUP_DIALOGS, &[]).expect("settings dialog");
            assert_eq!(dialog.consent, Consent::Settings);
            let (resume, answered, keys) =
                answering_with(screen, CLAUDE_READY, &[Consent::Bypass], true);
            assert!(
                matches!(&resume, Resume::Blocked(r) if r.contains("needs your answer")),
                "{resume:?}"
            );
            assert!(answered.is_empty() && keys.is_empty(), "{keys:?}");
        }
        // The cursor still decides the keys where an answer is allowed.
        let dialog = pending_dialog(cancel_first, STARTUP_DIALOGS, &[]).unwrap();
        assert_eq!(dialog.keys_on(cancel_first), Some(vec![DOWN, ENTER]));
        let bare = cancel_first.replace('❯', " ");
        assert_eq!(dialog.keys_on(&bare), None);
    }

    const BYPASS_WARNING: &str = " WARNING: Claude Code running in Bypass Permissions mode\n \
        In Bypass Permissions mode, Claude Code will not ask for your approval.\n \
        ❯ No, exit\n   Yes, I accept\n Enter to confirm · Esc to cancel\n";

    /// Bypass-permissions mode is accepted only with the evidence that the
    /// stopped agent already ran that way.
    #[test]
    fn claudes_bypass_permissions_warning_is_accepted_only_with_evidence() {
        let (resume, answered, keys) =
            answering_with(BYPASS_WARNING, CLAUDE_READY, &[Consent::Bypass], true);
        assert_eq!(resume, Resume::Ready);
        assert_eq!(answered, vec!["accepted bypass-permissions mode"]);
        assert_eq!(keys, vec!["\x1b[B\r".to_owned()]);
        let (resume, _, keys) = answering_with(BYPASS_WARNING, CLAUDE_READY, &[], true);
        assert!(matches!(resume, Resume::Blocked(r) if r.contains("needs your answer")));
        assert!(keys.is_empty());
        // The evidence: the switch as an option of the process's own.
        let argv = |words: &[&str]| words.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>();
        let has = |words: &[&str]| claude_switch(Some(&argv(words)), BYPASS_SWITCH);
        assert!(has(&[
            "claude",
            "--dangerously-skip-permissions",
            "--resume",
            "x"
        ]));
        assert!(has(&[
            "/x/claude",
            "--model",
            "opus",
            "--dangerously-skip-permissions"
        ]));
        // Prompt text, another option's value, after `--`, another program,
        // or unreadable: no.
        assert!(!has(&[
            "claude",
            "explain --dangerously-skip-permissions safely"
        ]));
        assert!(!has(&[
            "claude",
            "--append-system-prompt",
            "--dangerously-skip-permissions",
            "--resume",
            "x"
        ]));
        assert!(!has(&["claude", "--", "--dangerously-skip-permissions"]));
        assert!(!has(&["aas", "e", "b", "--dangerously-skip-permissions"]));
        assert!(!claude_switch(None, BYPASS_SWITCH));
    }

    /// A dialog's words quoted above the agent's own prompt are not a
    /// dialog: no key goes into the composer (reproduced by the Codex peer
    /// on #344, which typed Up and Enter here).
    #[test]
    fn dialog_words_above_the_composer_are_not_a_dialog() {
        let quoted = "Earlier explanation of a warning:\nBypass Permissions mode\nYes, I accept\n\
            ❯ half typed draft\n⏵⏵ bypass permissions on (shift+tab to cycle)\n? for shortcuts\n";
        assert!(pending_dialog(quoted, STARTUP_DIALOGS, &[]).is_none());
        let dialog = STARTUP_DIALOGS
            .iter()
            .find(|d| d.consent == Consent::Bypass)
            .unwrap();
        assert_eq!(dialog.keys_on(quoted), None);
        let (_, answered, keys) = answering_with(quoted, quoted, &[Consent::Bypass], true);
        assert!(answered.is_empty() && keys.is_empty(), "{keys:?}");
    }

    /// A menu whose rows answer two dialogs, or whose row is only quoted
    /// above it, is never answered: the settings gate cannot be lost to an
    /// earlier folder-trust label (reproduced by the Codex peer on #345).
    #[test]
    fn a_mixed_or_quoted_menu_is_not_answered() {
        // Folder-trust words above a live settings menu, at the menu's column.
        let same_column = "    Yes, I trust this folder\n  ❯ No, exit Claude Code\n    \
            Yes, I trust these settings\n  Enter to confirm · Esc to cancel\n";
        let mixed = " Yes, I trust this folder\n ❯ No, exit Claude Code\n   Yes, I trust these \
            settings\n Enter to confirm · Esc to cancel\n";
        for screen in [same_column, mixed] {
            if let Some(dialog) = pending_dialog(screen, STARTUP_DIALOGS, &[]) {
                assert_eq!(dialog.consent, Consent::Settings, "{screen}");
            }
            let (_, answered, keys) = answering_with(screen, CLAUDE_READY, &[], true);
            assert!(answered.is_empty() && keys.is_empty(), "{screen}: {keys:?}");
        }
        // The rows of the menu itself, and only those.
        let (rows, cursor) = option_rows(BYPASS_WARNING).unwrap();
        assert_eq!(rows, ["No, exit", "Yes, I accept"]);
        assert_eq!(cursor, 0);
    }

    /// A fixed-key dialog is answered only when the dialog on screen shows
    /// it is that one: its row in the current menu, or its own words on its
    /// last lines. An earlier mention never stands in (reproduced by the
    /// Codex peer on #345, which typed Down and Enter into another modal).
    #[test]
    fn a_fixed_key_dialog_needs_its_own_current_identity() {
        let other_menu =
            "Earlier discussion: Teach auto mode\nA different confirmation is now open\n \
            ❯ No, cancel\n   Yes, allow\n Enter to confirm · Esc to cancel\n";
        let unparseable =
            "Earlier discussion: Teach auto mode\n This folder pre-approves tools:\n \
            ❯ No, exit Claude Code\n   Yes, I trust these settings\n   ❯ other\n \
            Enter to confirm · Esc to cancel\n";
        for screen in [other_menu, unparseable] {
            let (_, answered, keys) =
                answering_with(screen, CLAUDE_READY, &[Consent::Bypass], true);
            assert!(answered.is_empty() && keys.is_empty(), "{screen}: {keys:?}");
        }
        // Codex's update prompt by its own menu row; its lock screen by its
        // own last line.
        let update = "  Update available · 0.164.0 → 0.166.0\n› 1. Update now (runs `npm i`)\n  \
            2. Skip\n  3. Skip until next version\n  enter continue · esc skip\n";
        assert!(pending_dialog(update, STARTUP_DIALOGS, &[]).is_some());
        let quoted_update = "Update available, Skip until next version, said the log\n \
            ❯ No, cancel\n   Yes, allow\n Enter to confirm · Esc to cancel\n";
        assert!(pending_dialog(quoted_update, STARTUP_DIALOGS, &[]).is_none());
        let lock = "  🔒  This conversation is open in another app        r to retry\n      \
            Close it there and press R to continue here.\n   r retry   f fork   esc/⌃c/q exit\n";
        assert!(pending_dialog(lock, STARTUP_DIALOGS, &[]).is_some());
    }

    /// A dialog left on the screen by an agent that is gone is not answered.
    #[test]
    fn a_stale_dialog_of_an_exited_agent_is_not_answered() {
        let (resume, _, keys) =
            answering_with(BYPASS_WARNING, CLAUDE_READY, &[Consent::Bypass], false);
        assert_eq!(resume, Resume::Exited);
        assert!(keys.is_empty());
    }

    #[test]
    fn codexs_update_prompt_is_skipped_not_installed() {
        // Seen live on 0.164.0: it blocked every relaunch until skipped.
        let update = "  Update available · 0.164.0 → 0.166.0\n  Release notes:\n  \
            https://github.com/openai/codex/releases/latest\n› 1. Update now (runs `npm install -g @openai/codex`)\n  \
            2. Skip\n  3. Skip until next version\n  enter continue · esc skip\n";
        let ready = "› Ask Codex to do anything\n  gpt-6 medium · ~/repo\n";
        let (resume, answered, keys) = answering(update, ready, 1, true);
        assert_eq!(resume, Resume::Ready);
        assert_eq!(answered, vec!["skipped the Codex update prompt"]);
        assert_eq!(keys, vec!["\x1b".to_owned()]);
    }

    #[test]
    fn a_trust_prompt_that_redraws_slowly_after_its_answer_is_waited_out() {
        let captures_after = std::cell::Cell::new(0usize);
        let answers = std::cell::Cell::new(0usize);
        let mut answered = Vec::new();
        let resume = wait_for_resume_answering(
            &RealClock,
            || {
                if answers.get() == 0 {
                    return Some(CLAUDE_TRUST.to_owned());
                }
                captures_after.set(captures_after.get() + 1);
                // Well past the quiet window before the answer shows.
                Some(
                    if captures_after.get() < 40 {
                        CLAUDE_TRUST
                    } else {
                        CLAUDE_READY
                    }
                    .to_owned(),
                )
            },
            || true,
            |_| {
                answers.set(answers.get() + 1);
                true
            },
            &|consent| consent == Consent::Bypass,
            Duration::from_secs(10),
            Duration::from_millis(20),
            Duration::from_millis(5),
            &mut answered,
        );
        assert_eq!(resume, Resume::Ready);
        assert_eq!(answers.get(), 1, "answered once, not again");
    }

    #[test]
    fn a_trust_prompt_that_cannot_be_answered_is_blocked_not_ready() {
        let (resume, _, _) = answering(CLAUDE_TRUST, CLAUDE_READY, 1, false);
        assert!(matches!(resume, Resume::Blocked(r) if r.contains("trusted the folder")));
    }

    #[test]
    fn an_unknown_dialog_is_blocked_so_no_follow_up_is_typed_into_it() {
        let unknown =
            "  Something new?\n  ❯ 1. Yes\n    2. No\n  Enter to confirm · Esc to cancel\n";
        let (resume, answered, keys) = answering(unknown, CLAUDE_READY, 1, true);
        assert!(matches!(resume, Resume::Blocked(r) if r.contains("Enter to confirm")));
        assert!(answered.is_empty() && keys.is_empty());
    }

    #[test]
    fn a_conversation_another_app_still_holds_is_retried_then_given_up_on() {
        let (resume, answered, keys) = answering(CODEX_LOCKED, RESUMED, 2, true);
        assert_eq!(resume, Resume::Ready);
        assert_eq!(answered.len(), 2);
        assert!(keys.iter().all(|k| k == "r"));
        // Still held after five tries: reported, not waited on forever.
        let (resume, answered, _) = answering(CODEX_LOCKED, RESUMED, usize::MAX, true);
        assert!(matches!(resume, Resume::Blocked(r) if r.contains("another app")));
        assert_eq!(answered.len(), 5);
    }

    #[test]
    fn codex_resume_menus_resume_the_goal_and_leave_the_directory_menu_alone() {
        // A swap relaunch never shows this menu (`tui.resume_cwd`); shown
        // anyway, nothing is typed and the swap says what is waiting.
        let directory = "  Working directory · resume\n  Session = latest cwd recorded in the resumed session\n\
            › 1. Use session directory (/Users/jiun/personal/steam)\n  2. Use current directory (/Users/jiun)\n\
              enter continue · esc use session · ⌃c quit\n";
        let (resume, answered, keys) = answering(directory, RESUMED, 1, true);
        assert!(matches!(resume, Resume::Blocked(_)), "{resume:?}");
        assert!(answered.is_empty());
        assert!(keys.is_empty());
        let goal = "    필요한 구현·테스트·문서 개선을 완료한다.\n  › 1. Resume goal   Mark it active\n    2. Leave paused\n    enter select · esc back\n";
        let (resume, answered, _) = answering(goal, RESUMED, 1, true);
        assert_eq!(resume, Resume::Ready);
        assert_eq!(answered, vec!["resumed the paused goal"]);
    }

    #[test]
    fn a_screen_that_never_settles_is_unsettled_unless_it_shows_a_blocker() {
        fn spinning(prefix: &'static str) -> Resume {
            let mut tick = 0u32;
            wait_for_resume(
                || {
                    tick += 1;
                    Some(format!("{prefix}spinner {tick}\n"))
                },
                || true,
                Duration::from_millis(60),
                Duration::from_millis(40),
                Duration::from_millis(5),
            )
        }
        assert_eq!(spinning(""), Resume::Unsettled);
        assert!(matches!(spinning(OWNED_ELSEWHERE), Resume::Blocked(_)));
    }

    #[test]
    fn a_resumed_turn_is_recognised_on_either_tui() {
        assert!(turn_running("✻ Thinking… (12s · ↓ 300 tokens)\n────\n❯ \n"));
        assert!(turn_running(
            "• Working (5s • esc to interrupt)\n› Ask Codex to do anything\n"
        ));
        assert!(!turn_running(
            "✻ Cooked for 1m 2s · done 4:45 PM\n────\n❯ \n"
        ));
        assert!(!turn_running(RESUMED));
    }

    #[test]
    fn a_startup_blocker_says_whether_tend_may_answer_it() {
        let update = "  Update available · 0.161.0 → 0.167.2\n› 1. Update now (runs `npm i`)\n  \
            2. Skip\n  3. Skip until next version\n  enter continue · esc skip\n";
        let found = startup_blocker(update).expect("the update menu");
        assert_eq!(found.label, "skipped the Codex update prompt");
        assert_eq!(found.keys, Some(vec![ESC]));
        assert_eq!(found.consent, Consent::None);
        let settings = " This folder pre-approves 2 tool permissions in .claude/settings.json:\n \
            ❯ Yes, I trust these settings\n   No, exit Claude Code\n Enter to confirm · Esc to cancel\n";
        let found = startup_blocker(settings).expect("the settings dialog");
        assert_eq!(found.keys, None, "never answered for the user");
        assert_eq!(found.consent, Consent::Settings);
        assert!(startup_blocker("› Ask Codex to do anything\n").is_none());
        // Captured from the real Codex 0.154.0 on a scratch tmux server
        // (2026-11-14): its footer is `Press enter to continue`, and Esc
        // skips it without installing anything.
        let real_0_154 = "  ✨ Update available! 0.154.0 -> 0.169.0\n  \
            Release notes: https://github.com/openai/codex/releases/latest\n\
            › 1. Update now (runs `npm install -g @openai/codex`)\n  2. Skip\n  \
            3. Skip until next version\n  Press enter to continue\n";
        let found = startup_blocker(real_0_154).expect("the 0.154 update menu");
        assert_eq!(found.keys, Some(vec![ESC]));
        // The same release's folder trust has no entry here: reported, not typed.
        let trust_0_154 = "> You are in /tmp/w\n  Do you trust the contents of this directory?\n\
            › 1. Yes, continue\n  2. No, quit\n  Press enter to continue\n";
        let found = startup_blocker(trust_0_154).expect("an open question");
        assert_eq!(found.label, "an unrecognised startup dialog");
        assert_eq!(found.keys, None);
    }

    #[test]
    fn a_generic_caller_gets_keys_only_for_the_update_menu() {
        let trust = "  Do you trust the contents of this directory?\n› 1. Trust and continue\n  2. Quit\n  \
            Trust this folder · enter continue\n";
        if let Some(found) = startup_blocker(trust) {
            assert_eq!(found.keys, None, "{}", found.label);
        }
        let goal = "    Finish the docs.\n  › 1. Resume goal   Mark it active\n    2. Leave paused\n    enter select · esc back\n";
        let found = startup_blocker(goal).expect("the goal menu");
        assert_eq!(found.label, "resumed the paused goal");
        assert_eq!(found.keys, None, "starting old work needs evidence");
    }
}
