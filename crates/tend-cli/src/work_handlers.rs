use super::{
    agent_find, agent_launch, agent_move, agent_records, orchestration, tmux_work, work_backlog,
    work_compose, work_init, work_options, work_pipeline, work_up, AgentCmd, Client, Config, Path,
    PathBuf, Result, WindowCmd, WorkCmd, WorkspaceCmd,
};

pub(super) async fn run_agent_cmd(
    action: AgentCmd,
    client: &Client,
    socket_path: &Path,
    cfg: &Config,
) -> Result<()> {
    match action {
        AgentCmd::Find(args) => agent_find::run(client, args).await,
        AgentCmd::Search(args) => agent_records::search(args),
        AgentCmd::Read(args) => agent_records::read(args),
        AgentCmd::Start(args) => agent_launch::run(args, client, socket_path, cfg).await,
        AgentCmd::Prepare(args) => {
            tokio::task::spawn_blocking(move || crate::worker_startup::run(args)).await?
        }
        AgentCmd::Control(args) => tmux_work::run_agent_control(args, client).await,
        AgentCmd::Move(args) => agent_move::run(args, Some(client.socket().to_path_buf())).await,
    }
}

pub(super) async fn run_window_cmd(action: WindowCmd) -> Result<()> {
    match action {
        WindowCmd::Rename(args) => tmux_work::run_window_rename(args),
        WindowCmd::Move(args) => agent_move::run_window(args, None).await,
    }
}

pub(super) async fn run_work_cmd(
    action: WorkCmd,
    cfg: &Config,
    config_path: Option<PathBuf>,
    client: &Client,
) -> Result<()> {
    match action {
        WorkCmd::FleetAsk => orchestration::shared_ask(cfg, client).await,
        WorkCmd::Dispatch(args) => {
            orchestration::dispatch(args, cfg, client, config_path.as_deref()).await
        }
        WorkCmd::DispatchOptions { at_coordinator } => {
            orchestration::options(cfg, client, at_coordinator).await
        }
        WorkCmd::DispatchStatus(args) => orchestration::status(args, cfg, client).await,
        WorkCmd::DispatchExecute(args) => {
            orchestration::execute(args, cfg, client, config_path.as_deref()).await
        }
        WorkCmd::DispatchWorkerStatus { dispatch_id } => {
            orchestration::worker_status(&dispatch_id, client).await
        }
        WorkCmd::Init(args) => work_init::run(args, cfg, config_path).await,
        WorkCmd::Compose(args) => work_compose::run(args, cfg).await,
        WorkCmd::Up(args) => work_up::run(args, cfg, config_path, Some(client)).await,
        WorkCmd::Start(args) => agent_launch::run_work_start(args, client.socket(), cfg),
        WorkCmd::Plan(args) => work_backlog::run_plan(args, client).await,
        WorkCmd::List(args) => match args.stage {
            Some(stage) => {
                work_backlog::run_list_stage(client, stage, args.workspace.as_deref(), args.json)
                    .await
            }
            None => tmux_work::run_work_list(args, client).await,
        },
        WorkCmd::Show(args) => tmux_work::run_work_show(args, client).await,
        WorkCmd::Done(args) => tmux_work::run_work_done(args, client, cfg).await,
        WorkCmd::Reconcile(args) => work_up::run_reconcile(args, client, cfg).await,
        WorkCmd::Close(args) | WorkCmd::Down(args) => tmux_work::run_work_close(args),
        WorkCmd::Options(args) => work_options::run_options(args, cfg, config_path),
        WorkCmd::Preset(args) => work_options::run_preset(args, config_path),
        WorkCmd::Pipeline(args) => work_pipeline::run_pipeline(args, config_path),
        WorkCmd::Route(args) => work_pipeline::run_route(args, config_path),
    }
}

pub(super) fn run_workspace_cmd(action: WorkspaceCmd) -> Result<()> {
    match action {
        WorkspaceCmd::List(args) => tmux_work::run_workspace_list(args),
        WorkspaceCmd::Show(args) => tmux_work::run_workspace_show(args),
        WorkspaceCmd::Close(args) => tmux_work::run_workspace_close(args),
        WorkspaceCmd::View(args) => tmux_work::run_workspace_view(args),
    }
}
