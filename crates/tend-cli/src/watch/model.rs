use super::{
    io, Agent, AgentKind, AgentState, App, BackendEndpoint, Client, Duration, HashMap, Instant,
    OffsetDateTime, PaneInfo, PaneKey, Path, SessionActivity, TopologyNodeKey, WatchSortKey,
    WindowKey,
};

/// One row of the dashboard. Either a tracked tend agent or a plain tmux
/// pane the daemon doesn't know about — listing both makes `tend watch` a
/// drop-in replacement for tmux's `choose-tree -Zs`.
pub(crate) enum WatchRow {
    Agent(Box<Agent>),
    BarePane(Box<PaneInfo>),
    Work(Box<WorkRow>),
}

/// Stable identity of a table row, used to keep the cursor pinned to the
/// same work / agent / pane across refreshes and re-sorts.
///
/// Deliberately *not* based on `pane_id`: a work row's
/// `representative_pane` is whichever of its agents was most recently
/// active, so it changes on its own as the work progresses. Identity has to
/// survive that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RowIdentity {
    Topology(TopologyNodeKey),
    Agent(AgentKind, String),
    BarePane(PaneKey),
    Work(WorkIdentity),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkIdentity {
    pub(super) window_key: Option<WindowKey>,
    pub(super) fallback: String,
}

/// One selectable line after runtime filtering and optional work
/// expansion have been applied. `agent_idx` points at a child agent inside a
/// work row; `None` is the ordinary top-level row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct VisibleTarget {
    pub(super) row_idx: usize,
    pub(super) agent_idx: Option<usize>,
}

/// One selectable line in the canonical tree presentation. The stable key is
/// the selection identity; depth and branch flags are presentation-only and
/// are rebuilt from the current snapshot on every render/filter operation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // independent branch/disclosure rendering facts
pub(super) struct TreeTarget {
    pub(super) key: TopologyNodeKey,
    /// The only window folded into this session row in pane view. The row
    /// keeps its session identity/action semantics, while rendering the work
    /// name and exposing the window's panes as direct visual children.
    pub(super) compacted_window: Option<WindowKey>,
    pub(super) depth: u8,
    pub(super) is_last_sibling: bool,
    pub(super) parent_is_last_sibling: bool,
    pub(super) has_children: bool,
    pub(super) expanded: bool,
}

/// One-shot context used to place the cursor after the initial empty frame.
/// The endpoint is present when the invoking multiplexer exposes it, allowing
/// `%N` values repeated on another socket to resolve without guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InitialPaneHint {
    pub(super) pane_id: String,
    pub(super) endpoint: Option<BackendEndpoint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WatchEventKind {
    Done,
    Attention,
    Error,
}

#[derive(Debug, Clone)]
pub(super) struct WatchEventEntry {
    pub(super) kind: WatchEventKind,
    pub(super) state: AgentState,
    pub(super) label: String,
    pub(super) summary: String,
    pub(super) occurred_at: OffsetDateTime,
}

pub(super) const MAX_WATCH_EVENTS: usize = 50;

#[derive(Debug, Clone)]
pub(crate) struct WorkRow {
    /// Parent workspace id: tmux session name or herdr workspace id. Activity
    /// remains session/workspace-scoped even though each tmux row is one window.
    pub session: String,
    /// Host-namespaced work identity. tmux keys include stable session and
    /// window ids; other hosts keep their native workspace grouping.
    pub group_key: String,
    /// Canonical identity of this backend window/tab. `None` is reserved for
    /// paneless/stale fallback rows that have no observable topology node.
    pub window_key: Option<WindowKey>,
    /// Human-facing `workspace › work` label on tmux; native workspace label
    /// on hosts that do not expose a window-level work unit.
    pub display_name: String,
    pub pane_ids: Vec<String>,
    pub representative_pane: Option<String>,
    pub latest_agent: Option<Agent>,
    pub agents: Vec<Agent>,
    pub pane_count: usize,
    pub bare_summary: Option<String>,
    pub activity: Option<SessionActivity>,
    /// `(reported, total)` over this work's pipeline agents, or `None` when
    /// the window has none — a hand-split pane never reports, so a ratio
    /// there would read as unfinished work that was never a pipeline.
    ///
    /// Without this a converged pipeline and a stalled one are the same
    /// picture: every agent idle, nothing to say the review already landed.
    pub completion: Option<(usize, usize)>,
    pub pipeline_run: Option<tend::pipeline_run::PipelineRunSummary>,
    pub(super) agent_states: HashMap<(AgentKind, String), AgentState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatchSortPreset {
    Name,
    Latest,
    Duration,
    State,
}

impl WatchSortPreset {
    pub(super) fn keys(self) -> Vec<WatchSortKey> {
        match self {
            Self::Name => vec![WatchSortKey::Name, WatchSortKey::Activity],
            Self::Latest => vec![WatchSortKey::Activity],
            Self::Duration => vec![WatchSortKey::Duration],
            Self::State => vec![WatchSortKey::State, WatchSortKey::Activity],
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Name => "NAME",
            Self::Latest => "LATEST",
            Self::Duration => "DUR",
            Self::State => "ST",
        }
    }
}

pub(super) fn sort_label(keys: &[WatchSortKey]) -> &'static str {
    match keys.first().copied() {
        Some(WatchSortKey::Name) | None => "NAME",
        Some(WatchSortKey::Activity) => "LATEST",
        Some(WatchSortKey::Duration) => "DUR",
        Some(WatchSortKey::State) => "ST",
        Some(WatchSortKey::Pane) => "PANE",
        Some(WatchSortKey::PaneId) => "PANE ID",
    }
}

fn sort_key_toml_name(key: WatchSortKey) -> &'static str {
    match key {
        WatchSortKey::Name => "name",
        WatchSortKey::Activity => "latest",
        WatchSortKey::Duration => "duration",
        WatchSortKey::State => "state",
        WatchSortKey::Pane => "pane",
        WatchSortKey::PaneId => "pane_id",
    }
}

pub(super) fn persist_watch_sort(
    path: &Path,
    keys: &[WatchSortKey],
) -> std::result::Result<(), String> {
    let original = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };

    let mut doc = if original.trim().is_empty() {
        toml_edit::DocumentMut::new()
    } else {
        original
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| format!("parse {}: {e}", path.display()))?
    };

    match doc.get("watch") {
        Some(toml_edit::Item::Table(_)) | None => {}
        Some(_) => return Err("[watch] is not a table".to_string()),
    }
    if doc.get("watch").is_none() {
        doc["watch"] = toml_edit::Item::Table(toml_edit::Table::new());
    }

    let watch = doc["watch"]
        .as_table_mut()
        .ok_or_else(|| "[watch] is not a table".to_string())?;
    let mut sort = toml_edit::Array::new();
    for key in keys {
        sort.push(sort_key_toml_name(*key));
    }
    watch["sort"] = toml_edit::Item::Value(toml_edit::Value::Array(sort));

    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, doc.to_string()).map_err(|e| format!("write {}: {e}", path.display()))
}

impl WatchRow {
    pub(super) fn agent(agent: Agent) -> Self {
        Self::Agent(Box::new(agent))
    }

    pub(super) fn pane_id(&self) -> Option<&str> {
        match self {
            Self::Agent(a) => a.pane.as_deref(),
            Self::BarePane(p) => Some(&p.pane_id),
            Self::Work(s) => s.representative_pane.as_deref(),
        }
    }

    /// Identity that survives a rebuild of the row set — see [`RowIdentity`].
    pub(super) fn identity(&self) -> RowIdentity {
        match self {
            Self::Agent(a) => RowIdentity::Agent(a.kind, a.session_id.clone()),
            Self::BarePane(p) => RowIdentity::BarePane(PaneKey::from_pane(
                tend::backend::pane_id_host_kind(&p.pane_id).unwrap_or(tend::HostKind::Tmux),
                p,
            )),
            Self::Work(s) => RowIdentity::Work(WorkIdentity {
                window_key: s.window_key.clone(),
                fallback: s.group_key.clone(),
            }),
        }
    }

    pub(super) fn contains_pane(&self, pane_id: &str) -> bool {
        match self {
            Self::Agent(a) => a.pane.as_deref() == Some(pane_id),
            Self::BarePane(p) => p.pane_id == pane_id,
            Self::Work(s) => {
                s.representative_pane.as_deref() == Some(pane_id)
                    || s.pane_ids.iter().any(|id| id == pane_id)
                    || s.agents.iter().any(|a| a.pane.as_deref() == Some(pane_id))
            }
        }
    }
}

/// A daemon error to surface in the header. We track whether the inner
/// error is the "daemon not reachable" variant so the renderer can drop the
/// `daemon error: ` prefix — otherwise the message reads
/// "daemon error: daemon not reachable at …" which is awkward.
#[derive(Clone)]
pub(crate) struct DaemonError {
    pub message: String,
    pub self_describing: bool,
}

/// Ask the daemon who it is, and record it when the answer is a different
/// build than this one.
///
/// Run once at startup and again after a restart. A skew does not change
/// between refreshes — the daemon would have to be replaced for that, which is
/// exactly the event we re-probe on — so this stays off the 500 ms tick.
///
/// A `hello` that fails outright is left alone: the daemon is unreachable or
/// refusing, and `last_error` already carries a better sentence for that than
/// a version comparison could.
/// Restart the daemon from inside watch and wait for the replacement to be
/// serving, without printing anything — the TUI owns the screen.
///
/// Completion is the new image's generation, not the daemon's acknowledgement.
/// A daemon accepts a restart before it drains, so returning on the ack would
/// report success while the old process was still the one answering, and the
/// skew re-probe that follows would read the corpse.
pub(super) async fn restart_daemon_from_watch(client: &Client) -> std::result::Result<(), String> {
    let before = client
        .hello(Duration::from_secs(2))
        .await
        .map_err(|error| error.to_string())?;
    if !before.capabilities.iter().any(|c| c == "restart") {
        return Err("this tendd is too old to restart itself; run `tend daemon restart`".into());
    }
    let generation = before
        .generation
        .ok_or_else(|| "tendd did not report an image generation".to_string())?;
    let request_error = client.restart(Duration::from_secs(5)).await.err();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if let Ok(after) = client.hello(Duration::from_secs(1)).await {
            if after.generation.is_some_and(|now| now > generation) {
                return Ok(());
            }
        }
    }
    Err(request_error.map_or_else(
        || "tendd accepted the restart but did not come back".to_string(),
        |error| error.to_string(),
    ))
}

pub(super) async fn probe_version_skew(client: &Client, app: &mut App) {
    if let Ok(hello) = client.hello(Duration::from_secs(2)).await {
        app.version_skew = hello.version_skew();
        app.open_counts_supported = hello
            .capabilities
            .iter()
            .any(|cap| cap == tend::collaboration::OPEN_COUNTS_CAPABILITY);
        app.human_query_supported = hello
            .capabilities
            .iter()
            .any(|cap| cap == tend::collaboration::HUMAN_QUERY_CAPABILITY);
        app.exact_session_supported = hello
            .capabilities
            .iter()
            .any(|cap| cap == tend::collaboration::EXACT_SESSION_CAPABILITY);
    }
}

/// The one-line warning for a daemon that is out of step, or `None` when the
/// two halves agree.
///
/// Covers both shapes of the same problem. A skew detected through `hello` is
/// the quiet one — every request still succeeds, so nothing else on screen
/// would ever mention it. A `protocol mismatch` in `last_error` is the loud
/// one, where the daemon is old enough to refuse the requests outright and the
/// table has been empty ever since. Both are fixed by the same keystroke, so
/// both get the same bar.
pub(crate) fn skew_banner(app: &App) -> Option<String> {
    let mismatched = app
        .last_error
        .as_ref()
        .is_some_and(|error| error.message.contains("protocol mismatch"));
    let skew = app.version_skew.as_ref();
    if skew.is_none() && !mismatched {
        return None;
    }
    let versions = skew.map_or_else(
        || "tendd is older than this CLI".to_string(),
        |skew| {
            format!(
                "tendd {} \u{2260} tend {}",
                skew.daemon_label(),
                skew.client
            )
        },
    );
    Some(format!(
        "\u{26a0} {versions} \u{2014} the daemon kept running across the upgrade. \
         Fix it here: `:daemon restart`"
    ))
}
