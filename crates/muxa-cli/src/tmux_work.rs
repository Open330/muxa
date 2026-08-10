//! Managed tmux work/session and agent/pane lifecycle.
//!
//! Muxa's tmux policy is deliberately narrow:
//! - one managed session represents one work item (normally a ticket);
//! - one managed pane represents one coding agent;
//! - windows are layout only and carry no work identity.
//!
//! Identity is stored in tmux user options so it survives muxad and MCP
//! process restarts without adding another database.

use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use serde::Serialize;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

const WORK_ID_OPTION: &str = "@muxa_work_id";
const WORK_CWD_OPTION: &str = "@muxa_work_cwd";
const MANAGED_WORK_OPTION: &str = "@muxa_managed_work";
const MANAGED_AGENT_OPTION: &str = "@muxa_managed_agent";
const AGENT_OPTION: &str = "@muxa_agent";
const AGENT_ROLE_OPTION: &str = "@muxa_agent_role";
const AGENT_TASK_OPTION: &str = "@muxa_agent_task";
const PANE_WORK_OPTION: &str = "@muxa_agent_work_id";

const SESSION_FORMAT: &str = "#{session_name}\t#{@muxa_work_id}\t#{@muxa_work_cwd}\t#{@muxa_managed_work}\t#{session_attached}\t#{session_windows}";
const PANE_FORMAT: &str = "#{session_name}\t#{pane_id}\t#{@muxa_agent}\t#{@muxa_agent_role}\t#{@muxa_agent_task}\t#{pane_current_command}\t#{pane_current_path}\t#{@muxa_managed_agent}\t#{@muxa_agent_work_id}";

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ManagedAgentPane {
    pub pane: String,
    pub agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    pub command: String,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WorkInfo {
    pub work: String,
    pub session: String,
    pub cwd: PathBuf,
    pub attached_clients: u32,
    pub windows: u32,
    pub agents: Vec<ManagedAgentPane>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum AgentControlAction {
    Interrupt,
    Terminate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManageAction {
    ListWork,
    ShowWork,
    InterruptAgent,
    TerminateAgent,
    CloseWork,
}

impl ManageAction {
    pub fn parse(value: &str) -> std::result::Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "list_work" | "list" => Ok(Self::ListWork),
            "show_work" | "show" => Ok(Self::ShowWork),
            "interrupt_agent" | "interrupt" | "abort" => Ok(Self::InterruptAgent),
            "terminate_agent" | "terminate" | "kill" => Ok(Self::TerminateAgent),
            "close_work" | "close" => Ok(Self::CloseWork),
            other => Err(format!(
                "unknown tmux action {other:?}; expected list_work, show_work, \
                 interrupt_agent, terminate_agent, or close_work"
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ManageRequest {
    pub action: ManageAction,
    pub pane: Option<String>,
    pub work: Option<String>,
    pub confirm: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ManageResult {
    Works {
        works: Vec<WorkInfo>,
    },
    Work {
        work: WorkInfo,
    },
    AgentControl {
        action: AgentControlAction,
        pane: String,
    },
    WorkClosed {
        work: String,
        session: String,
    },
}

#[derive(Debug, clap::Args)]
pub struct AgentControlArgs {
    /// Exact managed tmux pane id, for example %42.
    #[arg(long)]
    pub pane: String,
    /// Interrupt the current turn or terminate the whole pane.
    #[arg(long, value_enum)]
    pub action: AgentControlAction,
    /// Confirm the destructive terminate action.
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Emit JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct WorkListArgs {
    /// Emit JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct WorkShowArgs {
    /// Ticket/work id, for example CAL-7041.
    pub work: String,
    /// Emit JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct WorkCloseArgs {
    /// Ticket/work id, for example CAL-7041.
    pub work: String,
    /// Skip the confirmation prompt.
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Emit JSON.
    #[arg(long)]
    pub json: bool,
}

pub fn run_agent_control(args: AgentControlArgs) -> Result<()> {
    if args.action == AgentControlAction::Terminate
        && !confirm_destructive(
            args.yes,
            &format!("Terminate managed agent pane {}?", args.pane),
        )?
    {
        println!("cancelled");
        return Ok(());
    }
    let result = control_agent(&args.pane, args.action, args.yes)?;
    print_result(&result, args.json)
}

pub fn run_work_list(args: WorkListArgs) -> Result<()> {
    let works = list_works()?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&json_works(&works))?);
    } else if works.is_empty() {
        println!("no muxa-managed work sessions");
    } else {
        for work in works {
            println!(
                "{}  session={}  agents={}  cwd={}",
                work.work,
                work.session,
                work.agents.len(),
                work.cwd.display()
            );
        }
    }
    Ok(())
}

pub fn run_work_show(args: WorkShowArgs) -> Result<()> {
    let work = find_work(&args.work)?
        .ok_or_else(|| anyhow::anyhow!("managed work {:?} not found", args.work))?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&work)?);
    } else {
        println!(
            "{}  session={}  cwd={}",
            work.work,
            work.session,
            work.cwd.display()
        );
        for agent in &work.agents {
            println!(
                "  {}  {}{}{}",
                agent.pane,
                agent.agent,
                agent
                    .role
                    .as_deref()
                    .map_or_else(String::new, |role| format!(" role={role}")),
                agent
                    .task
                    .as_deref()
                    .map_or_else(String::new, |task| format!(" task={task}"))
            );
        }
    }
    Ok(())
}

pub fn run_work_close(args: WorkCloseArgs) -> Result<()> {
    if !confirm_destructive(
        args.yes,
        &format!("Close work {} and all agent panes?", args.work),
    )? {
        println!("cancelled");
        return Ok(());
    }
    let result = close_work(&args.work, args.yes)?;
    print_result(&result, args.json)
}

pub fn manage(request: ManageRequest) -> Result<ManageResult> {
    match request.action {
        ManageAction::ListWork => Ok(ManageResult::Works {
            works: list_works()?,
        }),
        ManageAction::ShowWork => {
            let raw = required(request.work.as_deref(), "show_work requires work")?;
            let work =
                find_work(raw)?.ok_or_else(|| anyhow::anyhow!("managed work {raw:?} not found"))?;
            Ok(ManageResult::Work { work })
        }
        ManageAction::InterruptAgent => {
            let pane = required(request.pane.as_deref(), "interrupt_agent requires pane")?;
            control_agent(pane, AgentControlAction::Interrupt, true)
        }
        ManageAction::TerminateAgent => {
            let pane = required(request.pane.as_deref(), "terminate_agent requires pane")?;
            control_agent(pane, AgentControlAction::Terminate, request.confirm)
        }
        ManageAction::CloseWork => {
            let work = required(request.work.as_deref(), "close_work requires work")?;
            close_work(work, request.confirm)
        }
    }
}

pub fn normalize_work_id(raw: &str) -> Result<String> {
    let work = raw.trim().to_ascii_uppercase();
    if work.is_empty() {
        bail!("work id cannot be empty");
    }
    if work.len() > 128 {
        bail!("work id is too long (max 128 bytes)");
    }
    if work
        .chars()
        .any(|ch| ch == '\t' || ch == '\n' || ch == '\r')
    {
        bail!("work id cannot contain tabs or newlines");
    }
    Ok(work)
}

pub fn find_work(raw: &str) -> Result<Option<WorkInfo>> {
    let wanted = normalize_work_id(raw)?;
    Ok(list_works()?.into_iter().find(|work| work.work == wanted))
}

pub fn list_works() -> Result<Vec<WorkInfo>> {
    let sessions = tmux_output_allow_no_server(&["list-sessions", "-F", SESSION_FORMAT])?;
    let panes = tmux_output_allow_no_server(&["list-panes", "-a", "-F", PANE_FORMAT])?;
    Ok(parse_works(&sessions, &panes))
}

pub fn session_name_for_work(work: &str) -> Result<String> {
    let normalized = normalize_work_id(work)?;
    let base = sanitize_session_name(&normalized.to_ascii_lowercase());
    let existing = all_session_names()?;
    Ok(unique_name(base, |candidate| {
        existing.iter().any(|name| name == candidate)
    }))
}

pub fn mark_work(session: &str, work: &str, cwd: &Path) -> Result<()> {
    let work = normalize_work_id(work)?;
    let cwd = cwd
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("work cwd is not valid UTF-8: {}", cwd.display()))?;
    set_option(false, session, WORK_ID_OPTION, &work)?;
    set_option(false, session, WORK_CWD_OPTION, cwd)?;
    set_option(false, session, MANAGED_WORK_OPTION, "1")?;
    Ok(())
}

pub fn mark_agent(
    pane: &str,
    agent: &str,
    work: Option<&str>,
    role: Option<&str>,
    task: Option<&str>,
) -> Result<()> {
    validate_pane_id(pane)?;
    set_option(true, pane, AGENT_OPTION, &metadata(agent, 64)?)?;
    set_option(true, pane, MANAGED_AGENT_OPTION, "1")?;
    if let Some(work) = work {
        set_option(true, pane, PANE_WORK_OPTION, &normalize_work_id(work)?)?;
    }
    if let Some(role) = role.filter(|value| !value.trim().is_empty()) {
        set_option(true, pane, AGENT_ROLE_OPTION, &metadata(role, 64)?)?;
    }
    if let Some(task) = task.filter(|value| !value.trim().is_empty()) {
        set_option(true, pane, AGENT_TASK_OPTION, &metadata(task, 256)?)?;
    }
    Ok(())
}

pub fn cleanup_pane(pane: &str) {
    if validate_pane_id(pane).is_ok() {
        let _ = muxa::tmux::tmux_command()
            .args(["kill-pane", "-t", pane])
            .status();
    }
}

fn control_agent(pane: &str, action: AgentControlAction, confirm: bool) -> Result<ManageResult> {
    validate_pane_id(pane)?;
    if action == AgentControlAction::Terminate && !confirm {
        bail!("terminate_agent requires confirm=true");
    }
    ensure_managed_agent(pane)?;
    let args = match action {
        AgentControlAction::Interrupt => vec!["send-keys", "-t", pane, "C-c"],
        AgentControlAction::Terminate => vec!["kill-pane", "-t", pane],
    };
    tmux_status(&args)?;
    Ok(ManageResult::AgentControl {
        action,
        pane: pane.to_string(),
    })
}

fn close_work(raw: &str, confirm: bool) -> Result<ManageResult> {
    if !confirm {
        bail!("close_work requires confirm=true");
    }
    let work = find_work(raw)?.ok_or_else(|| anyhow::anyhow!("managed work {raw:?} not found"))?;
    tmux_status(&["kill-session", "-t", &format!("={}", work.session)])?;
    Ok(ManageResult::WorkClosed {
        work: work.work,
        session: work.session,
    })
}

fn ensure_managed_agent(pane: &str) -> Result<()> {
    let output = tmux_output(&[
        "display-message",
        "-p",
        "-t",
        pane,
        "#{@muxa_managed_agent}\t#{@muxa_agent}",
    ])?;
    let mut fields = output.trim().split('\t');
    if fields.next() != Some("1") || fields.next().is_none_or(str::is_empty) {
        bail!("pane {pane} is not a muxa-managed agent pane");
    }
    Ok(())
}

fn validate_pane_id(pane: &str) -> Result<()> {
    if pane
        .strip_prefix('%')
        .is_some_and(|digits| !digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit()))
    {
        Ok(())
    } else {
        bail!("pane must be an exact tmux pane id such as %42")
    }
}

fn metadata(raw: &str, max: usize) -> Result<String> {
    let value = raw.trim();
    if value.is_empty() {
        bail!("metadata value cannot be empty");
    }
    if value.len() > max {
        bail!("metadata value is too long (max {max} bytes)");
    }
    if value
        .chars()
        .any(|ch| ch == '\t' || ch == '\n' || ch == '\r')
    {
        bail!("metadata value cannot contain tabs or newlines");
    }
    Ok(value.to_string())
}

fn parse_works(sessions: &str, panes: &str) -> Vec<WorkInfo> {
    let mut works = Vec::new();
    for line in sessions.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 6 || fields[1].trim().is_empty() || fields[3] != "1" {
            continue;
        }
        let session = fields[0].to_string();
        let work = fields[1].trim().to_ascii_uppercase();
        let agents = panes
            .lines()
            .filter_map(|line| parse_agent_pane(line, &session, &work))
            .collect();
        works.push(WorkInfo {
            work,
            session,
            cwd: PathBuf::from(fields[2]),
            attached_clients: fields[4].parse().unwrap_or(0),
            windows: fields[5].parse().unwrap_or(0),
            agents,
        });
    }
    works.sort_by(|left, right| left.work.cmp(&right.work));
    works
}

fn parse_agent_pane(line: &str, session: &str, work: &str) -> Option<ManagedAgentPane> {
    let fields: Vec<&str> = line.split('\t').collect();
    if fields.len() < 9
        || fields[0] != session
        || fields[2].trim().is_empty()
        || fields[7] != "1"
        || !fields[8].eq_ignore_ascii_case(work)
    {
        return None;
    }
    Some(ManagedAgentPane {
        pane: fields[1].to_string(),
        agent: fields[2].to_string(),
        role: option(fields[3]),
        task: option(fields[4]),
        command: fields[5].to_string(),
        cwd: PathBuf::from(fields[6]),
    })
}

fn option(value: &str) -> Option<String> {
    (!value.trim().is_empty()).then(|| value.to_string())
}

fn all_session_names() -> Result<Vec<String>> {
    Ok(
        tmux_output_allow_no_server(&["list-sessions", "-F", "#{session_name}"])?
            .lines()
            .map(str::to_string)
            .collect(),
    )
}

fn sanitize_session_name(name: &str) -> String {
    let mut cleaned = String::new();
    for ch in name.trim().chars() {
        let ch = if ch == '.' || ch == ':' || ch.is_whitespace() {
            '-'
        } else {
            ch
        };
        if ch != '-' || !cleaned.ends_with('-') {
            cleaned.push(ch);
        }
    }
    let cleaned = cleaned.trim_matches('-').to_string();
    if cleaned.is_empty() {
        "work".into()
    } else {
        cleaned
    }
}

fn unique_name(base: String, exists: impl Fn(&str) -> bool) -> String {
    if !exists(&base) {
        return base;
    }
    (2..10_000)
        .map(|suffix| format!("{base}-{suffix}"))
        .find(|candidate| !exists(candidate))
        .unwrap_or_else(|| format!("{base}-overflow"))
}

fn set_option(pane: bool, target: &str, key: &str, value: &str) -> Result<()> {
    let mut args = vec!["set-option"];
    if pane {
        args.push("-p");
    }
    args.extend(["-t", target, key, value]);
    tmux_status(&args)
}

fn tmux_status(args: &[&str]) -> Result<()> {
    let output = muxa::tmux::tmux_command()
        .args(args)
        .output()
        .with_context(|| format!("run tmux {}", args.first().unwrap_or(&"command")))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    bail!(
        "tmux {} failed{}",
        args.first().unwrap_or(&"command"),
        if stderr.is_empty() {
            String::new()
        } else {
            format!(": {stderr}")
        }
    )
}

fn tmux_output(args: &[&str]) -> Result<String> {
    let output = muxa::tmux::tmux_command()
        .args(args)
        .output()
        .with_context(|| format!("run tmux {}", args.first().unwrap_or(&"command")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(
            "tmux {} failed{}",
            args.first().unwrap_or(&"command"),
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            }
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn tmux_output_allow_no_server(args: &[&str]) -> Result<String> {
    match tmux_output(args) {
        Ok(output) => Ok(output),
        Err(error)
            if error.to_string().contains("no server running")
                || error.to_string().contains("no sessions") =>
        {
            Ok(String::new())
        }
        Err(error) => Err(error),
    }
}

fn confirm_destructive(yes: bool, prompt: &str) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("confirmation requires an interactive terminal; pass --yes");
    }
    Ok(cliclack::confirm(prompt).initial_value(false).interact()?)
}

fn required<'a>(value: Option<&'a str>, message: &str) -> Result<&'a str> {
    value
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!(message.to_string()))
}

fn json_works(works: &[WorkInfo]) -> serde_json::Value {
    serde_json::json!({ "works": works })
}

fn print_result(result: &ManageResult, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(result)?);
    } else {
        match result {
            ManageResult::AgentControl { action, pane } => {
                println!("{action:?} agent pane {pane}");
            }
            ManageResult::WorkClosed { work, session } => {
                println!("closed work {work} (session {session})");
            }
            ManageResult::Works { .. } | ManageResult::Work { .. } => {
                println!("{}", serde_json::to_string_pretty(result)?);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_ids_are_case_normalized_and_tmux_safe() {
        assert_eq!(normalize_work_id(" cal-7041 ").unwrap(), "CAL-7041");
        assert!(normalize_work_id("bad\nid").is_err());
        assert_eq!(
            sanitize_session_name(
                &normalize_work_id("CAL.7041: Review")
                    .unwrap()
                    .to_lowercase()
            ),
            "cal-7041-review"
        );
    }

    #[test]
    fn parser_keeps_only_managed_sessions_and_agent_panes() {
        let sessions = "cal-7041\tCAL-7041\t/repo\t1\t1\t2\n\
                        spoofed\tCAL-0000\t/tmp\t\t0\t1\n\
                        plain\t\t/tmp\t\t0\t1\n";
        let panes = "cal-7041\t%1\tcodex\timplementer\tmain\tcodex\t/repo\t1\tCAL-7041\n\
                     cal-7041\t%2\tcodex\treviewer\twrong\tcodex\t/repo\t1\tCAL-9999\n\
                     cal-7041\t%3\tcodex\treviewer\tunmanaged\tcodex\t/repo\t\tCAL-7041\n";
        let works = parse_works(sessions, panes);
        assert_eq!(works.len(), 1);
        assert_eq!(works[0].work, "CAL-7041");
        assert_eq!(works[0].agents.len(), 1);
        assert_eq!(works[0].agents[0].pane, "%1");
    }

    #[test]
    fn destructive_management_requires_explicit_confirmation() {
        let request = ManageRequest {
            action: ManageAction::TerminateAgent,
            pane: Some("%42".into()),
            work: None,
            confirm: false,
        };
        assert!(manage(request)
            .unwrap_err()
            .to_string()
            .contains("confirm=true"));
        assert_eq!(
            ManageAction::parse("close_work").unwrap(),
            ManageAction::CloseWork
        );
    }

    #[test]
    fn exact_pane_ids_are_required() {
        assert!(validate_pane_id("%42").is_ok());
        assert!(validate_pane_id("42").is_err());
        assert!(validate_pane_id("%4x").is_err());
    }
}
