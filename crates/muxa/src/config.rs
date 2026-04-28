//! Configuration model.
//!
//! Loaded from TOML. CLI/env-var overrides happen at the binary layer — this
//! module only parses.

use crate::error::{CoreError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Unix socket path. Overrides the XDG default.
    pub socket: Option<PathBuf>,

    pub notifier: NotifierConfig,
    pub watch: WatchConfig,
    pub dashboard: DashboardTomlConfig,
    pub discovery: DiscoveryConfig,
    pub reconciler: ReconcilerConfig,
    pub history: HistoryConfig,
    pub state: StateConfig,
    pub sinks: SinksConfig,
}

/// `[sinks]` config — opt-in fan-out to external systems.
///
/// Each sub-table corresponds to one sink implementation. All sinks are
/// off by default; a missing table is equivalent to one with
/// `enabled = false`. Resolution to runtime sink instances happens in the
/// daemon, not here — this struct only holds raw TOML.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SinksConfig {
    pub oh_my_prompt: OhMyPromptToml,
}

/// `[sinks.oh_my_prompt]` raw TOML schema. The daemon resolves these
/// fields against env vars + defaults via `OhMyPromptSink::resolve` at
/// startup.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OhMyPromptToml {
    pub enabled: Option<bool>,
    /// Base URL of the omp ingestion endpoint, e.g. `https://prompt.example`.
    /// `enabled = true` with no endpoint is a config error — there is no
    /// default endpoint by design.
    pub endpoint: Option<String>,
    /// Name of the env var holding the X-User-Token UUID. Defaults to
    /// `OMP_SERVER_TOKEN`. The token never lives in TOML.
    pub token_env: Option<String>,
    /// Optional device identifier echoed in the upload payload.
    pub device_id: Option<String>,
    /// Records per HTTP batch. Defaults to 50.
    pub batch_size: Option<usize>,
    /// Time-based flush interval (ms). Defaults to 5000.
    pub flush_interval_ms: Option<u64>,
}

/// `[dashboard]` config — the user-facing TOML schema for the dashboard
/// HTTP server. All fields are `Option` so the config-file layer can
/// distinguish "not set" (use default or env/flag override) from
/// "explicitly set". The fully-resolved [`DashboardConfig`](crate::dashboard::DashboardConfig)
/// lives in the dashboard module and is computed by
/// `DashboardConfig::resolve` at startup.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DashboardTomlConfig {
    pub enabled: Option<bool>,
    /// Socket address as `ip:port`. Default `127.0.0.1:7878`.
    pub bind: Option<String>,
    /// Bearer token. Empty string is treated as "unset". Required when
    /// `bind` is non-loopback.
    pub token: Option<String>,
    /// Required to be `true` for non-loopback `bind` values. Acts as an
    /// explicit acknowledgement that the operator means to expose the
    /// dashboard beyond the local machine.
    pub allow_public: Option<bool>,
    /// Pane scanner cache TTL in milliseconds. Default 2000.
    pub pane_cache_ttl_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotifierConfig {
    pub enabled: bool,
    pub backend: NotifierBackend,
}

impl Default for NotifierConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: NotifierBackend::None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifierBackend {
    None,
    Libnotify,
}

/// `[discovery]` config — controls the tmux-pane backfill scan.
///
/// When enabled, `muxad` runs a single discovery pass shortly after binding
/// its IPC socket and the `muxa sync` CLI uses the same routine on demand.
/// Discovery synthesizes `Started` events for any pane whose
/// `pane_current_command` matches a known agent CLI, populating the registry
/// without waiting for a real hook to fire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiscoveryConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// `[reconciler]` config — controls the periodic control loop that
/// converges the in-memory agent registry against tmux ground truth.
///
/// The reconciler runs in the daemon and uses tmux as the authoritative
/// source for which panes exist. Each pass reaps stale records, drops
/// synthetic placeholders that have been superseded by real entries, and
/// collapses duplicate rows for the same pane. It's idempotent — the
/// `interval_secs` knob is a tuning parameter, not a correctness one.
///
/// Disable only if you're driving reconciliation externally (e.g. an
/// integration test plugging in a fake `LivenessSource`); the daemon's
/// view of the world will rot otherwise.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReconcilerConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Cadence of reconciliation passes. Defaults to 30s — frequent enough
    /// that closed panes disappear from `muxa watch` within seconds, slow
    /// enough that the cost of shelling out to tmux is negligible.
    #[serde(default = "default_reconciler_interval_secs")]
    pub interval_secs: u64,
}

impl Default for ReconcilerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: default_reconciler_interval_secs(),
        }
    }
}

fn default_reconciler_interval_secs() -> u64 {
    30
}

/// `[history]` config — controls the disk-backed prompt audit log.
///
/// The daemon records every `PromptSubmitted` event in a bounded NDJSON
/// file plus an in-memory ring per pane. This is what powers `muxa recap
/// --all` even after the live agent record has been reaped.
///
/// Disable only if you're routing history exclusively through a sink
/// (e.g. oh-my-prompt) — otherwise you lose the ability to look back at
/// old prompts after a daemon restart or pane close.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HistoryConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Override the default `$XDG_DATA_HOME/muxa/prompts.ndjson` path.
    /// Useful for tests and for users who want history on a different
    /// filesystem (e.g. tmpfs for ephemeral, NFS for cross-machine).
    pub path: Option<PathBuf>,
    /// Cap on entries kept per pane in memory and on disk. Tuned to
    /// "comfortable for `recap` browsing" — bump if you want longer
    /// per-pane history.
    #[serde(default = "default_history_max_per_pane")]
    pub max_per_pane: usize,
    /// Compaction drops entries older than this. Defaults to 30 days,
    /// roughly one development sprint of context.
    #[serde(default = "default_history_max_age_days")]
    pub max_age_days: u32,
    /// How often the compaction task rewrites the file. The compaction
    /// pass is cheap (rewrites a few hundred lines), but doing it more
    /// often than necessary only burns disk I/O.
    #[serde(default = "default_history_compact_interval_secs")]
    pub compact_interval_secs: u64,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: None,
            max_per_pane: default_history_max_per_pane(),
            max_age_days: default_history_max_age_days(),
            compact_interval_secs: default_history_compact_interval_secs(),
        }
    }
}

/// `[state]` config — controls the agent-registry snapshot file.
///
/// The daemon mirrors its in-memory `agents` map to a single JSON file so a
/// restart can rehydrate every tracked session — real `session_id`,
/// `last_prompt`, `last_response`, `state`, model/cost metadata — instead of
/// falling back to discovery's `synthetic-%X` placeholders. Discovery still
/// runs on top of this for panes that started while the daemon was down.
///
/// Writes are event-driven (a `tokio::sync::Notify` from `Store::apply` wakes
/// a writer task) and debounced so bursts of events coalesce into a single
/// disk write. Idle steady-state produces zero I/O.
///
/// Disable only if you'd rather lose state on every restart — the file is
/// small (tens of KB) and the writes are already off the hot path.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StateConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Override the default `$XDG_DATA_HOME/muxa/state.json` path.
    pub path: Option<PathBuf>,
    /// Time to wait after a notify before snapshotting, so a burst of
    /// events (e.g. a tool-heavy turn firing `PromptSubmitted` →
    /// `ToolStarted` → `ToolCompleted` → `TurnStopped` within ms)
    /// coalesces into one write. Default 200ms — small enough to feel
    /// instant on a kill-and-restart, large enough to absorb most bursts.
    #[serde(default = "default_state_debounce_ms")]
    pub debounce_ms: u64,
}

impl Default for StateConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: None,
            debounce_ms: default_state_debounce_ms(),
        }
    }
}

fn default_state_debounce_ms() -> u64 {
    200
}

fn default_history_max_per_pane() -> usize {
    50
}
fn default_history_max_age_days() -> u32 {
    30
}
fn default_history_compact_interval_secs() -> u64 {
    3600
}

fn default_true() -> bool {
    true
}

impl Config {
    /// Load a config from the given file. Missing file is an error — use
    /// `load_or_default` if you want silent fallback.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let cfg: Self = toml::from_str(&text).map_err(|e| CoreError::ConfigParse {
            path: path.to_path_buf(),
            source: e,
        })?;
        Ok(cfg)
    }

    /// Load from the given path if it exists, otherwise return defaults.
    pub fn load_or_default(path: Option<&Path>) -> Result<Self> {
        if let Some(p) = path {
            if p.exists() {
                return Self::load(p);
            }
        }
        Ok(Self::default())
    }
}

/// `[watch]` config — controls the `muxa watch` TUI columns.
///
/// Validation of column keys and width specs happens lazily at render time
/// (in the watch crate) so that an unknown key warns rather than refuses to
/// start. See `watch::WatchColumn::from_key` for the canonical key list.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WatchConfig {
    /// Columns to display, in order. Omitted keys are hidden.
    pub columns: Vec<String>,
    /// Per-column width override. Keys are column keys; values are either
    /// a TOML integer (fixed length) or a string of the form `min:N` /
    /// `pct:N`. Missing keys fall back to the column's built-in default.
    pub widths: HashMap<String, WidthSpec>,
    /// Expanded detail line shown under the currently-selected row.
    pub detail: DetailConfig,
    /// Ordered list of sort keys applied to the agent rows in `muxa watch`.
    /// Stale agents (pane closed) always bucket at the end, regardless of
    /// what's listed here.
    ///
    /// Default: `["session", "activity"]` — groups by tmux session, then
    /// floats the most-recently-active agent inside each group to the top.
    pub sort: Vec<WatchSortKey>,
    /// Hide agents that aren't bound to a tmux pane.
    ///
    /// Paneless agents (Claude SDK sub-processes whose env didn't carry
    /// `$TMUX_PANE`, agents launched outside tmux, etc.) can't be attached
    /// to from the picker — Enter is a no-op — so they default to hidden
    /// to keep the row list focused on actionable targets. The footer
    /// surfaces a `+N paneless` count so they remain discoverable, and
    /// `muxa watch --include-paneless` reveals them for one invocation.
    /// Set `false` here to flip the default the other way.
    #[serde(default = "default_true")]
    pub hide_paneless: bool,
    /// Behaviour of the `p` preview overlay. See [`PreviewConfig`].
    pub preview: PreviewConfig,
}

/// `[watch.preview]` — controls the preview overlay opened with `p`.
///
/// Geometry (popup vs fullscreen) is keyed off `f`; what's *inside*
/// the overlay is keyed off `c`. This struct configures only the
/// initial content; both keys still toggle in either direction at
/// runtime regardless of the default.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PreviewConfig {
    /// What to show on first open. Defaults to `live_pane` so the
    /// overlay matches tmux's `prefix + s` choose-tree shape — a live
    /// snapshot of the actual pane contents — rather than the agent's
    /// stored prompt/response text. Set to `prompt_response` to revert
    /// to the previous text-only default.
    pub default_content: PreviewContent,
}

/// Content axis of the `muxa watch` preview overlay. Persisted in TOML
/// for [`PreviewConfig::default_content`] and consumed at runtime by
/// the watch crate when constructing a fresh `PreviewState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewContent {
    /// Render the agent's last prompt + last response from the
    /// in-memory store. Cheap (zero shell-out), text-only.
    PromptResponse,
    /// Live snapshot of the tmux pane's visible screen, captured via
    /// `tmux capture-pane -ep` on each refresh tick. Preserves ANSI
    /// colors — same shape as tmux's choose-tree preview.
    LivePane,
}

impl Default for PreviewConfig {
    fn default() -> Self {
        Self {
            default_content: PreviewContent::LivePane,
        }
    }
}

/// A single sort key for the `muxa watch` agent row ordering. The keys
/// are evaluated in the order listed in `WatchConfig::sort` and the first
/// non-equal comparison wins, with `pane_id` as a final stable tiebreaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchSortKey {
    /// tmux session name, ascending. Resolved against the panes inventory
    /// because agent records only carry `pane_id`.
    Session,
    /// `last_activity_at` descending — most recently updated first. The
    /// useful default to surface "what's moving right now" without
    /// scrolling.
    Activity,
    /// tmux window index, then pane index, both ascending and parsed
    /// numerically so `10` sorts after `2`. Combined with `Session`, this
    /// reproduces the tmux-native pane ordering.
    Pane,
    /// Raw `pane_id` (e.g. `%42`) lexicographic ascending. Mostly useful
    /// as a stable, predictable order for screenshots / docs.
    PaneId,
}

impl Default for WatchConfig {
    fn default() -> Self {
        // Prompt-forward defaults: lead with the last prompt. Users who
        // care about model/ctx/cost can opt back in via config.
        let columns = vec![
            "pane".to_string(),
            "state".to_string(),
            "prompt".to_string(),
            "activity".to_string(),
        ];
        let mut widths = HashMap::new();
        widths.insert("pane".to_string(), WidthSpec::Length(22));
        widths.insert("state".to_string(), WidthSpec::Length(14));
        widths.insert("prompt".to_string(), WidthSpec::Min(30));
        widths.insert("activity".to_string(), WidthSpec::Length(10));
        Self {
            columns,
            widths,
            detail: DetailConfig::default(),
            // Group by session, then bring the most recently active agent
            // in each group to the top — covers both "what's moving" and
            // "where is it" at a glance, without losing the grouping
            // shipped in c9a6572.
            sort: vec![WatchSortKey::Session, WatchSortKey::Activity],
            hide_paneless: true,
            preview: PreviewConfig::default(),
        }
    }
}

/// `[watch.detail]` — the second visual line rendered under the selected
/// row. Useful for glancing at the full last-prompt (or any other field)
/// without leaving the picker.
///
/// `template` is interpolated with `{name}` placeholders. Supported names:
/// `pane`, `kind`, `state`, `model`, `ctx`, `cost`, `activity`,
/// `last_prompt`, `last_response`, `last_notification`, `cwd`. Unknown placeholders are
/// preserved verbatim.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DetailConfig {
    pub enabled: bool,
    pub template: String,
}

impl Default for DetailConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            // Prefer the assistant's last response (the PROMPT column
            // already surfaces the user's last prompt), but fall back to
            // `last_prompt` when no response has been captured yet — older
            // agents that pre-date transcript tailing, agents mid-turn,
            // or hooks that fire `PromptSubmitted` without ever reaching
            // `TurnStopped`. Without the fallback the detail row vanishes
            // entirely in those cases, which reads as "the feature is
            // broken" rather than "no response yet".
            //
            // Pipe-separated alternatives in `format_detail` resolve
            // left-to-right and pick the first non-dash value. Users who
            // want both visible can override with e.g.
            // `template = "{last_prompt} → {last_response}"`.
            template: "{last_response|last_prompt}".to_string(),
        }
    }
}

/// One column's width directive. Mirrors a subset of
/// `ratatui::layout::Constraint`, but lives in `muxa` so the daemon's
/// config schema doesn't have to depend on the TUI crate.
///
/// TOML representations:
///
/// - integer `22`        -> `Length(22)`
/// - string `"min:30"`   -> `Min(30)`
/// - string `"pct:25"`   -> `Percentage(25)` (clamped 0..=100 on parse)
///
/// Unrecognized strings deserialize successfully as `Invalid(_)` so the
/// watch crate can warn and fall back to the column default rather than
/// failing to load the whole config.
#[derive(Debug, Clone)]
pub enum WidthSpec {
    Length(u16),
    Min(u16),
    Percentage(u16),
    Invalid(String),
}

impl Serialize for WidthSpec {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Self::Length(n) => s.serialize_u16(*n),
            Self::Min(n) => s.serialize_str(&format!("min:{n}")),
            Self::Percentage(n) => s.serialize_str(&format!("pct:{n}")),
            Self::Invalid(raw) => s.serialize_str(raw),
        }
    }
}

impl<'de> Deserialize<'de> for WidthSpec {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        // Accept either an integer or a string. We handle the string case
        // permissively — unknown patterns become `Invalid` so the watch
        // crate can warn-and-fallback instead of refusing to load.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Int(i64),
            Str(String),
        }

        match Raw::deserialize(d)? {
            Raw::Int(n) => {
                let clamped = n.clamp(0, i64::from(u16::MAX));
                #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                Ok(Self::Length(clamped as u16))
            }
            Raw::Str(s) => Ok(parse_width_string(&s)),
        }
    }
}

fn parse_width_string(raw: &str) -> WidthSpec {
    if let Some(rest) = raw.strip_prefix("min:") {
        if let Ok(n) = rest.parse::<u16>() {
            return WidthSpec::Min(n);
        }
    } else if let Some(rest) = raw.strip_prefix("pct:") {
        if let Ok(n) = rest.parse::<u16>() {
            return WidthSpec::Percentage(n.min(100));
        }
    }
    WidthSpec::Invalid(raw.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_empty_toml() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(!cfg.notifier.enabled);
        // Discovery defaults on so users get backfill out of the box.
        assert!(cfg.discovery.enabled);
    }

    #[test]
    fn discovery_can_be_disabled() {
        let cfg: Config = toml::from_str("[discovery]\nenabled = false\n").unwrap();
        assert!(!cfg.discovery.enabled);
    }

    #[test]
    fn rejects_unknown_fields() {
        let err = toml::from_str::<Config>("unknown_field = 1").unwrap_err();
        assert!(err.to_string().contains("unknown"));
    }

    #[test]
    fn watch_default_is_prompt_forward() {
        let cfg = WatchConfig::default();
        assert_eq!(cfg.columns, vec!["pane", "state", "prompt", "activity"]);
        assert!(matches!(
            cfg.widths.get("pane"),
            Some(WidthSpec::Length(22))
        ));
        assert!(matches!(
            cfg.widths.get("state"),
            Some(WidthSpec::Length(14))
        ));
        assert!(matches!(cfg.widths.get("prompt"), Some(WidthSpec::Min(30))));
        assert!(matches!(
            cfg.widths.get("activity"),
            Some(WidthSpec::Length(10))
        ));
    }

    #[test]
    fn parses_watch_section() {
        let toml = r#"
[watch]
columns = ["pane", "prompt"]

[watch.widths]
pane = 30
prompt = "min:40"
ratio = "pct:25"
broken = "what"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.watch.columns, vec!["pane", "prompt"]);
        assert!(matches!(
            cfg.watch.widths.get("pane"),
            Some(WidthSpec::Length(30))
        ));
        assert!(matches!(
            cfg.watch.widths.get("prompt"),
            Some(WidthSpec::Min(40))
        ));
        assert!(matches!(
            cfg.watch.widths.get("ratio"),
            Some(WidthSpec::Percentage(25))
        ));
        assert!(matches!(
            cfg.watch.widths.get("broken"),
            Some(WidthSpec::Invalid(_))
        ));
    }

    #[test]
    fn detail_defaults_to_last_response_fallback_to_last_prompt_template() {
        let cfg = WatchConfig::default();
        assert!(cfg.detail.enabled);
        assert_eq!(cfg.detail.template, "{last_response|last_prompt}");
    }

    #[test]
    fn watch_sort_default_keeps_session_grouping_then_activity() {
        let cfg = WatchConfig::default();
        assert_eq!(
            cfg.sort,
            vec![WatchSortKey::Session, WatchSortKey::Activity]
        );
    }

    #[test]
    fn parses_watch_sort_section() {
        let toml = r#"
[watch]
sort = ["activity"]
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.watch.sort, vec![WatchSortKey::Activity]);
    }

    #[test]
    fn parses_watch_sort_with_multiple_keys() {
        let toml = r#"
[watch]
sort = ["session", "pane"]
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.watch.sort,
            vec![WatchSortKey::Session, WatchSortKey::Pane]
        );
    }

    #[test]
    fn unknown_watch_sort_key_is_rejected() {
        // serde rejects unknown enum variants — typos surface as parse
        // errors rather than silently dropping the key, matching the
        // `deny_unknown_fields` posture elsewhere in the config.
        let toml = r#"
[watch]
sort = ["nope"]
"#;
        assert!(toml::from_str::<Config>(toml).is_err());
    }

    #[test]
    fn parses_watch_detail_section() {
        let toml = r#"
[watch.detail]
enabled = false
template = "{cwd} · {last_prompt}"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(!cfg.watch.detail.enabled);
        assert_eq!(cfg.watch.detail.template, "{cwd} · {last_prompt}");
    }

    /// Missing `[watch.detail]` section -> defaults applied (enabled +
    /// `{last_response|last_prompt}` fallback template). The
    /// `default = WatchConfig::default` machinery on the parent struct
    /// must kick in.
    #[test]
    fn missing_watch_detail_section_uses_defaults() {
        let toml = r#"
[watch]
columns = ["pane", "prompt"]
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(cfg.watch.detail.enabled);
        assert_eq!(cfg.watch.detail.template, "{last_response|last_prompt}");
    }

    /// Partial `[watch.detail]` (only `enabled`, no `template`) — the
    /// missing `template` field must fall back to its default. This is
    /// the `serde(default)` on `DetailConfig` doing its job.
    #[test]
    fn partial_watch_detail_section_fills_missing_fields() {
        let toml = "
[watch.detail]
enabled = false
";
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(!cfg.watch.detail.enabled);
        assert_eq!(cfg.watch.detail.template, "{last_response|last_prompt}");
    }

    /// `deny_unknown_fields` is in force on `DetailConfig` — a stray
    /// key in `[watch.detail]` must surface as a parse error so typos
    /// don't fail silently.
    #[test]
    fn unknown_field_in_watch_detail_is_rejected() {
        let toml = r#"
[watch.detail]
enabled = true
template = "{last_prompt}"
unknown = 1
"#;
        let err = toml::from_str::<Config>(toml).unwrap_err();
        assert!(err.to_string().contains("unknown"));
    }

    #[test]
    fn percentage_clamped_to_100() {
        let toml = r#"
[watch.widths]
prompt = "pct:250"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(matches!(
            cfg.watch.widths.get("prompt"),
            Some(WidthSpec::Percentage(100))
        ));
    }

    /// Default `WatchConfig` opens the preview overlay in `LivePane` mode.
    /// Locks down the headline UX shipped with `[watch.preview]` so a
    /// future flip of the default is intentional and not a slip.
    #[test]
    fn watch_preview_default_is_live_pane() {
        let cfg = WatchConfig::default();
        assert_eq!(cfg.preview.default_content, PreviewContent::LivePane);
    }

    /// `[watch.preview] default_content = "prompt_response"` in TOML
    /// flips the overlay back to the text view. Both serde branches
    /// must be wired so users on the text-only workflow can opt out.
    #[test]
    fn watch_preview_can_be_set_to_prompt_response() {
        let toml = r#"
[watch.preview]
default_content = "prompt_response"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.watch.preview.default_content,
            PreviewContent::PromptResponse,
        );
    }

    /// `live_pane` round-trips through TOML — the explicit-default form.
    /// Together with the previous test this pins both serde variants.
    #[test]
    fn watch_preview_live_pane_round_trips() {
        let toml = r#"
[watch.preview]
default_content = "live_pane"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.watch.preview.default_content, PreviewContent::LivePane);
    }

    /// Unknown `default_content` values are rejected by serde rather than
    /// silently falling through — typos surface as parse errors so users
    /// don't end up with a "broken" overlay they can't explain.
    #[test]
    fn watch_preview_unknown_content_is_rejected() {
        let toml = r#"
[watch.preview]
default_content = "nope"
"#;
        assert!(toml::from_str::<Config>(toml).is_err());
    }
}
