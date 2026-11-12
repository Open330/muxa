//! The `tend watch` popup, which needs the CLI's sidebar.

use crate::mux_control::{full_popup, Control};

/// Resolve the host and socket ourselves: an existing rmux server's run-shell
/// PATH can point `tmux` at native tmux rather than the rmux shim.
pub fn watch_popup(client: Option<&str>, source: Option<&str>) -> anyhow::Result<()> {
    let executable = std::env::current_exe()?;
    let mut words = vec![executable.into_os_string()];
    let work_pane = source.and_then(|pane| crate::sidebar::watch_source(&Control::Ambient, pane));
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--popup" {
            continue;
        }
        if work_pane.is_some() && arg == "--caller-pane" {
            args.next();
            continue;
        }
        if work_pane.is_some() && arg.to_string_lossy().starts_with("--caller-pane=") {
            continue;
        }
        words.push(arg);
    }
    if let Some(pane) = work_pane {
        words.extend(["--caller-pane".into(), pane.into()]);
    }
    full_popup(&words, client, "99%")
}
