//! When a restored pane can be typed into, and whether what was typed
//! actually started.
//!
//! Keys sent to a pane are only as good as the shell reading them. A shell
//! that is still running its startup files has no line editor yet: the keys
//! sit in the terminal's input queue, and anything in that startup which
//! reads or drains the terminal — an update prompt, a plugin, a `read -t` —
//! swallows them. The screen is no guide: a startup that prints first, or
//! Powerlevel10k's instant prompt, draws something long before the shell is
//! listening. What does change is the terminal mode: a line editor (zle,
//! readline, fish) switches the terminal out of canonical mode to read keys
//! one at a time, so a pane whose terminal is non-canonical has one reading.
//!
//! Sending is not the same as starting, either, so after the keys go out the
//! pane's foreground process is watched until it is no longer the shell.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use super::{is_shell, BUSY_GRACE};

/// What one poll of a pane saw.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct PaneProbe<'a> {
    /// `#{pane_current_command}`; `None` when the server did not answer.
    pub command: Option<&'a str>,
    /// Anything on the pane's screen.
    pub prompt_drawn: bool,
    /// Whether the pane's terminal is in non-canonical mode — a line editor
    /// reading keys. `None` when the mode could not be read, in which case
    /// the drawn prompt is all there is to go on.
    pub line_editing: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Readiness {
    /// Keep polling.
    Wait,
    /// A shell with its line editor up: keys typed now are read.
    Ready,
    /// Some other program holds the pane; nothing should be typed into it.
    Busy,
}

/// The decision behind `wait_for_shell`, fed one probe at a time.
#[derive(Debug, Default)]
pub(super) struct ShellWatch {
    busy_since: Option<Duration>,
    saw_shell: bool,
}

impl ShellWatch {
    pub fn observe(&mut self, elapsed: Duration, probe: PaneProbe<'_>) -> Readiness {
        match probe.command.map(str::trim) {
            Some(command) if is_shell(command) => {
                self.saw_shell = true;
                self.busy_since = None;
                // A drawn prompt alone is not enough: startup output and an
                // instant prompt are drawn before the line editor exists.
                if probe.prompt_drawn && probe.line_editing != Some(false) {
                    Readiness::Ready
                } else {
                    Readiness::Wait
                }
            }
            Some(_) => {
                let since = *self.busy_since.get_or_insert(elapsed);
                if elapsed.saturating_sub(since) >= BUSY_GRACE {
                    Readiness::Busy
                } else {
                    Readiness::Wait
                }
            }
            None => {
                self.busy_since = None;
                Readiness::Wait
            }
        }
    }

    /// The answer when time runs out: a shell that never looked ready is
    /// still taken at its word; a pane that was never a shell is not typed
    /// into.
    pub fn at_deadline(&self) -> bool {
        self.saw_shell
    }
}

/// Whether `stty -a` output describes a terminal in non-canonical mode.
pub(super) fn line_editing(stty: &str) -> Option<bool> {
    stty.split_whitespace().find_map(|flag| match flag {
        "-icanon" => Some(true),
        "icanon" => Some(false),
        _ => None,
    })
}

/// How long a relaunched pane has to show its program before it is reported
/// as unconfirmed. Covers a slow shell working through its typeahead.
pub(super) const CONFIRM_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) const UNCONFIRMED_NOTE: &str =
    "sent, but the pane was still at its shell prompt after 10s — \
     the shell may have dropped the keys, or the command exited at once";

/// Which relaunched panes have visibly started their program.
///
/// A pane counts as started once two polls in a row show the same program
/// other than its shell. One sighting is not enough: a prompt hook runs short
/// children (`git status` for the prompt) that would otherwise pass for the
/// relaunched command.
#[derive(Debug, Default)]
pub(super) struct LaunchWatch {
    last: BTreeMap<String, String>,
    pending: BTreeSet<String>,
}

impl LaunchWatch {
    pub fn new(targets: impl IntoIterator<Item = String>) -> Self {
        Self {
            last: BTreeMap::new(),
            pending: targets.into_iter().collect(),
        }
    }

    pub fn pending(&self) -> impl Iterator<Item = &String> {
        self.pending.iter()
    }

    pub fn is_settled(&self) -> bool {
        self.pending.is_empty()
    }

    /// Record one poll of `target`'s foreground command.
    pub fn observe(&mut self, target: &str, foreground: Option<&str>) {
        if !self.pending.contains(target) {
            return;
        }
        let Some(command) = foreground.map(str::trim).filter(|c| !c.is_empty()) else {
            self.last.remove(target);
            return;
        };
        if is_shell(command) {
            self.last.remove(target);
            return;
        }
        if self.last.get(target).map(String::as_str) == Some(command) {
            self.pending.remove(target);
        } else {
            self.last.insert(target.to_owned(), command.to_owned());
        }
    }

    /// The panes that never showed their program.
    pub fn unconfirmed(self) -> BTreeSet<String> {
        self.pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(command: &str, drawn: bool, editing: Option<bool>) -> PaneProbe<'_> {
        PaneProbe {
            command: Some(command),
            prompt_drawn: drawn,
            line_editing: editing,
        }
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn startup_output_is_not_a_ready_shell() {
        // A startup that prints first: zsh in the foreground, text on the
        // screen, but the terminal still canonical — no line editor yet, so
        // typed keys would sit where startup can swallow them.
        let mut watch = ShellWatch::default();
        assert_eq!(
            watch.observe(ms(0), probe("zsh", true, Some(false))),
            Readiness::Wait
        );
        assert_eq!(
            watch.observe(ms(2900), probe("zsh", true, Some(false))),
            Readiness::Wait
        );
        // zle comes up: non-canonical.
        assert_eq!(
            watch.observe(ms(3100), probe("zsh", true, Some(true))),
            Readiness::Ready
        );
    }

    #[test]
    fn a_drawn_prompt_suffices_when_the_mode_cannot_be_read() {
        let mut watch = ShellWatch::default();
        assert_eq!(
            watch.observe(ms(0), probe("-zsh", false, None)),
            Readiness::Wait
        );
        assert_eq!(
            watch.observe(ms(200), probe("-zsh", true, None)),
            Readiness::Ready
        );
    }

    #[test]
    fn a_pane_running_something_else_is_busy_after_the_grace() {
        let mut watch = ShellWatch::default();
        assert_eq!(
            watch.observe(ms(0), probe("node", true, Some(false))),
            Readiness::Wait
        );
        assert_eq!(
            watch.observe(ms(1900), probe("node", true, Some(false))),
            Readiness::Wait
        );
        assert_eq!(
            watch.observe(ms(2100), probe("node", true, Some(false))),
            Readiness::Busy
        );
        assert!(!watch.at_deadline(), "never a shell, never typed into");

        // A shell's own startup child for a moment resets once the shell is
        // back in front.
        let mut watch = ShellWatch::default();
        watch.observe(ms(0), probe("git", false, None));
        watch.observe(ms(1500), probe("zsh", false, Some(false)));
        assert_eq!(
            watch.observe(ms(3000), probe("git", false, None)),
            Readiness::Wait
        );
        assert!(watch.at_deadline());
    }

    #[test]
    fn the_terminal_mode_is_read_from_stty() {
        let macos = "speed 38400 baud; 50 rows; 200 columns;\n\
                     lflags: -icanon -isig -iexten -echo echoe echok";
        assert_eq!(line_editing(macos), Some(true));
        let linux = "speed 38400 baud; rows 50; columns 200; line = 0;\n\
                     isig icanon iexten echo echoe echok";
        assert_eq!(line_editing(linux), Some(false));
        assert_eq!(line_editing("stty: /dev/ttys9: No such file"), None);
    }

    #[test]
    fn a_relaunch_is_confirmed_by_its_program_holding_the_pane() {
        let mut watch = LaunchWatch::new(["=a:0.0".to_owned(), "=a:0.1".to_owned()]);
        // A prompt hook's child is seen once, then the shell again: no.
        watch.observe("=a:0.0", Some("git"));
        watch.observe("=a:0.0", Some("zsh"));
        watch.observe("=a:0.1", Some("zsh"));
        assert!(!watch.is_settled());
        // The relaunched program, twice in a row: yes.
        watch.observe("=a:0.0", Some("node"));
        watch.observe("=a:0.0", Some("node"));
        watch.observe("=a:0.1", Some("zsh"));
        assert_eq!(watch.pending().cloned().collect::<Vec<_>>(), ["=a:0.1"]);
        // An unanswered poll forgets the last sighting.
        watch.observe("=a:0.1", Some("tail"));
        watch.observe("=a:0.1", None);
        watch.observe("=a:0.1", Some("tail"));
        assert!(!watch.is_settled());
        assert_eq!(
            watch.unconfirmed().into_iter().collect::<Vec<_>>(),
            ["=a:0.1"]
        );
    }
}
