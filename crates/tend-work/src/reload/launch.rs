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

/// How long relaunched Claude panes are watched for the folder-trust prompt.
/// The prompt is the first thing Claude draws, so this only has to cover a
/// slow start; a pane that shows Claude's own screen is let go at once.
pub(super) const TRUST_TIMEOUT: Duration = Duration::from_secs(20);

/// How long the one arrow press gets to show "Yes" selected before the
/// prompt is left to a human.
const MOVE_TIMEOUT: Duration = Duration::from_secs(3);

pub(super) const TRUST_UNRESOLVED_NOTE: &str =
    "Claude is asking whether to trust this folder; answer it by hand";

pub(super) const TRUST_UNSEEN_NOTE: &str =
    "Claude's screen was not recognized within 20s; it may be waiting on a \
     startup prompt, so nothing more was typed";

pub(super) const TRUST_ELSEWHERE_NOTE: &str =
    "Claude is asking to trust a folder other than the one saved for this pane; \
     left for you to answer";

/// Where the cursor sits on Claude's folder-trust prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TrustCursor {
    OnYes,
    OnNo,
}

/// Claude's folder-trust prompt as read off a pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TrustPrompt {
    /// The directory the prompt asks about, as Claude printed it.
    pub workspace: String,
    pub cursor: TrustCursor,
}

/// Claude's folder-trust prompt, if `screen` is that prompt and nothing else.
///
/// Claude asks this before it opens a conversation in a directory its
/// config has not trusted. That includes `--resume`, when the relaunch comes
/// back under another account's config or the trust was never recorded.
///
/// The parse fails closed. It needs the whole dialog in order: the
/// "Accessing workspace:" header with its path, the safety question,
/// exactly the two menu rows with exactly one under the cursor, and the
/// "Enter to confirm" footer. Any sign of Claude's running screen (banner,
/// composer footer) rejects the capture, so a resumed conversation that
/// quotes the dialog is never read as one.
pub(super) fn claude_trust_prompt(screen: &str) -> Option<TrustPrompt> {
    if claude_started(screen) {
        return None;
    }
    let raw: Vec<&str> = screen.lines().collect();
    let lines: Vec<&str> = raw.iter().map(|line| line.trim()).collect();
    let header = lines
        .iter()
        .position(|line| *line == "Accessing workspace:")?;
    let path_start = header
        + 1
        + lines[header + 1..]
            .iter()
            .position(|line| !line.is_empty())?;
    // The path ends at a row with nothing but ASCII padding: a row holding
    // only a wrapped no-break space is part of it.
    let path_len = raw[path_start..]
        .iter()
        .position(|row| row.trim_matches([' ', '\r']).is_empty())
        .unwrap_or(raw.len() - path_start);
    // The path keeps every character but the dialog's ASCII indent and
    // padding: a name may end in other whitespace (Windows keeps a trailing
    // no-break space), and trimming it would name another folder.
    let rows: Vec<&str> = raw[path_start..path_start + path_len]
        .iter()
        .map(|row| row.trim_matches([' ', '\r']))
        .collect();
    // A long path wraps across rows of the capture. Where a row breaks, a
    // space may have been dropped, so a wrapped Windows path (where names
    // with spaces are the rule) is not trusted automatically.
    if rows.len() > 1 && !rows[0].starts_with('/') {
        return None;
    }
    let workspace: String = rows.concat();
    if !is_absolute_dir(&workspace) {
        return None;
    }
    let rest = &lines[path_start + path_len..];
    let question = rest
        .iter()
        .position(|line| line.contains("Is this a project you created or one you trust?"))?;
    let rest = &rest[question + 1..];

    let mut yes = None;
    let mut no = None;
    let mut footer = None;
    for (index, line) in rest.iter().enumerate() {
        if line.starts_with("Enter to confirm") {
            footer = Some(index);
            break;
        }
        let (selected, label) = match line.strip_prefix(['❯', '>', '›']) {
            Some(label) => (true, label.trim_start()),
            None => (false, *line),
        };
        let slot = match label {
            "Yes, I trust this folder" => &mut yes,
            "No, exit" => &mut no,
            _ => continue,
        };
        if slot.replace((index, selected)).is_some() {
            return None;
        }
    }
    let ((yes_at, yes_selected), (no_at, no_selected)) = (yes?, no?);
    let footer = footer?;
    if yes_at > footer || no_at > footer || yes_selected == no_selected {
        return None;
    }
    Some(TrustPrompt {
        workspace,
        cursor: if yes_selected {
            TrustCursor::OnYes
        } else {
            TrustCursor::OnNo
        },
    })
}

/// Whether `screen` is Claude past any startup prompt: its banner, or the
/// footer under its composer.
pub(super) fn claude_started(screen: &str) -> bool {
    [
        "Claude Code v",
        "? for shortcuts",
        "shift+tab to cycle",
        "esc to interrupt",
    ]
    .iter()
    .any(|marker| screen.contains(marker))
}

/// Whether `screen` might be a startup trust or policy dialog in any layout,
/// recognized or not: the word "trust" next to a confirm-or-quit choice.
/// Deliberately broad. It only ever holds input back, never sends any.
pub(super) fn may_be_trust_prompt(screen: &str) -> bool {
    let screen = screen.to_lowercase();
    screen.contains("trust")
        && [
            "no, exit",
            "no, quit",
            "enter to confirm",
            "press enter to continue",
        ]
        .iter()
        .any(|choice| screen.contains(choice))
}

/// A key to send to a pane sitting on the trust prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TrustKey {
    Down,
    Enter,
}

#[derive(Debug)]
struct TrustPane {
    /// The directories this pane was saved in: as recorded, and resolved.
    expected: Vec<String>,
    /// When the one arrow press went out.
    moved_at: Option<Duration>,
    /// Whether the latest capture was, or might have been, a trust prompt.
    on_prompt: bool,
}

/// Relaunched Claude panes being walked past the folder-trust prompt.
///
/// Each pane gets at most one arrow press and one Enter, and Enter only on a
/// capture showing "Yes" selected after any arrow press. With only one arrow
/// sent, no earlier key can still be in flight when that capture is read.
#[derive(Debug, Default)]
pub(super) struct TrustWatch {
    pending: BTreeMap<String, TrustPane>,
    left: BTreeMap<String, &'static str>,
}

impl TrustWatch {
    /// `targets` pairs each pane with the directories it was saved in.
    pub fn new(targets: impl IntoIterator<Item = (String, Vec<String>)>) -> Self {
        Self {
            pending: targets
                .into_iter()
                .map(|(target, expected)| {
                    let expected = expected
                        .into_iter()
                        .map(|dir| normalize_dir(&dir).to_owned())
                        .collect();
                    (
                        target,
                        TrustPane {
                            expected,
                            moved_at: None,
                            on_prompt: false,
                        },
                    )
                })
                .collect(),
            left: BTreeMap::new(),
        }
    }

    pub fn pending(&self) -> impl Iterator<Item = &String> {
        self.pending.keys()
    }

    pub fn is_settled(&self) -> bool {
        self.pending.is_empty()
    }

    /// Read one capture of `target` and say which key, if any, to send.
    /// Enter takes the pane off the watch; the caller decides whether it
    /// was answered, by whether the key went out.
    pub fn observe(&mut self, target: &str, elapsed: Duration, screen: &str) -> Option<TrustKey> {
        let pane = self.pending.get_mut(target)?;
        let Some(prompt) = claude_trust_prompt(screen) else {
            pane.on_prompt = may_be_trust_prompt(screen);
            if claude_started(screen) && !pane.on_prompt {
                self.pending.remove(target);
            }
            return None;
        };
        pane.on_prompt = true;
        if !pane
            .expected
            .iter()
            .any(|dir| dir == normalize_dir(&prompt.workspace))
        {
            self.leave(target, TRUST_ELSEWHERE_NOTE);
            return None;
        }
        match (prompt.cursor, pane.moved_at) {
            (TrustCursor::OnYes, _) => {
                self.pending.remove(target);
                Some(TrustKey::Enter)
            }
            (TrustCursor::OnNo, None) => {
                pane.moved_at = Some(elapsed);
                Some(TrustKey::Down)
            }
            (TrustCursor::OnNo, Some(at)) if elapsed.saturating_sub(at) < MOVE_TIMEOUT => None,
            (TrustCursor::OnNo, Some(_)) => {
                // The arrow never showed "Yes" selected: no second guess.
                self.leave(target, TRUST_UNRESOLVED_NOTE);
                None
            }
        }
    }

    /// Take `target` off the watch, left for a human with `note`.
    pub fn leave(&mut self, target: &str, note: &'static str) {
        self.pending.remove(target);
        self.left.insert(target.to_owned(), note);
    }

    /// The panes not known to be past the trust prompt, with what to tell a
    /// human. Run when time is up. Every pane still watched then counts:
    /// only a recognized Claude screen lets a pane go, so an unfamiliar or
    /// wrapped dialog is held back rather than assumed answered.
    pub fn unresolved(mut self) -> BTreeMap<String, &'static str> {
        for (target, pane) in std::mem::take(&mut self.pending) {
            let note = if pane.on_prompt {
                TRUST_UNRESOLVED_NOTE
            } else {
                TRUST_UNSEEN_NOTE
            };
            self.left.insert(target, note);
        }
        self.left
    }
}

/// Whether Claude printed a whole path: `/`-rooted, or a Windows drive or
/// UNC path (`C:\…`, `\\server\share`). Both grammars are accepted on
/// every host so the parse does not depend on where tend was built; the
/// workspace must still equal a directory saved for the pane.
fn is_absolute_dir(dir: &str) -> bool {
    let bytes = dir.as_bytes();
    dir.starts_with('/')
        || dir.starts_with(r"\\")
        || (bytes.len() > 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'\\' | b'/'))
}

fn normalize_dir(dir: &str) -> &str {
    match dir.trim_end_matches('/') {
        "" => "/",
        trimmed => trimmed,
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

    /// Captured from Claude Code 2.1.322 in a directory it had not trusted.
    const TRUST_PROMPT_NO: &str = "\
────────────────────────────────────────────────
 Accessing workspace:

 /tmp/untrusted-probe

 Quick safety check: Is this a project you created or one you trust? (Like your own code, a well-known open source
 project, or work from your team). If not, take a moment to review what's in this folder first.

 Claude Code'll be able to read, edit, and execute files here.

 Security guide

 ❯ No, exit
   Yes, I trust this folder

 Enter to confirm · Esc to cancel
";

    fn moved_to_yes() -> String {
        TRUST_PROMPT_NO
            .replace(" ❯ No, exit", "   No, exit")
            .replace("   Yes, I trust", " ❯ Yes, I trust")
    }

    fn watch(dirs: &[&str]) -> TrustWatch {
        TrustWatch::new([(
            "=a:0.0".to_owned(),
            dirs.iter().map(|dir| (*dir).to_owned()).collect(),
        )])
    }

    #[test]
    fn the_trust_prompt_is_read_by_its_cursor_row_and_path() {
        assert_eq!(
            claude_trust_prompt(TRUST_PROMPT_NO),
            Some(TrustPrompt {
                workspace: "/tmp/untrusted-probe".into(),
                cursor: TrustCursor::OnNo,
            })
        );
        assert_eq!(
            claude_trust_prompt(&moved_to_yes()).map(|prompt| prompt.cursor),
            Some(TrustCursor::OnYes)
        );
        // A path wider than the pane wraps; the rows join back up.
        let wrapped = TRUST_PROMPT_NO.replace(" /tmp/untrusted-probe", " /tmp/untrusted-\n probe");
        assert_eq!(
            claude_trust_prompt(&wrapped).map(|prompt| prompt.workspace),
            Some("/tmp/untrusted-probe".into())
        );
    }

    #[test]
    fn a_windows_workspace_is_read_as_a_whole_path() {
        let windows =
            TRUST_PROMPT_NO.replace("/tmp/untrusted-probe", r"C:\Users\j\untrusted-probe");
        assert_eq!(
            claude_trust_prompt(&windows).map(|prompt| prompt.workspace),
            Some(r"C:\Users\j\untrusted-probe".into())
        );
        let unc = TRUST_PROMPT_NO.replace("/tmp/untrusted-probe", r"\\host\share\probe");
        assert!(claude_trust_prompt(&unc).is_some());
        for relative in ["untrusted-probe", r"C:untrusted", r"..\probe"] {
            let screen = TRUST_PROMPT_NO.replace("/tmp/untrusted-probe", relative);
            assert_eq!(claude_trust_prompt(&screen), None, "{relative}");
        }
    }

    #[test]
    fn a_windows_path_is_never_rebuilt_into_another_trusted_folder() {
        let trusted = r"C:\Users\j\trusted";
        // A folder named like the trusted one plus a trailing no-break
        // space (Windows keeps it): the space is part of the name.
        let nbsp = moved_to_yes().replace("/tmp/untrusted-probe", &format!("{trusted}\u{a0}"));
        assert_eq!(
            claude_trust_prompt(&nbsp).map(|prompt| prompt.workspace),
            Some(format!("{trusted}\u{a0}"))
        );
        assert_eq!(watch(&[trusted]).observe("=a:0.0", ms(0), &nbsp), None);
        // `trust ed` wrapped at its space would join up as `trusted`.
        let wrapped = moved_to_yes().replace(
            " /tmp/untrusted-probe",
            r" C:\Users\j\trust
 ed",
        );
        assert_eq!(claude_trust_prompt(&wrapped), None);
        assert_eq!(watch(&[trusted]).observe("=a:0.0", ms(0), &wrapped), None);
        // Nor when that no-break space wraps onto a row of its own.
        let nbsp_row =
            moved_to_yes().replace("/tmp/untrusted-probe", &format!("{trusted}\n\u{a0}"));
        assert_eq!(claude_trust_prompt(&nbsp_row), None);
        assert_eq!(watch(&[trusted]).observe("=a:0.0", ms(0), &nbsp_row), None);
        // The exact folder on one row is still answered.
        let exact = moved_to_yes().replace("/tmp/untrusted-probe", trusted);
        assert_eq!(
            watch(&[trusted]).observe("=a:0.0", ms(0), &exact),
            Some(TrustKey::Enter)
        );
    }

    #[test]
    fn quoted_or_partial_prompts_are_not_the_prompt() {
        // A resumed conversation quoting the dialog: the composer is there.
        let quoted = format!(
            "{}\n❯ \n  ⏵⏵ bypass permissions on (shift+tab to cycle)\n",
            moved_to_yes()
        );
        assert_eq!(claude_trust_prompt(&quoted), None);
        let banner = format!(" ▐▛███▛█   Claude Code v2.1.322\n{}", moved_to_yes());
        assert_eq!(claude_trust_prompt(&banner), None);
        // One line holding the words is not a menu.
        assert_eq!(
            claude_trust_prompt(
                "> Is this a project you created or one you trust? Yes, I trust this folder\n❯ "
            ),
            None
        );
        // No footer, no header, two cursors, a doubled row: all refused.
        assert_eq!(
            claude_trust_prompt(&moved_to_yes().replace("Enter to confirm", "")),
            None
        );
        assert_eq!(
            claude_trust_prompt(&moved_to_yes().replace("Accessing workspace:", "")),
            None
        );
        assert_eq!(
            claude_trust_prompt(&TRUST_PROMPT_NO.replace("   Yes, I trust", " ❯ Yes, I trust")),
            None
        );
        assert_eq!(
            claude_trust_prompt(&moved_to_yes().replace("   No, exit", "   No, exit\n   No, exit")),
            None
        );
    }

    #[test]
    fn the_trust_prompt_is_walked_to_yes_before_enter() {
        let mut watch = watch(&["/tmp/untrusted-probe/"]);
        assert_eq!(
            watch.observe("=a:0.0", ms(0), TRUST_PROMPT_NO),
            Some(TrustKey::Down)
        );
        // The arrow has not landed yet: no second key.
        assert_eq!(watch.observe("=a:0.0", ms(300), TRUST_PROMPT_NO), None);
        assert_eq!(
            watch.observe("=a:0.0", ms(900), &moved_to_yes()),
            Some(TrustKey::Enter)
        );
        assert!(watch.is_settled());
        assert!(watch.unresolved().is_empty());
    }

    #[test]
    fn one_arrow_only_and_never_enter_on_no() {
        let mut watch = watch(&["/tmp/untrusted-probe"]);
        let mut keys = Vec::new();
        for step in 0..20 {
            keys.extend(watch.observe("=a:0.0", ms(step * 500), TRUST_PROMPT_NO));
        }
        assert_eq!(keys, [TrustKey::Down]);
        assert!(watch.is_settled(), "given up, left to a human");
        assert_eq!(
            watch.unresolved().into_values().collect::<Vec<_>>(),
            [TRUST_UNRESOLVED_NOTE]
        );
    }

    #[test]
    fn a_prompt_for_another_directory_is_left_alone() {
        // The saved directory was gone, so the shell's `cd` failed and Claude
        // started somewhere else: that is not the trust the pane had.
        let mut watch = watch(&["/home/june/project"]);
        assert_eq!(watch.observe("=a:0.0", ms(0), &moved_to_yes()), None);
        assert!(watch.is_settled());
        assert_eq!(
            watch.unresolved().into_values().collect::<Vec<_>>(),
            [TRUST_ELSEWHERE_NOTE]
        );
    }

    #[test]
    fn a_claude_that_starts_without_the_prompt_is_let_go() {
        let mut watch = TrustWatch::new([
            ("=a:0.0".to_owned(), vec!["/w".to_owned()]),
            ("=a:0.1".to_owned(), vec!["/w".to_owned()]),
        ]);
        assert_eq!(watch.observe("=a:0.0", ms(0), ""), None);
        assert_eq!(
            watch.observe("=a:0.0", ms(500), " ▐▛███▜▌   Claude Code v2.1.322\n❯ \n"),
            None
        );
        assert_eq!(watch.pending().cloned().collect::<Vec<_>>(), ["=a:0.1"]);
        // Time runs out on a pane whose screen was never recognized: it is
        // not assumed to be past any prompt.
        assert_eq!(
            watch.unresolved().into_iter().collect::<Vec<_>>(),
            [("=a:0.1".to_owned(), TRUST_UNSEEN_NOTE)]
        );
    }

    #[test]
    fn a_dialog_the_parser_does_not_know_is_still_held_back() {
        // A narrow pane wraps the question: no automatic answer, but the
        // pane is not let go either, even with banner text around it.
        let wrapped = TRUST_PROMPT_NO.replace(
            " Quick safety check: Is this a project you created or one you trust?",
            " Quick safety check: Is this a project you created\n or one you trust?",
        );
        assert_eq!(claude_trust_prompt(&wrapped), None);
        assert!(may_be_trust_prompt(&wrapped));
        let mut watch = watch(&["/tmp/untrusted-probe"]);
        assert_eq!(watch.observe("=a:0.0", ms(0), &wrapped), None);
        let with_banner = format!(" Claude Code v2.1.322\n{wrapped}");
        assert_eq!(watch.observe("=a:0.0", ms(500), &with_banner), None);
        assert_eq!(
            watch.unresolved().into_values().collect::<Vec<_>>(),
            [TRUST_UNRESOLVED_NOTE]
        );
    }

    #[test]
    fn a_pane_still_on_the_prompt_at_the_deadline_is_reported() {
        let mut watch = watch(&["/tmp/untrusted-probe"]);
        assert_eq!(
            watch.observe("=a:0.0", ms(0), TRUST_PROMPT_NO),
            Some(TrustKey::Down)
        );
        assert_eq!(
            watch.unresolved().into_keys().collect::<Vec<_>>(),
            ["=a:0.0"]
        );
    }
}
