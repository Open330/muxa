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
use std::path::PathBuf;

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
            Action::DeleteFile { .. } | Action::EnableSystemdUnit | Action::DisableSystemdUnit => {
                true
            }
            Action::SourceTmuxConf { .. } | Action::PrintDashboard { .. } => false,
        })
    }
}

pub fn build(direction: Direction, components: &[Component], detect: &Detection) -> Result<Plan> {
    let mut actions = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let comps = components.to_vec();

    let tmux_path = files::tmux::default_path();
    let mut tmux_touched = false;

    for c in &comps {
        match c {
            Component::TmuxPopup | Component::TmuxStatusLine => {
                if plan_tmux(direction, *c, tmux_path.as_ref(), &mut actions)? {
                    tmux_touched = true;
                }
            }
            Component::ClaudeHooks => plan_claude(direction, *c, &mut actions, &mut warnings)?,
            Component::CodexHooks => plan_codex(direction, *c, &mut actions)?,
            Component::GeminiHooks => plan_gemini(direction, *c, &mut actions)?,
            Component::MuxadSystemd => {
                let Some(path) = files::systemd::default_unit_path() else {
                    continue;
                };
                match direction {
                    Direction::Install => {
                        if !detect.systemd_user_available {
                            continue;
                        }
                        let before = read_to_string_opt(&path)?;
                        let (after, outcome) = files::systemd::upsert(before.as_deref());
                        actions.push(Action::EditFile {
                            component: *c,
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
                            actions.push(Action::DeleteFile {
                                component: *c,
                                path,
                            });
                        }
                    }
                }
            }
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

    // Re-source tmux config if we changed it and a tmux server is reachable.
    if tmux_touched {
        if let Some(path) = tmux_path {
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

/// Returns `true` if the tmux config was changed (caller appends a
/// `SourceTmuxConf` action).
fn plan_tmux(
    direction: Direction,
    c: Component,
    tmux_path: Option<&PathBuf>,
    actions: &mut Vec<Action>,
) -> Result<bool> {
    let Some(path) = tmux_path.cloned() else {
        return Ok(false);
    };
    let before = read_to_string_opt(&path)?;
    let original = before.clone().unwrap_or_default();
    let (after, outcome) = match direction {
        Direction::Install => files::tmux::upsert(&original, c),
        Direction::Uninstall => files::tmux::remove(&original, c),
    };
    let changed = outcome.changed();
    actions.push(Action::EditFile {
        component: c,
        path,
        before,
        after,
        outcome,
    });
    Ok(changed)
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

    #[test]
    fn empty_components_yields_empty_plan() {
        let d = Detection::default();
        let plan = build(Direction::Install, &[], &d).unwrap();
        assert!(plan.actions.is_empty());
        assert!(!plan.has_changes());
    }

    #[test]
    fn tmux_install_produces_edit_then_source() {
        // Detection irrelevant for tmux components — they don't gate on it.
        let d = Detection::default();
        let plan = build(Direction::Install, &[Component::TmuxPopup], &d).unwrap();
        // First action is EditFile; if it produced changes, last is SourceTmuxConf.
        assert!(matches!(
            plan.actions.first(),
            Some(Action::EditFile { .. })
        ));
        let last = plan.actions.last().unwrap();
        // SourceTmuxConf appears only if the edit actually changed
        // something. On systems without ~/.tmux.conf it always does
        // (file is created).
        let saw_source = matches!(last, Action::SourceTmuxConf { .. })
            || matches!(plan.actions.last(), Some(Action::EditFile { .. }));
        assert!(saw_source);
    }
}
