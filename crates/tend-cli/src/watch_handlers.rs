use super::{
    agent_move, all_panes, attend, dashboard_cmd, discovery, fleet_watch, jump_to_pane,
    jump_to_pane_cmux, jump_to_pane_logged, jump_to_topology_pane_logged, onboarding, paths, watch,
    Client, Config, Context, PathBuf, Result, ThemeArg, WatchConfig, WatchLayoutArg,
    WatchScreenArg, WatchSortArg, WatchTheme, WatchViewArg,
};

/// Jump to (or list) the agent that needs you. `attend::run` picks the
/// pane and endpoint — reusing the same selection that drives `--list` — and we perform
/// the actual focus through `jump_to_pane`, the identical machinery the
/// `tend watch` Enter action uses, so a jump lands the same way from both.
pub(super) async fn cmd_attend(client: &Client, args: attend::Args) -> Result<()> {
    // Enumerate panes across every active host so a herdr agent that needs
    // a human is jumpable from a tmux-primary shell (and vice versa). The
    // jump itself dispatches per-row in `jump_to_pane`. One row per pane, so
    // the queue never labels a grouped pane by a `~view~` client session.
    let panes = tend::tmux::canonical_panes(&all_panes().await);
    if let Some(target) = attend::run(client, panes, args).await? {
        if tend::backend::pane_id_host_kind(&target.pane) == Some(tend::HostKind::Cmux) {
            let backend = target.endpoint.map_or_else(
                tend::backend::cmux::CmuxBackend::new,
                tend::backend::cmux::CmuxBackend::with_endpoint,
            );
            jump_to_pane_cmux(&backend, &target.pane);
        } else {
            jump_to_pane(&target.pane);
        }
    }
    Ok(())
}

/// Backfill the daemon's registry from host panes. Idempotent.
pub(super) async fn cmd_sync(client: &Client) -> Result<()> {
    use std::fmt::Write as _;

    // Each CLI invocation resolves hosts in the user's environment rather
    // than the daemon's (which may have been started under a different
    // shell) — every active host, so a sync from a plain terminal or a tmux
    // shell still finds agents running in rmux.
    let mut report = discovery::DiscoveryReport::default();
    for backend in tend::active_backends() {
        let found = discovery::run_discovery(client, backend.as_ref())
            .await
            .context("running discovery")?;
        report.claude_code += found.claude_code;
        report.codex += found.codex;
        report.gemini_cli += found.gemini_cli;
        report.antigravity += found.antigravity;
        report.skipped_known += found.skipped_known;
        report.failed += found.failed;
    }

    // Build the kind breakdown only for non-zero counts so the line stays
    // readable when only one agent kind is present.
    let mut parts: Vec<String> = Vec::new();
    if report.claude_code > 0 {
        parts.push(format!("{} claude_code", report.claude_code));
    }
    if report.codex > 0 {
        parts.push(format!("{} codex", report.codex));
    }
    if report.gemini_cli > 0 {
        parts.push(format!("{} gemini_cli", report.gemini_cli));
    }
    if report.antigravity > 0 {
        parts.push(format!("{} antigravity", report.antigravity));
    }

    let total = report.total_ingested();
    if total == 0 && report.skipped_known == 0 && report.failed == 0 {
        println!("no agent panes discovered");
        return Ok(());
    }

    let mut line = format!("discovered {total} agent");
    if total != 1 {
        line.push('s');
    }
    if !parts.is_empty() {
        line.push_str(": ");
        line.push_str(&parts.join(", "));
    }
    if report.skipped_known > 0 {
        // `write!` to a String never fails — unwrap is fine and avoids the
        // intermediate allocation that `push_str(&format!(..))` would do.
        let _ = write!(line, " (skipped {} already-known)", report.skipped_known);
    }
    if report.failed > 0 {
        let _ = write!(line, " — {} failed", report.failed);
    }
    println!("{line}");
    Ok(())
}

/// One-shot per-invocation overrides for `tend watch`, bundled so the
/// argument list stays a description of *this run* rather than a flat
/// parade of options.
#[derive(Debug, Clone, Default)]
pub(crate) struct WatchInvocation {
    pub(crate) include_paneless: bool,
    pub(crate) view: Option<WatchViewArg>,
    pub(crate) layout: Option<WatchLayoutArg>,
    pub(crate) screen: Option<WatchScreenArg>,
    pub(crate) sort: Option<WatchSortArg>,
    pub(crate) theme: Option<ThemeArg>,
    pub(crate) caller_pane: Option<String>,
}

pub(crate) async fn cmd_fleet_watch(
    client: &Client,
    cfg: Config,
    config_path: Option<PathBuf>,
    selector: Option<String>,
    invocation: WatchInvocation,
) -> Result<()> {
    selector
        .as_deref()
        .map(str::parse::<tend::LabelSelector>)
        .transpose()
        .map_err(anyhow::Error::msg)
        .context("invalid fleet label selector")?;
    let initial = client
        .fleet_snapshot(selector.as_deref())
        .await
        .context("reading initial fleet state from tendd")?;
    // A lone local controller is the overwhelmingly common case and should
    // feel exactly like `tend watch`: adding the Fleet feature must not insert
    // a redundant host row or discard the mature inspector and collaboration
    // surface. Host hierarchy becomes visible only when it carries information.
    if fleet_watch::uses_native_local_watch(&initial) {
        return cmd_watch(client, cfg, config_path, invocation.clone()).await;
    }
    // The Work board reads this host's Work records and multiplexers; under
    // a host header it would pass off one host's Works as the fleet's.
    let screen = invocation
        .screen
        .map_or(cfg.watch.screen, tend::config::WatchScreen::from);
    if screen == tend::config::WatchScreen::Board {
        anyhow::bail!(
            "the Work board is local to this host; run `tend watch --screen board` without --fleet"
        );
    }
    fleet_watch::run(
        client.clone(),
        &cfg,
        selector,
        initial,
        invocation,
        config_path,
    )
    .await
}

pub(crate) async fn cmd_watch(
    client: &Client,
    cfg: Config,
    config_path: Option<PathBuf>,
    invocation: WatchInvocation,
) -> Result<()> {
    let WatchInvocation {
        include_paneless,
        view,
        layout,
        screen,
        sort,
        theme,
        caller_pane,
    } = invocation;
    // watch::run restores the terminal before returning, so by the time we
    // get here it's safe to exec tmux commands that mutate the client's
    // attached session / pane.
    //
    let message_skills = cfg.message.skills.clone();
    // CLI flag wins over config — one-shot override for the current
    // invocation without touching the user's ~/.config/tend/config.toml.
    let mut watch_cfg = WatchConfig {
        hide_paneless: cfg.watch.hide_paneless && !include_paneless,
        ..cfg.watch
    };
    if let Some(view) = view {
        watch_cfg.view = view.into();
    }
    if let Some(screen) = screen {
        watch_cfg.screen = screen.into();
    }
    if let Some(layout) = layout {
        watch_cfg.layout = layout.into();
    }
    if let Some(sort) = sort {
        watch_cfg.sort = sort.keys();
    }
    watch_cfg.theme = Some(
        theme
            .map(WatchTheme::from)
            .or(watch_cfg.theme)
            .unwrap_or(cfg.ui.theme),
    );
    let activity_path = cfg
        .activity
        .enabled
        .then(|| {
            cfg.activity
                .path
                .clone()
                .or_else(paths::default_activity_file)
        })
        .flatten();
    let session_activity_path = cfg
        .session_activity
        .enabled
        .then(|| {
            cfg.session_activity
                .path
                .clone()
                .or_else(paths::default_session_activity_file)
        })
        .flatten();
    let message_composer = watch::MessageComposerConfig {
        skills: message_skills,
        collaboration_scope: cfg.collaboration.scope,
    };
    if let Some(target) = Box::pin(watch::run(
        client,
        watch_cfg,
        message_composer,
        session_activity_path,
        activity_path.clone(),
        config_path,
        caller_pane,
    ))
    .await?
    {
        match target {
            watch::WatchOpenTarget::Demo => onboarding::run(onboarding::Args {
                quick: true,
                ..Default::default()
            })?,
            watch::WatchOpenTarget::Moved(key) => agent_move::follow(&key)?,
            watch::WatchOpenTarget::TopologyPane(key) => {
                jump_to_topology_pane_logged(&key, activity_path.as_deref()).await;
            }
            watch::WatchOpenTarget::LegacyPane(pane_id) => {
                jump_to_pane_logged(&pane_id, activity_path.as_deref()).await;
            }
        }
    }
    Ok(())
}

/// `tend dashboard`: `tend watch` on its Work board screen, with the
/// dashboard's flags carried over for this run only.
pub(super) async fn cmd_dashboard(
    client: &Client,
    mut cfg: Config,
    config_path: Option<PathBuf>,
    args: &dashboard_cmd::Args,
) -> Result<()> {
    cfg.watch.board_sort = args.board_sort();
    cfg.watch.board_since = Some(args.board_since()?);
    cmd_watch(
        client,
        cfg,
        config_path,
        WatchInvocation {
            include_paneless: args.include_paneless(),
            view: None,
            layout: None,
            screen: Some(WatchScreenArg::Board),
            sort: None,
            theme: args.theme(),
            caller_pane: None,
        },
    )
    .await
}
