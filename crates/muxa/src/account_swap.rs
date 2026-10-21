//! Move a running agent onto another `aas` account and resume its
//! conversation in place.
//!
//! When one account's usage cap is spent, the conversation itself is still
//! good: `aas` profiles share `projects/` (Claude) and `sessions/` (Codex),
//! so the same provider session id resumes under any account. A swap stops
//! the agent in its pane, types `aas e <next> … --resume <id>` back into the
//! pane's shell, and optionally sends a follow-up prompt so the interrupted
//! turn carries on.
//!
//! The module splits into a pure half — which agents to touch, what command
//! line to type, which account each gets — and [`execute`], the one place
//! that signals processes and types into panes. `muxa swap` and the
//! `swap_account` automation action both go through [`run`].

use crate::backend::SharedBackend;
use crate::event::{AgentKind, AgentState};
use crate::process_snapshot::ProcessInfo;
use crate::state::Agent;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

/// How long a stopped agent gets to exit on `SIGTERM` before `SIGKILL`.
const STOP_GRACE: Duration = Duration::from_secs(5);
/// How long the relaunched agent gets to show up in the pane's process tree.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a resumed TUI gets to finish drawing before the follow-up prompt.
const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// The screen must hold still this long to count as drawn.
const READY_QUIET: Duration = Duration::from_millis(1500);
/// Time for the shell to redraw its prompt after the agent exits.
const SHELL_SETTLE: Duration = Duration::from_millis(700);
/// How long `aas candidates` gets before a swap gives up on it.
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);

/// `[swap]` in `config.toml`: defaults shared by `muxa swap` and the
/// `swap_account` automation action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SwapConfig {
    /// What to do with an agent started without `aas` (`claude` / `codex`
    /// run bare).
    pub bare: BareMode,
    /// Prompt sent after the resume so the interrupted turn continues.
    /// Empty disables it.
    pub continue_text: String,
    /// Which swapped agents get [`Self::continue_text`].
    pub continue_when: ContinueWhen,
    /// The `aas` binary muxa queries for candidates. The daemon may not
    /// inherit the login shell's `PATH`, so a bare `aas` is also looked for
    /// in the usual install locations.
    pub aas: String,
}

impl Default for SwapConfig {
    fn default() -> Self {
        Self {
            bare: BareMode::Skip,
            continue_text: "continue".into(),
            continue_when: ContinueWhen::Limited,
            aas: "aas".into(),
        }
    }
}

/// An agent launched without the `aas` wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum BareMode {
    /// Leave it alone.
    Skip,
    /// Relaunch it as `aas e <account> …`.
    Wrap,
}

/// Which swapped agents get the follow-up prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ContinueWhen {
    /// Only agents that were stopped by a usage cap: their turn was cut off.
    Limited,
    Always,
    Never,
}

/// Which agents a swap touches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwapScope {
    /// Exactly these panes (or provider session ids).
    Targets(Vec<String>),
    /// Every agent currently stopped by a usage cap.
    Limited,
    /// Every Claude / Codex agent.
    All,
}

/// Which account each swapped agent moves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountChoice {
    /// The best-ranked eligible account from `aas candidates`.
    Auto,
    /// Round-robin over the eligible accounts, so a batch does not drain
    /// one account at once.
    Spread,
    /// This account, by its `aas` name.
    Named(String),
}

impl AccountChoice {
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw.trim() {
            "" | "auto" => Self::Auto,
            "spread" => Self::Spread,
            name => Self::Named(name.to_owned()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapRequest {
    pub scope: SwapScope,
    /// Restrict to one provider. `None` means Claude and Codex.
    pub provider: Option<AgentKind>,
    pub to: AccountChoice,
    /// Also stop agents that are mid-turn.
    pub force: bool,
    pub bare: BareMode,
    /// The follow-up prompt, already resolved against [`ContinueWhen`]'s
    /// "who gets it" by [`SwapRequest::continue_for`].
    pub continue_text: Option<String>,
    pub continue_when: ContinueWhen,
    /// Plan only: report what would happen, touch nothing.
    pub dry_run: bool,
}

impl SwapRequest {
    /// A request with the `[swap]` defaults and the given scope.
    #[must_use]
    pub fn new(scope: SwapScope, config: &SwapConfig) -> Self {
        Self {
            scope,
            provider: None,
            to: AccountChoice::Auto,
            force: false,
            bare: config.bare,
            continue_text: Some(config.continue_text.clone()).filter(|t| !t.trim().is_empty()),
            continue_when: config.continue_when,
            dry_run: false,
        }
    }

    fn continue_for(&self, was_limited: bool) -> Option<&str> {
        let wanted = match self.continue_when {
            ContinueWhen::Never => false,
            ContinueWhen::Always => true,
            ContinueWhen::Limited => was_limited,
        };
        self.continue_text.as_deref().filter(|_| wanted)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum SwapStatus {
    /// Relaunched on the new account.
    Swapped,
    /// `dry_run`: this is what would have been done.
    Planned,
    /// Deliberately left alone.
    Skipped,
    /// Tried, and did not get as far as a running agent.
    Failed,
}

/// One agent's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapOutcome {
    pub pane: Option<String>,
    pub agent_session_id: String,
    pub kind: AgentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    pub status: SwapStatus,
    /// The line typed (or that would be typed) into the pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

impl SwapOutcome {
    fn new(agent: &Agent, status: SwapStatus, note: impl Into<String>) -> Self {
        Self {
            pane: agent.pane.clone(),
            agent_session_id: agent.session_id.clone(),
            kind: agent.kind,
            from: None,
            to: None,
            status,
            command: None,
            note: note.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Pure planning
// ---------------------------------------------------------------------------

/// The `aas` provider name for an agent kind, when `aas` can switch it.
#[must_use]
pub fn provider_name(kind: AgentKind) -> Option<&'static str> {
    match kind {
        AgentKind::ClaudeCode => Some("claude"),
        AgentKind::Codex => Some("codex"),
        _ => None,
    }
}

/// Stopped by a usage cap: the same predicate the `rate_limited`
/// automation event uses.
#[must_use]
pub fn is_rate_limited(agent: &Agent) -> bool {
    agent.state == AgentState::Error && agent.rate_limit_scope.is_some()
}

fn is_busy(agent: &Agent) -> bool {
    matches!(agent.state, AgentState::Working | AgentState::Starting)
}

fn resumable_id(session_id: &str) -> bool {
    !session_id.is_empty() && !session_id.starts_with("synthetic")
}

/// Split `agents` into the ones the request covers and the reasons the rest
/// that matched its scope were left alone. Agents outside the scope are not
/// reported at all — `--limited` says nothing about healthy agents.
#[must_use]
pub fn select<'a>(
    agents: &'a [Agent],
    request: &SwapRequest,
) -> (Vec<&'a Agent>, Vec<SwapOutcome>) {
    let mut chosen = Vec::new();
    let mut skipped = Vec::new();
    for agent in agents {
        let in_scope = match &request.scope {
            SwapScope::Targets(targets) => targets.iter().any(|target| {
                agent.pane.as_deref() == Some(target.as_str()) || agent.session_id == *target
            }),
            SwapScope::Limited => is_rate_limited(agent),
            SwapScope::All => provider_name(agent.kind).is_some(),
        };
        if !in_scope || request.provider.is_some_and(|kind| kind != agent.kind) {
            continue;
        }
        let skip = |note: &str| SwapOutcome::new(agent, SwapStatus::Skipped, note);
        if provider_name(agent.kind).is_none() {
            skipped.push(skip("aas cannot switch this agent's provider"));
        } else if agent.pane.is_none() {
            skipped.push(skip("not in a multiplexer pane"));
        } else if is_busy(agent) && !request.force {
            skipped.push(skip("mid-turn; pass --force to stop it anyway"));
        } else {
            chosen.push(agent);
        }
    }
    (chosen, skipped)
}

/// One row of `aas candidates --json`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub name: String,
    pub provider: String,
    #[serde(default)]
    pub eligible: bool,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub remaining_pct: Option<f64>,
}

#[derive(Deserialize)]
struct CandidateList {
    accounts: Vec<Candidate>,
}

/// Parse `aas candidates --json`, keeping `aas`'s own ranking.
pub fn parse_candidates(json: &str) -> Result<Vec<Candidate>, String> {
    serde_json::from_str::<CandidateList>(json)
        .map(|list| list.accounts)
        .map_err(|error| format!("unreadable `aas candidates --json` output: {error}"))
}

/// The account each target moves to, in target order. `current` is each
/// target's present account (when known); an agent is never "moved" onto
/// the account it is already on. `Err` carries the reason that target is
/// skipped.
#[must_use]
pub fn assign(
    current: &[Option<String>],
    candidates: &[Candidate],
    choice: &AccountChoice,
) -> Vec<Result<String, String>> {
    let eligible: Vec<&str> = candidates
        .iter()
        .filter(|c| c.eligible)
        .map(|c| c.name.as_str())
        .collect();
    let mut next = 0usize;
    current
        .iter()
        .map(|from| {
            let differs = |name: &str| from.as_deref() != Some(name);
            match choice {
                AccountChoice::Named(name) => {
                    if from.as_deref() == Some(name.as_str()) {
                        Err(format!("already on {name}"))
                    } else {
                        Ok(name.clone())
                    }
                }
                AccountChoice::Auto => eligible
                    .iter()
                    .find(|name| differs(name))
                    .map(|name| (*name).to_owned())
                    .ok_or_else(|| "no other account has quota left".to_owned()),
                AccountChoice::Spread => {
                    let pool: Vec<&str> = eligible.iter().copied().filter(|n| differs(n)).collect();
                    if pool.is_empty() {
                        return Err("no other account has quota left".to_owned());
                    }
                    let name = pool[next % pool.len()];
                    next += 1;
                    Ok(name.to_owned())
                }
            }
        })
        .collect()
}

/// How an agent was started, read back from its process tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchLine {
    /// The command line of the outermost relevant process: the `aas`
    /// wrapper when there is one, the agent itself otherwise.
    pub tokens: Vec<String>,
    /// Index into `tokens` of the account name, for an `aas e <name>` line.
    pub account_at: Option<usize>,
}

impl LaunchLine {
    #[must_use]
    pub fn account(&self) -> Option<&str> {
        self.account_at
            .and_then(|at| self.tokens.get(at))
            .map(String::as_str)
    }

    /// Read an `aas e|exec [--fallback <acct>]… <name> …` line. `None` when
    /// the line is not an `aas` run.
    #[must_use]
    pub fn from_aas(cmdline: &str) -> Option<Self> {
        let tokens: Vec<String> = cmdline.split_whitespace().map(str::to_owned).collect();
        // `aas …`, or `sh …/aas …` for a script on the way.
        let program = tokens
            .iter()
            .take(2)
            .position(|t| crate::discovery::command_name(t) == "aas")?;
        if !matches!(
            tokens.get(program + 1).map(String::as_str),
            Some("e" | "exec")
        ) {
            return None;
        }
        // Relaunch through `aas` itself, not the interpreter that ran it.
        let tokens = tokens[program..].to_vec();
        let mut at = 2;
        while let Some(token) = tokens.get(at) {
            match token.as_str() {
                "--fallback" => at += 2,
                flag if flag.starts_with("--fallback=") => at += 1,
                _ => break,
            }
        }
        (at < tokens.len()).then_some(Self {
            tokens,
            account_at: Some(at),
        })
    }

    #[must_use]
    pub fn bare(cmdline: &str) -> Self {
        Self {
            tokens: cmdline.split_whitespace().map(str::to_owned).collect(),
            account_at: None,
        }
    }
}

/// Flags that take a separate value, per agent. A resumed relaunch keeps
/// every flag and these values, and drops other positional words — which on
/// macOS, where `ps` joins argv, is how an initial prompt (`claude "fix
/// the bug"`) is kept from being sent a second time.
fn value_flags(kind: AgentKind) -> &'static [&'static str] {
    match kind {
        AgentKind::ClaudeCode => &[
            "--model",
            "--permission-mode",
            "--add-dir",
            "--append-system-prompt",
            "--system-prompt",
            "--allowedTools",
            "--allowed-tools",
            "--disallowedTools",
            "--disallowed-tools",
            "--mcp-config",
            "--settings",
            "--output-format",
            "--input-format",
            "--fallback-model",
            "--agents",
            "--agent",
            "--setting-sources",
            "--plugin-dir",
            "--max-turns",
        ],
        AgentKind::Codex => &[
            "-m",
            "--model",
            "-c",
            "--config",
            "-p",
            "--profile",
            "-s",
            "--sandbox",
            "-a",
            "--ask-for-approval",
            "-C",
            "--cd",
            "-i",
            "--image",
            "--enable",
            "--disable",
            "--add-dir",
        ],
        _ => &[],
    }
}

/// The agent's own arguments with any earlier resume removed and every
/// positional word dropped.
fn resume_free_args(args: &[String], kind: AgentKind) -> Vec<String> {
    let flags = value_flags(kind);
    let mut kept = Vec::new();
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        let arg = arg.as_str();
        match (kind, arg) {
            (AgentKind::ClaudeCode, "--resume" | "-r" | "--session-id") => {
                iter.next();
            }
            (AgentKind::ClaudeCode, "--continue" | "-c" | "--fork-session") => {}
            (AgentKind::ClaudeCode, _)
                if arg.starts_with("--resume=") || arg.starts_with("--session-id=") => {}
            (AgentKind::Codex, "resume") => {
                // `resume <id>` or `resume --last`.
                iter.next();
            }
            (_, "--") => kept.push(arg.to_owned()),
            (_, flag) if flag.starts_with('-') => {
                kept.push(flag.to_owned());
                if !flag.contains('=') && flags.contains(&flag) {
                    if let Some(value) = iter.next() {
                        kept.push(value.clone());
                    }
                }
            }
            // A positional: the initial prompt, or a word of it.
            _ => {}
        }
    }
    // A trailing `--` with nothing after it is noise.
    if kept.last().map(String::as_str) == Some("--") {
        kept.pop();
    }
    kept
}

fn resume_suffix(kind: AgentKind, session_id: &str) -> [String; 2] {
    match kind {
        AgentKind::Codex => ["resume".into(), session_id.into()],
        _ => ["--resume".into(), session_id.into()],
    }
}

/// The line to type back into the pane: the same launch, on `account`,
/// resuming `session_id`.
pub fn relaunch_line(
    line: &LaunchLine,
    kind: AgentKind,
    session_id: &str,
    account: &str,
    bare: BareMode,
) -> Result<String, String> {
    let mut words: Vec<String> = Vec::new();
    if let Some(at) = line.account_at {
        words.extend(line.tokens[..at].iter().cloned());
        words.push(account.to_owned());
        let rest = &line.tokens[at + 1..];
        // `aas e <name> claude …` runs another provider's UI; keep it.
        let (frontend, args) = match rest.first().map(String::as_str) {
            Some("claude" | "codex") => (Some(rest[0].clone()), &rest[1..]),
            _ => (None, rest),
        };
        words.extend(frontend);
        words.extend(resume_free_args(args, kind));
    } else {
        if bare == BareMode::Skip {
            return Err("started without aas; use --bare wrap to move it onto an account".into());
        }
        words.extend(["aas".to_owned(), "e".to_owned(), account.to_owned()]);
        words.extend(resume_free_args(line.tokens.get(1..).unwrap_or(&[]), kind));
    }
    words.extend(resume_suffix(kind, session_id));
    // Typed into the pane's own shell: PowerShell under rmux on Windows.
    Ok(crate::shell::Dialect::native().command_line(&words))
}

/// Whether `process` is the interactive `kind` agent (not Codex's
/// app-server or a headless `exec` run).
pub(crate) fn is_agent_process(process: &ProcessInfo, kind: AgentKind) -> bool {
    let mut tokens = process.cmdline.split_whitespace();
    // `node …/bin/codex` — the npm shim — names the agent in its second word.
    let named = tokens
        .by_ref()
        .take(2)
        .any(|token| crate::discovery::classify_command(token) == Some(kind));
    if !named {
        return false;
    }
    kind != AgentKind::Codex
        || !process.cmdline.split_whitespace().any(|token| {
            matches!(
                token,
                "app-server" | "exec" | "review" | "mcp-server" | "exec-server"
            )
        })
}

/// What a pane runs, found among the processes under its shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneLaunch {
    /// The agent process itself.
    pub agent_pid: u32,
    /// The direct child of the pane's process that leads to the agent: the
    /// `aas` wrapper, or the agent itself.
    pub top_pid: u32,
    pub line: LaunchLine,
    /// The session id the agent was started on (`--resume <id>`, `resume
    /// <id>`), which is all muxa has for a screen-detected row.
    pub resumed_id: Option<String>,
}

/// The provider session id an agent's own command line names.
fn argv_session_id(cmdline: &str, kind: AgentKind) -> Option<String> {
    let mut tokens = cmdline.split_whitespace();
    while let Some(token) = tokens.next() {
        let id = match (kind, token) {
            (AgentKind::ClaudeCode, "--resume" | "-r" | "--session-id")
            | (AgentKind::Codex, "resume") => tokens.next(),
            (AgentKind::ClaudeCode, _) => token
                .strip_prefix("--resume=")
                .or_else(|| token.strip_prefix("--session-id=")),
            _ => None,
        };
        if let Some(id) = id.filter(|id| uuid::Uuid::parse_str(id).is_ok()) {
            return Some(id.to_owned());
        }
    }
    None
}

/// Find the `kind` agent under the pane process `pane_pid`, and the `aas`
/// wrapper above it when there is one.
#[must_use]
pub(crate) fn find_launch(
    processes: &[ProcessInfo],
    pane_pid: u32,
    kind: AgentKind,
) -> Option<PaneLaunch> {
    let agent = processes
        .iter()
        .filter(|p| is_agent_process(p, kind))
        .min_by_key(|p| p.depth)?;
    let by_pid = |pid: u32| processes.iter().find(|p| p.pid == pid);
    let mut chain = vec![agent];
    let mut parent = agent.parent_pid;
    while parent != pane_pid {
        let process = by_pid(parent)?;
        chain.push(process);
        parent = process.parent_pid;
    }
    let top = *chain.last()?;
    let wrapper = chain
        .iter()
        .find_map(|process| LaunchLine::from_aas(&process.cmdline));
    Some(PaneLaunch {
        agent_pid: agent.pid,
        top_pid: top.pid,
        line: wrapper.unwrap_or_else(|| LaunchLine::bare(&agent.cmdline)),
        resumed_id: argv_session_id(&agent.cmdline, kind),
    })
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

/// Query `aas candidates --json` for one provider.
pub fn query_candidates(aas: &str, provider: &str) -> Result<Vec<Candidate>, String> {
    let program = find_aas(aas).unwrap_or_else(|| aas.to_owned());
    query_candidates_within(&program, provider, QUERY_TIMEOUT)
}

/// [`query_candidates`] against an already resolved `program`, given up on
/// once `timeout` passes so a hung `aas` cannot hold a caller. The deadline
/// covers reading its output too: a helper it left behind can hold the
/// pipes open after `aas` itself exits.
pub fn query_candidates_within(
    program: &str,
    provider: &str,
    timeout: Duration,
) -> Result<Vec<Candidate>, String> {
    use std::io::Read;
    let deadline = Instant::now() + timeout;
    let late = || {
        format!(
            "`{program} candidates` did not answer within {}s",
            timeout.as_secs()
        )
    };
    let mut child = std::process::Command::new(program)
        .args(["candidates", "--json", "--provider", provider])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not run `{program} candidates`: {error}"))?;
    // Drained on their own threads so a full pipe cannot stall the child.
    // A reader still blocked at the deadline is abandoned, not joined.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buf);
            }
            let _ = sender.send(buf);
        });
        receiver
    };
    let stdout = drain(child.stdout.take().map(|p| Box::new(p) as _));
    let stderr = drain(child.stderr.take().map(|p| Box::new(p) as _));
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            outcome => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(match outcome {
                    Err(error) => format!("`{program} candidates` failed: {error}"),
                    _ => late(),
                });
            }
        }
    };
    let collect = |receiver: std::sync::mpsc::Receiver<Vec<u8>>| {
        receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| late())
    };
    let stdout = collect(stdout)?;
    let stderr = collect(stderr)?;
    if !status.success() {
        return Err(format!(
            "`{program} candidates` failed: {}",
            String::from_utf8_lossy(&stderr).trim()
        ));
    }
    parse_candidates(&String::from_utf8_lossy(&stdout))
}

/// Where the configured `aas` is: a path as given, a bare name looked up
/// on `PATH` and then where `aas` is usually installed — a
/// launchd-started daemon has a minimal `PATH`. Always absolute, because a
/// launch runs it from another directory. `None` when none exists.
#[must_use]
pub fn find_aas(configured: &str) -> Option<String> {
    let found = if configured.contains(['/', '\\']) {
        Some(std::path::PathBuf::from(configured)).filter(|path| path.is_file())
    } else if cfg!(windows) {
        // `aas.exe` / `aas.cmd`: PATH lookup has to try the extensions.
        crate::shell::find_program(configured)
    } else {
        let home = std::env::var("HOME").unwrap_or_default();
        std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            .chain(
                [
                    "/opt/homebrew/bin".to_owned(),
                    "/usr/local/bin".to_owned(),
                    format!("{home}/.cargo/bin"),
                    format!("{home}/.local/bin"),
                ]
                .map(std::path::PathBuf::from),
            )
            .map(|dir| dir.join(configured))
            .find(|path| path.is_file())
    }?;
    std::path::absolute(found)
        .ok()
        .map(|path| path.display().to_string())
}

/// Plan and carry out `request` over `agents`. Blocking: it signals
/// processes and waits on panes, so async callers run it on a blocking
/// thread. Targets are swapped one at a time.
#[must_use]
pub fn run(
    request: &SwapRequest,
    agents: &[Agent],
    backends: &[SharedBackend],
    config: &SwapConfig,
) -> Vec<SwapOutcome> {
    let (mut chosen, mut outcomes) = select(agents, request);
    // A pane can carry a hook-fed row and a screen-detected one; the
    // hook-fed row knows the session id, so it goes first and wins the pane.
    chosen.sort_by_key(|agent| !resumable_id(&agent.session_id));
    let mut planned: Vec<Planned> = Vec::new();
    for agent in chosen {
        if planned.iter().any(|p| p.agent.pane == agent.pane) {
            continue;
        }
        match locate(agent, backends) {
            Ok((launch, pane, backend)) => {
                let session_id = if resumable_id(&agent.session_id) {
                    agent.session_id.clone()
                } else if let Some(id) = launch.resumed_id.clone() {
                    id
                } else {
                    outcomes.push(SwapOutcome::new(
                        agent,
                        SwapStatus::Skipped,
                        "no provider session id to resume",
                    ));
                    continue;
                };
                planned.push(Planned {
                    agent,
                    session_id,
                    launch,
                    pane,
                    backend,
                });
            }
            Err(note) => outcomes.push(SwapOutcome::new(agent, SwapStatus::Skipped, note)),
        }
    }

    // One `aas candidates` per provider, shared by that provider's targets.
    let mut by_provider: Vec<(&'static str, Result<Vec<Candidate>, String>)> = Vec::new();
    for Planned { agent, .. } in &planned {
        let provider = provider_name(agent.kind).unwrap_or_default();
        if !by_provider.iter().any(|(name, _)| *name == provider) {
            let candidates = match request.to {
                // A named account needs no ranking.
                AccountChoice::Named(_) => Ok(Vec::new()),
                _ => query_candidates(&config.aas, provider),
            };
            by_provider.push((provider, candidates));
        }
    }

    for (provider, candidates) in &by_provider {
        let group: Vec<&Planned> = planned
            .iter()
            .filter(|p| provider_name(p.agent.kind) == Some(provider))
            .collect();
        let candidates = match candidates {
            Ok(candidates) => candidates,
            Err(error) => {
                for p in group {
                    outcomes.push(SwapOutcome::new(p.agent, SwapStatus::Failed, error.clone()));
                }
                continue;
            }
        };
        let current: Vec<Option<String>> = group
            .iter()
            .map(|p| p.launch.line.account().map(str::to_owned))
            .collect();
        let accounts = assign(&current, candidates, &request.to);
        for (planned, account) in group.into_iter().zip(accounts) {
            outcomes.push(swap_one(request, planned, account, candidates));
        }
    }
    outcomes
}

/// Carry one planned target through to its outcome.
fn swap_one(
    request: &SwapRequest,
    planned: &Planned,
    account: Result<String, String>,
    candidates: &[Candidate],
) -> SwapOutcome {
    let Planned {
        agent,
        session_id,
        launch,
        pane,
        backend,
    } = planned;
    let mut outcome = SwapOutcome::new(agent, SwapStatus::Skipped, "");
    outcome.agent_session_id.clone_from(session_id);
    outcome.from = launch.line.account().map(str::to_owned);
    let account = match account {
        Ok(account) => account,
        Err(note) => {
            outcome.note = match request.to {
                AccountChoice::Named(_) => note,
                _ => format!("{note} ({})", unavailable(candidates)),
            };
            return outcome;
        }
    };
    outcome.to = Some(account.clone());
    let command = match relaunch_line(&launch.line, agent.kind, session_id, &account, request.bare)
    {
        Ok(command) => command,
        Err(note) => {
            outcome.note = note;
            return outcome;
        }
    };
    outcome.command = Some(command.clone());
    let follow_up = request.continue_for(is_rate_limited(agent));
    if request.dry_run {
        outcome.status = SwapStatus::Planned;
        if let Some(text) = follow_up {
            outcome.note = format!("then send {text:?}");
        }
        return outcome;
    }
    let target = Target {
        backend: backend.clone(),
        socket: agent.tmux_socket.clone(),
        pane: pane.pane_id.clone(),
        pane_pid: pane.pane_pid,
        kind: agent.kind,
        session_id: session_id.clone(),
    };
    let (status, note) = match execute(&target, launch, &command, follow_up) {
        Ok(note) => (SwapStatus::Swapped, note),
        Err(note) => (SwapStatus::Failed, note),
    };
    outcome.status = status;
    outcome.note = note;
    outcome
}

struct Planned<'a> {
    agent: &'a Agent,
    /// The registry's id, or the one recovered from the agent's argv.
    session_id: String,
    launch: PaneLaunch,
    pane: crate::tmux::PaneInfo,
    backend: SharedBackend,
}

/// Why each account was passed over, for a "nothing left" report.
fn unavailable(candidates: &[Candidate]) -> String {
    if candidates.is_empty() {
        return "aas lists no accounts for this provider".into();
    }
    candidates
        .iter()
        .map(|c| {
            let why = if c.eligible {
                "current"
            } else {
                c.reason.as_str()
            };
            format!("{}: {why}", c.name)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Find the agent's pane, and how it was launched there.
fn locate(
    agent: &Agent,
    backends: &[SharedBackend],
) -> Result<(PaneLaunch, crate::tmux::PaneInfo, SharedBackend), String> {
    let pane_id = agent.pane.as_deref().unwrap_or_default();
    let host = crate::backend::pane_id_host_kind(pane_id)
        .ok_or_else(|| format!("unknown pane namespace for {pane_id}"))?;
    let backend = backends
        .iter()
        .find(|backend| backend.kind() == host)
        .ok_or_else(|| format!("no active {host} backend"))?;
    if !backend.caps().send_text {
        return Err(format!("{host} cannot type into panes"));
    }
    let pane = backend
        .list_panes()
        .into_iter()
        .find(|pane| {
            pane.pane_id == pane_id
                && agent
                    .tmux_socket
                    .as_deref()
                    .is_none_or(|socket| pane.socket.as_deref().is_none_or(|s| s == socket))
        })
        .ok_or_else(|| "pane is gone".to_owned())?;
    if pane.pane_pid == 0 {
        return Err("the pane's process id is unknown".into());
    }
    let processes = crate::process_tree::read_descendants(pane.pane_pid);
    let launch = find_launch(&processes, pane.pane_pid, agent.kind)
        .ok_or_else(|| format!("no running {} process in the pane", agent.kind))?;
    // A pane started *with* the agent (`new-window 'aas e …'`) has no shell
    // behind it: stopping the agent would close the pane.
    let root = process_command(pane.pane_pid).unwrap_or_default();
    if !is_shell(&root) {
        return Err("the pane runs the agent directly, with no shell to relaunch into".into());
    }
    Ok((launch, pane, backend.clone()))
}

struct Target {
    backend: SharedBackend,
    socket: Option<String>,
    pane: String,
    pane_pid: u32,
    kind: AgentKind,
    session_id: String,
}

/// Stop the agent, type `command` into the pane's shell, wait for the
/// resumed agent, then send `follow_up`. `Ok` carries a note for the report.
fn execute(
    target: &Target,
    launch: &PaneLaunch,
    command: &str,
    follow_up: Option<&str>,
) -> Result<String, String> {
    stop(launch)?;
    std::thread::sleep(SHELL_SETTLE);
    let socket = target.socket.as_deref();
    if !target.backend.send_text_on(socket, &target.pane, command)
        || !target.backend.send_text_on(socket, &target.pane, "\r")
    {
        return Err("could not type the relaunch command into the pane".into());
    }
    let started = Instant::now();
    let relaunched = loop {
        let processes = crate::process_tree::read_descendants(target.pane_pid);
        if processes.iter().any(|p| {
            is_agent_process(p, target.kind) && p.cmdline.contains(target.session_id.as_str())
        }) {
            break true;
        }
        if started.elapsed() >= LAUNCH_TIMEOUT {
            break false;
        }
        std::thread::sleep(Duration::from_millis(300));
    };
    if !relaunched {
        return Err("typed the relaunch, but the agent never appeared in the pane".into());
    }
    let Some(text) = follow_up else {
        return Ok(String::new());
    };
    if !wait_until_drawn(target) {
        return Ok("resumed; screen never settled, follow-up not sent".into());
    }
    let sent = target
        .backend
        .send_prompt_text_on(socket, &target.pane, text)
        && {
            std::thread::sleep(crate::backend::prompt_submit_grace(&target.pane));
            target.backend.send_text_on(socket, &target.pane, "\r")
        };
    Ok(if sent {
        format!("sent {text:?}")
    } else {
        "resumed; follow-up could not be typed".into()
    })
}

/// `SIGTERM` the agent (the wrapper exits with it), then the wrapper, then
/// `SIGKILL` whatever is left.
fn stop(launch: &PaneLaunch) -> Result<(), String> {
    let pids: Vec<u32> = if launch.top_pid == launch.agent_pid {
        vec![launch.agent_pid]
    } else {
        vec![launch.agent_pid, launch.top_pid]
    };
    for (signal, wait) in [
        ("TERM", STOP_GRACE),
        ("TERM", Duration::from_secs(2)),
        ("KILL", Duration::from_secs(2)),
    ] {
        let alive: Vec<u32> = pids.iter().copied().filter(|pid| is_alive(*pid)).collect();
        if alive.is_empty() {
            return Ok(());
        }
        // First round: the agent alone, so it exits through its own handler.
        let first = if signal == "TERM" && wait == STOP_GRACE {
            &alive[..1]
        } else {
            &alive[..]
        };
        signal_pids(signal, first);
        let started = Instant::now();
        while started.elapsed() < wait && pids.iter().any(|pid| is_alive(*pid)) {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    if pids.iter().any(|pid| is_alive(*pid)) {
        return Err("the agent did not exit".into());
    }
    Ok(())
}

fn signal_pids(signal: &str, pids: &[u32]) {
    // Windows has no signals: `taskkill` asks a process to close, and `/F`
    // is the `KILL` that cannot be refused.
    #[cfg(windows)]
    let mut command = {
        let mut command = std::process::Command::new("taskkill");
        if signal == "KILL" {
            command.arg("/F");
        }
        for pid in pids {
            command.args(["/PID", &pid.to_string()]);
        }
        command
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut command = std::process::Command::new("kill");
        command.arg(format!("-{signal}"));
        command.args(pids.iter().map(u32::to_string));
        command
    };
    let _ = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn is_alive(pid: u32) -> bool {
    process_command(pid).is_some()
}

/// The command line of `pid`, or `None` once it has exited.
#[cfg(windows)]
fn process_command(pid: u32) -> Option<String> {
    crate::process_snapshot::process_argv(pid).map(|argv| argv.join(" "))
}

/// The command line of `pid`, or `None` once it has exited.
#[cfg(not(windows))]
fn process_command(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-o", "args=", "-p", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    let line = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (output.status.success() && !line.is_empty()).then_some(line)
}

fn is_shell(command: &str) -> bool {
    crate::shell::is_shell_program(command)
}

/// Wait for the resumed TUI to stop redrawing, so the follow-up lands in
/// its input box rather than in its startup.
fn wait_until_drawn(target: &Target) -> bool {
    let started = Instant::now();
    let mut last = String::new();
    let mut quiet_since = Instant::now();
    while started.elapsed() < READY_TIMEOUT {
        let screen = target
            .backend
            .capture_pane_on(target.socket.as_deref(), &target.pane)
            .unwrap_or_default();
        if screen != last {
            last = screen;
            quiet_since = Instant::now();
        } else if !last.trim().is_empty() && quiet_since.elapsed() >= READY_QUIET {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::RateLimitScope;

    fn agent(pane: &str, sid: &str, kind: AgentKind, state: AgentState) -> Agent {
        let at = time::macros::datetime!(2026-10-15 12:00:00 UTC);
        Agent {
            tmux_socket: None,
            tmux_session: None,
            kind,
            session_id: sid.into(),
            surface: None,
            pid: None,
            workload: crate::WorkloadSummary::default(),
            subagents: Vec::new(),
            pane: Some(pane.into()),
            cwd: None,
            state,
            last_prompt: None,
            last_prompt_at: None,
            last_response: None,
            recap: None,
            ai_title: None,
            last_notification: None,
            model: None,
            context_used_pct: None,
            cost_usd: None,
            rate_limit_5h_pct: None,
            rate_limit_5h_resets_at: None,
            rate_limit_7d_pct: None,
            rate_limit_7d_resets_at: None,
            rate_limited_until: None,
            rate_limit_scope: None,
            rate_limit_source: None,
            started_at: at,
            last_activity_at: at,
            state_entered_at: at,
        }
    }

    fn limited(pane: &str, sid: &str) -> Agent {
        let mut agent = agent(pane, sid, AgentKind::ClaudeCode, AgentState::Error);
        agent.rate_limit_scope = Some(RateLimitScope::FiveHour);
        agent
    }

    fn request(scope: SwapScope) -> SwapRequest {
        SwapRequest::new(scope, &SwapConfig::default())
    }

    fn proc(pid: u32, parent: u32, depth: u8, cmdline: &str) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent_pid: parent,
            depth,
            comm: cmdline
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_owned(),
            cmdline: cmdline.to_owned(),
        }
    }

    fn candidate(name: &str, eligible: bool) -> Candidate {
        Candidate {
            name: name.into(),
            provider: "claude".into(),
            eligible,
            active: false,
            reason: String::new(),
            remaining_pct: None,
        }
    }

    #[test]
    fn limited_scope_takes_only_capped_agents() {
        let agents = vec![
            limited("%1", "s1"),
            agent("%2", "s2", AgentKind::ClaudeCode, AgentState::Idle),
        ];
        let (chosen, skipped) = select(&agents, &request(SwapScope::Limited));
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].session_id, "s1");
        assert!(
            skipped.is_empty(),
            "healthy agents are out of scope, not skipped"
        );
    }

    #[test]
    fn all_scope_skips_busy_agents_unless_forced() {
        let agents = vec![
            agent("%1", "s1", AgentKind::ClaudeCode, AgentState::Working),
            agent("%2", "s2", AgentKind::Codex, AgentState::Idle),
            agent("%3", "s3", AgentKind::GeminiCli, AgentState::Idle),
        ];
        let (chosen, skipped) = select(&agents, &request(SwapScope::All));
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].session_id, "s2");
        assert_eq!(skipped.len(), 1, "gemini is outside an aas swap's scope");
        assert!(skipped[0].note.contains("--force"));

        let mut forced = request(SwapScope::All);
        forced.force = true;
        assert_eq!(select(&agents, &forced).0.len(), 2);
    }

    #[test]
    fn targets_match_pane_or_session_and_report_unswappable_ones() {
        let agents = vec![
            limited("%1", "s1"),
            agent("%2", "synthetic-x", AgentKind::ClaudeCode, AgentState::Idle),
            agent("%3", "s3", AgentKind::Codex, AgentState::Idle),
        ];
        let scope = SwapScope::Targets(vec!["%2".into(), "s3".into()]);
        let (chosen, skipped) = select(&agents, &request(scope));
        // A screen-detected row is kept: its id may still be in the argv.
        assert_eq!(chosen.len(), 2);
        assert_eq!(chosen[1].session_id, "s3");
        assert!(skipped.is_empty());
    }

    #[test]
    fn provider_filter_narrows_the_scope() {
        let agents = vec![
            agent("%1", "s1", AgentKind::ClaudeCode, AgentState::Idle),
            agent("%2", "s2", AgentKind::Codex, AgentState::Idle),
        ];
        let mut req = request(SwapScope::All);
        req.provider = Some(AgentKind::Codex);
        let (chosen, _) = select(&agents, &req);
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].kind, AgentKind::Codex);
    }

    #[test]
    fn parses_aas_candidates_json() {
        let json = r#"{"accounts":[
            {"active":false,"eligible":true,"name":"k-june@claude","provider":"claude",
             "reason":"quota_available","remainingPct":100.0,"meters":[]},
            {"active":true,"eligible":false,"name":"e-ed@claude","provider":"claude",
             "reason":"usage_rate_limited","remainingPct":0.0,"usageCooldownUntilMs":1}
        ],"schemaVersion":1}"#;
        let list = parse_candidates(json).unwrap();
        assert_eq!(list.len(), 2);
        assert!(list[0].eligible);
        assert_eq!(list[1].reason, "usage_rate_limited");
        assert_eq!(list[0].remaining_pct, Some(100.0));
    }

    #[test]
    fn auto_takes_the_best_ranked_account_that_is_not_the_current_one() {
        let candidates = [
            candidate("a", true),
            candidate("b", true),
            candidate("c", false),
        ];
        let current = [Some("a".to_owned()), Some("x".to_owned()), None];
        assert_eq!(
            assign(&current, &candidates, &AccountChoice::Auto),
            vec![Ok("b".into()), Ok("a".into()), Ok("a".into())]
        );
    }

    #[test]
    fn spread_round_robins_over_eligible_accounts() {
        let candidates = [
            candidate("a", true),
            candidate("b", true),
            candidate("c", false),
        ];
        let current = [
            Some("x".to_owned()),
            Some("x".to_owned()),
            Some("x".to_owned()),
        ];
        assert_eq!(
            assign(&current, &candidates, &AccountChoice::Spread),
            vec![Ok("a".into()), Ok("b".into()), Ok("a".into())]
        );
    }

    #[test]
    fn no_eligible_account_and_same_named_account_are_reasons_not_moves() {
        let current = [Some("a".to_owned())];
        assert!(assign(&current, &[candidate("a", true)], &AccountChoice::Auto)[0].is_err());
        assert_eq!(
            assign(&current, &[], &AccountChoice::Named("a".into()))[0],
            Err("already on a".into())
        );
    }

    #[test]
    fn reads_the_account_from_an_aas_line() {
        let line =
            LaunchLine::from_aas("aas e e-ed@claude --dangerously-skip-permissions").unwrap();
        assert_eq!(line.account(), Some("e-ed@claude"));
        let line =
            LaunchLine::from_aas("/opt/homebrew/bin/aas exec --fallback b a -- --yolo").unwrap();
        assert_eq!(line.account(), Some("a"));
        assert!(LaunchLine::from_aas("aas usage").is_none());
        assert!(LaunchLine::from_aas("claude --resume x").is_none());
    }

    #[test]
    fn swaps_the_account_and_resumes_a_claude_session() {
        let line = LaunchLine::from_aas(
            "aas e e-ed@claude --dangerously-skip-permissions --resume old-id",
        )
        .unwrap();
        assert_eq!(
            relaunch_line(
                &line,
                AgentKind::ClaudeCode,
                "new-id",
                "jelly@claude",
                BareMode::Skip
            )
            .unwrap(),
            "aas e jelly@claude --dangerously-skip-permissions --resume new-id"
        );
    }

    #[test]
    fn drops_the_initial_prompt_and_keeps_flag_values() {
        let line = LaunchLine::from_aas(
            "aas e a@claude --model opus --permission-mode plan fix the login bug",
        )
        .unwrap();
        assert_eq!(
            relaunch_line(
                &line,
                AgentKind::ClaudeCode,
                "sid",
                "b@claude",
                BareMode::Skip
            )
            .unwrap(),
            "aas e b@claude --model opus --permission-mode plan --resume sid"
        );
    }

    #[test]
    fn resumes_codex_with_its_subcommand_and_keeps_the_separator() {
        let line = LaunchLine::from_aas("aas exec a@codex -- --yolo resume old -m gpt-5").unwrap();
        assert_eq!(
            relaunch_line(&line, AgentKind::Codex, "sid", "b@codex", BareMode::Skip).unwrap(),
            "aas exec b@codex -- --yolo -m gpt-5 resume sid"
        );
    }

    #[test]
    fn keeps_a_cross_provider_frontend() {
        let line = LaunchLine::from_aas("aas e a@zai claude --verbose").unwrap();
        assert_eq!(
            relaunch_line(&line, AgentKind::ClaudeCode, "sid", "b@zai", BareMode::Skip).unwrap(),
            "aas e b@zai claude --verbose --resume sid"
        );
    }

    #[test]
    fn bare_launches_are_skipped_or_wrapped() {
        let line = LaunchLine::bare("claude --dangerously-skip-permissions --resume old");
        assert!(relaunch_line(&line, AgentKind::ClaudeCode, "sid", "b", BareMode::Skip).is_err());
        assert_eq!(
            relaunch_line(
                &line,
                AgentKind::ClaudeCode,
                "sid",
                "b@claude",
                BareMode::Wrap
            )
            .unwrap(),
            "aas e b@claude --dangerously-skip-permissions --resume sid"
        );
    }

    #[test]
    fn finds_the_agent_and_its_aas_wrapper_under_the_shell() {
        let processes = [
            proc(
                20,
                10,
                1,
                "aas e e-ed@claude --dangerously-skip-permissions",
            ),
            proc(30, 20, 2, "claude --dangerously-skip-permissions"),
            proc(40, 30, 3, "node mcp-server.js"),
        ];
        let launch = find_launch(&processes, 10, AgentKind::ClaudeCode).unwrap();
        assert_eq!(launch.agent_pid, 30);
        assert_eq!(launch.top_pid, 20);
        assert_eq!(launch.line.account(), Some("e-ed@claude"));
    }

    #[test]
    fn a_bare_agent_is_its_own_top_and_codex_servers_are_not_agents() {
        let processes = [
            proc(20, 10, 1, "codex app-server --listen unix://"),
            proc(
                21,
                10,
                1,
                "node /usr/lib/node_modules/@openai/codex/bin/codex --yolo",
            ),
        ];
        let launch = find_launch(&processes, 10, AgentKind::Codex).unwrap();
        assert_eq!((launch.agent_pid, launch.top_pid), (21, 21));
        assert_eq!(launch.line.account(), None);
    }

    #[test]
    fn recovers_the_session_id_from_the_agent_argv() {
        let id = "090b45a6-3902-49cc-a0d9-044640b863f0";
        let processes = [
            proc(20, 10, 1, &format!("aas e e-ed@claude --resume {id}")),
            proc(
                30,
                20,
                2,
                &format!("claude --dangerously-skip-permissions --resume {id}"),
            ),
        ];
        let launch = find_launch(&processes, 10, AgentKind::ClaudeCode).unwrap();
        assert_eq!(launch.resumed_id.as_deref(), Some(id));
        assert_eq!(
            argv_session_id(&format!("codex --yolo resume {id}"), AgentKind::Codex).as_deref(),
            Some(id)
        );
        assert_eq!(
            argv_session_id("claude --resume not-a-uuid", AgentKind::ClaudeCode),
            None
        );
        assert_eq!(
            argv_session_id("codex resume --last", AgentKind::Codex),
            None
        );
    }

    #[test]
    fn explains_why_no_account_was_left() {
        let mut limited = candidate("a", false);
        limited.reason = "usage_rate_limited".into();
        assert_eq!(
            unavailable(&[limited, candidate("b", true)]),
            "a: usage_rate_limited, b: current"
        );
    }

    /// The whole swap against a private tmux server, with `aas` and
    /// `claude` stood in for by scripts: stop, retype, resume, follow up.
    #[test]
    #[ignore = "drives a private tmux server; run with --ignored"]
    fn swaps_a_live_pane_end_to_end() {
        use std::process::Command;
        let dir = std::env::temp_dir().join(format!("muxa-swap-{}", std::process::id()));
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let script = |name: &str, body: &str| {
            let path = bin.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            Command::new("chmod").arg("+x").arg(&path).status().unwrap();
        };
        // `aas e <name> args…` runs the agent as its child, like the real one.
        script("aas", r#"shift 2; "$(dirname "$0")/claude" "$@""#);
        script(
            "claude",
            r#"echo "FAKE CLAUDE $*"; while :; do sleep 1; done"#,
        );
        let socket = dir.join("tmux.sock");
        let socket = socket.to_str().unwrap();
        let tmux = |args: &[&str]| {
            let out = Command::new("tmux")
                .arg("-S")
                .arg(socket)
                .args(args)
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
        Command::new("env")
            .arg(format!("PATH={path}"))
            .args(["tmux", "-S", socket, "-f", "/dev/null", "new-session", "-d"])
            .args(["-x", "160", "-y", "40", "/bin/sh"])
            .status()
            .unwrap();
        std::env::set_var("MUXA_TMUX_SOCKET", socket);
        let pane = tmux(&["display-message", "-p", "#{pane_id}"]);
        let sid = "5a95b526-0439-4938-adb8-a24055198731";
        tmux(&[
            "send-keys",
            "-t",
            &pane,
            "aas e e-ed@claude --dangerously-skip-permissions fix it",
            "Enter",
        ]);
        std::thread::sleep(Duration::from_secs(1));

        let mut capped = limited(&pane, sid);
        capped.tmux_socket = None;
        let mut request = request(SwapScope::Targets(vec![pane.clone()]));
        request.to = AccountChoice::Named("jelly@claude".into());
        let backend: SharedBackend = std::sync::Arc::new(crate::backend::tmux::TmuxBackend::new());
        let outcomes = run(&request, &[capped], &[backend], &SwapConfig::default());
        let screen = tmux(&["capture-pane", "-p", "-t", &pane]);
        tmux(&["kill-server"]);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(outcomes.len(), 1, "{outcomes:?}");
        let outcome = &outcomes[0];
        assert_eq!(outcome.status, SwapStatus::Swapped, "{outcome:?}\n{screen}");
        assert_eq!(outcome.from.as_deref(), Some("e-ed@claude"));
        let command = outcome.command.as_deref().unwrap_or_default();
        assert!(!command.starts_with("/bin/sh"), "{command}");
        assert!(
            command.ends_with(&format!(
                "/aas e jelly@claude --dangerously-skip-permissions --resume {sid}"
            )),
            "{command}"
        );
        assert!(
            screen.contains(&format!(
                "FAKE CLAUDE --dangerously-skip-permissions --resume {sid}"
            )),
            "{screen}"
        );
        assert!(screen.trim_end().ends_with("continue"), "{screen}");
    }

    #[test]
    fn continue_text_goes_where_continue_when_says() {
        let mut req = request(SwapScope::All);
        assert_eq!(req.continue_for(true), Some("continue"));
        assert_eq!(req.continue_for(false), None);
        req.continue_when = ContinueWhen::Always;
        assert_eq!(req.continue_for(false), Some("continue"));
        req.continue_when = ContinueWhen::Never;
        assert_eq!(req.continue_for(true), None);
        req.continue_when = ContinueWhen::Always;
        req.continue_text = None;
        assert_eq!(req.continue_for(true), None);
    }

    #[test]
    fn account_choice_parses_keywords_and_names() {
        assert_eq!(AccountChoice::parse("auto"), AccountChoice::Auto);
        assert_eq!(AccountChoice::parse(""), AccountChoice::Auto);
        assert_eq!(AccountChoice::parse("spread"), AccountChoice::Spread);
        assert_eq!(
            AccountChoice::parse("jelly@claude"),
            AccountChoice::Named("jelly@claude".into())
        );
    }

    #[cfg(unix)]
    fn fake_aas(name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("muxa-aas-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("aas");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn the_query_deadline_covers_a_helper_holding_the_pipes() {
        let aas = fake_aas("helper", "sleep 3 &\nprintf '{\"accounts\":[]}'\nexit 0");
        let started = Instant::now();
        let outcome =
            query_candidates_within(aas.to_str().unwrap(), "claude", Duration::from_millis(300));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert!(outcome.unwrap_err().contains("did not answer"));
        std::fs::remove_dir_all(aas.parent().unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn the_query_reads_a_prompt_answer_and_kills_a_hung_one() {
        let ok = fake_aas(
            "ok",
            "printf '{\"accounts\":[{\"name\":\"a@claude\",\"provider\":\"claude\",\"eligible\":true}]}'",
        );
        let list = query_candidates_within(ok.to_str().unwrap(), "claude", Duration::from_secs(5))
            .unwrap();
        assert_eq!(list[0].name, "a@claude");
        let hung = fake_aas("hung", "exec sleep 30");
        let started = Instant::now();
        let outcome =
            query_candidates_within(hung.to_str().unwrap(), "claude", Duration::from_millis(200));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(outcome.unwrap_err().contains("did not answer"));
        for path in [ok, hung] {
            std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn find_aas_answers_with_an_absolute_path() {
        let aas = fake_aas("relative", "true");
        let dir = aas.parent().unwrap().to_owned();
        // A relative configured path, resolved against this process's cwd.
        let cwd = std::env::current_dir().unwrap();
        let relative = pathdiff(&aas, &cwd);
        let found = find_aas(&relative).unwrap();
        assert!(std::path::Path::new(&found).is_absolute(), "{found}");
        assert!(std::path::Path::new(&found).is_file(), "{found}");
        assert_eq!(find_aas(&format!("{}/missing", dir.display())), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// `target` relative to `base`, through enough `..` to reach the root.
    #[cfg(unix)]
    fn pathdiff(target: &std::path::Path, base: &std::path::Path) -> String {
        let ups = base.components().count().saturating_sub(1);
        format!(
            "{}{}",
            "../".repeat(ups),
            target.display().to_string().trim_start_matches('/')
        )
    }
}
