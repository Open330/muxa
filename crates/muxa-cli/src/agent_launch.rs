//! Deterministic tmux agent launcher shared by the CLI and MCP tool.
//!
//! This deliberately exposes an allowlist of known agent CLIs instead of an
//! arbitrary shell command. Agents can create a worker pane without spending
//! a model turn reconstructing tmux syntax, while callers still get a narrow,
//! predictable operation.

use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum AgentProgram {
    Claude,
    Codex,
    Gemini,
    Opencode,
}

impl AgentProgram {
    pub fn parse(value: &str) -> std::result::Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude_code" | "claude-code" => Ok(Self::Claude),
            "codex" | "cx" => Ok(Self::Codex),
            "gemini" | "gemini_cli" | "gemini-cli" => Ok(Self::Gemini),
            "opencode" => Ok(Self::Opencode),
            _ => Err(format!(
                "unknown agent {value:?}; expected claude, codex, gemini, or opencode"
            )),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::Opencode => "opencode",
        }
    }

    fn launch_command(self, prompt: Option<&str>) -> String {
        let quoted = prompt.map(shell_single_quote);
        match (self, quoted) {
            (Self::Claude, Some(prompt)) => {
                format!("claude --dangerously-skip-permissions {prompt}")
            }
            (Self::Claude, None) => "claude --dangerously-skip-permissions".into(),
            // cx in the user's shell is codex --yolo. Invoke the expanded
            // command so launch behavior does not depend on interactive zsh
            // alias loading inside tmux.
            (Self::Codex, Some(prompt)) => format!("codex --yolo {prompt}"),
            (Self::Codex, None) => "codex --yolo".into(),
            (Self::Gemini, Some(prompt)) => {
                format!("gemini --approval-mode yolo --skip-trust -i {prompt}")
            }
            (Self::Gemini, None) => "gemini --approval-mode yolo --skip-trust".into(),
            (Self::Opencode, Some(prompt)) => format!("opencode --prompt {prompt}"),
            (Self::Opencode, None) => "opencode".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    #[default]
    Pane,
    Window,
    Session,
}

impl Placement {
    pub fn parse(value: Option<&str>) -> std::result::Result<Self, String> {
        match value.unwrap_or("pane").trim().to_ascii_lowercase().as_str() {
            "pane" | "split" => Ok(Self::Pane),
            "window" => Ok(Self::Window),
            "session" => Ok(Self::Session),
            other => Err(format!(
                "unknown placement {other:?}; expected pane, window, or session"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum SplitDirection {
    #[default]
    Right,
    Down,
}

impl SplitDirection {
    pub fn parse(value: Option<&str>) -> std::result::Result<Self, String> {
        match value
            .unwrap_or("right")
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "right" | "horizontal" => Ok(Self::Right),
            "down" | "vertical" => Ok(Self::Down),
            other => Err(format!(
                "unknown direction {other:?}; expected right or down"
            )),
        }
    }
}

#[derive(Debug, clap::Args)]
pub struct StartArgs {
    /// Known agent CLI to launch. codex expands the local cx profile (codex --yolo).
    #[arg(long, value_enum)]
    pub agent: AgentProgram,
    /// Create a split pane (default), window, or independent session.
    #[arg(long, value_enum, default_value = "pane")]
    pub placement: Placement,
    /// tmux pane/window target. Defaults to `TMUX_PANE`; unused for session placement.
    #[arg(long)]
    pub target: Option<String>,
    /// Working directory. Defaults to the current directory.
    #[arg(long)]
    pub cwd: Option<PathBuf>,
    /// Initial task. Omit to start an interactive agent with no first prompt.
    #[arg(long)]
    pub prompt: Option<String>,
    /// Window/session name. Session placement derives it from cwd when omitted.
    #[arg(long)]
    pub name: Option<String>,
    /// Managed work/ticket. Reuses its tmux session or creates it once.
    #[arg(long)]
    pub work: Option<String>,
    /// Optional agent role stored on the pane, for example reviewer.
    #[arg(long)]
    pub role: Option<String>,
    /// Optional short task label stored on the pane.
    #[arg(long)]
    pub task: Option<String>,
    /// Split to the right (default) or below the target pane.
    #[arg(long, value_enum, default_value = "right")]
    pub direction: SplitDirection,
    /// Emit the structured result as JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct WorkStartArgs {
    /// Work/ticket id. One managed tmux session is created per id.
    pub work: String,
    /// First allowlisted agent, or another agent to add when the work exists.
    #[arg(long, value_enum)]
    pub agent: AgentProgram,
    /// Work directory. Defaults to current directory; checked on reuse.
    #[arg(long)]
    pub cwd: Option<PathBuf>,
    /// Initial task.
    #[arg(long)]
    pub prompt: Option<String>,
    /// Optional agent role, for example implementer or reviewer.
    #[arg(long)]
    pub role: Option<String>,
    /// Optional short task label.
    #[arg(long)]
    pub task: Option<String>,
    /// Split a reused work session to the right or below.
    #[arg(long, value_enum, default_value = "right")]
    pub direction: SplitDirection,
    /// Emit the structured result as JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone)]
pub struct StartRequest {
    pub agent: AgentProgram,
    pub placement: Placement,
    pub target: Option<String>,
    pub cwd: Option<PathBuf>,
    pub prompt: Option<String>,
    pub name: Option<String>,
    pub work: Option<String>,
    pub role: Option<String>,
    pub task: Option<String>,
    pub direction: SplitDirection,
}

impl From<&StartArgs> for StartRequest {
    fn from(args: &StartArgs) -> Self {
        Self {
            agent: args.agent,
            placement: args.placement,
            target: args.target.clone(),
            cwd: args.cwd.clone(),
            prompt: args.prompt.clone(),
            name: args.name.clone(),
            work: args.work.clone(),
            role: args.role.clone(),
            task: args.task.clone(),
            direction: args.direction,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StartResult {
    pub pane: String,
    pub agent: AgentProgram,
    pub placement: Placement,
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work: Option<String>,
    pub created_work: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    pub cwd: PathBuf,
    pub prompt_supplied: bool,
}

pub fn run(args: StartArgs) -> Result<()> {
    let json = args.json;
    let result = start(StartRequest::from(&args))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!(
            "started {} in {} {} (cwd {})",
            result.agent.label(),
            match result.placement {
                Placement::Pane => "pane",
                Placement::Window => "window pane",
                Placement::Session => "session pane",
            },
            result.pane,
            result.cwd.display()
        );
    }
    Ok(())
}

pub fn run_work_start(args: WorkStartArgs) -> Result<()> {
    let json = args.json;
    let result = start(StartRequest {
        agent: args.agent,
        placement: Placement::Pane,
        target: None,
        cwd: args.cwd,
        prompt: args.prompt,
        name: None,
        work: Some(args.work),
        role: args.role,
        task: args.task,
        direction: args.direction,
    })?;
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!(
            "{} agent {} in work {} (session {}, cwd {})",
            if result.created_work {
                "created"
            } else {
                "added"
            },
            result.pane,
            result.work.as_deref().unwrap_or("-"),
            result.name.as_deref().unwrap_or("-"),
            result.cwd.display()
        );
    }
    Ok(())
}

/// Start one allowlisted agent in a detached tmux surface and return its exact
/// pane id. The operation is synchronous and should be wrapped in
/// `spawn_blocking` by async callers.
pub fn start(mut request: StartRequest) -> Result<StartResult> {
    let PreparedLaunch {
        cwd,
        work,
        created_work,
    } = prepare_launch(&mut request)?;
    let prompt = request
        .prompt
        .as_deref()
        .filter(|prompt| !prompt.trim().is_empty());
    let command = request.agent.launch_command(prompt);
    let args = tmux_args(&request, &cwd, &command)?;
    let output = muxa::tmux::tmux_command()
        .args(&args)
        .output()
        .context("run tmux agent launcher")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(
            "tmux {} failed{}",
            args.first().map_or("command", String::as_str),
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            }
        );
    }
    let pane = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with('%'))
        .ok_or_else(|| anyhow::anyhow!("tmux created the surface but returned no pane id"))?
        .to_string();

    let mark = (|| {
        if created_work {
            let session = request
                .name
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("created work has no tmux session name"))?;
            crate::tmux_work::mark_work(
                session,
                work.as_deref().expect("created work has id"),
                &cwd,
            )?;
        }
        crate::tmux_work::mark_agent(
            &pane,
            request.agent.label(),
            work.as_deref(),
            request.role.as_deref(),
            request.task.as_deref(),
        )
    })();
    if let Err(error) = mark {
        crate::tmux_work::cleanup_pane(&pane);
        return Err(error).context("record muxa tmux metadata");
    }

    Ok(StartResult {
        pane,
        agent: request.agent,
        placement: request.placement,
        name: request.name,
        work,
        created_work,
        role: request.role,
        task: request.task,
        cwd,
        prompt_supplied: prompt.is_some(),
    })
}

struct PreparedLaunch {
    cwd: PathBuf,
    work: Option<String>,
    created_work: bool,
}

fn prepare_launch(request: &mut StartRequest) -> Result<PreparedLaunch> {
    let work = request
        .work
        .as_deref()
        .map(crate::tmux_work::normalize_work_id)
        .transpose()?;
    let existing_work = work
        .as_deref()
        .map(crate::tmux_work::find_work)
        .transpose()?
        .flatten();

    if work.is_some()
        && (request.placement != Placement::Pane
            || request.target.is_some()
            || request.name.is_some())
    {
        bail!("--work uses its managed session; do not combine it with --placement, --target, or --name");
    }

    let requested_cwd = request.cwd.take();
    let cwd_source = existing_work
        .as_ref()
        .map_or_else(
            || requested_cwd.clone().map_or_else(std::env::current_dir, Ok),
            |existing| Ok(existing.cwd.clone()),
        )
        .context("resolve current directory")?;
    let cwd = std::fs::canonicalize(&cwd_source)
        .with_context(|| format!("resolve cwd {}", cwd_source.display()))?;
    if !cwd.is_dir() {
        bail!("cwd is not a directory: {}", cwd.display());
    }
    if let (Some(existing), Some(requested)) = (&existing_work, requested_cwd) {
        let requested = std::fs::canonicalize(&requested)
            .with_context(|| format!("resolve cwd {}", requested.display()))?;
        if requested != cwd {
            bail!(
                "work {} already uses cwd {}; requested {}",
                existing.work,
                cwd.display(),
                requested.display()
            );
        }
    }

    let created_work = work.is_some() && existing_work.is_none();
    if let Some(existing) = &existing_work {
        request.placement = Placement::Pane;
        request.target = Some(existing.session.clone());
        request.name = Some(existing.session.clone());
    } else if let Some(work) = work.as_deref() {
        request.placement = Placement::Session;
        request.target = None;
        request.name = Some(crate::tmux_work::session_name_for_work(work)?);
    }

    // new-window rejects a pane id even though pane ids are the stable target
    // Muxa exposes to callers. Resolve either a pane or window input to the
    // owning tmux session and let tmux choose an unused window index.
    if request.placement == Placement::Window {
        let target = request
            .target
            .clone()
            .or_else(|| std::env::var("TMUX_PANE").ok())
            .filter(|target| !target.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("window placement needs --target or TMUX_PANE"))?;
        request.target = Some(resolve_window_session(&target)?);
    }
    if request.placement == Placement::Session {
        if request.target.is_some() {
            bail!("session placement does not accept --target");
        }
        let base = request
            .name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .map_or_else(
                || session_base_name(cwd.to_string_lossy().as_ref()),
                sanitize_session_name,
            );
        let existing = existing_session_names();
        request.name = Some(unique_session_name(base, |candidate| {
            existing.iter().any(|name| name == candidate)
        }));
    }
    Ok(PreparedLaunch {
        cwd,
        work,
        created_work,
    })
}

fn resolve_window_session(target: &str) -> Result<String> {
    let output = muxa::tmux::tmux_command()
        .args(["display-message", "-p", "-t", target, "#{session_name}"])
        .output()
        .context("resolve tmux window target")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(
            "cannot resolve tmux target {target:?}{}",
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            }
        );
    }
    let session = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if session.is_empty() {
        bail!("tmux target {target:?} resolved to an empty session");
    }
    Ok(session)
}

fn tmux_args(request: &StartRequest, cwd: &Path, command: &str) -> Result<Vec<String>> {
    let cwd = cwd
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("cwd is not valid UTF-8: {}", cwd.display()))?;
    let current_target = || {
        request
            .target
            .clone()
            .or_else(|| std::env::var("TMUX_PANE").ok())
            .filter(|target| !target.trim().is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{} placement needs --target or TMUX_PANE",
                    match request.placement {
                        Placement::Pane => "pane",
                        Placement::Window => "window",
                        Placement::Session => "session",
                    }
                )
            })
    };

    let args = match request.placement {
        Placement::Pane => {
            let target = current_target()?;
            let split = match request.direction {
                SplitDirection::Right => "-h",
                SplitDirection::Down => "-v",
            };
            vec![
                "split-window".into(),
                split.into(),
                "-d".into(),
                "-P".into(),
                "-F".into(),
                "#{pane_id}".into(),
                "-t".into(),
                target,
                "-c".into(),
                cwd.into(),
                command.into(),
            ]
        }
        Placement::Window => {
            let target = current_target()?;
            let mut args = vec![
                "new-window".into(),
                "-d".into(),
                "-P".into(),
                "-F".into(),
                "#{pane_id}".into(),
                "-t".into(),
                target,
            ];
            if let Some(name) = request
                .name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
            {
                args.extend(["-n".into(), name.into()]);
            }
            args.extend(["-c".into(), cwd.into(), command.into()]);
            args
        }
        Placement::Session => {
            if request.target.is_some() {
                bail!("session placement does not accept --target");
            }
            let name = request
                .name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .map_or_else(|| session_base_name(cwd), sanitize_session_name);
            vec![
                "new-session".into(),
                "-d".into(),
                "-P".into(),
                "-F".into(),
                "#{pane_id}".into(),
                "-s".into(),
                name,
                "-c".into(),
                cwd.into(),
                command.into(),
            ]
        }
    };
    Ok(args)
}

fn shell_single_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn session_base_name(cwd: &str) -> String {
    Path::new(cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| "agent".into(), sanitize_session_name)
}

fn sanitize_session_name(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|ch| if ch == '.' || ch == ':' { '-' } else { ch })
        .collect();
    if cleaned.is_empty() {
        "agent".into()
    } else {
        cleaned
    }
}

fn existing_session_names() -> Vec<String> {
    muxa::tmux::tmux_command()
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn unique_session_name(base: String, exists: impl Fn(&str) -> bool) -> String {
    if !exists(&base) {
        return base;
    }
    (2..10_000)
        .map(|suffix| format!("{base}-{suffix}"))
        .find(|candidate| !exists(candidate))
        .unwrap_or_else(|| format!("{base}-overflow"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(agent: AgentProgram, placement: Placement) -> StartRequest {
        StartRequest {
            agent,
            placement,
            target: Some("%9".into()),
            cwd: Some(PathBuf::from("/tmp")),
            prompt: Some("review June's changes; don't edit".into()),
            name: None,
            work: None,
            role: None,
            task: None,
            direction: SplitDirection::Right,
        }
    }

    #[test]
    fn codex_profile_expands_cx_and_quotes_the_prompt() {
        assert_eq!(
            AgentProgram::Codex.launch_command(Some("review June's changes; don't edit")),
            "codex --yolo 'review June'\\''s changes; don'\\''t edit'"
        );
    }

    #[test]
    fn pane_plan_is_detached_and_returns_the_pane_id_format() {
        let request = request(AgentProgram::Codex, Placement::Pane);
        let args = tmux_args(&request, Path::new("/tmp"), "codex --yolo").unwrap();
        assert_eq!(args[0], "split-window");
        assert!(args.iter().any(|arg| arg == "-d"));
        assert!(args.windows(2).any(|pair| pair == ["-F", "#{pane_id}"]));
        assert!(args.windows(2).any(|pair| pair == ["-t", "%9"]));
        assert_eq!(args.last().unwrap(), "codex --yolo");
    }

    #[test]
    fn window_and_session_plans_use_the_requested_surface() {
        let mut window = request(AgentProgram::Claude, Placement::Window);
        window.target = Some("muxa".into());
        window.name = Some("review".into());
        let args = tmux_args(&window, Path::new("/tmp"), "claude").unwrap();
        assert_eq!(args[0], "new-window");
        assert!(args.windows(2).any(|pair| pair == ["-n", "review"]));
        assert!(args.windows(2).any(|pair| pair == ["-t", "muxa"]));

        let mut session = request(AgentProgram::Gemini, Placement::Session);
        session.target = None;
        session.name = Some("cal.7041:review".into());
        let args = tmux_args(&session, Path::new("/tmp"), "gemini").unwrap();
        assert_eq!(args[0], "new-session");
        assert!(args
            .windows(2)
            .any(|pair| pair == ["-s", "cal-7041-review"]));
    }

    #[test]
    fn parsers_accept_short_agent_and_placement_aliases() {
        assert_eq!(AgentProgram::parse("cx").unwrap(), AgentProgram::Codex);
        assert_eq!(Placement::parse(Some("split")).unwrap(), Placement::Pane);
        assert_eq!(
            SplitDirection::parse(Some("vertical")).unwrap(),
            SplitDirection::Down
        );
    }

    #[test]
    fn repeated_session_names_receive_a_stable_suffix() {
        let existing = ["review", "review-2"];
        assert_eq!(
            unique_session_name("review".into(), |name| existing.contains(&name)),
            "review-3"
        );
    }
}
