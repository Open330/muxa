//! Structured decision context: what a person is asked to decide, the
//! choices, the asker's recommendation and the evidence behind it.
//!
//! Review items and human requests carry the same [`Decision`], so a client
//! can render one card for either and the person can decide from the card
//! instead of reading a pane. Every field is optional and every string is
//! declared by the asker: tend records it, it never derives or verifies it.
//! Evidence is a typed pointer (a commit, a PR, a test run); it is printed,
//! not trusted.

use serde::{Deserialize, Serialize};

/// Daemons that store [`Decision`] on review items.
pub const REVIEW_DECISION_CAPABILITY: &str = "review_decision_v1";
/// Daemons that store [`Decision`] on human requests.
pub const HUMAN_DECISION_CAPABILITY: &str = "collaboration_human_decision_v1";

/// Upper bound on [`Decision::options`].
pub const MAX_OPTIONS: usize = 8;
/// Upper bound on [`Decision::evidence`].
pub const MAX_EVIDENCE: usize = 16;
const MAX_NEEDED_BYTES: usize = 1024;
const MAX_OPTION_BYTES: usize = 512;
const MAX_RECOMMENDATION_BYTES: usize = 1024;
const MAX_LOCATOR_BYTES: usize = 2048;
/// Upper bound on a test result tail, after [`clean_tail`].
pub const MAX_TAIL_BYTES: usize = 2048;

/// What the asker wants decided and why. Empty fields are omitted on the
/// wire, so a record without a decision looks exactly as it did before.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    /// The question, in one or two sentences ("Merge #12 or hold for the
    /// perf run?").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needed: Option<String>,
    /// The choices on offer, one line each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    /// What the asker would do, and why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<Evidence>,
}

/// A typed pointer to something the person can open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Evidence {
    /// A git commit, optionally with the checkout it lives in.
    Commit {
        sha: String,
        /// Absolute path of a git checkout that has the commit.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<String>,
    },
    /// A pull or merge request URL.
    PullRequest { url: String },
    /// A git revision range (`main..feat/x`, `abc123^..abc123`).
    Diff {
        range: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<String>,
    },
    /// A test command and, when known, how it ended.
    Test {
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        passed: Option<bool>,
        /// The last lines of its output, cleaned by [`clean_tail`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tail: Option<String>,
    },
    /// A file or directory.
    Path { path: String },
}

impl Decision {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.needed.is_none()
            && self.options.is_empty()
            && self.recommendation.is_none()
            && self.evidence.is_empty()
    }

    /// `None` for an empty decision, so callers store nothing rather than
    /// an object with no fields.
    #[must_use]
    pub fn into_option(self) -> Option<Self> {
        (!self.is_empty()).then_some(self)
    }

    /// Refuse what a terminal should never print and what a client should
    /// never have to page through. The message names the offending field.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(needed) = &self.needed {
            check("decision_needed", needed, MAX_NEEDED_BYTES, Text::Prose)?;
        }
        if self.options.len() > MAX_OPTIONS {
            return Err(format!("at most {MAX_OPTIONS} options"));
        }
        for option in &self.options {
            check("option", option, MAX_OPTION_BYTES, Text::Line)?;
        }
        if let Some(recommendation) = &self.recommendation {
            check(
                "recommendation",
                recommendation,
                MAX_RECOMMENDATION_BYTES,
                Text::Prose,
            )?;
        }
        if self.evidence.len() > MAX_EVIDENCE {
            return Err(format!("at most {MAX_EVIDENCE} evidence entries"));
        }
        self.evidence.iter().try_for_each(Evidence::validate)
    }
}

impl Evidence {
    fn validate(&self) -> Result<(), String> {
        match self {
            Self::Commit { sha, repo } => {
                if !(4..=64).contains(&sha.len()) || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Err(format!(
                        "commit {sha:?} must be 4-64 hexadecimal characters"
                    ));
                }
                repo.as_deref().map_or(Ok(()), check_repo)
            }
            Self::PullRequest { url } => {
                check("pull request url", url, MAX_LOCATOR_BYTES, Text::Line)?;
                if !(url.starts_with("https://") || url.starts_with("http://")) {
                    return Err("pull request url must start with https://".into());
                }
                Ok(())
            }
            Self::Diff { range, repo } => {
                check("diff range", range, MAX_LOCATOR_BYTES, Text::Line)?;
                // A range is later handed to git as one argument; never let
                // it read as an option, and keep it to revision syntax.
                let revision = |c: char| c.is_ascii_alphanumeric() || "._/~^@{}-".contains(c);
                if range.starts_with('-') || !range.chars().all(revision) {
                    return Err(format!("diff range {range:?} is not a git revision range"));
                }
                repo.as_deref().map_or(Ok(()), check_repo)
            }
            Self::Test {
                command,
                passed: _,
                tail,
            } => {
                check("test command", command, MAX_LOCATOR_BYTES, Text::Line)?;
                tail.as_deref().map_or(Ok(()), |t| {
                    check("test tail", t, MAX_TAIL_BYTES, Text::Prose)
                })
            }
            Self::Path { path } => check("path", path, MAX_LOCATOR_BYTES, Text::Line),
        }
    }

    /// Parse the CLI shorthand: `commit:SHA`, `pr:URL` (or a bare
    /// `https://` URL), `diff:RANGE`, `test:COMMAND`, `test-pass:COMMAND`,
    /// `test-fail:COMMAND`, `path:PATH`. `repo` fills commit and diff
    /// entries. The result is validated.
    pub fn parse(spec: &str, repo: Option<&str>) -> Result<Self, String> {
        let repo = repo.map(str::to_owned);
        let evidence = if spec.starts_with("https://") || spec.starts_with("http://") {
            Self::PullRequest { url: spec.into() }
        } else {
            let Some((kind, value)) = spec.split_once(':') else {
                return Err(format!(
                    "evidence {spec:?} needs a kind: commit:, pr:, diff:, test:, test-pass:, test-fail: or path:"
                ));
            };
            let value = value.trim().to_owned();
            match kind.trim() {
                "commit" => Self::Commit { sha: value, repo },
                "pr" => Self::PullRequest { url: value },
                "diff" => Self::Diff { range: value, repo },
                "test" | "test-pass" | "test-fail" => Self::Test {
                    command: value,
                    passed: match kind.trim() {
                        "test-pass" => Some(true),
                        "test-fail" => Some(false),
                        _ => None,
                    },
                    tail: None,
                },
                "path" => Self::Path { path: value },
                other => return Err(format!("unknown evidence kind {other:?}")),
            }
        };
        evidence.validate()?;
        Ok(evidence)
    }

    /// Read a review anchor (`commit:SHA`, a URL) as evidence when it
    /// parses as one. Anchors predate typed evidence and stay free-form.
    #[must_use]
    pub fn from_anchor(anchor: &str) -> Option<Self> {
        Self::parse(anchor.trim(), None).ok()
    }

    /// One line for terminal output.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Commit { sha, repo } => match repo {
                Some(repo) => format!("commit {sha} in {repo}"),
                None => format!("commit {sha}"),
            },
            Self::PullRequest { url } => format!("pr {url}"),
            Self::Diff { range, repo } => match repo {
                Some(repo) => format!("diff {range} in {repo}"),
                None => format!("diff {range}"),
            },
            Self::Test {
                command, passed, ..
            } => match passed {
                Some(true) => format!("test passed: {command}"),
                Some(false) => format!("test FAILED: {command}"),
                None => format!("test: {command}"),
            },
            Self::Path { path } => format!("path {path}"),
        }
    }
}

fn check_repo(repo: &str) -> Result<(), String> {
    check("repo", repo, MAX_LOCATOR_BYTES, Text::Line)?;
    if !std::path::Path::new(repo).is_absolute() {
        return Err(format!("repo {repo:?} must be an absolute path"));
    }
    Ok(())
}

/// Make captured command output safe to store and print: drop ANSI escape
/// sequences and every control character but newline and tab, trim, and
/// keep the last [`MAX_TAIL_BYTES`] bytes on a line boundary when possible.
#[must_use]
pub fn clean_tail(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(MAX_TAIL_BYTES * 2));
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // CSI (`ESC [ … final`) and OSC (`ESC ] … BEL|ST`) sequences;
            // any other escape drops just the next character.
            match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' || (c == '\u{1b}' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        if c == '\r' {
            continue;
        }
        if !c.is_control() || c == '\n' || c == '\t' {
            out.push(c);
        }
    }
    let trimmed = out.trim();
    if trimmed.len() <= MAX_TAIL_BYTES {
        return trimmed.to_owned();
    }
    let mut start = trimmed.len() - MAX_TAIL_BYTES;
    while !trimmed.is_char_boundary(start) {
        start += 1;
    }
    let tail = &trimmed[start..];
    // Prefer starting on a whole line.
    match tail.find('\n') {
        Some(newline) if newline + 1 < tail.len() => tail[newline + 1..].to_owned(),
        _ => tail.to_owned(),
    }
}

/// What a text field may contain besides printable characters.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Text {
    /// Identifiers, labels, locators: no control characters at all.
    Line,
    /// Summaries and notes: newlines and tabs allowed, nothing else.
    Prose,
}

/// Every stored string is printed by the CLI and status surfaces, so control
/// characters (terminal escapes, NUL) are refused at the boundary.
pub(crate) fn check(name: &str, value: &str, max: usize, text: Text) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{name} must not be blank"));
    }
    if value.len() > max {
        return Err(format!("{name} exceeds {max} bytes"));
    }
    let allowed = |c: char| text == Text::Prose && (c == '\n' || c == '\t');
    if value.chars().any(|c| c.is_control() && !allowed(c)) {
        return Err(format!("{name} must not contain control characters"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shorthand_parses_each_kind() {
        let repo = Some("/src/tend");
        assert_eq!(
            Evidence::parse("commit:1a2b3c", repo).unwrap(),
            Evidence::Commit {
                sha: "1a2b3c".into(),
                repo: Some("/src/tend".into())
            }
        );
        assert_eq!(
            Evidence::parse("https://github.com/o/r/pull/1", None).unwrap(),
            Evidence::PullRequest {
                url: "https://github.com/o/r/pull/1".into()
            }
        );
        assert!(matches!(
            Evidence::parse("test-fail:cargo test -p tend", None).unwrap(),
            Evidence::Test {
                passed: Some(false),
                ..
            }
        ));
        assert!(matches!(
            Evidence::parse("diff:main..feat/x", None).unwrap(),
            Evidence::Diff { .. }
        ));
        assert!(matches!(
            Evidence::parse("path:docs/REVIEW.md", None).unwrap(),
            Evidence::Path { .. }
        ));
    }

    #[test]
    fn shorthand_refuses_what_git_or_a_terminal_would_misread() {
        assert!(Evidence::parse("commit:--exec=sh", None).is_err());
        assert!(Evidence::parse("commit:xyz", None).is_err());
        assert!(Evidence::parse("diff:--output=/tmp/x", None).is_err());
        assert!(Evidence::parse("diff:a..b;rm", None).is_err());
        assert!(Evidence::parse("pr:file:///etc/passwd", None).is_err());
        assert!(Evidence::parse("commit:abcd", Some("relative/repo")).is_err());
        assert!(Evidence::parse("path:a\u{1b}[31mb", None).is_err());
        assert!(Evidence::parse("nokind", None).is_err());
        assert!(Evidence::parse("blob:abc", None).is_err());
    }

    #[test]
    fn decision_bounds_are_enforced() {
        let mut decision = Decision {
            options: vec!["a".into(); MAX_OPTIONS + 1],
            ..Decision::default()
        };
        assert!(decision.validate().is_err());
        decision.options.truncate(2);
        assert!(decision.validate().is_ok());
        decision.recommendation = Some("take a\n\nbecause".into());
        assert!(decision.validate().is_ok());
        decision.options[0] = "two\nlines".into();
        assert!(decision.validate().is_err());
    }

    #[test]
    fn empty_decision_is_omitted() {
        assert!(Decision::default().into_option().is_none());
        assert_eq!(serde_json::to_string(&Decision::default()).unwrap(), "{}");
    }

    #[test]
    fn tail_drops_escapes_and_keeps_the_end() {
        let raw = "\u{1b}[32mok\u{1b}[0m 3 passed\r\n\u{1b}]0;title\u{7}done\u{0}\n";
        assert_eq!(clean_tail(raw), "ok 3 passed\ndone");
        let long = (0..1000)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tail = clean_tail(&long);
        assert!(tail.len() <= MAX_TAIL_BYTES);
        assert!(tail.ends_with("line 999"));
        assert!(
            tail.starts_with("line "),
            "starts on a whole line: {tail:.20}"
        );
        assert!(check("tail", &tail, MAX_TAIL_BYTES, Text::Prose).is_ok());
    }
}
