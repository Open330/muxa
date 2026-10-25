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
    /// Only agents whose turn was cut off: stopped by a usage cap, or
    /// interrupted mid-turn by the swap itself (`--force`).
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
    /// Panes left alone whatever the scope: the caller's own, for one.
    pub exclude: Vec<String>,
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
            exclude: Vec::new(),
        }
    }

    fn continue_for(&self, was_cut_off: bool) -> Option<&str> {
        let wanted = match self.continue_when {
            ContinueWhen::Never => false,
            ContinueWhen::Always => true,
            ContinueWhen::Limited => was_cut_off,
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
    select_at(agents, request, time::OffsetDateTime::now_utc())
}

/// How recently an agent must have done something to count as mid-turn
/// even when its state says otherwise: a long tool call shows no
/// transition, and stopping it would cut the turn off.
const RECENT_ACTIVITY: time::Duration = time::Duration::seconds(20);

/// Mid-turn by its activity: something happened within
/// [`RECENT_ACTIVITY`]. A capped agent's last activity is the cap itself.
fn recently_active(agent: &Agent, now: time::OffsetDateTime) -> bool {
    !is_rate_limited(agent) && now - agent.last_activity_at < RECENT_ACTIVITY
}

fn select_at<'a>(
    agents: &'a [Agent],
    request: &SwapRequest,
    now: time::OffsetDateTime,
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
        if agent
            .pane
            .as_deref()
            .is_some_and(|pane| request.exclude.iter().any(|excluded| excluded == pane))
        {
            skipped.push(SwapOutcome::new(agent, SwapStatus::Skipped, "excluded"));
            continue;
        }
        let skip = |note: &str| SwapOutcome::new(agent, SwapStatus::Skipped, note);
        if provider_name(agent.kind).is_none() {
            skipped.push(skip("aas cannot switch this agent's provider"));
        } else if agent.pane.is_none() {
            skipped.push(skip("not in a multiplexer pane"));
        } else if is_busy(agent) && !request.force {
            skipped.push(skip("mid-turn; pass --force to stop it anyway"));
        } else if recently_active(agent, now) && !request.force {
            skipped.push(skip(
                "active in the last 20s (likely mid-turn); pass --force to stop it anyway",
            ));
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

/// The account an agent runs on: the one its launch line names, or, for an
/// agent started bare, the account the provider marks active, since a bare
/// CLI runs on that one. Without this, `auto` could "move" a bare agent onto
/// the account it is already spending.
fn current_account(line: &LaunchLine, candidates: &[Candidate]) -> Option<String> {
    line.account()
        .map(str::to_owned)
        .or_else(|| candidates.iter().find(|c| c.active).map(|c| c.name.clone()))
}

/// A configured account `pid` runs on, read from its environment.
/// The configured account a launch runs on, read from the agent's
/// environment. Where that cannot be read (macOS hides it for its own signed
/// binaries), the account its `aas e <name>` line names, so `auto` still
/// does not send it back to the account it is capped on.
fn profile_account(
    resolved: &crate::accounts::ResolvedAccounts,
    launch: &PaneLaunch,
    provider: &str,
) -> Option<String> {
    let environ = crate::accounts::process_environ(launch.agent_pid);
    if environ.is_empty() {
        return launch
            .line
            .account()
            .map(|name| crate::accounts::bare_name(name, provider).to_owned());
    }
    resolved.identify(&environ).map(str::to_owned)
}

/// Configured accounts as ranked candidates, in config order; `capped` ones
/// are ineligible.
fn profile_candidates(
    profiles: &[crate::accounts::AccountProfile],
    provider: &str,
    capped: &[&str],
) -> Vec<Candidate> {
    profiles
        .iter()
        .map(|profile| {
            let is_capped = capped.contains(&profile.name.as_str());
            Candidate {
                name: profile.name.clone(),
                provider: provider.to_owned(),
                eligible: !is_capped,
                active: false,
                reason: if is_capped {
                    "rate limited here".into()
                } else {
                    "configured".into()
                },
                remaining_pct: None,
            }
        })
        .collect()
}

/// Where a relaunched agent turned out to run.
enum Landed {
    /// On this configured account, or on none of them.
    On(Option<String>),
    /// Its environment could not be read (macOS hides that of its own
    /// signed binaries), so nothing can be said.
    Unreadable,
}

/// The configured account the relaunched agent in `target`'s pane runs on.
fn resumed_account(target: &Target, resolved: &crate::accounts::ResolvedAccounts) -> Landed {
    let Some(agent) = pane_processes(target.pane_pid.get()).into_iter().find(|p| {
        is_agent_process(p, target.kind) && p.cmdline.contains(target.session_id.as_str())
    }) else {
        return Landed::On(None);
    };
    let environ = crate::accounts::process_environ(agent.pid);
    if environ.is_empty() {
        return Landed::Unreadable;
    }
    Landed::On(resolved.identify(&environ).map(str::to_owned))
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

/// The line that resumes `session_id` on a configured account: the
/// account's `env K=V … <agent>` prefix, then the agent's own flags. An
/// agent started through `aas` (or any other wrapper) loses the wrapper,
/// keeps its arguments.
pub fn profile_relaunch_line(
    line: &LaunchLine,
    kind: AgentKind,
    session_id: &str,
    profile: &crate::accounts::AccountProfile,
) -> Result<String, String> {
    let (program, args) = agent_program_and_args(line, kind);
    let mut words = profile.launch_prefix(&program);
    words.extend(resume_free_args(args, kind));
    words.extend(resume_suffix(kind, session_id));
    let dialect = crate::shell::Dialect::native();
    if cfg!(windows) {
        return Err(
            "configured accounts relaunch through `env`/`sh`, which this shell lacks".into(),
        );
    }
    Ok(dialect.command_line(&words))
}

/// The agent's program word and its arguments. An `aas e <account> …` line
/// names no program (the provider's CLI is implied); a bare one starts with
/// it, possibly behind an interpreter (`node …/bin/codex`).
fn agent_program_and_args(line: &LaunchLine, kind: AgentKind) -> (String, &[String]) {
    let provider = provider_name(kind).unwrap_or("claude").to_owned();
    if let Some(at) = line.account_at {
        let rest = &line.tokens[at + 1..];
        // `aas e <name> <provider> …` names this agent's own CLI; any other
        // frontend word is another provider's UI, not a program to keep.
        return match rest.first().map(String::as_str) {
            Some(word) if word == provider => (rest[0].clone(), &rest[1..]),
            Some("claude" | "codex") => (provider, &rest[1..]),
            _ => (provider, rest),
        };
    }
    match line
        .tokens
        .iter()
        .take(2)
        .position(|token| crate::discovery::classify_command(token) == Some(kind))
    {
        Some(at) => (line.tokens[at].clone(), &line.tokens[at + 1..]),
        None => (provider, line.tokens.get(1..).unwrap_or(&[])),
    }
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
    // A pane started as `exec claude …` has the agent as its own root.
    if agent.pid != pane_pid {
        let mut parent = agent.parent_pid;
        while parent != pane_pid {
            let process = by_pid(parent)?;
            chain.push(process);
            parent = process.parent_pid;
        }
        // A pane started as `aas e …` has the wrapper as its root.
        if let Some(root) =
            by_pid(pane_pid).filter(|root| LaunchLine::from_aas(&root.cmdline).is_some())
        {
            chain.push(root);
        }
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
    accounts: &crate::accounts::AccountsConfig,
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
            let candidates = if accounts.for_provider(provider).is_empty() {
                match request.to {
                    // A named account needs no ranking.
                    AccountChoice::Named(_) => Ok(Vec::new()),
                    _ => query_candidates(&config.aas, provider),
                }
            } else {
                // Configured accounts: ranked below, from what muxa sees.
                Ok(Vec::new())
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
        let profiles = accounts.for_provider(provider);
        if !profiles.is_empty() {
            outcomes.extend(swap_on_profiles(request, group, profiles, provider));
            continue;
        }
        let current: Vec<Option<String>> = group
            .iter()
            .map(|p| current_account(&p.launch.line, candidates))
            .collect();
        let accounts = assign(&current, candidates, &request.to);
        for (planned, account) in group.into_iter().zip(accounts) {
            outcomes.push(swap_one(request, planned, account, candidates, None));
        }
    }
    outcomes
}

/// [`run`] for a provider that lists `[[accounts]]`: accounts, identity and
/// relaunch all come from config, never `aas`.
fn swap_on_profiles(
    request: &SwapRequest,
    group: Vec<&Planned>,
    profiles: &[crate::accounts::AccountProfile],
    provider: &str,
) -> Vec<SwapOutcome> {
    let mut outcomes = Vec::new();
    let resolved = crate::accounts::ResolvedAccounts::new(profiles);
    let current: Vec<Option<String>> = group
        .iter()
        .map(|p| profile_account(&resolved, &p.launch, provider))
        .collect();
    // An account an agent here was capped on has no quota to give.
    let capped: Vec<&str> = group
        .iter()
        .zip(&current)
        .filter(|(p, _)| is_rate_limited(p.agent))
        .filter_map(|(_, account)| account.as_deref())
        .collect();
    let ranked = profile_candidates(profiles, provider, &capped);
    let choice = match &request.to {
        AccountChoice::Named(name) => {
            AccountChoice::Named(crate::accounts::bare_name(name, provider).to_owned())
        }
        other => other.clone(),
    };
    let chosen = assign(&current, &ranked, &choice);
    for ((planned, account), from) in group.into_iter().zip(chosen).zip(current) {
        let mut outcome = swap_one(
            request,
            planned,
            account,
            &ranked,
            Some((profiles, &resolved)),
        );
        outcome.from = from;
        outcomes.push(outcome);
    }
    outcomes
}

/// Carry one planned target through to its outcome.
fn swap_one(
    request: &SwapRequest,
    planned: &Planned,
    account: Result<String, String>,
    candidates: &[Candidate],
    profiles: Option<(
        &[crate::accounts::AccountProfile],
        &crate::accounts::ResolvedAccounts,
    )>,
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
    let provider = provider_name(agent.kind).unwrap_or_default();
    let relaunched = match profiles {
        Some((profiles, _)) => match crate::accounts::find_profile(profiles, provider, &account) {
            Some(profile) => profile_relaunch_line(&launch.line, agent.kind, session_id, profile),
            None => Err(format!(
                "no account named {account} in [[accounts.{provider}]] (aas accounts are not used once a provider lists its own)"
            )),
        },
        None => relaunch_line(&launch.line, agent.kind, session_id, &account, request.bare),
    };
    let command = match relaunched {
        Ok(command) => command,
        Err(note) => {
            outcome.note = note;
            return outcome;
        }
    };
    outcome.command = Some(command.clone());
    // The swap cuts a turn in progress off, so it gets the follow-up like a
    // capped one.
    // Recent activity only keeps an agent out of a non-forced swap; an idle
    // agent that just finished a turn has nothing to continue.
    let mid_turn = agent.state == AgentState::Working;
    let follow_up = request.continue_for(is_rate_limited(agent) || mid_turn);
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
        pane_pid: std::cell::Cell::new(pane.pane_pid),
        direct: !shell_above(
            &pane_processes(pane.pane_pid),
            launch.agent_pid,
            pane.pane_pid,
        )
        .unwrap_or(true),
        cwd: agent
            .cwd
            .clone()
            .filter(|cwd| !cwd.is_empty())
            .unwrap_or_else(|| pane.current_path.clone()),
        kind: agent.kind,
        session_id: session_id.clone(),
        busy: mid_turn,
    };
    // The relaunch must land on the account it was meant for, checked before
    // any follow-up starts work on the wrong one.
    let verify = |target: &Target| -> Result<String, String> {
        let Some((_, resolved)) = profiles else {
            return Ok(String::new());
        };
        match resumed_account(target, resolved) {
            Landed::On(landed) if landed.as_deref() != Some(account.as_str()) => Err(format!(
                "the resumed agent runs on {}",
                landed.as_deref().unwrap_or("no configured account")
            )),
            Landed::Unreadable => {
                Ok(" (account unconfirmed: its environment is not readable here)".into())
            }
            Landed::On(_) => Ok(String::new()),
        }
    };
    let (status, note) = match execute(&target, launch, &command, follow_up, &verify) {
        Ok(note) => (SwapStatus::Swapped, note),
        Err(Refusal::Failed(note)) => (SwapStatus::Failed, note),
        Err(Refusal::Kept(note)) => (SwapStatus::Skipped, note),
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
    let processes = pane_processes(pane.pane_pid);
    let launch = find_launch(&processes, pane.pane_pid, agent.kind)
        .ok_or_else(|| format!("no running {} process in the pane", agent.kind))?;
    // A pane started *with* the agent (`new-window 'aas e …'`) has no shell
    // behind it: it is kept open past the exit and respawned in place, which
    // only the tmux-family backends can do.
    let direct = !shell_above(&processes, launch.agent_pid, pane.pane_pid)
        .ok_or_else(|| "could not read the pane's root process".to_owned())?;
    if direct
        && !matches!(
            host,
            crate::backend::HostKind::Tmux | crate::backend::HostKind::Rmux
        )
    {
        return Err("the pane runs the agent directly, with no shell to relaunch into".into());
    }
    Ok((launch, pane, backend.clone()))
}

struct Target {
    backend: SharedBackend,
    socket: Option<String>,
    pane: String,
    /// The pane's root process; a respawn replaces it.
    pane_pid: std::cell::Cell<u32>,
    /// The pane runs the agent directly, with no shell to type into: it is
    /// respawned in place instead.
    direct: bool,
    /// Where a respawned agent starts.
    cwd: String,
    kind: AgentKind,
    session_id: String,
    /// Mid-turn when the swap began (`--force`).
    busy: bool,
}

/// Stop the agent, type `command` into the pane's shell, wait for the
/// resumed agent, then send `follow_up`. `Ok` carries a note for the report.
fn execute(
    target: &Target,
    launch: &PaneLaunch,
    command: &str,
    follow_up: Option<&str>,
    verify: &dyn Fn(&Target) -> Result<String, String>,
) -> Result<String, Refusal> {
    let socket = target.socket.as_deref();
    let mut answered: Vec<&'static str> = Vec::new();
    // Clearing the prompt to quit loses what was half-typed there; it is
    // typed back once the agent is up again (or stays, if it is kept).
    let draft = screen_text(target).and_then(|screen| composer_draft(&screen, target.kind));
    stop_agent(target, launch, follow_up, draft.as_deref(), &mut answered)?;
    if target.direct {
        relaunch_in_place(target, command)?;
    } else {
        std::thread::sleep(SHELL_SETTLE);
        if !target.backend.send_text_on(socket, &target.pane, command)
            || !target.backend.send_text_on(socket, &target.pane, "\r")
        {
            return Err("could not type the relaunch command into the pane".into());
        }
    }
    let started = Instant::now();
    let relaunched = loop {
        if resumed_agent_running(target) {
            break true;
        }
        if started.elapsed() >= LAUNCH_TIMEOUT {
            break false;
        }
        std::thread::sleep(Duration::from_millis(300));
    };
    if !relaunched {
        return Err(if target.direct {
            "respawned the pane, but the agent never appeared; the pane is kept open so its \
             output can be read"
        } else {
            "typed the relaunch, but the agent never appeared in the pane"
        }
        .into());
    }
    if target.direct {
        // Running again: the pane may close with its agent as before.
        target
            .backend
            .set_remain_on_exit_on(socket, &target.pane, false);
    }
    // A running process is not a resumed conversation: the TUI can be up and
    // showing that another app owns the conversation, or a startup error.
    // Every swap, with or without a follow-up, waits for a usable screen.
    let readiness = wait_for_resume_answering(
        || screen_text(target),
        || resumed_agent_running(target),
        |keys| {
            type_keys(
                |key| target.backend.send_text_on(socket, &target.pane, key),
                keys,
            )
        },
        READY_TIMEOUT,
        READY_QUIET,
        Duration::from_millis(250),
        &mut answered,
    );
    match readiness {
        Resume::Ready => {}
        Resume::Blocked(reason) => {
            let owners = if target.kind == AgentKind::Codex {
                codex_writer_owners(target.pane_pid.get())
            } else {
                String::new()
            };
            return Err(
                format!("relaunched, but {reason}{owners}; nothing was sent to the pane").into(),
            );
        }
        Resume::Exited => {
            return Err(
                "relaunched, but the agent exited before its conversation was ready".into(),
            );
        }
        Resume::Unsettled => {
            return Err(format!(
                "relaunched, but the conversation did not become ready within {}s; \
                 nothing was sent to the pane",
                READY_TIMEOUT.as_secs()
            )
            .into());
        }
    }
    let checked = verify(target)
        .map_err(|why| format!("relaunched, but {why}; nothing was sent to the pane"))?;
    Ok(follow_through(
        target,
        follow_up,
        draft.as_deref(),
        &answered,
        &checked,
    ))
}

/// After a usable resume: send the follow-up, see whether a turn started,
/// and type back the half-typed prompt. The note for the report.
fn follow_through(
    target: &Target,
    follow_up: Option<&str>,
    draft: Option<&str>,
    answered: &[&'static str],
    checked: &str,
) -> String {
    let socket = target.socket.as_deref();
    let answers = if answered.is_empty() {
        checked.to_owned()
    } else {
        format!(" ({}){checked}", answered.join(", "))
    };
    // The draft goes back after the follow-up: typed first, the two would
    // be submitted as one prompt.
    let restore = |note: String| match draft {
        Some(text)
            if target
                .backend
                .send_prompt_text_on(socket, &target.pane, text) =>
        {
            format!("{note}; restored the half-typed prompt")
        }
        _ => note,
    };
    let Some(text) = follow_up else {
        return restore(format!("resumed{answers}"));
    };
    if !send_follow_up(target, text) {
        return restore(format!("resumed{answers}; follow-up could not be typed"));
    }
    let started = wait_until(Duration::from_secs(20), || {
        screen_text(target).is_some_and(|screen| turn_running(&screen))
    });
    restore(if started {
        format!("sent {text:?}, and the agent picked the turn back up{answers}")
    } else {
        format!("sent {text:?}{answers}, but no turn started within 20s")
    })
}

/// Poll `ready` until it holds or `timeout` passes.
fn wait_until(timeout: Duration, mut ready: impl FnMut() -> bool) -> bool {
    let started = Instant::now();
    loop {
        if ready() {
            return true;
        }
        if started.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(400));
    }
}

/// Does the bottom of `screen` show a turn in progress? Both TUIs print an
/// interrupt hint while working; Claude's spinner line also counts up
/// (`✻ Thinking… (12s · ↓ 300 tokens)`).
fn turn_running(screen: &str) -> bool {
    let tail = screen_tail(screen, 12);
    tail.contains("esc to interrupt")
        || tail.lines().any(|line| {
            // Claude's spinner: `… (12s ·`, `… (1m 3s ·`.
            line.split_once("… (").is_some_and(|(_, rest)| {
                let digits = rest.chars().take_while(char::is_ascii_digit).count();
                digits > 0 && matches!(rest[digits..].chars().next(), Some('s' | 'm'))
            })
        })
}

/// Placeholder text an empty Codex composer shows.
const CODEX_PLACEHOLDERS: &[&str] = &[
    "Ask Codex to do anything",
    "Find and fix a bug",
    "Explain this codebase",
    "Summarize recent commits",
    "Improve documentation",
    "Write tests",
    "Implement {feature}",
    "Run /review",
    "Use /skills",
];

/// What was half-typed into the agent's prompt, if anything. Claude's prompt
/// sits between the last two horizontal rules, after `❯`; Codex's is the
/// last `› ` line that is not its placeholder or a menu row.
fn composer_draft(screen: &str, kind: AgentKind) -> Option<String> {
    let lines: Vec<&str> = screen.lines().collect();
    let text = match kind {
        AgentKind::ClaudeCode => {
            let rules: Vec<usize> = lines
                .iter()
                .enumerate()
                .filter(|(_, line)| {
                    let line = line.trim();
                    line.chars().count() >= 5 && line.chars().all(|c| c == '─')
                })
                .map(|(i, _)| i)
                .collect();
            let [.., top, bottom] = rules[..] else {
                return None;
            };
            lines[top + 1..bottom]
                .iter()
                .map(|line| line.trim())
                .collect::<Vec<_>>()
                .join(" ")
                .trim_start_matches('❯')
                .trim()
                .to_owned()
        }
        AgentKind::Codex => {
            let line = lines.iter().rev().find(|line| line.starts_with("› "))?;
            let text = line["› ".len()..].trim().to_owned();
            // A menu row (`› 1. Trust and continue`) sits above key hints.
            let menu_row = open_question(screen).is_some()
                && text
                    .split_once(". ")
                    .is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
            if menu_row || CODEX_PLACEHOLDERS.contains(&text.as_str()) {
                return None;
            }
            text
        }
        _ => return None,
    };
    (!text.is_empty()).then_some(text)
}

/// Stop the agent in `target`'s pane, keeping a direct pane open past the
/// exit so it can be respawned. A refusal undoes that.
fn stop_agent(
    target: &Target,
    launch: &PaneLaunch,
    follow_up: Option<&str>,
    draft: Option<&str>,
    answered: &mut Vec<&'static str>,
) -> Result<(), Refusal> {
    let socket = target.socket.as_deref();
    if target.direct
        && !target
            .backend
            .set_remain_on_exit_on(socket, &target.pane, true)
    {
        return Err(
            "the pane runs the agent directly and cannot be kept open to relaunch it".into(),
        );
    }
    let stopped = if target.kind == AgentKind::ClaudeCode {
        exit_claude(target, launch, follow_up, draft, answered)
    } else {
        stop(launch).map_err(Refusal::from)
    };
    if stopped.is_err() && target.direct {
        target
            .backend
            .set_remain_on_exit_on(socket, &target.pane, false);
    }
    stopped
}

/// Respawn a pane that ran its agent directly with `command`, keeping the
/// pane id, then follow the pane's new root process.
fn relaunch_in_place(target: &Target, command: &str) -> Result<(), Refusal> {
    let socket = target.socket.as_deref();
    // The pane had no shell; run the relaunch through the user's login
    // shell, which has the `PATH` the typed line would have had.
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let line =
        crate::shell::Dialect::native().command_line(&[shell, "-lic".into(), command.to_owned()]);
    if !target
        .backend
        .respawn_pane_on(socket, &target.pane, &target.cwd, &line)
    {
        return Err(
            "the agent stopped, but its pane could not be respawned with the relaunch; \
             it is kept open"
                .into(),
        );
    }
    // `%5` exists on every server: match the pane's own.
    for _ in 0..10 {
        if let Some(pane) = target.backend.list_panes().into_iter().find(|pane| {
            pane.pane_id == target.pane
                && socket.is_none_or(|socket| pane.socket.as_deref().is_none_or(|s| s == socket))
        }) {
            target.pane_pid.set(pane.pane_pid);
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Err("respawned the pane, but could not find it again to follow the relaunch".into())
}

/// The pane's visible text. Captures carry the TUI's colours (`-e`), and
/// Claude colours each word of a footer apart (`Enter` `to` `confirm`), so
/// every rule here reads the screen with the escapes stripped.
fn screen_text(target: &Target) -> Option<String> {
    target
        .backend
        .capture_pane_on(target.socket.as_deref(), &target.pane)
        .map(|screen| crate::screen::strip_ansi(&screen))
}

/// Type `text` and submit it.
fn send_follow_up(target: &Target, text: &str) -> bool {
    let socket = target.socket.as_deref();
    target
        .backend
        .send_prompt_text_on(socket, &target.pane, text)
        && {
            std::thread::sleep(crate::backend::prompt_submit_grace(&target.pane));
            target.backend.send_text_on(socket, &target.pane, "\r")
        }
}

/// Why [`execute`] did not finish a swap.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Refusal {
    /// Something went wrong; the note says what and how far it got.
    Failed(String),
    /// The agent was deliberately left running on its account.
    Kept(String),
}

impl From<String> for Refusal {
    fn from(note: String) -> Self {
        Self::Failed(note)
    }
}

impl From<&str> for Refusal {
    fn from(note: &str) -> Self {
        Self::Failed(note.to_owned())
    }
}

/// How long `/exit` gets to end a Claude session, dialogs included, before
/// the signal path takes over.
const EXIT_TIMEOUT: Duration = Duration::from_secs(15);

/// Quit Claude the way a person would. A signal ends the session without
/// its exit dialogs, so background shells and the worktree go with it;
/// `/exit` asks about them and the swap keeps both. A session that owns
/// scheduled tasks is left running: exiting would cancel them.
fn exit_claude(
    target: &Target,
    launch: &PaneLaunch,
    follow_up: Option<&str>,
    draft: Option<&str>,
    answered: &mut Vec<&'static str>,
) -> Result<(), Refusal> {
    let socket = target.socket.as_deref();
    let send = |keys: &str| target.backend.send_text_on(socket, &target.pane, keys);
    // The relaunch is typed into the shell, so the `aas` wrapper must be gone
    // too, not only the agent.
    let alive = || is_alive(launch.agent_pid) || is_alive(launch.top_pid);
    // A kept session that the swap interrupted gets its turn back.
    let keep = |note: &str| {
        if let Some(text) = follow_up.filter(|_| target.busy) {
            std::thread::sleep(Duration::from_millis(800));
            send_follow_up(target, text);
        }
        if let Some(text) = draft {
            std::thread::sleep(Duration::from_millis(300));
            target
                .backend
                .send_prompt_text_on(socket, &target.pane, text);
        }
        Err(Refusal::Kept(note.to_owned()))
    };
    if target.busy {
        // Interrupt the turn first; `/exit` is only read at the prompt.
        send("\x1b");
        std::thread::sleep(Duration::from_millis(1500));
    }
    // Ctrl-C clears a half-typed draft so `/exit` is not appended to it.
    if !(send("\x03") && {
        std::thread::sleep(Duration::from_millis(300));
        send_follow_up(target, "/exit")
    }) {
        return Err("could not type /exit into the pane".into());
    }
    let started = Instant::now();
    while alive() && started.elapsed() < EXIT_TIMEOUT {
        std::thread::sleep(Duration::from_millis(400));
        let screen = screen_text(target).unwrap_or_default();
        if owns_scheduled_tasks(&screen) {
            // Esc cancels the exit, whatever rows the dialog lists.
            send("\x1b");
            return keep(
                "the session owns scheduled tasks, which exiting would cancel; \
                 left it on its account",
            );
        }
        if let Some(dialog) = pending_dialog(&screen, EXIT_DIALOGS, answered) {
            if type_keys(send, dialog.keys) {
                answered.push(dialog.label);
            }
            std::thread::sleep(dialog.pause);
            continue;
        }
        if open_question(&screen).is_some() && started.elapsed() > Duration::from_secs(5) {
            send("\x1b");
            return keep(
                "/exit asked a question muxa does not know how to answer; \
                 left the session running",
            );
        }
    }
    if alive() {
        // Wedged or not reading input: fall back to the signal path.
        stop(launch)?;
    }
    Ok(())
}

/// Does the exit dialog on `screen` warn that scheduled tasks would stop?
fn owns_scheduled_tasks(screen: &str) -> bool {
    let tail = screen_tail(screen, DIALOG_TAIL_LINES);
    tail.contains("Exit and stop tasks") && tail.contains("scheduled task")
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

/// Is `command` an interactive shell, one with a prompt a relaunch can be
/// typed at? `sh script` and `sh -c '…'` run a command and have none; any
/// other option (`--norc`, `-o vi`, `-NoLogo`) leaves a shell interactive.
fn is_shell(command: &str) -> bool {
    let mut words = command.split_whitespace();
    if !crate::shell::is_shell_program(words.next().unwrap_or_default()) {
        return false;
    }
    !words.any(|word| {
        let short_c = word.starts_with('-')
            && !word.starts_with("--")
            && word[1..].chars().all(|c| c.is_ascii_alphabetic())
            && word.contains('c');
        // A script: a path, not an option value like `vi` in `-o vi`.
        let script = !word.starts_with('-')
            && (word.contains('/')
                || std::path::Path::new(word)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("sh")));
        short_c || script
    })
}

/// Is there an interactive shell between the agent and its pane's root
/// (the root included), one a relaunch can be typed into? `None` when the
/// root could not be read. A pane can start a wrapper that starts the shell
/// (`reattach-to-user-namespace -l zsh`), so the root alone does not tell.
fn shell_above(processes: &[ProcessInfo], agent_pid: u32, pane_pid: u32) -> Option<bool> {
    let by_pid = |pid: u32| processes.iter().find(|p| p.pid == pid);
    let root = by_pid(pane_pid)?;
    if agent_pid == pane_pid {
        return Some(false);
    }
    let mut pid = by_pid(agent_pid)?.parent_pid;
    while pid != pane_pid {
        let process = by_pid(pid)?;
        if is_shell(&process.cmdline) {
            return Some(true);
        }
        pid = process.parent_pid;
    }
    Some(is_shell(&root.cmdline))
}

/// Every process in a pane: its root, then the root's descendants. The root
/// is the agent itself in a pane started as `exec claude …`.
fn pane_processes(pane_pid: u32) -> Vec<ProcessInfo> {
    let mut processes = crate::process_tree::read_descendants(pane_pid);
    if let Some(cmdline) = process_command(pane_pid) {
        processes.insert(
            0,
            ProcessInfo {
                pid: pane_pid,
                parent_pid: 0,
                depth: 0,
                comm: cmdline
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_owned(),
                cmdline,
            },
        );
    }
    processes
}

/// Is the relaunched agent, on this conversation, still running in the pane?
fn resumed_agent_running(target: &Target) -> bool {
    pane_processes(target.pane_pid.get())
        .iter()
        .any(|p| is_agent_process(p, target.kind) && p.cmdline.contains(target.session_id.as_str()))
}

/// What a resumed pane turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Resume {
    /// Settled on a screen with no ownership or startup error, agent alive.
    Ready,
    /// Settled on an ownership or startup-error screen.
    Blocked(String),
    /// The agent process went away before the screen was usable.
    Exited,
    /// Never settled within the timeout.
    Unsettled,
}

/// Wait for the resumed TUI to stop redrawing, then accept it only if the
/// agent is still running and the screen is not an ownership or startup
/// error. A stable screen alone proves nothing: "This conversation is open
/// in another app" is stable too (Open330/muxa#275).
#[cfg(test)]
fn wait_for_resume(
    capture: impl FnMut() -> Option<String>,
    alive: impl FnMut() -> bool,
    timeout: Duration,
    quiet: Duration,
    tick: Duration,
) -> Resume {
    wait_for_resume_answering(
        capture,
        alive,
        |_| false,
        timeout,
        quiet,
        tick,
        &mut Vec::new(),
    )
}

/// [`wait_for_resume`], answering the startup dialogs in
/// [`STARTUP_DIALOGS`] through `answer` (which types keys into the pane and
/// reports whether it could) and recording each answer in `answered`. A
/// settled screen that still asks a question is never `Ready`: a follow-up
/// typed into Claude's folder-trust prompt picks its default, "No, exit".
fn wait_for_resume_answering(
    mut capture: impl FnMut() -> Option<String>,
    mut alive: impl FnMut() -> bool,
    mut answer: impl FnMut(&[&str]) -> bool,
    timeout: Duration,
    quiet: Duration,
    tick: Duration,
    answered: &mut Vec<&'static str>,
) -> Resume {
    let started = Instant::now();
    let mut last = String::new();
    let mut quiet_since = Instant::now();
    loop {
        let screen = capture().unwrap_or_default();
        if screen != last {
            last = screen;
            quiet_since = Instant::now();
        } else if !last.trim().is_empty() && quiet_since.elapsed() >= quiet {
            if answered_dialog_lingers(&last, answered)
                && open_question(&last).is_some()
                && started.elapsed() < timeout
            {
                // Answered already; the agent has not redrawn yet.
                quiet_since = Instant::now();
                std::thread::sleep(tick);
                continue;
            }
            if let Some(dialog) = pending_dialog(&last, STARTUP_DIALOGS, answered) {
                if answer(dialog.keys) {
                    answered.push(dialog.label);
                    // The answer redraws the screen; wait for it to settle again.
                    last.clear();
                    quiet_since = Instant::now();
                    // Bounded by the overall timeout so retries cannot outlast it.
                    std::thread::sleep(dialog.pause.min(timeout / 8));
                    continue;
                }
                return Resume::Blocked(format!("a startup dialog is waiting ({})", dialog.label));
            }
            if let Some(reason) = blocked_screen(&last) {
                return Resume::Blocked(reason.to_owned());
            }
            if let Some(line) = open_question(&last) {
                return Resume::Blocked(format!("a dialog is waiting for an answer ({line})"));
            }
            if !alive() {
                return Resume::Exited;
            }
            // Quiet is not ready: a wrapper or the agent itself can sit on
            // the shell's echo of the relaunch for seconds before the TUI
            // (or a trust dialog) draws. Wait for the agent's own prompt.
            if !agent_prompt_visible(&last) {
                if started.elapsed() >= timeout {
                    return Resume::Unsettled;
                }
                quiet_since = Instant::now();
                std::thread::sleep(tick);
                continue;
            }
            return Resume::Ready;
        }
        if started.elapsed() >= timeout {
            // A blocker can sit on a screen that keeps redrawing (a clock,
            // a spinner): report it rather than a bare timeout.
            if let Some(reason) = blocked_screen(&last) {
                return Resume::Blocked(reason.to_owned());
            }
            if let Some(line) = open_question(&last) {
                return Resume::Blocked(format!("a dialog is waiting for an answer ({line})"));
            }
            return if alive() {
                Resume::Unsettled
            } else {
                Resume::Exited
            };
        }
        std::thread::sleep(tick);
    }
}

/// Marks of an agent waiting at its own prompt, on the last lines of the
/// screen: Claude's mode and shortcut line, Codex's `›` composer. Claude's
/// `❯` is left out: shell prompts use it too.
const PROMPT_MARKS: &[&str] = &[
    "⏵⏵",
    "for shortcuts",
    "bypass permissions",
    "auto mode on",
    "accept edits on",
    "plan mode on",
    "Ask Codex",
];

/// Is the resumed agent's prompt on screen (not the shell's, not a dialog)?
fn agent_prompt_visible(screen: &str) -> bool {
    let tail = screen_tail(screen, 10);
    PROMPT_MARKS.iter().any(|mark| tail.contains(mark))
        || tail.lines().any(|line| line.starts_with("› "))
}

/// How many non-blank lines at the bottom of the screen a startup banner or
/// error can occupy. Only these are read, so a resumed conversation's own
/// history (which may quote the same words) cannot trip the check.
const BLOCKER_TAIL_LINES: usize = 8;

/// Line openings that mean the resumed agent cannot take input, with the
/// note shown to the operator. Matched case-insensitively at the start of a
/// line, after any leading box-drawing, bullet or `error:` decoration.
const BLOCKERS: &[(&str, &str)] = &[
    (
        "this conversation is open in another app",
        "another app still owns this conversation (\"This conversation is open in another app\")",
    ),
    (
        "conversation is open in another",
        "another app still owns this conversation",
    ),
    (
        "no conversation found",
        "the agent could not find the conversation to resume",
    ),
    (
        "no session found",
        "the agent could not find the session to resume",
    ),
    (
        "session not found",
        "the agent could not find the session to resume",
    ),
    (
        "failed to resume",
        "the agent failed to resume the conversation",
    ),
    (
        "unable to resume",
        "the agent failed to resume the conversation",
    ),
    (
        "too many open files",
        "the agent hit the open-file limit while starting",
    ),
    (
        "unexpected argument",
        "the agent rejected the relaunch arguments",
    ),
    (
        "command not found",
        "the relaunch command was not found in the pane's shell",
    ),
];

/// The reason a settled screen cannot take input, if it shows one.
fn blocked_screen(screen: &str) -> Option<&'static str> {
    let lines: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = &lines[lines.len().saturating_sub(BLOCKER_TAIL_LINES)..];
    tail.iter().find_map(|line| {
        let text = line.trim_start_matches(|c: char| {
            c.is_whitespace()
                || matches!(
                    c,
                    '│' | '┃'
                        | '║'
                        | '╭'
                        | '╰'
                        | '■'
                        | '▌'
                        | '⚠'
                        | '🔒'
                        | '✗'
                        | '×'
                        | '•'
                        | '›'
                        | '>'
                        | '!'
                        | '-'
                        | '*'
                )
        });
        // Work on the lowercased copy only: its byte offsets need not match
        // the original's once non-ASCII text is lowercased.
        let mut text = text.to_lowercase();
        // `zsh: command not found: aas` — the shell names itself first.
        for prefixes in [
            &["zsh:", "bash:", "sh:", "fish:"][..],
            &["error:", "warning:", "fatal:", "error"][..],
        ] {
            if let Some(rest) = prefixes.iter().find_map(|p| text.strip_prefix(p)) {
                text = rest.trim_start().to_owned();
            }
        }
        BLOCKERS
            .iter()
            .find(|(opening, _)| text.starts_with(opening))
            .map(|(_, reason)| *reason)
    })
}

/// A dialog a resumed or exiting agent can stop on, and the keys that answer
/// it the way a swap needs.
struct Dialog {
    /// Every one of these must be on the screen's last
    /// [`DIALOG_TAIL_LINES`] non-blank lines.
    needles: &'static [&'static str],
    /// Keys typed to answer it.
    keys: &'static [&'static str],
    /// Named in the report.
    label: &'static str,
    /// How many times one swap may answer it.
    max: usize,
    /// Time for the agent to act on the answer before the screen is read again.
    pause: Duration,
}

/// Down arrow and Enter, as typed into a pane. A dialog's keys go one at a
/// time: Claude's input layer reads `ESC [ B CR` arriving in one write as
/// a single unknown sequence, not as Down then Enter.
const DOWN: &str = "\x1b[B";
const ENTER: &str = "\r";
/// Pause between the keys of one answer.
const KEY_GAP: Duration = Duration::from_millis(300);

/// Type `keys` one by one, [`KEY_GAP`] apart. `false` if any could not be.
fn type_keys(mut send: impl FnMut(&str) -> bool, keys: &[&str]) -> bool {
    keys.iter().enumerate().all(|(i, key)| {
        if i > 0 {
            std::thread::sleep(KEY_GAP);
        }
        send(key)
    })
}

/// How many non-blank lines at the bottom of the screen a dialog occupies.
const DIALOG_TAIL_LINES: usize = 16;

/// Dialogs a relaunched agent shows before its conversation takes input.
/// The account changed, not the folder or the conversation, so each is
/// answered the way the agent was already running.
const STARTUP_DIALOGS: &[Dialog] = &[
    // Claude, on a profile that has not seen this folder. The default row is
    // "No, exit".
    Dialog {
        needles: &["Yes, I trust this folder", "Enter to confirm"],
        keys: &[DOWN, ENTER],
        label: "trusted the folder",
        max: 1,
        pause: Duration::from_millis(500),
    },
    Dialog {
        needles: &["Teach auto mode", "Enter to confirm"],
        keys: &[DOWN, ENTER],
        label: "dismissed the auto-mode tour",
        max: 1,
        pause: Duration::from_millis(500),
    },
    // Codex: "1. Trust and continue" is the default row.
    Dialog {
        needles: &["Trust this folder", "Trust and continue"],
        keys: &[ENTER],
        label: "trusted the folder",
        max: 1,
        pause: Duration::from_millis(500),
    },
    // Codex: the conversation's own last directory is the default row.
    Dialog {
        needles: &["Working directory · resume", "Use session directory"],
        keys: &[ENTER],
        label: "kept the session directory",
        max: 1,
        pause: Duration::from_millis(500),
    },
    Dialog {
        needles: &["1. Resume goal", "enter select"],
        keys: &[ENTER],
        label: "resumed the paused goal",
        max: 1,
        pause: Duration::from_millis(500),
    },
    // Codex: the old process's app-server can still hold the thread for a
    // few seconds after it exits.
    Dialog {
        needles: &["open in another app", "r to retry"],
        keys: &["r"],
        label: "retried a conversation another app still held",
        max: 5,
        pause: Duration::from_secs(3),
    },
];

/// What `/exit` may ask before Claude quits, and what a swap answers.
const EXIT_DIALOGS: &[Dialog] = &[
    // Background shells or agents: keep them running past the exit.
    Dialog {
        needles: &["Move to background and exit", "Enter to confirm"],
        keys: &[DOWN, ENTER],
        label: "moved background work out of the way",
        max: 1,
        pause: Duration::from_millis(500),
    },
    Dialog {
        needles: &["Keep worktree", "Enter to confirm"],
        keys: &[ENTER],
        label: "kept the worktree",
        max: 1,
        pause: Duration::from_millis(500),
    },
];

/// The first dialog of `table` on `screen` that this swap may still answer.
fn pending_dialog<'a>(
    screen: &str,
    table: &'a [Dialog],
    answered: &[&'static str],
) -> Option<&'a Dialog> {
    let tail = screen_tail(screen, DIALOG_TAIL_LINES);
    table.iter().find(|dialog| {
        dialog.needles.iter().all(|needle| tail.contains(needle))
            && answered
                .iter()
                .filter(|label| **label == dialog.label)
                .count()
                < dialog.max
    })
}

/// Is `screen` still showing a startup dialog this swap has already answered
/// as often as it may, other than a lock it gave up retrying? Paired with
/// [`open_question`]: output below an answered dialog means it is gone.
fn answered_dialog_lingers(screen: &str, answered: &[&'static str]) -> bool {
    let tail = screen_tail(screen, DIALOG_TAIL_LINES);
    STARTUP_DIALOGS.iter().any(|dialog| {
        dialog.max == 1
            && answered.contains(&dialog.label)
            && dialog.needles.iter().all(|needle| tail.contains(needle))
    })
}

/// The last `lines` non-blank lines of `screen`, joined.
fn screen_tail(screen: &str, lines: usize) -> String {
    let rows: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
    rows[rows.len().saturating_sub(lines)..].join("\n")
}

/// Footers of a selection dialog, read case-insensitively on the last few
/// lines. One means the agent waits for a choice muxa did not make.
const QUESTION_FOOTERS: &[&str] = &[
    "enter to confirm",
    "esc to cancel",
    "enter select",
    "enter continue",
    "r to retry",
];

/// The footer line of a dialog still waiting for an answer, if any. A
/// dialog's key hints are its last line (two when a narrow pane wraps
/// them); once answered, output below them means it is gone.
fn open_question(screen: &str) -> Option<String> {
    let tail = screen_tail(screen, 2);
    tail.lines()
        .find(|line| {
            let line = line.to_lowercase();
            QUESTION_FOOTERS.iter().any(|footer| line.contains(footer))
        })
        .map(|line| line.trim().chars().take(80).collect())
}

/// Codex app-servers outside this pane, which can keep writer ownership of a
/// conversation after the pane's own agent exits. Named, never stopped: one
/// shared server serves unrelated agents.
fn codex_writer_owners(pane_pid: u32) -> String {
    let processes = all_processes();
    let in_pane: std::collections::HashSet<u32> = crate::process_tree::read_descendants(pane_pid)
        .iter()
        .map(|p| p.pid)
        .collect();
    let owners = outside_codex_servers(processes.iter(), pane_pid, &in_pane);
    if owners.is_empty() {
        String::new()
    } else {
        format!(
            " — Codex app-server(s) outside the pane that may hold it: {}; \
             close the other client or restart that server when it is safe",
            owners.join(", ")
        )
    }
}

/// Every process's pid and command line. Linux reads `/proc` (there is no
/// whole-table snapshot there); elsewhere the shared process table.
#[cfg(target_os = "linux")]
fn all_processes() -> Vec<ProcessInfo> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            let raw = std::fs::read(entry.path().join("cmdline")).ok()?;
            let cmdline = String::from_utf8_lossy(&raw)
                .split('\0')
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            Some(ProcessInfo {
                pid,
                parent_pid: 0,
                depth: 0,
                comm: String::new(),
                cmdline,
            })
        })
        .collect()
}

#[cfg(not(target_os = "linux"))]
fn all_processes() -> Vec<ProcessInfo> {
    crate::process_snapshot::read_current_process_table()
        .processes()
        .cloned()
        .collect()
}

fn outside_codex_servers<'a>(
    processes: impl Iterator<Item = &'a ProcessInfo>,
    pane_pid: u32,
    in_pane: &std::collections::HashSet<u32>,
) -> Vec<String> {
    let mut owners: Vec<(u32, String)> = processes
        .filter(|p| p.pid != pane_pid && !in_pane.contains(&p.pid))
        .filter(|p| {
            let mut words = p.cmdline.split_whitespace();
            words.any(|w| w.rsplit('/').next() == Some("codex"))
                && p.cmdline.split_whitespace().any(|w| w == "app-server")
        })
        .map(|p| (p.pid, format!("pid {}", p.pid)))
        .collect();
    owners.sort();
    owners.into_iter().map(|(_, label)| label).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Open330/muxa#275: a running, settled TUI is not a resumed one ----

    const OWNED_ELSEWHERE: &str =
        "\n\n  ■ This conversation is open in another app.\n    Close it there to continue here.\n";
    const RESUMED: &str = "› Plan rate limiting\n\n• Edited src/limit.ts\n\n› Ask Codex to do anything\n  ⏎ send   ⌃J newline\n";

    fn quick(screens: Vec<&'static str>, alive: bool) -> Resume {
        let mut screens = screens.into_iter().map(str::to_owned);
        let mut last = String::new();
        wait_for_resume(
            || {
                if let Some(next) = screens.next() {
                    last = next;
                }
                Some(last.clone())
            },
            || alive,
            Duration::from_millis(300),
            Duration::from_millis(20),
            Duration::from_millis(5),
        )
    }

    #[test]
    fn a_stable_ownership_banner_is_blocked_not_ready() {
        assert!(
            matches!(quick(vec![OWNED_ELSEWHERE], true), Resume::Blocked(r) if r.contains("another app"))
        );
    }

    #[test]
    fn startup_errors_are_blocked() {
        for screen in [
            "Error: No conversation found with session ID 019a\n$ ",
            "error: unexpected argument '--resume' found\n",
            "⚠ Too many open files (os error 24)\n",
            "zsh: command not found: aas\n$ ",
        ] {
            assert!(blocked_screen(screen).is_some(), "{screen:?}");
        }
    }

    #[test]
    fn a_settled_resumed_conversation_with_a_live_agent_is_ready() {
        assert_eq!(quick(vec!["", "loading", RESUMED], true), Resume::Ready);
    }

    #[test]
    fn a_settled_screen_whose_agent_exited_is_not_ready() {
        assert_eq!(quick(vec![RESUMED], false), Resume::Exited);
    }

    #[test]
    fn history_that_quotes_a_blocker_does_not_trip_the_check() {
        let mut screen = String::from(
            "› why did the swap fail?\n• It said: This conversation is open in another app.\n",
        );
        screen.push_str(&"• more history\n".repeat(BLOCKER_TAIL_LINES));
        screen.push_str(RESUMED);
        assert_eq!(blocked_screen(&screen), None);
        // Mid-line mentions are not banners either.
        assert_eq!(
            blocked_screen("• the log said no conversation found yesterday\n› Ask Codex\n"),
            None
        );
    }

    // ---- startup dialogs a relaunch must answer, never type a follow-up into ----

    /// Claude on a profile that has not seen the folder (captured live).
    const CLAUDE_TRUST: &str = " Accessing workspace:\n /Users/june/personal/barshelf\n \
        Quick safety check: Is this a project you created or one you trust?\n \
        ❯ No, exit\n   Yes, I trust this folder\n Enter to confirm · Esc to cancel\n";
    const CLAUDE_READY: &str = "⏺ Resumed.\n────────\n❯ \n────────\n  \
        ⏵⏵ bypass permissions on (shift+tab to cycle)\n";
    const CODEX_LOCKED: &str =
        "  🔒  This conversation is open in another app        r to retry\n      \
        Close it there and press R to continue here.\n   r retry   f fork   esc/⌃c/q exit\n";

    /// Drive `wait_for_resume_answering` over a screen that becomes `after`
    /// once it has been answered `flips_after` times.
    fn answering(
        before: &'static str,
        after: &'static str,
        flips_after: usize,
        can_answer: bool,
    ) -> (Resume, Vec<&'static str>, Vec<String>) {
        let keys = std::cell::RefCell::new(Vec::<String>::new());
        let mut answered = Vec::new();
        let resume = wait_for_resume_answering(
            || {
                let n = keys.borrow().len();
                Some(if n >= flips_after { after } else { before }.to_owned())
            },
            || true,
            |k| {
                if can_answer {
                    keys.borrow_mut().push(k.concat());
                }
                can_answer
            },
            Duration::from_secs(10),
            Duration::from_millis(20),
            Duration::from_millis(5),
            &mut answered,
        );
        (resume, answered, keys.into_inner())
    }

    #[test]
    fn a_coloured_trust_prompt_is_still_recognised() {
        // Claude colours each footer word apart; `capture-pane -e` keeps it.
        let raw = " ❯ No, exit\n   Yes, I trust this folder\n \
            \x1b[38;5;246mEnter\x1b[39m \x1b[38;5;246mto\x1b[39m \x1b[38;5;246mconfirm\x1b[39m \
            \x1b[38;5;246m·\x1b[39m \x1b[38;5;246mEsc\x1b[39m \x1b[38;5;246mto\x1b[39m \x1b[38;5;246mcancel\x1b[39m\n";
        assert!(
            pending_dialog(raw, STARTUP_DIALOGS, &[]).is_none(),
            "raw escapes defeat it"
        );
        let plain = crate::screen::strip_ansi(raw);
        assert_eq!(
            pending_dialog(&plain, STARTUP_DIALOGS, &[]).map(|d| d.label),
            Some("trusted the folder")
        );
    }

    #[test]
    fn a_quiet_screen_before_the_agent_draws_is_not_ready() {
        // Seen live: `aas e …` printed nothing for seconds, the swap took the
        // shell's echo for a ready agent, and the trust prompt drew after.
        let echo = "~ ❯ aas e k-june@claude --dangerously-skip-permissions --resume c63f\n";
        let mut captures = 0usize;
        let mut answered = Vec::new();
        let keys = std::cell::RefCell::new(Vec::<String>::new());
        let resume = wait_for_resume_answering(
            || {
                captures += 1;
                Some(
                    match (captures, keys.borrow().len()) {
                        (0..=30, _) => echo,
                        (_, 0) => CLAUDE_TRUST,
                        _ => CLAUDE_READY,
                    }
                    .to_owned(),
                )
            },
            || true,
            |k| {
                keys.borrow_mut().push(k.concat());
                true
            },
            Duration::from_secs(10),
            Duration::from_millis(20),
            Duration::from_millis(5),
            &mut answered,
        );
        assert_eq!(resume, Resume::Ready);
        assert_eq!(answered, vec!["trusted the folder"]);
    }

    #[test]
    fn an_agent_that_never_draws_its_prompt_is_unsettled_not_ready() {
        let echo = "~ ❯ aas e k-june@claude --resume c63f\n";
        let resume = wait_for_resume(
            || Some(echo.to_owned()),
            || true,
            Duration::from_millis(150),
            Duration::from_millis(20),
            Duration::from_millis(5),
        );
        assert_eq!(resume, Resume::Unsettled);
    }

    #[test]
    fn claudes_trust_prompt_is_answered_yes_before_the_session_counts_as_ready() {
        let (resume, answered, keys) = answering(CLAUDE_TRUST, CLAUDE_READY, 1, true);
        assert_eq!(resume, Resume::Ready);
        assert_eq!(answered, vec!["trusted the folder"]);
        // Down to "Yes", then Enter: Enter alone would pick "No, exit".
        assert_eq!(keys, vec!["\x1b[B\r".to_owned()]);
    }

    #[test]
    fn a_trust_prompt_that_redraws_slowly_after_its_answer_is_waited_out() {
        let captures_after = std::cell::Cell::new(0usize);
        let answers = std::cell::Cell::new(0usize);
        let mut answered = Vec::new();
        let resume = wait_for_resume_answering(
            || {
                if answers.get() == 0 {
                    return Some(CLAUDE_TRUST.to_owned());
                }
                captures_after.set(captures_after.get() + 1);
                // Well past the quiet window before the answer shows.
                Some(
                    if captures_after.get() < 40 {
                        CLAUDE_TRUST
                    } else {
                        CLAUDE_READY
                    }
                    .to_owned(),
                )
            },
            || true,
            |_| {
                answers.set(answers.get() + 1);
                true
            },
            Duration::from_secs(10),
            Duration::from_millis(20),
            Duration::from_millis(5),
            &mut answered,
        );
        assert_eq!(resume, Resume::Ready);
        assert_eq!(answers.get(), 1, "answered once, not again");
    }

    #[test]
    fn a_trust_prompt_that_cannot_be_answered_is_blocked_not_ready() {
        let (resume, _, _) = answering(CLAUDE_TRUST, CLAUDE_READY, 1, false);
        assert!(matches!(resume, Resume::Blocked(r) if r.contains("trusted the folder")));
    }

    #[test]
    fn an_unknown_dialog_is_blocked_so_no_follow_up_is_typed_into_it() {
        let unknown =
            "  Something new?\n  ❯ 1. Yes\n    2. No\n  Enter to confirm · Esc to cancel\n";
        let (resume, answered, keys) = answering(unknown, CLAUDE_READY, 1, true);
        assert!(matches!(resume, Resume::Blocked(r) if r.contains("Enter to confirm")));
        assert!(answered.is_empty() && keys.is_empty());
    }

    #[test]
    fn a_conversation_another_app_still_holds_is_retried_then_given_up_on() {
        let (resume, answered, keys) = answering(CODEX_LOCKED, RESUMED, 2, true);
        assert_eq!(resume, Resume::Ready);
        assert_eq!(answered.len(), 2);
        assert!(keys.iter().all(|k| k == "r"));
        // Still held after five tries: reported, not waited on forever.
        let (resume, answered, _) = answering(CODEX_LOCKED, RESUMED, usize::MAX, true);
        assert!(matches!(resume, Resume::Blocked(r) if r.contains("another app")));
        assert_eq!(answered.len(), 5);
    }

    #[test]
    fn codex_resume_menus_keep_the_session_directory_and_the_goal() {
        let directory = "  Working directory · resume\n  Session = latest cwd recorded in the resumed session\n\
            › 1. Use session directory (/Users/jiun/personal/steam)\n  2. Use current directory (/Users/jiun)\n\
              enter continue · esc use session · ⌃c quit\n";
        let (resume, answered, keys) = answering(directory, RESUMED, 1, true);
        assert_eq!(resume, Resume::Ready);
        assert_eq!(answered, vec!["kept the session directory"]);
        assert_eq!(keys, vec!["\r".to_owned()]);
        let goal = "    필요한 구현·테스트·문서 개선을 완료한다.\n  › 1. Resume goal   Mark it active\n    2. Leave paused\n    enter select · esc back\n";
        let (resume, answered, _) = answering(goal, RESUMED, 1, true);
        assert_eq!(resume, Resume::Ready);
        assert_eq!(answered, vec!["resumed the paused goal"]);
    }

    #[test]
    fn exit_dialogs_keep_background_work_and_worktrees_and_spot_scheduled_tasks() {
        let background = "   Background work is running\n   The following will stop when you exit:\n   \
            shell · bash /tmp/build.sh\n   ❯ 1. Exit and stop tasks\n     2. Move to background and exit\n     \
            3. Stay\n   Enter to confirm · Esc to cancel\n";
        let dialog = pending_dialog(background, EXIT_DIALOGS, &[]).expect("background dialog");
        assert_eq!(dialog.keys, [DOWN, ENTER]);
        assert!(!owns_scheduled_tasks(background));
        let worktree = "   Exiting worktree session\n   ❯ 1. Keep worktree    Stays at /x/.claude/worktrees/a\n     \
            2. Remove worktree  Clean up the worktree directory.\n   Enter to confirm · Esc to cancel\n";
        assert_eq!(
            pending_dialog(worktree, EXIT_DIALOGS, &[]).unwrap().keys,
            [ENTER]
        );
        let scheduled = "   scheduled task · Every day at 9:03 AM · [mail check]\n   ❯ 1. Exit and stop tasks\n     \
            2. Move to background and exit\n     3. Stay\n   Enter to confirm · Esc to cancel\n";
        assert!(owns_scheduled_tasks(scheduled));
        // An answered dialog is not answered twice.
        assert!(pending_dialog(worktree, EXIT_DIALOGS, &["kept the worktree"]).is_none());
    }

    #[test]
    fn a_bare_agent_counts_as_on_the_active_account_so_auto_moves_it_elsewhere() {
        let mut active = candidate("e-ed@claude", true);
        active.active = true;
        let candidates = vec![active, candidate("k-june@claude", true)];
        let bare = LaunchLine::bare("claude --dangerously-skip-permissions");
        let current = current_account(&bare, &candidates);
        assert_eq!(current.as_deref(), Some("e-ed@claude"));
        let moves = assign(&[current], &candidates, &AccountChoice::Auto);
        assert_eq!(moves, vec![Ok("k-june@claude".to_owned())]);
        // A launch line's own account still wins.
        let named = LaunchLine::from_aas("aas e k-june@claude").unwrap();
        assert_eq!(
            current_account(&named, &candidates).as_deref(),
            Some("k-june@claude")
        );
    }

    #[test]
    fn a_screen_that_never_settles_is_unsettled_unless_it_shows_a_blocker() {
        fn spinning(prefix: &'static str) -> Resume {
            let mut tick = 0u32;
            wait_for_resume(
                || {
                    tick += 1;
                    Some(format!("{prefix}spinner {tick}\n"))
                },
                || true,
                Duration::from_millis(60),
                Duration::from_millis(40),
                Duration::from_millis(5),
            )
        }
        assert_eq!(spinning(""), Resume::Unsettled);
        assert!(matches!(spinning(OWNED_ELSEWHERE), Resume::Blocked(_)));
    }

    #[test]
    fn a_detached_codex_server_outside_the_pane_is_named_and_the_panes_own_is_not() {
        let processes = [
            proc(10, 1, 0, "-zsh"),
            proc(11, 10, 1, "node /opt/homebrew/bin/codex resume 019a"),
            proc(12, 11, 2, "/opt/homebrew/bin/codex app-server"),
            proc(
                40,
                1,
                0,
                "/opt/homebrew/bin/codex app-server --managed-daemon",
            ),
            proc(41, 1, 0, "/usr/bin/vim app-server.md"),
        ];
        let in_pane: std::collections::HashSet<u32> = [11, 12].into_iter().collect();
        assert_eq!(
            outside_codex_servers(processes.iter(), 10, &in_pane),
            vec!["pid 40".to_owned()]
        );
    }
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
    /// The live tests point the tmux backend at their own server through the
    /// process-wide `MUXA_TMUX_SOCKET`; run in parallel, one would swap the
    /// other's pane.
    static LIVE_TMUX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn live_tmux() -> std::sync::MutexGuard<'static, ()> {
        LIVE_TMUX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    // ---- panes that run the agent directly, drafts, resumed turns ----

    #[test]
    fn only_a_shell_waiting_at_its_prompt_counts_as_one() {
        for shell in [
            "-zsh",
            "/bin/zsh",
            "bash -l",
            "zsh -i",
            "/opt/homebrew/bin/fish",
            "bash --norc",
            "zsh -o vi",
            "pwsh -NoLogo",
        ] {
            assert!(is_shell(shell), "{shell}");
        }
        for not_one in [
            "sh /tmp/bin/aas e x@claude",
            "zsh -lic 'claude; exec zsh'",
            "sh -c claude",
            "claude",
        ] {
            assert!(!is_shell(not_one), "{not_one}");
        }
    }

    #[test]
    fn a_shell_behind_a_wrapper_still_takes_the_relaunch() {
        // tmux's default-command on macOS: the root is not the shell.
        let processes = [
            proc(10, 0, 0, "reattach-to-user-namespace -l /bin/zsh"),
            proc(11, 10, 1, "/bin/zsh"),
            proc(12, 11, 2, "claude --resume x"),
        ];
        assert_eq!(shell_above(&processes, 12, 10), Some(true));
        let direct = [
            proc(20, 0, 0, "/bin/sh /tmp/bin/aas e x@claude"),
            proc(21, 20, 1, "/bin/sh /tmp/bin/claude"),
        ];
        assert_eq!(shell_above(&direct, 21, 20), Some(false));
        // An unreadable root is never taken as "no shell".
        assert_eq!(shell_above(&processes[1..], 12, 10), None);
    }

    #[test]
    fn an_agent_that_is_its_panes_root_process_is_found() {
        let processes = [proc(
            10,
            0,
            0,
            "claude --dangerously-skip-permissions --resume 5a95b526-0439-4938-adb8-a24055198731",
        )];
        let launch = find_launch(&processes, 10, AgentKind::ClaudeCode).unwrap();
        assert_eq!((launch.agent_pid, launch.top_pid), (10, 10));
        assert_eq!(
            launch.resumed_id.as_deref(),
            Some("5a95b526-0439-4938-adb8-a24055198731")
        );
    }

    #[test]
    fn excluded_panes_are_left_alone_whatever_the_scope() {
        let agents = [limited("%1", "s1"), limited("%2", "s2")];
        let mut req = request(SwapScope::Limited);
        req.exclude = vec!["%1".into()];
        let (chosen, skipped) = select(&agents, &req);
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].pane.as_deref(), Some("%2"));
        assert_eq!(skipped[0].note, "excluded");
    }

    #[test]
    fn recent_activity_counts_as_mid_turn_unless_forced_or_capped() {
        let now = time::OffsetDateTime::now_utc();
        let mut quiet = agent("%1", "s1", AgentKind::ClaudeCode, AgentState::Idle);
        quiet.last_activity_at = now - time::Duration::seconds(5);
        let mut capped = limited("%2", "s2");
        capped.last_activity_at = now;
        let agents = [quiet, capped];
        let mut req = request(SwapScope::All);
        let (chosen, skipped) = select_at(&agents, &req, now);
        assert_eq!(chosen.len(), 1, "the capped one goes");
        assert!(
            skipped[0].note.contains("active in the last 20s"),
            "{skipped:?}"
        );
        req.force = true;
        assert_eq!(select_at(&agents, &req, now).0.len(), 2);
    }

    #[test]
    fn a_half_typed_prompt_is_read_back_for_both_agents() {
        let claude = "⏺ Done.\n────────\n❯ fix the login\n  redirect too\n────────\n  ⏵⏵ bypass permissions on\n";
        assert_eq!(
            composer_draft(claude, AgentKind::ClaudeCode).as_deref(),
            Some("fix the login redirect too")
        );
        let empty = "⏺ Done.\n────────\n❯ \n────────\n  ⏵⏵ bypass permissions on\n";
        assert_eq!(composer_draft(empty, AgentKind::ClaudeCode), None);
        let codex = "• Edited x\n› write a test for the parser\n  GPT-6.1-Sol medium · ~/p\n";
        assert_eq!(
            composer_draft(codex, AgentKind::Codex).as_deref(),
            Some("write a test for the parser")
        );
        for idle in [
            "› Ask Codex to do anything\n",
            "› 1. Trust and continue\n  2. Quit\n  enter continue · esc quit\n",
        ] {
            assert_eq!(composer_draft(idle, AgentKind::Codex), None, "{idle}");
        }
    }

    #[test]
    fn a_resumed_turn_is_recognised_on_either_tui() {
        assert!(turn_running("✻ Thinking… (12s · ↓ 300 tokens)\n────\n❯ \n"));
        assert!(turn_running(
            "• Working (5s • esc to interrupt)\n› Ask Codex to do anything\n"
        ));
        assert!(!turn_running(
            "✻ Cooked for 1m 2s · done 4:45 PM\n────\n❯ \n"
        ));
        assert!(!turn_running(RESUMED));
    }

    // ---- configured accounts: no aas involved ----

    fn account(name: &str, key: &str, value: &str) -> crate::accounts::AccountProfile {
        crate::accounts::AccountProfile {
            name: name.into(),
            env: [(key.to_owned(), value.to_owned())].into(),
            env_file: None,
        }
    }

    #[test]
    fn an_aas_started_agent_moves_onto_a_configured_account_without_aas() {
        let line = LaunchLine::from_aas("aas e e-ed@claude --dangerously-skip-permissions fix it")
            .unwrap();
        let work = account(
            "work",
            "CLAUDE_CONFIG_DIR",
            "/Users/j/Library/Application Support/claude-work",
        );
        let command = profile_relaunch_line(&line, AgentKind::ClaudeCode, "sid-1", &work).unwrap();
        assert!(!command.contains("aas"), "{command}");
        assert!(
            command.starts_with(
                "env 'CLAUDE_CONFIG_DIR=/Users/j/Library/Application Support/claude-work' claude"
            ),
            "{command}"
        );
        assert!(
            command.ends_with("claude --dangerously-skip-permissions --resume sid-1"),
            "{command}"
        );
    }

    #[test]
    fn a_bare_codex_behind_node_keeps_its_own_binary_and_flags() {
        let line = LaunchLine::bare("node /opt/bin/codex --yolo resume 019a-old");
        let home = account("home", "CODEX_HOME", "/h/.codex-home");
        let command = profile_relaunch_line(&line, AgentKind::Codex, "019a-new", &home).unwrap();
        assert_eq!(
            command,
            "env CODEX_HOME=/h/.codex-home /opt/bin/codex --yolo resume 019a-new"
        );
    }

    #[test]
    fn configured_accounts_rank_in_order_and_skip_the_current_and_capped_ones() {
        let profiles = vec![
            account("a", "K", "1"),
            account("b", "K", "2"),
            account("c", "K", "3"),
        ];
        let ranked = profile_candidates(&profiles, "claude", &["b"]);
        assert_eq!(
            ranked.iter().map(|c| c.eligible).collect::<Vec<_>>(),
            [true, false, true]
        );
        let moves = assign(&[Some("a".to_owned()), None], &ranked, &AccountChoice::Auto);
        assert_eq!(moves, vec![Ok("c".to_owned()), Ok("a".to_owned())]);
    }

    #[test]
    #[ignore = "drives a private tmux server; run with --ignored"]
    fn swaps_a_live_pane_between_configured_accounts() {
        use std::process::Command;
        let _live = live_tmux();
        let dir = std::env::temp_dir().join(format!("muxa-swap-acct-{}", std::process::id()));
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let claude = bin.join("claude");
        // macOS hides the environment of its own signed binaries, `/bin/sh`
        // included; a Homebrew shell's stays readable, as the real CLI's does.
        let shell = ["/opt/homebrew/bin/bash", "/usr/local/bin/bash"]
            .into_iter()
            .find(|path| cfg!(target_os = "macos") && std::path::Path::new(path).exists())
            .unwrap_or("/bin/sh");
        crate::test_support::write_executable(
            &claude,
            format!(
                "#!{shell}\n{}",
                r#"trap '' INT
echo "FAKE CLAUDE on $CLAUDE_CONFIG_DIR $*"
echo '⏵⏵ bypass permissions on'
while read line; do case "$line" in */exit*) exit 0 ;; esac; echo "GOT $line"; echo '✻ Working… (1s · esc to interrupt)'; done
"#
            ),
            0o755,
        );
        let socket = dir.join("tmux.sock");
        let socket = socket.to_str().unwrap();
        let tmux = |args: &[&str]| {
            let out = Command::new(crate::tmux::tmux_binary())
                .arg("-S")
                .arg(socket)
                .args(args)
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        tmux(&[
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-x",
            "160",
            "-y",
            "40",
            "/bin/sh",
        ]);
        std::env::set_var("MUXA_TMUX_SOCKET", socket);
        let pane = tmux(&["display-message", "-p", "#{pane_id}"]);
        let sid = "6f1d1c0e-0000-4000-8000-00000000a11c";
        let launch = format!(
            "env CLAUDE_CONFIG_DIR=/acct/work {} --dangerously-skip-permissions fix it",
            claude.display()
        );
        tmux(&["send-keys", "-t", &pane, &launch, "Enter"]);
        std::thread::sleep(Duration::from_secs(1));

        let accounts = crate::accounts::AccountsConfig {
            claude: vec![
                account("work", "CLAUDE_CONFIG_DIR", "/acct/work"),
                account("home", "CLAUDE_CONFIG_DIR", "/acct/home"),
            ],
            codex: Vec::new(),
        };
        let mut capped = limited(&pane, sid);
        capped.tmux_socket = None;
        let request = request(SwapScope::Targets(vec![pane.clone()]));
        let backend: SharedBackend = std::sync::Arc::new(crate::backend::tmux::TmuxBackend::new());
        let outcomes = run(
            &request,
            &[capped],
            &[backend],
            &SwapConfig::default(),
            &accounts,
        );
        let screen = tmux(&["capture-pane", "-p", "-t", &pane]);
        tmux(&["kill-server"]);
        let _ = std::fs::remove_dir_all(&dir);

        let outcome = &outcomes[0];
        assert_eq!(outcome.status, SwapStatus::Swapped, "{outcome:?}\n{screen}");
        assert_eq!(outcome.from.as_deref(), Some("work"), "{outcome:?}");
        assert_eq!(outcome.to.as_deref(), Some("home"), "{outcome:?}");
        assert!(
            screen.contains(&format!(
                "FAKE CLAUDE on /acct/home --dangerously-skip-permissions --resume {sid}"
            )),
            "{screen}"
        );
        assert!(screen.contains("GOT continue"), "{screen}");
        assert!(
            outcome.note.contains("picked the turn back up"),
            "{outcome:?}"
        );
    }

    #[test]
    #[ignore = "drives a private tmux server; run with --ignored"]
    fn swaps_a_live_pane_that_runs_its_agent_directly() {
        use std::process::Command;
        let _live = live_tmux();
        let dir = std::env::temp_dir().join(format!("muxa-swap-direct-{}", std::process::id()));
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let script = |name: &str, body: &str| {
            crate::test_support::write_executable(
                &bin.join(name),
                format!("#!/bin/sh\n{body}\n"),
                0o755,
            );
        };
        script(
            "aas",
            r#"trap '' INT; shift 2; "$(dirname "$0")/claude" "$@""#,
        );
        script(
            "claude",
            r#"trap '' INT
echo "FAKE CLAUDE $*"
echo '⏵⏵ bypass permissions on'
while read line; do case "$line" in */exit*) exit 0 ;; esac; echo "GOT $line"; echo '✻ Working… (1s · esc to interrupt)'; done"#,
        );
        let socket = dir.join("tmux.sock");
        let socket = socket.to_str().unwrap();
        let tmux = |args: &[&str]| {
            let out = Command::new(crate::tmux::tmux_binary())
                .arg("-S")
                .arg(socket)
                .args(args)
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        // The pane *is* the agent: no shell behind it to type a relaunch into.
        let launch = format!(
            "{} e e-ed@claude --dangerously-skip-permissions fix it",
            bin.join("aas").display()
        );
        tmux(&[
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-x",
            "160",
            "-y",
            "40",
            &launch,
        ]);
        std::env::set_var("MUXA_TMUX_SOCKET", socket);
        let pane = tmux(&["display-message", "-p", "#{pane_id}"]);
        std::thread::sleep(Duration::from_secs(1));

        let sid = "0c5a1d2e-0000-4000-8000-0000000d17ec";
        let mut capped = limited(&pane, sid);
        capped.tmux_socket = None;
        let mut request = request(SwapScope::Targets(vec![pane.clone()]));
        request.to = AccountChoice::Named("jelly@claude".into());
        let backend: SharedBackend = std::sync::Arc::new(crate::backend::tmux::TmuxBackend::new());
        let outcomes = run(
            &request,
            &[capped],
            &[backend],
            &SwapConfig::default(),
            &crate::accounts::AccountsConfig::default(),
        );
        let screen = tmux(&["capture-pane", "-p", "-t", &pane]);
        let still_there = tmux(&[
            "display-message",
            "-p",
            "-t",
            &pane,
            "#{pane_id} #{pane_dead}",
        ]);
        tmux(&["kill-server"]);
        let _ = std::fs::remove_dir_all(&dir);

        let outcome = &outcomes[0];
        assert_eq!(outcome.status, SwapStatus::Swapped, "{outcome:?}\n{screen}");
        assert_eq!(still_there, format!("{pane} 0"), "same pane, alive");
        assert!(
            screen.contains(&format!(
                "FAKE CLAUDE --dangerously-skip-permissions --resume {sid}"
            )),
            "{screen}"
        );
        assert!(screen.contains("GOT continue"), "{screen}");
    }

    #[test]
    #[ignore = "drives a private tmux server; run with --ignored"]
    fn swaps_a_live_pane_end_to_end() {
        use std::process::Command;
        let _live = live_tmux();
        let dir = std::env::temp_dir().join(format!("muxa-swap-{}", std::process::id()));
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let script = |name: &str, body: &str| {
            crate::test_support::write_executable(
                &bin.join(name),
                format!("#!/bin/sh\n{body}\n"),
                0o755,
            );
        };
        // `aas e <name> args…` runs the agent as its child, like the real one.
        script(
            "aas",
            r#"trap '' INT; shift 2; "$(dirname "$0")/claude" "$@""#,
        );
        // Like the real CLI: Ctrl-C does not quit it, `/exit` does, and a
        // resume on a fresh profile first asks to trust the folder, where
        // Enter alone means "No, exit".
        script(
            "claude",
            r#"trap '' INT
echo "FAKE CLAUDE $*"
case "$*" in *--resume*)
  printf ' ❯ No, exit\n   Yes, I trust this folder\n \033[38;5;246mEnter\033[39m \033[38;5;246mto\033[39m \033[38;5;246mconfirm\033[39m · Esc to cancel\n'
  read answer
  case "$answer" in *'[B'*) echo TRUSTED ;; *) echo REFUSED; exit 1 ;; esac ;;
esac
echo '⏵⏵ bypass permissions on'
while read line; do case "$line" in */exit*) exit 0 ;; esac; echo "GOT $line"; echo '✻ Working… (1s · esc to interrupt)'; done"#,
        );
        let socket = dir.join("tmux.sock");
        let socket = socket.to_str().unwrap();
        let tmux = |args: &[&str]| {
            let out = Command::new(crate::tmux::tmux_binary())
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
            .arg(crate::tmux::tmux_binary())
            .args(["-S", socket, "-f", "/dev/null", "new-session", "-d"])
            .args(["-x", "160", "-y", "40", "/bin/sh"])
            .status()
            .unwrap();
        std::env::set_var("MUXA_TMUX_SOCKET", socket);
        let pane = tmux(&["display-message", "-p", "#{pane_id}"]);
        let sid = "5a95b526-0439-4938-adb8-a24055198731";
        // By absolute path, so a real `aas` earlier on the pane shell's PATH
        // can never run instead of the fixture.
        let launch = format!(
            "{} e e-ed@claude --dangerously-skip-permissions fix it",
            bin.join("aas").display()
        );
        tmux(&["send-keys", "-t", &pane, &launch, "Enter"]);
        std::thread::sleep(Duration::from_secs(1));

        let mut capped = limited(&pane, sid);
        capped.tmux_socket = None;
        let mut request = request(SwapScope::Targets(vec![pane.clone()]));
        request.to = AccountChoice::Named("jelly@claude".into());
        let backend: SharedBackend = std::sync::Arc::new(crate::backend::tmux::TmuxBackend::new());
        let outcomes = run(
            &request,
            &[capped],
            &[backend],
            &SwapConfig::default(),
            &crate::accounts::AccountsConfig::default(),
        );
        let screen = tmux(&["capture-pane", "-p", "-t", &pane]);
        tmux(&["kill-server"]);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(outcomes.len(), 1, "{outcomes:?}");
        let outcome = &outcomes[0];
        assert_eq!(outcome.status, SwapStatus::Swapped, "{outcome:?}\n{screen}");
        assert!(screen.contains("TRUSTED"), "{screen}");
        assert!(outcome.note.contains("trusted the folder"), "{outcome:?}");
        assert!(screen.contains("GOT continue"), "{screen}");
        assert!(
            outcome.note.contains("picked the turn back up"),
            "{outcome:?}"
        );
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
        // Anything but `candidates` exits at once, so the probe below is free.
        std::fs::write(
            &path,
            format!("#!/bin/sh\n[ \"$1\" = candidates ] || exit 0\n{body}\n"),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A test thread forking while the file was open for writing leaves
        // it busy (ETXTBSY) until that child execs; wait that out here so
        // the query under test never sees it.
        for _ in 0..100 {
            match std::process::Command::new(&path).arg("probe").status() {
                Err(error) if error.raw_os_error() == Some(26) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => break,
            }
        }
        path
    }

    #[cfg(unix)]
    #[test]
    fn the_query_deadline_covers_a_helper_holding_the_pipes() {
        let aas = fake_aas("helper", "sleep 20 &\nprintf '{\"accounts\":[]}'\nexit 0");
        let started = Instant::now();
        let outcome =
            query_candidates_within(aas.to_str().unwrap(), "claude", Duration::from_millis(300));
        // Far under the helper's 20s, with room for a loaded test machine.
        assert!(
            started.elapsed() < Duration::from_secs(10),
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
        assert!(started.elapsed() < Duration::from_secs(10));
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
