//! Screen-manifest fallback detection.
//!
//! For agent CLIs muxa has **no hooks** for (cursor-agent, amp, copilot, aider,
//! goose, …) there is no authoritative event stream to drive a registry row.
//! This module implements the *screen-inference* fallback herdr validated:
//! match a set of TOML-declared regex rules against a pane's captured tail to
//! infer whether the agent is `Working`, `Blocked` (waiting on the operator),
//! or `Idle`.
//!
//! The daemon side (candidate selection, pane capture, synthetic-row ingest)
//! lives in `muxad::screen_detect`; this module is the pure, unit-testable core:
//! manifest parsing, bundled + user-override loading, capture preparation
//! (ANSI-strip + tail), and the classifier.
//!
//! ## Precedence (documented in full in `docs/SCREEN_DETECTION.md`)
//!
//! Hooks are authoritative when present; herdr's own detection covers herdr
//! hosts; screen inference is the *last* resort. The daemon enforces this by
//! only synthesizing rows for panes no authoritative row owns, and by minting
//! those rows SYNTHETIC so a real hook evicts them the instant it fires.
//!
//! ## Classifier contract (STRICT-blocked, unknown-keeps-previous)
//!
//! [`AgentManifest::classify`] tests rule categories **in a fixed order**:
//!
//! 1. `blocked` — tested FIRST and STRICT: only clear approval/permission UI
//!    should ever match here. A false `blocked` is the worst outcome (a working
//!    agent shown as "needs input"), so the bundled patterns require an
//!    unambiguous yes/no affordance or explicit permission wording.
//! 2. `working` — spinner glyphs / interrupt hints.
//! 3. `idle` — a bare prompt marker.
//!
//! If NOTHING matches, `classify` returns `None` — the caller keeps the pane's
//! previous state. An unrecognized screen never *transitions* a row; only a
//! positive match to a *different* category does. This is the "unknown
//! transitions are dropped" rule that stops screen noise from flapping a row.

use std::path::PathBuf;
use std::sync::OnceLock;

use regex::{Regex, RegexSet};
use serde::Deserialize;

/// The inferred agent state for one pane capture. A deliberately small set:
/// screen inference can reliably tell "busy" from "waiting on me" from "idle
/// prompt", but not the richer states hooks provide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenState {
    /// An approval/permission prompt is on screen — the agent needs operator
    /// input. Maps to muxa `WaitingInput`.
    Blocked,
    /// The agent is actively generating (spinner / interrupt hint). Maps to
    /// muxa `Working`.
    Working,
    /// A bare input prompt is waiting for the next instruction. Maps to muxa
    /// `Idle`.
    Idle,
}

/// Why a manifest failed to load. Parse failures are logged and the offending
/// file skipped — never fatal (see [`load_manifests`]).
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("TOML parse error: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("invalid regex in [rules].{category}: {source}")]
    Regex {
        category: &'static str,
        #[source]
        source: regex::Error,
    },
    #[error("[agent].name must not be empty")]
    EmptyName,
    #[error("[agent].command must list at least one non-empty command")]
    EmptyCommand,
}

/// Raw TOML shape. Unknown fields are *tolerated* (not `deny_unknown_fields`):
/// a manifest written for a newer muxa should still load its known rules rather
/// than being dropped wholesale over one unrecognized key.
#[derive(Debug, Deserialize)]
struct RawManifest {
    agent: RawAgent,
    #[serde(default)]
    rules: RawRules,
}

#[derive(Debug, Deserialize)]
struct RawAgent {
    name: String,
    command: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawRules {
    #[serde(default)]
    blocked: Vec<String>,
    #[serde(default)]
    working: Vec<String>,
    #[serde(default)]
    idle: Vec<String>,
}

/// One agent's screen-detection manifest: its identity, the process names that
/// select it, and the compiled regex rule sets.
#[derive(Debug, Clone)]
pub struct AgentManifest {
    /// Display name — carried into the synthetic row's `model` field (the kind
    /// stays `Unknown`). Also the override key: a user file with the same name
    /// replaces the bundled manifest.
    pub name: String,
    /// Lowercased command *basenames* that select this manifest against a
    /// pane's `current_command` (tmux reports the basename only).
    commands: Vec<String>,
    blocked: RegexSet,
    working: RegexSet,
    idle: RegexSet,
}

impl AgentManifest {
    /// Does this manifest govern a pane whose foreground command is `cmd`?
    /// Matching is on the command basename, case-insensitive — mirroring
    /// `discovery::classify_command`.
    #[must_use]
    pub fn matches_command(&self, cmd: &str) -> bool {
        let base = command_basename(cmd);
        self.commands.iter().any(|c| c == &base)
    }

    /// Classify a *prepared* capture (see [`prepare_capture`]). Returns `None`
    /// when no rule matches — the caller keeps the pane's previous state. See
    /// the module-level classifier contract for the STRICT-blocked ordering.
    #[must_use]
    pub fn classify(&self, text: &str) -> Option<ScreenState> {
        if self.blocked.is_match(text) {
            Some(ScreenState::Blocked)
        } else if self.working.is_match(text) {
            Some(ScreenState::Working)
        } else if self.idle.is_match(text) {
            Some(ScreenState::Idle)
        } else {
            None
        }
    }
}

/// The full set of loaded manifests, indexed for command lookup.
#[derive(Debug, Clone, Default)]
pub struct ManifestSet {
    manifests: Vec<AgentManifest>,
}

impl ManifestSet {
    /// The first manifest that governs `current_command`, if any. First-match
    /// wins; with the bundled set every `command` list is disjoint, and a user
    /// override *replaces* (not appends) its bundled peer, so there is at most
    /// one match in practice.
    #[must_use]
    pub fn manifest_for_command(&self, current_command: &str) -> Option<&AgentManifest> {
        self.manifests
            .iter()
            .find(|m| m.matches_command(current_command))
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.manifests.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.manifests.len()
    }

    /// Names of the loaded manifests, for startup logging.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.manifests.iter().map(|m| m.name.as_str())
    }
}

/// Compile one raw regex list into a [`RegexSet`], tagging the category on
/// failure so the log line points at the offending `[rules]` key.
fn compile(patterns: &[String], category: &'static str) -> Result<RegexSet, ManifestError> {
    RegexSet::new(patterns).map_err(|source| ManifestError::Regex { category, source })
}

/// Parse and compile one manifest from TOML text. Returns an error (never
/// panics) for malformed TOML, an empty name/command, or an uncompilable regex.
pub fn parse_manifest(toml_str: &str) -> Result<AgentManifest, ManifestError> {
    let raw: RawManifest = toml::from_str(toml_str)?;
    if raw.agent.name.trim().is_empty() {
        return Err(ManifestError::EmptyName);
    }
    let commands: Vec<String> = raw
        .agent
        .command
        .iter()
        .map(|c| command_basename(c))
        .filter(|c| !c.is_empty())
        .collect();
    if commands.is_empty() {
        return Err(ManifestError::EmptyCommand);
    }
    Ok(AgentManifest {
        name: raw.agent.name.trim().to_owned(),
        commands,
        blocked: compile(&raw.rules.blocked, "blocked")?,
        working: compile(&raw.rules.working, "working")?,
        idle: compile(&raw.rules.idle, "idle")?,
    })
}

/// Lowercase command basename: `/usr/local/bin/Cursor-Agent` → `cursor-agent`.
/// Delegates the basename extraction to [`crate::discovery::command_name`] (the
/// same rule `discovery::classify_command` uses) and only adds the lowercasing
/// the manifest matcher wants, so the screen matcher and `classify_command`
/// agree on "the command" by construction.
fn command_basename(cmd: &str) -> String {
    crate::discovery::command_name(cmd).to_ascii_lowercase()
}

/// The bundled manifest sources, shipped in the binary via `include_str!`.
/// These are muxa-authored and MUST parse — a parse failure is a build-time
/// bug caught by [`tests::every_bundled_manifest_parses`].
fn bundled_sources() -> [(&'static str, &'static str); 6] {
    [
        ("agy", include_str!("screen/agents/agy.toml")),
        ("cursor", include_str!("screen/agents/cursor.toml")),
        ("amp", include_str!("screen/agents/amp.toml")),
        ("copilot", include_str!("screen/agents/copilot.toml")),
        ("aider", include_str!("screen/agents/aider.toml")),
        ("goose", include_str!("screen/agents/goose.toml")),
    ]
}

/// The bundled manifests, parsed. Panics with a pointed message if a *bundled*
/// manifest fails to parse (that's a muxa bug, not a user error).
#[must_use]
pub fn bundled_manifests() -> Vec<AgentManifest> {
    bundled_sources()
        .into_iter()
        .map(|(file, src)| {
            parse_manifest(src)
                .unwrap_or_else(|e| panic!("bundled manifest {file}.toml failed to parse: {e}"))
        })
        .collect()
}

/// `$XDG_CONFIG_HOME/muxa/agents`, falling back to the platform config dir's
/// `muxa/agents`. `XDG_CONFIG_HOME` is honored FIRST and explicitly so the path
/// documented in `docs/SCREEN_DETECTION.md` works cross-platform — on macOS
/// `dirs::config_dir()` returns `~/Library/Application Support` and ignores
/// `XDG_CONFIG_HOME`, which would otherwise silently miss user overrides.
fn user_agents_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("muxa").join("agents"));
        }
    }
    dirs::config_dir().map(|d| d.join("muxa").join("agents"))
}

/// Load every manifest: the bundled set, then user overrides from
/// `$XDG_CONFIG_HOME/muxa/agents/*.toml`. A user file whose `[agent].name`
/// matches a bundled manifest **replaces** it; a new name is appended. Parse
/// errors are logged (`warn`) and the file skipped — never fatal.
#[must_use]
pub fn load_manifests() -> ManifestSet {
    let mut manifests = bundled_manifests();
    if let Some(dir) = user_agents_dir() {
        load_user_overrides(&dir, &mut manifests);
    }
    ManifestSet { manifests }
}

/// Read `*.toml` files from `dir` and fold them into `manifests`, replacing any
/// bundled manifest of the same name. Deterministic order (files sorted) so a
/// later-sorted file with a duplicate name wins predictably. Missing dir is a
/// no-op; unreadable/malformed files warn and are skipped.
fn load_user_overrides(dir: &std::path::Path, manifests: &mut Vec<AgentManifest>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        // Missing/unreadable directory is the common case (no overrides) — not
        // worth a warning.
        return;
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("toml"))
        })
        .collect();
    files.sort();
    for path in files {
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(file = %path.display(), error = %e, "screen manifest: read failed, skipping");
                continue;
            }
        };
        match parse_manifest(&text) {
            Ok(manifest) => {
                if let Some(existing) = manifests.iter_mut().find(|m| m.name == manifest.name) {
                    tracing::info!(name = %manifest.name, file = %path.display(), "screen manifest: user override replaces bundled");
                    *existing = manifest;
                } else {
                    tracing::info!(name = %manifest.name, file = %path.display(), "screen manifest: user manifest loaded");
                    manifests.push(manifest);
                }
            }
            Err(e) => {
                tracing::warn!(file = %path.display(), error = %e, "screen manifest: parse failed, skipping");
            }
        }
    }
}

/// Prepare a raw pane capture for classification: strip ANSI escape sequences
/// (so color codes don't defeat the regexes) and keep only the bottom
/// `max_lines` lines (the active prompt/spinner region, and a bound on regex
/// work). Spinner glyphs and box-drawing characters are Unicode, not ANSI, so
/// they survive the strip.
#[must_use]
pub fn prepare_capture(raw: &str, max_lines: usize) -> String {
    let stripped = strip_ansi(raw);
    let lines: Vec<&str> = stripped.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

/// Remove the ANSI control sequences a `tmux capture-pane -e` emits: CSI
/// (`ESC [ … final`), OSC (`ESC ] … BEL`), and stray two-byte escapes. Not a
/// full terminal parser — just enough that the text rules match on the visible
/// characters.
fn strip_ansi(s: &str) -> String {
    static ANSI: OnceLock<Regex> = OnceLock::new();
    let re = ANSI.get_or_init(|| {
        // CSI: ESC [ params intermediates final | OSC: ESC ] ... BEL |
        // two-byte: ESC <single char>.
        Regex::new(r"\x1b\[[0-9;:?]*[ -/]*[@-~]|\x1b\][^\x07]*\x07|\x1b[@-Z\\-_]")
            .expect("static ANSI strip regex is valid")
    });
    re.replace_all(s, "").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor() -> AgentManifest {
        parse_manifest(include_str!("screen/agents/cursor.toml")).unwrap()
    }

    // --- parsing ------------------------------------------------------------

    #[test]
    fn parses_a_good_manifest() {
        let m = parse_manifest(
            r#"
[agent]
name = "demo"
command = ["/usr/bin/Demo-Agent", "demo2"]
[rules]
blocked = ['\[y/n\]']
working = ['thinking']
idle = ['^> $']
"#,
        )
        .expect("valid manifest parses");
        assert_eq!(m.name, "demo");
        // command basenames are lowercased.
        assert!(m.matches_command("Demo-Agent"));
        assert!(m.matches_command("/opt/x/demo-agent"));
        assert!(m.matches_command("demo2"));
        assert!(!m.matches_command("other"));
    }

    #[test]
    fn empty_name_is_rejected() {
        let err = parse_manifest("[agent]\nname = \"\"\ncommand = [\"x\"]\n").unwrap_err();
        assert!(matches!(err, ManifestError::EmptyName));
    }

    #[test]
    fn empty_command_is_rejected() {
        let err = parse_manifest("[agent]\nname = \"x\"\ncommand = []\n").unwrap_err();
        assert!(matches!(err, ManifestError::EmptyCommand));
    }

    #[test]
    fn bad_toml_is_rejected() {
        let err = parse_manifest("this is not toml {{{").unwrap_err();
        assert!(matches!(err, ManifestError::Toml(_)));
    }

    #[test]
    fn bad_regex_is_rejected_with_category() {
        let err = parse_manifest(
            "[agent]\nname = \"x\"\ncommand = [\"x\"]\n[rules]\nworking = ['(unclosed']\n",
        )
        .unwrap_err();
        match err {
            ManifestError::Regex { category, .. } => assert_eq!(category, "working"),
            other => panic!("expected a Regex error, got {other:?}"),
        }
    }

    #[test]
    fn missing_rules_section_is_ok_and_matches_nothing() {
        let m = parse_manifest("[agent]\nname = \"x\"\ncommand = [\"x\"]\n").unwrap();
        assert_eq!(m.classify("anything at all"), None);
    }

    // --- classifier semantics ----------------------------------------------

    #[test]
    fn blocked_wins_over_working_and_idle_strict_ordering() {
        // A screen that carries BOTH a spinner glyph and an approval prompt and
        // an idle marker must classify as Blocked — blocked is tested first.
        let m = parse_manifest(
            r#"
[agent]
name = "x"
command = ["x"]
[rules]
blocked = ['\[y/n\]']
working = ['thinking']
idle = ['^> $']
"#,
        )
        .unwrap();
        let screen = "thinking...\nProceed? [y/n]\n> ";
        assert_eq!(m.classify(screen), Some(ScreenState::Blocked));
    }

    #[test]
    fn working_wins_over_idle() {
        let m = cursor();
        // spinner glyph present with a trailing prompt-ish line.
        assert_eq!(m.classify("⠹ Generating\n"), Some(ScreenState::Working));
    }

    #[test]
    fn unknown_screen_keeps_previous_returns_none() {
        let m = cursor();
        assert_eq!(m.classify("just some ordinary output text\n"), None);
    }

    #[test]
    fn idle_matches_bare_prompt() {
        let m = cursor();
        assert_eq!(
            m.classify("some earlier output\n> "),
            Some(ScreenState::Idle)
        );
    }

    // --- per-bundled-agent fixtures ----------------------------------------

    #[test]
    fn every_bundled_manifest_parses() {
        let set = bundled_manifests();
        assert_eq!(set.len(), 6);
        for m in &set {
            assert!(!m.name.is_empty());
        }
    }

    #[test]
    fn cursor_fixtures() {
        let m = cursor();
        assert_eq!(
            m.classify("⠋ Thinking\nesc to interrupt"),
            Some(ScreenState::Working)
        );
        assert_eq!(
            m.classify("Do you want to allow this command? [y/n]"),
            Some(ScreenState::Blocked)
        );
        assert_eq!(m.classify("\n> "), Some(ScreenState::Idle));
    }

    /// Fixtures captured from a live agy 1.1.17 pane (working/idle) and from
    /// agy's own confirmation-widget labels (blocked).
    #[test]
    fn agy_fixtures() {
        let m = parse_manifest(include_str!("screen/agents/agy.toml")).unwrap();
        assert!(m.matches_command("agy"));
        assert!(m.matches_command("/Users/x/.local/bin/agy"));

        // Verbatim tails of real captures, three seconds apart in one turn.
        assert_eq!(
            m.classify("> Run the shell command: echo hi\n⡿  Generating...\n>\nesc to cancel"),
            Some(ScreenState::Working),
        );
        assert_eq!(
            m.classify(
                "● Bash(echo hi) (ctrl+o to expand)\n⣷  Running command...\n>\nesc to cancel"
            ),
            Some(ScreenState::Working),
        );

        // The idle footer, and the bare prompt on its own.
        assert_eq!(
            m.classify("? for shortcuts                       Gemini 3.7 Flash · high"),
            Some(ScreenState::Idle),
        );
        assert_eq!(m.classify("\n> "), Some(ScreenState::Idle));

        // `working` is tested before `idle`, so the bare `>` input line that
        // agy keeps drawing mid-turn cannot flip a busy pane to idle.
        assert_eq!(
            m.classify("⣻  Listing directory...\n>\nesc to cancel"),
            Some(ScreenState::Working),
            "the input line is drawn during generation too",
        );

        // agy's permission widget.
        assert_eq!(
            m.classify("Run command?\n> Yes, and always allow for commands that start with 'echo'\n  No, deny"),
            Some(ScreenState::Blocked),
        );
        assert_eq!(
            m.classify("Allow access to this file?\n> Yes, allow access\n  No, deny access"),
            Some(ScreenState::Blocked),
        );
        // The folder-trust gate blocks the very first prompt of a session.
        assert_eq!(
            m.classify(
                "Do you trust the contents of this project?\n> Yes, I trust this folder\n  No, exit"
            ),
            Some(ScreenState::Blocked),
        );

        // agy echoes tool output into the same pane, so prose that merely
        // talks about allowing or generating must classify as nothing.
        assert_eq!(
            m.classify("These flags allow access to the cache and deny writes."),
            None,
            "prose about allow/deny must not read as an approval prompt",
        );
        assert_eq!(
            m.classify("Generating the report is handled by the nightly job."),
            None,
            "`Generating` without agy's `...` suffix is prose, not a spinner",
        );
        assert_eq!(
            m.classify("nothing to commit, working tree clean"),
            None,
            "the word `working` in prose must not read as busy",
        );
    }

    #[test]
    fn amp_fixtures() {
        let m = parse_manifest(include_str!("screen/agents/amp.toml")).unwrap();
        assert!(m.matches_command("amp"));
        // A spinner glyph is busy; the bare word "working" alone is NOT (it
        // shows up in ordinary output like "working tree clean").
        assert_eq!(m.classify("⠸ generating"), Some(ScreenState::Working));
        assert_eq!(
            m.classify("nothing to commit, working tree clean"),
            None,
            "the word `working` in prose must not read as busy",
        );
        assert_eq!(
            m.classify("Allow this command? (y/n)"),
            Some(ScreenState::Blocked)
        );
        assert_eq!(m.classify("> "), Some(ScreenState::Idle));
    }

    #[test]
    fn copilot_fixtures() {
        let m = parse_manifest(include_str!("screen/agents/copilot.toml")).unwrap();
        assert!(m.matches_command("copilot"));
        assert_eq!(m.classify("⠴ generating"), Some(ScreenState::Working));
        assert_eq!(
            m.classify("working tree clean"),
            None,
            "the word `working` in prose must not read as busy",
        );
        assert_eq!(
            m.classify("Do you want to run this? [y/n]"),
            Some(ScreenState::Blocked)
        );
        // A real yes/no selection widget (highlighted `❯ Yes` row) reads as
        // blocked...
        assert_eq!(
            m.classify("  Run this command?\n❯ Yes\n  No"),
            Some(ScreenState::Blocked),
        );
        // ...but an ordinary sentence containing yes / no / ? does NOT (the old
        // `\byes\b.*\bno\b.*\?` pattern wrongly matched this).
        assert_eq!(
            m.classify("Yes, we can do that, but no rush — right?"),
            None,
            "prose with yes/no/? must not read as an approval prompt",
        );
    }

    #[test]
    fn aider_fixtures() {
        let m = parse_manifest(include_str!("screen/agents/aider.toml")).unwrap();
        assert!(m.matches_command("aider"));
        // Aider's distinctive (Y)es/(N)o confirm.
        assert_eq!(
            m.classify("Add main.py to the chat? (Y)es/(N)o [Yes]:"),
            Some(ScreenState::Blocked)
        );
        assert_eq!(m.classify("architect> "), Some(ScreenState::Idle));
    }

    #[test]
    fn goose_fixtures() {
        let m = parse_manifest(include_str!("screen/agents/goose.toml")).unwrap();
        assert!(m.matches_command("goose"));
        assert_eq!(m.classify("⠧ thinking"), Some(ScreenState::Working));
        assert_eq!(
            m.classify("working tree clean"),
            None,
            "the word `working` in prose must not read as busy",
        );
        assert_eq!(
            m.classify("Goose would like to call the shell tool. Allow? [y/n]"),
            Some(ScreenState::Blocked)
        );
        // The literal `Allow?` affordance on its own reads as blocked...
        assert_eq!(m.classify("Allow?"), Some(ScreenState::Blocked));
        // ...but the bare word "allow" in prose does NOT (the old pattern made
        // every token after "allow" optional, matching a lone "allow ").
        assert_eq!(
            m.classify("These settings allow faster incremental rebuilds."),
            None,
            "the word `allow` in prose must not read as an approval prompt",
        );
    }

    // --- capture preparation -----------------------------------------------

    #[test]
    fn strip_ansi_removes_color_codes_keeps_glyphs() {
        let raw = "\x1b[1;32m⠋ Thinking\x1b[0m\x1b]0;title\x07";
        let out = strip_ansi(raw);
        assert_eq!(out, "⠋ Thinking");
    }

    #[test]
    fn prepare_capture_keeps_only_the_tail() {
        use std::fmt::Write as _;
        let mut raw = String::new();
        for i in 0..100 {
            let _ = writeln!(raw, "line {i}");
        }
        let out = prepare_capture(&raw, 5);
        assert_eq!(out.lines().count(), 5);
        assert!(out.starts_with("line 95"));
        assert!(out.ends_with("line 99"));
    }

    #[test]
    fn prepare_capture_classifies_through_ansi() {
        // A colored spinner line must still classify as Working after prep.
        let m = cursor();
        let raw = "\x1b[2K\x1b[36m⠙ Generating…\x1b[0m\n";
        let prepared = prepare_capture(raw, 40);
        assert_eq!(m.classify(&prepared), Some(ScreenState::Working));
    }

    // --- loading / overrides -----------------------------------------------

    #[test]
    fn user_override_replaces_bundled_by_name() {
        let mut manifests = bundled_manifests();
        let before = manifests.len();
        let override_src = r#"
[agent]
name = "cursor"
command = ["cursor-agent", "cursor"]
[rules]
working = ['CUSTOM-SPINNER']
"#;
        // Simulate load_user_overrides' replace-by-name step directly.
        let parsed = parse_manifest(override_src).unwrap();
        if let Some(existing) = manifests.iter_mut().find(|m| m.name == parsed.name) {
            *existing = parsed;
        } else {
            manifests.push(parsed);
        }
        assert_eq!(manifests.len(), before, "override replaces, not appends");
        let set = ManifestSet { manifests };
        let cursor = set.manifest_for_command("cursor-agent").unwrap();
        assert_eq!(
            cursor.classify("CUSTOM-SPINNER"),
            Some(ScreenState::Working)
        );
        // The bundled cursor spinner glyphs are gone (replaced wholesale).
        assert_eq!(cursor.classify("⠋ Thinking"), None);
    }

    #[test]
    fn load_user_overrides_from_a_temp_dir() {
        let dir = std::env::temp_dir().join(format!("muxa-screen-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A valid new agent + a malformed file that must be skipped, not fatal.
        std::fs::write(
            dir.join("newbie.toml"),
            "[agent]\nname = \"newbie\"\ncommand = [\"newbie\"]\n[rules]\nworking = ['spin']\n",
        )
        .unwrap();
        std::fs::write(dir.join("broken.toml"), "not valid ][ toml").unwrap();

        let mut manifests = bundled_manifests();
        let before = manifests.len();
        load_user_overrides(&dir, &mut manifests);
        assert_eq!(manifests.len(), before + 1, "one new agent, broken skipped");
        assert!(manifests.iter().any(|m| m.name == "newbie"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_for_command_matches_basename() {
        let set = ManifestSet {
            manifests: bundled_manifests(),
        };
        assert_eq!(
            set.manifest_for_command("/usr/local/bin/cursor-agent")
                .map(|m| m.name.as_str()),
            Some("cursor"),
        );
        assert!(set.manifest_for_command("bash").is_none());
        assert!(set.manifest_for_command("claude").is_none());
    }
}
