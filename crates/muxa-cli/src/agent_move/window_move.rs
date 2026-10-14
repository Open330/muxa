//! Whole-window transfer uses the host's move-window, never join-pane.
use super::*;

pub(super) const HISTORY: &str = "@muxa_window_move_origin";

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct History {
    origin: Location,
    panes: Vec<Location>,
}

fn check_members(rows: &[Location], request: &MoveRequest) -> Result<()> {
    ensure!(
        window_members(rows, &request.source) == request.window_panes,
        "window panes or processes changed; reopen the move menu"
    );
    Ok(())
}

fn restore_panes(control: &Control, panes: &[Location]) {
    for pane in panes {
        let _ = pane_options(control, &pane.target(), pane.options());
    }
}

#[allow(clippy::too_many_lines)] // destination setup and rollback form one transfer transaction
pub(super) fn execute(request: MoveRequest) -> Result<MoveOutcome> {
    let endpoint = &request.source.window.session.endpoint;
    ensure!(
        crate::mux_control::supported(endpoint.host),
        "whole-window moves require tmux or rmux"
    );
    let control = Control::Endpoint(endpoint.clone());
    let rows = inventory(&control)?;
    let source = locate(&rows, &request.source)?.clone();
    ensure!(
        source.pid == request.source_pid,
        "source process changed; reopen the move menu"
    );
    check_members(&rows, &request)?;
    let panes: Vec<_> = rows
        .iter()
        .filter(|row| row.session_id == source.session_id && row.window_id == source.window_id)
        .cloned()
        .collect();
    let source_target = format!("{}:{}", source.session_id, source.window_id);
    let old_history = control
        .output(&["show-options", "-w", "-v", "-t", &source_target, HISTORY])
        .unwrap_or_default();
    let undo: Option<History> = if matches!(request.destination, Destination::Undo) {
        Some(
            serde_json::from_str(old_history.trim())
                .context("no previous whole-window move to restore")?,
        )
    } else {
        None
    };
    if let Some(history) = &undo {
        let mut previous: Vec<_> = history
            .panes
            .iter()
            .map(|pane| (pane.pane.clone(), pane.pid.clone()))
            .collect();
        previous.sort();
        ensure!(
            previous == request.window_panes,
            "window changed since the previous move; choose a session manually"
        );
    }
    let mut created_session: Option<Location> = None;
    let mut source_keeper: Option<Location> = None;
    let mut adopted_workspace: Option<(String, String)> = None;
    let mut follow_view: Option<crate::tmux_work::MoveFollowView> = None;
    let mut renamed = false;
    let mut moved = false;
    let result = (|| -> Result<MoveOutcome> {
        let dest = match &request.destination {
            Destination::NewSession(name) => {
                let name = valid_name(name)?;
                ensure!(
                    !rows.iter().any(|row| row.session_name == name),
                    "session {name} already exists"
                );
                let dest = new_session(&control, &name, &source.cwd)?;
                created_session = Some(dest.clone());
                if source.managed_workspace || source.managed_work {
                    for (option, value) in [
                        (
                            "@muxa_workspace_id",
                            crate::tmux_work::normalize_workspace_id(&name)?,
                        ),
                        ("@muxa_workspace_cwd", source.cwd.clone()),
                        ("@muxa_managed_workspace", "1".into()),
                    ] {
                        set_option(&control, "", &dest.session_id, option, &value)?;
                    }
                }
                dest
            }
            Destination::NewWindow { session, name } => {
                ensure!(
                    name.is_none(),
                    "whole-window moves preserve the window name; rename it separately"
                );
                rows.iter()
                    .find(|row| &row.session_id == session || &row.session_name == session)
                    .context("destination session no longer exists")?
                    .clone()
            }
            Destination::Undo => {
                let history = undo.as_ref().expect("undo decoded");
                rows.iter()
                    .find(|row| row.session_id == history.origin.session_id)
                    .context("previous session was closed; choose a destination manually")?
                    .clone()
            }
            Destination::Window { .. } => anyhow::bail!(
                "a whole window moves into a session; it cannot be merged into another window"
            ),
        };
        ensure!(
            !rows
                .iter()
                .any(|row| row.session_id == dest.session_id && row.window_id == source.window_id),
            "window is already linked into that session"
        );
        let base_name = undo
            .as_ref()
            .map_or(source.window_name.as_str(), |history| {
                history.origin.window_name.as_str()
            });
        let mut window_name = base_name.to_string();
        let mut suffix = 2;
        while rows
            .iter()
            .any(|row| row.session_id == dest.session_id && row.window_name == window_name)
        {
            window_name = format!("{base_name}-{suffix}");
            suffix += 1;
        }
        ensure!(
            source.work.is_empty()
                || !rows
                    .iter()
                    .any(|row| row.session_id == dest.session_id && row.work == source.work),
            "destination already contains Work {}",
            source.work
        );
        let mut workspace = control
            .output(&[
                "show-options",
                "-v",
                "-t",
                &dest.session_id,
                "@muxa_workspace_id",
            ])
            .unwrap_or_default()
            .trim()
            .to_string();
        if workspace.is_empty()
            && (source.managed_work || panes.iter().any(|pane| !pane.pane_workspace.is_empty()))
        {
            adopted_workspace = Some((dest.session_id.clone(), workspace.clone()));
            workspace = crate::tmux_work::normalize_workspace_id(&dest.session_name)?;
            set_option(
                &control,
                "",
                &dest.session_id,
                "@muxa_workspace_id",
                &workspace,
            )?;
        }
        for pane in &panes {
            if let Some(history) = &undo {
                let origin = history
                    .panes
                    .iter()
                    .find(|old| old.pane == pane.pane)
                    .expect("members checked");
                pane_options(&control, &pane.target(), origin.options())?;
            } else {
                let old_workspace = if pane.pane_workspace.is_empty() {
                    &source.workspace
                } else {
                    &pane.pane_workspace
                };
                let work = if pane.pane_work.is_empty() {
                    &source.work
                } else {
                    &pane.pane_work
                };
                // A cross-workspace move changes WorkIdentity. The old
                // pipeline generation cannot complete that different Work.
                let generation = if old_workspace == &workspace {
                    &pane.generation
                } else {
                    ""
                };
                pane_options(&control, &pane.target(), [&workspace, work, generation])?;
            }
        }
        set_option(
            &control,
            "-w",
            &source_target,
            HISTORY,
            &serde_json::to_string(&History {
                origin: source.clone(),
                panes: panes.clone(),
            })?,
        )?;
        if rows
            .iter()
            .filter(|row| row.session_id == source.session_id)
            .all(|row| row.window_id == source.window_id)
        {
            // Keep the source clients and popup connected when their last
            // window leaves. Its panes and layout are never split or resized.
            source_keeper = Some(new_window(
                &control,
                &source.session_id,
                "shell",
                &source.cwd,
            )?);
        }
        check_members(&inventory(&control)?, &request)?;
        if window_name != source.window_name {
            control.run(&["rename-window", "-t", &source_target, &window_name])?;
            renamed = true;
        }
        let caller = request.follow.then(|| crate::CALLER_CLIENT.get()).flatten();
        if let Some(client) = caller {
            follow_view = Some(crate::tmux_work::prepare_window_move_follow(
                &control,
                client,
                &dest.session_id,
            )?);
        }
        let mut commands = vec![
            "move-window".to_string(),
            "-d".into(),
            "-s".into(),
            source_target.clone(),
            "-t".into(),
            format!("{}:", dest.session_id),
        ];
        if let (Some(client), Some(view)) = (caller, &follow_view) {
            let target = format!("{}:{}", view.session, source.window_id);
            commands.extend([
                ";".into(),
                "switch-client".into(),
                "-c".into(),
                client.clone(),
                "-t".into(),
                target.clone(),
            ]);
            commands.extend([
                ";".into(),
                "select-window".into(),
                "-t".into(),
                target.clone(),
            ]);
            commands.extend([
                ";".into(),
                "select-pane".into(),
                "-t".into(),
                format!("{target}.{}", source.pane),
            ]);
            if view.private {
                commands.extend([
                    ";".into(),
                    "set-option".into(),
                    "-t".into(),
                    view.session.clone(),
                    "destroy-unattached".into(),
                    "on".into(),
                ]);
            }
        }
        if let Some(placeholder) = &created_session {
            commands.extend([
                ";".into(),
                "kill-window".into(),
                "-t".into(),
                format!("{}:{}", placeholder.session_id, placeholder.window_id),
            ]);
        }
        // The host owns this complete command queue even if moving the
        // originating window closes the popup and its PTY midway through.
        let arguments: Vec<_> = commands.iter().map(String::as_str).collect();
        let transfer = control.run(&arguments);
        let mut warnings = Vec::new();
        if let Err(error) = transfer {
            match inventory(&control) {
                Ok(observed)
                    if observed.iter().any(|row| {
                        row.session_id == dest.session_id && row.window_id == source.window_id
                    }) =>
                {
                    moved = true;
                    warnings.push(format!(
                        "window moved; follow/cleanup needs attention: {error}"
                    ));
                }
                Ok(observed)
                    if observed.iter().any(|row| {
                        row.session_id == source.session_id && row.window_id == source.window_id
                    }) =>
                {
                    return Err(error)
                }
                _ => {
                    // Never delete a newly-created destination when the move
                    // acknowledgement is lost and the agents may be there.
                    moved = true;
                    return Err(error.context(
                        "transfer outcome is unknown; inspect the live window before retrying",
                    ));
                }
            }
        } else {
            moved = true;
        }
        let mut actual = source.clone();
        actual.session_id.clone_from(&dest.session_id);
        actual.session_name.clone_from(&dest.session_name);
        actual.window_name.clone_from(&window_name);
        match inventory(&control) {
            Ok(observed) => {
                let key = actual.key(endpoint);
                if window_members(&observed, &key) != request.window_panes {
                    warnings.push("window membership changed after transfer".into());
                }
            }
            Err(error) => warnings.push(format!(
                "transfer succeeded; location refresh failed: {error}"
            )),
        }
        let mut message = format!(
            "moved whole window {} ({} panes) to {} — processes and layout preserved",
            window_name,
            panes.len(),
            dest.session_name
        );
        if !warnings.is_empty() {
            let _ = write!(message, " ({})", warnings.join("; "));
        }
        Ok(MoveOutcome {
            pane: actual.key(endpoint),
            follow: request.follow && warnings.is_empty(),
            message,
        })
    })();
    if result.is_err() && !moved {
        if renamed {
            let _ = control.run(&["rename-window", "-t", &source_target, &source.window_name]);
        }
        restore_panes(&control, &panes);
        let _ = set_option(
            &control,
            "-w",
            &source_target,
            HISTORY,
            old_history.trim_end(),
        );
        if let Some(keeper) = source_keeper {
            let _ = control.run(&[
                "kill-window",
                "-t",
                &format!("{}:{}", keeper.session_id, keeper.window_id),
            ]);
        }
        if let Some((session, workspace)) = adopted_workspace {
            let _ = set_option(&control, "", &session, "@muxa_workspace_id", &workspace);
        }
        if let Some(view) = &follow_view {
            if view.created {
                let _ = control.run(&["kill-session", "-t", &view.session]);
            }
        }
        if let Some(created) = created_session {
            let _ = control.run(&["kill-session", "-t", &created.session_id]);
        }
    }
    result
}
