//! Which conversation a running agent process is on, read from evidence that
//! names that exact process rather than taken from the registry.
//!
//! Claude Code writes `<config dir>/sessions/<pid>.json` for each running
//! interactive session: its pid, the conversation (`sessionId`) and
//! `procStart`, the process's start time. On Linux `procStart` is field 22 of
//! `/proc/<pid>/stat` (clock ticks since boot) written as a decimal string,
//! so a file left behind by an exited process whose pid was reused does not
//! match the new one. Elsewhere the start time is not compared and the
//! conversation stays unknown: a pid alone is not an identity.
//!
//! A swap stops and relaunches the agent, so a guess is worse than nothing:
//! an unreadable process, a missing file and files that disagree all come
//! back as something other than [`Conversation::Verified`].

use std::path::{Path, PathBuf};

/// A session file is a few hundred bytes; anything larger is not one.
const MAX_SESSION_FILE: u64 = 64 * 1024;
/// Only `CLAUDE_CONFIG_DIR` is wanted out of a process environment.
const MAX_ENVIRON: u64 = 1024 * 1024;

/// One `<config dir>/sessions/<pid>.json`, as far as identity goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeSessionFile {
    pub pid: u32,
    pub session_id: String,
    /// Kept as written: compared as a string, never converted.
    pub proc_start: String,
}

/// What the evidence says about the conversation a process is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conversation {
    /// Every file naming this exact process agrees on this id.
    Verified(String),
    /// Files naming this exact process name different ids.
    Conflict(Vec<String>),
    /// Nothing ties a conversation to this process; the reason says why.
    Unknown(String),
    /// The process was seen to change while it was being read. Unlike
    /// [`Conversation::Unknown`] this is evidence against the plan, never a
    /// reason to fall back to the registry.
    Unstable(String),
}

/// Read a session file's identity fields. `None` for anything malformed.
#[must_use]
pub fn parse_claude_session(bytes: &[u8]) -> Option<ClaudeSessionFile> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let pid = u32::try_from(value.get("pid")?.as_u64()?).ok()?;
    let session_id = value.get("sessionId")?.as_str()?;
    uuid::Uuid::parse_str(session_id).ok()?;
    let proc_start = match value.get("procStart")? {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Number(number) => number.to_string(),
        _ => return None,
    };
    Some(ClaudeSessionFile {
        pid,
        session_id: session_id.to_owned(),
        proc_start,
    })
}

/// The conversation `pid`, started at `start`, is on according to `files`.
/// Only files for that pid *and* that start time count; mirrored copies of
/// one file (account profiles sharing a directory) are one answer.
#[must_use]
pub fn claude_conversation_from(
    pid: u32,
    start: Option<&str>,
    files: &[ClaudeSessionFile],
) -> Conversation {
    let Some(start) = start else {
        return Conversation::Unknown("the process start time cannot be read here".into());
    };
    let mut ids: Vec<&str> = files
        .iter()
        .filter(|file| file.pid == pid && file.proc_start == start)
        .map(|file| file.session_id.as_str())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    match ids.as_slice() {
        [] => Conversation::Unknown("no session file names this process".into()),
        [id] => Conversation::Verified((*id).to_owned()),
        _ => Conversation::Conflict(ids.into_iter().map(str::to_owned).collect()),
    }
}

/// A process's start time in the form Claude writes as `procStart`, or
/// `None` where that form is not known.
#[must_use]
pub fn process_start(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        linux_start_ticks(&stat)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// Field 22 of a `/proc/<pid>/stat` line. The command name (field 2) may
/// hold spaces and parentheses, so fields are counted after its last `)`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_start_ticks(stat: &str) -> Option<String> {
    let rest = &stat[stat.rfind(')')? + 1..];
    // Fields 3.. follow the name; field 22 is the 20th of them.
    let ticks = rest.split_whitespace().nth(19)?;
    ticks.parse::<u64>().ok()?;
    Some(ticks.to_owned())
}

/// Where Claude may have written this process's session file: its own
/// `CLAUDE_CONFIG_DIR` first, then this process's, `~/.claude` and the
/// account profiles under `~/.config/asx/profiles`. Duplicates (symlinks,
/// the same directory twice) are listed once.
#[must_use]
pub fn claude_roots(pid: u32) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    roots.extend(process_env(pid, "CLAUDE_CONFIG_DIR").map(PathBuf::from));
    roots.extend(std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from));
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join(".claude"));
        if let Ok(profiles) = std::fs::read_dir(home.join(".config/asx/profiles")) {
            let mut found: Vec<PathBuf> = profiles.flatten().map(|entry| entry.path()).collect();
            found.sort();
            roots.extend(found);
        }
    }
    let mut seen = Vec::new();
    roots.retain(|root| {
        let key = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        if seen.contains(&key) {
            false
        } else {
            seen.push(key);
            true
        }
    });
    roots
}

/// Every readable `sessions/<pid>.json` under `roots`. Missing, oversized
/// and malformed files are skipped.
#[must_use]
pub fn read_claude_sessions(pid: u32, roots: &[PathBuf]) -> Vec<ClaudeSessionFile> {
    roots
        .iter()
        .filter_map(|root| {
            read_bounded(
                &root.join("sessions").join(format!("{pid}.json")),
                MAX_SESSION_FILE,
            )
        })
        .filter_map(|bytes| parse_claude_session(&bytes))
        .collect()
}

/// The conversation the running Claude process `pid` is on. The start time
/// is read again after the files, so a process that exited (and a pid that
/// was reused) in between is not taken for the one that was read about.
#[must_use]
pub fn claude_conversation(pid: u32) -> Conversation {
    let before = process_start(pid);
    let files = read_claude_sessions(pid, &claude_roots(pid));
    let answer = claude_conversation_from(pid, before.as_deref(), &files);
    if before.is_some() && process_start(pid) != before {
        return Conversation::Unstable("the process changed while it was being read".into());
    }
    answer
}

/// One variable out of a same-user process's environment. Never logged or
/// kept; `None` when unreadable.
fn process_env(pid: u32, name: &str) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let bytes = read_bounded(Path::new(&format!("/proc/{pid}/environ")), MAX_ENVIRON)?;
        let prefix = format!("{name}=");
        bytes
            .split(|byte| *byte == 0)
            .filter_map(|entry| std::str::from_utf8(entry).ok())
            .find_map(|entry| entry.strip_prefix(&prefix))
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (pid, name);
        None
    }
}

fn read_bounded(path: &Path, limit: u64) -> Option<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= limit).then_some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "96f5e88c-62df-42a7-914e-e206355969fb";
    const B: &str = "01a18182-0bf0-7801-97bf-c91154132258";

    fn file(pid: u32, id: &str, start: &str) -> ClaudeSessionFile {
        ClaudeSessionFile {
            pid,
            session_id: id.into(),
            proc_start: start.into(),
        }
    }

    /// The shape Claude Code 2.1 writes, trimmed to what identity needs.
    #[test]
    fn a_claude_session_file_reads_its_identity() {
        let json = format!(
            r#"{{"pid":157653,"sessionId":"{A}","cwd":"/w","procStart":"51554631","tmux":"callabo:@232.%490","status":"idle"}}"#
        );
        assert_eq!(
            parse_claude_session(json.as_bytes()),
            Some(file(157_653, A, "51554631"))
        );
        assert_eq!(
            parse_claude_session(br#"{"pid":1,"sessionId":"x","procStart":"1"}"#),
            None
        );
        assert_eq!(parse_claude_session(br#"{"pid":1,"sessionId":"x"}"#), None);
        assert_eq!(parse_claude_session(b"not json"), None);
    }

    /// The start time read from `/proc` is the same decimal Claude writes.
    #[test]
    fn the_start_time_is_field_22_counted_after_the_command_name() {
        let stat =
            "157653 (claude (x) y) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 51554631 99";
        assert_eq!(linux_start_ticks(stat).as_deref(), Some("51554631"));
        assert_eq!(linux_start_ticks("1 (x) S 2"), None);
    }

    #[test]
    fn only_a_file_for_this_exact_process_counts() {
        let files = [file(7, A, "100"), file(7, A, "100")];
        // Mirrored copies of one file are one answer.
        assert_eq!(
            claude_conversation_from(7, Some("100"), &files),
            Conversation::Verified(A.into())
        );
        // A file left by an earlier process with the same pid is not this one.
        assert!(matches!(
            claude_conversation_from(7, Some("200"), &files),
            Conversation::Unknown(_)
        ));
        assert!(matches!(
            claude_conversation_from(8, Some("100"), &files),
            Conversation::Unknown(_)
        ));
        // Without a start time there is no identity to compare.
        assert!(matches!(
            claude_conversation_from(7, None, &files),
            Conversation::Unknown(_)
        ));
    }

    #[test]
    fn files_that_disagree_are_a_conflict_not_a_choice() {
        let files = [file(7, A, "100"), file(7, B, "100")];
        let Conversation::Conflict(ids) = claude_conversation_from(7, Some("100"), &files) else {
            panic!("two ids for one process must not resolve");
        };
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn oversized_files_are_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.json");
        std::fs::write(&path, vec![b' '; 70 * 1024]).unwrap();
        assert!(read_bounded(&path, MAX_SESSION_FILE).is_none());
        std::fs::write(&path, b"{}").unwrap();
        assert_eq!(
            read_bounded(&path, MAX_SESSION_FILE).as_deref(),
            Some(&b"{}"[..])
        );
    }

    #[test]
    fn session_files_are_found_under_each_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("profile");
        std::fs::create_dir_all(root.join("sessions")).unwrap();
        std::fs::write(
            root.join("sessions/42.json"),
            format!(r#"{{"pid":42,"sessionId":"{A}","procStart":"9"}}"#),
        )
        .unwrap();
        let found = read_claude_sessions(42, &[dir.path().join("missing"), root]);
        assert_eq!(found, vec![file(42, A, "9")]);
    }

    /// This test's own process: on Linux its start time reads, and nothing
    /// names it, so it is unknown rather than guessed.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_live_process_without_a_session_file_is_unknown() {
        let pid = std::process::id();
        assert!(process_start(pid).is_some());
        assert!(matches!(
            claude_conversation_from(pid, process_start(pid).as_deref(), &[]),
            Conversation::Unknown(_)
        ));
    }
}
