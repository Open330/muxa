//! Configuration model.
//!
//! Loaded from TOML. CLI/env-var overrides happen at the binary layer — this
//! module only parses.

use crate::error::{CoreError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Unix socket path. Overrides the XDG default.
    pub socket: Option<PathBuf>,

    pub notifier: NotifierConfig,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_empty_toml() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(!cfg.notifier.enabled);
    }

    #[test]
    fn rejects_unknown_fields() {
        let err = toml::from_str::<Config>("unknown_field = 1").unwrap_err();
        assert!(err.to_string().contains("unknown"));
    }
}
