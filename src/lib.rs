pub mod adapter;
pub mod event;
pub mod ipc;
pub mod state;
pub mod tmux;

use std::path::PathBuf;

pub fn default_socket_path() -> PathBuf {
    if let Some(dir) = dirs::runtime_dir() {
        return dir.join("muxa.sock");
    }
    let uid = posix_uid();
    PathBuf::from(format!("/tmp/muxa-{uid}.sock"))
}

fn posix_uid() -> u32 {
    // Avoid pulling in the libc crate just for this.
    // `id -u` is ubiquitous; this path is only hit when $XDG_RUNTIME_DIR is unset.
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}
