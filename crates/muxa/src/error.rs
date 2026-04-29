//! Typed errors for the core crate.

use std::path::PathBuf;

use crate::config::ConfigError;

pub type Result<T, E = CoreError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("config parse error ({path}): {source}")]
    ConfigParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("config validation error ({path}): {source}")]
    ConfigValidate {
        path: PathBuf,
        #[source]
        source: ConfigError,
    },

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}
