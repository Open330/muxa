//! The Work board screen for `muxa watch`.
//!
//! Its rows are Works — persisted records joined with the Runs scanned for
//! them — not topology nodes. A Work outlives its windows and can have
//! several Runs, so this is a screen of its own rather than another `layout`
//! over the same node set. The rows come from [`muxa::work::build_snapshot`],
//! the same projection the web dashboard and `muxa dashboard` read, and the
//! cursor follows a Work's identity rather than its position, so a refresh
//! that reorders the board does not move the operator to another Work.

use std::cmp::Reverse;
use std::fmt::Write as _;

use muxa::event::AgentState;
use muxa::state::Agent;
use muxa::work::WorkSnapshotItem;
use muxa::work::{BoardStage, RunSnapshot, WorkIdentity, WorkSignal, WorkSnapshot};
use ratatui::layout::Constraint;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Row};
use time::OffsetDateTime;

use crate::collab_screen::age;
use crate::watch::WatchThemeSpec;

/// Agent glyphs shown per row before the rest collapse into `+n`.
const AGENT_GLYPHS: usize = 6;

#[derive(Debug, Default)]
pub(crate) struct BoardScreen {
    /// Attention first; see [`attention_order`].
    works: Vec<WorkSnapshotItem>,
    /// Executions no Work claims. They are not rows here (topology shows
    /// them), but the count is reported so the board never looks complete
    /// while hiding live work.
    unlinked: usize,
    /// The Work the cursor is on, by identity.
    selected: Option<WorkIdentity>,
    /// Scan problems that still left a board to show.
    pub(crate) notes: Vec<String>,
    /// A snapshot has been attempted; separates "loading" from "no Work".
    pub(crate) loaded: bool,
}

impl BoardScreen {
    pub(crate) fn set_snapshot(&mut self, snapshot: WorkSnapshot, notes: Vec<String>) {
        let mut works = snapshot.works;
        works.sort_by_key(attention_order);
        self.works = works;
        self.unlinked = snapshot.unlinked_executions.len();
        self.notes = notes;
        self.loaded = true;
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.works.is_empty()
    }

    pub(crate) fn unlinked(&self) -> usize {
        self.unlinked
    }

    /// The Works a filter leaves on screen, in board order.
    pub(crate) fn visible<'a>(&'a self, filter: &str) -> Vec<&'a WorkSnapshotItem> {
        let needle = filter.trim().to_lowercase();
        self.works
            .iter()
            .filter(|work| needle.is_empty() || matches_filter(work, &needle))
            .collect()
    }

    /// Index of the cursor among the visible Works. A selected Work the
    /// filter hides, or one that is gone, puts the cursor on the first row.
    pub(crate) fn selected_index(&self, filter: &str) -> usize {
        let Some(selected) = &self.selected else {
            return 0;
        };
        self.visible(filter)
            .iter()
            .position(|work| &work.identity == selected)
            .unwrap_or(0)
    }

    pub(crate) fn selected_work(&self, filter: &str) -> Option<&WorkSnapshotItem> {
        let visible = self.visible(filter);
        let index = self.selected_index(filter);
        visible.get(index).copied()
    }

    /// Move within the filtered board, saturating at both ends.
    pub(crate) fn move_selection(&mut self, delta: isize, filter: &str) {
        let visible = self.visible(filter);
        if visible.is_empty() {
            return;
        }
        let current = self.selected_index(filter);
        let last = visible.len() - 1;
        let next = if delta >= 0 {
            current.saturating_add(delta.unsigned_abs()).min(last)
        } else {
            current.saturating_sub(delta.unsigned_abs())
        };
        self.selected = Some(visible[next].identity.clone());
    }

    pub(crate) fn select_first(&mut self, filter: &str) {
        self.selected = self
            .visible(filter)
            .first()
            .map(|work| work.identity.clone());
    }

    pub(crate) fn select_last(&mut self, filter: &str) {
        self.selected = self
            .visible(filter)
            .last()
            .map(|work| work.identity.clone());
    }
}

/// Works that need the operator first, then the ones in flight, newest first
/// within each; identity breaks ties so the order is stable across refreshes.
fn attention_order(
    work: &WorkSnapshotItem,
) -> (bool, u8, Reverse<Option<OffsetDateTime>>, WorkIdentity) {
    let stage = match work.stage {
        BoardStage::InProgress => 0,
        BoardStage::Review => 1,
        BoardStage::Queued => 2,
        BoardStage::Done => 3,
    };
    (
        !needs_operator(work),
        stage,
        Reverse(work.latest_at),
        work.identity.clone(),
    )
}

/// A signal on the Work, or an agent in it waiting on a person or failing.
pub(crate) fn needs_operator(work: &WorkSnapshotItem) -> bool {
    !work.signals.is_empty() || agents(work).any(|agent| waits_on_operator(agent.state))
}

fn waits_on_operator(state: AgentState) -> bool {
    matches!(
        state,
        AgentState::WaitingInput | AgentState::WaitingChoice | AgentState::Error
    )
}

fn agents(work: &WorkSnapshotItem) -> impl Iterator<Item = &Agent> {
    work.runs
        .iter()
        .flat_map(|run| run.panes.iter())
        .filter_map(|pane| pane.agent.as_ref())
}

fn matches_filter(work: &WorkSnapshotItem, needle: &str) -> bool {
    let haystack = [
        Some(work.title.as_str()),
        Some(work.identity.workspace_id.as_str()),
        Some(work.identity.work_id.as_str()),
        work.goal.as_deref(),
        work.next_action.as_deref(),
    ];
    haystack
        .into_iter()
        .flatten()
        .chain(
            work.external_items
                .iter()
                .map(|item| item.display_key.as_str()),
        )
        .any(|text| text.to_lowercase().contains(needle))
}

/// Where `Enter` goes for a Work: the agent pane that is waiting on the
/// operator, else any agent pane, else the first pane of its first Run.
/// `None` for a Work with no live Run.
pub(crate) fn jump_target(work: &WorkSnapshotItem) -> Option<(&RunSnapshot, &str)> {
    let panes = || {
        work.runs
            .iter()
            .flat_map(|run| run.panes.iter().map(move |pane| (run, pane)))
    };
    panes()
        .find(|(_, pane)| {
            pane.agent
                .as_ref()
                .is_some_and(|a| waits_on_operator(a.state))
        })
        .or_else(|| panes().find(|(_, pane)| pane.agent.is_some()))
        .or_else(|| panes().next())
        .map(|(run, pane)| (run, pane.pane_id.as_str()))
}

pub(crate) const COLUMNS: [Constraint; 7] = [
    Constraint::Length(11),
    Constraint::Percentage(26),
    Constraint::Min(18),
    Constraint::Length(12),
    Constraint::Length(4),
    Constraint::Length(9),
    Constraint::Length(5),
];

pub(crate) const HEADERS: [&str; 7] = ["STAGE", "WORK", "TITLE", "ISSUE", "RUNS", "AGENTS", "AGE"];

pub(crate) fn stage_label(stage: BoardStage) -> &'static str {
    match stage {
        BoardStage::Queued => "queued",
        BoardStage::InProgress => "in progress",
        BoardStage::Review => "review",
        BoardStage::Done => "done",
    }
}

fn signal_label(signal: WorkSignal) -> &'static str {
    match signal {
        WorkSignal::Attention => "attention",
        WorkSignal::Blocked => "blocked",
        WorkSignal::Error => "error",
    }
}

fn agent_glyphs<'a>(work: &WorkSnapshotItem, theme: WatchThemeSpec) -> Line<'a> {
    let states: Vec<AgentState> = agents(work).map(|agent| agent.state).collect();
    let mut spans: Vec<Span> = states
        .iter()
        .take(AGENT_GLYPHS)
        .map(|state| Span::styled(crate::state_icon(*state), theme.state_style(*state)))
        .collect();
    if states.len() > AGENT_GLYPHS {
        spans.push(Span::raw(format!("+{}", states.len() - AGENT_GLYPHS)));
    }
    if spans.is_empty() {
        spans.push(Span::styled("-", theme.dim_style()));
    }
    Line::from(spans)
}

pub(crate) fn row<'a>(
    work: &WorkSnapshotItem,
    now: OffsetDateTime,
    theme: WatchThemeSpec,
    selected: bool,
) -> Row<'a> {
    let stage = if needs_operator(work) {
        Span::styled(
            format!("! {}", stage_label(work.stage)),
            theme.state_style(AgentState::WaitingInput),
        )
    } else {
        Span::raw(format!("  {}", stage_label(work.stage)))
    };
    let issue = work
        .external_items
        .first()
        .map_or_else(String::new, |item| item.display_key.clone());
    let cells = vec![
        Cell::from(Line::from(stage)),
        Cell::from(format!(
            "{}/{}",
            work.identity.workspace_id, work.identity.work_id
        )),
        // A Work with no title of its own is titled by its id, which the
        // WORK column already shows.
        Cell::from(if work.title == work.identity.work_id {
            String::new()
        } else {
            work.title.clone()
        }),
        Cell::from(issue),
        Cell::from(work.runs.len().to_string()),
        Cell::from(agent_glyphs(work, theme)),
        Cell::from(work.latest_at.map_or_else(|| "-".into(), |at| age(now, at))),
    ];
    let row = Row::new(cells);
    if selected {
        row.style(theme.selected_style())
    } else {
        row
    }
}

/// The selected Work in full: what it is for, its issue, and every Run with
/// the agents in it.
pub(crate) fn detail_lines<'a>(
    work: &WorkSnapshotItem,
    now: OffsetDateTime,
    theme: WatchThemeSpec,
) -> Vec<Line<'a>> {
    let mut lines = Vec::new();
    let mut head = vec![
        Span::styled(
            work.title.clone(),
            theme.table_header_style().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("  {}", stage_label(work.stage))),
    ];
    for signal in &work.signals {
        head.push(Span::styled(
            format!("  {}", signal_label(*signal)),
            theme.state_style(AgentState::Error),
        ));
    }
    lines.push(Line::from(head));
    if let Some(goal) = &work.goal {
        lines.push(Line::from(format!("goal  {goal}")));
    }
    if let Some(next) = &work.next_action {
        lines.push(Line::from(format!("next  {next}")));
    }
    for item in &work.external_items {
        let mut text = format!("issue {}", item.display_key);
        if let Some(status) = &item.status {
            let _ = write!(text, " · {status}");
        }
        if let Some(title) = &item.title {
            let _ = write!(text, " · {title}");
        }
        lines.push(Line::from(text));
    }
    if work.runs.is_empty() {
        lines.push(Line::from(Span::styled(
            "no live Run — `muxa work up` starts one",
            theme.dim_style(),
        )));
    }
    for run in &work.runs {
        lines.push(Line::from(Span::styled(
            format!(
                "run {}:{}  {:?}{}",
                run.session_name,
                run.window_name,
                run.state,
                run.cwd
                    .as_deref()
                    .map_or_else(String::new, |cwd| format!("  {cwd}"))
            )
            .to_lowercase(),
            theme.dim_style(),
        )));
        for pane in &run.panes {
            let Some(agent) = &pane.agent else {
                continue;
            };
            let mut spans = vec![
                Span::raw(format!("  {} ", pane.pane_id)),
                Span::styled(
                    crate::state_icon(agent.state),
                    theme.state_style(agent.state),
                ),
                Span::raw(format!(" {}", agent.kind)),
            ];
            if let Some(role) = &pane.role {
                spans.push(Span::raw(format!(" · {role}")));
            }
            if let Some(prompt) = &agent.last_prompt {
                let when = agent
                    .last_prompt_at
                    .map_or_else(String::new, |at| format!(" {} ago", age(now, at)));
                spans.push(Span::styled(
                    format!("  ›{when} {}", excerpt(prompt, 70)),
                    theme.dim_style(),
                ));
            }
            lines.push(Line::from(spans));
        }
    }
    lines
}

fn excerpt(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let cut: String = flat.chars().take(max - 1).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use muxa::work::{ExecutionIdentity, RunPaneSnapshot, RunState, WorkMetadata, WorkSource};

    fn work(
        id: &str,
        stage: BoardStage,
        signals: Vec<WorkSignal>,
        minutes_ago: i64,
    ) -> WorkSnapshotItem {
        let at = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()
            - time::Duration::minutes(minutes_ago);
        WorkSnapshotItem {
            identity: WorkIdentity {
                workspace_id: "callabo".into(),
                work_id: id.into(),
            },
            title: format!("title {id}"),
            goal: None,
            next_action: None,
            stage,
            signals,
            external_items: Vec::new(),
            runs: Vec::new(),
            participants: 0,
            latest_at: Some(at),
            source: WorkSource::Persisted,
            metadata: WorkMetadata {
                title: None,
                goal: None,
                next_action: None,
                stage: muxa::work::WorkStage::default(),
                updated_at: at,
            },
        }
    }

    fn run(socket: &str, panes: Vec<(&str, Option<AgentState>)>) -> RunSnapshot {
        RunSnapshot {
            id: format!("{socket}:@1"),
            state: RunState::Running,
            linked: true,
            work: None,
            execution: ExecutionIdentity {
                host: muxa::backend::HostKind::Tmux,
                socket: socket.into(),
                session_id: "$1".into(),
                window_id: "@1".into(),
            },
            session_name: "callabo".into(),
            window_name: "CAL-1".into(),
            window_index: "1".into(),
            cwd: None,
            panes: panes
                .into_iter()
                .map(|(pane_id, state)| RunPaneSnapshot {
                    pane_id: pane_id.into(),
                    pane_index: "0".into(),
                    current_command: String::new(),
                    title: String::new(),
                    current_path: String::new(),
                    attach_command: String::new(),
                    role: None,
                    task: None,
                    agent: state.map(agent),
                })
                .collect(),
            latest_at: None,
        }
    }

    fn agent(state: AgentState) -> Agent {
        let now = OffsetDateTime::UNIX_EPOCH;
        Agent {
            tmux_socket: None,
            tmux_session: None,
            kind: muxa::event::AgentKind::Codex,
            session_id: "s".into(),
            surface: None,
            pane: None,
            pid: None,
            workload: muxa::WorkloadSummary::default(),
            subagents: Vec::new(),
            cwd: None,
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

    fn snapshot(works: Vec<WorkSnapshotItem>) -> WorkSnapshot {
        WorkSnapshot {
            schema_version: 1,
            generated_at: OffsetDateTime::UNIX_EPOCH,
            workspaces: Vec::new(),
            works,
            unlinked_executions: Vec::new(),
        }
    }

    #[test]
    fn works_that_need_the_operator_come_first_then_stage_then_recency() {
        let mut waiting = work("CAL-4", BoardStage::Done, Vec::new(), 90);
        waiting.runs = vec![run("/s", vec![("%4", Some(AgentState::WaitingInput))])];
        let mut board = BoardScreen::default();
        board.set_snapshot(
            snapshot(vec![
                work("CAL-1", BoardStage::Queued, Vec::new(), 1),
                work("CAL-2", BoardStage::InProgress, Vec::new(), 30),
                work("CAL-3", BoardStage::InProgress, Vec::new(), 5),
                waiting,
                work("CAL-5", BoardStage::Review, vec![WorkSignal::Blocked], 60),
            ]),
            Vec::new(),
        );
        let order: Vec<_> = board
            .visible("")
            .iter()
            .map(|work| work.identity.work_id.as_str())
            .collect();
        assert_eq!(order, ["CAL-5", "CAL-4", "CAL-3", "CAL-2", "CAL-1"]);
    }

    #[test]
    fn the_cursor_follows_the_work_across_a_reordering_refresh() {
        let mut board = BoardScreen::default();
        board.set_snapshot(
            snapshot(vec![
                work("CAL-1", BoardStage::InProgress, Vec::new(), 1),
                work("CAL-2", BoardStage::InProgress, Vec::new(), 2),
            ]),
            Vec::new(),
        );
        board.move_selection(1, "");
        assert_eq!(board.selected_work("").unwrap().identity.work_id, "CAL-2");
        // CAL-2 now needs the operator and jumps to the top.
        board.set_snapshot(
            snapshot(vec![
                work("CAL-1", BoardStage::InProgress, Vec::new(), 1),
                work("CAL-2", BoardStage::InProgress, vec![WorkSignal::Error], 2),
            ]),
            Vec::new(),
        );
        assert_eq!(board.selected_index(""), 0);
        assert_eq!(board.selected_work("").unwrap().identity.work_id, "CAL-2");
    }

    #[test]
    fn filtering_matches_ids_titles_and_issue_keys_and_keeps_the_cursor_inside() {
        let mut tagged = work("CAL-9", BoardStage::Queued, Vec::new(), 1);
        tagged.external_items = vec![muxa::work::ExternalItemRef {
            source: "linear".into(),
            scope: None,
            stable_id: None,
            display_key: "LIN-42".into(),
            title: None,
            url: None,
            status: None,
            item_type: None,
            synced_at: OffsetDateTime::UNIX_EPOCH,
        }];
        let mut board = BoardScreen::default();
        board.set_snapshot(
            snapshot(vec![
                work("CAL-1", BoardStage::Queued, Vec::new(), 2),
                tagged,
            ]),
            Vec::new(),
        );
        assert_eq!(board.visible("lin-42").len(), 1);
        assert_eq!(board.visible("title cal-1").len(), 1);
        assert_eq!(board.visible("callabo").len(), 2);
        board.select_last("");
        assert_eq!(board.selected_work("").unwrap().identity.work_id, "CAL-1");
        // The selected Work is filtered out: the cursor lands on what is left.
        assert_eq!(
            board.selected_work("lin").unwrap().identity.work_id,
            "CAL-9"
        );
        board.move_selection(5, "nothing matches");
        assert!(board.selected_work("nothing matches").is_none());
    }

    #[test]
    fn enter_prefers_the_agent_waiting_on_the_operator_and_keeps_its_socket() {
        let mut item = work("CAL-1", BoardStage::InProgress, Vec::new(), 1);
        item.runs = vec![
            run("/a", vec![("%1", None), ("%2", Some(AgentState::Working))]),
            run("/b", vec![("%2", Some(AgentState::WaitingChoice))]),
        ];
        let (target, pane) = jump_target(&item).unwrap();
        assert_eq!((target.execution.socket.as_str(), pane), ("/b", "%2"));

        item.runs[1].panes[0].agent.as_mut().unwrap().state = AgentState::Idle;
        let (target, pane) = jump_target(&item).unwrap();
        assert_eq!((target.execution.socket.as_str(), pane), ("/a", "%2"));

        item.runs = vec![run("/a", vec![("%7", None)])];
        assert_eq!(jump_target(&item).unwrap().1, "%7");
        item.runs.clear();
        assert!(jump_target(&item).is_none());
    }

    #[test]
    fn unlinked_executions_are_counted_not_listed() {
        let mut snap = snapshot(vec![work("CAL-1", BoardStage::Queued, Vec::new(), 1)]);
        snap.unlinked_executions = vec![run("/s", Vec::new()), run("/t", Vec::new())];
        let mut board = BoardScreen::default();
        board.set_snapshot(snap, Vec::new());
        assert_eq!((board.visible("").len(), board.unlinked()), (1, 2));
    }
}
