//! Make review evidence openable from `tend review show`.
//!
//! Each piece of evidence becomes the exact command that opens it: `git
//! show`/`git diff` in the recorded checkout, `gh pr view`/`gh pr diff` for a
//! pull request, the test command to rerun. tend prints them and runs none:
//! the checkout is the asker's declaration, and git reads repository config
//! that can run programs (signature verifiers, lazy fetches, diff drivers),
//! so opening it is the person's own act, in their own shell. Evidence is a
//! claim; the commands let the person check it.

use std::fmt::Write as _;
use tend::decision::Evidence;
use tend::work_control::shell_quote;

/// Every piece of evidence on `item`, the typed entries first, then the
/// free-form anchor when it reads as evidence and is not already listed.
pub(crate) fn evidence_of(item: &tend::review::ReviewItem) -> Vec<Evidence> {
    let mut evidence = item
        .decision
        .as_ref()
        .map(|decision| decision.evidence.clone())
        .unwrap_or_default();
    if let Some(anchor) = item.anchor.as_deref().and_then(Evidence::from_anchor) {
        if !evidence.contains(&anchor) {
            evidence.push(anchor);
        }
    }
    evidence
}

/// The `open:` section of `tend review show`: one block per entry with
/// the command that opens it.
pub(crate) fn render_open(evidence: &[Evidence]) -> String {
    let mut out = String::new();
    if evidence.is_empty() {
        return out;
    }
    out.push_str("\nopen:\n");
    for entry in evidence {
        let _ = writeln!(out, "  {}", entry.describe());
        for line in open(entry) {
            let _ = writeln!(out, "    {line}");
        }
    }
    out
}

fn open(evidence: &Evidence) -> Vec<String> {
    let in_checkout = |repo: &Option<String>, verb: &str, rev: &str| match repo {
        Some(repo) => vec![
            format!("$ git -C {} {verb} --stat {rev}", shell_quote(repo)),
            format!("$ git -C {} {verb} {rev}", shell_quote(repo)),
        ],
        None => vec![
            format!("$ git {verb} --stat {rev}"),
            "(no checkout recorded; run it in the repository)".into(),
        ],
    };
    match evidence {
        Evidence::Commit { sha, repo } => in_checkout(repo, "show", sha),
        Evidence::Diff { range, repo } => in_checkout(repo, "diff", range),
        Evidence::PullRequest { url } => vec![
            format!("$ gh pr view {}", shell_quote(url)),
            format!("$ gh pr diff {}", shell_quote(url)),
        ],
        // The recorded output tail is already shown under `evidence:`.
        Evidence::Test { command, .. } => vec![format!("rerun: {command}")],
        Evidence::Path { path } => {
            let state = match std::fs::symlink_metadata(path) {
                Ok(meta) if meta.is_dir() => "directory".to_owned(),
                Ok(meta) => format!("{} bytes", meta.len()),
                Err(_) => "not found from here".to_owned(),
            };
            vec![format!("{} ({state})", shell_quote(path))]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_commit_in_a_recorded_checkout_prints_commands_and_runs_nothing() {
        let out = render_open(&[
            Evidence::Commit {
                sha: "abc123".into(),
                repo: Some("/src/my repo".into()),
            },
            Evidence::Diff {
                range: "main..feat/x".into(),
                repo: Some("/src/tend".into()),
            },
        ]);
        assert!(
            out.contains("$ git -C '/src/my repo' show --stat abc123\n"),
            "{out}"
        );
        assert!(
            out.contains("$ git -C '/src/my repo' show abc123\n"),
            "{out}"
        );
        assert!(
            out.contains("$ git -C '/src/tend' diff --stat main..feat/x\n"),
            "{out}"
        );
    }

    #[test]
    fn remote_and_test_evidence_print_the_command_without_running_it() {
        let out = render_open(&[
            Evidence::PullRequest {
                url: "https://github.com/o/r/pull/1".into(),
            },
            Evidence::Test {
                command: "cargo test".into(),
                passed: Some(false),
                tail: Some("boom".into()),
                verified: false,
            },
            Evidence::Commit {
                sha: "abc123".into(),
                repo: None,
            },
        ]);
        assert!(
            out.contains("$ gh pr diff 'https://github.com/o/r/pull/1'"),
            "{out}"
        );
        assert!(out.contains("rerun: cargo test\n"), "{out}");
        assert!(
            !out.contains("boom"),
            "the tail is shown once, under evidence"
        );
        assert!(
            out.contains("$ git show --stat abc123\n    (no checkout recorded"),
            "{out}"
        );
    }

    #[test]
    fn a_commit_anchor_counts_as_evidence_once() {
        let mut item: tend::review::ReviewItem = serde_json::from_value(serde_json::json!({
            "id": "rv_1", "target": {"kind": "session", "session_id": "s"}, "generation": 1,
            "summary": "x", "anchor": "commit:abc123", "requested_at": "2026-11-12T00:00:00Z",
            "requested_by": {"kind": "agent"}
        }))
        .unwrap();
        assert_eq!(evidence_of(&item).len(), 1);
        item.decision = Some(tend::decision::Decision {
            evidence: vec![Evidence::Commit {
                sha: "abc123".into(),
                repo: None,
            }],
            ..Default::default()
        });
        assert_eq!(evidence_of(&item).len(), 1, "not listed twice");
        item.anchor = Some("see the log".into());
        assert_eq!(
            evidence_of(&item).len(),
            1,
            "free-form anchors stay free-form"
        );
    }
}
