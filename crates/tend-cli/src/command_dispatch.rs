use super::{
    activity_query, agent_move, automation, cmd_ask, cmd_ask_admin, cmd_attach, cmd_attend,
    cmd_dashboard, cmd_detach, cmd_fleet_watch, cmd_forget_agent, cmd_identity, cmd_msg, cmd_panes,
    cmd_peers, cmd_prune, cmd_recap, cmd_register, cmd_run, cmd_status, cmd_status_line, cmd_sync,
    cmd_watch, cmd_zellij_plugin_snapshot, collaboration_client_kind, config_cmd, daemon, doctor,
    fleet_cli, graph, handle_hook, init, logs, mcp, message_skill, mux_control, onboarding,
    parse_args, paths, peek, relay, reload, run_agent_cmd, run_window_cmd, run_work_cmd,
    run_workspace_cmd, servers_cmd, set_icon_set, sidebar, start, stats, swap, timeline, upgrade,
    worker_cmd, worktrees_cmd, AgentCmd, AgentViewCmd, AskOptions, Client, Cmd, Config, Context,
    ProcCmd, RegistryCmd, Result, WatchInvocation, WindowCmd, WorkCmd, WorkspaceCmd, CALLER_CLIENT,
};

#[allow(clippy::too_many_lines)] // top-level CLI subcommand wiring
pub(super) async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "tend=warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = parse_args();
    if args.check_config {
        let path = args.config.clone().or_else(paths::default_config_file);
        Config::load_or_default(path.as_deref()).context("checking startup configuration")?;
        return Ok(());
    }
    let cmd = args.cmd.unwrap_or(Cmd::Start);
    if let Cmd::Agent {
        action: AgentCmd::Move(move_args),
    } = &cmd
    {
        return agent_move::run(move_args.clone(), args.socket.clone()).await;
    }
    if let Cmd::Window {
        action: WindowCmd::Move(move_args),
    } = &cmd
    {
        return agent_move::run_window(move_args.clone(), args.socket.clone()).await;
    }
    if let Cmd::Peek(peek_args) = &cmd {
        if peek_args.popup {
            return mux_control::peek_popup(peek_args.caller_client.as_deref());
        }
    }
    if let Cmd::Watch {
        popup: true,
        caller_client,
        caller_pane,
        ..
    } = &cmd
    {
        return mux_control::watch_popup(caller_client.as_deref(), caller_pane.as_deref());
    }
    if matches!(cmd, Cmd::Start) && !start::interactive() {
        start::print_help();
        return Ok(());
    }
    // Onboarding must remain available even when config.toml is malformed;
    // it is a recovery/learning surface and does not need daemon state.
    if let Cmd::Onboard(onboard_args) = &cmd {
        return onboarding::run(onboard_args.clone());
    }
    let config_path = args.config.clone().or_else(paths::default_config_file);
    let skill_path = config_path.clone();
    let cfg = match Config::load_or_default(config_path.as_deref()) {
        Ok(cfg) => cfg,
        // A hook handler must not hard-fail on an unreadable config. agy
        // reads a non-zero hook exit as its verdict
        // (`tool call denied by pre-tool hook`), so one TOML typo in
        // config.toml would otherwise block every tool call in every agy
        // session — with nothing on screen naming the cause. Degrade to
        // defaults and put the reason on stderr, never stdout (stdout is
        // the verdict channel).
        Err(e) if matches!(cmd, Cmd::Hook { .. }) => {
            eprintln!("tend: config unreadable ({e:#}); this hook is using defaults");
            Config::default()
        }
        Err(e) => return Err(e).context("loading config"),
    };
    set_icon_set(cfg.ui.icons);
    let socket = args
        .socket
        .or_else(|| cfg.socket.clone())
        .unwrap_or_else(paths::default_socket);
    let client =
        Client::new(socket.clone()).with_collaboration_client_kind(collaboration_client_kind(&cmd));

    let invalidates_directory = matches!(
        &cmd,
        Cmd::Agent {
            action: AgentCmd::Start(_)
                | AgentCmd::Prepare(_)
                | AgentCmd::Control(_)
                | AgentCmd::Move(_)
        } | Cmd::Window { .. }
            | Cmd::Reload(_)
            | Cmd::Restore(_)
            | Cmd::Swap(_)
            | Cmd::Work {
                action: WorkCmd::Up(_)
                    | WorkCmd::Start(_)
                    | WorkCmd::Done(_)
                    | WorkCmd::Reconcile(_)
                    | WorkCmd::Close(_)
                    | WorkCmd::Down(_)
            }
            | Cmd::Workspace {
                action: WorkspaceCmd::Close(_) | WorkspaceCmd::View(_)
            }
    );
    let invalidation_client = Client::new(socket.clone());
    let result = match cmd {
        Cmd::Start => start::run(&client, cfg, socket, config_path).await,
        Cmd::AgentView(AgentViewCmd::Agents(_)) => {
            println!(
                "{}",
                serde_json::to_string(&client.agents_with_launch_identity().await?)?
            );
            Ok(())
        }
        Cmd::Status { theme, flat, json } => cmd_status(&client, &cfg, theme, flat, json).await,
        Cmd::StatusLine {
            pane,
            needs_attention,
        } => cmd_status_line(&client, pane, needs_attention).await,
        Cmd::Recap { pane, limit, all } => cmd_recap(&client, pane, limit, all).await,
        Cmd::Peers { json } => cmd_peers(&client, json).await,
        Cmd::Peer(args) => super::peer_cli::run(&client, &cfg, args).await,
        Cmd::Msg { action } => cmd_msg(&client, action).await,
        Cmd::Ask {
            action: Some(action),
            ..
        } => cmd_ask_admin(&client, action).await,
        Cmd::Ask {
            action: None,
            prompt,
            agent,
            api_key_stdin,
            detach,
            new,
            json,
            timeout_secs,
        } => {
            let prompt = prompt.context(
                "give a question to ask, or a subcommand (`tend ask providers`, `tend ask provider set …`)",
            )?;
            cmd_ask(
                &client,
                prompt,
                AskOptions {
                    agent,
                    api_key_stdin,
                    detach,
                    new,
                    json,
                    timeout_secs,
                },
            )
            .await
        }
        Cmd::Skill(a) => message_skill::run(a, &cfg.message, skill_path.as_deref()),
        Cmd::Automation(a) => automation::run(a, &client).await,
        Cmd::Config(a) => config_cmd::run(a, socket.clone()).await,
        Cmd::Workers(a) => worker_cmd::run(a, &Client::new(socket.clone())).await,
        Cmd::Worktrees(a) => worktrees_cmd::run(a).await,
        Cmd::Servers(a) => servers_cmd::run(a, &Client::new(socket.clone())).await,
        Cmd::Host(a) => fleet_cli::run_host(a, &client, &cfg, config_path.as_deref()).await,
        Cmd::Fleet(a) => {
            Box::pin(fleet_cli::run_fleet(
                a,
                &client,
                &cfg,
                config_path.as_deref(),
            ))
            .await
        }
        Cmd::Agent { action } => run_agent_cmd(action, &client, &socket, &cfg).await,
        Cmd::Window { action } => run_window_cmd(action).await,
        Cmd::Work { action } => run_work_cmd(action, &cfg, config_path, &client).await,
        Cmd::Workspace { action } => run_workspace_cmd(action),
        Cmd::Identity { action } => cmd_identity(&client, action).await,
        Cmd::Stats(stats_command) => stats::run_command(&client, &cfg, stats_command).await,
        Cmd::Report(report) => stats::run_report(&client, &cfg, &report.window, report.args).await,
        Cmd::Timeline(timeline_args) => timeline::run(&client, &cfg, timeline_args).await,
        Cmd::Dashboard(args) => {
            cmd_dashboard(&client, cfg.clone(), config_path.clone(), &args).await
        }
        Cmd::Graph(args) => graph::run(&cfg, args).await,
        Cmd::Activity(activity) => activity_query::run(&cfg, &activity.window, activity.args).await,
        Cmd::Hook { which } => handle_hook(&client, which).await,
        Cmd::Panes => cmd_panes(),
        Cmd::Registry { action } => match action {
            RegistryCmd::Sync => cmd_sync(&client).await,
            RegistryCmd::Prune(prune) => {
                cmd_prune(&client, &prune.older_than, prune.all, prune.yes).await
            }
            RegistryCmd::Forget { session_id, yes } => {
                cmd_forget_agent(&client, &session_id, yes).await
            }
            RegistryCmd::Panes => cmd_panes(),
        },
        Cmd::Peek(peek_args) => peek::run(&client, peek_args).await,
        Cmd::Sidebar(args) => sidebar::run(&client, &cfg, args, config_path.as_deref()).await,
        Cmd::Watch {
            popup: _,
            fleet,
            selector,
            include_paneless,
            view,
            layout,
            collab_layout,
            screen,
            board_sort,
            since,
            sort,
            theme,
            caller_client,
            caller_pane,
        } => {
            let mut cfg = cfg;
            if let Some(board_sort) = board_sort {
                cfg.watch.board_sort = board_sort;
            }
            if let Some(since) = since {
                cfg.watch.board_since = Some(since);
            }
            if let Some(collab_layout) = collab_layout {
                cfg.watch.collab_layout = collab_layout;
            }
            // Pin focus-moving commands to the requesting client. A value
            // still containing `#{` is a binding that never expanded.
            if let Some(client_name) = caller_client.filter(|c| !c.contains("#{")) {
                let _ = CALLER_CLIENT.set(client_name);
            }
            if fleet {
                return cmd_fleet_watch(
                    &client,
                    cfg,
                    config_path.clone(),
                    selector,
                    WatchInvocation {
                        include_paneless,
                        view,
                        layout,
                        screen,
                        sort,
                        theme,
                        caller_pane: caller_pane.filter(|p| p.starts_with('%')),
                    },
                )
                .await;
            }
            cmd_watch(
                &client,
                cfg,
                config_path.clone(),
                WatchInvocation {
                    include_paneless,
                    view,
                    layout,
                    screen,
                    sort,
                    theme,
                    // Same unexpanded-format guard as the client: a literal
                    // `#{pane_id}` would seed the room and cursor with a
                    // pane that matches nothing.
                    caller_pane: caller_pane.filter(|p| p.starts_with('%')),
                },
            )
            .await
        }
        Cmd::Proc { action } => match action {
            ProcCmd::Run(run) => {
                cmd_run(&client, &socket, run.name, run.cwd, run.detach, run.command).await
            }
            ProcCmd::Attach { session } => cmd_attach(&client, &session).await,
            ProcCmd::Detach { session } => cmd_detach(&client, &session).await,
            ProcCmd::Register(register) => {
                cmd_register(
                    &client,
                    register.name,
                    register.pid,
                    register.cwd,
                    register.pane,
                )
                .await
            }
        },
        Cmd::Run(run) => {
            cmd_run(&client, &socket, run.name, run.cwd, run.detach, run.command).await
        }
        Cmd::Attach { session } => cmd_attach(&client, &session).await,
        Cmd::Detach { session } => cmd_detach(&client, &session).await,
        Cmd::Register(register) => {
            cmd_register(
                &client,
                register.name,
                register.pid,
                register.cwd,
                register.pane,
            )
            .await
        }
        Cmd::ZellijPluginSnapshot { json } => cmd_zellij_plugin_snapshot(&client, &json).await,
        Cmd::Relay { stdio } => {
            if !stdio {
                anyhow::bail!("only --stdio relay transport is supported");
            }
            relay::run(client).await
        }
        Cmd::FleetRemoteAttach { token, fit } => relay::remote_attach(&token, fit),
        Cmd::Attend(attend_args) => cmd_attend(&client, attend_args).await,
        Cmd::Sync => cmd_sync(&client).await,
        Cmd::Init(init_args) => init::run(init_args, socket, config_path).await,
        Cmd::Doctor { verbose } => doctor::run(socket, config_path.as_deref(), verbose).await,
        Cmd::Daemon { action } => {
            daemon::run(action, &client, &socket, config_path.as_deref()).await
        }
        Cmd::Onboard(onboard_args) => onboarding::run(onboard_args),
        Cmd::Mcp => mcp::run(client, cfg).await,
        Cmd::Logs(logs_args) => logs::run(logs_args).await,
        Cmd::Snapshot(snapshot_args) => reload::snapshot(&client, snapshot_args).await,
        Cmd::Restore(restore_args) => reload::restore(client.socket(), restore_args),
        Cmd::Reload(reload_args) => reload::reload(&client, reload_args).await,
        Cmd::Swap(swap_args) => {
            swap::run(
                &client,
                swap_args,
                &cfg.swap,
                &cfg.accounts,
                args.config.as_deref(),
            )
            .await
        }
        Cmd::Upgrade(upgrade_args) => upgrade::run(upgrade_args, socket, config_path).await,
        Cmd::Prune(prune) => cmd_prune(&client, &prune.older_than, prune.all, prune.yes).await,
    };
    if invalidates_directory {
        let _ = invalidation_client.invalidate_agent_directory().await;
    }
    result
}
