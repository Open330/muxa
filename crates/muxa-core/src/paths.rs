//! XDG-aware default paths.

use std::path::PathBuf;

pub const SOCKET_FILENAME: &str = "muxa.sock";
pub const CONFIG_DIRNAME: &str = "muxa";
pub const CONFIG_FILENAME: &str = "config.toml";

/// Default daemon socket path. Prefers `$XDG_RUNTIME_DIR/muxa.sock`; falls
/// back to `/tmp/muxa-<uid>.sock` when the runtime dir is unset.
pub fn default_socket() -> PathBuf {
    if let Some(dir) = dirs::runtime_dir() {
        return dir.join(SOCKET_FILENAME);
    }
    PathBuf::from(format!("/tmp/muxa-{}.sock", posix_uid()))
}

/// Default config file path: `$XDG_CONFIG_HOME/muxa/config.toml`, falling
/// back to `$HOME/.config/muxa/config.toml`.
pub fn default_config_file() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join(CONFIG_DIRNAME).join(CONFIG_FILENAME))
}

fn posix_uid() -> u32 {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}
