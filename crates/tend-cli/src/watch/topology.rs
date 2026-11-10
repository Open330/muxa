use super::{
    agent_pane_display, sort_panes, sort_works, Agent, AgentKind, AgentState, App, HashMap,
    HashSet, OffsetDateTime, Ordering, PaneInfo, PaneKey, PaneNode, SessionActivity, SessionInfo,
    SessionNode, StateDistribution, TopologyInput, TopologyNodeKey, TopologySnapshot, TreeTarget,
    VisibleTarget, WatchRow, WatchSortKey, WatchView, WindowKey, WindowNode, WorkRow,
};

pub(super) fn target_pane(app: &App, target: VisibleTarget) -> Option<String> {
    match app.rows.get(target.row_idx)? {
        WatchRow::Work(session) => target
            .agent_idx
            .and_then(|idx| session.agents.get(idx))
            .and_then(|agent| agent.pane.clone())
            .or_else(|| session.representative_pane.clone()),
        row => row.pane_id().map(String::from),
    }
}

pub(super) fn topology_keys(snapshot: &TopologySnapshot) -> HashSet<TopologyNodeKey> {
    let mut keys = HashSet::new();
    for session in &snapshot.sessions {
        keys.insert(session.node_key());
        for window in &session.windows {
            keys.insert(window.node_key());
            keys.extend(window.panes.iter().map(PaneNode::node_key));
        }
    }
    keys
}

pub(super) fn topology_key_label(key: &TopologyNodeKey) -> String {
    match key {
        TopologyNodeKey::Session(session) => format!(
            "{}:{} session {}",
            session.endpoint.host, session.endpoint.socket, session.session_id
        ),
        TopologyNodeKey::Window(window) => format!(
            "{}:{} session {} window {}",
            window.session.endpoint.host,
            window.session.endpoint.socket,
            window.session.session_id,
            window.window_id
        ),
        TopologyNodeKey::Pane(pane) => format!(
            "{}:{} session {} window {} pane {}",
            pane.window.session.endpoint.host,
            pane.window.session.endpoint.socket,
            pane.window.session.session_id,
            pane.window.window_id,
            pane.pane_id
        ),
    }
}

pub(super) fn default_tree_expanded(key: &TopologyNodeKey, view: WatchView) -> bool {
    matches!(
        (key, view),
        (
            TopologyNodeKey::Session(_),
            WatchView::Window | WatchView::Pane
        ) | (TopologyNodeKey::Window(_), WatchView::Pane)
    )
}

fn topology_parent_key(key: &TopologyNodeKey) -> Option<TopologyNodeKey> {
    match key {
        TopologyNodeKey::Session(_) => None,
        TopologyNodeKey::Window(key) => Some(TopologyNodeKey::Session(key.session.clone())),
        TopologyNodeKey::Pane(key) => Some(TopologyNodeKey::Window(key.window.clone())),
    }
}

/// Parent in the rendered tree rather than in the canonical topology.
///
/// A pane at depth one belongs to a compacted single-window session, so its
/// visible parent is the session row even though its durable topology key
/// still records the hidden window.
pub(super) fn visible_tree_parent_key(target: &TreeTarget) -> Option<TopologyNodeKey> {
    match (&target.key, target.depth) {
        (TopologyNodeKey::Pane(pane), 1) => {
            Some(TopologyNodeKey::Session(pane.window.session.clone()))
        }
        _ => topology_parent_key(&target.key),
    }
}

pub(super) fn selected_tree_target_from<'a>(
    app: &App,
    targets: &'a [TreeTarget],
) -> Option<&'a TreeTarget> {
    targets.get(app.table_state.selected()?)
}

pub(super) fn distribution_rank(states: &StateDistribution) -> u8 {
    if states.error > 0 {
        state_sort_rank(AgentState::Error)
    } else if states.waiting_choice > 0 {
        state_sort_rank(AgentState::WaitingChoice)
    } else if states.waiting_input > 0 {
        state_sort_rank(AgentState::WaitingInput)
    } else if states.working > 0 {
        state_sort_rank(AgentState::Working)
    } else if states.starting > 0 {
        state_sort_rank(AgentState::Starting)
    } else if states.idle > 0 {
        state_sort_rank(AgentState::Idle)
    } else if states.stopped > 0 {
        state_sort_rank(AgentState::Stopped)
    } else {
        u8::MAX
    }
}

fn node_query_match(values: impl IntoIterator<Item = String>, query: &str) -> bool {
    query.is_empty() || values.into_iter().any(|value| contains_ci(&value, query))
}

fn session_matches_query(session: &SessionNode, query: &str) -> bool {
    node_query_match(
        [
            session.name.clone(),
            session.key.session_id.clone(),
            session.key.endpoint.host.to_string(),
            session.key.endpoint.socket.clone(),
        ],
        query,
    )
}

fn window_matches_query(window: &WindowNode, session: &SessionNode, query: &str) -> bool {
    node_query_match(
        [
            session.name.clone(),
            window.name.clone(),
            window.index.clone(),
            window.key.window_id.clone(),
            window.cwd.clone().unwrap_or_default(),
        ],
        query,
    )
}

fn pane_matches_query(pane: &PaneNode, query: &str) -> bool {
    if node_query_match(
        [
            pane.key.pane_id.clone(),
            pane.index.clone(),
            pane.current_command.clone(),
            pane.title.clone(),
            pane.cwd.clone(),
        ],
        query,
    ) {
        return true;
    }
    pane.agent
        .as_ref()
        .is_some_and(|agent| agent_matches_query(agent, &[], query))
}

pub(super) fn tree_pane_relevant(
    pane: &PaneNode,
    session: &SessionNode,
    window: &WindowNode,
    query: &str,
    attention_only: bool,
) -> bool {
    let query_match = query.is_empty()
        || session_matches_query(session, query)
        || window_matches_query(window, session, query)
        || pane_matches_query(pane, query);
    let attention_match = !attention_only
        || pane
            .agent
            .as_ref()
            .is_some_and(|agent| agent_needs_attention(agent.state));
    query_match && attention_match
}

pub(super) fn tree_window_relevant(
    window: &WindowNode,
    session: &SessionNode,
    query: &str,
    attention_only: bool,
) -> bool {
    let query_match = query.is_empty()
        || window_matches_query(window, session, query)
        || window
            .panes
            .iter()
            .any(|pane| pane_matches_query(pane, query));
    let attention_match = !attention_only || window.states.needs_attention();
    query_match && attention_match
}

pub(super) fn tree_session_relevant(
    session: &SessionNode,
    query: &str,
    attention_only: bool,
) -> bool {
    let query_match = query.is_empty()
        || session_matches_query(session, query)
        || session.windows.iter().any(|window| {
            window_matches_query(window, session, query)
                || window
                    .panes
                    .iter()
                    .any(|pane| pane_matches_query(pane, query))
        });
    let attention_match = !attention_only || session.states.needs_attention();
    query_match && attention_match
}

pub(super) struct TopologySortFacts {
    label: String,
    state_rank: u8,
    activity: Option<OffsetDateTime>,
    duration_secs: u64,
    native_index: u64,
    pane_id: String,
    stable: TopologyNodeKey,
}

impl TopologySortFacts {
    pub(super) fn for_session(session: &SessionNode, duration_secs: u64) -> Self {
        Self {
            label: session.name.clone(),
            state_rank: distribution_rank(&session.states),
            activity: session
                .windows
                .iter()
                .flat_map(|window| &window.panes)
                .filter_map(|pane| pane.agent.as_ref().map(|agent| agent.last_activity_at))
                .max(),
            duration_secs,
            native_index: 0,
            pane_id: String::new(),
            stable: session.node_key(),
        }
    }

    pub(super) fn for_window(window: &WindowNode) -> Self {
        let now = OffsetDateTime::now_utc();
        let earliest = window
            .panes
            .iter()
            .filter_map(|pane| pane.agent.as_ref().map(|agent| agent.started_at))
            .min();
        Self {
            label: window.name.clone(),
            state_rank: distribution_rank(&window.states),
            activity: window
                .panes
                .iter()
                .filter_map(|pane| pane.agent.as_ref().map(|agent| agent.last_activity_at))
                .max(),
            duration_secs: earliest.map_or(0, |at| {
                u64::try_from((now - at).whole_seconds().max(0)).unwrap_or(u64::MAX)
            }),
            native_index: window.index.parse().unwrap_or(u64::MAX),
            pane_id: String::new(),
            stable: window.node_key(),
        }
    }

    pub(super) fn for_pane(pane: &PaneNode) -> Self {
        let now = OffsetDateTime::now_utc();
        Self {
            label: pane
                .agent
                .as_ref()
                .map_or_else(|| pane.title.clone(), |agent| agent.kind.to_string()),
            state_rank: pane
                .agent
                .as_ref()
                .map_or(u8::MAX, |agent| state_sort_rank(agent.state)),
            activity: pane.agent.as_ref().map(|agent| agent.last_activity_at),
            duration_secs: pane.agent.as_ref().map_or(0, |agent| {
                u64::try_from((now - agent.state_entered_at).whole_seconds().max(0))
                    .unwrap_or(u64::MAX)
            }),
            native_index: pane.index.parse().unwrap_or(u64::MAX),
            pane_id: pane.key.pane_id.clone(),
            stable: pane.node_key(),
        }
    }
}

pub(super) fn compare_topology_siblings(
    keys: &[WatchSortKey],
    left: &TopologySortFacts,
    right: &TopologySortFacts,
) -> Ordering {
    for key in keys {
        let ordering = match key {
            WatchSortKey::Name => left.label.cmp(&right.label),
            WatchSortKey::Activity => right.activity.cmp(&left.activity),
            WatchSortKey::State => left.state_rank.cmp(&right.state_rank),
            WatchSortKey::Duration => right.duration_secs.cmp(&left.duration_secs),
            WatchSortKey::Pane => left.native_index.cmp(&right.native_index),
            WatchSortKey::PaneId => left.pane_id.cmp(&right.pane_id),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    left.stable.cmp(&right.stable)
}

pub(super) fn contains_ci(value: &str, lowercase_query: &str) -> bool {
    value.to_lowercase().contains(lowercase_query)
}

pub(super) fn agent_matches_query(agent: &Agent, panes: &[PaneInfo], query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let pane_label = agent_pane_display(agent, panes);
    let kind = agent.kind.to_string();
    let state = agent.state.to_string();
    let matches = [
        Some(agent.session_id.as_str()),
        Some(kind.as_str()),
        Some(state.as_str()),
        Some(pane_label.as_str()),
        agent.tmux_session.as_deref(),
        agent.cwd.as_deref(),
        agent.model.as_deref(),
        agent.last_prompt.as_deref(),
        agent.last_response.as_deref(),
        agent.last_notification.as_deref(),
        agent.recap.as_deref(),
        agent.ai_title.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|value| contains_ci(value, query));
    matches
}

pub(super) fn row_matches_query(row: &WatchRow, panes: &[PaneInfo], query: &str) -> bool {
    match row {
        WatchRow::Agent(agent) => agent_matches_query(agent, panes, query),
        WatchRow::BarePane(pane) => [
            pane.pane_id.as_str(),
            pane.session.as_str(),
            pane.current_command.as_str(),
            pane.title.as_str(),
            pane.current_path.as_str(),
        ]
        .into_iter()
        .any(|value| contains_ci(value, query)),
        WatchRow::Work(session) => {
            contains_ci(&session.display_name, query)
                || contains_ci(&session.session, query)
                || session
                    .bare_summary
                    .as_deref()
                    .is_some_and(|value| contains_ci(value, query))
                || session
                    .agents
                    .iter()
                    .any(|agent| agent_matches_query(agent, panes, query))
        }
    }
}

pub(super) fn row_needs_attention(row: &WatchRow) -> bool {
    match row {
        WatchRow::Agent(agent) => agent_needs_attention(agent.state),
        WatchRow::Work(session) => session
            .agents
            .iter()
            .any(|agent| agent_needs_attention(agent.state)),
        WatchRow::BarePane(_) => false,
    }
}

pub(super) struct SortContext<'a> {
    pane_by_id: HashMap<&'a str, Vec<&'a PaneInfo>>,
    session_id_by_name: HashMap<&'a str, &'a str>,
    /// Display name for a group key. A group is keyed by `PaneInfo.session`:
    /// the session name on tmux, the `workspace_id` on herdr. Mapped from
    /// both `SessionInfo.name` and `SessionInfo.session_id` so a key resolves
    /// however the pane spells it — on tmux both entries point at the same
    /// name (identity display); on herdr the `workspace_id` entry surfaces
    /// the human label.
    display_by_key: HashMap<&'a str, &'a str>,
    activity_by_id: HashMap<&'a str, &'a SessionActivity>,
    now: OffsetDateTime,
}

impl<'a> SortContext<'a> {
    pub(super) fn new(
        panes: &'a [PaneInfo],
        sessions: &'a [SessionInfo],
        session_activity: &'a [SessionActivity],
        now: OffsetDateTime,
    ) -> Self {
        let mut display_by_key = HashMap::new();
        for s in sessions {
            display_by_key.insert(s.name.as_str(), s.name.as_str());
            display_by_key.insert(s.session_id.as_str(), s.name.as_str());
        }
        // One entry per pane per server: a grouped session lists each pane
        // once per `~view~` member, which must not read as ambiguous.
        let mut pane_by_id: HashMap<&str, Vec<&PaneInfo>> = HashMap::new();
        for pane in panes {
            let entries = pane_by_id.entry(pane.pane_id.as_str()).or_default();
            match entries.iter_mut().find(|kept| kept.socket == pane.socket) {
                Some(kept) => {
                    if tend::tmux::prefer_listing(pane, *kept) {
                        *kept = pane;
                    }
                }
                None => entries.push(pane),
            }
        }
        Self {
            pane_by_id,
            session_id_by_name: sessions
                .iter()
                .map(|s| (s.name.as_str(), s.session_id.as_str()))
                .collect(),
            display_by_key,
            activity_by_id: session_activity
                .iter()
                .map(|a| (a.session_id.as_str(), a))
                .collect(),
            now,
        }
    }

    pub(super) fn pane(&self, pane_id: &str) -> Option<&'a PaneInfo> {
        let candidates = self.pane_by_id.get(pane_id)?;
        (candidates.len() == 1).then_some(candidates[0])
    }

    pub(super) fn pane_for_agent(&self, agent: &Agent) -> Option<&'a PaneInfo> {
        let pane_id = agent.pane.as_deref()?;
        let candidates = self.pane_by_id.get(pane_id)?;
        match agent.tmux_socket.as_deref() {
            Some(endpoint) => candidates.iter().copied().find(|pane| {
                let host =
                    tend::backend::pane_id_host_kind(&pane.pane_id).unwrap_or(tend::HostKind::Tmux);
                let pane_endpoint = PaneKey::from_pane(host, pane)
                    .window
                    .session
                    .endpoint
                    .socket;
                tend::backend::pane_endpoint_identity(Some(pane_id), endpoint) == pane_endpoint
            }),
            None if candidates.len() == 1 => Some(candidates[0]),
            None => None,
        }
    }

    pub(super) fn pane_for_window(&self, window_key: &WindowKey) -> Option<&'a PaneInfo> {
        self.pane_by_id.values().flatten().copied().find(|pane| {
            let host =
                tend::backend::pane_id_host_kind(&pane.pane_id).unwrap_or(tend::HostKind::Tmux);
            WindowKey::from_pane(host, pane) == *window_key
        })
    }

    /// The human-facing display name for a group key, falling back to the key
    /// itself when no session metadata matches (e.g. a stale pane).
    fn display_name(&self, session: &str) -> String {
        self.display_by_key
            .get(session)
            .copied()
            .unwrap_or(session)
            .to_string()
    }

    fn activity_for_session_name(&self, session: &str) -> Option<&'a SessionActivity> {
        // tmux keys panes by the mutable session *name* but the ledger by the
        // stable session *id*, so bridge name → id → activity. On herdr the
        // pane's session is already the stable `workspace_id` (= the ledger
        // key), so the name lookup misses and we fall back to keying the
        // ledger directly. The fallback is a no-op on tmux (session names
        // never collide with `$N` session ids) and only fires after a miss.
        self.session_id_by_name
            .get(session)
            .and_then(|id| self.activity_by_id.get(*id).copied())
            .or_else(|| self.activity_by_id.get(session).copied())
    }

    pub(super) fn session_duration_secs(&self, session: &str) -> u64 {
        self.activity_for_session_name(session)
            .map_or(0, |a| a.effective_total_secs(self.now))
    }

    fn agent_session_duration_secs(&self, agent: &Agent) -> u64 {
        self.pane_for_agent(agent).map_or(0, |pane| {
            if tend::backend::pane_id_host_kind(&pane.pane_id) == Some(tend::HostKind::Rmux) {
                0
            } else {
                self.session_duration_secs(&pane.session)
            }
        })
    }
}

/// Whether an agent state is blocked on a human — the same predicate the
/// notifier and `attend` use. Kept in sync with `state_sort_rank`'s top
/// three ranks.
pub(super) fn agent_needs_attention(state: AgentState) -> bool {
    matches!(
        state,
        AgentState::WaitingInput | AgentState::WaitingChoice | AgentState::Error
    )
}

pub(super) fn state_sort_rank(state: AgentState) -> u8 {
    match state {
        AgentState::Error => 0,
        AgentState::WaitingChoice => 1,
        AgentState::WaitingInput => 2,
        AgentState::Working => 3,
        AgentState::Starting => 4,
        AgentState::Idle => 5,
        AgentState::Stopped => 6,
    }
}

/// Compare two agents according to the user-configured sort keys.
///
/// Comparison flow (each step exits as soon as one agent is "less than"
/// the other — the rest are tiebreakers):
/// 1. Live agents always sort before stale agents (pane closed).
/// 2. Each `WatchSortKey` from the config, in order.
/// 3. `pane_id` lex ascending — final stable tiebreaker so the order is
///    deterministic across refreshes when every other key ties (matters
///    most for `Activity` when timestamps quantize to the same second).
pub(super) fn sort_agents(
    a: &Agent,
    b: &Agent,
    keys: &[WatchSortKey],
    sort_context: &SortContext<'_>,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    let info_a = sort_context.pane_for_agent(a);
    let info_b = sort_context.pane_for_agent(b);

    // Stale (pane gone) → Ordering::Greater so it sinks to the bottom.
    match (info_a.is_some(), info_b.is_some()) {
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        _ => {}
    }

    for key in keys {
        let cmp = match key {
            WatchSortKey::Name => info_a
                .map(|p| p.session.as_str())
                .cmp(&info_b.map(|p| p.session.as_str())),
            WatchSortKey::Activity => {
                // Reverse so newer (= larger timestamp) ends up first.
                b.last_activity_at.cmp(&a.last_activity_at)
            }
            WatchSortKey::State => state_sort_rank(a.state).cmp(&state_sort_rank(b.state)),
            WatchSortKey::Duration => sort_context
                .agent_session_duration_secs(b)
                .cmp(&sort_context.agent_session_duration_secs(a)),
            WatchSortKey::Pane => {
                let key_for = |info: Option<&PaneInfo>| {
                    info.map(|p| {
                        (
                            p.window_index.parse::<u32>().unwrap_or(u32::MAX),
                            p.pane_index.parse::<u32>().unwrap_or(u32::MAX),
                        )
                    })
                };
                key_for(info_a).cmp(&key_for(info_b))
            }
            WatchSortKey::PaneId => a.pane.as_deref().cmp(&b.pane.as_deref()),
        };
        if cmp != Ordering::Equal {
            return cmp;
        }
    }

    a.pane.as_deref().cmp(&b.pane.as_deref())
}

/// Host-namespaced fallback grouping key for a workspace-level row.
///
/// A tmux session and a herdr workspace can share a raw session id (both
/// named "w1"); grouping by the raw id alone merges them into one corrupted
/// row (wrong pane counts, mixed agents). Prefixing with the host keeps them
/// distinct. Ledger/display lookups still use the *raw* id (`session`), which
/// is unambiguous because each host's id-space is disjoint — this composite is
/// grouping/identity only. A row with no classifiable host (paneless agent,
/// `(no session)`, a background task) keys on the raw id, matching the
/// pre-multi-host grouping exactly.
pub(super) fn session_group_key(host: Option<tend::HostKind>, session: &str) -> String {
    match host {
        Some(tend::HostKind::Tmux) => format!("tmux:{session}"),
        Some(tend::HostKind::Cmux) => format!("cmux:{session}"),
        Some(tend::HostKind::Rmux) => format!("rmux:{session}"),
        Some(tend::HostKind::Herdr) => format!("herdr:{session}"),
        Some(tend::HostKind::Zellij) => format!("zellij:{session}"),
        None => session.to_string(),
    }
}

/// Terminals attached to the workspace `info` names, counting the whole
/// session group.
///
/// `attached_clients` is per session, and topology folds a session group into
/// one node — so the members' counts belong on that node too. Without the sum
/// a workspace open in two terminals reports one, which is the count for a row
/// the user is deliberately no longer being shown.
pub(super) fn group_attached_clients(info: &SessionInfo, all: &[SessionInfo]) -> u32 {
    let Some(group) = info.group.as_deref() else {
        return info.attached_clients;
    };
    all.iter()
        .filter(|member| member.group.as_deref() == Some(group))
        .map(|member| member.attached_clients)
        .sum()
}

pub(super) fn build_watch_topology(
    agents: &[Agent],
    panes: &[PaneInfo],
    session_info: &[SessionInfo],
    pipeline_runs: &[tend::pipeline_run::PipelineRun],
) -> TopologySnapshot {
    let mut panes_by_host: HashMap<tend::HostKind, Vec<PaneInfo>> = HashMap::new();
    for pane in panes {
        // Legacy un-namespaced ids predate multi-host support and historically
        // belonged to tmux. Keeping that fallback preserves visibility while
        // still assigning a complete host+socket topology key.
        let host = tend::backend::pane_id_host_kind(&pane.pane_id).unwrap_or(tend::HostKind::Tmux);
        panes_by_host.entry(host).or_default().push(pane.clone());
    }
    let inputs = panes_by_host
        .into_iter()
        .map(|(host, panes)| TopologyInput::new(host, panes))
        .collect();
    let mut snapshot = TopologySnapshot::build(OffsetDateTime::now_utc(), inputs, agents.to_vec());
    let unique_topology_sessions: HashSet<(tend::HostKind, String)> = snapshot
        .sessions
        .iter()
        .filter(|candidate| {
            snapshot
                .sessions
                .iter()
                .filter(|other| {
                    other.key.endpoint.host == candidate.key.endpoint.host
                        && other.key.session_id == candidate.key.session_id
                })
                .count()
                == 1
        })
        .map(|session| (session.key.endpoint.host, session.key.session_id.clone()))
        .collect();
    for node in &mut snapshot.sessions {
        // SessionInfo currently has no endpoint field. Apply it only when its
        // native id/name identifies one topology session uniquely; duplicate
        // `$N` ids across sockets deliberately remain unset rather than
        // borrowing another server's attached-client count.
        if node.key.endpoint.host != tend::HostKind::Tmux
            || !unique_topology_sessions
                .contains(&(node.key.endpoint.host, node.key.session_id.clone()))
        {
            continue;
        }
        let mut matches = session_info
            .iter()
            .filter(|info| info.session_id == node.key.session_id || info.name == node.name);
        if let Some(info) = matches.next() {
            if matches.next().is_none() {
                node.name.clone_from(&info.name);
                // `attached_clients` is per session, and topology folded this
                // node's session group into one row — so the count for the
                // group's other members belongs here too. Without the sum a
                // workspace open in two terminals reports one, which is the
                // number for the row the user is no longer being shown.
                node.attached_clients = Some(group_attached_clients(info, session_info));
            }
        }
    }
    let window_id_counts = snapshot
        .sessions
        .iter()
        .flat_map(|session| &session.windows)
        .fold(HashMap::<String, usize>::new(), |mut counts, window| {
            *counts.entry(window.key.window_id.clone()).or_default() += 1;
            counts
        });
    for session in &mut snapshot.sessions {
        for window in &mut session.windows {
            // Durable Runs are launched on the locally scoped tmux server.
            // Raw `@N` ids repeat across backends and sockets, so an
            // ambiguous id must remain unjoined instead of displaying one
            // Work's state on another window.
            if window.key.session.endpoint.host != tend::HostKind::Tmux
                || window_id_counts
                    .get(window.key.window_id.as_str())
                    .copied()
                    .unwrap_or_default()
                    != 1
            {
                continue;
            }
            if let Some(run) = pipeline_runs
                .iter()
                .filter(|run| run.window_id.as_deref() == Some(window.key.window_id.as_str()))
                .max_by_key(|run| run.updated_at)
            {
                let (done, total) = run.completion();
                window.completion = Some(tend::topology::WorkCompletion { done, total });
                window.pipeline_run = Some(run.summary());
            }
        }
    }
    snapshot
}

/// Stable grouping key for one visible work unit. tmux exposes the complete
/// three-level topology, so the work identity is its window inside a session.
/// Other backends currently expose only a workspace/session level.
pub(super) fn pane_work_group_key(pane: &PaneInfo) -> String {
    let host = tend::backend::pane_id_host_kind(&pane.pane_id).unwrap_or(tend::HostKind::Tmux);
    // JSON is used only as an opaque in-process identity string here. Unlike a
    // delimiter-joined label it cannot collide when a socket path or native id
    // itself contains `:`.
    serde_json::to_string(&WindowKey::from_pane(host, pane))
        .expect("topology window keys are serializable")
}

pub(super) fn pane_work_display_name(pane: &PaneInfo) -> Option<String> {
    if tend::backend::pane_id_host_kind(&pane.pane_id) != Some(tend::HostKind::Tmux) {
        // Non-tmux session labels come from their SessionInfo projection
        // (herdr workspace label, zellij/rmux native metadata). Defer until
        // `finish_work_row`, then append this pane's native tab/window label.
        return None;
    }
    let window = if pane.window_name.trim().is_empty() {
        format!("window {}", pane.window_index)
    } else {
        pane.window_name.clone()
    };
    Some(format!("{} › {window}", pane.session))
}

/// Resolve an agent's session group: the raw session id (display/ledger key)
/// plus the pane's host namespace (grouping key input). Resolved from the
/// agent's pane when it's in the inventory; a paneless / stale agent has no
/// host, so it falls back to a raw synthetic session and `None`.
fn agent_session_group(
    agent: &Agent,
    pane_lookup: impl Fn(&str) -> Option<(String, Option<tend::HostKind>)>,
) -> (String, Option<tend::HostKind>) {
    agent
        .pane
        .as_deref()
        .and_then(&pane_lookup)
        .unwrap_or_else(|| {
            let session = match agent.pane.as_deref() {
                Some(p) => format!("(stale {p})"),
                // Paneless background tasks group under their own name.
                None if agent.kind == AgentKind::Task => agent.session_id.clone(),
                None => "(no session)".to_string(),
            };
            (session, None)
        })
}

#[derive(Default)]
struct WorkRowBuilder {
    /// Raw session id (display/ledger key).
    session: String,
    /// Host-namespaced grouping/identity key.
    group_key: String,
    window_key: Option<WindowKey>,
    display_name: Option<String>,
    panes: Vec<PaneInfo>,
    agents: Vec<Agent>,
    activity: Option<SessionActivity>,
}

fn finish_work_row(mut builder: WorkRowBuilder, sort_context: &SortContext<'_>) -> WorkRow {
    builder.panes.sort_by(sort_panes);
    builder.agents.sort_by(|a, b| {
        b.last_activity_at
            .cmp(&a.last_activity_at)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    let latest_agent = builder.agents.first().cloned();
    let representative_pane = latest_agent
        .as_ref()
        .and_then(|a| a.pane.clone())
        .or_else(|| builder.panes.first().map(|p| p.pane_id.clone()));
    let bare_summary = if latest_agent.is_none() {
        builder.panes.first().map(|p| {
            let first = if p.title.is_empty() || p.title == p.current_command {
                p.current_command.clone()
            } else {
                format!("{}  {}", p.current_command, p.title)
            };
            if builder.panes.len() > 1 {
                format!("{first} · {} panes", builder.panes.len())
            } else {
                first
            }
        })
    } else {
        None
    };
    let agent_states = builder
        .agents
        .iter()
        .map(|agent| ((agent.kind, agent.session_id.clone()), agent.state))
        .collect();
    let display_name = builder.display_name.unwrap_or_else(|| {
        let session = sort_context.display_name(&builder.session);
        builder.panes.first().map_or_else(
            || session.clone(),
            |pane| {
                let window = if pane.window_name.trim().is_empty() {
                    format!("window {}", pane.window_index)
                } else {
                    pane.window_name.clone()
                };
                format!("{session} › {window}")
            },
        )
    });
    WorkRow {
        display_name,
        group_key: builder.group_key,
        window_key: builder.window_key,
        session: builder.session,
        pane_ids: builder
            .panes
            .iter()
            .map(|pane| pane.pane_id.clone())
            .collect(),
        representative_pane,
        latest_agent,
        agents: builder.agents,
        pane_count: builder.panes.len(),
        bare_summary,
        activity: builder.activity,
        // Set from the durable pipeline Run once one is matched to this row;
        // tmux alone cannot say how far a pipeline has got.
        completion: None,
        pipeline_run: None,
        agent_states,
    }
}

pub(super) fn annotate_work_rows(
    rows: &mut Vec<WatchRow>,
    runs: &[tend::pipeline_run::PipelineRun],
) {
    let window_id_counts = rows
        .iter()
        .filter_map(|row| match row {
            WatchRow::Work(work) => work.window_key.as_ref(),
            WatchRow::Agent(_) | WatchRow::BarePane(_) => None,
        })
        .fold(HashMap::<String, usize>::new(), |mut counts, window| {
            *counts.entry(window.window_id.clone()).or_default() += 1;
            counts
        });
    let mut displayed = std::collections::BTreeSet::new();
    for row in rows.iter_mut() {
        let WatchRow::Work(work) = row else {
            continue;
        };
        let Some(window) = work.window_key.as_ref() else {
            continue;
        };
        if window.session.endpoint.host != tend::HostKind::Tmux
            || window_id_counts
                .get(window.window_id.as_str())
                .copied()
                .unwrap_or_default()
                != 1
        {
            continue;
        }
        let Some(run) = runs
            .iter()
            .filter(|run| run.window_id.as_deref() == Some(window.window_id.as_str()))
            .max_by_key(|run| run.updated_at)
        else {
            continue;
        };
        work.completion = Some(run.completion());
        work.pipeline_run = Some(run.summary());
        displayed.insert(run.identity.key());
    }
    // The Run is the durable object; a tmux window is only its current
    // binding. Keep a prompt-free row visible after that binding disappears
    // so watch and list tell the same operational truth.
    for run in runs
        .iter()
        .filter(|run| !displayed.contains(&run.identity.key()))
    {
        rows.push(WatchRow::Work(Box::new(WorkRow {
            session: run.identity.workspace_id.clone(),
            group_key: format!("pipeline:{}", run.identity.key()),
            window_key: None,
            display_name: format!("{} › {}", run.identity.workspace_id, run.identity.work_id),
            pane_ids: Vec::new(),
            representative_pane: None,
            latest_agent: None,
            agents: Vec::new(),
            pane_count: 0,
            bare_summary: Some("durable Run · no live window".to_string()),
            activity: None,
            completion: Some(run.completion()),
            pipeline_run: Some(run.summary()),
            agent_states: HashMap::new(),
        })));
    }
}

pub(super) fn build_work_rows(
    agents: Vec<Agent>,
    panes: &[PaneInfo],
    sessions: &[SessionInfo],
    session_activity: &[SessionActivity],
    sort_keys: &[WatchSortKey],
) -> Vec<WatchRow> {
    // One work row per window, not one per `~view~` member listing it. Jumps
    // still land in the caller's own view (`resolve_jump_session`).
    let canonical = tend::tmux::canonical_panes(panes);
    let panes = canonical.as_slice();
    let sort_context =
        SortContext::new(panes, sessions, session_activity, OffsetDateTime::now_utc());

    // tmux groups each window as one work row. A session with three ticket
    // windows therefore renders three parents, each expanding to its agents.
    let mut builders: HashMap<String, WorkRowBuilder> = HashMap::new();
    for p in panes {
        let key = pane_work_group_key(p);
        let entry = builders
            .entry(key.clone())
            .or_insert_with(|| WorkRowBuilder {
                session: p.session.clone(),
                group_key: key,
                window_key: Some(WindowKey::from_pane(
                    tend::backend::pane_id_host_kind(&p.pane_id).unwrap_or(tend::HostKind::Tmux),
                    p,
                )),
                display_name: pane_work_display_name(p),
                ..WorkRowBuilder::default()
            });
        entry.panes.push(p.clone());
    }

    for agent in agents {
        let located = sort_context.pane_for_agent(&agent).map(|pane| {
            let host =
                tend::backend::pane_id_host_kind(&pane.pane_id).unwrap_or(tend::HostKind::Tmux);
            (
                pane.session.clone(),
                pane_work_group_key(pane),
                Some(WindowKey::from_pane(host, pane)),
                pane_work_display_name(pane),
            )
        });
        let (session, key, window_key, display_name) = located.unwrap_or_else(|| {
            let (session, host) = agent_session_group(&agent, |_| None);
            let key = session_group_key(host, &session);
            (session, key, None, None)
        });
        let entry = builders
            .entry(key.clone())
            .or_insert_with(|| WorkRowBuilder {
                session,
                group_key: key,
                window_key,
                display_name,
                ..WorkRowBuilder::default()
            });
        entry.agents.push(agent);
    }

    for builder in builders.values_mut() {
        // rmux session ids intentionally mirror tmux's `$N` shape, but the
        // activity ledger has no rmux source yet. Never borrow a same-numbered
        // tmux duration for an rmux row.
        if !builder.group_key.starts_with("rmux:") {
            if let Some(activity) = sort_context.activity_for_session_name(&builder.session) {
                builder.activity = Some(activity.clone());
            }
        }
    }

    let mut rows: Vec<WorkRow> = builders
        .into_values()
        .map(|builder| finish_work_row(builder, &sort_context))
        .collect();

    rows.sort_by(|a, b| sort_works(a, b, sort_keys, &sort_context));
    rows.into_iter()
        .map(|row| WatchRow::Work(Box::new(row)))
        .collect()
}

/// The host a row belongs to, classified by its pane id's namespace
/// (`%…`→tmux, `herdr:…`→herdr, `zellij:…`→zellij). `None` when the row
/// carries no classifiable pane id (a paneless agent, a legacy/synthetic
/// id) — such rows get no host badge.
pub(super) fn row_host(row: &WatchRow) -> Option<tend::HostKind> {
    let pane_id = match row {
        WatchRow::Agent(a) => a.pane.as_deref(),
        WatchRow::BarePane(p) => Some(p.pane_id.as_str()),
        WatchRow::Work(s) => s
            .representative_pane
            .as_deref()
            .or_else(|| s.pane_ids.first().map(String::as_str)),
    }?;
    tend::backend::pane_id_host_kind(pane_id)
}
