//! A single interactive entry point; explicit commands remain scriptable.

use anyhow::{Context, Result};
use muxa::{ipc::Client, Config};
use std::io::IsTerminal;
use std::path::PathBuf;

use crate::agent_launch::{AgentProgram, LaunchHost, Placement, StartArgs};

pub(crate) fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

pub(crate) fn print_help() {
    println!("Muxa — see your coding agents and go to the one waiting for you.\n\nRun `muxa` in a terminal to get started.\n  muxa status --json          inspect running agents\n  muxa watch                 open the live view\n  muxa onboard --quick        try the three-step demo\n  muxa onboard --quick --print  read the demo guide\n  muxa --help                list all commands");
}

pub(crate) async fn run(
    client: &Client,
    cfg: Config,
    socket: PathBuf,
    config_path: Option<PathBuf>,
) -> Result<()> {
    // Existing users go directly to their agents. Do not turn an endpoint
    // failure into an empty fleet or offer to spawn duplicate workers.
    if crate::daemon::socket_responding(&socket) {
        let agents = client.snapshot().await.context("finding running agents")?;
        if !agents.is_empty() {
            return crate::cmd_watch(client, cfg, config_path, crate::WatchInvocation::default())
                .await;
        }
    }

    let cwd = std::env::current_dir().context("finding the current project")?;
    let choice = choose(&cwd)?;
    if choice == "exit" {
        return Ok(());
    }
    if choice == "demo" {
        return crate::onboarding::run(crate::onboarding::Args {
            quick: true,
            ..Default::default()
        });
    }
    if choice == "watch" {
        crate::daemon::run(
            crate::daemon::Action::Start { quiet: true },
            client,
            &socket,
            config_path.as_deref(),
        )
        .await?;
    } else {
        anyhow::ensure!(
            which::which("tmux").is_ok(),
            "Starting an agent in this terminal needs tmux. Install tmux, then run `muxa` again."
        );
        let program = AgentProgram::parse(choice).map_err(anyhow::Error::msg)?;
        if !configure_agent(program, &socket, config_path.clone()).await? {
            return Ok(());
        }
        crate::daemon::run(
            crate::daemon::Action::Start { quiet: true },
            client,
            &socket,
            config_path.as_deref(),
        )
        .await?;
        crate::agent_launch::run(
            StartArgs {
                purpose: muxa::collaboration::LaunchPurpose::Primary,
                agent: program,
                host: LaunchHost::Tmux,
                placement: if std::env::var_os("TMUX").is_some() {
                    Placement::Pane
                } else {
                    Placement::Session
                },
                cwd: Some(cwd),
                target: None,
                prompt: None,
                name: None,
                workspace: None,
                work: None,
                role: None,
                task: None,
                alias: None,
                options: Vec::new(),
                direction: None,
                json: false,
            },
            client,
            &socket,
            &cfg,
        )
        .await?;
    }
    crate::cmd_watch(client, cfg, config_path, crate::WatchInvocation::default()).await
}

async fn configure_agent(
    program: AgentProgram,
    socket: &std::path::Path,
    config_path: Option<PathBuf>,
) -> Result<bool> {
    // Reuse the installer's preview, backups, and confirmation. MCP and
    // collaboration are not prerequisites for this first agent.
    let hook = match program {
        AgentProgram::Claude => "claude-hooks",
        AgentProgram::Codex => "codex-hooks",
        AgentProgram::Gemini => "gemini-hooks",
        AgentProgram::Antigravity => "agy-hooks",
        AgentProgram::Opencode => "opencode-hooks",
    };
    crate::init::run_for_start(
        crate::init::Args {
            component: vec!["tmux-popup".into(), hook.into()],
            start_daemon: true,
            ..Default::default()
        },
        socket.to_path_buf(),
        config_path,
    )
    .await
}

fn choose(cwd: &std::path::Path) -> Result<&'static str> {
    let language = crate::onboarding::Language::Auto.resolve();
    let text = |en, ko| crate::onboarding::tr(language, en, ko);
    cliclack::intro("Muxa")?;
    let mut choices = cliclack::select(format!(
        "{}: {}",
        text("Project", "프로젝트"),
        cwd.display()
    ));
    let programs = [
        AgentProgram::Codex,
        AgentProgram::Claude,
        AgentProgram::Gemini,
        AgentProgram::Antigravity,
        AgentProgram::Opencode,
    ];
    for program in programs {
        if which::which(program.label()).is_ok() {
            choices = choices.item(
                program.label(),
                format!(
                    "{} {}",
                    program.label(),
                    text("— start here", "— 여기서 시작")
                ),
                text(
                    "Use the current folder; no task or role required",
                    "현재 폴더에서 시작합니다. 작업명·역할 입력은 필요 없습니다.",
                ),
            );
        }
    }
    choices = choices.item(
        "watch",
        text("View existing terminals", "기존 터미널 보기"),
        text(
            "Find agents already running in your multiplexer",
            "이미 실행 중인 에이전트를 찾습니다.",
        ),
    );
    // The isolated tour cannot nest inside a real tmux client.
    if std::env::var_os("TMUX").is_none() && which::which("tmux").is_ok() {
        choices = choices.item(
            "demo",
            text("Try the three-step demo", "3단계 데모 체험"),
            text(
                "Practice agents; no API calls or account required",
                "연습용 에이전트입니다. API 호출·계정이 필요 없습니다.",
            ),
        );
    }
    choices = choices.item("exit", text("Exit", "종료"), "");
    let choice = choices.interact()?;
    cliclack::outro(text("Ready", "선택 완료"))?;
    Ok(choice)
}
