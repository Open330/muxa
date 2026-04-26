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
        Self { columns, widths }
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
}
