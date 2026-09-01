//! Canonical work orchestration domain shared by dashboard surfaces.
//!
//! A work item is a durable, muxa-owned outcome. It may reference an item in
//! Linear, GitHub, Jira, or another tracker, but it is never the tracker item
//! itself. Runs and agent sessions are execution attempts; tmux windows and
//! panes are only their current bindings.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::backend::{pane_endpoint_identity, HostKind};
use crate::event::AgentState;
use crate::state::Agent;
use crate::tmux::scanner::PaneSummary;

pub const WORK_SCHEMA_VERSION: u8 = 2;

/// Stable muxa identity. Unlike a window id, this survives closing and
/// recreating an execution surface.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct WorkIdentity {
    pub workspace_id: String,
    pub work_id: String,
}

impl WorkIdentity {
    #[must_use]
    pub fn new(workspace_id: impl Into<String>, work_id: impl Into<String>) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            work_id: work_id.into(),
        }
    }

    #[must_use]
    pub fn key(&self) -> String {
        format!("{}/{}", self.workspace_id, self.work_id)
    }
}

/// An optional external source attached to a muxa work item.
///
/// `display_key` is the human identifier (`CAL-123`, `#79`). `stable_id`
/// carries the provider's immutable id when one is available. Keeping both
/// avoids using a mutable or repository-local label as a global identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalItemRef {
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stable_id: Option<String>,
    pub display_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_type: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub synced_at: OffsetDateTime,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkStage {
    #[default]
    Auto,
    Queued,
    InProgress,
    Review,
    /// Kept for v1 compatibility. v2 renders this as a blocker signal on a
    /// queued/running work item rather than as a board lane.
    Blocked,
    Done,
}

impl WorkStage {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Queued => "queued",
            Self::InProgress => "in_progress",
            Self::Review => "review",
            Self::Blocked => "blocked",
            Self::Done => "done",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoardStage {
    Queued,
    InProgress,
    Review,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkSignal {
    Attention,
    Blocked,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Starting,
    Running,
    Waiting,
    Idle,
    Failed,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_action: Option<String>,
    #[serde(default)]
    pub stage: WorkStage,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WorkMetadataPatch {
    pub title: Option<String>,
    pub goal: Option<String>,
    pub next_action: Option<String>,
    #[serde(default)]
    pub stage: WorkStage,
}

impl WorkMetadataPatch {
    pub fn validate(self) -> Result<Self, String> {
        Ok(Self {
            title: clean(self.title, 160, "title")?,
            goal: clean(self.goal, 4_000, "goal")?,
            next_action: clean(self.next_action, 1_000, "next action")?,
            stage: self.stage,
        })
    }

    #[must_use]
    pub fn into_metadata(self, updated_at: OffsetDateTime) -> WorkMetadata {
        WorkMetadata {
            title: self.title,
            goal: self.goal,
            next_action: self.next_action,
            stage: self.stage,
            updated_at,
        }
    }
}

fn clean(value: Option<String>, max: usize, label: &str) -> Result<Option<String>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > max {
        return Err(format!("{label} is too long (max {max} bytes)"));
    }
    if value.chars().any(|ch| ch == '\0') {
        return Err(format!("{label} cannot contain NUL"));
    }
    Ok(Some(value.to_string()))
}

/// Durable muxa-owned record. Live execution bindings are deliberately not
/// the primary key and can disappear without deleting this record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkRecord {
    pub identity: WorkIdentity,
    pub metadata: WorkMetadata,
    #[serde(default)]
    pub external_items: Vec<ExternalItemRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_binding: Option<ExecutionIdentity>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ExecutionIdentity {
    pub host: HostKind,
    pub socket: String,
    pub session_id: String,
    pub window_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkSnapshot {
    pub schema_version: u8,
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
    pub workspaces: Vec<WorkspaceSnapshot>,
    pub works: Vec<WorkSnapshotItem>,
    pub unlinked_executions: Vec<RunSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub work_count: usize,
    pub attention_count: usize,
    pub active_runs: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkSnapshotItem {
    pub identity: WorkIdentity,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_action: Option<String>,
    pub stage: BoardStage,
    pub signals: Vec<WorkSignal>,
    pub external_items: Vec<ExternalItemRef>,
    pub runs: Vec<RunSnapshot>,
    pub participants: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_at: Option<OffsetDateTime>,
    pub source: WorkSource,
    pub metadata: WorkMetadata,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkSource {
    Managed,
    Persisted,
    Migrated,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSnapshot {
    pub id: String,
    pub state: RunState,
    pub linked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work: Option<WorkIdentity>,
    pub execution: ExecutionIdentity,
    pub session_name: String,
    pub window_name: String,
    pub window_index: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub panes: Vec<RunPaneSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunPaneSnapshot {
    pub pane_id: String,
    pub pane_index: String,
    pub current_command: String,
    pub title: String,
    pub current_path: String,
    pub attach_command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<Agent>,
}

#[derive(Debug)]
struct RunBuilder<'a> {
    execution: ExecutionIdentity,
    linked_work: Option<WorkIdentity>,
    session_name: String,
    window_name: String,
    window_index: String,
    cwd: Option<String>,
    panes: Vec<&'a PaneSummary>,
}

/// Build the one canonical work projection used by HTTP, tests, and the CLI.
/// Unmanaged windows never become work items implicitly.
#[must_use]
pub fn build_snapshot(
    panes: &[PaneSummary],
    agents: &[Agent],
    records: &[WorkRecord],
    generated_at: OffsetDateTime,
) -> WorkSnapshot {
    let agent_index = AgentIndex::new(panes, agents);
    let mut runs = BTreeMap::<ExecutionIdentity, RunBuilder<'_>>::new();
    for pane in panes {
        let execution = execution_identity(pane);
        let linked_work = work_identity_for_pane(pane).or_else(|| {
            records.iter().find_map(|record| {
                (record.legacy_binding.as_ref() == Some(&execution))
                    .then(|| record.identity.clone())
            })
        });
        let builder = runs.entry(execution.clone()).or_insert_with(|| RunBuilder {
            execution,
            linked_work: linked_work.clone(),
            session_name: pane.session.clone(),
            window_name: pane.window_name.clone(),
            window_index: pane.window_index.clone(),
            cwd: pane
                .muxa
                .work_cwd
                .clone()
                .or_else(|| (!pane.current_path.is_empty()).then(|| pane.current_path.clone())),
            panes: Vec::new(),
        });
        if builder.linked_work.is_none() {
            builder.linked_work = linked_work;
        }
        builder.panes.push(pane);
    }

    let mut linked_runs = BTreeMap::<WorkIdentity, Vec<RunSnapshot>>::new();
    let mut unlinked_executions = Vec::new();
    for builder in runs.into_values() {
        let snapshot = finish_run(builder, &agent_index);
        if let Some(identity) = snapshot.work.clone() {
            linked_runs.entry(identity).or_default().push(snapshot);
        } else {
            unlinked_executions.push(snapshot);
        }
    }

    let works = build_work_items(records, linked_runs, generated_at);
    unlinked_executions.sort_by(|left, right| {
        right
            .latest_at
            .cmp(&left.latest_at)
            .then_with(|| left.session_name.cmp(&right.session_name))
            .then_with(|| left.window_index.cmp(&right.window_index))
    });

    let workspaces = build_workspace_rows(&works);

    WorkSnapshot {
        schema_version: WORK_SCHEMA_VERSION,
        generated_at,
        workspaces,
        works,
        unlinked_executions,
    }
}

fn build_work_items(
    records: &[WorkRecord],
    mut linked_runs: BTreeMap<WorkIdentity, Vec<RunSnapshot>>,
    now: OffsetDateTime,
) -> Vec<WorkSnapshotItem> {
    let records_by_id = records
        .iter()
        .map(|record| (record.identity.clone(), record))
        .collect::<BTreeMap<_, _>>();
    let identities = records_by_id
        .keys()
        .cloned()
        .chain(linked_runs.keys().cloned())
        .collect::<BTreeSet<_>>();
    let mut works = identities
        .into_iter()
        .map(|identity| {
            let record = records_by_id.get(&identity).copied();
            let mut runs = linked_runs.remove(&identity).unwrap_or_default();
            runs.sort_by_key(|run| std::cmp::Reverse(run.latest_at));
            finish_work(identity, record, runs, now)
        })
        .collect::<Vec<_>>();
    works.sort_by(compare_work);
    works
}

fn finish_work(
    identity: WorkIdentity,
    record: Option<&WorkRecord>,
    runs: Vec<RunSnapshot>,
    now: OffsetDateTime,
) -> WorkSnapshotItem {
    let metadata = record.map_or_else(
        || WorkMetadata {
            title: None,
            goal: None,
            next_action: None,
            stage: WorkStage::Auto,
            updated_at: now,
        },
        |record| record.metadata.clone(),
    );
    let runtime = RuntimeSummary::from_runs(&runs);
    let title = metadata
        .title
        .clone()
        .or_else(|| {
            record
                .and_then(|record| record.external_items.first())
                .and_then(|item| item.title.clone())
        })
        .unwrap_or_else(|| identity.work_id.clone());
    let source = match record {
        Some(record) if record.legacy_binding.is_some() => WorkSource::Migrated,
        Some(_) if runs.is_empty() => WorkSource::Persisted,
        _ => WorkSource::Managed,
    };
    WorkSnapshotItem {
        identity,
        title,
        goal: metadata.goal.clone(),
        next_action: metadata.next_action.clone(),
        stage: board_stage(metadata.stage, &runtime),
        signals: work_signals(metadata.stage, &runtime),
        external_items: record
            .map(|record| record.external_items.clone())
            .unwrap_or_default(),
        participants: runtime.participants,
        latest_at: runtime.latest_at,
        runs,
        source,
        metadata,
    }
}

fn build_workspace_rows(works: &[WorkSnapshotItem]) -> Vec<WorkspaceSnapshot> {
    let mut rows = BTreeMap::<String, WorkspaceSnapshot>::new();
    for work in works {
        let row = rows
            .entry(work.identity.workspace_id.clone())
            .or_insert_with(|| WorkspaceSnapshot {
                id: work.identity.workspace_id.clone(),
                name: work.identity.workspace_id.clone(),
                cwd: work.runs.iter().find_map(|run| run.cwd.clone()),
                work_count: 0,
                attention_count: 0,
                active_runs: 0,
            });
        row.work_count += 1;
        row.attention_count += usize::from(!work.signals.is_empty());
        row.active_runs += work
            .runs
            .iter()
            .filter(|run| {
                matches!(
                    run.state,
                    RunState::Starting | RunState::Running | RunState::Waiting
                )
            })
            .count();
    }
    let mut workspaces = rows.into_values().collect::<Vec<_>>();
    workspaces.sort_by(|left, right| {
        right
            .attention_count
            .cmp(&left.attention_count)
            .then_with(|| right.active_runs.cmp(&left.active_runs))
            .then_with(|| left.name.cmp(&right.name))
    });
    workspaces
}

#[must_use]
pub fn work_identity_for_pane(pane: &PaneSummary) -> Option<WorkIdentity> {
    if !pane.muxa.managed_work {
        return None;
    }
    Some(WorkIdentity::new(
        pane.muxa.workspace_id.as_ref()?.clone(),
        pane.muxa.work_id.as_ref()?.clone(),
    ))
}

#[must_use]
pub fn external_item_for_pane(
    pane: &PaneSummary,
    synced_at: OffsetDateTime,
) -> Option<(WorkIdentity, ExternalItemRef)> {
    let identity = work_identity_for_pane(pane)?;
    let source = pane.muxa.external_source.as_ref()?.trim();
    let display_key = pane
        .muxa
        .external_key
        .as_deref()
        .unwrap_or(&identity.work_id)
        .trim()
        .to_string();
    if source.is_empty() || display_key.is_empty() {
        return None;
    }
    Some((
        identity,
        ExternalItemRef {
            source: source.to_string(),
            scope: pane.muxa.external_scope.clone(),
            stable_id: pane.muxa.external_stable_id.clone(),
            display_key,
            title: pane.muxa.external_title.clone(),
            url: pane.muxa.external_url.clone(),
            status: pane.muxa.external_status.clone(),
            item_type: Some("issue".into()),
            synced_at,
        },
    ))
}

#[must_use]
pub fn execution_identity(pane: &PaneSummary) -> ExecutionIdentity {
    ExecutionIdentity {
        host: pane.host,
        socket: pane_socket_identity(pane),
        session_id: pane.session_id.clone(),
        window_id: pane.window_id.clone(),
    }
}

#[must_use]
pub fn pane_socket_identity(pane: &PaneSummary) -> String {
    if pane.host == HostKind::Tmux {
        pane.socket
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("default")
            .to_string()
    } else {
        pane.socket.to_string_lossy().to_string()
    }
}

fn finish_run(builder: RunBuilder<'_>, agent_index: &AgentIndex<'_>) -> RunSnapshot {
    let mut panes = builder
        .panes
        .into_iter()
        .map(|pane| RunPaneSnapshot {
            pane_id: pane.pane_id.clone(),
            pane_index: pane.pane_index.clone(),
            current_command: pane.current_command.clone(),
            title: pane.title.clone(),
            current_path: pane.current_path.clone(),
            attach_command: pane.attach_command.clone(),
            role: pane.muxa.role.clone(),
            task: pane.muxa.task.clone(),
            agent: agent_index.for_pane(pane).cloned(),
        })
        .collect::<Vec<_>>();
    panes.sort_by(|left, right| left.pane_index.cmp(&right.pane_index));
    let states = panes
        .iter()
        .filter_map(|pane| pane.agent.as_ref().map(|agent| agent.state))
        .collect::<Vec<_>>();
    let latest_at = panes
        .iter()
        .filter_map(|pane| pane.agent.as_ref().map(|agent| agent.last_activity_at))
        .max();
    let state = run_state(&states);
    let id = format!(
        "{}:{}:{}:{}",
        builder.execution.host,
        builder.execution.socket,
        builder.execution.session_id,
        builder.execution.window_id
    );
    RunSnapshot {
        id,
        state,
        linked: builder.linked_work.is_some(),
        work: builder.linked_work,
        execution: builder.execution,
        session_name: builder.session_name,
        window_name: builder.window_name,
        window_index: builder.window_index,
        cwd: builder.cwd,
        panes,
        latest_at,
    }
}

fn run_state(states: &[AgentState]) -> RunState {
    if states.contains(&AgentState::Error) {
        RunState::Failed
    } else if states
        .iter()
        .any(|state| matches!(state, AgentState::WaitingInput | AgentState::WaitingChoice))
    {
        RunState::Waiting
    } else if states.contains(&AgentState::Working) {
        RunState::Running
    } else if states.contains(&AgentState::Starting) {
        RunState::Starting
    } else if !states.is_empty() && states.iter().all(|state| *state == AgentState::Stopped) {
        RunState::Completed
    } else {
        RunState::Idle
    }
}

#[derive(Default)]
struct RuntimeSummary {
    participants: usize,
    working: usize,
    waiting: usize,
    errors: usize,
    latest_at: Option<OffsetDateTime>,
}

impl RuntimeSummary {
    fn from_runs(runs: &[RunSnapshot]) -> Self {
        let mut summary = Self::default();
        for run in runs {
            summary.latest_at = summary.latest_at.max(run.latest_at);
            for pane in &run.panes {
                let Some(agent) = &pane.agent else {
                    continue;
                };
                summary.participants += 1;
                match agent.state {
                    AgentState::Working | AgentState::Starting => summary.working += 1,
                    AgentState::WaitingInput | AgentState::WaitingChoice => summary.waiting += 1,
                    AgentState::Error => summary.errors += 1,
                    AgentState::Idle | AgentState::Stopped => {}
                }
            }
        }
        summary
    }
}

fn board_stage(stage: WorkStage, runtime: &RuntimeSummary) -> BoardStage {
    match stage {
        WorkStage::Done => BoardStage::Done,
        WorkStage::Review => BoardStage::Review,
        WorkStage::InProgress => BoardStage::InProgress,
        WorkStage::Queued => BoardStage::Queued,
        WorkStage::Blocked => {
            if runtime.participants > 0 {
                BoardStage::InProgress
            } else {
                BoardStage::Queued
            }
        }
        WorkStage::Auto => {
            if runtime.working > 0 || runtime.waiting > 0 || runtime.errors > 0 {
                BoardStage::InProgress
            } else {
                BoardStage::Queued
            }
        }
    }
}

fn work_signals(stage: WorkStage, runtime: &RuntimeSummary) -> Vec<WorkSignal> {
    let mut signals = BTreeSet::new();
    if stage == WorkStage::Blocked {
        signals.insert(WorkSignal::Blocked);
    }
    if runtime.waiting > 0 {
        signals.insert(WorkSignal::Attention);
    }
    if runtime.errors > 0 {
        signals.insert(WorkSignal::Attention);
        signals.insert(WorkSignal::Error);
    }
    signals.into_iter().collect()
}

fn compare_work(left: &WorkSnapshotItem, right: &WorkSnapshotItem) -> std::cmp::Ordering {
    let priority = |work: &WorkSnapshotItem| {
        let signal = if work.signals.contains(&WorkSignal::Error) {
            3
        } else if work.signals.contains(&WorkSignal::Attention) {
            2
        } else {
            i32::from(work.signals.contains(&WorkSignal::Blocked))
        };
        let stage = match work.stage {
            BoardStage::InProgress => 4,
            BoardStage::Review => 3,
            BoardStage::Queued => 2,
            BoardStage::Done => 1,
        };
        (signal, stage)
    };
    priority(right)
        .cmp(&priority(left))
        .then_with(|| right.latest_at.cmp(&left.latest_at))
        .then_with(|| left.title.cmp(&right.title))
}

struct AgentIndex<'a> {
    exact: HashMap<(HostKind, String, String), &'a Agent>,
    by_pane: HashMap<String, Vec<&'a Agent>>,
    duplicate_panes: HashSet<(HostKind, String)>,
}

impl<'a> AgentIndex<'a> {
    fn new(panes: &[PaneSummary], agents: &'a [Agent]) -> Self {
        let mut counts = HashMap::<(HostKind, String), usize>::new();
        for pane in panes {
            *counts.entry((pane.host, pane.pane_id.clone())).or_default() += 1;
        }
        let duplicate_panes = counts
            .into_iter()
            .filter_map(|(key, count)| (count > 1).then_some(key))
            .collect();
        let mut exact = HashMap::new();
        let mut by_pane = HashMap::<String, Vec<&Agent>>::new();
        for agent in agents {
            let Some(pane) = agent.pane.as_ref() else {
                continue;
            };
            by_pane.entry(pane.clone()).or_default().push(agent);
            if let Some(socket) = agent.tmux_socket.as_deref() {
                let host = crate::backend::pane_id_host_kind(pane).unwrap_or(HostKind::Tmux);
                exact.insert(
                    (
                        host,
                        pane_endpoint_identity(Some(pane), socket),
                        pane.clone(),
                    ),
                    agent,
                );
            }
        }
        Self {
            exact,
            by_pane,
            duplicate_panes,
        }
    }

    fn for_pane(&self, pane: &PaneSummary) -> Option<&'a Agent> {
        let endpoint = pane_socket_identity(pane);
        self.exact
            .get(&(pane.host, endpoint, pane.pane_id.clone()))
            .copied()
            .or_else(|| {
                (!self
                    .duplicate_panes
                    .contains(&(pane.host, pane.pane_id.clone())))
                .then(|| self.by_pane.get(&pane.pane_id))
                .flatten()
                .and_then(|agents| (agents.len() == 1).then_some(agents[0]))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::event::AgentKind;
    use crate::process_tree::WorkloadSummary;
    use crate::tmux::scanner::MuxaPaneMetadata;

    fn pane(id: &str, window: &str, managed: bool) -> PaneSummary {
        PaneSummary {
            host: HostKind::Tmux,
            pane_id: id.into(),
            session_id: "$1".into(),
            session: "muxa".into(),
            window_id: window.into(),
            window_name: if managed { "CAL-1" } else { "node" }.into(),
            window_index: "1".into(),
            pane_index: "0".into(),
            tty: String::new(),
            current_command: "codex".into(),
            title: String::new(),
            current_path: "/repo".into(),
            socket: PathBuf::from("/tmp/tmux-1/default"),
            muxa: MuxaPaneMetadata {
                managed_workspace: managed,
                managed_work: managed,
                managed_agent: managed,
                workspace_id: managed.then(|| "muxa".into()),
                workspace_cwd: managed.then(|| "/repo".into()),
                work_id: managed.then(|| "CAL-1".into()),
                work_cwd: managed.then(|| "/repo".into()),
                agent: managed.then(|| "codex".into()),
                role: None,
                task: None,
                external_source: None,
                external_scope: None,
                external_stable_id: None,
                external_key: None,
                external_title: None,
                external_url: None,
                external_status: None,
            },
            attach_command: String::new(),
        }
    }

    fn agent(pane: &str, state: AgentState) -> Agent {
        let now = OffsetDateTime::now_utc();
        Agent {
            kind: AgentKind::Codex,
            session_id: format!("s-{pane}"),
            surface: None,
            pane: Some(pane.into()),
            tmux_socket: Some("default".into()),
            tmux_session: Some("muxa".into()),
            cwd: Some("/repo".into()),
            pid: None,
            workload: WorkloadSummary::default(),
            subagents: Vec::new(),
            state,
            last_prompt: None,
            last_prompt_at: None,
            last_response: None,
            recap: None,
            ai_title: None,
            last_notification: None,
            model: None,
            context_used_pct: None,
            cost_usd: None,
            rate_limit_5h_pct: None,
            rate_limit_5h_resets_at: None,
            rate_limit_7d_pct: None,
            rate_limit_7d_resets_at: None,
            rate_limited_until: None,
            rate_limit_scope: None,
            rate_limit_source: None,
            started_at: now,
            last_activity_at: now,
            state_entered_at: now,
        }
    }

    #[test]
    fn unmanaged_window_is_an_unlinked_execution_not_a_work_item() {
        let panes = vec![pane("%1", "@1", false)];
        let snapshot = build_snapshot(&panes, &[], &[], OffsetDateTime::now_utc());
        assert!(snapshot.works.is_empty());
        assert_eq!(snapshot.unlinked_executions.len(), 1);
        assert!(!snapshot.unlinked_executions[0].linked);
    }

    #[test]
    fn attention_is_a_signal_without_moving_the_work_out_of_progress() {
        let panes = vec![pane("%1", "@1", true)];
        let agents = vec![agent("%1", AgentState::WaitingInput)];
        let snapshot = build_snapshot(&panes, &agents, &[], OffsetDateTime::now_utc());
        assert_eq!(snapshot.works.len(), 1);
        assert_eq!(snapshot.works[0].stage, BoardStage::InProgress);
        assert!(snapshot.works[0].signals.contains(&WorkSignal::Attention));
    }
}
