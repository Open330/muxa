//! Find agent-side muxa processes still running a pre-upgrade binary.
//!
//! `muxa upgrade` replaces the binaries and re-execs muxad, and every hook
//! runs the new `muxa` on its next event. The long-lived `muxa mcp` servers
//! are different: each agent (or Codex's shared app-server, once per thread)
//! spawned one and keeps it until that agent or server restarts, so their
//! tools keep the old behavior after an upgrade. Killing one does not help —
//! the agent does not respawn it and its muxa tools simply fail.
//!
//! This scan lists those processes by who owns them, so `muxa upgrade` and
//! `muxa doctor` can say exactly what to restart.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, SystemTime};

/// A process older than the binary by less than this is treated as fresh:
/// the install writes the file a moment before anything re-launches.
const START_SLACK: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Proc {
    pid: u32,
    parent_pid: u32,
    age: Duration,
    args: String,
}

/// Stale `muxa mcp` servers grouped by what has to restart to replace them.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StaleReport {
    /// Codex shared app-servers: `(server pid, CODEX_HOME if readable)` →
    /// number of stale per-thread servers.
    pub shared_codex: BTreeMap<(u32, Option<String>), usize>,
    /// Pane-hosted agents: pane label → number of stale servers.
    pub panes: BTreeMap<String, usize>,
    /// Stale servers whose owner could not be placed.
    pub other: usize,
}

impl StaleReport {
    pub fn total(&self) -> usize {
        self.shared_codex.values().sum::<usize>() + self.panes.values().sum::<usize>() + self.other
    }

    /// Human guidance, or `None` when nothing is stale.
    pub fn guidance(&self) -> Option<String> {
        if self.total() == 0 {
            return None;
        }
        let mut out = format!(
            "{} agent tool server(s) (`muxa mcp`) still run the pre-upgrade muxa; their \
             muxa tools keep the old behavior until the owner restarts. Hooks need nothing.",
            self.total()
        );
        for ((pid, home), count) in &self.shared_codex {
            let env = home
                .as_deref()
                .map(|home| format!("CODEX_HOME={home} "))
                .unwrap_or_default();
            let _ = write!(
                out,
                "\n• Codex shared app-server (pid {pid}): {count} thread(s). When those \
                 sessions are idle:\n    {env}codex app-server daemon restart"
            );
        }
        if !self.panes.is_empty() {
            let panes: Vec<_> = self
                .panes
                .iter()
                .map(|(pane, count)| {
                    if *count > 1 {
                        format!("{pane} ×{count}")
                    } else {
                        pane.clone()
                    }
                })
                .collect();
            let _ = write!(
                out,
                "\n• Agents in {}: restart each (resume its conversation), or restart \
                 every pane at once from outside the multiplexer:\n    muxa reload",
                panes.join(", ")
            );
        }
        if self.other > 0 {
            let _ = write!(
                out,
                "\n• {} more whose owner could not be identified — restart the agent \
                 that launched it.",
                self.other
            );
        }
        out.push_str("\nNever kill a `muxa mcp` to refresh it: the agent will not respawn it.");
        Some(out)
    }
}

/// The `muxa` agents launch: `muxa mcp` is registered by name, so the first
/// one on `PATH`, falling back to this executable. A Linux exe replaced in
/// place reads back as `… (deleted)`; the live path is the one without it.
pub fn installed_muxa() -> Option<std::path::PathBuf> {
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join("muxa"))
                .find(|candidate| candidate.is_file())
        })
        .or_else(|| {
            let exe = std::env::current_exe().ok()?;
            let text = exe.to_string_lossy();
            Some(
                text.strip_suffix(" (deleted)")
                    .map_or(exe.clone(), std::path::PathBuf::from),
            )
        })
}

/// Scan for `muxa mcp` processes started before `binary` was last written.
pub fn scan(binary: &Path) -> StaleReport {
    let Some(installed) = std::fs::metadata(binary)
        .and_then(|meta| meta.modified())
        .ok()
    else {
        return StaleReport::default();
    };
    let age_of_binary = SystemTime::now()
        .duration_since(installed)
        .unwrap_or_default();
    let procs = read_processes();
    if procs.is_empty() {
        return StaleReport::default();
    }
    let panes: Vec<muxa::tmux::PaneInfo> = muxa::active_backends()
        .into_iter()
        .flat_map(|backend| backend.list_panes())
        .collect();
    classify(&procs, age_of_binary, &panes, codex_home_of)
}

fn classify(
    procs: &[Proc],
    age_of_binary: Duration,
    panes: &[muxa::tmux::PaneInfo],
    codex_home: impl Fn(u32) -> Option<String>,
) -> StaleReport {
    let by_pid: HashMap<u32, &Proc> = procs.iter().map(|p| (p.pid, p)).collect();
    // A pane in a grouped session is listed once per `~view~` client
    // session; name it by its base session.
    let mut pane_of_pid: HashMap<u32, &muxa::tmux::PaneInfo> = HashMap::new();
    for pane in panes.iter().filter(|pane| pane.pane_pid != 0) {
        let slot = pane_of_pid.entry(pane.pane_pid).or_insert(pane);
        if slot.session.contains("~view~") && !pane.session.contains("~view~") {
            *slot = pane;
        }
    }
    let mut report = StaleReport::default();
    for proc in procs {
        if !is_muxa_mcp(&proc.args) || proc.age <= age_of_binary + START_SLACK {
            continue;
        }
        let mut cur = proc.parent_pid;
        let mut placed = false;
        for _ in 0..32 {
            if let Some(pane) = pane_of_pid.get(&cur) {
                let label = format!(
                    "{} ({}:{}.{})",
                    pane.pane_id, pane.session, pane.window_index, pane.pane_index
                );
                *report.panes.entry(label).or_default() += 1;
                placed = true;
                break;
            }
            let Some(parent) = by_pid.get(&cur) else {
                break;
            };
            if codex_subcommand(&parent.args) == Some("app-server") {
                *report
                    .shared_codex
                    .entry((parent.pid, codex_home(parent.pid)))
                    .or_default() += 1;
                placed = true;
                break;
            }
            if parent.parent_pid <= 1 || parent.parent_pid == cur {
                break;
            }
            cur = parent.parent_pid;
        }
        if !placed {
            report.other += 1;
        }
    }
    report
}

fn is_muxa_mcp(args: &str) -> bool {
    let mut words = args.split_whitespace();
    words.next().and_then(|p| p.rsplit('/').next()) == Some("muxa") && words.next() == Some("mcp")
}

fn codex_subcommand(args: &str) -> Option<&str> {
    let mut words = args.split_whitespace();
    let program = words.next()?;
    (program.rsplit('/').next() == Some("codex")).then(|| words.next().unwrap_or(""))
}

/// One `ps` pass: pid, parent pid, elapsed time, command line.
fn read_processes() -> Vec<Proc> {
    let Ok(output) = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,etime=,args="])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let parent_pid = fields.next()?.parse().ok()?;
            let age = parse_etime(fields.next()?)?;
            let args = fields.collect::<Vec<_>>().join(" ");
            Some(Proc {
                pid,
                parent_pid,
                age,
                args,
            })
        })
        .collect()
}

/// `ps` elapsed time: `[[dd-]hh:]mm:ss`.
fn parse_etime(value: &str) -> Option<Duration> {
    let (days, clock) = match value.split_once('-') {
        Some((days, clock)) => (days.parse::<u64>().ok()?, clock),
        None => (0, value),
    };
    let parts: Vec<u64> = clock
        .split(':')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    let seconds = match parts.as_slice() {
        [m, s] => m * 60 + s,
        [h, m, s] => h * 3600 + m * 60 + s,
        _ => return None,
    };
    Some(Duration::from_secs(days * 86_400 + seconds))
}

/// The server's `CODEX_HOME`, which names the daemon to restart. Only
/// readable from `/proc`; elsewhere the default home is implied.
fn codex_home_of(pid: u32) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    raw.split(|b| *b == 0)
        .filter_map(|kv| std::str::from_utf8(kv).ok())
        .find_map(|kv| kv.strip_prefix("CODEX_HOME=").map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, parent_pid: u32, age_secs: u64, args: &str) -> Proc {
        Proc {
            pid,
            parent_pid,
            age: Duration::from_secs(age_secs),
            args: args.into(),
        }
    }

    fn pane(id: &str, pid: u32) -> muxa::tmux::PaneInfo {
        serde_json::from_value(serde_json::json!({
            "pane_id": id, "session": "callabo", "window_index": "4", "pane_index": "1",
            "tty": "", "current_command": "aas", "title": "", "pane_pid": pid
        }))
        .unwrap()
    }

    #[test]
    fn etime_formats_parse() {
        assert_eq!(parse_etime("05:07"), Some(Duration::from_secs(307)));
        assert_eq!(parse_etime("01:00:00"), Some(Duration::from_secs(3600)));
        assert_eq!(
            parse_etime("2-03:00:01"),
            Some(Duration::from_secs(2 * 86_400 + 3 * 3600 + 1))
        );
        assert_eq!(parse_etime("bogus"), None);
    }

    #[test]
    fn stale_servers_are_grouped_by_owner() {
        let procs = [
            proc(1, 0, 99_999, "/sbin/init"),
            // Shared Codex server with two old thread servers and one new.
            proc(
                100,
                1,
                9000,
                "/x/codex app-server --listen unix:// --managed-daemon",
            ),
            proc(101, 100, 9000, "muxa mcp"),
            proc(102, 100, 8000, "/home/u/.cargo/bin/muxa mcp"),
            proc(103, 100, 10, "muxa mcp"),
            // An embedded agent in a pane.
            proc(200, 1, 9000, "-zsh"),
            proc(201, 200, 9000, "claude"),
            proc(202, 201, 9000, "muxa mcp"),
            // Not an MCP server.
            proc(300, 1, 9000, "muxa watch"),
        ];
        let report = classify(
            &procs,
            Duration::from_secs(60),
            &[
                {
                    let mut view = pane("rmux:%217", 200);
                    view.session = "callabo~view~1".into();
                    view
                },
                pane("rmux:%217", 200),
            ],
            |_| Some("/home/u/.codex".into()),
        );
        assert_eq!(
            report
                .shared_codex
                .get(&(100, Some("/home/u/.codex".into()))),
            Some(&2)
        );
        assert_eq!(report.panes.get("rmux:%217 (callabo:4.1)"), Some(&1));
        assert_eq!(report.total(), 3);
        let text = report.guidance().unwrap();
        assert!(text.contains("CODEX_HOME=/home/u/.codex codex app-server daemon restart"));
        assert!(text.contains("muxa reload"));
        assert!(StaleReport::default().guidance().is_none());
    }
}
