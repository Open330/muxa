//! Configuration model.
//!
//! Loaded from TOML. CLI/env-var overrides happen at the binary layer — this
//! module only parses.

use crate::collaboration::RequestKind;
use crate::error::{CoreError, Result};
use crate::fleet::{validate_label_key, validate_label_value, HostAccessMode};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::net::{AddrParseError, SocketAddr};
use std::path::{Path, PathBuf};

/// Env var consulted at config-load time to satisfy the "non-loopback bind
/// requires a token" invariant when the TOML doesn't carry one. Mirrors the
/// `--dashboard-token` CLI flag in `muxad`.
const DASHBOARD_TOKEN_ENV: &str = "MUXA_DASHBOARD_TOKEN";

/// All known `[watch] columns` keys. Used at load time to warn on typos.
const WATCH_COLUMN_KEYS: &[&str] = &[
    "pane",
    "kind",
    "state",
    "state_age",
    "model",
    "ctx",
    "cost",
    "limits",
    "workload",
    "prompt",
    "activity",
    "duration",
];

/// All known placeholder names accepted in `[watch.detail] template`. A
/// placeholder is any `{name}` (or `{a|b|c}`) sequence; the resolver in
/// `muxa-cli` accepts these names. Unknown ones still pass through verbatim
/// at render time — we only warn at load.
const WATCH_DETAIL_PLACEHOLDERS: &[&str] = &[
    "pane",
    "kind",
    "state",
    "model",
    "ctx",
    "cost",
    "activity",
    "last_prompt",
    "last_response",
    "last_notification",
    "cwd",
    "workload",
    "rate_limit",
    "rate_limit_resets_at",
    "rate_limit_scope",
];

/// Semantic validation errors raised by [`Config::validate`] — i.e. shapes
/// that pass TOML deserialization but violate cross-field invariants we
/// want to surface at load time rather than during late startup. Each
/// variant points at the offending field with a path-style name so the
/// user can find it quickly.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("dashboard.bind: {addr:?} is not a valid socket address: {source}")]
    InvalidDashboardBind {
        addr: String,
        #[source]
        source: AddrParseError,
    },

    #[error(
        "dashboard.bind: {addr} is non-loopback; set dashboard.allow_public = true \
         (or pass --allow-public) to confirm you want to expose the dashboard \
         beyond this host"
    )]
    DashboardRequiresAllowPublic { addr: SocketAddr },

    #[error(
        "dashboard.bind: {addr} is non-loopback; a bearer token is required \
         — set `dashboard.token` in config OR `MUXA_DASHBOARD_TOKEN` in the \
         running daemon's environment. To intentionally expose read-only \
         API data without auth, set `dashboard.auth = \"none\"` too \
         (note: under systemd the unit's `Environment=` is what counts, \
         not your interactive shell)"
    )]
    DashboardRequiresToken { addr: SocketAddr },

    #[error(
        "sinks.oh_my_prompt: enabled = true but [sinks.oh_my_prompt].endpoint is \
         not set (no default endpoint by design)"
    )]
    OhMyPromptMissingEndpoint,

    #[error(
        "sinks.webhook: enabled = true but neither [sinks.webhook].endpoint \
         nor [sinks.webhook].endpoint_env is set (one of the two is required)"
    )]
    WebhookMissingEndpoint,

    #[error("{path}: {message}")]
    InvalidFleet { path: String, message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Unix socket path. Overrides the XDG default.
    pub socket: Option<PathBuf>,

    pub ui: UiConfig,
    pub notifier: NotifierConfig,
    pub watch: WatchConfig,
    pub dashboard: DashboardTomlConfig,
    /// SSH-connected physical hosts aggregated by the local daemon.
    pub fleet: FleetConfig,
    pub discovery: DiscoveryConfig,
    pub reconciler: ReconcilerConfig,
    pub screen_detect: ScreenDetectConfig,
    pub collaboration: CollaborationConfig,
    /// Reusable text templates for the interactive `m` message composer.
    pub message: MessageConfig,
    #[serde(default)]
    pub ask: AskConfig,
    pub history: HistoryConfig,
    pub activity: ActivityConfig,
    pub state: StateConfig,
    pub session_activity: SessionActivityConfig,
    pub sinks: SinksConfig,
    pub stats: StatsConfig,
    /// How `muxa work up` turns a work id into ticket context.
    pub ticket: TicketConfig,
    /// Work-id routing rules, first match wins.
    pub route: Vec<RouteConfig>,
    /// Named agent line-ups, keyed by pipeline name.
    pub pipeline: BTreeMap<String, PipelineConfig>,
}

/// Fleet-wide connection and refresh policy. Inventory keys are stable local
/// aliases; the remote relay supplies the durable physical [`NodeId`](crate::fleet::NodeId).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FleetConfig {
    /// Enable persistent outbound SSH connections. The controller's local
    /// node is published regardless of this flag.
    pub enabled: bool,
    /// Metadata for the controller node. It is always visible; `enabled`
    /// controls only outbound SSH host connections.
    pub local: FleetLocalConfig,
    pub refresh_secs: u64,
    pub keepalive_secs: u64,
    pub offline_after_secs: u64,
    pub connect_timeout_secs: u64,
    pub command_timeout_secs: u64,
    pub max_parallel_connects: usize,
    pub capture_policy: FleetCapturePolicy,
    pub hosts: BTreeMap<String, FleetHostConfig>,
}

impl Default for FleetConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            local: FleetLocalConfig::default(),
            refresh_secs: 15,
            keepalive_secs: 10,
            offline_after_secs: 30,
            connect_timeout_secs: 10,
            command_timeout_secs: 10,
            max_parallel_connects: 6,
            capture_policy: FleetCapturePolicy::Selected,
            hosts: BTreeMap::new(),
        }
    }
}

/// User-managed metadata layered onto the always-present local Fleet node.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FleetLocalConfig {
    pub labels: BTreeMap<String, String>,
    pub annotations: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FleetCapturePolicy {
    Never,
    #[default]
    Selected,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FleetConnectPolicy {
    #[default]
    Auto,
    OnDemand,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FleetHostConfig {
    /// OpenSSH destination or Host alias. User/port/key/ProxyJump stay in
    /// `~/.ssh/config` rather than being duplicated in muxa configuration.
    pub ssh: String,
    /// Remote binary invoked as a fixed command. Kept shell-token-safe because
    /// OpenSSH ultimately passes a command string to the remote login shell.
    pub muxa_path: String,
    pub remote_socket: Option<PathBuf>,
    pub enabled: bool,
    pub connect: FleetConnectPolicy,
    pub mode: HostAccessMode,
    pub labels: BTreeMap<String, String>,
    pub annotations: BTreeMap<String, String>,
}

impl Default for FleetHostConfig {
    fn default() -> Self {
        Self {
            ssh: String::new(),
            muxa_path: "muxa".into(),
            remote_socket: None,
            enabled: true,
            connect: FleetConnectPolicy::Auto,
            mode: HostAccessMode::Observe,
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
        }
    }
}

/// `[message.skills]` config — reusable prompt templates for message
/// composers. A sorted map keeps the `/` palette deterministic while the TOML
/// remains pleasantly hand-editable:
///
/// ```toml
/// [message.skills]
/// agent-review = "Create a new pane with codex and review our changes."
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct MessageConfig {
    pub skills: BTreeMap<String, String>,
}

/// Default wall-clock ceiling for one headless ask turn.
pub const DEFAULT_ASK_TIMEOUT_SECS: u64 = 30 * 60;

/// `[ask]` config — headless one-shot queries from `muxa watch`.
///
/// Disabled by default: enabling it lets the daemon spawn an agent CLI
/// that bills the user's account. That is a grant, like collaboration's.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AskConfig {
    pub enabled: bool,
    /// `claude` or `codex`.
    pub agent: String,
    /// Directory the headless process runs in. Defaults to `$HOME`; a neutral
    /// cwd keeps default-mode questions away from a working tree. Explicit
    /// `edit`/`bypass` automation can select its roots separately.
    pub cwd: Option<PathBuf>,
    /// Permission policy passed to the headless agent. `bypass` is the
    /// default because ask runs unattended and cannot answer approval
    /// prompts; `default` preserves the agent CLI's normal policy, while
    /// `edit` retains its sandbox/review layer.
    pub permission_mode: AskPermissionMode,
    /// Extra workspace roots exposed to the headless agent. This is required
    /// when a path below `cwd` is a symlink whose real path lives elsewhere.
    pub additional_dirs: Vec<PathBuf>,
    /// Wall-clock ceiling per question. Long-running skills often spend
    /// several minutes preparing a persistent worker before they answer.
    pub timeout_secs: u64,
    /// History snapshot. Defaults to `$XDG_DATA_HOME/muxa/ask.json`.
    pub path: Option<PathBuf>,
    /// Answers retained before the oldest are dropped.
    pub keep: usize,
}

impl Default for AskConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            agent: "claude".into(),
            cwd: None,
            permission_mode: AskPermissionMode::Bypass,
            additional_dirs: Vec::new(),
            timeout_secs: DEFAULT_ASK_TIMEOUT_SECS,
            path: None,
            keep: 200,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskPermissionMode {
    /// Preserve the selected agent CLI's normal permission behavior.
    Default,
    /// Permit workspace edits while retaining sandbox/review protection.
    Edit,
    /// Disable approval and sandbox checks for unattended automation.
    #[default]
    Bypass,
}

/// Default wall-clock ceiling for one external-issue resolver turn. Short
/// next to `[ask]`'s: a resolver looks one issue up, it does not do the Work.
pub const DEFAULT_TICKET_TIMEOUT_SECS: u64 = 180;

/// `[ticket]` config — how a work id becomes ticket context.
///
/// Muxa does not speak Linear, Jira, or GitHub. It runs one headless agent
/// turn and asks *that* agent to fetch the ticket, because the user has
/// already taught their agent CLI how: skills, MCP servers, `gh`, an API
/// token in the environment. Teaching muxa the same thing a second time
/// would mean shipping a client per provider and chasing every schema
/// change. Here, adding a provider is a prompt.
///
/// Disabled by default in the sense that matters: with no `[ticket.source]`
/// entries nothing is ever spawned, and `muxa work up` runs on the work id
/// alone.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TicketConfig {
    /// Resolver agent CLI: `claude` or `codex`. Only these two have a
    /// print mode that reports completion as a fact rather than a guess.
    pub agent: String,
    /// Directory the resolver runs in. Defaults to `$HOME`, which is both
    /// where user-scoped skills live and a neutral place to run something
    /// that should be reading an issue tracker, not a working tree.
    pub cwd: Option<PathBuf>,
    /// Permission policy for the resolver turn. `bypass` by default for
    /// the same reason `[ask]` uses it: nobody is at the keyboard to
    /// answer an approval prompt.
    pub permission_mode: AskPermissionMode,
    /// Extra roots exposed to the resolver, for skills that live outside
    /// `cwd` or reach it through a symlink.
    pub additional_dirs: Vec<PathBuf>,
    /// Wall-clock ceiling for one resolver turn.
    pub timeout_secs: u64,
    /// Serve a cached ticket this long before spending another agent turn.
    /// Re-running `muxa work up` to add a pane is common; paying for the
    /// same lookup each time is not. `0` disables the cache.
    pub cache_secs: u64,
    /// Ticket sources keyed by name, tried in sorted-key order. The first
    /// whose `match` accepts the work id wins.
    pub source: BTreeMap<String, TicketSource>,
}

impl Default for TicketConfig {
    fn default() -> Self {
        Self {
            agent: "claude".into(),
            cwd: None,
            permission_mode: AskPermissionMode::Bypass,
            additional_dirs: Vec::new(),
            timeout_secs: DEFAULT_TICKET_TIMEOUT_SECS,
            cache_secs: 900,
            source: BTreeMap::new(),
        }
    }
}

/// One `[ticket.source.<name>]` entry — a work-id pattern and the prompt
/// that turns a matching id into ticket JSON.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct TicketSource {
    /// Regex matched against the work id, case-insensitively.
    #[serde(rename = "match")]
    pub pattern: String,
    /// Prompt handed to the resolver. Placeholders are rendered before it
    /// is sent; `{{id}}` is the work id. The reply is scanned for a JSON
    /// object, so the prompt should ask for one and nothing else.
    pub prompt: String,
}

/// One `[[route]]` entry — a work-id pattern and the tmux geography plus
/// pipeline it selects. First match wins, so specific rules go above the
/// catch-all.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct RouteConfig {
    /// Regex matched against the work id, case-insensitively.
    #[serde(rename = "match")]
    pub pattern: String,
    /// Workspace id, i.e. the tmux session. Defaults to the directory name
    /// of the resolved cwd, matching `muxa work start`.
    pub workspace: Option<String>,
    /// Pipeline to staff the work window with. Without one, `muxa work up`
    /// needs `--pipeline`.
    pub pipeline: Option<String>,
    /// Working directory for the work window. Ignored when `worktree` is
    /// set, which computes its own.
    pub cwd: Option<String>,
    /// Give this work its own git worktree instead of sharing a checkout.
    pub worktree: Option<WorktreeConfig>,
    /// Command that provisions this work's environment, run once when the
    /// work window does not exist yet.
    ///
    /// Teams that already own provisioning — a workspace manager, a
    /// container, a devbox — should keep owning it. muxa runs the command
    /// and then works in `cwd`; it does not try to learn what a workspace
    /// is. Pair it with `cwd`, since the directory usually does not exist
    /// until the command has run.
    ///
    /// Placeholders resolve first, including `{{ticket.*}}` — so a branch
    /// name the resolver decided (`fix/` for a bug, `feat/` otherwise) can
    /// be passed straight through.
    pub prepare: Option<String>,
}

/// `[route.worktree]` — a git worktree per work item, so three agents in
/// one window cannot trip over each other's edits in a shared checkout.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WorktreeConfig {
    /// Repository the worktree is added to.
    pub repo: String,
    /// Where the worktree is created. Defaults to
    /// `<repo>/../<repo-name>-worktrees/{{id}}`, which keeps it beside the
    /// repo rather than inside it.
    pub path: Option<String>,
    /// Branch to check out, created when absent. Defaults to `{{id}}`.
    pub branch: Option<String>,
    /// Commit-ish a newly created branch starts from. Defaults to the
    /// repo's `origin/HEAD`.
    pub base: Option<String>,
}

/// One `[pipeline.<name>]` entry — the set of agents a work window should
/// end up staffed with. This is a desired state, not a script: `muxa work
/// up` compares it against the panes that already exist and creates only
/// what is missing, so re-running it converges instead of duplicating.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PipelineConfig {
    /// Shown by `muxa work up --dry-run` and in error messages.
    pub description: Option<String>,
    /// tmux layout applied once every pane exists, e.g. `main-vertical`,
    /// `even-horizontal`, `tiled`. Omit to leave tmux's own arrangement.
    pub layout: Option<String>,
    /// Prompt prefix prepended to every agent's prompt in this pipeline —
    /// the ticket context each of them needs before its own instructions.
    pub prompt: Option<String>,
    /// The agents themselves, in creation order.
    pub agent: Vec<PipelineAgentConfig>,
}

/// One `[[pipeline.<name>.agent]]` entry — one pane.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PipelineAgentConfig {
    /// Stable name for this pane within the work window. This is the key
    /// the desired-vs-actual diff runs on, so it has to be unique in the
    /// pipeline and must not change once panes exist under it.
    pub alias: String,
    /// Allowlisted agent CLI: `claude`, `codex`, `gemini`, or `opencode`.
    pub program: String,
    /// Collaboration role, recorded on the pane so peers can address it as
    /// `role:<role>`.
    pub role: Option<String>,
    /// Short label shown in `muxa work show` and `muxa watch`.
    pub task: Option<String>,
    /// This agent's own instructions, appended to the pipeline prompt.
    pub prompt: Option<String>,
    /// Split direction when this pane joins an existing window: `right`
    /// (default) or `down`.
    pub direction: Option<String>,
    /// Aliases in this pipeline that must report finishing before this
    /// agent is launched at all.
    ///
    /// Without it every agent starts at once, which is right for work that
    /// is genuinely parallel and wrong for work that is not: a reviewer
    /// launched beside its implementer reviews a tree that changes while it
    /// reads, and spends its rounds rediscovering that the code moved. An
    /// edge here is what makes the difference expressible.
    pub after: Vec<String>,
}

/// `[collaboration]` config — same-window durable agent request/reply.
/// Disabled by default because idle wake-up injects a prompt into a peer pane.
/// Enabling it is an explicit grant for local peer coordination.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CollaborationConfig {
    pub enabled: bool,
    /// Atomic mailbox snapshot. Defaults to
    /// `$XDG_DATA_HOME/muxa/collaboration.json`.
    pub path: Option<PathBuf>,
    /// `never` keeps delivery pull-only; `idle_only` wakes hook-authoritative
    /// agents only at their top-level idle prompt.
    pub wake: CollaborationWake,
    /// `notice` injects only a mailbox notification. `full` atomically claims
    /// each request and injects its metadata and body into the recipient's
    /// prompt, avoiding a separate inbox tool round.
    pub wake_payload: CollaborationWakePayload,
    /// How far an explicit `pane:%N` target may reach. `window` (default)
    /// keeps requests inside the sender's tmux window — co-locating agents
    /// in a window is the consent to let them talk. `host` lets a request
    /// address any tracked agent pane on this host, which is what makes
    /// `muxa watch`'s composer work against whatever row the cursor is on.
    /// `peer` / `@alias` / `role:` stay window-scoped either way: they are
    /// room concepts, and host-wide alias matching would invite misdelivery.
    pub scope: CollaborationScope,
    #[serde(default = "default_collaboration_max_message_bytes")]
    pub max_message_bytes: usize,
}

impl Default for CollaborationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: None,
            wake: CollaborationWake::IdleOnly,
            wake_payload: CollaborationWakePayload::Notice,
            scope: CollaborationScope::default(),
            max_message_bytes: default_collaboration_max_message_bytes(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CollaborationScope {
    #[default]
    Window,
    Host,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CollaborationWake {
    Never,
    #[default]
    IdleOnly,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CollaborationWakePayload {
    #[default]
    Notice,
    Full,
}

fn default_collaboration_max_message_bytes() -> usize {
    16 * 1024
}

/// `[stats]` config — tuning for the engaged ("active") time estimate in
/// `muxa stats` / `muxa report`. ACTIVE pads each human action (a prompt or a
/// tmux input tick) into a window; these control its size.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StatsConfig {
    /// Seconds credited *before* each action (reading/orienting just prior).
    pub active_lookback_secs: u64,
    /// Idle timeout: seconds a *prompt* keeps the "active" clock running after
    /// it. Gaps longer than this read as away. Larger = more generous.
    pub active_timeout_secs: u64,
    /// Idle timeout applied to *tmux input ticks* (keypress / scroll) instead of
    /// `active_timeout_secs`. A single keypress or scroll implies far less
    /// sustained work than submitting a prompt, so this is shorter to stop sparse
    /// scrolling while watching an agent from chaining into hours of "active"
    /// time. Applies to both `active` and `work_active`.
    pub active_tick_timeout_secs: u64,
    /// Whether tmux input ticks (keypress / scrollback, derived from a client's
    /// `#{client_activity}` advancing between polls) seed ACTIVE windows.
    ///
    /// tmux advances `client_activity` for *any* client input — and with
    /// `mouse on`, that includes mouse motion, wheel, clicks, and focus events,
    /// which tmux exposes no way to tell apart from a keypress. So a session left
    /// attached can keep accruing `ACTIVE`/`WORK_ACTIVE` purely from the mouse
    /// passing over it, even while its agent is idle. Set this to `false` to drop tmux
    /// ticks entirely and anchor ACTIVE only on deliberate actions — submitted
    /// prompts and time spent while an agent waits on you (thinking). Default
    /// `true` keeps the historical behavior (tmux ticks counted).
    pub count_tmux_input: bool,
}

impl Default for StatsConfig {
    fn default() -> Self {
        Self {
            active_lookback_secs: default_active_lookback_secs(),
            active_timeout_secs: default_active_timeout_secs(),
            active_tick_timeout_secs: default_active_tick_timeout_secs(),
            count_tmux_input: default_count_tmux_input(),
        }
    }
}

fn default_active_lookback_secs() -> u64 {
    60
}

fn default_active_timeout_secs() -> u64 {
    300
}

fn default_active_tick_timeout_secs() -> u64 {
    90
}

fn default_count_tmux_input() -> bool {
    true
}

/// `[ui]` config — shared visual defaults for human-facing terminal output.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    /// Visual preset used by table output and, unless overridden, `muxa watch`.
    pub theme: WatchTheme,
    /// Glyph set for agent-state icons across `status`, `status-line`,
    /// `attend`, and `watch`. Defaults to the Unicode geometric glyphs;
    /// set `ascii` for terminals whose font lacks them (or substitutes a
    /// mismatched-size fallback font).
    pub icons: IconSet,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: WatchTheme::Classic,
            icons: IconSet::Unicode,
        }
    }
}

/// Glyph set for agent-state icons in human-facing terminal output.
///
/// `unicode` uses the basic Geometric Shapes glyphs (`●▶◆■○◌×`), which are
/// present in virtually every monospace font. `ascii` falls back to single
/// `[char]` markers for terminals whose primary font lacks those codepoints
/// and would otherwise borrow a mismatched-size glyph from a fallback font.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IconSet {
    #[default]
    Unicode,
    Ascii,
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
    pub webhook: WebhookToml,
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

/// `[sinks.webhook]` raw TOML schema. The daemon resolves these fields
/// against env vars + defaults via `WebhookSink::resolve` at startup.
///
/// Either `endpoint` (URL inline in TOML) OR `endpoint_env` (name of an
/// env var holding the URL) is required when `enabled = true`. The
/// env-var path is preferred for Slack/Discord webhooks because the URL
/// itself is the secret — committing it to a shared dotfile is a leak.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebhookToml {
    pub enabled: Option<bool>,
    /// Full webhook URL. Mutually-optional with `endpoint_env`; the env
    /// var wins when both are set.
    pub endpoint: Option<String>,
    /// Name of an env var holding the full webhook URL. Set this in
    /// preference to `endpoint` so the secret URL never lives in TOML.
    pub endpoint_env: Option<String>,
    /// Wire-format flavor: `slack` | `discord` | `generic`. Auto-detected
    /// from the URL when unset.
    pub flavor: Option<String>,
    /// State transitions to forward. Defaults to `["WaitingInput",
    /// "Error"]` — the two states that mean "operator attention needed".
    /// `PascalCase` or `snake_case` are both accepted at resolve time.
    pub on_states: Option<Vec<String>>,
    /// Per-`(kind, session_id, state)` rate-limit window in seconds.
    /// Defaults to 60. Set to 0 to disable (one notification per
    /// transition, even if the agent flaps).
    pub rate_limit_secs: Option<u64>,
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
    /// API authentication mode. `"token"` protects reads and writes,
    /// `"public_read"` exposes reads while requiring the token for control
    /// actions, and `"none"` exposes reads with control actions disabled.
    pub auth: Option<DashboardAuthMode>,
    /// Bearer token / browser PAT. Empty string is treated as "unset".
    /// Required by both `auth = "token"` and `auth = "public_read"`.
    pub token: Option<String>,
    /// Required to be `true` for non-loopback `bind` values. Acts as an
    /// explicit acknowledgement that the operator means to expose the
    /// dashboard beyond the local machine.
    pub allow_public: Option<bool>,
    /// Pane scanner cache TTL in milliseconds. Default 2000.
    pub pane_cache_ttl_ms: Option<u64>,
    /// Let the dashboard stand up a work pipeline — that is, *launch agent
    /// processes* — through `POST /api/work-control/up`.
    ///
    /// Off by default, and deliberately its own switch rather than riding
    /// on the control token. Every other write route steers a process the
    /// operator already started; this one starts new ones with permissions
    /// bypassed. That is a different kind of authority and deserves a
    /// different grant, the same way `[ask]` and `[collaboration]` do.
    pub allow_work_start: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DashboardAuthMode {
    Token,
    PublicRead,
    None,
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
    /// Cadence of the periodic discovery rescan, in seconds. Startup
    /// discovery always runs once; this keeps newly-created panes (a fresh
    /// `claude`/`codex`/`gemini` session in a new tmux session) appearing in
    /// `muxa status` within `interval_secs` instead of only after the agent
    /// fires its first hook. Set `0` to keep the legacy run-once-at-startup
    /// behavior. The pass uses one `tmux list-panes` and, only for wrapper
    /// foreground commands, one bounded process-table snapshot.
    #[serde(default = "default_discovery_interval_secs")]
    pub interval_secs: u64,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: default_discovery_interval_secs(),
        }
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
    /// If non-zero, agents stuck in `Working` for longer than this many
    /// seconds get auto-downgraded to `Idle`. Insurance against missed
    /// `Stop`/`TurnStopped` hook firings — without it a single dropped
    /// hook leaves the row glowing green forever.
    ///
    /// Default `0` (disabled) preserves the historical "state changes
    /// only on explicit events" guarantee. A reasonable opt-in value
    /// is `300` (5 min) for interactive use; longer if your agents
    /// routinely run multi-hour tasks.
    #[serde(default = "default_zero")]
    pub stuck_working_timeout_secs: u64,
    /// Same shape as `stuck_working_timeout_secs` but for the
    /// `WaitingInput` state. Specifically targets Codex's
    /// permission-grant gap: `permission_request` flips the row to
    /// `WaitingInput`, the user grants permission, Codex resumes —
    /// but Codex never fires another hook, so the row stays yellow
    /// indefinitely.
    ///
    /// Default `0` (disabled). A reasonable opt-in is `600` (10 min)
    /// — `WaitingInput` legitimately means the user is away from the
    /// keyboard, so the cutoff should be generous.
    #[serde(default = "default_zero")]
    pub stuck_waiting_timeout_secs: u64,
    /// Poll codex session-rollout files (`~/.codex/sessions`) each tick for
    /// rate-limit state. Codex exposes no error/rate-limit hook, so this is
    /// the only way muxa learns a codex usage cap — including a cap that
    /// blocks a turn before any hook fires. Reads the tail of each live
    /// codex session's JSONL; cost scales with the number of live codex
    /// sessions, not history size.
    ///
    /// Default `true`. Set `false` to disable (e.g. non-codex deployments
    /// that want to skip the per-tick directory scan entirely).
    #[serde(default = "default_true")]
    pub codex_rollout_enabled: bool,
    /// Age (seconds) after which a fully orphaned agent row — no pane, no
    /// surface, no pid — is flipped to `Stopped` so the regular GC can reap
    /// it. Closes the liveness hole where a codex session driven through a
    /// detached `app-server`/remote bridge fires paneless hooks and never
    /// transitions to `Stopped`, so the row lingers forever and `muxa
    /// watch`'s `+N paneless` count only ever grows.
    ///
    /// Default `86400` (24h): a human-driven remote session idle for a full
    /// day is effectively dead, and only the registry row is removed — the
    /// underlying tmux session (if any) is never touched. Set `0` to disable
    /// and preserve the historical "orphan rows persist" behaviour.
    #[serde(default = "default_paneless_stale_secs")]
    pub paneless_stale_timeout_secs: u64,
}

impl Default for ReconcilerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: default_reconciler_interval_secs(),
            stuck_working_timeout_secs: 0,
            stuck_waiting_timeout_secs: 0,
            codex_rollout_enabled: true,
            paneless_stale_timeout_secs: default_paneless_stale_secs(),
        }
    }
}

fn default_reconciler_interval_secs() -> u64 {
    30
}

/// `[screen_detect]` config — the screen-manifest fallback detector.
///
/// For agent CLIs muxa has **no hooks** for (cursor-agent, amp, copilot, aider,
/// goose, plus any user-declared agent), the daemon periodically captures the
/// pane and matches TOML manifest rules against the visible tail to infer
/// `Working` / `WaitingInput` / `Idle`. This is the *last-resort* fallback:
/// hooks stay authoritative when present, herdr hosts are covered by herdr's
/// own detection + bridge (and are skipped here), and the synthetic rows this
/// task mints are evicted the instant a real hook claims the pane. See
/// `docs/SCREEN_DETECTION.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScreenDetectConfig {
    /// Master switch. Default `true` — the detector only does real work when a
    /// pane's foreground command matches a manifest AND no authoritative row
    /// owns the pane, so its idle cost is ~one pane list per tick.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Cadence of the capture/classify pass, in seconds. Default 3 — brisk
    /// enough to feel live, slow enough that the per-candidate `capture-pane`
    /// shell-outs (each bounded by tmux's 1s timeout) stay negligible. A tick is
    /// skipped if the previous one is still running.
    #[serde(default = "default_screen_detect_interval_secs")]
    pub interval_secs: u64,
}

impl Default for ScreenDetectConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: default_screen_detect_interval_secs(),
        }
    }
}

fn default_screen_detect_interval_secs() -> u64 {
    3
}

fn default_paneless_stale_secs() -> u64 {
    // 24 hours — see `ReconcilerConfig::paneless_stale_timeout_secs`.
    86_400
}

fn default_discovery_interval_secs() -> u64 {
    30
}

fn default_zero() -> u64 {
    0
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

/// `[activity]` config — controls the append-only duration ledger.
///
/// The activity ledger records closed intervals for agent state transitions
/// (`Working`, `WaitingInput`, `Error`, etc.) and tmux session foreground
/// time. Unlike the live registry, these rows survive pane/session removal,
/// which lets `muxa stats --since ...` compute windowed duration later.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ActivityConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Override the default `$XDG_DATA_HOME/muxa/activity.ndjson` path.
    pub path: Option<PathBuf>,
    /// Compaction drops intervals whose end timestamp is older than this.
    #[serde(default = "default_activity_max_age_days")]
    pub max_age_days: u32,
    /// How often the compaction task rewrites the file.
    #[serde(default = "default_activity_compact_interval_secs")]
    pub compact_interval_secs: u64,
}

impl Default for ActivityConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: None,
            max_age_days: default_activity_max_age_days(),
            compact_interval_secs: default_activity_compact_interval_secs(),
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

/// `[session_activity]` config — tracks cumulative tmux foreground time.
///
/// A session counts as active while an interactive tmux client has that
/// session foregrounded (`tmux list-clients` grouped by `client_session`).
/// The daemon polls tmux and persists totals so `muxa watch --view session`
/// can show "how long was I actually attached to this session?" rather
/// than just agent hook time.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionActivityConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Override the default `$XDG_DATA_HOME/muxa/session-activity.json` path.
    pub path: Option<PathBuf>,
    /// Poll cadence. Defaults to 5s, which keeps display error small while
    /// making the tmux shell-out cost negligible.
    #[serde(default = "default_session_activity_interval_secs")]
    pub interval_secs: u64,
}

impl Default for SessionActivityConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: None,
            interval_secs: default_session_activity_interval_secs(),
        }
    }
}

fn default_session_activity_interval_secs() -> u64 {
    5
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
fn default_activity_max_age_days() -> u32 {
    30
}
fn default_activity_compact_interval_secs() -> u64 {
    3600
}

fn default_true() -> bool {
    true
}

impl Config {
    /// Load a config from the given file. Missing file is an error — use
    /// `load_or_default` if you want silent fallback.
    ///
    /// Runs [`Self::validate`] after deserialization so semantic errors
    /// that apply to *every* consumer surface here. Daemon-only checks
    /// (dashboard bind/token, sink endpoints) are *not* run here — those
    /// live behind [`Self::validate_for_daemon`] so CLI commands like
    /// `muxa watch` and `muxa status` are unaffected by daemon-only
    /// misconfiguration. Soft issues (unknown `[watch]` column key,
    /// unknown detail-template placeholder) emit `tracing::warn!` but do
    /// not fail the load — config compatibility matters more than strict
    /// validation for those.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let cfg: Self = toml::from_str(&text).map_err(|e| CoreError::ConfigParse {
            path: path.to_path_buf(),
            source: e,
        })?;
        cfg.validate().map_err(|source| CoreError::ConfigValidate {
            path: path.to_path_buf(),
            source,
        })?;
        cfg.warn_soft_issues();
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

    /// Run hard semantic checks that apply to *every* consumer (CLI and
    /// daemon alike). Idempotent and side-effect-free; safe to call from
    /// tests against a synthesized `Config`.
    ///
    /// Currently this is a no-op — every check we have today is
    /// daemon-specific (dashboard wire-up, sink fan-out) and lives in
    /// [`Self::validate_for_daemon`]. The method is kept (rather than
    /// inlined away) so future cross-consumer invariants have an obvious
    /// home, and so the always-on / daemon-only split stays legible to
    /// callers.
    pub fn validate(&self) -> std::result::Result<(), ConfigError> {
        validate_fleet(&self.fleet)
    }

    /// Run hard semantic checks that only matter when the daemon is
    /// actually starting up: the dashboard server wire-up
    /// (`bind` / `token` / `allow_public`) and sink fan-out
    /// (`[sinks.oh_my_prompt]` endpoint). Called by `muxad/main.rs` from
    /// inside `tokio::main` after `Config::load` returns; deliberately
    /// *not* called from the CLI's load path because `muxa watch` /
    /// `muxa status` / `muxa recap` etc. don't touch the dashboard or
    /// sinks at all and shouldn't fail on dashboard-only misconfig.
    ///
    /// The runtime resolvers (`DashboardConfig::resolve`,
    /// `OhMyPromptSink::resolve`) repeat the same checks so CLI/env
    /// overrides applied later still get validated — this method only
    /// covers what we can determine from the TOML alone.
    pub fn validate_for_daemon(&self) -> std::result::Result<(), ConfigError> {
        validate_dashboard(&self.dashboard)?;
        validate_oh_my_prompt(&self.sinks.oh_my_prompt)?;
        validate_webhook(&self.sinks.webhook)?;
        Ok(())
    }

    /// Emit `tracing::warn!` for soft validation issues — typos in
    /// `[watch] columns`, unknown placeholders in
    /// `[watch.detail] template`. Never errors. Keeping these as warnings
    /// means a config written for a newer/older `muxa` version still loads.
    fn warn_soft_issues(&self) {
        for key in &self.watch.columns {
            if !WATCH_COLUMN_KEYS.contains(&key.as_str()) {
                tracing::warn!(
                    column = %key,
                    known = ?WATCH_COLUMN_KEYS,
                    "watch.columns: unknown key — it will be skipped at render time",
                );
            }
        }
        for key in self.watch.widths.keys() {
            if !WATCH_COLUMN_KEYS.contains(&key.as_str()) {
                tracing::warn!(
                    column = %key,
                    known = ?WATCH_COLUMN_KEYS,
                    "watch.widths: unknown key — it will be ignored at render time",
                );
            }
        }
        for placeholder in unknown_detail_placeholders(&self.watch.detail.template) {
            tracing::warn!(
                placeholder = %placeholder,
                known = ?WATCH_DETAIL_PLACEHOLDERS,
                "watch.detail.template: unknown placeholder — it will be left verbatim at render time",
            );
        }
    }
}

fn validate_fleet(cfg: &FleetConfig) -> std::result::Result<(), ConfigError> {
    let invalid = |path: String, message: String| ConfigError::InvalidFleet { path, message };
    if cfg.refresh_secs == 0 {
        return Err(invalid(
            "fleet.refresh_secs".into(),
            "must be greater than zero".into(),
        ));
    }
    if cfg.keepalive_secs == 0 {
        return Err(invalid(
            "fleet.keepalive_secs".into(),
            "must be greater than zero".into(),
        ));
    }
    if cfg.offline_after_secs < cfg.keepalive_secs.saturating_mul(2) {
        return Err(invalid(
            "fleet.offline_after_secs".into(),
            "must allow at least two keepalive intervals".into(),
        ));
    }
    if cfg.connect_timeout_secs == 0 || cfg.command_timeout_secs == 0 {
        return Err(invalid(
            "fleet".into(),
            "connect_timeout_secs and command_timeout_secs must be greater than zero".into(),
        ));
    }
    if cfg.max_parallel_connects == 0 || cfg.max_parallel_connects > 128 {
        return Err(invalid(
            "fleet.max_parallel_connects".into(),
            "must be between 1 and 128".into(),
        ));
    }
    validate_fleet_metadata(
        &cfg.local.labels,
        &cfg.local.annotations,
        "fleet.local",
        crate::fleet::LOCAL_MANAGED_LABELS,
    )?;
    for (alias, host) in &cfg.hosts {
        if alias == crate::fleet::LOCAL_HOST_ALIAS {
            return Err(invalid(
                format!("fleet.hosts.{alias}"),
                "the alias 'local' is reserved for this muxad node; configure its metadata under [fleet.local]"
                    .into(),
            ));
        }
        validate_label_value(alias).map_err(|message| {
            invalid(
                format!("fleet.hosts.{alias}"),
                format!("invalid host alias: {message}"),
            )
        })?;
        if host.ssh.is_empty()
            || host.ssh.starts_with('-')
            || host
                .ssh
                .chars()
                .any(|ch| ch.is_control() || ch.is_whitespace())
        {
            return Err(invalid(
                format!("fleet.hosts.{alias}.ssh"),
                "must be a non-empty OpenSSH destination without whitespace and must not start with '-'"
                    .into(),
            ));
        }
        if host.muxa_path.is_empty()
            || !host
                .muxa_path
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | '+'))
        {
            return Err(invalid(
                format!("fleet.hosts.{alias}.muxa_path"),
                "must be a shell-token-safe binary name or path".into(),
            ));
        }
        if let Some(socket) = &host.remote_socket {
            let value = socket.to_string_lossy();
            if value.is_empty()
                || value.chars().any(|ch| {
                    ch.is_control() || ch.is_whitespace() || "'\"`;|&<>$(){}[]".contains(ch)
                })
            {
                return Err(invalid(
                    format!("fleet.hosts.{alias}.remote_socket"),
                    "must be a shell-token-safe remote path without whitespace".into(),
                ));
            }
        }
        validate_fleet_metadata(
            &host.labels,
            &host.annotations,
            &format!("fleet.hosts.{alias}"),
            &[],
        )?;
    }
    Ok(())
}

fn validate_fleet_metadata(
    labels: &BTreeMap<String, String>,
    annotations: &BTreeMap<String, String>,
    path: &str,
    managed_labels: &[&str],
) -> std::result::Result<(), ConfigError> {
    for (key, value) in labels {
        if managed_labels.contains(&key.as_str()) {
            return Err(ConfigError::InvalidFleet {
                path: format!("{path}.labels.{key}"),
                message: "is managed by muxad and cannot be overridden".into(),
            });
        }
        validate_label_key(key).map_err(|message| ConfigError::InvalidFleet {
            path: format!("{path}.labels.{key}"),
            message,
        })?;
        validate_label_value(value).map_err(|message| ConfigError::InvalidFleet {
            path: format!("{path}.labels.{key}"),
            message,
        })?;
    }
    for key in annotations.keys() {
        validate_label_key(key).map_err(|message| ConfigError::InvalidFleet {
            path: format!("{path}.annotations.{key}"),
            message,
        })?;
    }
    Ok(())
}

fn validate_dashboard(cfg: &DashboardTomlConfig) -> std::result::Result<(), ConfigError> {
    let Some(bind_str) = cfg.bind.as_deref() else {
        return Ok(());
    };
    let bind: SocketAddr =
        bind_str
            .parse()
            .map_err(|source| ConfigError::InvalidDashboardBind {
                addr: bind_str.to_string(),
                source,
            })?;

    if bind.ip().is_loopback() {
        return Ok(());
    }

    // Non-loopback path mirrors `DashboardConfig::resolve`: allow_public
    // plus either a non-empty token or explicit `auth = "none"` is
    // required. `public_read` still needs a token for control actions.
    // We honor the env var here because `muxad` reads it via
    // clap; without it we'd emit false positives for users whose only
    // token source is the env.
    if !cfg.allow_public.unwrap_or(false) {
        return Err(ConfigError::DashboardRequiresAllowPublic { addr: bind });
    }
    if matches!(cfg.auth, Some(DashboardAuthMode::None)) {
        return Ok(());
    }

    // Whitespace-only tokens (`"   "`) are pathological — they pass a
    // naive non-empty check but the dashboard's bearer-token comparator
    // will never accept a real client. Treat them as unset on both
    // sides (TOML and env) so the user gets a config-time error
    // instead of a "why won't anyone authenticate" runtime mystery.
    let toml_token = cfg.token.as_deref().filter(|s| !s.trim().is_empty());
    let env_token = std::env::var(DASHBOARD_TOKEN_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty());
    if toml_token.is_none() && env_token.is_none() {
        return Err(ConfigError::DashboardRequiresToken { addr: bind });
    }
    Ok(())
}

fn validate_oh_my_prompt(cfg: &OhMyPromptToml) -> std::result::Result<(), ConfigError> {
    if !cfg.enabled.unwrap_or(false) {
        return Ok(());
    }
    if cfg.endpoint.as_deref().is_none_or(str::is_empty) {
        return Err(ConfigError::OhMyPromptMissingEndpoint);
    }
    Ok(())
}

fn validate_webhook(cfg: &WebhookToml) -> std::result::Result<(), ConfigError> {
    if !cfg.enabled.unwrap_or(false) {
        return Ok(());
    }
    let has_endpoint = cfg.endpoint.as_deref().is_some_and(|s| !s.is_empty());
    let has_endpoint_env = cfg.endpoint_env.as_deref().is_some_and(|s| !s.is_empty());
    if !has_endpoint && !has_endpoint_env {
        return Err(ConfigError::WebhookMissingEndpoint);
    }
    Ok(())
}

/// Walk a `[watch.detail] template` string and yield each placeholder name
/// (or pipe-fallback name) that isn't in [`WATCH_DETAIL_PLACEHOLDERS`].
/// Unbalanced `{` / missing `}` are tolerated silently — the runtime
/// renderer treats them as literal text.
fn unknown_detail_placeholders(template: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '{' {
            continue;
        }
        let mut name = String::new();
        let mut closed = false;
        for nc in chars.by_ref() {
            if nc == '}' {
                closed = true;
                break;
            }
            name.push(nc);
        }
        if !closed {
            continue;
        }
        for part in name.split('|') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            if !WATCH_DETAIL_PLACEHOLDERS.contains(&part) {
                out.push(part.to_string());
            }
        }
    }
    out
}

/// Last delivery mode selected in the `muxa watch` collaboration composer.
/// Unlike [`crate::collaboration::WorkMode`], this includes `just_send`,
/// which is a watch-only keystroke path rather than a durable request.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WatchCollaborationMode {
    #[default]
    ReadOnly,
    Execute,
    JustSend,
}

/// `[watch]` config — controls the `muxa watch` TUI columns.
///
/// Validation of column keys and width specs happens lazily at render time
/// (in the watch crate) so that an unknown key warns rather than refuses to
/// start. See `watch::WatchColumn::from_key` for the canonical key list.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WatchConfig {
    /// Optional visual preset override for the `muxa watch` terminal UI.
    /// When omitted, watch inherits `[ui].theme`.
    pub theme: Option<WatchTheme>,
    /// Columns to display, in order. Omitted keys are hidden.
    pub columns: Vec<String>,
    /// Per-column width override. Keys are column keys; values are either
    /// a TOML integer (fixed length) or a string of the form `min:N` /
    /// `pct:N`. Missing keys fall back to the column's built-in default.
    pub widths: HashMap<String, WidthSpec>,
    /// Table/inspector split for `muxa watch`, persisted when `|` cycles
    /// it: `"50/50"`, `"70/30"` or `"30/70"`. Unknown values fall back to
    /// the default.
    #[serde(default)]
    pub inspector_split: Option<String>,
    /// Last request kind selected in the `m` composer. Written by watch when
    /// `Tab` changes the badge so the next composer (and next watch process)
    /// starts from the same contract.
    #[serde(default)]
    pub collaboration_kind: Option<RequestKind>,
    /// Last delivery mode selected in the `m` composer. Written by watch when
    /// `Ctrl-E` changes the badge.
    #[serde(default)]
    pub collaboration_mode: Option<WatchCollaborationMode>,
    /// Default expansion depth for `muxa watch`. Defaults to `window`.
    pub view: WatchView,
    /// Presentation of the same canonical topology. Defaults to `tree`.
    #[serde(default)]
    pub layout: WatchLayout,
    /// How tree children are revealed. `focus` keeps only the selected path
    /// open, `always` expands every node through the configured `view`, and
    /// `manual` changes expansion only through the tree navigation keys.
    #[serde(default)]
    pub tree_expansion: WatchTreeExpansion,
    /// Expanded detail line shown under the currently-selected row.
    pub detail: DetailConfig,
    /// Ordered list of sort keys applied to the agent rows in `muxa watch`.
    /// Stale agents (pane closed) always bucket at the end, regardless of
    /// what's listed here.
    ///
    /// Default: `["state", "name", "latest"]` — floats needs-attention
    /// rows (error / input / choice) to the top so a blocked agent is never
    /// buried below busy/idle ones, then sorts siblings by name and recent
    /// activity. `t` (sort by state)
    /// and any user-configured order still take over verbatim.
    pub sort: Vec<WatchSortKey>,
    /// What the summary column shows, and in what priority order.
    ///
    /// Default `recap`: the agent's own session recap when it has one,
    /// else its rolling session title, else the last prompt. Claude Code
    /// writes a recap only when you come back after being away — rich but
    /// sparse — so the title tier keeps the column meaningful in between.
    /// Agents with no recap source (Codex, Gemini) fall straight through
    /// to the last prompt.
    #[serde(default)]
    pub summary: WatchSummary,
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
    /// Animate the state glyph in the `muxa watch` TUI — a spinning braille
    /// dot for `working`, a rotating half-circle for `starting`. Purely a
    /// watch-TUI affordance: `muxa status` and the tmux status-line always
    /// render the static `[ui] icons` glyph. Set `false` for calm static
    /// icons (or a terminal without braille).
    #[serde(default = "default_true")]
    pub spinner: bool,
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

/// Granularity of the `muxa watch` table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchView {
    /// Show session roots; children start collapsed.
    Session,
    /// Show sessions and their windows; pane children start collapsed.
    #[default]
    Window,
    /// Show the complete session → window → pane ancestry.
    Pane,
}

/// Presentation of the canonical watch topology.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchLayout {
    /// Selectable session → window → pane tree.
    #[default]
    Tree,
    /// Dense animated collaboration-room clusters. Node identities and the
    /// requested topology depth remain unchanged.
    Swarm,
}

/// Expansion policy for the selectable watch topology tree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchTreeExpansion {
    /// Accordion-style navigation: the selected session reveals its windows;
    /// in pane view, the selected window also reveals its panes. Moving to a
    /// sibling folds the previous path.
    #[default]
    Focus,
    /// Expand all nodes down to [`WatchConfig::view`], matching the original
    /// canonical-tree presentation.
    Always,
    /// Start collapsed and change expansion only with `h`/`l` or arrows.
    Manual,
}

/// Priority chain for the `muxa watch` summary column.
///
/// Each variant names the *highest* tier it will show; lower tiers stay as
/// fallbacks, so the column is never empty when a last prompt exists.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchSummary {
    /// recap → session title → last prompt. The default: prefer the agent's
    /// own summary of what it's doing, and degrade gracefully.
    #[default]
    Recap,
    /// session title → last prompt. Skips the sparse recap for operators who
    /// want a stable one-liner that changes only when the topic does.
    Title,
    /// last prompt only — the historical pre-recap behavior.
    Prompt,
}

/// Visual preset for muxa's human-facing terminal UIs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WatchTheme {
    /// Existing neutral palette and square borders.
    #[default]
    Classic,
    /// A polished preset: warmer title, rounded chrome, and calmer
    /// selection colors. Named as the first `oh-my-muxa` building block.
    #[serde(alias = "oh_my_muxa")]
    OhMyMuxa,
    /// Low-noise palette that keeps attention on the selected row and
    /// blocking states during long monitoring sessions.
    Focus,
    /// Operational palette that makes waiting/error/rate-limit rows stand
    /// out more aggressively.
    Ops,
    /// Mostly monochrome palette for SSH, logs, screenshots, and terminals
    /// with unreliable color support.
    Mono,
    /// Strong contrast palette for bright terminals and accessibility.
    #[serde(alias = "high_contrast")]
    HighContrast,
    /// Low-decoration preset for dense terminals and screenshots.
    Minimal,
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
    /// Session/window/pane node name, ascending within its siblings.
    Name,
    /// `last_activity_at` descending — most recently updated first. The
    /// useful default to surface "what's moving right now" without
    /// scrolling.
    #[serde(rename = "latest", alias = "activity", alias = "act")]
    Activity,
    /// Semantic agent state priority: error/input/choice rows first, then
    /// working/starting/idle/stopped. This is deliberately operational,
    /// not alphabetical.
    #[serde(alias = "st")]
    State,
    /// Hierarchy-specific duration descending within siblings. Session rows use
    /// attached duration when it is unambiguous, windows use their earliest agent
    /// lifetime, and panes use their current state age.
    #[serde(alias = "dur")]
    Duration,
    /// tmux window index, then pane index, both ascending and parsed
    /// numerically so `10` sorts after `2`. Combined with `Name`, this
    /// reproduces the tmux-native pane ordering.
    Pane,
    /// Raw `pane_id` (e.g. `%42`) lexicographic ascending. Mostly useful
    /// as a stable, predictable order for screenshots / docs.
    PaneId,
}

impl Default for WatchConfig {
    fn default() -> Self {
        // Pane-view defaults: NAME / STATE / ACT / LAST PROMPT — lead with
        // identity, then the current state plus time spent in it, with the
        // variable-width prompt last
        // so it can absorb the remaining width. Child shell/subagent workload
        // is shown in the selected row's detail line by default; users can
        // opt back into an always-visible `workload` column if they prefer.
        // Work view folds state counts into the WORKSPACE › WORK label and
        // swaps ST for DUR
        // at render setup time. Users who care about model/ctx/cost can
        // opt back in via config.
        let columns = vec![
            "pane".to_string(),
            "state_age".to_string(),
            "activity".to_string(),
            "prompt".to_string(),
        ];
        let mut widths = HashMap::new();
        widths.insert("pane".to_string(), WidthSpec::Length(22));
        widths.insert("state".to_string(), WidthSpec::Length(3));
        widths.insert("state_age".to_string(), WidthSpec::Length(12));
        widths.insert("prompt".to_string(), WidthSpec::Min(20));
        widths.insert("activity".to_string(), WidthSpec::Length(6));
        widths.insert("workload".to_string(), WidthSpec::Length(8));
        widths.insert("duration".to_string(), WidthSpec::Length(6));
        Self {
            theme: None,
            columns,
            inspector_split: None,
            collaboration_kind: None,
            collaboration_mode: None,
            widths,
            view: WatchView::Window,
            layout: WatchLayout::Tree,
            tree_expansion: WatchTreeExpansion::Focus,
            summary: WatchSummary::default(),
            detail: DetailConfig::default(),
            // Lead with State so needs-attention rows (error / input /
            // choice) float to the top and a blocked agent is never buried
            // below busy/idle ones. Then sort siblings by name and bring the most
            // recently active node in each group to the top — covers "who
            // needs me", "what's moving", and "where is it" at a glance,
            // without losing the grouping shipped in c9a6572. Users who
            // preferred name-first order can set
            // `sort = ["name", "latest"]`.
            sort: vec![
                WatchSortKey::State,
                WatchSortKey::Name,
                WatchSortKey::Activity,
            ],
            hide_paneless: true,
            spinner: true,
            preview: PreviewConfig::default(),
        }
    }
}

/// `[watch.detail]` — the second visual line rendered under the selected
/// row. Useful for glancing at the full last-prompt (or any other field)
/// without leaving the picker.
///
/// `template` is interpolated with `{name}` placeholders. Supported names:
/// `pane`, `kind`, `state`, `model`, `ctx`, `cost`, `activity`, `workload`,
/// `last_prompt`, `last_response`, `last_notification`, `cwd`,
/// `rate_limit`, `rate_limit_resets_at`, `rate_limit_scope`. Unknown
/// placeholders are preserved verbatim.
///
/// **Rate-limit placeholder formats**:
/// * `{rate_limit}` mirrors the LIMITS column — human-friendly, e.g.
///   `⛔ 5h in 2h 14m`, `5h 84%`, or `-`.
/// * `{rate_limit_resets_at}` is RFC 3339 (e.g. `2026-04-30T12:30:00Z`)
///   so users wiring the detail line into scripts get a machine-readable
///   timestamp; the column already covers the at-a-glance case.
/// * `{rate_limit_scope}` is `5h` / `7d` / `unknown` / `-`.
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
            // Prefer `last_notification` — the message the agent explicitly
            // pushed ("approve permission to run X?", a choice prompt, an
            // error line) — because that's the "why does this need me"
            // context the PROMPT column and preview overlay otherwise hide.
            // It's populated almost exclusively on blocked/error rows, so
            // for working/idle rows it's empty and the chain falls through
            // to the assistant's last response, then to the user's last
            // prompt when no response has been captured yet (older agents
            // that pre-date transcript tailing, agents mid-turn, or hooks
            // that fire `PromptSubmitted` without ever reaching
            // `TurnStopped`). Without the fallbacks the detail row vanishes
            // entirely in those cases, which reads as "the feature is
            // broken" rather than "no response yet".
            //
            // Pipe-separated alternatives in `format_detail` resolve
            // left-to-right and pick the first non-dash value. Users who
            // want both visible can override with e.g.
            // `template = "{last_prompt} → {last_response}"`.
            template: "{last_notification|last_response|last_prompt}".to_string(),
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
        assert_eq!(cfg.ask.permission_mode, AskPermissionMode::Bypass);
        assert_eq!(cfg.ask.timeout_secs, DEFAULT_ASK_TIMEOUT_SECS);
    }

    #[test]
    fn parses_route_owned_prepare_command() {
        let cfg: Config = toml::from_str(
            r#"
[[route]]
match = "^cal-"
cwd = "~/workspace-agent/{{id}}"
prepare = "workspace-tool create {{id}} {{ticket.branch}}"
"#,
        )
        .unwrap();
        assert_eq!(
            cfg.route[0].prepare.as_deref(),
            Some("workspace-tool create {{id}} {{ticket.branch}}")
        );
    }

    #[test]
    fn parses_ask_permission_mode_and_additional_dirs() {
        let cfg: Config = toml::from_str(
            r#"
[ask]
enabled = true
permission_mode = "bypass"
additional_dirs = ["/nfs/home/june", "/srv/shared"]
"#,
        )
        .unwrap();
        assert_eq!(cfg.ask.permission_mode, AskPermissionMode::Bypass);
        assert_eq!(
            cfg.ask.additional_dirs,
            vec![
                PathBuf::from("/nfs/home/june"),
                PathBuf::from("/srv/shared")
            ]
        );
    }

    #[test]
    fn parses_sorted_message_skills() {
        let cfg: Config = toml::from_str(
            r#"
[message.skills]
summarize = "summarize our changes"
agent-review = "create a codex pane and pass our changes for review"
"리뷰" = "다른 에이전트에게 리뷰를 요청해"
"#,
        )
        .unwrap();
        assert_eq!(
            cfg.message.skills.keys().cloned().collect::<Vec<_>>(),
            vec!["agent-review", "summarize", "리뷰"]
        );
        assert_eq!(
            cfg.message.skills.get("agent-review").map(String::as_str),
            Some("create a codex pane and pass our changes for review")
        );
    }

    #[test]
    fn discovery_can_be_disabled() {
        let cfg: Config = toml::from_str("[discovery]\nenabled = false\n").unwrap();
        assert!(!cfg.discovery.enabled);
    }

    #[test]
    fn screen_detect_defaults_on_at_3s() {
        let cfg = Config::default();
        assert!(cfg.screen_detect.enabled);
        assert_eq!(cfg.screen_detect.interval_secs, 3);
        // Parsed from an empty document, the defaults still apply.
        let empty: Config = toml::from_str("").unwrap();
        assert!(empty.screen_detect.enabled);
        assert_eq!(empty.screen_detect.interval_secs, 3);
    }

    #[test]
    fn screen_detect_can_be_configured() {
        let cfg: Config =
            toml::from_str("[screen_detect]\nenabled = false\ninterval_secs = 10\n").unwrap();
        assert!(!cfg.screen_detect.enabled);
        assert_eq!(cfg.screen_detect.interval_secs, 10);
    }

    #[test]
    fn collaboration_is_opt_in_and_parses_wake_policy() {
        assert!(!Config::default().collaboration.enabled);
        let cfg: Config = toml::from_str(
            "[collaboration]\nenabled = true\nwake = \"never\"\nwake_payload = \"full\"\nmax_message_bytes = 4096\n",
        )
        .unwrap();
        assert!(cfg.collaboration.enabled);
        assert_eq!(cfg.collaboration.wake, CollaborationWake::Never);
        assert_eq!(
            cfg.collaboration.wake_payload,
            CollaborationWakePayload::Full
        );
        assert_eq!(cfg.collaboration.max_message_bytes, 4096);

        let defaults: Config = toml::from_str("[collaboration]\nenabled = true\n").unwrap();
        assert_eq!(
            defaults.collaboration.wake_payload,
            CollaborationWakePayload::Notice
        );
    }

    #[test]
    fn discovery_interval_defaults_to_30s() {
        let cfg = Config::default();
        assert_eq!(cfg.discovery.interval_secs, 30);
        let parsed: Config = toml::from_str("[discovery]\ninterval_secs = 5\n").unwrap();
        assert_eq!(parsed.discovery.interval_secs, 5);
    }

    #[test]
    fn rejects_unknown_fields() {
        let err = toml::from_str::<Config>("unknown_field = 1").unwrap_err();
        assert!(err.to_string().contains("unknown"));
    }

    #[test]
    fn watch_default_is_prompt_forward() {
        let cfg = WatchConfig::default();
        assert_eq!(cfg.theme, None);
        assert_eq!(cfg.columns, vec!["pane", "state_age", "activity", "prompt"]);
        assert!(matches!(
            cfg.widths.get("pane"),
            Some(WidthSpec::Length(22))
        ));
        assert!(matches!(
            cfg.widths.get("state"),
            Some(WidthSpec::Length(3))
        ));
        assert!(matches!(
            cfg.widths.get("state_age"),
            Some(WidthSpec::Length(12))
        ));
        assert!(cfg
            .columns
            .iter()
            .all(|column| WATCH_COLUMN_KEYS.contains(&column.as_str())));
        assert!(matches!(cfg.widths.get("prompt"), Some(WidthSpec::Min(20))));
        assert!(matches!(
            cfg.widths.get("activity"),
            Some(WidthSpec::Length(6))
        ));
        assert!(matches!(
            cfg.widths.get("workload"),
            Some(WidthSpec::Length(8))
        ));
    }

    #[test]
    fn ui_theme_defaults_to_classic() {
        let cfg = Config::default();
        assert_eq!(cfg.ui.theme, WatchTheme::Classic);
    }

    #[test]
    fn parses_watch_section() {
        let toml = r#"
[watch]
theme = "oh-my-muxa"
columns = ["pane", "prompt"]

[watch.widths]
pane = 30
prompt = "min:40"
ratio = "pct:25"
broken = "what"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.watch.theme, Some(WatchTheme::OhMyMuxa));
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
    fn parses_ui_theme() {
        let cfg: Config = toml::from_str("[ui]\ntheme = \"focus\"\n").unwrap();
        assert_eq!(cfg.ui.theme, WatchTheme::Focus);
        assert_eq!(cfg.watch.theme, None);
    }

    #[test]
    fn detail_defaults_to_notification_then_response_then_prompt_template() {
        let cfg = WatchConfig::default();
        assert!(cfg.detail.enabled);
        assert_eq!(
            cfg.detail.template,
            "{last_notification|last_response|last_prompt}"
        );
    }

    #[test]
    fn watch_sort_default_leads_with_state_then_name_then_activity() {
        let cfg = WatchConfig::default();
        assert_eq!(
            cfg.sort,
            vec![
                WatchSortKey::State,
                WatchSortKey::Name,
                WatchSortKey::Activity
            ]
        );
    }

    #[test]
    fn parses_watch_sort_section() {
        let toml = r#"
[watch]
sort = ["latest"]
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.watch.sort, vec![WatchSortKey::Activity]);
    }

    #[test]
    fn parses_watch_sort_aliases_for_cli_column_names() {
        let toml = r#"
[watch]
sort = ["activity", "act", "latest", "st", "dur", "duration"]
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.watch.sort,
            vec![
                WatchSortKey::Activity,
                WatchSortKey::Activity,
                WatchSortKey::Activity,
                WatchSortKey::State,
                WatchSortKey::Duration,
                WatchSortKey::Duration,
            ]
        );
    }

    #[test]
    fn serializes_activity_sort_as_latest() {
        #[derive(Serialize)]
        struct SortOnly {
            sort: Vec<WatchSortKey>,
        }

        let rendered = toml::to_string(&SortOnly {
            sort: vec![WatchSortKey::Name, WatchSortKey::Activity],
        })
        .unwrap();
        assert_eq!(rendered.trim(), "sort = [\"name\", \"latest\"]");
    }

    #[test]
    fn parses_watch_window_view() {
        let cfg: Config = toml::from_str("[watch]\nview = \"window\"\n").unwrap();
        assert_eq!(cfg.watch.view, WatchView::Window);
    }

    /// The default view is `window` — the fleet-at-a-glance granularity.
    /// Both the struct default and a config that omits `view` resolve to it,
    /// so an empty `~/.config/muxa/config.toml` lands on window view.
    #[test]
    fn default_watch_view_is_window() {
        assert_eq!(WatchConfig::default().view, WatchView::Window);
        assert_eq!(WatchView::default(), WatchView::Window);
        let cfg: Config = toml::from_str("[watch]\n").unwrap();
        assert_eq!(cfg.watch.view, WatchView::Window);
    }

    #[test]
    fn parses_watch_layout_independently_from_view() {
        let cfg: Config = toml::from_str("[watch]\nview = \"pane\"\nlayout = \"swarm\"\n").unwrap();
        assert_eq!(cfg.watch.view, WatchView::Pane);
        assert_eq!(cfg.watch.layout, WatchLayout::Swarm);
    }

    #[test]
    fn watch_tree_expansion_defaults_to_focus_and_parses_all_policies() {
        assert_eq!(
            WatchConfig::default().tree_expansion,
            WatchTreeExpansion::Focus
        );
        for (raw, expected) in [
            ("focus", WatchTreeExpansion::Focus),
            ("always", WatchTreeExpansion::Always),
            ("manual", WatchTreeExpansion::Manual),
        ] {
            let cfg: Config =
                toml::from_str(&format!("[watch]\ntree_expansion = \"{raw}\"\n")).unwrap();
            assert_eq!(cfg.watch.tree_expansion, expected);
        }
    }

    /// `pane` is now the opt-in (non-default) granularity; setting it
    /// explicitly still parses.
    #[test]
    fn parses_watch_pane_view() {
        let cfg: Config = toml::from_str("[watch]\nview = \"pane\"\n").unwrap();
        assert_eq!(cfg.watch.view, WatchView::Pane);
    }

    #[test]
    fn watch_theme_accepts_aliases() {
        for (raw, expected) in [
            ("oh-my-muxa", WatchTheme::OhMyMuxa),
            ("oh_my_muxa", WatchTheme::OhMyMuxa),
            ("focus", WatchTheme::Focus),
            ("ops", WatchTheme::Ops),
            ("mono", WatchTheme::Mono),
            ("high-contrast", WatchTheme::HighContrast),
            ("high_contrast", WatchTheme::HighContrast),
            ("minimal", WatchTheme::Minimal),
        ] {
            let cfg: Config = toml::from_str(&format!("[watch]\ntheme = \"{raw}\"\n")).unwrap();
            assert_eq!(cfg.watch.theme, Some(expected));
        }
    }

    #[test]
    fn parses_watch_sort_with_multiple_keys() {
        let toml = r#"
[watch]
sort = ["name", "pane"]
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.watch.sort, vec![WatchSortKey::Name, WatchSortKey::Pane]);
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
    fn legacy_workspace_sort_names_are_not_accepted() {
        for key in ["workspace", "workspace_time"] {
            let toml = format!("[watch]\nsort = [\"{key}\"]\n");
            assert!(toml::from_str::<Config>(&toml).is_err(), "accepted {key}");
        }
    }

    #[test]
    fn legacy_work_and_swarm_view_names_are_not_accepted() {
        for view in ["work", "swarm"] {
            let toml = format!("[watch]\nview = \"{view}\"\n");
            assert!(toml::from_str::<Config>(&toml).is_err(), "accepted {view}");
        }
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
    /// `{last_notification|last_response|last_prompt}` fallback template).
    /// The `default = WatchConfig::default` machinery on the parent struct
    /// must kick in.
    #[test]
    fn missing_watch_detail_section_uses_defaults() {
        let toml = r#"
[watch]
columns = ["pane", "prompt"]
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(cfg.watch.detail.enabled);
        assert_eq!(
            cfg.watch.detail.template,
            "{last_notification|last_response|last_prompt}"
        );
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
        assert_eq!(
            cfg.watch.detail.template,
            "{last_notification|last_response|last_prompt}"
        );
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

    // ---- semantic validation (Config::validate_for_daemon) ----

    /// A non-loopback `dashboard.bind` without `allow_public = true` must
    /// fail validation with a `DashboardRequiresAllowPublic` error so the
    /// user sees a deliberate "you need to opt in" message at load time
    /// instead of a runtime crash.
    #[test]
    fn validate_rejects_non_loopback_bind_without_allow_public() {
        let cfg = Config {
            dashboard: DashboardTomlConfig {
                bind: Some("0.0.0.0:7878".into()),
                ..DashboardTomlConfig::default()
            },
            ..Config::default()
        };
        let err = cfg.validate_for_daemon().unwrap_err();
        assert!(
            matches!(err, ConfigError::DashboardRequiresAllowPublic { .. }),
            "got {err:?}",
        );
    }

    /// `allow_public = true` alone is not enough — a token (TOML or env)
    /// is also required for non-loopback binds.
    #[test]
    fn validate_rejects_non_loopback_bind_without_token() {
        // Make sure the env-var fallback isn't masking the intent of the
        // test. `remove_var` is unsafe-by-spec under multi-threading; the
        // test runner itself is multi-threaded, so we set a known-empty
        // string instead. Empty strings are treated as unset by the
        // validator (matching `DashboardConfig::resolve`).
        std::env::set_var(DASHBOARD_TOKEN_ENV, "");

        let cfg = Config {
            dashboard: DashboardTomlConfig {
                bind: Some("0.0.0.0:7878".into()),
                allow_public: Some(true),
                ..DashboardTomlConfig::default()
            },
            ..Config::default()
        };
        let err = cfg.validate_for_daemon().unwrap_err();
        assert!(
            matches!(err, ConfigError::DashboardRequiresToken { .. }),
            "got {err:?}",
        );
    }

    /// Non-loopback bind + `allow_public = true` + non-empty token →
    /// validation passes. The happy path for a publicly-bound dashboard.
    #[test]
    fn validate_accepts_non_loopback_bind_with_allow_public_and_token() {
        let cfg = Config {
            dashboard: DashboardTomlConfig {
                bind: Some("0.0.0.0:7878".into()),
                allow_public: Some(true),
                token: Some("s3cret".into()),
                ..DashboardTomlConfig::default()
            },
            ..Config::default()
        };
        assert!(cfg.validate_for_daemon().is_ok());
    }

    #[test]
    fn validate_accepts_non_loopback_bind_with_explicit_auth_none() {
        std::env::set_var(DASHBOARD_TOKEN_ENV, "");

        let cfg = Config {
            dashboard: DashboardTomlConfig {
                bind: Some("0.0.0.0:7878".into()),
                allow_public: Some(true),
                auth: Some(DashboardAuthMode::None),
                ..DashboardTomlConfig::default()
            },
            ..Config::default()
        };
        assert!(cfg.validate_for_daemon().is_ok());
    }

    /// A malformed `dashboard.bind` ("not-an-address") fails validation
    /// with a parse-error variant rather than passing through to a late
    /// `SocketAddr::parse()` blow-up.
    #[test]
    fn validate_rejects_unparseable_dashboard_bind() {
        let cfg = Config {
            dashboard: DashboardTomlConfig {
                bind: Some("not-an-address".into()),
                ..DashboardTomlConfig::default()
            },
            ..Config::default()
        };
        let err = cfg.validate_for_daemon().unwrap_err();
        assert!(
            matches!(err, ConfigError::InvalidDashboardBind { .. }),
            "got {err:?}",
        );
    }

    /// Loopback default bind passes validation with no token/no
    /// `allow_public` — the laptop-default case must keep working.
    #[test]
    fn validate_accepts_default_loopback_dashboard() {
        let cfg = Config::default();
        assert!(cfg.validate_for_daemon().is_ok());
    }

    /// IPv6 loopback (`[::1]`) must be treated identically to `127.0.0.1`
    /// — no token / `allow_public` required. `Ipv6Addr::is_loopback`
    /// already returns true for `::1`, but we lock it down here so a
    /// future refactor that mishandles the v4/v6 split can't silently
    /// break the laptop-default case for v6-first hosts.
    #[test]
    fn validate_accepts_ipv6_loopback_bind() {
        let cfg = Config {
            dashboard: DashboardTomlConfig {
                bind: Some("[::1]:7878".into()),
                ..DashboardTomlConfig::default()
            },
            ..Config::default()
        };
        assert!(cfg.validate_for_daemon().is_ok());
    }

    /// IPv6 unspecified (`[::]`) is the v6 equivalent of `0.0.0.0` — it
    /// binds every interface — and must be rejected without
    /// `allow_public`. Otherwise a user typing `[::]:7878` thinking
    /// "loopback by another name" would silently get a publicly-exposed
    /// dashboard.
    #[test]
    fn validate_rejects_ipv6_unspecified_bind_without_allow_public() {
        let cfg = Config {
            dashboard: DashboardTomlConfig {
                bind: Some("[::]:7878".into()),
                ..DashboardTomlConfig::default()
            },
            ..Config::default()
        };
        let err = cfg.validate_for_daemon().unwrap_err();
        assert!(
            matches!(err, ConfigError::DashboardRequiresAllowPublic { .. }),
            "got {err:?}",
        );
    }

    /// Whitespace-only tokens (`"   "`) are treated as unset, the same
    /// way empty strings already are. A token that's all spaces would
    /// pass a naive "non-empty" check but never authenticate any real
    /// client — surface that as a config-time error rather than a
    /// runtime mystery.
    #[test]
    fn validate_rejects_whitespace_only_token() {
        std::env::set_var(DASHBOARD_TOKEN_ENV, "");
        let cfg = Config {
            dashboard: DashboardTomlConfig {
                bind: Some("0.0.0.0:7878".into()),
                allow_public: Some(true),
                token: Some("   ".into()),
                ..DashboardTomlConfig::default()
            },
            ..Config::default()
        };
        let err = cfg.validate_for_daemon().unwrap_err();
        assert!(
            matches!(err, ConfigError::DashboardRequiresToken { .. }),
            "got {err:?}",
        );
    }

    /// `[sinks.oh_my_prompt] enabled = true` with no endpoint must fail
    /// at load — there is no default endpoint by design.
    #[test]
    fn validate_rejects_oh_my_prompt_enabled_without_endpoint() {
        let cfg = Config {
            sinks: SinksConfig {
                oh_my_prompt: OhMyPromptToml {
                    enabled: Some(true),
                    ..OhMyPromptToml::default()
                },
                ..SinksConfig::default()
            },
            ..Config::default()
        };
        let err = cfg.validate_for_daemon().unwrap_err();
        assert!(
            matches!(err, ConfigError::OhMyPromptMissingEndpoint),
            "got {err:?}",
        );
    }

    /// `enabled = true` with a non-empty endpoint passes validation. The
    /// token check happens later in `OhMyPromptSink::resolve` (env var
    /// only — not part of the TOML invariant) and is intentionally not
    /// duplicated here.
    #[test]
    fn validate_accepts_oh_my_prompt_enabled_with_endpoint() {
        let cfg = Config {
            sinks: SinksConfig {
                oh_my_prompt: OhMyPromptToml {
                    enabled: Some(true),
                    endpoint: Some("https://example.dev".into()),
                    ..OhMyPromptToml::default()
                },
                ..SinksConfig::default()
            },
            ..Config::default()
        };
        assert!(cfg.validate_for_daemon().is_ok());
    }

    /// `enabled = false` with an empty endpoint is fine — disabled sinks
    /// don't need to resolve.
    #[test]
    fn validate_accepts_disabled_oh_my_prompt_without_endpoint() {
        let cfg = Config::default();
        assert!(cfg.validate_for_daemon().is_ok());
    }

    /// `[sinks.webhook] enabled = true` with neither endpoint nor
    /// `endpoint_env` set must fail at load — there is no default URL,
    /// and a sink that can't deliver alerts is worse than no sink at
    /// all (operator thinks they're being watched).
    #[test]
    fn validate_rejects_webhook_enabled_without_endpoint() {
        let cfg = Config {
            sinks: SinksConfig {
                webhook: WebhookToml {
                    enabled: Some(true),
                    ..WebhookToml::default()
                },
                ..SinksConfig::default()
            },
            ..Config::default()
        };
        let err = cfg.validate_for_daemon().unwrap_err();
        assert!(
            matches!(err, ConfigError::WebhookMissingEndpoint),
            "got {err:?}",
        );
    }

    /// Either `endpoint` or `endpoint_env` is sufficient. We don't
    /// validate the env var contents here because that's a daemon-runtime
    /// concern — the env var may be populated only inside the unit
    /// `Environment=` and the user's interactive shell wouldn't see it.
    #[test]
    fn validate_accepts_webhook_with_endpoint() {
        let cfg = Config {
            sinks: SinksConfig {
                webhook: WebhookToml {
                    enabled: Some(true),
                    endpoint: Some("https://hooks.slack.com/services/T0/B0/x".into()),
                    ..WebhookToml::default()
                },
                ..SinksConfig::default()
            },
            ..Config::default()
        };
        assert!(cfg.validate_for_daemon().is_ok());
    }

    #[test]
    fn validate_accepts_webhook_with_endpoint_env_only() {
        let cfg = Config {
            sinks: SinksConfig {
                webhook: WebhookToml {
                    enabled: Some(true),
                    endpoint_env: Some("MUXA_SLACK_URL".into()),
                    ..WebhookToml::default()
                },
                ..SinksConfig::default()
            },
            ..Config::default()
        };
        assert!(cfg.validate_for_daemon().is_ok());
    }

    /// Webhook section parses with the documented field set and
    /// `deny_unknown_fields` rejects typos.
    #[test]
    fn parses_webhook_section() {
        let toml = r#"
[sinks.webhook]
enabled = true
endpoint = "https://hooks.slack.com/services/T0/B0/x"
flavor = "slack"
on_states = ["WaitingInput", "Error"]
rate_limit_secs = 30
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.sinks.webhook.enabled, Some(true));
        assert_eq!(cfg.sinks.webhook.flavor.as_deref(), Some("slack"));
        assert_eq!(cfg.sinks.webhook.rate_limit_secs, Some(30));
    }

    #[test]
    fn rejects_unknown_field_in_webhook_section() {
        let toml = r#"
[sinks.webhook]
enabled = true
endpoint = "https://example.com"
typoed_field = 1
"#;
        assert!(toml::from_str::<Config>(toml).is_err());
    }

    /// CLI commands like `muxa watch` only call `Config::validate()`,
    /// never `validate_for_daemon()`. A config that's *only*
    /// daemon-misconfigured (e.g. `dashboard.bind = "0.0.0.0:7878"` with
    /// no token, intended for a future `muxad --dashboard` run, or a
    /// stale leftover from a test) must NOT block the CLI from loading
    /// — `muxa status` / `muxa recap` / `muxa watch` don't even touch
    /// the dashboard. This test pins that gating: the daemon validator
    /// rejects, the CLI validator accepts.
    #[test]
    fn validate_accepts_config_that_validate_for_daemon_rejects() {
        std::env::set_var(DASHBOARD_TOKEN_ENV, "");
        let cfg = Config {
            dashboard: DashboardTomlConfig {
                bind: Some("0.0.0.0:7878".into()),
                ..DashboardTomlConfig::default()
            },
            sinks: SinksConfig {
                oh_my_prompt: OhMyPromptToml {
                    enabled: Some(true),
                    ..OhMyPromptToml::default()
                },
                ..SinksConfig::default()
            },
            ..Config::default()
        };
        // CLI path: load + validate must succeed. The CLI doesn't open
        // the dashboard or instantiate sinks, so dashboard-only and
        // sink-only misconfig are harmless.
        assert!(cfg.validate().is_ok());
        // Daemon path: same config must fail — the daemon would try to
        // bind a public socket without a token.
        assert!(cfg.validate_for_daemon().is_err());
    }

    /// Unknown `[watch] columns` keys are NOT a hard error — they parse,
    /// validate, and survive verbatim on the resolved struct so the
    /// render layer can warn-and-skip. We log a warning in
    /// `warn_soft_issues` (not asserted here without `tracing-test` in
    /// deps; the behavior contract is "load succeeds, value preserved").
    #[test]
    fn validate_allows_unknown_watch_columns_key_with_warning() {
        let toml = r#"
[watch]
columns = ["pane", "definitely_not_real", "prompt"]
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        // Hard validation passes.
        assert!(cfg.validate().is_ok());
        // Soft-warn pass also doesn't error.
        cfg.warn_soft_issues();
        // Unknown key is preserved verbatim on the struct so the renderer
        // can decide what to do at render time.
        assert!(cfg.watch.columns.iter().any(|c| c == "definitely_not_real"));
    }

    /// `[watch.detail] template` with an unknown placeholder loads fine
    /// and preserves the placeholder verbatim. Render-time behavior
    /// (literal pass-through) is unchanged; only a warning is added.
    #[test]
    fn validate_allows_unknown_detail_placeholder_with_warning() {
        let toml = r#"
[watch.detail]
template = "{nope} {last_prompt}"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(cfg.validate().is_ok());
        cfg.warn_soft_issues();
        assert_eq!(cfg.watch.detail.template, "{nope} {last_prompt}");
    }

    /// The placeholder scanner must accept every name the renderer knows
    /// about — otherwise a perfectly legal default template would warn.
    #[test]
    fn unknown_detail_placeholders_recognizes_default_template() {
        // Default template uses a pipe fallback; both names are legal.
        let unknown = unknown_detail_placeholders("{last_response|last_prompt}");
        assert!(unknown.is_empty(), "got {unknown:?}");
    }

    /// `unknown_detail_placeholders` must flag each unknown name in a
    /// pipe chain individually, not just the first.
    #[test]
    fn unknown_detail_placeholders_flags_each_unknown_alternative() {
        let unknown = unknown_detail_placeholders("{nope|also_nope|last_prompt}");
        assert_eq!(unknown, vec!["nope".to_string(), "also_nope".to_string()]);
    }

    #[test]
    fn parses_fleet_inventory_labels_annotations_and_policy() {
        let input = r#"
[fleet]
enabled = true
refresh_secs = 12
keepalive_secs = 5
offline_after_secs = 15

[fleet.hosts.dev]
ssh = "devbox"
mode = "control"
connect = "on_demand"

[fleet.hosts.dev.labels]
"muxa.dev/environment" = "development"
accelerator = "gpu"

[fleet.hosts.dev.annotations]
"muxa.dev/owner" = "June <june@example.com>"
"#;
        let cfg: Config = toml::from_str(input).unwrap();
        cfg.validate().unwrap();
        assert!(cfg.fleet.enabled);
        assert_eq!(cfg.fleet.hosts["dev"].mode, HostAccessMode::Control);
        assert_eq!(cfg.fleet.hosts["dev"].connect, FleetConnectPolicy::OnDemand);
        assert_eq!(cfg.fleet.hosts["dev"].labels["accelerator"], "gpu");
        assert_eq!(
            cfg.fleet.hosts["dev"].annotations["muxa.dev/owner"],
            "June <june@example.com>"
        );
    }

    #[test]
    fn fleet_validation_rejects_unsafe_ssh_and_remote_shell_tokens() {
        let mut cfg = Config::default();
        cfg.fleet.hosts.insert(
            "dev".into(),
            FleetHostConfig {
                ssh: "-oProxyCommand=bad".into(),
                ..FleetHostConfig::default()
            },
        );
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::InvalidFleet { .. })
        ));
        cfg.fleet.hosts.get_mut("dev").unwrap().ssh = "devbox".into();
        cfg.fleet.hosts.get_mut("dev").unwrap().muxa_path = "muxa;bad".into();
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::InvalidFleet { .. })
        ));
    }

    #[test]
    fn fleet_validation_requires_two_keepalive_windows_before_offline() {
        let mut cfg = Config::default();
        cfg.fleet.keepalive_secs = 10;
        cfg.fleet.offline_after_secs = 19;
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::InvalidFleet { .. })
        ));
        cfg.fleet.offline_after_secs = 20;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn fleet_local_metadata_is_independent_of_remote_enablement() {
        let input = r#"
[fleet]
enabled = false

[fleet.local.labels]
environment = "development"

[fleet.local.annotations]
"muxa.dev/owner" = "June <june@example.com>"
"#;
        let cfg: Config = toml::from_str(input).unwrap();
        cfg.validate().unwrap();
        assert!(!cfg.fleet.enabled);
        assert_eq!(cfg.fleet.local.labels["environment"], "development");
        assert_eq!(
            cfg.fleet.local.annotations["muxa.dev/owner"],
            "June <june@example.com>"
        );
    }

    #[test]
    fn fleet_reserves_local_alias_and_managed_labels() {
        let mut cfg = Config::default();
        cfg.fleet.hosts.insert(
            crate::fleet::LOCAL_HOST_ALIAS.into(),
            FleetHostConfig {
                ssh: "other-machine".into(),
                ..FleetHostConfig::default()
            },
        );
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::InvalidFleet { .. })
        ));

        cfg.fleet.hosts.clear();
        cfg.fleet
            .local
            .labels
            .insert("muxa.io/local".into(), "false".into());
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::InvalidFleet { .. })
        ));
    }
}
