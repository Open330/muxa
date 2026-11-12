//! Which conversation a running agent process is on, read from evidence that
//! names that exact process rather than taken from the registry.
//!
//! Claude Code writes `<config dir>/sessions/<pid>.json` for each running
//! interactive session: its pid, the conversation (`sessionId`) and
//! `procStart`, the process's start time. On Linux `procStart` is field 22 of
//! `/proc/<pid>/stat` (clock ticks since boot) written as a decimal string;
//! on macOS it is `ps -o lstart` read in UTC (`Thu Nov 12 05:42:03 2026`).
//! Either way a file left behind by an exited process whose pid was reused
//! does not match the new one. Where the start time cannot be read the
//! conversation stays unknown: a pid alone is not an identity.
//!
//! Codex keeps the rollout it appends to open for the life of the thread
//! (`<CODEX_HOME>/sessions/YYYY/MM/DD/rollout-<time>-<thread id>.jsonl`),
//! so the thread a TUI process is on is the one its own open rollout names.
//!
//! A swap stops and relaunches the agent, so a guess is worse than nothing:
//! an unreadable process, a missing file and files that disagree all come
//! back as something other than [`Conversation::Verified`].

use std::path::{Path, PathBuf};

/// A session file is a few hundred bytes; anything larger is not one.
const MAX_SESSION_FILE: u64 = 64 * 1024;
/// Only `CLAUDE_CONFIG_DIR` is wanted out of a process environment.
#[cfg(target_os = "linux")]
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
        .filter(|file| file.pid == pid && same_start(&file.proc_start, start))
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

/// A process's start time: on Linux the decimal Claude writes as
/// `procStart`, elsewhere `epoch:<unix seconds>`, which [`same_start`]
/// compares with the `lstart` text Claude writes there. `None` where it
/// cannot be read.
#[must_use]
pub fn process_start(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        linux_start_ticks(&stat)
    }
    #[cfg(not(target_os = "linux"))]
    {
        use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
        let wanted = Pid::from_u32(pid);
        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[wanted]),
            true,
            ProcessRefreshKind::nothing(),
        );
        // A process that could not be read comes back with a zero start:
        // that is no identity at all.
        system
            .process(wanted)
            .map(sysinfo::Process::start_time)
            .filter(|start| *start > 0)
            .map(|start| format!("epoch:{start}"))
    }
}

/// Whether a session file's `procStart` is the start [`process_start`] read.
/// Linux writes the same decimal both ways. On macOS Claude writes
/// `ps -o lstart` in UTC, which is compared to the second.
fn same_start(written: &str, start: &str) -> bool {
    if written == start {
        return true;
    }
    let Some(seconds) = start
        .strip_prefix("epoch:")
        .and_then(|s| s.parse::<i64>().ok())
    else {
        return false;
    };
    lstart_utc(written) == Some(seconds)
}

/// `Thu Nov 12 05:42:03 2026` (`ps -o lstart`, read in UTC) as Unix
/// seconds. A single-digit day may be padded with a space.
fn lstart_utc(text: &str) -> Option<i64> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let [_weekday, month, day, clock, year] = words.as_slice() else {
        return None;
    };
    let month = match *month {
        "Jan" => time::Month::January,
        "Feb" => time::Month::February,
        "Mar" => time::Month::March,
        "Apr" => time::Month::April,
        "May" => time::Month::May,
        "Jun" => time::Month::June,
        "Jul" => time::Month::July,
        "Aug" => time::Month::August,
        "Sep" => time::Month::September,
        "Oct" => time::Month::October,
        "Nov" => time::Month::November,
        "Dec" => time::Month::December,
        _ => return None,
    };
    let mut hms = clock.split(':').map(str::parse::<u8>);
    let (Some(Ok(hour)), Some(Ok(minute)), Some(Ok(second)), None) =
        (hms.next(), hms.next(), hms.next(), hms.next())
    else {
        return None;
    };
    let date = time::Date::from_calendar_date(year.parse().ok()?, month, day.parse().ok()?).ok()?;
    let clock = time::Time::from_hms(hour, minute, second).ok()?;
    Some(date.with_time(clock).assume_utc().unix_timestamp())
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
/// aas account profiles. Duplicates (symlinks,
/// the same directory twice) are listed once.
#[must_use]
pub fn claude_roots(pid: u32) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    roots.extend(process_env(pid, "CLAUDE_CONFIG_DIR").map(PathBuf::from));
    roots.extend(std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from));
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join(".claude"));
        // aas keeps its profiles in the platform's data directory
        // (`~/Library/Application Support` on macOS) or `~/.config`.
        let mut bases = vec![home.join(".config/asx/profiles")];
        bases.extend(dirs::data_dir().map(|data| data.join("asx/profiles")));
        for base in bases {
            if let Ok(profiles) = std::fs::read_dir(base) {
                let mut found: Vec<PathBuf> =
                    profiles.flatten().map(|entry| entry.path()).collect();
                found.sort();
                roots.extend(found);
            }
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
    let bytes = read_bounded(Path::new(&format!("/proc/{pid}/environ")), MAX_ENVIRON)?;
    #[cfg(not(target_os = "linux"))]
    let bytes = crate::accounts::process_environ_raw(pid)?;
    let prefix = format!("{name}=");
    bytes
        .split(|byte| *byte == 0)
        .filter_map(|entry| std::str::from_utf8(entry).ok())
        .find_map(|entry| entry.strip_prefix(&prefix))
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// The Codex thread a rollout path names: `rollout-<time>-<thread id>.jsonl`
/// directly under a `sessions/YYYY/MM/DD` tree. Anything else (an archived
/// rollout, another file the process has open) names none.
#[must_use]
pub fn rollout_thread_id(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    let id = stem.get(stem.len().checked_sub(36)?..)?;
    uuid::Uuid::parse_str(id).ok()?;
    // `.../sessions/2026/11/10/rollout-...`: three date levels, then `sessions`.
    let sessions = path.ancestors().nth(4)?;
    (sessions.file_name()? == "sessions").then(|| id.to_owned())
}

/// The thread a Codex process is on according to the files it has open.
/// `None`: its open files could not be listed.
#[must_use]
pub fn codex_conversation_from(open: Option<&[PathBuf]>) -> Conversation {
    let Some(open) = open else {
        return Conversation::Unknown("the process's open files cannot be read here".into());
    };
    let mut ids: Vec<String> = open
        .iter()
        .filter_map(|path| rollout_thread_id(path))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    match ids.len() {
        0 => Conversation::Unknown("the process has no rollout open".into()),
        1 => Conversation::Verified(ids.remove(0)),
        _ => Conversation::Conflict(ids),
    }
}

/// The thread the running Codex process `pid` is on, from the rollout it
/// has open. Read like [`claude_conversation`]: the start time before and
/// after, so a pid reused in between is not taken for this one.
#[must_use]
pub fn codex_conversation(pid: u32) -> Conversation {
    let before = process_start(pid);
    if before.is_none() {
        return Conversation::Unknown("the process start time cannot be read here".into());
    }
    let open = open_files(pid);
    let answer = match codex_conversation_from(open.as_deref()) {
        // The file name is the claim; the rollout's own first record has
        // to agree before it is taken as the thread.
        Conversation::Verified(id) => {
            let mut named = open
                .iter()
                .flatten()
                .filter(|path| rollout_thread_id(path).as_deref() == Some(id.as_str()));
            if named.all(|path| rollout_meta_id(path).as_deref() == Some(id.as_str())) {
                Conversation::Verified(id)
            } else {
                Conversation::Unknown("the open rollout's session record does not name it".into())
            }
        }
        other => other,
    };
    if process_start(pid) != before {
        return Conversation::Unstable("the process changed while it was being read".into());
    }
    answer
}

/// The thread id a rollout's first record (`session_meta`) names. That
/// record carries the base instructions, so it is read up to 1 MiB.
fn rollout_meta_id(path: &Path) -> Option<String> {
    use std::io::{BufRead, Read};
    const MAX_META: u64 = 1024 * 1024;
    let file = std::fs::File::open(path).ok()?;
    let mut line = String::new();
    std::io::BufReader::new(file.take(MAX_META))
        .read_line(&mut line)
        .ok()?;
    if !line.ends_with('\n') {
        return None;
    }
    let record: serde_json::Value = serde_json::from_str(&line).ok()?;
    if record.get("type")?.as_str()? != "session_meta" {
        return None;
    }
    let id = record.get("payload")?.get("id")?.as_str()?;
    uuid::Uuid::parse_str(id).ok()?;
    Some(id.to_owned())
}

/// The paths of the regular files `pid` has open, or `None` when they
/// cannot be listed.
#[cfg(target_os = "linux")]
fn open_files(pid: u32) -> Option<Vec<PathBuf>> {
    let entries = std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?;
    Some(
        entries
            .flatten()
            .filter_map(|entry| std::fs::read_link(entry.path()).ok())
            .filter(|path| path.is_absolute())
            .collect(),
    )
}

/// The paths of the regular files `pid` has open, from `lsof`, or `None`
/// when they cannot be listed in time.
#[cfg(all(unix, not(target_os = "linux")))]
fn open_files(pid: u32) -> Option<Vec<PathBuf>> {
    use std::process::{Command, Stdio};
    const LSOF_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
    let mut child = Command::new("lsof")
        // -b/-w: never block on a kernel call or warn; -a: AND the
        // selections; -Fn: one `n<path>` line per file.
        .args([
            "-b",
            "-w",
            "-n",
            "-P",
            "-a",
            "-p",
            &pid.to_string(),
            "-d",
            "^txt",
            "-Fn",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Drained on its own thread so a long listing never fills the pipe
    // and stalls `lsof` until the deadline.
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut stdout, &mut text).map(|_| text)
    });
    let deadline = std::time::Instant::now() + LSOF_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let text = reader.join().ok()?.ok()?;
    Some(parse_lsof_names(&text))
}

#[cfg(windows)]
fn open_files(_pid: u32) -> Option<Vec<PathBuf>> {
    None
}

/// The `n<path>` lines of `lsof -Fn` output.
#[cfg_attr(not(all(unix, not(target_os = "linux"))), allow(dead_code))]
fn parse_lsof_names(text: &str) -> Vec<PathBuf> {
    text.lines()
        .filter_map(|line| line.strip_prefix('n'))
        .filter(|path| path.starts_with('/'))
        .map(PathBuf::from)
        .collect()
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
            r#"{{"pid":157653,"sessionId":"{A}","cwd":"/w","procStart":"51554631","tmux":"webapp:@232.%490","status":"idle"}}"#
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

    /// macOS: Claude writes `ps -o lstart` in UTC; tend reads the start in
    /// seconds. They are the same process only to the second.
    #[test]
    fn a_macos_proc_start_matches_the_same_second_only() {
        // Thu Nov 12 05:42:03 2026 UTC.
        let seconds = time::macros::datetime!(2026-11-12 05:42:03 UTC).unix_timestamp();
        let start = format!("epoch:{seconds}");
        assert!(same_start("Thu Nov 12 05:42:03 2026", &start));
        assert!(same_start(
            "Thu Nov  5 05:42:03 2026",
            &format!("epoch:{}", seconds - 7 * 86_400)
        ));
        assert!(!same_start("Thu Nov 12 05:42:04 2026", &start));
        // Read in local time (KST) it is another instant.
        assert!(!same_start("Thu Nov 12 14:42:03 2026", &start));
        assert!(!same_start("not a date", &start));
        assert!(!same_start("51554631", &start));
        // Linux writes the same decimal both ways.
        assert!(same_start("51554631", "51554631"));
        assert!(!same_start("51554631", "51554632"));
        let files = [file(7, A, "Thu Nov 12 05:42:03 2026")];
        assert_eq!(
            claude_conversation_from(7, Some(&start), &files),
            Conversation::Verified(A.into())
        );
    }

    /// This test's own process has a start time on every platform tend
    /// swaps on, so a session file can be matched to it.
    #[cfg(unix)]
    #[test]
    fn this_process_has_a_start_time() {
        let start = process_start(std::process::id()).expect("a start time");
        #[cfg(target_os = "linux")]
        assert!(start.parse::<u64>().is_ok());
        #[cfg(not(target_os = "linux"))]
        {
            let seconds: i64 = start.strip_prefix("epoch:").unwrap().parse().unwrap();
            let written = time::OffsetDateTime::from_unix_timestamp(seconds)
                .unwrap()
                .format(time::macros::format_description!(
                    "[weekday repr:short] [month repr:short] [day padding:space] [hour]:[minute]:[second] [year]"
                ))
                .unwrap();
            assert!(same_start(&written, &start), "{written} vs {start}");
        }
    }

    #[test]
    fn a_rollout_path_names_its_thread() {
        let path = Path::new(
            "/h/.codex/sessions/2026/11/10/rollout-2026-11-10T16-39-46-01a1c466-13a8-7d80-9b8c-752a2b822a25.jsonl",
        );
        assert_eq!(
            rollout_thread_id(path).as_deref(),
            Some("01a1c466-13a8-7d80-9b8c-752a2b822a25")
        );
        // Archived, misplaced, or not a rollout: no thread.
        for other in [
            "/h/.codex/archived_sessions/rollout-2026-11-10T16-39-46-01a1c466-13a8-7d80-9b8c-752a2b822a25.jsonl",
            "/h/.codex/sessions/2026/11/rollout-2026-11-10T16-39-46-01a1c466-13a8-7d80-9b8c-752a2b822a25.jsonl",
            "/h/.codex/sessions/2026/11/10/rollout-2026-11-10T16-39-46-not-a-uuid.jsonl",
            "/h/.codex/sessions/2026/11/10/history.jsonl",
            "/h/.codex/log/codex-tui.log",
        ] {
            assert_eq!(rollout_thread_id(Path::new(other)), None, "{other}");
        }
    }

    #[test]
    fn a_codex_process_is_on_the_one_rollout_it_has_open() {
        let rollout = |id: &str| {
            PathBuf::from(format!(
                "/h/.codex/sessions/2026/11/10/rollout-2026-11-10T16-39-46-{id}.jsonl"
            ))
        };
        let log = PathBuf::from("/h/.codex/log/codex-tui.log");
        assert_eq!(
            codex_conversation_from(Some(&[log.clone(), rollout(B), rollout(B)])),
            Conversation::Verified(B.into())
        );
        assert!(matches!(
            codex_conversation_from(Some(std::slice::from_ref(&log))),
            Conversation::Unknown(_)
        ));
        assert!(matches!(
            codex_conversation_from(None),
            Conversation::Unknown(_)
        ));
        assert!(matches!(
            codex_conversation_from(Some(&[rollout(A), rollout(B)])),
            Conversation::Conflict(_)
        ));
    }

    #[test]
    fn lsof_name_lines_are_paths() {
        let out = "p42\nf3\nn/h/.codex/log/codex-tui.log\nf42\nn/h/x y.jsonl\nf7\nn[kqueue]\n";
        assert_eq!(
            parse_lsof_names(out),
            vec![
                PathBuf::from("/h/.codex/log/codex-tui.log"),
                PathBuf::from("/h/x y.jsonl")
            ]
        );
    }

    /// End to end on this host: a process holding a rollout open reads as
    /// on that thread, through `/proc/<pid>/fd` or `lsof`.
    #[cfg(unix)]
    #[test]
    fn an_open_rollout_is_read_from_the_live_process() {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("sessions/2026/11/10");
        std::fs::create_dir_all(&day).unwrap();
        let path = day.join(format!("rollout-2026-11-10T16-39-46-{B}.jsonl"));
        std::fs::write(
            &path,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{B}\",\"source\":\"cli\"}}}}\n"
            ),
        )
        .unwrap();
        // A child of its own, so no other test's open files are in the way.
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("exec 3<\"$1\"; exec sleep 30")
            .arg("sh")
            .arg(&path)
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut seen = codex_conversation(child.id());
        while seen != Conversation::Verified(B.into()) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
            seen = codex_conversation(child.id());
        }
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(seen, Conversation::Verified(B.into()));
    }

    #[test]
    fn a_rollout_is_on_the_thread_its_session_record_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.jsonl");
        let meta = |id: &str| {
            format!(r#"{{"type":"session_meta","payload":{{"id":"{id}","source":"cli"}}}}"#)
        };
        std::fs::write(&path, format!("{}\n{{}}\n", meta(B))).unwrap();
        assert_eq!(rollout_meta_id(&path).as_deref(), Some(B));
        // A first record still being written, or another record type.
        std::fs::write(&path, meta(B)).unwrap();
        assert_eq!(rollout_meta_id(&path), None);
        std::fs::write(&path, "{\"type\":\"response_item\",\"payload\":{}}\n").unwrap();
        assert_eq!(rollout_meta_id(&path), None);
    }
}
