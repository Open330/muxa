//! Which agent a pane runs and which conversation it is on, decided from the
//! pane's own command line rather than taken on trust from the registry.
//!
//! The registry can be stale (a row that says Claude for a pane now running
//! Codex) or blind (a screen-detected Codex row with a `synthetic-…` id).
//! Replaying either as recorded resumed the wrong provider, or relaunched a
//! fresh agent where a conversation should have come back.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::AgentShape;

/// The agent a command line runs, and the conversation id it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Launched {
    pub kind: &'static str,
    pub session_id: Option<String>,
}

fn is_uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok()
}

/// Read the agent out of `argv`: the program (`claude`, `…/codex`), or the
/// `aas` account it runs under (`june@claude`, `e-ed@codex`).
pub(super) fn launched(argv: &[String]) -> Option<Launched> {
    let kind = argv.iter().find_map(|argument| {
        let name = argument.rsplit('/').next().unwrap_or(argument);
        if name == "claude" || name.ends_with("@claude") {
            Some("claude_code")
        } else if name == "codex" || name.ends_with("@codex") {
            Some("codex")
        } else {
            None
        }
    })?;
    let mut session_id = None;
    let mut arguments = argv.iter();
    while let Some(argument) = arguments.next() {
        let id = match (kind, argument.as_str()) {
            ("claude_code", "--resume" | "-r" | "--session-id") | ("codex", "resume") => {
                arguments.next().map(String::as_str)
            }
            ("claude_code", flag) => flag
                .strip_prefix("--resume=")
                .or_else(|| flag.strip_prefix("--session-id=")),
            _ => None,
        };
        if let Some(id) = id.filter(|id| is_uuid(id)) {
            session_id = Some(id.to_owned());
            break;
        }
    }
    Some(Launched { kind, session_id })
}

pub(super) fn resumable(session_id: &str) -> bool {
    !session_id.is_empty() && !session_id.starts_with("synthetic")
}

/// The agent to record for a pane. The command line decides the kind; the
/// registry's id is kept only when it is about that same agent, and an id
/// the command line names fills in for a registry that has none. A pane whose
/// command names no agent keeps whatever the registry said.
pub(super) fn reconcile(
    recorded: Option<AgentShape>,
    argv: Option<&[String]>,
) -> Option<AgentShape> {
    let Some(launched) = argv.and_then(launched) else {
        return recorded;
    };
    let working = recorded.as_ref().is_some_and(|agent| agent.working);
    let recorded_id = recorded
        .filter(|agent| agent.kind == launched.kind && resumable(&agent.session_id))
        .map(|agent| agent.session_id);
    Some(AgentShape {
        kind: launched.kind.to_owned(),
        session_id: recorded_id.or(launched.session_id).unwrap_or_default(),
        working,
    })
}

// Linux builds read neither argv nor start times through `sysinfo`, so the
// thread recovery below runs on macOS only.
#[cfg_attr(target_os = "linux", allow(dead_code))]
/// A Codex thread's first rollout line: who it is, where it ran, and when it
/// began.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Thread {
    pub id: String,
    pub cwd: PathBuf,
    /// Unix seconds.
    pub started: i64,
}

#[cfg_attr(target_os = "linux", allow(dead_code))]
/// Parse a rollout's `session_meta` line.
pub(super) fn thread_of(first_line: &str) -> Option<Thread> {
    let value: serde_json::Value = serde_json::from_str(first_line).ok()?;
    let payload = value.get("payload")?;
    let id = payload.get("id")?.as_str()?.to_owned();
    let cwd = PathBuf::from(payload.get("cwd")?.as_str()?);
    let started = time::OffsetDateTime::parse(
        payload.get("timestamp")?.as_str()?,
        &time::format_description::well_known::Rfc3339,
    )
    .ok()?
    .unix_timestamp();
    Some(Thread { id, cwd, started })
}

#[cfg_attr(target_os = "linux", allow(dead_code))]
/// Every thread begun since `since` (Unix seconds), from the rollouts under
/// `sessions/YYYY/MM/DD`. Only day directories that can hold such a thread
/// are read.
pub(super) fn threads_since(sessions: &Path, since: i64) -> Vec<Thread> {
    let first_day = time::OffsetDateTime::from_unix_timestamp(since - 86_400)
        .map_or(time::Date::MIN, time::OffsetDateTime::date);
    let mut threads = Vec::new();
    let read = |dir: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
            .unwrap_or_default()
    };
    for year in read(sessions) {
        for month in read(&year) {
            for day in read(&month) {
                let date = || -> Option<time::Date> {
                    let number = |path: &Path| path.file_name()?.to_str()?.parse::<i32>().ok();
                    time::Date::from_calendar_date(
                        number(&year)?,
                        time::Month::try_from(u8::try_from(number(&month)?).ok()?).ok()?,
                        u8::try_from(number(&day)?).ok()?,
                    )
                    .ok()
                };
                if date().is_none_or(|date| date < first_day) {
                    continue;
                }
                for file in read(&day) {
                    let name = file
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or_default();
                    let jsonl = Path::new(name)
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"));
                    if !(name.starts_with("rollout-") && jsonl) {
                        continue;
                    }
                    let first_line = std::fs::File::open(&file).ok().and_then(|file| {
                        use std::io::BufRead;
                        std::io::BufReader::new(file).lines().next()?.ok()
                    });
                    if let Some(thread) = first_line.as_deref().and_then(thread_of) {
                        if thread.started >= since {
                            threads.push(thread);
                        }
                    }
                }
            }
        }
    }
    threads
}

#[cfg_attr(target_os = "linux", allow(dead_code))]
/// A Codex pane muxa has no thread id for: where it runs and when its agent
/// started.
#[derive(Debug, Clone)]
pub(super) struct Orphan {
    pub cwd: PathBuf,
    /// Unix seconds.
    pub started: i64,
}

#[cfg_attr(target_os = "linux", allow(dead_code))]
/// How far before its process a thread may claim to start (clock skew).
const START_SLACK_SECS: i64 = 5;

#[cfg_attr(target_os = "linux", allow(dead_code))]
/// Match each orphan to the one thread begun in its directory after it
/// started. Only an unambiguous pairing counts: a thread two orphans could
/// both claim, or an orphan with two candidates, stays unknown. Threads some
/// other pane already resumes are not candidates.
pub(super) fn match_orphans(
    orphans: &BTreeMap<usize, Orphan>,
    threads: &[Thread],
    claimed: &BTreeSet<String>,
) -> BTreeMap<usize, String> {
    let candidates: BTreeMap<usize, Vec<&str>> = orphans
        .iter()
        .map(|(at, orphan)| {
            let ids = threads
                .iter()
                .filter(|thread| {
                    thread.cwd == orphan.cwd
                        && thread.started >= orphan.started - START_SLACK_SECS
                        && !claimed.contains(&thread.id)
                })
                .map(|thread| thread.id.as_str())
                .collect();
            (*at, ids)
        })
        .collect();
    candidates
        .iter()
        .filter_map(|(at, ids)| {
            let [id] = ids.as_slice() else {
                return None;
            };
            let contested = candidates
                .iter()
                .any(|(other, ids)| other != at && ids.contains(id));
            (!contested).then(|| (*at, (*id).to_owned()))
        })
        .collect()
}

#[cfg_attr(target_os = "linux", allow(dead_code))]
/// Where Codex writes rollouts: `$CODEX_HOME/sessions`, else
/// `~/.codex/sessions`. `aas` profiles share that directory.
pub(super) fn codex_sessions_dir() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
        .map(|home| home.join("sessions"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(line: &str) -> Vec<String> {
        line.split(' ').map(str::to_owned).collect()
    }

    const ID: &str = "01a13dc8-98fb-75f2-a6ee-5322749de1a7";

    #[test]
    fn reads_the_agent_and_its_conversation_from_argv() {
        assert_eq!(
            launched(&argv(&format!(
                "/Users/j/.local/bin/aas exec e-ed@codex -- --yolo resume {ID}"
            ))),
            Some(Launched {
                kind: "codex",
                session_id: Some(ID.into())
            })
        );
        assert_eq!(
            launched(&argv("aas e june@claude --dangerously-skip-permissions")),
            Some(Launched {
                kind: "claude_code",
                session_id: None
            })
        );
        assert_eq!(launched(&argv("npm run dev")), None);
        assert_eq!(
            launched(&argv("claude --resume not-an-id"))
                .unwrap()
                .session_id,
            None
        );
    }

    #[test]
    fn the_command_line_overrides_a_stale_registry_kind() {
        let stale = AgentShape {
            kind: "claude_code".into(),
            session_id: "63031d1f-edc2-482d-b6e2-0d59d5d9f38b".into(),
            working: true,
        };
        let line = argv("/Users/j/.local/bin/aas exec e-ed@codex -- --yolo");
        let agent = reconcile(Some(stale), Some(&line)).unwrap();
        assert_eq!(agent.kind, "codex");
        assert_eq!(agent.session_id, "", "a Claude id is no Codex conversation");
        assert!(agent.working);
    }

    #[test]
    fn a_synthetic_row_takes_the_id_its_command_names() {
        let synthetic = AgentShape {
            kind: "codex".into(),
            session_id: "synthetic-7:default:%19".into(),
            working: false,
        };
        let line = argv(&format!("codex --yolo resume {ID}"));
        assert_eq!(
            reconcile(Some(synthetic), Some(&line)).unwrap().session_id,
            ID
        );
    }

    #[test]
    fn a_matching_registry_id_wins_and_unknown_commands_keep_the_row() {
        let row = AgentShape {
            kind: "claude_code".into(),
            session_id: "hook-id".into(),
            working: false,
        };
        let line = argv(&format!("claude --resume {ID}"));
        assert_eq!(
            reconcile(Some(row.clone()), Some(&line))
                .unwrap()
                .session_id,
            "hook-id"
        );
        assert_eq!(
            reconcile(Some(row.clone()), Some(&argv("bash run.sh"))),
            Some(row)
        );
    }

    #[test]
    fn parses_a_rollout_session_meta_line() {
        let line = r#"{"timestamp":"2026-10-15T01:45:20.490Z","type":"session_meta","payload":{"id":"01a13d3c-1298-78b0-af09-e9bddd2209af","timestamp":"2026-10-15T01:45:09.543Z","cwd":"/Users/june/personal/aas"}}"#;
        let thread = thread_of(line).unwrap();
        assert_eq!(thread.id, "01a13d3c-1298-78b0-af09-e9bddd2209af");
        assert_eq!(thread.cwd, PathBuf::from("/Users/june/personal/aas"));
        assert_eq!(thread.started, 1_792_028_709);
    }

    fn thread(id: &str, cwd: &str, started: i64) -> Thread {
        Thread {
            id: id.into(),
            cwd: cwd.into(),
            started,
        }
    }

    #[test]
    fn an_orphan_gets_the_one_thread_begun_in_its_directory_after_it_started() {
        let orphans = BTreeMap::from([
            (
                0,
                Orphan {
                    cwd: "/w/ix".into(),
                    started: 1000,
                },
            ),
            (
                1,
                Orphan {
                    cwd: "/w/qqg".into(),
                    started: 1000,
                },
            ),
        ]);
        let threads = [
            thread("old", "/w/ix", 900),
            thread("ix-new", "/w/ix", 1003),
            thread("elsewhere", "/w/other", 1003),
            thread("qqg-a", "/w/qqg", 1001),
            thread("qqg-b", "/w/qqg", 1002),
        ];
        let matched = match_orphans(&orphans, &threads, &BTreeSet::new());
        assert_eq!(
            matched,
            BTreeMap::from([(0, "ix-new".to_owned())]),
            "qqg is ambiguous"
        );
    }

    #[test]
    fn a_thread_two_orphans_could_claim_or_one_already_resumed_is_not_a_match() {
        let orphans = BTreeMap::from([
            (
                0,
                Orphan {
                    cwd: "/w/qqg".into(),
                    started: 1000,
                },
            ),
            (
                1,
                Orphan {
                    cwd: "/w/qqg".into(),
                    started: 1000,
                },
            ),
        ]);
        let threads = [thread("only", "/w/qqg", 1001)];
        assert!(match_orphans(&orphans, &threads, &BTreeSet::new()).is_empty());

        let lone = BTreeMap::from([(
            0,
            Orphan {
                cwd: "/w/ix".into(),
                started: 1000,
            },
        )]);
        let threads = [thread("taken", "/w/ix", 1001)];
        let claimed = BTreeSet::from(["taken".to_owned()]);
        assert!(match_orphans(&lone, &threads, &claimed).is_empty());
    }
}
