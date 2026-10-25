//! Find agent-side muxa processes still running a pre-upgrade binary.
//!
//! `muxa upgrade` replaces the binaries and re-execs muxad, and every hook
//! runs the new `muxa` on its next event. The long-lived `muxa mcp` servers
//! are different: each agent (or Codex's shared app-server, once per thread)
//! spawned one and keeps it until the host refreshes MCP or restarts, so their
//! tools keep the old behavior after an upgrade. Killing one does not help —
//! the agent does not respawn it and its muxa tools simply fail.
//!
//! This scan lists those processes by who owns them, so `muxa upgrade` and
//! `muxa doctor` can identify the owner that must refresh MCP.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, SystemTime};

use muxa::adapters::proc_ancestry::codex_subcommand;

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
    /// Pane owners known to be standalone Codex CLI, not a shared app-server.
    pub standalone_codex: BTreeSet<String>,
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
             muxa tools keep the old behavior until the host refreshes them. Hooks need nothing.",
            self.total()
        );
        for ((pid, home), count) in &self.shared_codex {
            let env = home
                .as_deref()
                .map(|home| format!("CODEX_HOME={home} "))
                .unwrap_or_default();
            let profile = if home.is_none() {
                " --codex-home <owning-profile>"
            } else {
                ""
            };
            let _ = write!(
                out,
                "\n• Codex shared app-server (pid {pid}): {count} old MCP process(es). Queue MCP refresh:\n    \
                 {env}muxa reload mcp{profile}\n  The host queues refresh; old connections may remain through active turns or until the next new turn. \
                 If unsupported, restart the owning Codex app-server when its sessions are idle."
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
                "\n• Agents in {}: reconnect MCP in each agent client. If its client has no \
                 reconnect support, restart that agent and resume its conversation.",
                panes.join(", ")
            );
        }
        if self.other > 0 {
            let _ = write!(
                out,
                "\n• {} more whose owner could not be identified — identify the client \
                 that launched it and reconnect MCP there.",
                self.other
            );
        }
        if !self.standalone_codex.is_empty() {
            out.push_str("\nStandalone Codex CLI owners are not controlled by the shared app-server reload API. Reconnect in that client, or restart and resume it when idle.");
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
    let canonical = muxa::tmux::canonical_panes(panes);
    let pane_of_pid: HashMap<u32, &muxa::tmux::PaneInfo> = canonical
        .iter()
        .filter(|pane| pane.pane_pid != 0)
        .map(|pane| (pane.pane_pid, pane))
        .collect();
    let mut report = StaleReport::default();
    for proc in procs {
        if !is_muxa_mcp(&proc.args) || proc.age <= age_of_binary + START_SLACK {
            continue;
        }
        let mut cur = proc.parent_pid;
        let mut placed = false;
        let mut standalone_codex = false;
        for _ in 0..32 {
            if let Some(pane) = pane_of_pid.get(&cur) {
                let label = format!(
                    "{} ({}:{}.{})",
                    pane.pane_id, pane.session, pane.window_index, pane.pane_index
                );
                if standalone_codex {
                    report.standalone_codex.insert(label.clone());
                }
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
            standalone_codex |=
                matches!(codex_subcommand(&parent.args), Some("" | "resume" | "exec"));
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
    words.next().map(muxa::shell::program_name) == Some("muxa") && words.next() == Some("mcp")
}

/// One pass over the process table: pid, parent pid, age, command line.
///
/// Windows has no `ps`; its table comes from `sysinfo`. The program is
/// reduced to its name there, because a path such as `C:\Program Files\…`
/// would otherwise split into several words.
#[cfg(windows)]
fn read_processes() -> Vec<Proc> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always),
    );
    system
        .processes()
        .values()
        .filter_map(|process| {
            let mut words = process
                .cmd()
                .iter()
                .map(|argument| argument.to_string_lossy().into_owned());
            let program = words.next()?;
            let line = std::iter::once(muxa::shell::program_name(&program).to_owned())
                .chain(words)
                .collect::<Vec<_>>()
                .join(" ");
            Some(Proc {
                pid: process.pid().as_u32(),
                parent_pid: process.parent().map_or(0, sysinfo::Pid::as_u32),
                age: Duration::from_secs(process.run_time()),
                args: line,
            })
        })
        .collect()
}

/// One `ps` pass: pid, parent pid, elapsed time, command line.
#[cfg(not(windows))]
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
#[cfg_attr(windows, allow(dead_code))] // Windows reads ages from sysinfo
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

/// The server's `CODEX_HOME`, which names the daemon to restart.
#[cfg(target_os = "linux")]
fn codex_home_of(pid: u32) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    codex_home_from_environment(&raw)
}

/// The server's `CODEX_HOME`, which names the daemon to restart. There is
/// no `/proc` here, and `ps eww` joins the environment with spaces, so a
/// profile under `~/Library/Application Support/…` would be cut at the
/// space; sysinfo returns each entry whole (macOS `KERN_PROCARGS2`).
#[cfg(not(target_os = "linux"))]
fn codex_home_of(pid: u32) -> Option<String> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        // macOS parses argv and the environment from one KERN_PROCARGS2
        // read; sysinfo fills `environ` only when `cmd` is refreshed too.
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .with_environ(UpdateKind::Always),
    );
    let raw: Vec<u8> = system
        .process(pid)?
        .environ()
        .iter()
        .flat_map(|entry| entry.as_encoded_bytes().iter().copied().chain([0]))
        .collect();
    codex_home_from_environment(&raw)
}

fn codex_home_from_environment(raw: &[u8]) -> Option<String> {
    let value = |key: &str| {
        raw.split(|byte| *byte == 0)
            .filter_map(|entry| std::str::from_utf8(entry).ok())
            .find_map(|entry| entry.strip_prefix(key).filter(|value| !value.is_empty()))
    };
    value("CODEX_HOME=").map(str::to_owned).or_else(|| {
        value("HOME=").map(|home| {
            Path::new(home)
                .join(".codex")
                .to_string_lossy()
                .into_owned()
        })
    })
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
        assert!(text.contains("CODEX_HOME=/home/u/.codex muxa reload mcp"));
        assert!(text.contains("reconnect MCP in each agent client"));
        assert!(!text.contains("every pane at once"));
        assert!(StaleReport::default().guidance().is_none());
    }

    #[test]
    fn an_app_server_started_with_config_overrides_is_still_shared() {
        let procs = [
            proc(
                2693,
                1,
                9000,
                "codex -c features.code_mode_host=true app-server --listen unix://",
            ),
            proc(16327, 2693, 9000, "muxa mcp"),
        ];
        let report = classify(&procs, Duration::from_secs(60), &[], |_| {
            Some("/home/u/.codex".into())
        });
        assert_eq!(
            report
                .shared_codex
                .get(&(2693, Some("/home/u/.codex".into()))),
            Some(&1)
        );
        assert_eq!(report.other, 0);
    }

    #[test]
    fn standalone_codex_is_not_mistaken_for_its_profiles_shared_server() {
        let procs = [
            proc(200, 1, 9000, "zsh"),
            proc(
                201,
                200,
                9000,
                "/bin/codex --yolo --no-daemon resume session",
            ),
            proc(202, 201, 9000, "muxa mcp"),
            proc(300, 1, 9000, "/bin/codex app-server --managed-daemon"),
            proc(301, 300, 9000, "muxa mcp"),
        ];
        let report = classify(
            &procs,
            Duration::from_secs(60),
            &[pane("rmux:%439", 200)],
            |_| Some("/profiles/shared".into()),
        );
        assert_eq!(
            report
                .shared_codex
                .get(&(300, Some("/profiles/shared".into()))),
            Some(&1)
        );
        assert!(report.standalone_codex.contains("rmux:%439 (callabo:4.1)"));
        assert_eq!(report.total(), 2);
        assert!(report.guidance().unwrap().contains("when idle"));
    }
    #[test]
    fn codex_profile_uses_owners_default_home_when_unset() {
        assert_eq!(
            codex_home_from_environment(b"HOME=/owner\0"),
            Some("/owner/.codex".into())
        );
        assert_eq!(
            codex_home_from_environment(b"HOME=/owner\0CODEX_HOME=/profile\0"),
            Some("/profile".into())
        );
        assert_eq!(codex_home_from_environment(b"OTHER=value\0"), None);
    }

    #[test]
    fn a_profile_path_with_spaces_is_kept_whole() {
        assert_eq!(
            codex_home_from_environment(
                b"HOME=/Users/u\0CODEX_HOME=/Users/u/Library/Application Support/asx/profiles/codex-a\0"
            ),
            Some("/Users/u/Library/Application Support/asx/profiles/codex-a".into())
        );
    }

    /// Parks a child process for the live lookup below; never run directly.
    #[test]
    #[ignore = "helper process for reads_a_live_processs_codex_home_including_spaces"]
    fn codex_home_lookup_sleeper() {
        std::thread::sleep(std::time::Duration::from_secs(30));
    }

    /// The real lookup on this OS (`/proc` on Linux, sysinfo elsewhere)
    /// against a live process whose profile path contains a space, as
    /// `asx` profiles under `~/Library/Application Support` do. The child is
    /// this test binary: macOS withholds the environment of platform
    /// binaries such as `/bin/sleep`, which a Codex app-server is not.
    #[cfg(unix)]
    #[test]
    fn reads_a_live_processs_codex_home_including_spaces() {
        let home = "/tmp/muxa test/Application Support/codex-profile";
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "stale_agents::tests::codex_home_lookup_sleeper",
            ])
            .env("CODEX_HOME", home)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn helper");
        // Readable once the child has reached `exec`.
        let mut found = None;
        for _ in 0..100 {
            found = codex_home_of(child.id());
            if found.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(found.as_deref(), Some(home));
    }
}
