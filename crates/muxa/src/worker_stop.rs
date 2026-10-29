//! Gracefully quit a worker agent, once, and only when it is plainly idle.
//!
//! This is the provider half of pruning a worker. The daemon calls it under
//! its own gate, after it has committed the stop and fenced the process; this
//! module only decides whether the screen is safe to type into, types the
//! provider's own quit command a single time, and watches for the process to
//! go away.
//!
//! It is deliberately unlike `account_swap`'s stop: no Ctrl-C to clear a
//! draft, no answering dialogs, no signals, no escalation. Anything it cannot
//! positively read as an idle, empty prompt is refused with nothing typed.

use crate::backend::PaneBackend;
use crate::event::AgentKind;
use std::time::{Duration, Instant};

/// How often the exit probe is asked while waiting.
const POLL: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopOutcome {
    /// The fenced process is positively gone.
    Exited,
    /// Nothing was typed: the prompt was not positively idle and empty, a
    /// dialog was open, or the provider has no verified quit command.
    Refused(String),
    /// The quit command was typed once and the process is still alive after
    /// the wait. Nothing further was tried.
    LeftRunning(String),
    /// The quit command may have been typed, and the outcome is unknown.
    Uncertain(String),
}

pub struct StopRequest<'a> {
    pub backend: &'a dyn PaneBackend,
    /// The pane's mux socket, for the send calls.
    pub socket: Option<&'a str>,
    pub pane: &'a str,
    pub kind: AgentKind,
    pub timeout: Duration,
    /// `Some(true)` the fenced process is gone, `Some(false)` alive, `None`
    /// cannot tell.
    pub exited: &'a (dyn Fn() -> Option<bool> + Sync),
}

/// What the input line of a provider's TUI shows.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Composer {
    /// A recognised, idle prompt with nothing typed into it.
    Empty,
    /// Something is typed: a human's draft, or a half-sent prompt.
    Draft,
    /// The screen is not a prompt this module recognises, a turn is running,
    /// or a dialog is open. Never treated as empty.
    Unknown(String),
}

/// The provider's own quit command, where it is verified.
fn quit_command(kind: AgentKind) -> Option<&'static str> {
    match kind {
        AgentKind::ClaudeCode => Some("/exit"),
        AgentKind::Codex => Some("/quit"),
        _ => None,
    }
}

/// Read the composer strictly. Unlike swap's draft reader, a screen that does
/// not parse is `Unknown`, never `Empty`.
fn read_composer(screen: &str, kind: AgentKind) -> Composer {
    if crate::account_swap::turn_running(screen) {
        return Composer::Unknown("a turn is running".into());
    }
    if let Some(question) = crate::account_swap::open_question(screen) {
        return Composer::Unknown(format!("a dialog is open: {question}"));
    }
    let lines: Vec<&str> = screen.lines().collect();
    match kind {
        AgentKind::ClaudeCode => {
            // The prompt sits between the last two horizontal rules, after `❯`.
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
                return Composer::Unknown("no Claude prompt box on screen".into());
            };
            let inside: Vec<&str> = lines[top + 1..bottom]
                .iter()
                .map(|line| line.trim())
                .filter(|line| !line.is_empty())
                .collect();
            match inside.as_slice() {
                [only]
                    if only.trim_start_matches('❯').trim().is_empty() && only.starts_with('❯') =>
                {
                    Composer::Empty
                }
                [first, ..] if first.starts_with('❯') => Composer::Draft,
                _ => Composer::Unknown("the Claude prompt box is not a plain input".into()),
            }
        }
        AgentKind::Codex => {
            let Some(line) = lines.iter().rev().find(|line| line.starts_with('›')) else {
                return Composer::Unknown("no Codex composer on screen".into());
            };
            let text = line.trim_start_matches('›').trim();
            if text.is_empty() || crate::account_swap::CODEX_PLACEHOLDERS.contains(&text) {
                Composer::Empty
            } else {
                Composer::Draft
            }
        }
        _ => Composer::Unknown("no verified prompt reader for this provider".into()),
    }
}

/// A narrow Codex pane cuts its placeholder short (`› Ask Codex to`), which
/// no longer matches the full text. It is still the placeholder when the
/// composer's text is drawn dim — as Codex draws placeholders, never typed
/// input — *and* is the start of a known placeholder. Both must hold.
fn codex_placeholder_shown(raw: &str, kind: AgentKind) -> bool {
    const DIM: &str = "\u{1b}[2m";
    if kind != AgentKind::Codex {
        return false;
    }
    let Some(line) = raw
        .lines()
        .rev()
        .find(|line| crate::screen::strip_ansi(line).starts_with('›'))
    else {
        return false;
    };
    // After the glyph and its own styling, the text must open dim and carry
    // no other styling before its reset.
    let Some(after_glyph) = line.split_once('›').map(|(_, rest)| rest) else {
        return false;
    };
    let after_glyph = after_glyph.trim_start_matches("\u{1b}[0m").trim_start();
    let Some(text) = after_glyph.strip_prefix(DIM) else {
        return false;
    };
    let text = text.trim_end().trim_end_matches("\u{1b}[0m");
    if text.contains('\u{1b}') {
        return false;
    }
    let shown = text.trim().trim_end_matches('…').trim_end();
    !shown.is_empty()
        && crate::account_swap::CODEX_PLACEHOLDERS
            .iter()
            .any(|placeholder| placeholder.starts_with(shown))
}

/// Quit the agent in `req.pane` if, and only if, its prompt is plainly idle
/// and empty. Blocking: it waits up to `req.timeout` for the exit.
pub fn graceful_stop(req: &StopRequest<'_>) -> StopOutcome {
    let Some(quit) = quit_command(req.kind) else {
        return StopOutcome::Refused(format!("no verified quit command for {:?}", req.kind));
    };
    match (req.exited)() {
        Some(true) => return StopOutcome::Exited,
        Some(false) => {}
        None => return StopOutcome::Refused("cannot observe the worker process".into()),
    }
    // Read the pane on its own server, as plain text: captures keep colour
    // escapes, and a coloured `›` or `❯` would hide an empty prompt.
    let Some(raw) = req.backend.capture_pane_on(req.socket, req.pane) else {
        return StopOutcome::Refused("could not read the pane".into());
    };
    let screen = crate::screen::strip_ansi(&raw);
    let composer = match read_composer(&screen, req.kind) {
        Composer::Draft if codex_placeholder_shown(&raw, req.kind) => Composer::Empty,
        composer => composer,
    };
    match composer {
        Composer::Empty => {}
        Composer::Draft => {
            return StopOutcome::Refused("something is typed into the prompt; left alone".into())
        }
        Composer::Unknown(why) => return StopOutcome::Refused(why),
    }
    // From here the quit may reach the agent; failures are uncertain, not
    // refusals, because part of it can already have been delivered.
    let typed = req.backend.send_prompt_text_on(req.socket, req.pane, quit);
    if !typed {
        return StopOutcome::Uncertain(format!("typing {quit} failed part-way or entirely"));
    }
    std::thread::sleep(crate::backend::prompt_submit_grace(req.pane));
    if !req.backend.send_text_on(req.socket, req.pane, "\r") {
        return StopOutcome::Uncertain(format!("{quit} was typed but Enter could not be sent"));
    }
    let deadline = Instant::now() + req.timeout;
    loop {
        match (req.exited)() {
            Some(true) => return StopOutcome::Exited,
            Some(false) if Instant::now() < deadline => std::thread::sleep(POLL),
            Some(false) => {
                return StopOutcome::LeftRunning(format!(
                    "{quit} was sent; the process is still running after {}s",
                    req.timeout.as_secs()
                ))
            }
            None => {
                return StopOutcome::Uncertain(format!(
                    "{quit} was sent; the process can no longer be observed"
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::BackendCaps;
    use crate::backend::HostKind;
    use crate::tmux::PaneInfo;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    struct FakePane {
        screen: Option<String>,
        typed: Mutex<Vec<String>>,
        fail_send: bool,
        /// When set, the screen is only readable on this server; reading the
        /// process-default server sees nothing.
        only_on: Option<&'static str>,
    }

    impl FakePane {
        fn showing(screen: &str) -> Self {
            Self {
                screen: Some(screen.into()),
                typed: Mutex::new(Vec::new()),
                fail_send: false,
                only_on: None,
            }
        }
        fn typed(&self) -> Vec<String> {
            self.typed.lock().unwrap().clone()
        }
    }

    impl PaneBackend for FakePane {
        fn kind(&self) -> HostKind {
            HostKind::Tmux
        }
        fn list_panes(&self) -> Vec<PaneInfo> {
            Vec::new()
        }
        fn resolve_pane(&self, _pane_id: &str) -> Option<PaneInfo> {
            None
        }
        fn capture_pane(&self, _pane_id: &str) -> Option<String> {
            if self.only_on.is_some() {
                return None;
            }
            self.screen.clone()
        }
        fn capture_pane_on(&self, socket: Option<&str>, pane_id: &str) -> Option<String> {
            match self.only_on {
                Some(only) => (socket == Some(only))
                    .then(|| self.screen.clone())
                    .flatten(),
                None => self.capture_pane(pane_id),
            }
        }
        fn pane_pid_map(&self) -> HashMap<u32, String> {
            HashMap::new()
        }
        fn current_pane(&self) -> Option<String> {
            None
        }
        fn focus_pane(&self, _pane_id: &str) -> bool {
            true
        }
        fn caps(&self) -> BackendCaps {
            BackendCaps::default()
        }
        fn send_text_on(&self, _socket: Option<&str>, _pane_id: &str, text: &str) -> bool {
            if self.fail_send {
                return false;
            }
            self.typed.lock().unwrap().push(text.into());
            true
        }
        fn send_prompt_text_on(&self, socket: Option<&str>, pane_id: &str, text: &str) -> bool {
            self.send_text_on(socket, pane_id, text)
        }
    }

    const CLAUDE_IDLE: &str =
        "✻ Done\n\n────────────────────────\n❯ \n────────────────────────\n  ? for shortcuts\n";
    const CLAUDE_DRAFT: &str =
        "────────────────────────\n❯ please also fix\n────────────────────────\n";
    const CODEX_IDLE: &str = "• Done.\n\n› Ask Codex to do anything\n\n  gpt · ~/repo\n";
    const CODEX_DRAFT: &str = "• Done.\n\n› half a thought\n";
    /// What a live Codex pane captures as: the prompt glyph and placeholder
    /// are coloured.
    /// A narrow pane: the placeholder is cut short, but still drawn dim.
    const CODEX_IDLE_NARROW: &str =
        "• Done.\n\n\u{1b}[1m›\u{1b}[0m \u{1b}[2mAsk Codex to\u{1b}[0m\n\n  gpt\n";
    /// The same words typed, not dim: a draft.
    const CODEX_TYPED_PREFIX: &str = "• Done.\n\n\u{1b}[1m›\u{1b}[0m Ask Codex to\n";
    /// Dim, but not the start of any placeholder.
    const CODEX_DIM_OTHER: &str = "• Done.\n\n\u{1b}[1m›\u{1b}[0m \u{1b}[2mdeploy prod\u{1b}[0m\n";
    const CODEX_IDLE_COLOURED: &str = "• Done.\n\n\u{1b}[1m›\u{1b}[0m \u{1b}[2mAsk Codex to do anything\u{1b}[0m\n\n  gpt · ~/repo\n";

    fn stop(
        pane: &FakePane,
        kind: AgentKind,
        exited: &(dyn Fn() -> Option<bool> + Sync),
    ) -> StopOutcome {
        graceful_stop(&StopRequest {
            backend: pane,
            socket: None,
            pane: "%1",
            kind,
            timeout: Duration::from_millis(300),
            exited,
        })
    }

    #[test]
    fn an_idle_empty_prompt_gets_the_providers_quit_once() {
        for (screen, kind, quit) in [
            (CLAUDE_IDLE, AgentKind::ClaudeCode, "/exit"),
            (CODEX_IDLE, AgentKind::Codex, "/quit"),
            (CODEX_IDLE_COLOURED, AgentKind::Codex, "/quit"),
            (CODEX_IDLE_NARROW, AgentKind::Codex, "/quit"),
        ] {
            let pane = FakePane::showing(screen);
            let probes = AtomicUsize::new(0);
            // Alive at the check, gone once the quit lands.
            let exited = || Some(probes.fetch_add(1, Ordering::SeqCst) > 0);
            assert_eq!(stop(&pane, kind, &exited), StopOutcome::Exited);
            assert_eq!(pane.typed(), [quit, "\r"]);
        }
    }

    #[test]
    fn the_pane_is_read_on_the_workers_own_server() {
        let mut pane = FakePane::showing(CODEX_IDLE);
        pane.only_on = Some("/tmp/rmux-b/default");
        let probes = AtomicUsize::new(0);
        let exited = || Some(probes.fetch_add(1, Ordering::SeqCst) > 0);
        let outcome = graceful_stop(&StopRequest {
            backend: &pane,
            socket: Some("/tmp/rmux-b/default"),
            pane: "%1",
            kind: AgentKind::Codex,
            timeout: Duration::from_millis(300),
            exited: &exited,
        });
        assert_eq!(outcome, StopOutcome::Exited);
    }

    #[test]
    fn a_draft_a_running_turn_a_dialog_or_an_unknown_screen_is_refused_untouched() {
        let alive = || Some(false);
        for (screen, kind) in [
            (CLAUDE_DRAFT, AgentKind::ClaudeCode),
            (CODEX_DRAFT, AgentKind::Codex),
            (CODEX_TYPED_PREFIX, AgentKind::Codex),
            (CODEX_DIM_OTHER, AgentKind::Codex),
            // A dim placeholder only overrides a draft reading, never a turn
            // in progress.
            (
                "• Working (3s • esc to interrupt)\n\n\u{1b}[1m›\u{1b}[0m \u{1b}[2mAsk Codex to\u{1b}[0m\n",
                AgentKind::Codex,
            ),
            (
                "✻ Thinking… (12s · esc to interrupt)\n",
                AgentKind::ClaudeCode,
            ),
            ("Some output with no prompt box\n", AgentKind::ClaudeCode),
            ("plain shell $ \n", AgentKind::Codex),
        ] {
            let pane = FakePane::showing(screen);
            assert!(
                matches!(stop(&pane, kind, &alive), StopOutcome::Refused(_)),
                "{screen:?}"
            );
            assert!(pane.typed().is_empty(), "typed into {screen:?}");
        }
        let unknown = FakePane::showing(CLAUDE_IDLE);
        assert!(matches!(
            stop(&unknown, AgentKind::GeminiCli, &alive),
            StopOutcome::Refused(_)
        ));
        let blind = FakePane::showing(CLAUDE_IDLE);
        assert!(matches!(
            stop(&blind, AgentKind::ClaudeCode, &|| None),
            StopOutcome::Refused(_)
        ));
        assert!(unknown.typed().is_empty() && blind.typed().is_empty());
    }

    #[test]
    fn a_quit_that_does_not_end_the_process_is_left_running_without_escalation() {
        let pane = FakePane::showing(CODEX_IDLE);
        let outcome = stop(&pane, AgentKind::Codex, &|| Some(false));
        assert!(
            matches!(outcome, StopOutcome::LeftRunning(_)),
            "{outcome:?}"
        );
        // One quit, one Enter, nothing else: no Ctrl-C, no retry.
        assert_eq!(pane.typed(), ["/quit", "\r"]);
    }

    #[test]
    fn losing_sight_of_the_process_or_the_send_is_uncertain() {
        let pane = FakePane::showing(CLAUDE_IDLE);
        let probes = AtomicUsize::new(0);
        let vanishes = || (probes.fetch_add(1, Ordering::SeqCst) == 0).then_some(false);
        assert!(matches!(
            stop(&pane, AgentKind::ClaudeCode, &vanishes),
            StopOutcome::Uncertain(_)
        ));
        let mut broken = FakePane::showing(CLAUDE_IDLE);
        broken.fail_send = true;
        assert!(matches!(
            stop(&broken, AgentKind::ClaudeCode, &|| Some(false)),
            StopOutcome::Uncertain(_)
        ));
    }
}
