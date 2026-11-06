use super::{
    agent_matches_query, agent_needs_attention, agent_pane_display, annotate_work_rows,
    build_watch_topology, build_work_rows, compare_topology_siblings, contains_ci,
    default_tree_expanded, pane_display, pulse_kind, resolve_display_columns, row_matches_query,
    row_needs_attention, sort_agents, sort_panes, sort_works, target_pane, topology_keys,
    tree_pane_relevant, tree_session_relevant, tree_window_relevant,
    unambiguous_session_activity_secs, visible_tree_parent_key, ActionOutcome, Agent, AgentKind,
    AgentState, AskComposer, AskPanelState, BTreeMap, BackendEndpoint, BoardCollab,
    BoardMailboxCache, BoardPending, BoardRefresher, BoardScreen, BoardStats, BoardWorkers,
    BroadcastReport, CollabScreen, CollaborationComposeDefaults, CollaborationComposer,
    CollaborationMailboxState, CollaborationMark, ConfirmPopup, DaemonError, HashMap, HashSet,
    InitialPaneHint, InspectorSplit, Instant, MessageSkillEditor, OffsetDateTime, PaneGeometry,
    PaneInfo, PaneKey, PaneNode, Pulse, PulseKind, RenameComposer, RenameLevel, RequestKind,
    RowIdentity, SessionActivity, SessionInfo, SessionKey, SessionNode, SortContext, SpawnComposer,
    TableState, Text, TopologyNodeKey, TopologyNodeRef, TopologySnapshot, TopologySortFacts,
    TreeTarget, VecDeque, VersionSkew, VisibleTarget, WatchCollaboration, WatchColumn, WatchConfig,
    WatchEventEntry, WatchEventKind, WatchHistoryCache, WatchLayout, WatchRow, WatchScreen,
    WatchSortPreset, WatchTreeExpansion, WatchView, WindowKey, WindowNode, WorkComposer,
    WorkIdentity, FOOTER_HINT_TTL, MAX_WATCH_EVENTS, PULSE_WINDOW, WINDOW_CAPTURE_PARALLELISM,
};

/// Editable `:` command line, with the same UTF-8-safe cursor behavior
/// as the request composer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CommandPalette {
    pub input: String,
    pub cursor: usize,
}

impl CommandPalette {
    pub(super) fn insert(&mut self, c: char) {
        let idx = char_to_byte_idx(&self.input, self.cursor);
        self.input.insert(idx, c);
        self.cursor += 1;
    }

    pub(super) fn insert_str(&mut self, text: &str) {
        for c in text.chars() {
            self.insert(c);
        }
    }

    pub(super) fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = char_to_byte_idx(&self.input, self.cursor - 1);
        let end = char_to_byte_idx(&self.input, self.cursor);
        self.input.replace_range(start..end, "");
        self.cursor -= 1;
    }

    pub(super) fn delete(&mut self) {
        if self.cursor >= self.input.chars().count() {
            return;
        }
        let start = char_to_byte_idx(&self.input, self.cursor);
        let end = char_to_byte_idx(&self.input, self.cursor + 1);
        self.input.replace_range(start..end, "");
    }

    pub(super) fn delete_word(&mut self) {
        while self.cursor > 0
            && self
                .input
                .chars()
                .nth(self.cursor - 1)
                .is_some_and(char::is_whitespace)
        {
            self.backspace();
        }
        while self.cursor > 0
            && self
                .input
                .chars()
                .nth(self.cursor - 1)
                .is_some_and(|c| !c.is_whitespace())
        {
            self.backspace();
        }
    }

    pub(super) fn clear(&mut self) {
        self.input.clear();
        self.cursor = 0;
    }

    pub(super) fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub(super) fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.input.chars().count());
    }

    pub(super) fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub(super) fn move_end(&mut self) {
        self.cursor = self.input.chars().count();
    }
}

pub(super) fn char_to_byte_idx(s: &str, char_idx: usize) -> usize {
    if char_idx == 0 {
        return 0;
    }
    s.char_indices()
        .nth(char_idx)
        .map_or_else(|| s.len(), |(idx, _)| idx)
}

/// A transient hint pinned to the footer for ~2 s after a quick action
/// runs (or fails). Replaces the default keybinding strip while active.
#[derive(Debug, Clone)]
pub(crate) struct FooterHint {
    pub message: String,
    pub level: HintLevel,
    pub set_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HintLevel {
    /// Green — successful action, "✔ copied prompt".
    Ok,
    /// Red — action failed; user probably wants to try again.
    Err,
    /// Yellow — action vetoed because the row didn't qualify
    /// (paneless agent, missing prompt). Not the user's fault, but
    /// not a backend failure either.
    Warn,
}

impl FooterHint {
    pub(super) fn fresh(&self) -> bool {
        self.set_at.elapsed() < FOOTER_HINT_TTL
    }
}

/// State held by the TUI.
///
/// Kept separate from rendering so the smoke test can construct it
/// directly without touching a real terminal.
#[allow(clippy::struct_excessive_bools)] // independent overlay/filter/runtime flags are clearer than a coupled state enum
pub(crate) struct App {
    pub rows: Vec<WatchRow>,
    /// Canonical nested snapshot behind every production presentation and
    /// action. Compatibility rows remain as a transition/event cache and for
    /// the test-only flat renderer; they never define tree identity.
    pub topology: TopologySnapshot,
    /// Internal harness for the pre-tree renderer's regression tests. It is
    /// never enabled by the production constructor path and can be deleted
    /// with those golden tests after the tree snapshots have replaced them.
    pub(super) legacy_flat_table: bool,
    pub table_state: TableState,
    /// Monotonic frame counter advanced once per paint. Drives the swarm
    /// view's dot-spinner animation; unused by the table views.
    pub anim_frame: usize,
    /// Last-seen state per agent, diffed on each update to detect transitions
    /// that should flash a [`Pulse`]. Keyed by `(kind, session_id)` — the
    /// codebase's compound agent identity — so two agents that happen to share
    /// a `session_id` string across kinds don't clobber each other.
    prev_agent_states: HashMap<(AgentKind, String), AgentState>,
    /// Active transition pulses, same `(kind, session_id)` key. Pruned as they
    /// expire past [`PULSE_WINDOW`].
    pulses: HashMap<(AgentKind, String), Pulse>,
    /// Incremental filter typed into the table once `/` has armed it.
    pub search_query: String,
    /// True after `/` armed filter input — the only way in, since typing on
    /// its own stays bound to the browse keys. It remains true while the
    /// query is empty so reserved keys (`q`, `g`, `h`, …) can open a search.
    pub explicit_search: bool,
    /// First half of the conventional `gg` jump. Any non-`g` key clears it.
    pub(super) pending_g: bool,
    /// Number of selectable rows in the most recently rendered table body.
    /// Page movement uses this rather than a hard-coded terminal size.
    pub(super) table_page_rows: usize,
    /// When true, only error / input / choice targets remain visible.
    pub attention_only: bool,
    /// Explicitly expanded session/window keys. Pane keys are leaves and never
    /// enter this set. Keys survive refresh and sort because they include the
    /// complete host+socket ancestry.
    pub(super) expanded_nodes: HashSet<TopologyNodeKey>,
    /// Whether default expansion has been applied to at least one non-empty
    /// topology. This distinguishes an intentionally-collapsed tree from the
    /// empty first frame.
    tree_expansion_initialized: bool,
    /// Full key selected before a search/attention filter hid it. The visible
    /// cursor may temporarily sit on an ancestor, but clearing the filter
    /// restores this node. Explicit navigation while filtered replaces it.
    filtered_selection_anchor: Option<TopologyNodeKey>,
    /// Work group currently expanded into individually-selectable child rows.
    /// Selection keeps this to at most one group: the selected work, or the
    /// parent work of the selected child agent.
    expanded_works: HashSet<String>,
    /// Wide terminals show a persistent live inspector unless explicitly
    /// disabled with Alt-I.
    pub inspector_enabled: bool,
    /// Set by the last render pass; lets the capture loop avoid polling a
    /// live pane when the terminal is too narrow to show the inspector.
    pub inspector_visible: bool,
    /// Whether the last rendered viewport contains a working/starting state.
    /// Computed from the already-built frame model so the 16 ms input loop does
    /// not rebuild the hierarchy just to choose an animation cadence.
    pub(super) visible_spinner_active: bool,
    /// Persistent transition inbox for this watch process.
    pub(super) events: VecDeque<WatchEventEntry>,
    pub unread_events: usize,
    pub event_inbox_open: bool,
    pub last_error: Option<DaemonError>,
    /// Set when the daemon answering this watch is not the same build as this
    /// binary. Held separately from `last_error` because it is not an error —
    /// nothing has failed yet, and on a matching protocol nothing will. It is
    /// the state that silently precedes every mismatch after an upgrade.
    pub version_skew: Option<VersionSkew>,
    pub last_refresh: OffsetDateTime,
    /// Watch config — held by value so the rendering path doesn't need to
    /// re-resolve columns every frame, and the smoke tests can swap it in.
    pub watch_cfg: WatchConfig,
    /// Column set resolved from `watch_cfg` once at construction. Unknown
    /// keys are warned-and-skipped here (see `resolve_columns`).
    pub columns: Vec<WatchColumn>,
    /// Snapshot of the full tmux pane inventory from the last refresh.
    /// Used by `pane_display` to render `session:window.pane` labels for
    /// agent rows without shelling out to tmux on every render frame —
    /// per-row resolves used to cost ~5 ms each, so a 35-agent table
    /// blocked the input loop for ~175 ms per paint.
    pub panes: Vec<PaneInfo>,
    /// Snapshot of tmux sessions from the last refresh. Only populated for
    /// tmux hosts; zellij and host-down cases leave it empty.
    pub sessions: Vec<SessionInfo>,
    /// Persisted cumulative attached-time counters keyed by tmux session id.
    pub session_activity: Vec<SessionActivity>,
    /// Daemon-owned pipeline Runs from the last coherent refresh.
    pub pipeline_runs: Vec<muxa::pipeline_run::PipelineRun>,
    /// True between a user-triggered refresh request (`r`) and the
    /// matching outcome landing on the channel. Surfaces a brief
    /// "↻ refreshing…" hint in the header so mashing `r` during a
    /// slow daemon snapshot isn't silently swallowed. Periodic
    /// background ticks intentionally don't toggle this — only the
    /// user's deliberate action lights it up.
    pub refresh_pending: bool,
    /// Pane to focus on the first non-empty data load. In the topology view
    /// this resolves to the nearest visible ancestor at the configured depth.
    /// Consumed once so later refreshes never steal the user's selection.
    initial_pane: Option<InitialPaneHint>,
    /// `Some` when the user has popped open the full-screen detail
    /// preview (`o` / `Alt-P`). The table is hidden behind the preview while
    /// this is set; `q`/`Esc`/`p` clears it.
    pub preview: Option<PreviewState>,
    /// Count of paneless agents that were filtered out of `rows` because
    /// `watch_cfg.hide_paneless` is true. Surfaced as a footer hint so
    /// users know the rows aren't lost — they just aren't actionable from
    /// the picker. Always 0 when `hide_paneless = false`.
    pub paneless_hidden: usize,
    /// Of the `paneless_hidden` agents, how many are blocked on a human
    /// (`WaitingInput` / `WaitingChoice` / `Error`). These would otherwise
    /// be completely invisible — no row, and the plain `+N paneless` footer
    /// hint doesn't say any of them need you — so the header attention
    /// summary folds this count in. Always 0 when `hide_paneless = false`
    /// (they're shown as normal rows and counted there).
    pub paneless_attention: usize,
    /// Most recent `tmux capture-pane -ep` result, keyed by `pane_id`.
    /// Populated on demand when the preview is in
    /// [`PreviewContent::LivePane`] and re-captured on every refresh
    /// tick while the preview stays open in that mode. `None` when the
    /// preview is closed or showing prompt/response content.
    pub pane_capture: Option<CapturedPane>,
    /// Geometry and visible screen contents for the selected tmux window.
    /// Unlike `pane_capture`, this owns one capture per visible pane and is
    /// refreshed at most once per second. Non-tmux windows leave it unused and
    /// retain the textual pane roster.
    pub window_capture: Option<CapturedWindow>,
    /// Exact selected tmux window for which the last render exposed a mosaic.
    /// The async capture scheduler reads this O(1) field instead of rebuilding
    /// the full topology on every input-poll iteration.
    pub(super) window_capture_target: Option<WindowKey>,
    /// `Some` when a destructive action (`Alt-K`, `Alt-X`, or a `:` command)
    /// is waiting on a
    /// y/N reply. Suppresses table input and renders a centred popup;
    /// the input handler resolves the popup before any other key is
    /// interpreted.
    pub confirm: Option<ConfirmPopup>,
    pub(super) collaboration: WatchCollaboration,
    pub(super) collaboration_history: WatchHistoryCache,
    /// `[collaboration] scope` from config: `host` lets the composer target
    /// the tracked agent under the cursor in any window; `window` keeps the
    /// classic same-room contract.
    pub(super) collaboration_scope: muxa::config::CollaborationScope,
    /// Table/inspector width split, cycled with `|`. Session-local: a
    /// glance preference, not configuration.
    pub(super) inspector_split: InspectorSplit,
    pub(super) collaboration_mailbox: CollaborationMailboxState,
    /// What `W` returns to. A toggle that always went back to `tree` would
    /// silently discard a swarm the operator was in the middle of using.
    previous_layout: WatchLayout,
    /// Fleet-wide collaboration listing, populated only while its screen is
    /// the one on show — an operator surface has no business polling every
    /// room in the daemon to render a topology tree.
    pub(crate) collab: CollabScreen,
    /// The Work board, populated only while its screen is on show: building
    /// it scans every multiplexer and reads the Work records.
    pub(crate) board: BoardScreen,
    pub(super) board_refresher: BoardRefresher,
    /// The board operation in progress, with its targets frozen when its
    /// composer or confirmation opened. Only that operation's own submission
    /// or confirmation passes the board gate, once; cancelling, confirming or
    /// leaving the board ends it.
    pub(super) board_pending: Option<BoardPending>,
    /// Background many-pane actions still running, with the panes each is
    /// writing to: no other action may write to those panes meanwhile.
    pub(super) pending_outcomes: Vec<(Vec<PaneKey>, tokio::sync::oneshot::Receiver<ActionOutcome>)>,
    pub(super) board_stats: BoardStats,
    pub(super) board_collab: BoardCollab,
    pub(super) board_collab_cache: BoardMailboxCache,
    pub(super) board_workers: BoardWorkers,
    /// The daemon resolves exact agent sessions (`session:` targets and
    /// exact-session origins). Board collaboration acts for one exact agent,
    /// so it is refused without this rather than risk a pane fallback.
    pub(super) exact_session_supported: bool,
    pub(super) open_counts_supported: bool,
    pub(super) human_query_supported: bool,
    pub(super) collaboration_composer: Option<CollaborationComposer>,
    /// Agents the operator marked with `Space`, to be addressed together.
    ///
    /// Keyed by `(socket, pane, agent session)` — the same triple the mailbox
    /// pins a recipient to — so a mark names one agent session rather than a
    /// screen position. A re-sort cannot move it onto someone else, and an
    /// agent that exits takes its mark with it: resolution intersects the set
    /// with the room's current peers, and what is gone is dropped rather than
    /// silently addressed.
    pub(super) collaboration_marks: std::collections::HashSet<CollaborationMark>,
    /// Per-recipient outcome of the last broadcast, kept on screen until
    /// dismissed. A fan-out that reports one line cannot say which of five
    /// agents did not get it.
    pub(super) broadcast_report: Option<BroadcastReport>,
    /// Reusable templates loaded once from `[message.skills]`; palette
    /// filtering never touches disk or the daemon.
    pub(super) message_skills: BTreeMap<String, String>,
    /// Add/update form layered over an active message or ask skill palette.
    pub(super) message_skill_editor: Option<MessageSkillEditor>,
    /// Last kind/mode chosen in a request composer. Seeded from `[watch]`
    /// and updated immediately as its badges cycle.
    pub(super) collaboration_compose_defaults: CollaborationComposeDefaults,
    /// `Some` while the `n` spawn form is open.
    pub(super) spawn: Option<SpawnComposer>,
    /// `Some` while the `a` ask composer is open.
    pub(super) ask_composer: Option<AskComposer>,
    pub(super) work_composer: Option<WorkComposer>,
    pub(super) agent_move: Option<crate::agent_move::MoveMenu>,
    pub(super) rename: Option<RenameComposer>,
    pub(super) ask_panel: AskPanelState,
    pub(super) ask_entries: Vec<muxa::ask::AskEntry>,
    /// Durable conversations, newest first, and the one whose transcript the
    /// Ask panel is showing. Older daemons leave both empty and retain the
    /// legacy provider-filtered history behavior.
    pub(super) ask_conversations: Vec<muxa::ask::AskConversation>,
    pub(super) active_ask_conversation_id: Option<String>,
    pub(super) ask_conversations_available: bool,
    /// Agent the next question goes to, as the daemon reports it.
    pub(super) ask_agent: String,
    /// Editable `:` command palette. Like other overlays it owns keyboard
    /// input until Enter executes or Esc cancels.
    pub command_palette: Option<CommandPalette>,
    /// True while the `?` help overlay is visible. Renders as a centred
    /// popup listing every keybinding — the same pattern as `confirm`,
    /// just with a static body and no follow-up dispatch.
    pub help_open: bool,
    pub(super) demo_available: bool,
    /// Transient footer hint set after a quick action runs. The
    /// renderer hides the default keybinding strip while this is fresh
    /// (`set_at.elapsed() < FOOTER_HINT_TTL`). The run loop never
    /// explicitly clears the slot — the renderer just stops reading it.
    pub footer_hint: Option<FooterHint>,
}

/// A `muxa watch` preview overlay — detail view of the selected agent.
/// Geometry (popup vs fullscreen) and content (prompt/response vs live
/// pane capture) are independent axes so the two toggles compose: a
/// user can read the prompt in a popup, then `c` to flip to a live pane
/// view in the same popup, then `f` to fullscreen that. The selection
/// is pinned to a `pane_id` (not a row index) so background refreshes
/// that re-sort the table can't drift the preview onto a different
/// agent.
#[derive(Debug, Clone)]
pub struct PreviewState {
    /// Pane id at the time the preview was opened — also the lookup key
    /// every render frame uses to find the live agent record.
    pub pane_id: String,
    /// Exact topology identity when opened from the tree. Legacy previews
    /// retain `None` and use their historical pane-id lookup.
    pub pane_key: Option<PaneKey>,
    /// Vertical scroll offset in *lines from the top of the content*.
    /// `↑/↓` (or `j/k`) increment / decrement; `saturating_*` so we
    /// can't underflow past the top.
    pub scroll: u16,
    /// Whether to render as a centred popup (default — keeps the
    /// surrounding table visible for context) or as a full-screen take-
    /// over. The `f` key toggles between the two.
    pub mode: PreviewMode,
    /// What's being shown inside the box: prompt + response (default,
    /// text-only) or a live capture of the actual tmux pane contents.
    /// `c` toggles between the two.
    pub content: PreviewContent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewMode {
    /// Centred popup roughly 80% × 70% of the table area. Default — the
    /// surrounding rows stay visible so the user keeps a sense of "where
    /// am I in the list" while reading the prompt/response.
    Popup,
    /// Full-screen takeover, useful for very long responses where the
    /// 80×70 box still wraps too aggressively.
    Fullscreen,
}

/// Content axis of the preview overlay. Re-exported from
/// [`muxa::config::PreviewContent`] so tests and downstream code in the
/// watch crate can keep `PreviewContent::LivePane` working without
/// having to know the enum lives in the muxa core. Independent of
/// [`PreviewMode`]'s geometry — both compose freely. The runtime
/// rendering path always reads from this enum; `[watch.preview]
/// default_content` only seeds the initial value when a fresh
/// `PreviewState` is constructed.
pub use muxa::config::PreviewContent;

/// Cached pane-capture result. One slot per `App` since only the
/// currently-previewed pane needs a capture; flipping rows or closing
/// the preview invalidates by `pane_id` mismatch and the next refresh
/// repopulates.
#[derive(Debug, Clone)]
pub struct CapturedPane {
    pub pane_id: String,
    pub socket: Option<String>,
    /// Raw stdout from `tmux capture-pane -ep`, ANSI escapes intact.
    /// Decoded to ratatui `Text` lazily at render time so a stale row
    /// re-render never re-parses the same bytes.
    pub text: String,
    /// ANSI decoded once when the capture arrives; redraws clone the compact
    /// Ratatui model instead of reparsing escape sequences at animation rate.
    pub(super) parsed: Option<Text<'static>>,
    /// Monotonic timestamp; the main loop uses `elapsed()` to gate the
    /// next re-capture so we don't shell out every frame.
    pub fetched_at: std::time::Instant,
}

impl CapturedPane {
    pub(super) fn new(pane_id: String, socket: Option<String>, text: String) -> Self {
        use ansi_to_tui::IntoText;

        let parsed = (!text.is_empty())
            .then(|| text.as_bytes().into_text().ok())
            .flatten();
        Self {
            pane_id,
            socket,
            text,
            parsed,
            fetched_at: std::time::Instant::now(),
        }
    }
}

/// One pane inside a cached window mosaic.
#[derive(Debug, Clone)]
pub struct CapturedWindowPane {
    pub geometry: PaneGeometry,
    /// ANSI-decoded visible terminal, prepared off the input thread.
    pub text: Option<Text<'static>>,
}

/// Cached geometry and terminal contents for one selected window.
#[derive(Debug, Clone)]
pub struct CapturedWindow {
    pub key: WindowKey,
    pub panes: Vec<CapturedWindowPane>,
    pub zoomed: bool,
    pub fetched_at: std::time::Instant,
}

/// The explorer already exists in watch's navigation tree. Omit its terminal
/// before capture, and rebase the work panes so it leaves no empty side strip.
pub(super) fn content_window_geometry(raw: &str) -> (Vec<PaneGeometry>, bool) {
    let content = raw
        .lines()
        .filter(|line| line.split('\t').nth(11) != Some("1"))
        .collect::<Vec<_>>()
        .join("\n");
    let (mut panes, zoomed) = muxa::tmux::layout::parse_pane_geometry_lines(&content);
    let left = panes.iter().map(|pane| pane.left).min().unwrap_or(0);
    let top = panes.iter().map(|pane| pane.top).min().unwrap_or(0);
    for pane in &mut panes {
        pane.left -= left;
        pane.top -= top;
    }
    (panes, zoomed)
}

pub(super) fn pane_capture_target(key: &PaneKey) -> String {
    if key.window.session.endpoint.host == muxa::HostKind::Rmux {
        muxa::backend::rmux::qualified_pane_target(
            &key.window.session.session_id,
            &key.window.window_id,
            &key.pane_id,
        )
        .unwrap_or_else(|| key.pane_id.clone())
    } else {
        key.pane_id.clone()
    }
}

pub(super) fn capture_tmux_window(key: WindowKey) -> CapturedWindow {
    use ansi_to_tui::IntoText;

    let socket = key.session.endpoint.socket.clone();
    let format = format!(
        "{}\t#{{@muxa_sidebar}}",
        muxa::tmux::layout::PANE_GEOMETRY_FMT
    );
    let (geometries, zoomed) = crate::mux_control::capture(
        &key.session.endpoint,
        &[
            "list-panes",
            "-t",
            &format!("{}:{}", key.session.session_id, key.window_id),
            "-F",
            &format,
        ],
    )
    .map(|raw| content_window_geometry(&raw))
    .unwrap_or_default();
    let backend = crate::backend_for_kind(key.session.endpoint.host);

    // A slow or wedged pane must not make N panes cost N timeout windows.
    // Each capture is independent, so run them concurrently inside the one
    // blocking task owned by the watch loop.
    let visible = geometries
        .into_iter()
        .filter(|geometry| !zoomed || geometry.active)
        .collect::<Vec<_>>();
    let mut panes = Vec::with_capacity(visible.len());
    for batch in visible.chunks(WINDOW_CAPTURE_PARALLELISM) {
        panes.extend(std::thread::scope(|scope| {
            let handles = batch
                .iter()
                .map(|geometry| {
                    let backend = backend.clone();
                    let socket = socket.clone();
                    let geometry = geometry.clone();
                    let capture_target = pane_capture_target(&PaneKey {
                        window: key.clone(),
                        pane_id: geometry.pane_id.clone(),
                    });
                    scope.spawn(move || {
                        let text = backend
                            .capture_pane_on(Some(&socket), &capture_target)
                            .filter(|raw| !raw.is_empty())
                            .and_then(|raw| raw.as_bytes().into_text().ok());
                        CapturedWindowPane { geometry, text }
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .filter_map(|handle| handle.join().ok())
                .collect::<Vec<_>>()
        }));
    }

    CapturedWindow {
        key,
        panes,
        zoomed,
        fetched_at: std::time::Instant::now(),
    }
}

impl App {
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        // Pin pane view for the pane-level tests built around this helper
        // (sorting, grouping, per-row actions, single-agent updates). The
        // *production* default is `work` (see `WatchConfig::default`);
        // work-view behavior is covered separately by config-crate tests and
        // the explicit `view: Work` cases below. Pinning here keeps
        // those pane-mechanics assertions stable regardless of the default.
        let mut app = Self::with_config(WatchConfig {
            view: WatchView::Pane,
            ..WatchConfig::default()
        });
        app.legacy_flat_table = true;
        app
    }

    #[cfg(test)]
    pub(super) fn with_legacy_config(cfg: WatchConfig) -> Self {
        let mut app = Self::with_config(cfg);
        app.legacy_flat_table = true;
        app
    }

    pub(crate) fn with_config(cfg: WatchConfig) -> Self {
        let columns = resolve_display_columns(&cfg);
        let inspector_split = cfg
            .inspector_split
            .as_deref()
            .and_then(InspectorSplit::from_label)
            .unwrap_or_default();
        let collaboration_compose_defaults = CollaborationComposeDefaults {
            kind: cfg.collaboration_kind.unwrap_or(RequestKind::Question),
            mode: cfg.collaboration_mode.unwrap_or_default().into(),
        };
        let mut collab = CollabScreen::default();
        collab.set_layout(cfg.collab_layout);
        let mut board = BoardScreen::default();
        board.set_sort(cfg.board_sort);
        Self {
            rows: Vec::new(),
            topology: TopologySnapshot::build(OffsetDateTime::now_utc(), Vec::new(), Vec::new()),
            legacy_flat_table: false,
            table_state: TableState::default(),
            anim_frame: 0,
            prev_agent_states: HashMap::new(),
            pulses: HashMap::new(),
            search_query: String::new(),
            explicit_search: false,
            pending_g: false,
            table_page_rows: 10,
            attention_only: false,
            expanded_nodes: HashSet::new(),
            tree_expansion_initialized: false,
            filtered_selection_anchor: None,
            expanded_works: HashSet::new(),
            inspector_enabled: true,
            inspector_visible: false,
            visible_spinner_active: false,
            events: VecDeque::new(),
            unread_events: 0,
            event_inbox_open: false,
            last_error: None,
            version_skew: None,
            last_refresh: OffsetDateTime::now_utc(),
            watch_cfg: cfg,
            columns,
            panes: Vec::new(),
            sessions: Vec::new(),
            session_activity: Vec::new(),
            pipeline_runs: Vec::new(),
            refresh_pending: false,
            initial_pane: None,
            preview: None,
            paneless_hidden: 0,
            paneless_attention: 0,
            pane_capture: None,
            window_capture: None,
            window_capture_target: None,
            confirm: None,
            collaboration: WatchCollaboration::default(),
            collaboration_history: WatchHistoryCache::default(),
            collaboration_scope: muxa::config::CollaborationScope::default(),
            inspector_split,
            collaboration_mailbox: CollaborationMailboxState::default(),
            previous_layout: WatchLayout::Tree,
            collab,
            board,
            board_refresher: BoardRefresher::default(),
            board_pending: None,
            board_stats: BoardStats::default(),
            board_collab: BoardCollab::default(),
            board_collab_cache: BoardMailboxCache::default(),
            board_workers: BoardWorkers::default(),
            exact_session_supported: false,
            open_counts_supported: false,
            human_query_supported: false,
            pending_outcomes: Vec::new(),
            collaboration_composer: None,
            collaboration_marks: std::collections::HashSet::new(),
            broadcast_report: None,
            message_skills: BTreeMap::new(),
            message_skill_editor: None,
            collaboration_compose_defaults,
            spawn: None,
            ask_composer: None,
            work_composer: None,
            agent_move: None,
            rename: None,
            ask_panel: AskPanelState::default(),
            ask_entries: Vec::new(),
            ask_conversations: Vec::new(),
            active_ask_conversation_id: None,
            ask_conversations_available: false,
            ask_agent: "claude".into(),
            command_palette: None,
            help_open: false,
            demo_available: std::env::var_os("TMUX").is_none() && which::which("tmux").is_ok(),
            footer_hint: None,
        }
    }

    /// Hint which pane should be highlighted on first load — typically
    /// `$TMUX_PANE` so launching `muxa watch` from inside tmux lands the
    /// cursor on the user's current pane instead of always row 0.
    #[cfg(test)]
    pub(crate) fn set_initial_pane(&mut self, pane: Option<String>) {
        self.set_initial_pane_on(pane, None);
    }

    pub(super) fn set_initial_pane_on(
        &mut self,
        pane: Option<String>,
        endpoint: Option<BackendEndpoint>,
    ) {
        self.initial_pane = pane.map(|pane_id| InitialPaneHint { pane_id, endpoint });
    }

    /// Replace the row set. `agents` (tracked) are listed first in stable
    /// order; `panes` minus any pane already represented by an agent are
    /// appended as `BarePane` rows.
    #[cfg(test)]
    pub(crate) fn set_data(&mut self, agents: Vec<Agent>, panes: Vec<PaneInfo>) {
        self.set_data_with_sessions(agents, panes, Vec::new(), Vec::new());
    }

    /// Clone the lightweight agent records needed for transition detection.
    /// A work row also keeps a cloned agent list, so this is independent of
    /// the current table granularity.
    pub(super) fn current_agents(&self) -> Vec<Agent> {
        self.rows
            .iter()
            .flat_map(|r| match r {
                WatchRow::Agent(a) => vec![(**a).clone()],
                WatchRow::Work(s) => s.agents.clone(),
                WatchRow::BarePane(_) => Vec::new(),
            })
            .collect()
    }

    /// Diff the current agent states against the previous snapshot and record a
    /// [`Pulse`] for any transition worth flashing (done / error), and append
    /// durable entries to the in-process event inbox. Disabling animation
    /// suppresses pulses only; inbox tracking remains active.
    pub(super) fn detect_pulses(&mut self) {
        if !self.watch_cfg.spinner {
            self.pulses.clear();
        }
        let now = std::time::Instant::now();
        let current = self.current_agents();
        for agent in &current {
            let key = (agent.kind, agent.session_id.clone());
            let previous = self.prev_agent_states.get(&key).copied();
            if self.watch_cfg.spinner {
                if let Some(pk) = pulse_kind(previous, agent.state) {
                    self.pulses.insert(
                        key.clone(),
                        Pulse {
                            kind: pk,
                            started: now,
                        },
                    );
                }
            }

            let event_kind = match (previous, agent.state) {
                (Some(AgentState::Working), AgentState::Idle) => Some(WatchEventKind::Done),
                (Some(prev), AgentState::Error) if prev != AgentState::Error => {
                    Some(WatchEventKind::Error)
                }
                (Some(prev), AgentState::WaitingInput | AgentState::WaitingChoice)
                    if prev != agent.state =>
                {
                    Some(WatchEventKind::Attention)
                }
                _ => None,
            };
            if let Some(kind) = event_kind {
                let label = agent.pane.as_deref().map_or_else(
                    || agent.session_id.clone(),
                    |_| agent_pane_display(agent, &self.panes),
                );
                let summary = muxa::prompt_headline(
                    agent
                        .last_notification
                        .as_deref()
                        .or(agent.last_response.as_deref())
                        .or(agent.last_prompt.as_deref())
                        .unwrap_or(""),
                );
                let summary = summary
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(80)
                    .collect();
                self.events.push_front(WatchEventEntry {
                    kind,
                    state: agent.state,
                    label,
                    summary,
                    occurred_at: agent.state_entered_at,
                });
                self.events.truncate(MAX_WATCH_EVENTS);
                if !self.event_inbox_open {
                    self.unread_events = self.unread_events.saturating_add(1);
                }
            }
        }
        self.prev_agent_states = current
            .into_iter()
            .map(|a| ((a.kind, a.session_id), a.state))
            .collect();
        self.pulses
            .retain(|_, p| now.duration_since(p.started) < PULSE_WINDOW);
    }

    /// The active pulse for `(kind, session_id)`, if one is still within its
    /// window.
    pub(super) fn active_pulse(
        &self,
        kind: AgentKind,
        session_id: &str,
        now: std::time::Instant,
    ) -> Option<PulseKind> {
        self.pulses
            .get(&(kind, session_id.to_string()))
            .and_then(|p| (now.duration_since(p.started) < PULSE_WINDOW).then_some(p.kind))
    }

    /// Whether any pulse is still lit (gates the fast repaint cadence).
    pub(super) fn has_active_pulse(&self, now: std::time::Instant) -> bool {
        self.pulses
            .values()
            .any(|p| now.duration_since(p.started) < PULSE_WINDOW)
    }

    pub(crate) fn set_data_with_sessions(
        &mut self,
        mut agents: Vec<Agent>,
        panes: Vec<PaneInfo>,
        sessions: Vec<SessionInfo>,
        session_activity: Vec<SessionActivity>,
    ) {
        // Remember *which row* the cursor is on before the row set is
        // rebuilt from scratch below. Every refresh re-sorts, and the
        // default `sort = ["state", "name", "latest"]` reorders as
        // agents flip state, so holding the raw table index would silently
        // slide the highlight onto a neighbouring session.
        let selected = self.selection_identity_for_rebuild();
        let previous_topology_keys = topology_keys(&self.topology);

        // Build the shared topology before presentation-specific filtering.
        // This retains paneless/ambiguous agents in `unassigned_agents` and
        // gives every tree action a collision-free target.
        self.topology = build_watch_topology(&agents, &panes, &sessions, &self.pipeline_runs);
        self.reconcile_tree_expansion(&previous_topology_keys);

        // Filter out paneless agents up front when the user has opted in
        // (the default). They can't be attached to from the picker — Enter
        // is a no-op — so listing them just clutters the actionable view.
        // The count is preserved on `paneless_hidden` so the footer can
        // surface a `+N paneless` hint and the rows aren't silently lost.
        self.paneless_hidden = 0;
        self.paneless_attention = 0;
        if self.watch_cfg.hide_paneless {
            let before = agents.len();
            // Background tasks are intentionally paneless but are the whole
            // point of being visible, so they're exempt from the hide.
            // Before dropping the rest, tally how many of them are blocked
            // on a human — a detached/SDK-hosted agent that goes
            // WaitingInput has no row and no attend target, so the header
            // attention summary is the only place it can surface.
            self.paneless_attention = agents
                .iter()
                .filter(|a| a.pane.is_none() && a.kind != AgentKind::Task)
                .filter(|a| agent_needs_attention(a.state))
                .count();
            agents.retain(|a| a.pane.is_some() || a.kind == AgentKind::Task);
            self.paneless_hidden = before - agents.len();
        }

        if self.watch_cfg.layout == WatchLayout::Swarm || self.watch_cfg.view != WatchView::Pane {
            self.rows = build_work_rows(
                agents,
                &panes,
                &sessions,
                &session_activity,
                &self.watch_cfg.sort,
            );
            annotate_work_rows(&mut self.rows, &self.pipeline_runs);
            self.panes = panes;
            self.sessions = sessions;
            self.session_activity = session_activity;
            self.last_refresh = OffsetDateTime::now_utc();
            self.restore_selection(selected);
            return;
        }

        // Sort agent rows according to the user's `[watch] sort` config.
        // Stale agents (pane already closed, i.e. lookup miss against the
        // panes inventory) always bucket at the end so live agents stay
        // visually grouped at the top regardless of the sort keys.
        //
        // Agent records carry only `pane_id`; session / window / pane
        // indices are resolved via the panes inventory collected this
        // refresh.
        let sort_context = SortContext::new(
            &panes,
            &sessions,
            &session_activity,
            OffsetDateTime::now_utc(),
        );
        let sort_keys = &self.watch_cfg.sort;
        agents.sort_by(|a, b| sort_agents(a, b, sort_keys, &sort_context));

        let known: HashSet<PaneKey> = self
            .topology
            .sessions
            .iter()
            .flat_map(|session| &session.windows)
            .flat_map(|window| &window.panes)
            .filter(|pane| pane.agent.is_some())
            .map(|pane| pane.key.clone())
            .collect();

        let mut bare: Vec<PaneInfo> = panes
            .iter()
            .filter(|pane| {
                let host =
                    muxa::backend::pane_id_host_kind(&pane.pane_id).unwrap_or(muxa::HostKind::Tmux);
                !known.contains(&PaneKey::from_pane(host, pane))
            })
            .cloned()
            .collect();
        bare.sort_by(|a, b| {
            a.session
                .cmp(&b.session)
                .then_with(|| a.window_index.cmp(&b.window_index))
                .then_with(|| a.pane_index.cmp(&b.pane_index))
        });

        let mut rows: Vec<WatchRow> = Vec::with_capacity(agents.len() + bare.len());
        rows.extend(agents.into_iter().map(WatchRow::agent));
        rows.extend(bare.into_iter().map(|p| WatchRow::BarePane(Box::new(p))));

        self.rows = rows;
        // Keep the *full* pane inventory (not just the bare ones) so
        // `pane_display` can resolve `session:window.pane` labels for
        // agent rows by lookup instead of a tmux shell-out per render.
        self.panes = panes;
        self.sessions = sessions;
        self.session_activity = session_activity;
        self.last_refresh = OffsetDateTime::now_utc();
        self.restore_selection(selected);
    }

    pub(crate) fn apply_sort_preset(&mut self, preset: WatchSortPreset) {
        self.watch_cfg.sort = preset.keys();
        self.resort_rows_preserving_selection();
    }

    /// Change granularity using the cached refresh payload, then keep the
    /// cursor on the same pane where possible. The next daemon refresh uses
    /// the new view automatically because `watch_cfg.view` is already set.
    pub(crate) fn apply_view(&mut self, view: WatchView) {
        if self.watch_cfg.view == view {
            return;
        }
        let selected = self.selection_identity_for_rebuild();
        let paneless_hidden = self.paneless_hidden;
        let paneless_attention = self.paneless_attention;
        let agents = self.current_agents();
        let panes = self.panes.clone();
        let sessions = self.sessions.clone();
        let session_activity = self.session_activity.clone();

        self.watch_cfg.view = view;
        self.columns = resolve_display_columns(&self.watch_cfg);
        self.expanded_works.clear();
        self.expanded_nodes.clear();
        self.tree_expansion_initialized = false;
        self.set_data_with_sessions(agents, panes, sessions, session_activity);
        // Hidden paneless agents are not present in `current_agents()`, so a
        // cache-only view rebuild cannot recount them. Preserve the counts
        // until the next full daemon refresh supplies the complete agent set.
        self.paneless_hidden = paneless_hidden;
        self.paneless_attention = paneless_attention;

        if let Some(selected) = selected {
            self.restore_selection(Some(selected));
        }
    }

    /// Switch which list watch is showing.
    ///
    /// Nothing about the topology changes, so the tree keeps its cursor,
    /// expansion and filter for when the operator comes back to it.
    pub(crate) fn apply_screen(&mut self, screen: WatchScreen) {
        self.watch_cfg.screen = screen;
        if matches!(screen, WatchScreen::Collab) {
            // The cursor belongs to whatever the listing turns out to hold,
            // and the previous visit's row is not it.
            self.collab.select_first();
        }
    }

    /// The topology key for a pane a collaboration request named, when that
    /// pane is still on screen. Requests outlive the panes that sent them, so
    /// this is genuinely optional rather than an invariant.
    pub(crate) fn pane_key_for(&self, pane_id: &str, socket: Option<&str>) -> Option<PaneKey> {
        self.topology
            .sessions
            .iter()
            .flat_map(|session| session.windows.iter())
            .flat_map(|window| window.panes.iter())
            .find(|pane| {
                pane.key.pane_id == pane_id
                    // A pane id is only unique within one server, so a socket
                    // the request carried has to match as well.
                    && socket.is_none_or(|socket| {
                        pane.key.window.session.endpoint.socket == socket
                    })
            })
            .map(|pane| pane.key.clone())
    }

    /// The topology key for a pane of a Work's Run, when it is on screen.
    ///
    /// Host, session, window and pane must agree, and the server through
    /// [`muxa::backend::pane_endpoints_match`]: a tmux socket's path and its
    /// short name are the same server, while rmux and cmux endpoints are
    /// compared whole, since two servers can share a file name.
    pub(crate) fn board_pane_key(
        &self,
        run: &muxa::work::RunSnapshot,
        pane_id: &str,
    ) -> Option<PaneKey> {
        let wanted = &run.execution;
        self.topology
            .sessions
            .iter()
            .flat_map(|session| session.windows.iter())
            .flat_map(|window| window.panes.iter())
            .map(|pane| &pane.key)
            .find(|key| {
                key.pane_id == pane_id
                    && key.window.session.endpoint.host == wanted.host
                    && key.window.window_id == wanted.window_id
                    && key.window.session.session_id == wanted.session_id
                    && muxa::backend::pane_endpoints_match(
                        Some(pane_id),
                        &key.window.session.endpoint.socket,
                        &wanted.socket,
                    )
            })
            .cloned()
    }

    /// What `W` would switch to from here, without switching.
    pub(super) fn next_work_layout(&self) -> WatchLayout {
        if self.watch_cfg.layout == WatchLayout::Work {
            self.previous_layout
        } else {
            WatchLayout::Work
        }
    }

    pub(crate) fn apply_layout(&mut self, layout: WatchLayout) {
        if self.watch_cfg.layout == layout {
            return;
        }
        // Whichever route reached the work table — `W`, the palette, or a
        // config default — `W` has to know what it interrupted.
        if layout == WatchLayout::Work {
            self.previous_layout = self.watch_cfg.layout;
        }
        let selected = self.selection_identity_for_rebuild();
        let agents = self.current_agents();
        let panes = self.panes.clone();
        let sessions = self.sessions.clone();
        let activity = self.session_activity.clone();
        self.watch_cfg.layout = layout;
        self.set_data_with_sessions(agents, panes, sessions, activity);
        if let Some(selected) = selected {
            self.restore_selection(Some(selected));
        }
    }

    fn resort_rows_preserving_selection(&mut self) {
        let selected = self.selection_identity_for_rebuild();
        self.resort_rows_restoring_selection(selected);
    }

    pub(super) fn resort_rows_restoring_selection(&mut self, selected: Option<RowIdentity>) {
        if self.uses_tree() {
            self.restore_selection(selected);
            return;
        }
        let sort_context = SortContext::new(
            &self.panes,
            &self.sessions,
            &self.session_activity,
            OffsetDateTime::now_utc(),
        );
        let sort_keys = &self.watch_cfg.sort;

        if self.watch_cfg.view == WatchView::Window {
            self.rows.sort_by(|a, b| match (a, b) {
                (WatchRow::Work(a), WatchRow::Work(b)) => {
                    sort_works(a, b, sort_keys, &sort_context)
                }
                (WatchRow::Work(_), _) => std::cmp::Ordering::Less,
                (_, WatchRow::Work(_)) => std::cmp::Ordering::Greater,
                _ => std::cmp::Ordering::Equal,
            });
        } else {
            self.rows.sort_by(|a, b| match (a, b) {
                (WatchRow::Agent(a), WatchRow::Agent(b)) => {
                    sort_agents(a, b, sort_keys, &sort_context)
                }
                (WatchRow::Agent(_), _) => std::cmp::Ordering::Less,
                (_, WatchRow::Agent(_)) => std::cmp::Ordering::Greater,
                (WatchRow::BarePane(a), WatchRow::BarePane(b)) => sort_panes(a, b),
                _ => std::cmp::Ordering::Equal,
            });
        }

        self.restore_selection(selected);
    }

    pub(super) fn uses_tree(&self) -> bool {
        // Both tree and swarm are presentations over the same canonical
        // node set. Only the test-only legacy renderer opts out.
        !self.legacy_flat_table
    }

    fn reconcile_tree_expansion(&mut self, previous_keys: &HashSet<TopologyNodeKey>) {
        let current = topology_keys(&self.topology);
        self.expanded_nodes.retain(|key| current.contains(key));
        if self.watch_cfg.tree_expansion != WatchTreeExpansion::Always {
            return;
        }
        let initialize_all = !self.tree_expansion_initialized && !self.topology.sessions.is_empty();
        for session in &self.topology.sessions {
            let session_key = session.node_key();
            if default_tree_expanded(&session_key, self.watch_cfg.view)
                && (initialize_all || !previous_keys.contains(&session_key))
            {
                self.expanded_nodes.insert(session_key);
            }
            for window in &session.windows {
                let window_key = window.node_key();
                if default_tree_expanded(&window_key, self.watch_cfg.view)
                    && (initialize_all || !previous_keys.contains(&window_key))
                {
                    self.expanded_nodes.insert(window_key);
                }
            }
        }
        if initialize_all {
            self.tree_expansion_initialized = true;
        }
    }

    /// Apply the accordion expansion implied by `key`, then return the same
    /// key so callers can re-pin the cursor after the visible indices change.
    ///
    /// Focus mode opens one level below the selected node, bounded by `view`:
    /// a selected session reveals windows, and in pane view a selected window
    /// reveals panes. Explicitly selected descendants always keep their full
    /// ancestry open even when they are deeper than the automatic view depth.
    /// Moving to another session folds the previous path.
    fn set_focused_tree_expansion(&mut self, key: Option<&TopologyNodeKey>) {
        if self.watch_cfg.tree_expansion != WatchTreeExpansion::Focus {
            return;
        }
        self.expanded_nodes.clear();
        let Some(key) = key else {
            return;
        };
        match key {
            TopologyNodeKey::Session(session) => {
                if matches!(self.watch_cfg.view, WatchView::Window | WatchView::Pane) {
                    self.expanded_nodes
                        .insert(TopologyNodeKey::Session(session.clone()));
                }
            }
            TopologyNodeKey::Window(window) => {
                self.expanded_nodes
                    .insert(TopologyNodeKey::Session(window.session.clone()));
                if self.watch_cfg.view == WatchView::Pane {
                    self.expanded_nodes
                        .insert(TopologyNodeKey::Window(window.clone()));
                }
            }
            TopologyNodeKey::Pane(pane) => {
                self.expanded_nodes
                    .insert(TopologyNodeKey::Session(pane.window.session.clone()));
                self.expanded_nodes
                    .insert(TopologyNodeKey::Window(pane.window.clone()));
            }
        }
    }

    fn sync_focused_tree_expansion(&mut self, key: TopologyNodeKey) {
        self.set_focused_tree_expansion(Some(&key));
        if let Some(index) = self
            .tree_targets()
            .iter()
            .position(|target| target.key == key)
        {
            self.table_state.select(Some(index));
        }
    }

    pub(super) fn tree_targets(&self) -> Vec<TreeTarget> {
        if self.watch_cfg.layout == WatchLayout::Work {
            return self.work_targets();
        }
        let query = self.search_query.trim().to_lowercase();
        let filtering = !query.is_empty() || self.attention_only;
        let sessions = self.sorted_sessions();

        let mut targets = Vec::new();
        let matching_sessions: Vec<_> = sessions
            .into_iter()
            .filter(|session| tree_session_relevant(session, &query, self.attention_only))
            .collect();
        let session_count = matching_sessions.len();
        for (session_index, session) in matching_sessions.into_iter().enumerate() {
            let windows = self.sorted_windows(session.windows.iter().filter(|window| {
                tree_window_relevant(window, session, &query, self.attention_only)
            }));
            // Pane view is about reaching agents. When the unfiltered
            // topology has a single window, a separate session -> window
            // step repeats the same scope without adding a choice. Fold the
            // work name into the session row and expose panes directly. A
            // filter deliberately restores the full ancestry so a lone
            // matching window is never mistaken for the session's full
            // topology.
            let compacted_window = (!filtering
                && self.watch_cfg.view == WatchView::Pane
                && session.windows.len() == 1)
                .then(|| &session.windows[0]);
            let session_key = session.node_key();
            let session_expanded =
                self.expanded_nodes.contains(&session_key) || (filtering && !windows.is_empty());
            targets.push(TreeTarget {
                key: session_key,
                compacted_window: compacted_window.map(|window| window.key.clone()),
                depth: 0,
                is_last_sibling: session_index + 1 == session_count,
                parent_is_last_sibling: true,
                has_children: compacted_window.map_or_else(
                    || !session.windows.is_empty(),
                    |window| !window.panes.is_empty(),
                ),
                expanded: session_expanded,
            });
            if !session_expanded {
                continue;
            }

            if let Some(window) = compacted_window {
                let panes = self.sorted_panes(window.panes.iter());
                let pane_count = panes.len();
                for (pane_index, pane) in panes.into_iter().enumerate() {
                    targets.push(TreeTarget {
                        key: pane.node_key(),
                        compacted_window: None,
                        depth: 1,
                        is_last_sibling: pane_index + 1 == pane_count,
                        parent_is_last_sibling: true,
                        has_children: false,
                        expanded: false,
                    });
                }
                continue;
            }

            let window_count = windows.len();
            for (window_index, window) in windows.into_iter().enumerate() {
                let panes = self.sorted_panes(window.panes.iter().filter(|pane| {
                    tree_pane_relevant(pane, session, window, &query, self.attention_only)
                }));
                let window_key = window.node_key();
                let window_expanded =
                    self.expanded_nodes.contains(&window_key) || (filtering && !panes.is_empty());
                targets.push(TreeTarget {
                    key: window_key,
                    compacted_window: None,
                    depth: 1,
                    is_last_sibling: window_index + 1 == window_count,
                    parent_is_last_sibling: session_index + 1 == session_count,
                    has_children: !window.panes.is_empty(),
                    expanded: window_expanded,
                });
                if !window_expanded {
                    continue;
                }
                let pane_count = panes.len();
                for (pane_index, pane) in panes.into_iter().enumerate() {
                    targets.push(TreeTarget {
                        key: pane.node_key(),
                        compacted_window: None,
                        depth: 2,
                        is_last_sibling: pane_index + 1 == pane_count,
                        parent_is_last_sibling: window_index + 1 == window_count,
                        has_children: false,
                        expanded: false,
                    });
                }
            }
        }
        targets
    }

    /// One row per Work, in the sibling order the tree would have used.
    ///
    /// Window nodes, not a parallel row type: everything that acts on the
    /// cursor — attach, preview, the composer, kill — addresses a topology
    /// key, so a Work here answers those exactly as it does in the tree, and
    /// the selection survives a layout switch without translation.
    fn work_targets(&self) -> Vec<TreeTarget> {
        let query = self.search_query.trim().to_lowercase();
        let mut targets = Vec::new();
        let sessions: Vec<_> = self
            .sorted_sessions()
            .into_iter()
            .filter(|session| tree_session_relevant(session, &query, self.attention_only))
            .collect();
        for session in sessions {
            for window in self.sorted_windows(session.windows.iter().filter(|window| {
                tree_window_relevant(window, session, &query, self.attention_only)
            })) {
                targets.push(TreeTarget {
                    key: window.node_key(),
                    // Session-row compaction is a tree affordance: it folds a
                    // lone window into its session so the tree does not spend
                    // two rows on one Work. Here the Work *is* the row.
                    compacted_window: None,
                    // Flat: no ancestry to draw, and nothing to expand into
                    // — a Work's panes are the ALIASES column here.
                    depth: 0,
                    is_last_sibling: false,
                    parent_is_last_sibling: true,
                    has_children: false,
                    expanded: false,
                });
            }
        }
        if let Some(last) = targets.last_mut() {
            last.is_last_sibling = true;
        }
        targets
    }

    pub(super) fn sorted_sessions(&self) -> Vec<&SessionNode> {
        let now = OffsetDateTime::now_utc();
        let mut decorated = self
            .topology
            .sessions
            .iter()
            .map(|session| {
                (
                    session,
                    TopologySortFacts::for_session(
                        session,
                        unambiguous_session_activity_secs(self, session, now).unwrap_or(0),
                    ),
                )
            })
            .collect::<Vec<_>>();
        decorated.sort_by(|(_, left), (_, right)| {
            compare_topology_siblings(&self.watch_cfg.sort, left, right)
        });
        decorated.into_iter().map(|(session, _)| session).collect()
    }

    pub(super) fn sorted_windows<'a>(
        &self,
        windows: impl Iterator<Item = &'a WindowNode>,
    ) -> Vec<&'a WindowNode> {
        let mut decorated = windows
            .map(|window| (window, TopologySortFacts::for_window(window)))
            .collect::<Vec<_>>();
        decorated.sort_by(|(_, left), (_, right)| {
            compare_topology_siblings(&self.watch_cfg.sort, left, right)
        });
        decorated.into_iter().map(|(window, _)| window).collect()
    }

    pub(super) fn sorted_panes<'a>(
        &self,
        panes: impl Iterator<Item = &'a PaneNode>,
    ) -> Vec<&'a PaneNode> {
        let mut decorated = panes
            .map(|pane| (pane, TopologySortFacts::for_pane(pane)))
            .collect::<Vec<_>>();
        decorated.sort_by(|(_, left), (_, right)| {
            compare_topology_siblings(&self.watch_cfg.sort, left, right)
        });
        decorated.into_iter().map(|(pane, _)| pane).collect()
    }

    fn selected_tree_target(&self) -> Option<TreeTarget> {
        let index = self.table_state.selected()?;
        self.tree_targets().get(index).cloned()
    }

    pub(super) fn selected_node_key(&self) -> Option<TopologyNodeKey> {
        self.selected_tree_target().map(|target| target.key)
    }

    /// Open the rename form on the row under the cursor, prefilled with the
    /// name it has now.
    ///
    /// The cursor lands at the end of the prefill: renaming is usually an edit
    /// of what is there (`eng-7306` → `eng-7306-review`), and a cursor at
    /// column zero turns the first keystroke into a prefix splice.
    pub(super) fn open_rename(&mut self) {
        let Some(node) = self.selected_node() else {
            self.set_hint(
                "rename needs a session, window, or pane row",
                HintLevel::Warn,
            );
            return;
        };
        let (key, level, original) = match node {
            TopologyNodeRef::Session(session) => (
                session.node_key(),
                RenameLevel::Session,
                session.name.clone(),
            ),
            TopologyNodeRef::Window(window) => {
                (window.node_key(), RenameLevel::Window, window.name.clone())
            }
            TopologyNodeRef::Pane(pane) => (pane.node_key(), RenameLevel::Pane, pane.title.clone()),
        };
        if !crate::mux_control::supported(key.endpoint().host) {
            self.set_hint("rename is unavailable for this backend", HintLevel::Warn);
            return;
        }
        let cursor = original.chars().count();
        self.rename = Some(RenameComposer {
            key,
            level,
            original: original.clone(),
            input: original,
            cursor,
        });
    }

    pub(super) fn selected_node(&self) -> Option<TopologyNodeRef<'_>> {
        let key = self.selected_node_key()?;
        self.topology.find(&key)
    }

    /// Resolve the `c` binding: whichever session owns the selected row, plus
    /// the directory that row works in. Every level answers — a session is
    /// the target itself, and a window or pane names the session it lives in
    /// — so the key never depends on where in the tree the cursor stopped.
    pub(super) fn new_window_context(&self) -> Option<(SessionKey, Option<String>)> {
        Some(match self.selected_node()? {
            TopologyNodeRef::Session(session) => (
                session.key.clone(),
                session
                    .active_window()
                    .and_then(|window| window.cwd.clone()),
            ),
            TopologyNodeRef::Window(window) => (window.key.session.clone(), window.cwd.clone()),
            TopologyNodeRef::Pane(pane) => (
                pane.key.window.session.clone(),
                (!pane.cwd.is_empty()).then(|| pane.cwd.clone()),
            ),
        })
    }

    pub(super) fn spawn_context(
        &self,
        fallback_dir: String,
    ) -> (Option<TopologyNodeKey>, String, String) {
        if !self.uses_tree() {
            return (None, fallback_dir, String::new());
        }
        let Some(node) = self.selected_node() else {
            return (None, fallback_dir, String::new());
        };
        match node {
            TopologyNodeRef::Session(session) => (
                Some(session.node_key()),
                session
                    .active_window()
                    .and_then(|window| window.cwd.clone())
                    .unwrap_or(fallback_dir),
                String::new(),
            ),
            TopologyNodeRef::Window(window) => (
                Some(window.node_key()),
                window.cwd.clone().unwrap_or(fallback_dir),
                window.name.clone(),
            ),
            TopologyNodeRef::Pane(pane) => (
                Some(pane.node_key()),
                if pane.cwd.is_empty() {
                    fallback_dir
                } else {
                    pane.cwd.clone()
                },
                self.topology
                    .find(&TopologyNodeKey::Window(pane.key.window.clone()))
                    .and_then(|node| match node {
                        TopologyNodeRef::Window(window) => Some(window.name.clone()),
                        TopologyNodeRef::Session(_) | TopologyNodeRef::Pane(_) => None,
                    })
                    .unwrap_or_default(),
            ),
        }
    }

    /// Identity of the row the cursor is on, if any.
    pub(super) fn selected_identity(&self) -> Option<RowIdentity> {
        if self.uses_tree() {
            return self.selected_node_key().map(RowIdentity::Topology);
        }
        let target = self.selected_target()?;
        self.identity_for_target(target)
    }

    fn topology_filtering(&self) -> bool {
        !self.search_query.trim().is_empty() || self.attention_only
    }

    pub(super) fn selection_identity_for_rebuild(&self) -> Option<RowIdentity> {
        if self.uses_tree() && self.topology_filtering() {
            self.filtered_selection_anchor
                .clone()
                .map(RowIdentity::Topology)
                .or_else(|| self.selected_identity())
        } else {
            self.selected_identity()
        }
    }

    pub(super) fn pin_filtered_tree_selection(&mut self) {
        if self.uses_tree() && self.topology_filtering() {
            self.filtered_selection_anchor = self.selected_node_key();
        }
    }

    fn identity_for_target(&self, target: VisibleTarget) -> Option<RowIdentity> {
        if let Some(agent_idx) = target.agent_idx {
            let WatchRow::Work(session) = self.rows.get(target.row_idx)? else {
                return None;
            };
            let agent = session.agents.get(agent_idx)?;
            Some(RowIdentity::Agent(agent.kind, agent.session_id.clone()))
        } else {
            self.rows.get(target.row_idx).map(WatchRow::identity)
        }
    }

    fn work_group_for_identity(&self, identity: &RowIdentity) -> Option<String> {
        if self.uses_tree() {
            return None;
        }
        match identity {
            RowIdentity::Work(identity) => self.rows.iter().find_map(|row| {
                let WatchRow::Work(session) = row else {
                    return None;
                };
                let matches = identity.window_key.as_ref().map_or_else(
                    || session.group_key == identity.fallback,
                    |key| session.window_key.as_ref() == Some(key),
                );
                (matches && session.pane_count > 1 && !session.agents.is_empty())
                    .then(|| session.group_key.clone())
            }),
            RowIdentity::Agent(kind, session_id) => self.rows.iter().find_map(|row| {
                let WatchRow::Work(session) = row else {
                    return None;
                };
                (session.pane_count > 1
                    && !session.agents.is_empty()
                    && session
                        .agents
                        .iter()
                        .any(|agent| agent.kind == *kind && agent.session_id == *session_id))
                .then(|| session.group_key.clone())
            }),
            RowIdentity::Topology(_) | RowIdentity::BarePane(_) => None,
        }
    }

    fn set_auto_expansion(&mut self, identity: Option<&RowIdentity>) {
        if self.uses_tree() {
            let key = identity.and_then(|identity| match identity {
                RowIdentity::Topology(key) => Some(key),
                RowIdentity::Agent(_, _) | RowIdentity::BarePane(_) | RowIdentity::Work(_) => None,
            });
            self.set_focused_tree_expansion(key);
            return;
        }
        let group_key = identity.and_then(|id| self.work_group_for_identity(id));
        self.expanded_works.clear();
        if let Some(group_key) = group_key {
            self.expanded_works.insert(group_key);
        }
    }

    fn target_index_for_identity(&self, identity: &RowIdentity) -> Option<usize> {
        if let RowIdentity::Topology(key) = identity {
            return self
                .tree_targets()
                .iter()
                .position(|target| &target.key == key);
        }
        self.visible_targets().iter().position(|target| {
            self.identity_for_target(*target)
                .is_some_and(|candidate| &candidate == identity)
        })
    }

    /// Keep the selected work open without requiring a separate expand
    /// keystroke. Selecting one of its child agents keeps the same parent open;
    /// moving to another work folds the previous one and opens the new one.
    pub(super) fn sync_auto_expansion(&mut self) {
        if self.uses_tree() {
            return;
        }
        let identity = self.selected_identity();
        self.set_auto_expansion(identity.as_ref());
        if let Some(identity) = identity {
            if let Some(index) = self.target_index_for_identity(&identity) {
                self.table_state.select(Some(index));
            }
        }
    }

    /// Re-pin the cursor after `rows` was rebuilt or re-sorted.
    ///
    /// **Why identity and not the table index**: rows are re-sorted on
    /// every refresh (~2 Hz, plus one per pushed transition), and the
    /// default sort leads with `state`. When a *neighbouring* work
    /// starts working or goes blocked it jumps past the highlighted row,
    /// which shifts everything below it down one. Keeping the raw index
    /// then leaves the highlight sitting on a different work than the one
    /// the user aimed at — the "list looks shifted by one, Enter opened the
    /// wrong work" report. Pinning by identity makes the
    /// highlight travel *with* its row, so Enter always targets what the
    /// user is looking at.
    ///
    /// Falls back to the index clamp only when the row genuinely
    /// disappeared (work window closed, agent exited).
    fn restore_selection(&mut self, previous: Option<RowIdentity>) {
        if let Some(prev) = previous.as_ref() {
            self.set_auto_expansion(Some(prev));
            if let Some(idx) = self.target_index_for_identity(prev) {
                self.table_state.select(Some(idx));
                return;
            }
            if let RowIdentity::Topology(key) = prev {
                let ancestors: Vec<TopologyNodeKey> = match key {
                    TopologyNodeKey::Pane(pane) => vec![
                        TopologyNodeKey::Window(pane.window.clone()),
                        TopologyNodeKey::Session(pane.window.session.clone()),
                    ],
                    TopologyNodeKey::Window(window) => {
                        vec![TopologyNodeKey::Session(window.session.clone())]
                    }
                    TopologyNodeKey::Session(_) => Vec::new(),
                };
                for ancestor in ancestors {
                    if let Some(index) =
                        self.target_index_for_identity(&RowIdentity::Topology(ancestor))
                    {
                        self.table_state.select(Some(index));
                        return;
                    }
                }
            }
        }
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        if self.uses_tree() {
            self.clamp_tree_selection();
            return;
        }
        let targets = self.visible_targets();
        if targets.is_empty() {
            self.table_state.select(None);
            self.expanded_works.clear();
            return;
        }
        match self.table_state.selected() {
            Some(i) if i >= targets.len() => {
                self.table_state.select(Some(targets.len() - 1));
            }
            None => {
                // First non-empty load: prefer the row matching the pane the
                // user invoked muxa from, so the cursor lands on context.
                // `take()` ensures later refreshes don't re-snap selection.
                let hint = self.initial_pane.take();
                let initial = hint
                    .as_ref()
                    .and_then(|hint| {
                        targets.iter().position(|target| {
                            target_pane(self, *target).as_deref() == Some(&hint.pane_id)
                                || self.rows[target.row_idx].contains_pane(&hint.pane_id)
                        })
                    })
                    .unwrap_or(0);
                self.table_state.select(Some(initial));
            }
            Some(_) => {}
        }
        self.sync_auto_expansion();
    }

    fn initial_tree_target_index(
        &self,
        hint: &InitialPaneHint,
        targets: &[TreeTarget],
    ) -> Option<usize> {
        let pane = self.initial_tree_pane_key(hint)?;

        [
            TopologyNodeKey::Pane(pane.clone()),
            TopologyNodeKey::Window(pane.window.clone()),
            TopologyNodeKey::Session(pane.window.session.clone()),
        ]
        .into_iter()
        .find_map(|key| targets.iter().position(|target| target.key == key))
    }

    fn initial_tree_pane_key(&self, hint: &InitialPaneHint) -> Option<PaneKey> {
        let mut matches = self
            .topology
            .sessions
            .iter()
            .flat_map(|session| &session.windows)
            .flat_map(|window| &window.panes)
            .filter(|pane| {
                pane.key.pane_id == hint.pane_id
                    && hint
                        .endpoint
                        .as_ref()
                        .is_none_or(|endpoint| &pane.key.window.session.endpoint == endpoint)
            });
        let pane = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        Some(pane.key.clone())
    }

    fn clamp_tree_selection(&mut self) {
        let mut targets = self.tree_targets();
        if targets.is_empty() {
            self.table_state.select(None);
            return;
        }
        match self.table_state.selected() {
            Some(index) if index >= targets.len() => {
                self.table_state.select(Some(targets.len() - 1));
            }
            None => {
                let hint = self.initial_pane.take();
                if self.watch_cfg.tree_expansion == WatchTreeExpansion::Focus {
                    if let Some(pane) = hint
                        .as_ref()
                        .and_then(|hint| self.initial_tree_pane_key(hint))
                    {
                        let initial_key = match self.watch_cfg.view {
                            WatchView::Session => {
                                TopologyNodeKey::Session(pane.window.session.clone())
                            }
                            WatchView::Window => TopologyNodeKey::Window(pane.window.clone()),
                            WatchView::Pane => TopologyNodeKey::Pane(pane),
                        };
                        self.set_focused_tree_expansion(Some(&initial_key));
                        targets = self.tree_targets();
                    }
                }
                let initial = hint
                    .as_ref()
                    .and_then(|hint| self.initial_tree_target_index(hint, &targets))
                    .unwrap_or(0);
                self.table_state.select(Some(initial));
            }
            Some(_) => {}
        }
        if self.watch_cfg.tree_expansion == WatchTreeExpansion::Focus {
            if let Some(key) = self.selected_node_key() {
                self.sync_focused_tree_expansion(key);
            }
        }
    }

    pub(crate) fn move_down(&mut self) {
        self.move_vertical(1);
    }

    pub(crate) fn move_up(&mut self) {
        self.move_vertical(-1);
    }

    pub(super) fn move_first(&mut self) {
        self.move_to_vertical_boundary(false);
    }

    pub(super) fn move_last(&mut self) {
        self.move_to_vertical_boundary(true);
    }

    pub(super) fn move_page_down(&mut self) {
        self.move_vertical_bounded(self.page_step(false));
    }

    pub(super) fn move_page_up(&mut self) {
        self.move_vertical_bounded(-self.page_step(false));
    }

    pub(super) fn move_half_page_down(&mut self) {
        self.move_vertical_bounded(self.page_step(true));
    }

    pub(super) fn move_half_page_up(&mut self) {
        self.move_vertical_bounded(-self.page_step(true));
    }

    fn page_step(&self, half: bool) -> isize {
        let rows = if half {
            (self.table_page_rows / 2).max(1)
        } else {
            self.table_page_rows.max(1)
        };
        isize::try_from(rows).unwrap_or(isize::MAX)
    }

    /// Move across work parents by default, even though the selected work's
    /// children are already visible. Once `→` enters a child, the same keys
    /// cycle only that work's visible children until `←` returns
    /// to the parent. This keeps a long child roster from slowing down the
    /// common case of scanning between work items.
    fn move_vertical(&mut self, delta: isize) {
        if self.uses_tree() {
            self.move_tree_vertical(delta, true);
            return;
        }
        let targets = self.visible_targets();
        if targets.is_empty() {
            return;
        }
        let selected = self.selected_visible_index(&targets);
        let candidates = self.vertical_candidates(&targets, selected);
        if candidates.is_empty() {
            return;
        }
        let current_position =
            selected.and_then(|index| candidates.iter().position(|candidate| *candidate == index));
        let next_position = match (current_position, delta.is_positive()) {
            (Some(position), true) => (position + 1) % candidates.len(),
            (Some(0), false) => candidates.len() - 1,
            (Some(position), false) => position - 1,
            (None, _) => 0,
        };
        self.table_state.select(Some(candidates[next_position]));
        self.sync_auto_expansion();
    }

    fn move_vertical_bounded(&mut self, delta: isize) {
        if self.uses_tree() {
            self.move_tree_vertical(delta, false);
            return;
        }
        let targets = self.visible_targets();
        if targets.is_empty() {
            return;
        }
        let selected = self.selected_visible_index(&targets);
        let candidates = self.vertical_candidates(&targets, selected);
        if candidates.is_empty() {
            return;
        }
        let current = selected
            .and_then(|index| candidates.iter().position(|candidate| *candidate == index))
            .unwrap_or(0);
        let last = candidates.len() - 1;
        let next = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs()).min(last)
        } else {
            current.saturating_add(delta.unsigned_abs()).min(last)
        };
        self.table_state.select(Some(candidates[next]));
        self.sync_auto_expansion();
    }

    fn move_to_vertical_boundary(&mut self, last: bool) {
        if self.uses_tree() {
            let targets = self.tree_targets();
            if !targets.is_empty() {
                let current = self
                    .table_state
                    .selected()
                    .unwrap_or(0)
                    .min(targets.len() - 1);
                let (candidates, _) = self.tree_vertical_navigation(&targets, current);
                let index = if last {
                    *candidates.last().unwrap_or(&current)
                } else {
                    *candidates.first().unwrap_or(&current)
                };
                let key = targets[index].key.clone();
                self.table_state.select(Some(index));
                self.sync_focused_tree_expansion(key);
                self.pin_filtered_tree_selection();
            }
            return;
        }
        let targets = self.visible_targets();
        if targets.is_empty() {
            return;
        }
        let selected = self.selected_visible_index(&targets);
        let candidates = self.vertical_candidates(&targets, selected);
        let next = if last {
            candidates.last()
        } else {
            candidates.first()
        };
        if let Some(index) = next {
            self.table_state.select(Some(*index));
            self.sync_auto_expansion();
        }
    }

    fn selected_visible_index(&self, targets: &[VisibleTarget]) -> Option<usize> {
        self.table_state
            .selected()
            .filter(|index| *index < targets.len())
    }

    fn move_tree_vertical(&mut self, delta: isize, wrap: bool) {
        let targets = self.tree_targets();
        if targets.is_empty() {
            return;
        }
        let current = self
            .table_state
            .selected()
            .unwrap_or(0)
            .min(targets.len() - 1);
        let (candidates, current_position) = self.tree_vertical_navigation(&targets, current);
        if candidates.is_empty() {
            return;
        }
        let step = delta.unsigned_abs();
        let next_position = if wrap {
            if delta.is_negative() {
                (current_position + candidates.len() - (step % candidates.len())) % candidates.len()
            } else {
                (current_position + (step % candidates.len())) % candidates.len()
            }
        } else if delta.is_negative() {
            current_position.saturating_sub(step)
        } else {
            current_position
                .saturating_add(step)
                .min(candidates.len() - 1)
        };
        let next = candidates[next_position];
        let key = targets[next].key.clone();
        self.table_state.select(Some(next));
        self.sync_focused_tree_expansion(key);
        self.pin_filtered_tree_selection();
    }

    /// In the accordion presentation, visible descendants are context rather
    /// than mandatory cursor stops. Vertical movement stays within the
    /// selected node's sibling group; `h`/`l` cross hierarchy levels. A
    /// one-node sibling group bubbles to its parent's siblings so vertical
    /// navigation never gets trapped at a single window or pane. The other
    /// expansion policies retain the conventional visible-row order.
    fn tree_vertical_navigation(
        &self,
        targets: &[TreeTarget],
        current: usize,
    ) -> (Vec<usize>, usize) {
        // The accordion rule — vertical movement stays inside the selected
        // node's sibling group — exists because the tree shows ancestry. The
        // work table shows none: every row is a Work, and rows from different
        // sessions are siblings of nothing, which would strand the cursor on
        // whichever row it started on.
        if self.watch_cfg.layout == WatchLayout::Work
            || self.watch_cfg.tree_expansion != WatchTreeExpansion::Focus
        {
            return ((0..targets.len()).collect(), current);
        }

        let mut anchor = targets[current].key.clone();
        loop {
            let anchor_index = targets
                .iter()
                .position(|target| target.key == anchor)
                .unwrap_or(current);
            let parent = visible_tree_parent_key(&targets[anchor_index]);
            let candidates = targets
                .iter()
                .enumerate()
                .filter_map(|(index, target)| {
                    (visible_tree_parent_key(target) == parent).then_some(index)
                })
                .collect::<Vec<_>>();
            let anchor_position = candidates
                .iter()
                .position(|candidate| *candidate == anchor_index)
                .unwrap_or(0);

            if candidates.len() != 1 {
                return (candidates, anchor_position);
            }
            let Some(parent) = parent else {
                return (candidates, anchor_position);
            };
            anchor = parent;
        }
    }

    fn vertical_candidates(
        &self,
        targets: &[VisibleTarget],
        selected: Option<usize>,
    ) -> Vec<usize> {
        let current = selected.and_then(|index| targets.get(index));
        if self.uses_tree() {
            (0..targets.len()).collect()
        } else {
            match current {
                Some(target) if target.agent_idx.is_some() => targets
                    .iter()
                    .enumerate()
                    .filter_map(|(index, candidate)| {
                        (candidate.row_idx == target.row_idx && candidate.agent_idx.is_some())
                            .then_some(index)
                    })
                    .collect(),
                _ => targets
                    .iter()
                    .enumerate()
                    .filter_map(|(index, target)| target.agent_idx.is_none().then_some(index))
                    .collect(),
            }
        }
    }

    pub(super) fn move_into_work(&mut self) {
        if self.uses_tree() {
            self.expand_or_enter_tree_child();
            return;
        }
        self.sync_auto_expansion();
        let Some(index) = self.table_state.selected() else {
            return;
        };
        let targets = self.visible_targets();
        let Some(current) = targets.get(index) else {
            return;
        };
        if current.agent_idx.is_some() {
            return;
        }
        if targets
            .get(index + 1)
            .is_some_and(|next| next.row_idx == current.row_idx && next.agent_idx.is_some())
        {
            self.table_state.select(Some(index + 1));
            self.sync_auto_expansion();
        }
    }

    pub(super) fn move_to_work_parent(&mut self) {
        if self.uses_tree() {
            self.collapse_or_select_tree_parent();
            return;
        }
        let Some(target) = self.selected_target() else {
            return;
        };
        if target.agent_idx.is_none() {
            return;
        }
        let Some(WatchRow::Work(session)) = self.rows.get(target.row_idx) else {
            return;
        };
        self.restore_selection(Some(RowIdentity::Work(WorkIdentity {
            window_key: session.window_key.clone(),
            fallback: session.group_key.clone(),
        })));
    }

    pub(super) fn expand_or_enter_tree_child(&mut self) {
        let Some(target) = self.selected_tree_target() else {
            return;
        };
        if target.has_children && !self.expanded_nodes.contains(&target.key) {
            self.expanded_nodes.insert(target.key.clone());
            // In focus mode Right/l means "enter": reveal and select the
            // first child in one step. Manual/always retain conventional
            // two-step expand-then-enter tree navigation.
            if self.watch_cfg.tree_expansion != WatchTreeExpansion::Focus {
                if let Some(index) = self
                    .tree_targets()
                    .iter()
                    .position(|candidate| candidate.key == target.key)
                {
                    self.table_state.select(Some(index));
                    self.pin_filtered_tree_selection();
                }
                return;
            }
        }
        let targets = self.tree_targets();
        let Some(index) = targets
            .iter()
            .position(|candidate| candidate.key == target.key)
        else {
            return;
        };
        if targets
            .get(index + 1)
            .is_some_and(|child| child.depth == target.depth + 1)
        {
            let key = targets[index + 1].key.clone();
            self.table_state.select(Some(index + 1));
            self.sync_focused_tree_expansion(key);
            self.pin_filtered_tree_selection();
        }
    }

    pub(super) fn collapse_or_select_tree_parent(&mut self) {
        let Some(target) = self.selected_tree_target() else {
            return;
        };
        let parent = visible_tree_parent_key(&target);

        // Focus mode expands the selected path automatically, so collapsing
        // that same node would be immediately undone on refresh. Treat left
        // as a direct one-level ascent instead.
        if self.watch_cfg.tree_expansion == WatchTreeExpansion::Focus {
            if let Some(parent) = parent {
                self.sync_focused_tree_expansion(parent);
                self.pin_filtered_tree_selection();
            }
            return;
        }

        if target.has_children && self.expanded_nodes.remove(&target.key) {
            if let Some(index) = self
                .tree_targets()
                .iter()
                .position(|candidate| candidate.key == target.key)
            {
                self.table_state.select(Some(index));
                self.pin_filtered_tree_selection();
            }
            return;
        }
        if let Some(parent) = parent {
            if let Some(index) = self
                .tree_targets()
                .iter()
                .position(|candidate| candidate.key == parent)
            {
                self.table_state.select(Some(index));
                self.pin_filtered_tree_selection();
            }
        }
    }

    /// `pane_id` of the currently selected row, if any.
    pub(crate) fn selected_pane(&self) -> Option<String> {
        if self.uses_tree() {
            return self
                .selected_action_pane()
                .map(|pane| pane.key.pane_id.clone());
        }
        target_pane(self, self.selected_target()?)
    }

    pub(super) fn selected_action_pane(&self) -> Option<&PaneNode> {
        match self.selected_node()? {
            TopologyNodeRef::Session(session) => session.active_window()?.active_pane(),
            TopologyNodeRef::Window(window) => window.active_pane(),
            TopologyNodeRef::Pane(pane) => Some(pane),
        }
    }

    pub(super) fn selected_exact_pane(&self) -> Option<&PaneNode> {
        match self.selected_node()? {
            TopologyNodeRef::Pane(pane) => Some(pane),
            TopologyNodeRef::Session(_) | TopologyNodeRef::Window(_) => None,
        }
    }

    pub(super) fn selected_topology_panes(&self) -> Vec<&PaneNode> {
        match self.selected_node() {
            Some(TopologyNodeRef::Session(session)) => session
                .windows
                .iter()
                .flat_map(|window| &window.panes)
                .collect(),
            Some(TopologyNodeRef::Window(window)) => window.panes.iter().collect(),
            Some(TopologyNodeRef::Pane(pane)) => vec![pane],
            None => Vec::new(),
        }
    }

    pub(super) fn selected_target(&self) -> Option<VisibleTarget> {
        let index = self.table_state.selected()?;
        self.visible_targets().get(index).copied()
    }

    /// Borrow the currently-selected row, if any. Used by the quick-
    /// action handlers to decide whether the action is even applicable
    /// (e.g. `K` only applies to `WatchRow::Agent` rows with a
    /// non-`None` pane).
    pub(crate) fn selected_row(&self) -> Option<&WatchRow> {
        if self.uses_tree() {
            return None;
        }
        let target = self.selected_target()?;
        self.rows.get(target.row_idx)
    }

    pub(super) fn selected_agent(&self) -> Option<&Agent> {
        if self.uses_tree() {
            return self.selected_exact_pane()?.agent.as_ref();
        }
        let target = self.selected_target()?;
        match self.rows.get(target.row_idx)? {
            WatchRow::Agent(agent) => Some(agent),
            WatchRow::Work(session) => target
                .agent_idx
                .and_then(|idx| session.agents.get(idx))
                .or(session.latest_agent.as_ref()),
            WatchRow::BarePane(_) => None,
        }
    }

    /// `last_prompt` for the selected agent row, if it has one and
    /// the row is an `Agent` (bare panes have no prompt). Threaded
    /// through `c` to populate `QuickAction::CopyPrompt`.
    pub(crate) fn selected_last_prompt(&self) -> Option<&str> {
        self.selected_agent()?.last_prompt.as_deref()
    }

    pub(super) fn visible_targets(&self) -> Vec<VisibleTarget> {
        let query = self.search_query.trim().to_lowercase();
        let mut targets = Vec::new();
        for (row_idx, row) in self.rows.iter().enumerate() {
            let row_query_match = query.is_empty() || row_matches_query(row, &self.panes, &query);
            let row_attention_match = !self.attention_only || row_needs_attention(row);
            if row_query_match && row_attention_match {
                targets.push(VisibleTarget {
                    row_idx,
                    agent_idx: None,
                });
            }

            let WatchRow::Work(session) = row else {
                continue;
            };
            if self.watch_cfg.layout == WatchLayout::Swarm
                || session.pane_count <= 1
                || !self.expanded_works.contains(&session.group_key)
            {
                continue;
            }
            let session_name_match = query.is_empty()
                || contains_ci(&session.display_name, &query)
                || contains_ci(&session.session, &query);
            for (agent_idx, agent) in session.agents.iter().enumerate() {
                let query_match =
                    session_name_match || agent_matches_query(agent, &self.panes, &query);
                let attention_match = !self.attention_only || agent_needs_attention(agent.state);
                if query_match && attention_match {
                    targets.push(VisibleTarget {
                        row_idx,
                        agent_idx: Some(agent_idx),
                    });
                }
            }
        }
        targets
    }

    pub(super) fn edit_search(&mut self, edit: impl FnOnce(&mut String)) {
        let was_filtering = self.topology_filtering();
        if self.uses_tree() && !was_filtering {
            self.filtered_selection_anchor = self.selected_node_key();
        }
        edit(&mut self.search_query);
        let is_filtering = self.topology_filtering();
        let selected = if self.uses_tree() {
            self.filtered_selection_anchor
                .clone()
                .map(RowIdentity::Topology)
                .or_else(|| self.selected_identity())
        } else {
            self.selected_identity()
        };
        self.restore_selection(selected);
        if self.uses_tree() && !is_filtering {
            self.filtered_selection_anchor = None;
        }
    }

    pub(super) fn browse_keys_active(&self) -> bool {
        self.search_query.is_empty() && !self.explicit_search
    }

    pub(super) fn arm_explicit_search(&mut self) {
        self.explicit_search = true;
        self.pending_g = false;
    }

    pub(super) fn clear_search(&mut self) {
        self.explicit_search = false;
        self.edit_search(String::clear);
    }

    pub(super) fn delete_search_word(&mut self) {
        self.edit_search(|query| {
            while query.chars().last().is_some_and(char::is_whitespace) {
                query.pop();
            }
            while query.chars().last().is_some_and(|c| !c.is_whitespace()) {
                query.pop();
            }
        });
    }

    pub(super) fn toggle_attention_only(&mut self) {
        let was_filtering = self.topology_filtering();
        if self.uses_tree() && !was_filtering {
            self.filtered_selection_anchor = self.selected_node_key();
        }
        self.attention_only = !self.attention_only;
        let is_filtering = self.topology_filtering();
        let selected = if self.uses_tree() {
            self.filtered_selection_anchor
                .clone()
                .map(RowIdentity::Topology)
                .or_else(|| self.selected_identity())
        } else {
            self.selected_identity()
        };
        self.restore_selection(selected);
        if self.uses_tree() && !is_filtering {
            self.filtered_selection_anchor = None;
        }
    }

    pub(super) fn toggle_event_inbox(&mut self) {
        self.event_inbox_open = !self.event_inbox_open;
        if self.event_inbox_open {
            self.unread_events = 0;
        }
    }

    /// Toggle the wide-screen inspector, and say which way it went.
    ///
    /// The hint carries real information here. The inspector is on by
    /// default, so the first `Alt-I` *hides* a panel rather than
    /// summoning one — and on a terminal under 120 columns nothing
    /// visibly changes either way, because the panel had no room to
    /// render. Both cases read as a dead key without a word from the
    /// footer, which is how a working binding gets reported as broken.
    pub(super) fn toggle_inspector(&mut self) {
        self.inspector_enabled = !self.inspector_enabled;
        self.inspector_visible = false;
        self.pane_capture = None;
        self.set_hint(
            if self.inspector_enabled {
                "inspector enabled"
            } else {
                "inspector disabled"
            },
            HintLevel::Ok,
        );
    }

    /// Pre-format the pane label the way the user would read it in the
    /// table — `session:window.pane` when resolvable, raw `%id`
    /// otherwise. Lives on `App` because the panes inventory is here;
    /// `dispatch_quick_action` doesn't have access to it.
    pub(crate) fn pane_label(&self, pane_id: &str) -> String {
        pane_display(Some(pane_id), &self.panes)
    }

    /// Stamp a transient footer hint from the result of a quick action.
    /// The renderer reads this back and hides the default keybinding
    /// strip while the hint is fresh — see `FOOTER_HINT_TTL`.
    pub(crate) fn set_hint(&mut self, message: impl Into<String>, level: HintLevel) {
        self.footer_hint = Some(FooterHint {
            message: message.into(),
            level,
            set_at: Instant::now(),
        });
    }
}
