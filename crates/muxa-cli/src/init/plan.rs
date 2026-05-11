//! Build an `Plan` (`Vec<Action>`) from a set of components.
//!
//! The plan is computed eagerly: each `Action::EditFile` already
//! carries the new file content. This means dry-run rendering and
//! the apply step share exactly the same bytes, and the network of
//! per-file editors is decoupled from the I/O layer.

use crate::init::components::Component;
use crate::init::detect::Detection;
use crate::init::files;
use crate::init::marker::Outcome;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Direction of a plan: install (upsert) or uninstall (remove).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Install,
    Uninstall,
}

#[derive(Debug)]
pub enum Action {
    /// Write `new_content` to `path`, backing up any existing file.
    /// `outcome` is precomputed so apply.rs can short-circuit
    /// `Outcome::Unchanged` without touching the disk.
    EditFile {
        component: Component,
        path: PathBuf,
        before: Option<String>,
        after: String,
        outcome: Outcome,
    },
    /// Delete a file (currently only the systemd unit on uninstall).
    DeleteFile { component: Component, path: PathBuf },
    /// Run `systemctl --user enable --now muxad.service`.
    EnableSystemdUnit,
    /// Run `systemctl --user disable --now muxad.service`.
    DisableSystemdUnit,
    /// `launchctl bootstrap gui/<uid> <plist>` and kickstart.
    EnableLaunchdUnit { plist_path: PathBuf },
    /// `launchctl bootout gui/<uid>/<label>`.
    DisableLaunchdUnit,
    /// Start `muxad` in the background if it isn't already responding
    /// on the IPC socket. Cross-platform — works regardless of which
    /// (or no) daemon-manager component was selected. Honours
    /// `--start-daemon=false`.
    StartDaemonIfNeeded,
    /// Reload the user's tmux config in place if a tmux server is up.
    /// No-op when not inside tmux + no live server.
    SourceTmuxConf { path: PathBuf },
    /// Print the dashboard URL + token at the end. Captured here so
    /// apply.rs can render it as a final "info" line.
    PrintDashboard { token: String, bind: String },
}

#[derive(Debug)]
pub struct Plan {
    pub direction: Direction,
    pub components: Vec<Component>,
    pub actions: Vec<Action>,
    /// Plan-time warnings — surfaced to the user before they confirm
    /// (e.g. "you have a custom Claude statusLine, leaving it alone").
    pub warnings: Vec<String>,
}

impl Plan {
    /// True if any action would actually mutate the disk.
    pub fn has_changes(&self) -> bool {
        self.actions.iter().any(|a| match a {
            Action::EditFile { outcome, .. } => outcome.changed(),
            Action::DeleteFile { .. }
            | Action::EnableSystemdUnit
            | Action::DisableSystemdUnit
            | Action::EnableLaunchdUnit { .. }
            | Action::DisableLaunchdUnit
            | Action::StartDaemonIfNeeded => true,
            Action::SourceTmuxConf { .. } | Action::PrintDashboard { .. } => false,
        })
    }
}

pub fn build(
    direction: Direction,
    components: &[Component],
    detect: &Detection,
    socket: &Path,
) -> Result<Plan> {
    let mut actions = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let comps = components.to_vec();

    let tmux_path = files::tmux::default_path();
    let mut tmux_selected = false;

    for c in &comps {
        match c {
            Component::TmuxPopup | Component::TmuxStatusLine => {
                tmux_selected = true;
                plan_tmux(direction, *c, tmux_path.as_ref(), &mut actions)?;
            }
            Component::ClaudeHooks => plan_claude(direction, *c, &mut actions, &mut warnings)?,
            Component::CodexHooks => plan_codex(direction, *c, &mut actions)?,
            Component::GeminiHooks => plan_gemini(direction, *c, &mut actions)?,
            Component::MuxadSystemd => plan_systemd(direction, *c, detect, &mut actions)?,
            Component::MuxadLaunchd => plan_launchd(direction, *c, detect, &mut actions)?,
            Component::MuxadShellrc => plan_shellrc(direction, *c, &mut actions)?,
            Component::Dashboard => {
                let Some(path) = files::dashboard::default_path() else {
                    continue;
                };
                let before = read_to_string_opt(&path)?;
                let original = before.clone().unwrap_or_default();
                match direction {
                    Direction::Install => {
                        let (after, outcome, token) = files::dashboard::upsert(&original)
                            .context("writing dashboard token to config.toml")?;
                        actions.push(Action::EditFile {
                            component: *c,
                            path,
                            before,
                            after,
                            outcome,
                        });
                        actions.push(Action::PrintDashboard {
                            token,
                            bind: "127.0.0.1:7878".into(),
                        });
                    }
                    Direction::Uninstall => {
                        let (after, outcome) = files::dashboard::remove(&original)
                            .context("scrubbing dashboard config")?;
                        actions.push(Action::EditFile {
                            component: *c,
                            path,
                            before,
                            after,
                            outcome,
                        });
                    }
                }
            }
        }
    }

    // Whenever any tmux component is selected, also upsert/remove the
    // auto-managed `tmux-env` block that pins MUXA_SOCKET. This is the
    // only path that survives `tmux kill-server` — without it, the
    // runtime `set-environment` issued at init time is lost the next time
    // the tmux server restarts and every fresh pane ends up unable to
    // find muxad. We always include SourceTmuxConf here because the env
    // block may be brand-new even when popup/statusline are unchanged.
    if tmux_selected {
        if let Some(path) = tmux_path {
            plan_tmux_env(direction, &path, socket, &mut actions)?;
            actions.push(Action::SourceTmuxConf { path });
        }
    }

    Ok(Plan {
        direction,
        components: comps,
        actions,
        warnings,
    })
}

/// Append an `EditFile` for the auto-managed `tmux-env` block. Uses the
/// `TmuxStatusLine` component slot only as a label/grouping hint — it
/// isn't user-selectable and doesn't appear in the components catalog.
fn plan_tmux_env(
    direction: Direction,
    tmux_conf: &Path,
    socket: &Path,
    actions: &mut Vec<Action>,
) -> Result<()> {
    let path = tmux_conf.to_path_buf();
    // Re-read tmux.conf from disk fresh: an earlier `plan_tmux` action
    // for popup/statusline only carries its post-edit content in the
    // `Action`, not on disk. Pulling from disk would race with the
    // earlier edit when apply.rs runs sequentially. Instead, fold our
    // change on top of that pending content by replaying the earlier
    // `EditFile` outputs targeting the same path.
    let mut latest = read_to_string_opt(&path)?.unwrap_or_default();
    for action in actions.iter() {
        if let Action::EditFile {
            path: p, after, ..
        } = action
        {
            if p == &path {
                latest = after.clone();
            }
        }
    }
    let (after, outcome) = match direction {
        Direction::Install => files::tmux::upsert_env(&latest, socket),
        Direction::Uninstall => files::tmux::remove_env(&latest),
    };
    actions.push(Action::EditFile {
        // Group under TmuxStatusLine for the dry-run label — the env
        // pin is conceptually a sibling of the status-right glyph.
        component: Component::TmuxStatusLine,
        path,
        before: Some(latest),
        after,
        outcome,
    });
    Ok(())
}

fn plan_tmux(
    direction: Direction,
    c: Component,
    tmux_path: Option<&PathBuf>,
    actions: &mut Vec<Action>,
) -> Result<()> {
    let Some(path) = tmux_path.cloned() else {
        return Ok(());
    };
    let before = read_to_string_opt(&path)?;
    let original = before.clone().unwrap_or_default();
    let (after, outcome) = match direction {
        Direction::Install => files::tmux::upsert(&original, c),
        Direction::Uninstall => files::tmux::remove(&original, c),
    };
    actions.push(Action::EditFile {
        component: c,
        path,
        before,
        after,
        outcome,
    });
    Ok(())
}

fn plan_claude(
    direction: Direction,
    c: Component,
    actions: &mut Vec<Action>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let Some(path) = files::claude::default_path() else {
        return Ok(());
    };
    let before = read_to_string_opt(&path)?;
    let original = before.clone().unwrap_or_default();
    let (after, outcome) = match direction {
        Direction::Install => {
            let (text, report) =
                files::claude::upsert(&original).context("merging claude settings.json")?;
            if matches!(
                report.statusline,
                files::claude::StatusLineDecision::SkippedUserOwned
            ) {
                warnings.push(
                    "Claude Code already has a custom statusLine — leaving it alone. \
                     To layer muxa over it, see `muxa hook claude-statusline --forward`."
                        .into(),
                );
            }
            (text, report.outcome)
        }
        Direction::Uninstall => {
            files::claude::remove(&original).context("scrubbing claude settings.json")?
        }
    };
    actions.push(Action::EditFile {
        component: c,
        path,
        before,
        after,
        outcome,
    });
    Ok(())
}

fn plan_codex(direction: Direction, c: Component, actions: &mut Vec<Action>) -> Result<()> {
    let Some(path) = files::codex::default_path() else {
        return Ok(());
    };
    let before = read_to_string_opt(&path)?;
    let original = before.clone().unwrap_or_default();
    let (after, outcome) = match direction {
        Direction::Install => {
            files::codex::upsert(&original).context("merging codex config.toml")?
        }
        Direction::Uninstall => {
            files::codex::remove(&original).context("scrubbing codex config.toml")?
        }
    };
    actions.push(Action::EditFile {
        component: c,
        path,
        before,
        after,
        outcome,
    });
    Ok(())
}

fn plan_gemini(direction: Direction, c: Component, actions: &mut Vec<Action>) -> Result<()> {
    let Some(path) = files::gemini::default_path() else {
        return Ok(());
    };
    let before = read_to_string_opt(&path)?;
    let original = before.clone().unwrap_or_default();
    let (after, outcome) = match direction {
        Direction::Install => {
            files::gemini::upsert(&original).context("merging gemini settings.json")?
        }
        Direction::Uninstall => {
            files::gemini::remove(&original).context("scrubbing gemini settings.json")?
        }
    };
    actions.push(Action::EditFile {
        component: c,
        path,
        before,
        after,
        outcome,
    });
    Ok(())
}

fn plan_systemd(
    direction: Direction,
    c: Component,
    detect: &Detection,
    actions: &mut Vec<Action>,
) -> Result<()> {
    let Some(path) = files::systemd::default_unit_path() else {
        return Ok(());
    };
    match direction {
        Direction::Install => {
            if !detect.systemd_user_available {
                return Ok(());
            }
            let before = read_to_string_opt(&path)?;
            let (after, outcome) = files::systemd::upsert(before.as_deref());
            actions.push(Action::EditFile {
                component: c,
                path,
                before,
                after,
                outcome,
            });
            actions.push(Action::EnableSystemdUnit);
        }
        Direction::Uninstall => {
            actions.push(Action::DisableSystemdUnit);
            if path.is_file() {
                actions.push(Action::DeleteFile { component: c, path });
            }
        }
    }
    Ok(())
}

fn plan_launchd(
    direction: Direction,
    c: Component,
    detect: &Detection,
    actions: &mut Vec<Action>,
) -> Result<()> {
    let Some(path) = files::launchd::default_unit_path() else {
        return Ok(());
    };
    match direction {
        Direction::Install => {
            if !detect.launchctl_available {
                return Ok(());
            }
            let before = read_to_string_opt(&path)?;
            let want = files::launchd::render_plist(&files::launchd::locate_muxad());
            let (after, outcome) = files::launchd::upsert(before.as_deref(), &want);
            actions.push(Action::EditFile {
                component: c,
                path: path.clone(),
                before,
                after,
                outcome,
            });
            actions.push(Action::EnableLaunchdUnit { plist_path: path });
        }
        Direction::Uninstall => {
            actions.push(Action::DisableLaunchdUnit);
            if path.is_file() {
                actions.push(Action::DeleteFile { component: c, path });
            }
        }
    }
    Ok(())
}

fn plan_shellrc(direction: Direction, c: Component, actions: &mut Vec<Action>) -> Result<()> {
    let Some(path) = files::shellrc::default_path() else {
        return Ok(());
    };
    let before = read_to_string_opt(&path)?;
    let original = before.clone().unwrap_or_default();
    let (after, outcome) = match direction {
        Direction::Install => files::shellrc::upsert(&original),
        Direction::Uninstall => files::shellrc::remove(&original),
    };
    actions.push(Action::EditFile {
        component: c,
        path,
        before,
        after,
        outcome,
    });
    Ok(())
}

fn read_to_string_opt(p: &PathBuf) -> Result<Option<String>> {
    match std::fs::read_to_string(p) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::components::Component;

    fn fake_socket() -> PathBuf {
        PathBuf::from("/tmp/muxa-test.sock")
    }

    #[test]
    fn empty_components_yields_empty_plan() {
        let d = Detection::default();
        let plan = build(Direction::Install, &[], &d, &fake_socket()).unwrap();
        assert!(plan.actions.is_empty());
        assert!(!plan.has_changes());
    }

    #[test]
    fn tmux_install_produces_edit_then_env_then_source() {
        // Detection irrelevant for tmux components — they don't gate on it.
        let d = Detection::default();
        let plan = build(
            Direction::Install,
            &[Component::TmuxPopup],
            &d,
            &fake_socket(),
        )
        .unwrap();

        // Expected action order: popup edit → tmux-env edit → source.
        // The env edit guarantees socket propagation lands in conf even
        // when only the popup component was selected.
        assert!(
            matches!(plan.actions.first(), Some(Action::EditFile { .. })),
            "first action must be the popup EditFile"
        );

        let env_after = plan.actions.iter().any(|a| {
            matches!(a, Action::EditFile { after, .. } if after.contains("MUXA_SOCKET"))
        });
        assert!(
            env_after,
            "tmux-env block must be auto-included with any tmux component"
        );

        assert!(
            matches!(plan.actions.last(), Some(Action::SourceTmuxConf { .. })),
            "last action must be SourceTmuxConf so the new env line takes effect live"
        );
    }

    #[test]
    fn tmux_install_pins_provided_socket_path() {
        let d = Detection::default();
        let socket = PathBuf::from("/run/user/501/muxa.sock");
        let plan = build(Direction::Install, &[Component::TmuxStatusLine], &d, &socket).unwrap();
        let pinned = plan.actions.iter().any(|a| {
            matches!(
                a,
                Action::EditFile { after, .. }
                    if after.contains(r#"set-environment -g MUXA_SOCKET "/run/user/501/muxa.sock""#)
            )
        });
        assert!(pinned, "env block must pin the resolved socket path");
    }
}
