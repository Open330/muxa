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
            Component::TmuxPopup | Component::TmuxStatusLine | Component::TmuxPeek => {
                tmux_selected = true;
                plan_tmux(direction, *c, tmux_path.as_ref(), &mut actions)?;
            }
            Component::ClaudeHooks => plan_claude(direction, *c, &mut actions, &mut warnings)?,
            Component::CodexHooks => plan_codex(direction, *c, &mut actions)?,
            Component::GeminiHooks => plan_gemini(direction, *c, &mut actions)?,
            Component::OpencodeHooks => plan_opencode(direction, *c, &mut actions)?,
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
            Component::Ask => plan_ask(direction, *c, &mut actions)?,
            Component::Collaboration => {
                let Some(path) = files::collaboration::default_path() else {
                    continue;
                };
                let before = read_to_string_opt(&path)?;
                let original = before.clone().unwrap_or_default();
                let (after, outcome) = match direction {
                    Direction::Install => files::collaboration::upsert(&original)
                        .context("enabling collaboration in config.toml")?,
                    Direction::Uninstall => files::collaboration::remove(&original)
                        .context("disabling collaboration in config.toml")?,
                };
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

    // Whenever any tmux component is selected, reconcile the auto-managed
    // `tmux-env` block that pins MUXA_SOCKET. A *custom* socket needs the
    // pin: it's the only path that survives `tmux kill-server`, and
    // without it the runtime `set-environment` issued at init time is
    // lost the next time the tmux server restarts. The default socket
    // needs no pin at all — see `needs_socket_pin` — so this reconciles
    // in both directions and will scrub a stale pin left by an earlier
    // muxa. We always include SourceTmuxConf here because the env block
    // may be brand-new (or newly removed) even when popup/statusline are
    // unchanged.
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

fn plan_opencode(direction: Direction, c: Component, actions: &mut Vec<Action>) -> Result<()> {
    let Some(path) = files::opencode::default_path() else {
        return Ok(());
    };
    let before = read_to_string_opt(&path)?;
    let original = before.clone().unwrap_or_default();
    let (after, outcome) = match direction {
        Direction::Install => files::opencode::upsert(&original),
        Direction::Uninstall => files::opencode::remove(&original),
    };
    push_edit_or_delete(direction, c, path, before, after, outcome, actions);
    Ok(())
}

/// Push an `EditFile`, but on **uninstall** demote it to a `DeleteFile`
/// when stripping our block leaves the file empty. This keeps us from
/// leaving a zero-byte orphan behind for files muxa created from scratch
/// (the opencode `muxa.ts`, or a codex/gemini config that held nothing
/// but our hooks). An empty remainder is itself proof there was no
/// pre-existing user content to preserve, so this never deletes a file
/// the user had populated by hand.
fn push_edit_or_delete(
    direction: Direction,
    component: Component,
    path: PathBuf,
    before: Option<String>,
    after: String,
    outcome: Outcome,
    actions: &mut Vec<Action>,
) {
    if direction == Direction::Uninstall
        && before.is_some()
        && outcome.changed()
        && after.trim().is_empty()
    {
        actions.push(Action::DeleteFile { component, path });
        return;
    }
    actions.push(Action::EditFile {
        component,
        path,
        before,
        after,
        outcome,
    });
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
        if let Action::EditFile { path: p, after, .. } = action {
            if p == &path {
                latest.clone_from(after);
            }
        }
    }
    let (after, outcome) = match direction {
        Direction::Install if !needs_socket_pin(socket, &muxa::paths::default_socket()) => {
            files::tmux::remove_env(&latest)
        }
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

/// Whether the resolved socket is worth writing into `~/.tmux.conf`.
///
/// Only a socket that differs from [`muxa::paths::default_socket`] is.
/// Every muxa binary a pane runs calls that same function when
/// `MUXA_SOCKET` is unset, so pinning the default tells a pane something
/// it was going to compute anyway — while baking this host's uid
/// (`/tmp/muxa-<uid>.sock`, `/run/user/<uid>/muxa.sock`) into a file
/// people commonly symlink out of a dotfiles repo and share across
/// machines. On the next machine the pin is simply wrong, and it wins
/// over the correct value the binary would have derived.
///
/// A custom socket — `muxad`'s `config.toml` pointing somewhere else —
/// is unguessable, so it still gets pinned. That is the case the block
/// was added for.
///
/// This does not regress the cold-start story the pin was meant to cover.
/// `muxad` injects its own socket into a running tmux server's global
/// environment at startup (`should_heal_tmux_socket_env` in muxad), which
/// handles a tmux server whose environment diverges from the one `muxa
/// init` ran in. Between that heal and the pane's own `default_socket()`,
/// a default-socket pin has no remaining job.
fn needs_socket_pin(socket: &Path, default_socket: &Path) -> bool {
    socket != default_socket
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

/// `[ask]` in config.toml. Same shape as the collaboration planner —
/// both are a single grant toggled in one table.
fn plan_ask(direction: Direction, component: Component, actions: &mut Vec<Action>) -> Result<()> {
    let Some(path) = files::ask::default_path() else {
        return Ok(());
    };
    let before = read_to_string_opt(&path)?;
    let original = before.clone().unwrap_or_default();
    let (after, outcome) = match direction {
        Direction::Install => {
            files::ask::upsert(&original).context("enabling ask in config.toml")?
        }
        Direction::Uninstall => {
            files::ask::remove(&original).context("disabling ask in config.toml")?
        }
    };
    actions.push(Action::EditFile {
        component,
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
    push_edit_or_delete(direction, c, path, before, after, outcome, actions);
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
    push_edit_or_delete(direction, c, path, before, after, outcome, actions);
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

    /// A socket `default_socket()` can never produce — its fallback form
    /// is `/tmp/muxa-<uid>.sock`, always numeric after the dash. Tests
    /// that want the pin planned must use a path like this, or they'd
    /// silently depend on the uid of whoever runs them.
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
        // A custom socket's propagation lands in conf even when only the
        // popup component was selected.
        assert!(
            matches!(plan.actions.first(), Some(Action::EditFile { .. })),
            "first action must be the popup EditFile"
        );

        let env_after = plan
            .actions
            .iter()
            .any(|a| matches!(a, Action::EditFile { after, .. } if after.contains("MUXA_SOCKET")));
        assert!(
            env_after,
            "a custom socket's tmux-env block must be auto-included with any tmux component"
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
        let plan = build(
            Direction::Install,
            &[Component::TmuxStatusLine],
            &d,
            &socket,
        )
        .unwrap();
        let pinned = plan.actions.iter().any(|a| {
            matches!(
                a,
                Action::EditFile { after, .. }
                    if after.contains(r#"set-environment -g MUXA_SOCKET "/run/user/501/muxa.sock""#)
            )
        });
        assert!(pinned, "env block must pin the resolved socket path");
    }

    #[test]
    fn default_socket_is_never_pinned_into_tmux_conf() {
        // The pin's whole value is telling a pane something it can't
        // derive. For the default socket it can, and writing it anyway
        // bakes this host's uid into a file that is routinely symlinked
        // out of a dotfiles repo onto other machines.
        let d = Detection::default();
        let plan = build(
            Direction::Install,
            &[Component::TmuxStatusLine, Component::TmuxPopup],
            &d,
            &muxa::paths::default_socket(),
        )
        .unwrap();
        // Asserted as a delta, not as absence: these tests read the real
        // `~/.tmux.conf`, so whoever runs them may well have the word
        // MUXA_SOCKET in the file already (a stale pin, or a comment
        // about one). "Never adds a pin, may remove one" is the actual
        // contract, and it holds whatever the developer's file contains.
        let pin = "set-environment -g MUXA_SOCKET";
        for action in &plan.actions {
            if let Action::EditFile { before, after, .. } = action {
                let was = before.as_deref().unwrap_or_default().matches(pin).count();
                let now = after.matches(pin).count();
                assert!(
                    now <= was,
                    "planning the default socket must never add a pin (had {was}, planned {now})"
                );
            }
        }

        // The reconcile still has to run and still has to be sourced —
        // that is what scrubs a pin an older muxa already wrote.
        assert!(
            matches!(plan.actions.last(), Some(Action::SourceTmuxConf { .. })),
            "removal must still be sourced so the stale pin stops applying"
        );
    }

    #[test]
    fn only_a_socket_off_the_default_earns_a_pin() {
        let default = PathBuf::from("/run/user/1044/muxa.sock");
        assert!(
            !needs_socket_pin(&default, &default),
            "a pane derives the default itself"
        );
        assert!(
            needs_socket_pin(&PathBuf::from("/var/run/muxa-custom.sock"), &default),
            "a config-overridden socket is unguessable and must be pinned"
        );
        // `Path`'s comparison walks components, so a redundant `.` is not
        // mistaken for a custom socket and does not earn a spurious pin.
        assert!(!needs_socket_pin(
            &PathBuf::from("/run/user/1044/./muxa.sock"),
            &default
        ));
    }

    #[test]
    fn uninstall_deletes_file_emptied_by_removal() {
        // A file muxa created from scratch (opencode muxa.ts, or a
        // codex config with nothing but our hooks) is empty after the
        // block is stripped — we should delete it, not leave a 0-byte orphan.
        let mut actions = Vec::new();
        push_edit_or_delete(
            Direction::Uninstall,
            Component::OpencodeHooks,
            PathBuf::from("/home/u/.config/opencode/plugins/muxa.ts"),
            Some("// block\n".into()),
            "   \n".into(),
            Outcome::Removed,
            &mut actions,
        );
        assert!(
            matches!(actions.as_slice(), [Action::DeleteFile { .. }]),
            "empty remainder on uninstall must become a DeleteFile, got {actions:?}"
        );
    }

    #[test]
    fn uninstall_keeps_file_with_remaining_user_content() {
        let mut actions = Vec::new();
        push_edit_or_delete(
            Direction::Uninstall,
            Component::CodexHooks,
            PathBuf::from("/home/u/.codex/config.toml"),
            Some("[user]\nkey = 1\n\n[[hooks.X]]\n".into()),
            "[user]\nkey = 1\n".into(),
            Outcome::Removed,
            &mut actions,
        );
        assert!(
            matches!(actions.as_slice(), [Action::EditFile { .. }]),
            "a non-empty remainder must stay an EditFile so user content survives"
        );
    }

    #[test]
    fn install_never_deletes_even_with_empty_after() {
        // Defensive: the demotion is uninstall-only. An empty `after`
        // on install (shouldn't happen, but be safe) must not delete.
        let mut actions = Vec::new();
        push_edit_or_delete(
            Direction::Install,
            Component::OpencodeHooks,
            PathBuf::from("/x"),
            Some("x".into()),
            String::new(),
            Outcome::Replaced,
            &mut actions,
        );
        assert!(matches!(actions.as_slice(), [Action::EditFile { .. }]));
    }

    #[test]
    fn uninstall_absent_file_is_not_deleted() {
        // File never existed (before None) → nothing to delete.
        let mut actions = Vec::new();
        push_edit_or_delete(
            Direction::Uninstall,
            Component::GeminiHooks,
            PathBuf::from("/x"),
            None,
            String::new(),
            Outcome::AlreadyAbsent,
            &mut actions,
        );
        assert!(matches!(actions.as_slice(), [Action::EditFile { .. }]));
    }
}
