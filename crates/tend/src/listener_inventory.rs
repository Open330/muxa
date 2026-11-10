//! TCP listeners associated with tend: servers this user's processes keep
//! listening on that descend from a pane where tend tracks an agent, or run
//! inside a worktree tend created. Shared by `tend servers` and the daemon's
//! Fleet `server_inventory` operation.
//!
//! Read-only, and an association, not proof of ownership: a process under an
//! agent's pane may have been started by the person or an earlier agent, and
//! a process inside a worktree may have been started by hand. Each row keeps
//! the evidence it was listed on. The status says what is known (the Work
//! completed, the agent is idle, needs input, stopped, still working, or
//! unknown); nothing here says a process is safe to stop.
//!
//! Collection is bounded like the worktree inventory and fails closed: a
//! scan that could not run, was cut short or ran out of time says so instead
//! of reading as "no servers". Only this user's processes are listed, only
//! by executable name and working directory, never argv or environment.
//! Agent CLIs' own listeners (an app-server, say) are left out: they are
//! the agent, not something it left behind.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::event::AgentState;
use crate::state::Agent;
use crate::worktree_inventory::{self, lenient_canonical};
use crate::worktree_receipt::{ReceiptState, WorktreeReceipt};

/// How a listener's address is bound. Binding is not reachability:
/// firewalls, containers and proxies decide that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Bind {
    Loopback,
    /// Every interface (`*`, `0.0.0.0`, `::`).
    Wildcard,
    /// One specific non-loopback address.
    Interface,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub address: String,
    pub port: u16,
    pub bind: Bind,
}

/// What a listener is associated with, strongest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Association {
    /// The process descends from the pane's first process, and tend tracks
    /// an agent on that pane.
    Pane {
        /// The pane's server endpoint (tmux socket name, or socket path).
        endpoint: String,
        pane_id: String,
        agent_kind: String,
        agent_session_id: String,
    },
    /// The process's working directory is inside a worktree tend created.
    Worktree {
        receipt_id: String,
        path: PathBuf,
        origin: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListenerStatus {
    /// The Work or dispatch of its worktree completed, by its own evidence.
    WorkCompleted,
    AgentStopped,
    AgentIdle,
    AgentError,
    AgentNeedsInput,
    AgentWorking,
    /// Nothing tend knows says how it stands.
    Unknown,
}

/// One process and every TCP endpoint it listens on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListenerRow {
    pub pid: u32,
    /// Opaque start identity: with `pid`, tells a process from a later one
    /// that reused its pid.
    pub process_start: Option<String>,
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub started_at: Option<OffsetDateTime>,
    pub uid: u32,
    /// The executable's name, control characters replaced.
    pub process_name: String,
    pub cwd: Option<String>,
    pub endpoints: Vec<Endpoint>,
    pub association: Association,
    pub status: ListenerStatus,
    /// Seconds in its agent's current state, for idle or stopped agents.
    pub state_for_secs: Option<i64>,
}

/// Whether a part of the scan saw everything it needed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Coverage {
    Complete,
    /// Some evidence was missing; rows may be missing or less specific.
    Partial {
        reason: String,
    },
    /// This part could not run at all.
    Unavailable {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListenerInventory {
    pub rows: Vec<ListenerRow>,
    /// Rows before any limit.
    pub total: usize,
    /// Listening sockets and their processes.
    pub collection: Coverage,
    /// Panes, agents, worktrees and Work state used to associate them.
    pub attribution: Coverage,
    #[serde(with = "time::serde::rfc3339")]
    pub observed_at: OffsetDateTime,
}

/// The agents tend tracks on this host, or why they could not be read.
pub type AgentsView = Result<Vec<Agent>, String>;

/// Lists listeners associated with tend; blocking, bounded by
/// [`worktree_inventory::SCAN_BUDGET`]. With a `limit`, rows are sorted
/// first (the most likely leftovers first) so the cut is deliberate.
#[must_use]
pub fn inventory(agents: &AgentsView, limit: Option<usize>) -> ListenerInventory {
    let deadline = Instant::now() + worktree_inventory::SCAN_BUDGET;
    worktree_inventory::with_deadline(deadline, || scan(agents, limit, deadline))
}

fn scan(agents: &AgentsView, limit: Option<usize>, deadline: Instant) -> ListenerInventory {
    let observed_at = OffsetDateTime::now_utc();
    let empty = |collection: Coverage| ListenerInventory {
        rows: Vec::new(),
        total: 0,
        collection,
        attribution: Coverage::Complete,
        observed_at,
    };
    let Some(me) = own_uid() else {
        return empty(Coverage::Unavailable {
            reason: "could not tell which user this is".into(),
        });
    };
    let listeners = match listening_sockets(me) {
        Ok(listeners) => listeners,
        Err(reason) => return empty(Coverage::Unavailable { reason }),
    };
    if listeners.is_empty() {
        return empty(Coverage::Complete);
    }
    let Some(table) = process_table() else {
        return empty(Coverage::Unavailable {
            reason: "could not read the process table".into(),
        });
    };
    let mut gaps: Vec<String> = Vec::new();
    let evidence = Evidence::gather(agents, &mut gaps);
    let mut owned_cache: HashMap<String, bool> = HashMap::new();
    let mut rows = Vec::new();
    let now = OffsetDateTime::now_utc();
    for (pid, endpoints) in listeners {
        if Instant::now() >= deadline {
            gaps.push("ran out of time associating listeners".into());
            break;
        }
        let Some(process) = table.get(&pid) else {
            continue;
        };
        if process.uid != Some(me) {
            continue;
        }
        // An agent CLI's own listener (Codex's app-server, say) is part of
        // the agent, not something it left running.
        if crate::discovery::classify_command(&process.name).is_some() {
            continue;
        }
        let Some((association, status, state_for_secs)) =
            associate(pid, process, &table, &evidence, &mut owned_cache, now)
        else {
            continue;
        };
        rows.push(ListenerRow {
            pid,
            process_start: process.start.clone(),
            started_at: process
                .started_at
                .and_then(|secs| OffsetDateTime::from_unix_timestamp(secs).ok()),
            uid: me,
            process_name: printable(&process.name),
            cwd: process
                .cwd
                .as_ref()
                .map(|cwd| printable(&cwd.to_string_lossy())),
            endpoints,
            association,
            status,
            state_for_secs,
        });
    }
    sort_rows(&mut rows);
    let total = rows.len();
    rows.truncate(limit.unwrap_or(usize::MAX));
    let attribution = if gaps.is_empty() {
        Coverage::Complete
    } else {
        Coverage::Partial {
            reason: gaps.join("; "),
        }
    };
    ListenerInventory {
        rows,
        total,
        collection: Coverage::Complete,
        attribution,
        observed_at,
    }
}

/// What `process` is associated with and how it stands, or None when it is
/// not tend's business. Completed Work is the strongest status; a pane with
/// a tracked agent comes next; a worktree alone says only "unknown".
fn associate(
    pid: u32,
    process: &Proc,
    table: &HashMap<u32, Proc>,
    evidence: &Evidence<'_>,
    owned_cache: &mut HashMap<String, bool>,
    now: OffsetDateTime,
) -> Option<(Association, ListenerStatus, Option<i64>)> {
    let pane = pane_ancestor(pid, table, &evidence.panes);
    let agent = pane
        .as_ref()
        .and_then(|pane| tracked_agent(pane, evidence.agents));
    let worktree = process
        .cwd
        .as_ref()
        .and_then(|cwd| worktree_for(cwd, &evidence.receipts, owned_cache));
    match (&pane, agent, worktree) {
        (_, _, Some(receipt))
            if worktree_inventory::work_completed(
                receipt,
                &evidence.runs,
                evidence.dispatches.as_deref(),
            ) =>
        {
            Some((
                worktree_association(receipt),
                ListenerStatus::WorkCompleted,
                None,
            ))
        }
        (Some(pane), Some(agent), _) => {
            let (status, secs) = agent_status(agent, now);
            Some((
                Association::Pane {
                    endpoint: pane.endpoint.clone(),
                    pane_id: pane.pane_id.clone(),
                    agent_kind: format!("{:?}", agent.kind).to_lowercase(),
                    agent_session_id: agent.session_id.clone(),
                },
                status,
                secs,
            ))
        }
        (_, _, Some(receipt)) => {
            Some((worktree_association(receipt), ListenerStatus::Unknown, None))
        }
        _ => None,
    }
}

/// Everything a listener can be associated through, read once per scan.
struct Evidence<'a> {
    panes: HashMap<u32, Option<PaneRoot>>,
    agents: &'a [Agent],
    receipts: Vec<WorktreeReceipt>,
    runs: Vec<crate::pipeline_run::PipelineRun>,
    dispatches: Option<PathBuf>,
}

impl<'a> Evidence<'a> {
    fn gather(agents: &'a AgentsView, gaps: &mut Vec<String>) -> Self {
        let panes = pane_roots(gaps);
        let agents = match agents {
            Ok(agents) => agents.as_slice(),
            Err(error) => {
                gaps.push(format!("could not read tend's agents: {error}"));
                &[]
            }
        };
        let receipts: Vec<WorktreeReceipt> = crate::worktree_receipt::list()
            .into_iter()
            .filter(|receipt| {
                matches!(
                    receipt.state,
                    ReceiptState::Created | ReceiptState::SetAside
                )
            })
            .filter(|receipt| receipt.pending.is_none())
            .collect();
        let runs = match crate::paths::default_pipeline_run_file() {
            Some(path) => crate::pipeline_run::read_runs(&path).unwrap_or_else(|error| {
                gaps.push(format!("could not read Work state: {error}"));
                Vec::new()
            }),
            None => Vec::new(),
        };
        let dispatches = crate::paths::default_node_id_file()
            .and_then(|file| file.parent().map(Path::to_path_buf))
            .map(|data| data.join("dispatches").join("worker"));
        Self {
            panes,
            agents,
            receipts,
            runs,
            dispatches,
        }
    }
}

/// Completed Work first, then agents that stopped or sat idle longest, then
/// the rest; ties by pid, so the order (and any cut) is stable.
fn sort_rows(rows: &mut [ListenerRow]) {
    let rank = |status: ListenerStatus| match status {
        ListenerStatus::WorkCompleted => 0,
        ListenerStatus::AgentStopped => 1,
        ListenerStatus::AgentIdle => 2,
        ListenerStatus::AgentError => 3,
        ListenerStatus::Unknown => 4,
        ListenerStatus::AgentNeedsInput => 5,
        ListenerStatus::AgentWorking => 6,
    };
    rows.sort_by(|a, b| {
        rank(a.status)
            .cmp(&rank(b.status))
            .then(
                b.state_for_secs
                    .unwrap_or(0)
                    .cmp(&a.state_for_secs.unwrap_or(0)),
            )
            .then(a.pid.cmp(&b.pid))
    });
}

fn agent_status(agent: &Agent, now: OffsetDateTime) -> (ListenerStatus, Option<i64>) {
    let secs = Some((now - agent.state_entered_at).whole_seconds().max(0));
    match agent.state {
        AgentState::Stopped => (ListenerStatus::AgentStopped, secs),
        AgentState::Idle => (ListenerStatus::AgentIdle, secs),
        AgentState::Error => (ListenerStatus::AgentError, secs),
        AgentState::WaitingInput | AgentState::WaitingChoice => {
            (ListenerStatus::AgentNeedsInput, None)
        }
        AgentState::Starting | AgentState::Working => (ListenerStatus::AgentWorking, None),
    }
}

fn worktree_association(receipt: &WorktreeReceipt) -> Association {
    let origin = match &receipt.origin {
        crate::worktree_receipt::ReceiptOrigin::Work { workspace, work } => match workspace {
            Some(ws) => format!("Work {ws}/{work}"),
            None => format!("Work {work}"),
        },
        crate::worktree_receipt::ReceiptOrigin::Dispatch {
            workspace, work, ..
        } => format!("Dispatch {workspace}/{work}"),
    };
    Association::Worktree {
        receipt_id: receipt.id.clone(),
        path: receipt.location().to_path_buf(),
        origin,
    }
}

/// The most specific receipted worktree holding `cwd`, still carrying its
/// receipt's mark.
fn worktree_for<'a>(
    cwd: &Path,
    receipts: &'a [WorktreeReceipt],
    owned: &mut HashMap<String, bool>,
) -> Option<&'a WorktreeReceipt> {
    let cwd = lenient_canonical(cwd);
    let mut matches: Vec<&WorktreeReceipt> = receipts
        .iter()
        .filter(|receipt| cwd.starts_with(lenient_canonical(receipt.location())))
        .collect();
    matches.sort_by_key(|receipt| std::cmp::Reverse(receipt.location().as_os_str().len()));
    matches.into_iter().find(|receipt| {
        *owned
            .entry(receipt.id.clone())
            .or_insert_with(|| receipt.owns_at(receipt.location()) == Some(true))
    })
}

/// A pane's first process, by its server endpoint and pane id.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PaneRoot {
    endpoint: String,
    pane_id: String,
}

/// Pane first-process pids on every active backend. A pid two different
/// panes claim is dropped as ambiguous.
fn pane_roots(gaps: &mut Vec<String>) -> HashMap<u32, Option<PaneRoot>> {
    let mut roots: HashMap<u32, Option<PaneRoot>> = HashMap::new();
    for backend in crate::active_backends() {
        let observation = backend.observe_panes();
        match observation.completeness {
            crate::backend::ObservationCompleteness::Complete => {}
            crate::backend::ObservationCompleteness::Partial => gaps.push(format!(
                "{:?} does not list its panes, so listeners under them are found only through worktrees",
                backend.kind()
            )),
            crate::backend::ObservationCompleteness::Incomplete => {
                gaps.push(format!("{:?} did not list every pane", backend.kind()));
            }
        }
        for pane in observation.panes {
            if pane.pane_pid == 0 {
                continue;
            }
            let root = PaneRoot {
                endpoint: crate::backend::pane_endpoint_identity(
                    Some(&pane.pane_id),
                    pane.socket.as_deref().unwrap_or(""),
                ),
                pane_id: pane.pane_id.clone(),
            };
            match roots.get(&pane.pane_pid) {
                None => {
                    roots.insert(pane.pane_pid, Some(root));
                }
                Some(Some(existing)) if *existing != root => {
                    roots.insert(pane.pane_pid, None);
                }
                _ => {}
            }
        }
    }
    roots
}

/// The pane whose first process is `pid` or one of its ancestors, each
/// ancestor older than the process below it (a younger "parent" means the
/// pid was reused).
fn pane_ancestor(
    pid: u32,
    table: &HashMap<u32, Proc>,
    roots: &HashMap<u32, Option<PaneRoot>>,
) -> Option<PaneRoot> {
    let mut current = pid;
    let mut seen = std::collections::HashSet::new();
    for _ in 0..64 {
        if !seen.insert(current) {
            return None;
        }
        if let Some(root) = roots.get(&current) {
            return root.clone();
        }
        let process = table.get(&current)?;
        let parent = process.ppid;
        if parent == 0 || parent == current {
            return None;
        }
        let parent_process = table.get(&parent)?;
        if let (Some(child), Some(older)) = (process.started_at, parent_process.started_at) {
            if older > child {
                return None;
            }
        }
        current = parent;
    }
    None
}

/// The one agent tend tracks on that pane; none when there is none, or more
/// than one could be it.
fn tracked_agent<'a>(pane: &PaneRoot, agents: &'a [Agent]) -> Option<&'a Agent> {
    let mut found = agents.iter().filter(|agent| {
        agent.pane.as_deref() == Some(pane.pane_id.as_str())
            && crate::backend::pane_endpoint_identity(
                Some(&pane.pane_id),
                agent.tmux_socket.as_deref().unwrap_or(""),
            ) == pane.endpoint
    });
    let first = found.next()?;
    found.next().is_none().then_some(first)
}

/// Control characters replaced, so a name or path cannot rewrite a terminal.
#[must_use]
pub fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// One process, as much as tend reads of it.
#[derive(Debug, Clone)]
struct Proc {
    ppid: u32,
    uid: Option<u32>,
    start: Option<String>,
    started_at: Option<i64>,
    name: String,
    cwd: Option<PathBuf>,
}

#[cfg(target_os = "linux")]
fn own_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata("/proc/self").ok().map(|meta| meta.uid())
}

#[cfg(not(target_os = "linux"))]
fn own_uid() -> Option<u32> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let pid = sysinfo::get_current_pid().ok()?;
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().with_user(UpdateKind::Always),
    );
    system.process(pid)?.user_id().map(|uid| **uid)
}

#[cfg(target_os = "linux")]
fn process_table() -> Option<HashMap<u32, Proc>> {
    use std::os::unix::fs::MetadataExt as _;
    let boot = std::fs::read_to_string("/proc/stat").ok().and_then(|stat| {
        stat.lines()
            .find_map(|line| line.strip_prefix("btime "))
            .and_then(|value| value.trim().parse::<i64>().ok())
    });
    let mut table = HashMap::new();
    for entry in std::fs::read_dir("/proc").ok()? {
        let Ok(entry) = entry else { continue };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else {
            continue;
        };
        let name = stat[open + 1..close].to_string();
        let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
        let parent = fields.get(1).and_then(|f| f.parse().ok()).unwrap_or(0);
        let ticks = fields.get(19).and_then(|f| f.parse::<i64>().ok());
        table.insert(
            pid,
            Proc {
                ppid: parent,
                uid: entry.metadata().ok().map(|meta| meta.uid()),
                start: ticks.map(|ticks| format!("ticks:{ticks}")),
                // Clock ticks are 100 per second on every Linux tend runs on.
                started_at: boot.zip(ticks).map(|(boot, ticks)| boot + ticks / 100),
                name,
                cwd: std::fs::read_link(entry.path().join("cwd")).ok(),
            },
        );
    }
    Some(table)
}

#[cfg(not(target_os = "linux"))]
fn process_table() -> Option<HashMap<u32, Proc>> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_cwd(UpdateKind::Always)
            .with_user(UpdateKind::Always),
    );
    let table: HashMap<u32, Proc> = system
        .processes()
        .iter()
        .map(|(pid, process)| {
            let started = i64::try_from(process.start_time()).ok().filter(|s| *s > 0);
            (
                pid.as_u32(),
                Proc {
                    ppid: process.parent().map_or(0, sysinfo::Pid::as_u32),
                    uid: process.user_id().map(|uid| **uid),
                    start: started.map(|s| format!("epoch:{s}")),
                    started_at: started,
                    name: process.name().to_string_lossy().into_owned(),
                    cwd: process.cwd().map(Path::to_path_buf),
                },
            )
        })
        .collect();
    (!table.is_empty()).then_some(table)
}

/// Every TCP listening endpoint this user's processes hold, by pid.
fn listening_sockets(me: u32) -> Result<Vec<(u32, Vec<Endpoint>)>, String> {
    #[cfg(target_os = "linux")]
    {
        if let Some(run) = worktree_inventory::bounded_status("ss", &["-Hltnp".as_ref()]) {
            if run.code == Some(0) {
                return Ok(group(parse_ss(&run.stdout)));
            }
            return Err(format!("ss failed: {}", first_line(&run.stderr)));
        }
    }
    let uid = me.to_string();
    let run = worktree_inventory::bounded_status(
        "lsof",
        &[
            "-nPw".as_ref(),
            "-a".as_ref(),
            "-u".as_ref(),
            uid.as_ref(),
            "-iTCP".as_ref(),
            "-sTCP:LISTEN".as_ref(),
            "-F0pn".as_ref(),
        ],
    )
    .ok_or("lsof could not run, ran out of time or printed too much")?;
    match run.code {
        Some(0) => Ok(group(parse_lsof(&run.stdout))),
        // lsof exits 1 both when nothing matched and on errors; only an
        // empty, silent run is "nothing matched".
        Some(1) if run.stdout.trim().is_empty() && run.stderr.trim().is_empty() => Ok(Vec::new()),
        _ => Err(format!("lsof failed: {}", first_line(&run.stderr))),
    }
}

fn first_line(text: &str) -> String {
    printable(text.lines().next().unwrap_or("no message").trim())
}

fn group(pairs: Vec<(u32, Endpoint)>) -> Vec<(u32, Vec<Endpoint>)> {
    let mut by_pid: Vec<(u32, Vec<Endpoint>)> = Vec::new();
    for (pid, endpoint) in pairs {
        match by_pid.iter_mut().find(|(p, _)| *p == pid) {
            Some((_, endpoints)) => {
                if !endpoints.contains(&endpoint) {
                    endpoints.push(endpoint);
                }
            }
            None => by_pid.push((pid, vec![endpoint])),
        }
    }
    by_pid
}

/// `lsof -F0pn` output: a `p<pid>` field starts a process, each `n<addr>`
/// field after it is one of its listening sockets.
fn parse_lsof(out: &str) -> Vec<(u32, Endpoint)> {
    let mut pairs = Vec::new();
    let mut pid = None;
    for field in out.split('\0').map(|f| f.trim_start_matches('\n')) {
        if let Some(value) = field.strip_prefix('p') {
            pid = value.parse::<u32>().ok();
        } else if let (Some(value), Some(pid)) = (field.strip_prefix('n'), pid) {
            if let Some(endpoint) = parse_endpoint(value) {
                pairs.push((pid, endpoint));
            }
        }
    }
    pairs
}

/// `ss -Hltnp` lines: the fourth column is the local address; every
/// `pid=` in the process column owns the socket.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_ss(out: &str) -> Vec<(u32, Endpoint)> {
    let mut pairs = Vec::new();
    for line in out.lines() {
        let columns: Vec<&str> = line.split_whitespace().collect();
        let Some(endpoint) = columns.get(3).and_then(|local| parse_endpoint(local)) else {
            continue;
        };
        let mut rest = line;
        while let Some(at) = rest.find("pid=") {
            rest = &rest[at + 4..];
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(pid) = digits.parse::<u32>() {
                pairs.push((pid, endpoint.clone()));
            }
        }
    }
    pairs
}

/// `host:port`, with IPv6 in brackets and an optional `%zone`.
fn parse_endpoint(text: &str) -> Option<Endpoint> {
    let (host, port) = text.rsplit_once(':')?;
    let port = port.parse::<u16>().ok()?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let bare = host.split('%').next().unwrap_or(host);
    Some(Endpoint {
        address: host.to_string(),
        port,
        bind: bind_of(bare),
    })
}

fn bind_of(host: &str) -> Bind {
    if matches!(host, "*" | "0.0.0.0" | "::" | "") {
        return Bind::Wildcard;
    }
    let v4 = host
        .strip_prefix("::ffff:")
        .or_else(|| host.strip_prefix("::FFFF:"))
        .unwrap_or(host);
    match v4.parse::<std::net::IpAddr>() {
        Ok(ip) if ip.is_loopback() => Bind::Loopback,
        Ok(ip) if ip.is_unspecified() => Bind::Wildcard,
        Err(_) if host == "localhost" => Bind::Loopback,
        _ => Bind::Interface,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_parse_and_bind_kinds_are_honest() {
        let e = parse_endpoint("*:3000").unwrap();
        assert_eq!((e.port, e.bind), (3000, Bind::Wildcard));
        assert_eq!(
            parse_endpoint("127.0.0.53:53").unwrap().bind,
            Bind::Loopback
        );
        assert_eq!(parse_endpoint("[::1]:8080").unwrap().bind, Bind::Loopback);
        assert_eq!(
            parse_endpoint("[::ffff:127.0.0.1]:9").unwrap().bind,
            Bind::Loopback
        );
        assert_eq!(parse_endpoint("[::]:22").unwrap().bind, Bind::Wildcard);
        assert_eq!(
            parse_endpoint("0.0.0.0%lo:53").unwrap().bind,
            Bind::Wildcard
        );
        let zoned = parse_endpoint("[fe80::1%lo0]:5000").unwrap();
        assert_eq!(
            (zoned.address.as_str(), zoned.bind),
            ("fe80::1%lo0", Bind::Interface)
        );
        assert_eq!(
            parse_endpoint("192.168.1.4:5173").unwrap().bind,
            Bind::Interface
        );
        assert!(parse_endpoint("nonsense").is_none());
    }

    #[test]
    fn lsof_and_ss_output_keep_every_owner_and_endpoint() {
        let lsof = "p10\0\nf20\0n*:3000\0\nf21\0n[::1]:3000\0\np11\0\nf5\0n127.0.0.1:5173\0\n";
        let grouped = group(parse_lsof(lsof));
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[0].0, 10);
        assert_eq!(grouped[0].1.len(), 2);
        let ss = "LISTEN 0 511 0.0.0.0:3000 0.0.0.0:* users:((\"node\",pid=7,fd=20),(\"node\",pid=8,fd=20))\n\
                  LISTEN 0 128 [::]:22 [::]:* \n";
        let pairs = parse_ss(ss);
        assert_eq!(
            pairs.iter().map(|(pid, _)| *pid).collect::<Vec<_>>(),
            vec![7, 8]
        );
    }

    fn proc(ppid: u32, started_at: i64) -> Proc {
        Proc {
            ppid,
            uid: Some(1),
            start: Some(format!("epoch:{started_at}")),
            started_at: Some(started_at),
            name: "node".into(),
            cwd: None,
        }
    }

    #[test]
    fn a_pane_is_found_through_older_ancestors_only() {
        let root = PaneRoot {
            endpoint: "default".into(),
            pane_id: "%3".into(),
        };
        let roots = HashMap::from([(100, Some(root.clone())), (200, None)]);
        let mut table = HashMap::from([
            (100, proc(1, 10)),
            (101, proc(100, 20)),
            (102, proc(101, 30)),
        ]);
        assert_eq!(pane_ancestor(102, &table, &roots), Some(root));
        // A "parent" younger than its child is a reused pid.
        table.insert(101, proc(100, 40));
        assert_eq!(pane_ancestor(102, &table, &roots), None);
        // A pid two panes claim is ambiguous; a cycle ends the walk.
        table.insert(201, proc(200, 50));
        assert_eq!(pane_ancestor(201, &table, &roots), None);
        table.insert(300, proc(301, 60));
        table.insert(301, proc(300, 60));
        assert_eq!(pane_ancestor(300, &table, &roots), None);
    }

    #[test]
    fn rows_sort_completed_and_long_idle_first_and_names_are_printable() {
        let row = |pid, status, secs| ListenerRow {
            pid,
            process_start: None,
            started_at: None,
            uid: 1,
            process_name: printable("no\u{1b}[2Jde"),
            cwd: None,
            endpoints: vec![],
            association: Association::Worktree {
                receipt_id: "r".into(),
                path: "/w".into(),
                origin: "Work w".into(),
            },
            status,
            state_for_secs: secs,
        };
        let mut rows = vec![
            row(5, ListenerStatus::AgentWorking, None),
            row(4, ListenerStatus::AgentIdle, Some(60)),
            row(3, ListenerStatus::AgentIdle, Some(600)),
            row(2, ListenerStatus::WorkCompleted, None),
        ];
        sort_rows(&mut rows);
        assert_eq!(
            rows.iter().map(|r| r.pid).collect::<Vec<_>>(),
            vec![2, 3, 4, 5]
        );
        assert_eq!(rows[0].process_name, "no?[2Jde");
    }

    fn agent(pane: &str, socket: &str, state: &str, entered_secs_ago: i64) -> Agent {
        let entered = OffsetDateTime::now_utc() - time::Duration::seconds(entered_secs_ago);
        let at = entered
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        serde_json::from_value(serde_json::json!({
            "kind": "codex", "agent_session_id": format!("s-{pane}-{socket}"),
            "pane": pane, "tmux_socket": socket, "cwd": null, "state": state,
            "last_prompt": null, "last_response": null, "last_notification": null,
            "model": null, "context_used_pct": null, "cost_usd": null,
            "started_at": at, "last_activity_at": at, "state_entered_at": at
        }))
        .unwrap()
    }

    fn evidence(agents: &[Agent], roots: HashMap<u32, Option<PaneRoot>>) -> Evidence<'_> {
        Evidence {
            panes: roots,
            agents,
            receipts: vec![],
            runs: vec![],
            dispatches: None,
        }
    }

    #[test]
    fn a_server_under_an_idle_agents_pane_says_idle_not_finished() {
        let root = PaneRoot {
            endpoint: "default".into(),
            pane_id: "%3".into(),
        };
        let table = HashMap::from([(100, proc(1, 10)), (101, proc(100, 20))]);
        let agents = vec![agent("%3", "default", "idle", 3600)];
        let ev = evidence(&agents, HashMap::from([(100, Some(root.clone()))]));
        let mut cache = HashMap::new();
        let now = OffsetDateTime::now_utc();
        let (association, status, secs) =
            associate(101, &table[&101], &table, &ev, &mut cache, now).unwrap();
        assert!(matches!(association, Association::Pane { ref pane_id, .. } if pane_id == "%3"));
        assert_eq!(status, ListenerStatus::AgentIdle);
        assert!(secs.unwrap() >= 3600);

        // The same pane id on another tmux socket is a different pane.
        let elsewhere = vec![agent("%3", "work", "idle", 3600)];
        let ev = evidence(&elsewhere, HashMap::from([(100, Some(root.clone()))]));
        assert!(associate(101, &table[&101], &table, &ev, &mut cache, now).is_none());

        // No tracked agent on the pane (a person's own pane) and no
        // worktree: not tend's business.
        let ev = evidence(&[], HashMap::from([(100, Some(root.clone()))]));
        assert!(associate(101, &table[&101], &table, &ev, &mut cache, now).is_none());

        // Two agents claiming the pane: ambiguous, not guessed.
        let two = vec![
            agent("%3", "default", "idle", 10),
            agent("%3", "default", "working", 10),
        ];
        let ev = evidence(&two, HashMap::from([(100, Some(root.clone()))]));
        assert!(associate(101, &table[&101], &table, &ev, &mut cache, now).is_none());

        // Reparented away from the pane (parent is init): no pane evidence.
        let orphan = HashMap::from([(101, proc(1, 20))]);
        let ev = evidence(&agents, HashMap::from([(100, Some(root))]));
        assert!(associate(101, &orphan[&101], &orphan, &ev, &mut cache, now).is_none());

        // Waiting and working agents are never sorted as leftovers.
        assert_eq!(
            agent_status(&agent("%3", "default", "waiting_input", 9000), now).0,
            ListenerStatus::AgentNeedsInput
        );
        assert_eq!(
            agent_status(&agent("%3", "default", "working", 9000), now).0,
            ListenerStatus::AgentWorking
        );
    }
}
