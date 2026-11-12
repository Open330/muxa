//! `tend watch` — fullscreen ratatui dashboard.
//!
//! Polls the daemon via `Client::snapshot()` every 500 ms and renders a
//! live-updating table of tracked agents. Input is handled via crossterm
//! events:
//!
//! - Filtering: `/` arms the filter and printable characters narrow the row
//!   set from there. A bare letter is always a binding, never the first
//!   character of a query. Backspace edits and Esc clears before quitting.
//! - Navigation: arrows always move between sessions. While the filter is
//!   disarmed, `hjkl`, `gg` / `G`, Home / End, page keys, and Ctrl-U / Ctrl-D
//!   add conventional TUI navigation. Enter jumps through the selected node's
//!   active descendants; `m` owns prompt/message composition.
//! - Inspection: `Alt-P` opens preview, `Alt-I` toggles the responsive split
//!   inspector, and `Alt-E` opens the persistent transition inbox.
//! - Destructive and sort actions retain Alt chords (`Alt-K`, `Alt-X`, …),
//!   while common browse actions also have `o`, `r`, `?`, and `:` aliases.
//! - Discovery: F1 / `Alt-?` toggles the complete binding reference.
//!
//! Terminal lifecycle is managed by a RAII `TerminalGuard` so raw mode and
//! the alternate screen are always restored — even on panic.

pub(crate) mod sidebar;

use std::cmp::Ordering;
use std::future::Future;
use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, HighlightSpacing, Padding, Paragraph, Row, Table,
    TableState, Wrap,
};
use ratatui::{Frame, Terminal};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use tend::collaboration::{
    CollaborationOrigin, CollaborationRequest, MailboxScope, NewRequest, Participant, RequestKind,
    RequestMailbox, RequestStatus, RoomContext, WorkMode,
};
use tend::config::{
    IconSet, WatchCollaborationMode, WatchConfig, WatchLayout, WatchScreen, WatchSortKey,
    WatchSummary, WatchTheme, WatchTreeExpansion, WatchView, WidthSpec,
};
use tend::event::RateLimitScope;
use tend::ipc::{Client, RuntimeError, VersionSkew};
use tend::process_tree::WorkloadProcessKind;
use tend::session_activity::SessionActivity;
use tend::state::Agent;
use tend::tmux::layout::PaneGeometry;
use tend::tmux::{PaneInfo, SessionInfo};
use tend::{
    ActivityEntry, BackendEndpoint, HumanInteractionEntry, HumanInteractionInput,
    HumanInteractionKind, PaneKey, PaneNode, SessionNode, StateDistribution, TopologyInput,
    TopologyNodeKey, TopologyNodeRef, TopologySnapshot, WindowKey, WindowNode,
};
use tend::{AgentKind, AgentState, SessionKey};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use crate::board_screen::{self, BoardScreen};
pub use crate::collab_screen::CollabLayout;
use crate::collab_screen::{self, CollabRow, CollabScreen, SequenceRow};
use crate::message_skill::{
    insert_prompt as insert_message_skill_prompt, matching_skills, remove as remove_message_skill,
    upsert as upsert_message_skill, validate_name as validate_message_skill_name,
    Palette as MessageSkillPalette,
};

/// Polling cadence when no streaming `Subscribe` is active. We still
/// fall back to this if the daemon doesn't speak the streaming
/// variant or the subscription drops mid-session.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Topology catch-up cadence while registry updates arrive through the
/// coalesced invalidation stream. Old daemons use `POLL_INTERVAL` instead.
const STREAMING_FALLBACK_INTERVAL: Duration = Duration::from_secs(5);

/// Max time to wait for a single keystroke when the input buffer is
/// empty. ~60 Hz so a press feels immediate without burning CPU on an
/// idle terminal. Held keys / fast typing are absorbed by the
/// drain-all-pending pattern in `run`, so this only governs idle
/// responsiveness.
const INPUT_POLL: Duration = Duration::from_millis(16);

/// Idle repaint cadence. The render loop normally repaints only when
/// something changed (input, a refresh outcome, a preview recapture). This
/// floor keeps time-derived cells fresh anyway — the Activity column renders
/// `relative_time` at second granularity — without the old behaviour of one
/// full render per `INPUT_POLL` tick (~62 fps) even on a completely idle UI.
const IDLE_REDRAW_INTERVAL: Duration = Duration::from_secs(1);
/// Multi-pane window captures are more expensive than the selected-pane
/// preview. One snapshot per second keeps the layout alive without launching
/// one `capture-pane` subprocess per pane on every animation frame.
const WINDOW_CAPTURE_INTERVAL: Duration = Duration::from_secs(1);
const WINDOW_CAPTURE_PARALLELISM: usize = 8;
/// Below these dimensions a tiled terminal preview is less legible than the
/// pane roster it replaces.
const WINDOW_MOSAIC_MIN_WIDTH: u16 = 42;
const WINDOW_MOSAIC_MIN_HEIGHT: u16 = 6;
/// Faster idle repaint cadence for the swarm view so its dot spinners
/// animate smoothly (~8 fps) even with no input.
const SWARM_REDRAW_INTERVAL: Duration = Duration::from_millis(120);
/// Tree rows need motion to distinguish active states, but 8 fps made large
/// Inspector frames needlessly expensive. Four frames per second stays visibly
/// animated while halving steady-state topology and terminal rendering work.
const TREE_REDRAW_INTERVAL: Duration = Duration::from_millis(250);

/// Parse the collaboration-only presentation axis. Kept public so the CLI
/// argument/config layer can seed watch without adding sequence variants to
/// the unrelated topology [`WatchLayout`] enum.
pub fn parse_collab_layout(value: &str) -> Option<CollabLayout> {
    collab_screen::parse_layout(value)
}

/// How long a transient action hint stays visible in the footer before
/// the renderer falls back to the default keybinding strip. 2 s is the
/// sweet spot from the spec: long enough to catch with a glance after
/// hitting `K`/`R`/`c`, short enough not to mask the next interaction.
const FOOTER_HINT_TTL: Duration = Duration::from_secs(2);

/// Channel capacity for the wake signal sent from the input loop to the
/// background refresh task. Capacity 1 is intentional: when the user mashes
/// `r`, we want extra requests to coalesce into a single pending wake rather
/// than queue up.
const WAKE_CAPACITY: usize = 1;
/// Channel capacity for refresh outcomes flowing from the background task to
/// the main loop. 2 is just enough to absorb a tick that lands while the
/// main task is mid-render without stalling the refresh task; the main loop
/// always drains all pending outcomes before each render.
const OUTCOME_CAPACITY: usize = 2;

mod app;
mod background;
mod board_actions;
mod collaboration;
mod collaboration_model;
mod effects;
mod footer;
mod form_input;
mod forms;
mod input;
mod inspector;
mod model;
mod preview;
mod preview_input;
mod render;
mod render_board;
mod render_forms;
mod render_mailbox;
mod render_table;
mod render_tree;
mod rows;
mod runtime;
mod theme;
mod topology;

use app::{capture_tmux_window, char_to_byte_idx, pane_capture_target};
use background::{
    board_window, is_many_pane, read_board_open_requests, refresh_board_collaboration, refuse_busy,
    start_quick_in_background, take_finished_quick, BoardMailboxCache, BoardScan,
    BOARD_COLLAB_INTERVAL, BOARD_STATS_INTERVAL, BOARD_WORKERS_INTERVAL, BOARD_WORKER_IDLE_SECS,
};
use board_actions::{
    board_abort_action, board_attach_action, board_claim_action, board_collaboration_unsupported,
    board_copy_action, board_cursor_agent, board_kill_action, board_mailbox_anchor,
    board_message_action, board_message_stale, board_pending_matches, board_prompt_action,
    board_selected_pane, busy_target, collab_attach_action, finish_board_message, open_preview_for,
    still_sending, BoardPending,
};
use collaboration::{
    active_ask_conversation, append_human_interaction, ask_revision_updates,
    clamp_collaboration_mailbox, collaboration_open_hint, collaboration_requests,
    collaboration_revision_updates, detail_for, is_ask_running, move_collaboration_mailbox,
    open_watch_collaboration_composer, open_watch_collaboration_mailbox,
    participant_matches_window, refresh_ask_entries, refresh_fleet_collaboration,
    refresh_selected_collaboration_detail, refresh_watch_collaboration, reply_status_badge,
    request_kind_badge, request_kind_label, run_watch_collaboration_composer, scan_board,
    selected_collaboration_request, selected_move_target, short_collaboration_request_id,
    sync_collab_screen_from_cache, toggle_collaboration_mailbox, toggle_collaboration_mark,
    visible_ask_entries, work_mode_badge, work_mode_label, BOARD_REFRESH_INTERVAL,
};
use collaboration_model::{
    history_for_scope, BroadcastReport, BroadcastRow, CollaborationComposeDefaults,
    CollaborationComposeTarget, CollaborationComposer, CollaborationMailboxState,
    CollaborationMailboxTab, CollaborationMark, ComposeSendMode, MailboxAnchor, MailboxDetail,
    MailboxHistoryScope, WatchCollaboration, WatchHistoryCache,
};
use effects::{create_window, pane_target_label, WINDOWS_GET_CLIPBOARD};
use footer::render_footer;
use form_input::{
    clamp_active_message_skill, focus_active_message_skill, handle_ask_composer_event,
    handle_ask_panel_event, handle_collaboration_composer_event,
    handle_collaboration_mailbox_event, handle_message_skill_editor_event, handle_rename_event,
    handle_spawn_event, handle_work_composer_event, is_session_target,
};
use forms::{
    insert_str_at, persist_watch_collab_layout, persist_watch_collaboration_defaults,
    persist_watch_inspector_split, persist_watch_value, session_base_name, system_clipboard_text,
    wrap_input_rows, AskComposer, AskPanelState, AskReader, InspectorSplit, MessageSkillEditor,
    MessageSkillEditorField, RenameComposer, RenameLevel, SpawnAgent, SpawnComposer, SpawnField,
    WorkComposer,
};
use input::{
    apply_rename, board_quick_panes, command_suggestions, drain_pending_events, finish_rename,
    handle_event, interaction_session, open_work_up_window,
};
use inspector::{render_topology_inspector, unambiguous_session_activity_secs};
use model::{
    persist_watch_sort, probe_version_skew, restart_daemon_from_watch, sort_label, InitialPaneHint,
    TreeTarget, VisibleTarget, WatchEventEntry, WatchEventKind, MAX_WATCH_EVENTS,
};
use preview::{build_pane_capture_body, build_pane_capture_body_on, render_preview};
use preview_input::{handle_preview_event, preview_label, preview_position};
use render::{
    centered_rect, message_composer_rect, truncate_prompt_input, VisiblePromptInput,
    COLLAB_DETAIL_HEIGHT,
};
use render_board::{render_board_screen, render_collab_screen};
use render_forms::{
    ask_composer_starts_new_conversation, render_ask_composer, render_ask_overlay,
    render_broadcast_report_overlay, render_collaboration_composer, render_message_skill_editor,
    render_rename_overlay, render_spawn, render_work_composer_overlay,
};
use render_mailbox::{bottom_prompt_rect, render_collaboration_mailbox, wrap_detail_text};
use render_table::{
    agent_workload_badge, limits_text, onboarding_empty, relative_time, render_empty_table,
    render_table, summary_line, truncate_chars, work_workload_text, workload_text,
};
use render_tree::{
    icons_unicode, pipeline_status_label, render_swarm, render_tree_table, render_work_table,
    tree_node_label, tree_node_states, SUMMARY_MIN_TABLE_WIDTH, SWARM_DOTS, SWARM_START,
};
use rows::{
    agent_pane_display, duration_text, format_duration, pane_display, plural, prepend_host_badge,
    prepend_tree_prefix, pulse_cell, pulse_glyph_style, pulse_kind, resolve_row_pulses,
    rows_have_active_spinner, rows_multi_host, session_state_age_text, sort_panes, sort_works,
    state_age_label, state_age_text, state_marker, state_summary_gutter_spans, state_summary_spans,
    work_label, Pulse, PulseKind, Spinner, PULSE_WINDOW,
};
use runtime::{compute_refresh, refresh_task, request_refresh, setup_terminal, TerminalGuard};
use theme::{resolve_display_columns, resolve_width};
use topology::{
    agent_matches_query, agent_needs_attention, annotate_work_rows, build_watch_topology,
    build_work_rows, compare_topology_siblings, contains_ci, default_tree_expanded,
    pane_work_display_name, pane_work_group_key, row_host, row_matches_query, row_needs_attention,
    selected_tree_target_from, session_group_key, sort_agents, state_sort_rank, target_pane,
    topology_key_label, topology_keys, tree_pane_relevant, tree_session_relevant,
    tree_window_relevant, visible_tree_parent_key, SortContext, TopologySortFacts,
};

pub(crate) use app::App;
pub use app::CapturedPane;
pub use app::CapturedWindow;
pub use app::CapturedWindowPane;
pub(crate) use app::CommandPalette;
pub(crate) use app::HintLevel;
pub use app::PreviewContent;
pub use app::PreviewMode;
pub use app::PreviewState;
pub(crate) use background::BoardCollab;
pub(crate) use background::BoardRefresher;
pub(crate) use background::BoardStats;
pub(crate) use background::BoardWorkers;
pub(crate) use collaboration::watch_collaboration_origin;
pub(crate) use effects::apply_outcome_to_app;
pub(crate) use effects::dispatch_quick_action;
pub(crate) use effects::help_overlay_text;
pub(crate) use effects::ActionOutcome;
pub(crate) use effects::ConfirmAction;
pub(crate) use effects::QuickAction;
pub(crate) use effects::RealEffects;
pub(crate) use forms::ConfirmPopup;
pub(crate) use input::Action;
pub use input::WatchOpenTarget;
pub(crate) use model::skew_banner;
pub(crate) use model::DaemonError;
pub(crate) use model::RowIdentity;
pub(crate) use model::WatchRow;
pub(crate) use model::WatchSortPreset;
pub(crate) use model::WorkIdentity;
pub(crate) use model::WorkRow;
pub(crate) use preview_input::quick_abort_action;
pub(crate) use preview_input::quick_copy_action;
pub(crate) use preview_input::quick_kill_action;
pub(crate) use preview_input::quick_prompt_action;
pub(crate) use render::render;
pub(crate) use render_tree::agent_kind_short;
pub(crate) use runtime::apply_outcome;
pub use runtime::run;
pub(crate) use runtime::MessageComposerConfig;
pub(crate) use runtime::RefreshOutcome;
pub(crate) use theme::watch_theme;
pub(crate) use theme::WatchColumn;
pub(crate) use theme::WatchThemeSpec;

#[cfg(test)]
mod tests;

#[cfg(test)]
use effects::{many_pane_outcome, parse_new_window_target, plan_new_window, Effects};

#[cfg(test)]
use forms::{shell_single_quote, AskFilter};

#[cfg(test)]
use app::content_window_geometry;

#[cfg(test)]
use topology::{distribution_rank, group_attached_clients};

#[cfg(test)]
use rows::WORK_STATE_GUTTER_WIDTH;

#[cfg(test)]
use runtime::{
    apply_single_agent, initialize_watch_collaboration, merge_agent_for_ui, FullRefresh,
};

#[cfg(test)]
use collaboration::{
    empty_room_hint, mailbox_history_scope, open_prompt_only_composer, peer_choice_hint,
    read_human_requests, read_watch_history, reconcile_search_history, resolved_marks,
    stale_mark_count, watch_collaboration_origin_from, watch_history,
};

#[cfg(test)]
use background::start_quick_with;

#[cfg(test)]
use input::{execute_palette_command, gate_board_action, rename_label, window_name_taken};

#[cfg(test)]
use form_input::{
    composer_cycle_mode, composer_submit_action, handle_message_skill_palette_event,
    open_watch_collaboration_reply_composer, spawn_edit_text,
};

#[cfg(test)]
use preview_input::{preview_target_position, work_preview_targets};

#[cfg(test)]
use render_forms::{ask_composer_title, collaboration_composer_title};

#[cfg(test)]
use render_mailbox::collaboration_mailbox_request_lines;

#[cfg(test)]
use render_board::fit_corner_title;

#[cfg(test)]
use preview::build_preview_lines;

#[cfg(test)]
use inspector::{inspector_agent_summary, inspector_pane_roster_line, scaled_window_pane_rect};

#[cfg(test)]
use render_tree::{
    topology_table_columns, tree_state_cell, tree_state_is_redundant, window_node_summary,
    TopologyTableColumn,
};

#[cfg(test)]
use render_table::{detail_host_column, effective_columns, format_detail, workload_detail_string};
