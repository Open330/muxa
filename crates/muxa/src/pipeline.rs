//! Work pipelines — a declared line-up of agents, reconciled onto tmux.
//!
//! `muxa work start` creates one agent pane at a time, which is the right
//! primitive and the wrong ergonomics for a Work whose external issue wants a planner, an
//! implementer, and a reviewer sitting in the same window. This module is
//! the layer above it: a work id in, a *desired set of panes* out, compared
//! against the panes that already exist.
//!
//! Three deliberate choices shape it.
//!
//! **External issue lookup is delegated to an agent.** The config and Rust
//! type retain the historical `ticket` name for compatibility. Muxa never learns Linear's
//! GraphQL or Jira's REST. It spends one headless turn asking an agent CLI
//! to fetch the ticket, because the user already taught that agent how —
//! through a skill, an MCP server, `gh`, a token in the environment. Adding
//! a provider is a prompt in `[ticket.source]`, not a muxa release. See
//! [`crate::ask::one_shot`].
//!
//! **The plan is a diff, not a script.** [`plan`] answers "what is missing"
//! rather than "what to run", so `muxa work up` on an already-staffed
//! window is a no-op instead of a second copy of every agent. Panes are
//! keyed by [`PipelineAgentConfig::alias`](crate::config::PipelineAgentConfig::alias),
//! stored on the pane itself, so the key survives the daemon, the CLI
//! process, and the agent restarting.
//!
//! **Nothing existing is ever mutated implicitly.** A pane the plan did not
//! ask for is reported in [`Plan::unclaimed`] and left alone. Reconciling
//! toward a desired state is useful; reconciling *away* from a pane a human
//! opened is how orchestration earns distrust.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::{
    Config, PipelineAgentConfig, PipelineConfig, RouteConfig, TicketConfig, TicketSource,
    WorktreeConfig,
};
use crate::event::AgentState;

/// Ticket body characters kept in a rendered prompt. A description can run
/// to thousands of words; the prompt carries the shape of the task and the
/// URL, and the agent can read the rest itself.
pub const MAX_BODY_CHARS: usize = 4000;

/// Placeholder key holding the caller's composed request — the skill,
/// body, and context that `muxa work up` and `muxa_call_peer` both accept
/// (see [`crate::request`]).
pub const REQUEST_KEY: &str = "request";

#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error("no [[route]] matches work id {0:?}; add a catch-all route with match = '.*'")]
    NoRoute(String),
    #[error(
        "route for {0:?} names no pipeline; set `pipeline = \"...\"` on the route or pass --pipeline"
    )]
    NoPipeline(String),
    #[error("pipeline {0:?} is not defined; add a [pipeline.{0}] section")]
    UnknownPipeline(String),
    #[error("pipeline {0:?} declares no agents; add a [[pipeline.{0}.agent]] entry")]
    EmptyPipeline(String),
    #[error("pipeline {pipeline:?} uses alias {alias:?} twice; aliases key the pane diff and must be unique")]
    DuplicateAlias { pipeline: String, alias: String },
    #[error("{scope} pattern {pattern:?} is not a valid regex: {source}")]
    BadPattern {
        scope: &'static str,
        pattern: String,
        source: regex::Error,
    },
    #[error("the ticket resolver answered without a JSON object; its reply began: {0}")]
    NoTicketJson(String),
    #[error("the ticket resolver answered with JSON that is not a ticket: {0}")]
    BadTicketJson(String),
    #[error("the reply carried no TOML; it began: {0}")]
    NoToml(String),
    #[error("the generated TOML does not parse: {0}")]
    BadToml(String),
    #[error("the generated config configures nothing: expected at least one [[route]] and one [pipeline.*]")]
    EmptyProposal,
    #[error("pipeline {pipeline:?} agent {alias:?} names program {program:?}, which is not an allowlisted agent CLI (claude, codex, gemini, opencode)")]
    UnknownProgram {
        pipeline: String,
        alias: String,
        program: String,
    },
}

/// Ticket context, as the resolver agent reported it.
///
/// Every field past `id` is optional on purpose: a resolver that only
/// manages a title is more useful than one that fails closed, and a work
/// window is perfectly launchable on the id alone.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ticket {
    /// Resolver source key (`linear`, `github`, `jira`, ...). This is set by
    /// muxa after source selection, not trusted from the resolver response.
    #[serde(default)]
    pub source: Option<String>,
    /// Provider scope such as a Linear workspace/team or GitHub owner/repo.
    #[serde(default)]
    pub scope: Option<String>,
    /// Provider-internal immutable identity when returned by the resolver.
    #[serde(default)]
    pub stable_id: Option<String>,
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    /// Branch the ticket asks to be worked on, when the tracker suggests
    /// one. Used as the worktree branch before falling back to the id.
    #[serde(default)]
    pub branch: Option<String>,
}

impl Ticket {
    /// Parse the resolver's reply.
    ///
    /// Agent CLIs answer in prose around their JSON as often as not — a
    /// fenced block, a sentence of preamble, a trailing "let me know if
    /// you need more". So the object is *extracted* rather than parsed
    /// from position zero, and common field spellings are accepted, since
    /// the prompt asking for `body` will sometimes come back as
    /// `description`.
    ///
    /// # Errors
    /// [`PipelineError::NoTicketJson`] when the reply carries no balanced
    /// JSON object, [`PipelineError::BadTicketJson`] when it does but the
    /// object is not one.
    pub fn parse_reply(id: &str, reply: &str) -> Result<Self, PipelineError> {
        let json = extract_json_object(reply).ok_or_else(|| {
            PipelineError::NoTicketJson(truncate_chars(reply.trim(), 160).into_owned())
        })?;
        let value: serde_json::Value = serde_json::from_str(json)
            .map_err(|error| PipelineError::BadTicketJson(error.to_string()))?;
        let object = value
            .as_object()
            .ok_or_else(|| PipelineError::BadTicketJson("top level is not an object".into()))?;
        let pick = |keys: &[&str]| -> Option<String> {
            keys.iter().find_map(|key| {
                object
                    .get(*key)
                    .and_then(|value| match value {
                        serde_json::Value::String(text) => Some(text.clone()),
                        serde_json::Value::Number(number) => Some(number.to_string()),
                        // `state` commonly arrives as `{"name": "In Progress"}`.
                        serde_json::Value::Object(nested) => nested
                            .get("name")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string),
                        _ => None,
                    })
                    .map(|text| text.trim().to_string())
                    .filter(|text| !text.is_empty())
            })
        };
        let display_id = pick(&["identifier", "key", "number", "id"])
            .filter(|value| !looks_like_uuid(value))
            .unwrap_or_else(|| id.to_string());
        let stable_id = pick(&["id", "uuid", "node_id"]).filter(|value| value != &display_id);
        Ok(Self {
            source: None,
            scope: pick(&["scope", "repository", "repo", "team"]),
            stable_id,
            // `identifier`/`key` before `id`: trackers carry both a human
            // ticket id and an internal UUID, and Linear puts the UUID under
            // the more obvious name. Getting this backwards is invisible in
            // a unit test and glaring in a window title.
            id: display_id,
            title: pick(&["title", "name", "summary"]),
            body: pick(&["body", "description", "content"]),
            url: pick(&["url", "html_url", "link", "permalink"]),
            state: pick(&["state", "status"]),
            branch: pick(&["branch", "branchName", "branch_name"]),
        })
    }
}

/// Find the last balanced JSON object in a block of text.
///
/// Last rather than first: a resolver that explains its plan before
/// answering tends to put the answer at the end, and a prompt that shows
/// the expected shape gets that shape echoed back before the real one.
/// String literals are tracked so a brace inside a title cannot unbalance
/// the scan.
#[must_use]
pub fn extract_json_object(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let mut best: Option<(usize, usize)> = None;
    let mut starts: Vec<usize> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for (index, byte) in bytes.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => starts.push(index),
            b'}' => {
                if let Some(start) = starts.pop() {
                    if starts.is_empty() {
                        best = Some((start, index + 1));
                    }
                }
            }
            _ => {}
        }
    }
    let (start, end) = best?;
    text.get(start..end)
}

/// Agent CLIs a pipeline may name. Kept here so a generated config can be
/// rejected before it reaches tmux, rather than failing one pane at a time.
pub const ALLOWLISTED_PROGRAMS: [&str; 4] = ["claude", "codex", "gemini", "opencode"];

/// Pull a TOML document out of an agent's reply.
///
/// Prefers a fenced `toml` code block, because that is what the prompt asks
/// for and what models reliably produce. Falls back to the whole reply so
/// a model that answered with bare TOML still works.
#[must_use]
pub fn extract_toml_block(reply: &str) -> Option<&str> {
    let mut rest = reply;
    while let Some(open) = rest.find("```") {
        let after = &rest[open + 3..];
        let (tag, body) = after.split_once('\n')?;
        let tag = tag.trim();
        let Some(close) = body.find("```") else {
            // Unterminated fence: take what is there rather than nothing.
            return (tag.is_empty() || tag.eq_ignore_ascii_case("toml"))
                .then_some(body)
                .filter(|body| !body.trim().is_empty());
        };
        if tag.is_empty() || tag.eq_ignore_ascii_case("toml") {
            let block = &body[..close];
            if !block.trim().is_empty() {
                return Some(block);
            }
        }
        rest = &body[close + 3..];
    }
    (!reply.trim().is_empty()).then_some(reply)
}

/// What a generated config actually configures, for the confirmation the
/// operator sees before anything is written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProposalSummary {
    pub ticket_sources: Vec<String>,
    pub routes: Vec<String>,
    /// `name → agent count`, in the order a pipeline's agents are declared.
    pub pipelines: Vec<(String, usize)>,
}

/// Parse and check a generated `[ticket]`/`[[route]]`/`[pipeline.*]` block
/// before it is allowed anywhere near the operator's config file.
///
/// A model that invents a key, a bad regex, or a pipeline nobody defined
/// would otherwise take the daemon down at its next start —
/// [`crate::config::Config`] denies unknown fields, so a typo is a parse
/// error rather than a shrug. Better to refuse here, while the text is
/// still just text.
///
/// # Errors
/// [`PipelineError::BadToml`] when the text is not a valid config,
/// [`PipelineError::EmptyProposal`] when it configures nothing,
/// [`PipelineError::BadPattern`] for a regex that will not compile,
/// [`PipelineError::UnknownPipeline`] for a route naming a pipeline that
/// does not exist, [`PipelineError::EmptyPipeline`] /
/// [`PipelineError::DuplicateAlias`] / [`PipelineError::UnknownProgram`]
/// for a line-up that could never launch.
pub fn validate_proposal(toml_text: &str) -> Result<(Config, ProposalSummary), PipelineError> {
    let config: Config =
        toml::from_str(toml_text).map_err(|error| PipelineError::BadToml(error.to_string()))?;
    if config.route.is_empty() || config.pipeline.is_empty() {
        return Err(PipelineError::EmptyProposal);
    }
    for source in config.ticket.source.values() {
        if !source.pattern.is_empty() {
            compile("ticket source", &source.pattern)?;
        }
    }
    for route in &config.route {
        if !route.pattern.is_empty() {
            compile("route", &route.pattern)?;
        }
        if let Some(name) = route.pipeline.as_deref() {
            if !config.pipeline.contains_key(name) {
                return Err(PipelineError::UnknownPipeline(name.to_string()));
            }
        }
    }
    for (name, pipeline) in &config.pipeline {
        // Rendering with empty vars is the cheapest way to reuse the same
        // emptiness and duplicate-alias rules the launcher enforces.
        desired_agents(name, pipeline, &Vars::new())?;
        for agent in &pipeline.agent {
            let program = agent.program.trim().to_ascii_lowercase();
            if !ALLOWLISTED_PROGRAMS.contains(&program.as_str()) {
                return Err(PipelineError::UnknownProgram {
                    pipeline: name.clone(),
                    alias: agent.alias.clone(),
                    program: agent.program.clone(),
                });
            }
        }
    }
    let summary = ProposalSummary {
        ticket_sources: config.ticket.source.keys().cloned().collect(),
        routes: config
            .route
            .iter()
            .map(|route| {
                format!(
                    "{} → {}",
                    route.pattern,
                    route.pipeline.as_deref().unwrap_or("(no pipeline)")
                )
            })
            .collect(),
        pipelines: config
            .pipeline
            .iter()
            .map(|(name, pipeline)| (name.clone(), pipeline.agent.len()))
            .collect(),
    };
    Ok((config, summary))
}

fn compile(scope: &'static str, pattern: &str) -> Result<(), PipelineError> {
    regex::RegexBuilder::new(pattern)
        .case_insensitive(true)
        .build()
        .map(|_| ())
        .map_err(|source| PipelineError::BadPattern {
            scope,
            pattern: pattern.to_string(),
            source,
        })
}

/// Placeholder values available to `[ticket.source]` prompts, `[[route]]`
/// paths, and pipeline prompts.
///
/// Rendering only substitutes keys that exist here and leaves every other
/// `{{...}}` untouched. That is what lets a resolver prompt contain the
/// literal JSON shape it is asking for without the renderer chewing on it,
/// and it makes a typo show up in the prompt instead of silently blanking.
#[derive(Debug, Clone, Default)]
pub struct Vars(BTreeMap<String, String>);

impl Vars {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn set(mut self, key: &str, value: impl Into<String>) -> Self {
        self.0.insert(key.to_string(), value.into());
        self
    }

    pub fn set_opt(&mut self, key: &str, value: Option<&str>) {
        if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
            self.0.insert(key.to_string(), value.to_string());
        }
    }

    /// Add `ticket.*` keys, truncating the body so a long description
    /// cannot dominate a launch prompt.
    #[must_use]
    pub fn with_ticket(mut self, ticket: &Ticket) -> Self {
        self.0.insert("ticket.id".into(), ticket.id.clone());
        self.set_opt("ticket.title", ticket.title.as_deref());
        self.set_opt("ticket.url", ticket.url.as_deref());
        self.set_opt("ticket.state", ticket.state.as_deref());
        self.set_opt("ticket.branch", ticket.branch.as_deref());
        if let Some(body) = ticket.body.as_deref() {
            self.0.insert(
                "ticket.body".into(),
                truncate_chars(body, MAX_BODY_CHARS).into_owned(),
            );
        }
        self
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    /// Substitute every `{{key}}` this map knows about.
    #[must_use]
    pub fn render(&self, template: &str) -> String {
        let mut out = String::with_capacity(template.len());
        let mut rest = template;
        while let Some(open) = rest.find("{{") {
            let (before, after) = rest.split_at(open);
            out.push_str(before);
            let Some(close) = after.find("}}") else {
                out.push_str(after);
                return out;
            };
            let key = after[2..close].trim();
            match self.0.get(key) {
                Some(value) => out.push_str(value),
                // Unknown keys survive verbatim — see the type docs.
                None => out.push_str(&after[..close + 2]),
            }
            rest = &after[close + 2..];
        }
        out.push_str(rest);
        out
    }
}

/// A bare RFC 4122-shaped id, which is a database key rather than
/// something a person would ever type or a window would ever be named.
/// Preferring `identifier` is not enough on its own: a source that reports
/// only `id` would still put a UUID everywhere a ticket id belongs.
fn looks_like_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

/// Whether a template places `{{request}}` itself, tolerating inner
/// whitespace exactly as [`Vars::render`] does.
fn places_request(template: &str) -> bool {
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        let after = &rest[open..];
        let Some(close) = after.find("}}") else {
            return false;
        };
        if after[2..close].trim() == REQUEST_KEY {
            return true;
        }
        rest = &after[close + 2..];
    }
    false
}

/// Truncate on a character boundary, marking the cut so a reader (human or
/// agent) can tell the text was clipped rather than ended.
#[must_use]
pub fn truncate_chars(text: &str, limit: usize) -> std::borrow::Cow<'_, str> {
    if text.chars().count() <= limit {
        return std::borrow::Cow::Borrowed(text);
    }
    let kept: String = text.chars().take(limit).collect();
    std::borrow::Cow::Owned(format!("{kept}\n…[truncated]"))
}

fn matches(scope: &'static str, pattern: &str, value: &str) -> Result<bool, PipelineError> {
    regex::RegexBuilder::new(pattern)
        .case_insensitive(true)
        .build()
        .map_err(|source| PipelineError::BadPattern {
            scope,
            pattern: pattern.to_string(),
            source,
        })
        .map(|regex| regex.is_match(value))
}

/// First `[ticket.source]` whose pattern accepts this work id, in
/// sorted-key order so the choice does not depend on TOML ordering.
///
/// # Errors
/// [`PipelineError::BadPattern`] if a source's `match` is not a regex.
pub fn select_source<'a>(
    config: &'a TicketConfig,
    work: &str,
) -> Result<Option<(&'a str, &'a TicketSource)>, PipelineError> {
    for (name, source) in &config.source {
        if !source.pattern.is_empty() && matches("ticket source", &source.pattern, work)? {
            return Ok(Some((name.as_str(), source)));
        }
    }
    Ok(None)
}

/// First `[[route]]` whose pattern accepts this work id, in declaration
/// order — routes are an ordered list precisely so a catch-all can sit at
/// the bottom.
///
/// # Errors
/// [`PipelineError::BadPattern`] if a route's `match` is not a regex.
pub fn select_route<'a>(
    routes: &'a [RouteConfig],
    work: &str,
) -> Result<Option<&'a RouteConfig>, PipelineError> {
    for route in routes {
        if !route.pattern.is_empty() && matches("route", &route.pattern, work)? {
            return Ok(Some(route));
        }
    }
    Ok(None)
}

/// Where a work item's worktree goes, with the route's defaults filled in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorktreePlan {
    pub repo: std::path::PathBuf,
    pub path: std::path::PathBuf,
    pub branch: String,
    pub base: Option<String>,
}

/// Resolve `[route.worktree]` into concrete paths.
///
/// The default path sits *beside* the repo rather than inside it: a
/// worktree under the repo shows up in the parent's status, its own
/// ignore rules, and every `find` the agents run.
#[must_use]
pub fn worktree_plan(
    config: &WorktreeConfig,
    vars: &Vars,
    expand: &dyn Fn(&str) -> String,
) -> WorktreePlan {
    let repo = std::path::PathBuf::from(expand(&vars.render(&config.repo)));
    // Without an explicit branch, take the one the tracker suggests before
    // falling back to the work id — a tracker that names a branch is naming
    // the branch the team will actually push.
    let branch = match config.branch.as_deref() {
        Some(branch) => vars.render(branch),
        None => vars
            .get("ticket.branch")
            .or_else(|| vars.get("id"))
            .unwrap_or_default()
            .to_string(),
    };
    let path = match config.path.as_deref() {
        Some(path) => std::path::PathBuf::from(expand(&vars.render(path))),
        None => default_worktree_path(&repo, vars.get("id").unwrap_or("work")),
    };
    WorktreePlan {
        repo,
        path,
        branch,
        base: config.base.as_deref().map(|base| vars.render(base)),
    }
}

fn default_worktree_path(repo: &Path, work: &str) -> std::path::PathBuf {
    let name = repo.file_name().map_or_else(
        || "repo".to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    repo.parent()
        .unwrap_or(repo)
        .join(format!("{name}-worktrees"))
        .join(work)
}

/// One pane the pipeline wants, with every template already rendered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DesiredAgent {
    pub alias: String,
    pub program: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direction: Option<String>,
}

/// Render a pipeline into the panes it asks for.
///
/// # Errors
/// [`PipelineError::EmptyPipeline`] for a pipeline with no agents, and
/// [`PipelineError::DuplicateAlias`] when two agents share the alias the
/// diff keys on.
pub fn desired_agents(
    name: &str,
    pipeline: &PipelineConfig,
    vars: &Vars,
) -> Result<Vec<DesiredAgent>, PipelineError> {
    if pipeline.agent.is_empty() {
        return Err(PipelineError::EmptyPipeline(name.to_string()));
    }
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut agents = Vec::with_capacity(pipeline.agent.len());
    for agent in &pipeline.agent {
        let alias = agent.alias.trim().to_ascii_lowercase();
        if !seen.insert(alias.clone()) {
            return Err(PipelineError::DuplicateAlias {
                pipeline: name.to_string(),
                alias,
            });
        }
        agents.push(render_agent(
            &alias,
            agent,
            pipeline.prompt.as_deref(),
            vars,
        ));
    }
    Ok(agents)
}

fn render_agent(
    alias: &str,
    agent: &PipelineAgentConfig,
    shared_prompt: Option<&str>,
    vars: &Vars,
) -> DesiredAgent {
    let mut vars = vars.clone();
    vars.set_opt("alias", Some(alias));
    vars.set_opt("role", agent.role.as_deref());
    vars.set_opt("program", Some(&agent.program));
    // Three layers, outermost first: what the caller asked for, what the
    // pipeline says every agent needs, and what this agent's job is.
    let templates: Vec<&str> = [shared_prompt, agent.prompt.as_deref()]
        .into_iter()
        .flatten()
        .collect();
    let mut sections = Vec::new();
    // The request leads unless a template placed `{{request}}` itself. A
    // pipeline written before the caller ever passed a body must not
    // silently swallow one, and a pipeline that wants it somewhere else
    // says so.
    if !templates.iter().any(|template| places_request(template)) {
        if let Some(request) = vars.get(REQUEST_KEY) {
            sections.push(request.to_string());
        }
    }
    for template in templates {
        let rendered = vars.render(template).trim().to_string();
        if !rendered.is_empty() {
            sections.push(rendered);
        }
    }
    let prompt = (!sections.is_empty()).then(|| sections.join("\n\n"));
    DesiredAgent {
        alias: alias.to_string(),
        program: agent.program.trim().to_ascii_lowercase(),
        role: agent.role.as_deref().map(|role| vars.render(role)),
        task: agent.task.as_deref().map(|task| vars.render(task)),
        prompt,
        direction: agent.direction.clone(),
    }
}

/// A pane that already exists in the work window.
///
/// `state` is what makes a re-run a reconcile rather than a headcount. A
/// pane proves someone was started there; it says nothing about whether
/// that agent is working, idle, or stuck on a permission prompt nobody
/// will ever answer. Those look identical from tmux and could not be
/// further apart in what they need.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExistingAgent {
    pub pane: String,
    pub program: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// From the daemon's registry. `None` means the daemon had nothing for
    /// this pane — usually that it is not running.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<AgentState>,
}

/// Whether this agent is blocked on a person.
///
/// A prompt typed at an agent sitting on an approval dialog is swallowed
/// by the dialog, so muxa must report it rather than talk over it.
#[must_use]
pub fn needs_a_human(state: Option<AgentState>) -> bool {
    matches!(
        state,
        Some(AgentState::WaitingInput | AgentState::WaitingChoice | AgentState::Error)
    )
}

/// What `muxa work up` intends to do to one pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PlanStep {
    /// The alias has no pane yet.
    Launch(DesiredAgent),
    /// The alias has a live pane and a message to deliver to it.
    Reprompt {
        alias: String,
        pane: String,
        prompt: String,
    },
    /// The alias has a live pane and nothing to say to it. Converged.
    Keep {
        alias: String,
        pane: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        state: Option<AgentState>,
    },
    /// The alias is blocked on a person — an approval prompt or an error.
    /// Never prompted over: the dialog would eat the message, and the
    /// operator would be left believing it was delivered.
    Attention {
        alias: String,
        pane: String,
        state: Option<AgentState>,
    },
}

impl PlanStep {
    #[must_use]
    pub fn alias(&self) -> &str {
        match self {
            Self::Launch(agent) => &agent.alias,
            Self::Reprompt { alias, .. }
            | Self::Keep { alias, .. }
            | Self::Attention { alias, .. } => alias,
        }
    }
}

/// The full desired-vs-actual comparison for one work window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Plan {
    pub steps: Vec<PlanStep>,
    /// Panes in the window that no pipeline alias claims — a manually
    /// started agent, or one left by a pipeline that has since changed.
    /// Reported so the operator can see them; never touched.
    pub unclaimed: Vec<ExistingAgent>,
}

impl Plan {
    #[must_use]
    pub fn launches(&self) -> usize {
        self.steps
            .iter()
            .filter(|step| matches!(step, PlanStep::Launch(_)))
            .count()
    }

    #[must_use]
    pub fn reprompts(&self) -> usize {
        self.steps
            .iter()
            .filter(|step| matches!(step, PlanStep::Reprompt { .. }))
            .count()
    }

    /// True when every desired pane already exists and there is nothing to
    /// say to any of them.
    /// Aliases blocked on a person. Converging is not possible until
    /// somebody answers them, so they are counted separately.
    #[must_use]
    pub fn attention(&self) -> usize {
        self.steps
            .iter()
            .filter(|step| matches!(step, PlanStep::Attention { .. }))
            .count()
    }

    #[must_use]
    pub fn converged(&self) -> bool {
        self.launches() == 0 && self.reprompts() == 0 && self.attention() == 0
    }
}

/// Compare the pipeline's panes against the window's panes.
///
/// `broadcast` is the "improve work already in flight" path: with it, every
/// alias that *already* has a pane is sent the message instead of being
/// left alone. Without it a re-run only fills gaps, because injecting a
/// prompt into an agent that is mid-turn is disruptive enough to deserve
/// an explicit ask.
#[must_use]
pub fn plan(desired: &[DesiredAgent], existing: &[ExistingAgent], broadcast: Option<&str>) -> Plan {
    let steps = desired
        .iter()
        .map(|agent| {
            let live = existing.iter().find(|candidate| {
                candidate
                    .alias
                    .as_deref()
                    .is_some_and(|alias| alias.eq_ignore_ascii_case(&agent.alias))
            });
            match (live, broadcast) {
                (None, _) => PlanStep::Launch(agent.clone()),
                // Blocked on a person beats everything: do not type at it,
                // and do not report it as converged either.
                (Some(live), _) if needs_a_human(live.state) => PlanStep::Attention {
                    alias: agent.alias.clone(),
                    pane: live.pane.clone(),
                    state: live.state,
                },
                (Some(live), Some(message)) => PlanStep::Reprompt {
                    alias: agent.alias.clone(),
                    pane: live.pane.clone(),
                    prompt: message.to_string(),
                },
                (Some(live), None) => PlanStep::Keep {
                    alias: agent.alias.clone(),
                    pane: live.pane.clone(),
                    state: live.state,
                },
            }
        })
        .collect();
    let unclaimed = existing
        .iter()
        .filter(|candidate| {
            candidate.alias.as_deref().is_none_or(|alias| {
                !desired
                    .iter()
                    .any(|agent| agent.alias.eq_ignore_ascii_case(alias))
            })
        })
        .cloned()
        .collect();
    Plan { steps, unclaimed }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn pipeline_config() -> Config {
        toml::from_str(
            r"
[[route]]
match = '^cal-'
workspace = 'callabo'
pipeline = 'triad'

[[route]]
match = '.*'
pipeline = 'solo'

[pipeline.triad]
layout = 'main-vertical'
prompt = '{{ticket.title}}'

[[pipeline.triad.agent]]
alias = 'plan'
program = 'codex'
role = 'planner'
prompt = 'Plan {{id}}.'

[[pipeline.triad.agent]]
alias = 'impl'
program = 'codex'
role = 'implementer'

[[pipeline.triad.agent]]
alias = 'review'
program = 'claude'
role = 'reviewer'
",
        )
        .expect("pipeline config parses")
    }

    #[test]
    fn routes_match_in_declaration_order_so_a_catch_all_can_sit_last() {
        let config = pipeline_config();
        let cal = select_route(&config.route, "cal-1234").unwrap().unwrap();
        assert_eq!(cal.workspace.as_deref(), Some("callabo"));
        assert_eq!(cal.pipeline.as_deref(), Some("triad"));
        let other = select_route(&config.route, "PROJ-7").unwrap().unwrap();
        assert_eq!(other.pipeline.as_deref(), Some("solo"));
    }

    #[test]
    fn work_ids_route_case_insensitively() {
        let config = pipeline_config();
        let route = select_route(&config.route, "CAL-1234").unwrap().unwrap();
        assert_eq!(route.workspace.as_deref(), Some("callabo"));
    }

    #[test]
    fn a_bad_pattern_names_itself_instead_of_silently_not_matching() {
        let routes = vec![RouteConfig {
            pattern: "cal-[".into(),
            ..RouteConfig::default()
        }];
        let error = select_route(&routes, "cal-1").unwrap_err().to_string();
        assert!(error.contains("route pattern"), "{error}");
        assert!(error.contains("cal-["), "{error}");
    }

    #[test]
    fn rendering_substitutes_known_keys_and_leaves_the_rest_alone() {
        let vars = Vars::new()
            .set("id", "cal-1234")
            .set("workspace", "callabo");
        assert_eq!(vars.render("{{workspace}}/{{id}}"), "callabo/cal-1234");
        // A resolver prompt shows the JSON shape it wants back. Neither the
        // single braces nor an unknown key may be eaten.
        assert_eq!(
            vars.render(r#"Fetch {{id}} and answer {"id":"...","title":"..."} only. {{nope}}"#),
            r#"Fetch cal-1234 and answer {"id":"...","title":"..."} only. {{nope}}"#
        );
    }

    #[test]
    fn an_unterminated_placeholder_is_left_verbatim() {
        let vars = Vars::new().set("id", "cal-1");
        assert_eq!(vars.render("{{id}} and {{oops"), "cal-1 and {{oops");
    }

    #[test]
    fn a_long_ticket_body_is_clipped_and_says_so() {
        let ticket = Ticket {
            id: "cal-1".into(),
            body: Some("x".repeat(MAX_BODY_CHARS + 50)),
            ..Ticket::default()
        };
        let vars = Vars::new().with_ticket(&ticket);
        let body = vars.get("ticket.body").expect("body is set");
        assert!(body.ends_with("…[truncated]"), "{body}");
        assert!(body.chars().count() < MAX_BODY_CHARS + 50);
    }

    #[test]
    fn the_ticket_json_is_found_after_prose_and_a_fence() {
        let reply = "Sure! Here is the ticket:\n```json\n{\"id\": \"CAL-1234\", \"title\": \"Fix the reaper\", \"state\": {\"name\": \"In Progress\"}}\n```\nLet me know if you need more.";
        let ticket = Ticket::parse_reply("cal-1234", reply).expect("parses");
        assert_eq!(ticket.id, "CAL-1234");
        assert_eq!(ticket.title.as_deref(), Some("Fix the reaper"));
        assert_eq!(ticket.state.as_deref(), Some("In Progress"));
    }

    #[test]
    fn the_last_object_wins_so_an_echoed_example_does_not() {
        let reply = r#"You asked for {"id":"...","title":"..."}. Here it is:
{"id":"CAL-9","title":"Real one"}"#;
        let ticket = Ticket::parse_reply("cal-9", reply).expect("parses");
        assert_eq!(ticket.title.as_deref(), Some("Real one"));
    }

    #[test]
    fn a_brace_inside_a_title_does_not_unbalance_the_scan() {
        let reply = r#"{"id":"CAL-3","title":"fix {{id}} rendering"}"#;
        let ticket = Ticket::parse_reply("cal-3", reply).expect("parses");
        assert_eq!(ticket.title.as_deref(), Some("fix {{id}} rendering"));
    }

    #[test]
    fn alternate_field_spellings_are_accepted() {
        let reply =
            r#"{"identifier":"CAL-4","name":"Title","description":"Body","html_url":"http://x"}"#;
        let ticket = Ticket::parse_reply("cal-4", reply).expect("parses");
        assert_eq!(ticket.id, "CAL-4");
        assert_eq!(ticket.title.as_deref(), Some("Title"));
        assert_eq!(ticket.body.as_deref(), Some("Body"));
        assert_eq!(ticket.url.as_deref(), Some("http://x"));
    }

    #[test]
    fn the_human_ticket_id_wins_over_the_trackers_internal_uuid() {
        // Verbatim shape from `linear-issue.sh json CAL-7093`.
        let reply = r#"{"id":"3f594ef7-8e1e-4586-ae8f-a1fda4a57f0c","identifier":"CAL-7093","title":"마이크 목록 로드 실패","url":"https://linear.app/rtzr/issue/CAL-7093"}"#;
        let ticket = Ticket::parse_reply("cal-7093", reply).expect("parses");
        assert_eq!(ticket.id, "CAL-7093");
    }

    #[test]
    fn a_uuid_only_reply_falls_back_to_the_id_we_asked_about() {
        let reply = r#"{"id":"3f594ef7-8e1e-4586-ae8f-a1fda4a57f0c","title":"T"}"#;
        let ticket = Ticket::parse_reply("cal-7093", reply).expect("parses");
        assert_eq!(ticket.id, "cal-7093");
    }

    #[test]
    fn uuid_detection_does_not_catch_ordinary_ticket_ids() {
        assert!(looks_like_uuid("3f594ef7-8e1e-4586-ae8f-a1fda4a57f0c"));
        assert!(!looks_like_uuid("CAL-7093"));
        assert!(!looks_like_uuid("3f594ef7-8e1e-4586-ae8f-a1fda4a57f0"));
        assert!(!looks_like_uuid("zzzzzzzz-8e1e-4586-ae8f-a1fda4a57f0c"));
    }

    #[test]
    fn a_reply_without_json_reports_what_it_said_instead() {
        let error = Ticket::parse_reply("cal-5", "I could not find that ticket.")
            .unwrap_err()
            .to_string();
        assert!(error.contains("I could not find that ticket."), "{error}");
    }

    #[test]
    fn the_shared_prompt_leads_each_agents_own() {
        let config = pipeline_config();
        let vars = Vars::new().set("id", "cal-1234").with_ticket(&Ticket {
            id: "CAL-1234".into(),
            title: Some("Fix the reaper".into()),
            ..Ticket::default()
        });
        let agents = desired_agents("triad", &config.pipeline["triad"], &vars).unwrap();
        assert_eq!(agents.len(), 3);
        assert_eq!(
            agents[0].prompt.as_deref(),
            Some("Fix the reaper\n\nPlan cal-1234.")
        );
        // An agent with no prompt of its own still gets the shared context.
        assert_eq!(agents[1].prompt.as_deref(), Some("Fix the reaper"));
        assert_eq!(agents[2].role.as_deref(), Some("reviewer"));
    }

    #[test]
    fn the_request_leads_when_no_template_places_it() {
        let config = pipeline_config();
        let vars = Vars::new()
            .set("id", "cal-1234")
            .set(REQUEST_KEY, "Fix the double reap.")
            .with_ticket(&Ticket {
                id: "CAL-1234".into(),
                title: Some("Reaper".into()),
                ..Ticket::default()
            });
        let agents = desired_agents("triad", &config.pipeline["triad"], &vars).unwrap();
        assert_eq!(
            agents[0].prompt.as_deref(),
            Some("Fix the double reap.\n\nReaper\n\nPlan cal-1234.")
        );
    }

    #[test]
    fn a_template_that_places_the_request_is_not_double_fed() {
        let config: Config = toml::from_str(
            r"
[pipeline.p]
prompt = 'context: {{ticket.title}}'

[[pipeline.p.agent]]
alias = 'a'
program = 'codex'
prompt = 'do this: {{ request }}'
",
        )
        .unwrap();
        let vars = Vars::new()
            .set(REQUEST_KEY, "ship it")
            .with_ticket(&Ticket {
                id: "X".into(),
                title: Some("T".into()),
                ..Ticket::default()
            });
        let agents = desired_agents("p", &config.pipeline["p"], &vars).unwrap();
        // Placed once, by the template — not prepended as well.
        assert_eq!(
            agents[0].prompt.as_deref(),
            Some("context: T\n\ndo this: ship it")
        );
    }

    #[test]
    fn a_request_alone_is_the_whole_prompt_for_a_bare_pipeline() {
        let config: Config = toml::from_str(
            r"
[[pipeline.bare.agent]]
alias = 'a'
program = 'claude'
",
        )
        .unwrap();
        let vars = Vars::new().set(REQUEST_KEY, "just do the thing");
        let agents = desired_agents("bare", &config.pipeline["bare"], &vars).unwrap();
        assert_eq!(agents[0].prompt.as_deref(), Some("just do the thing"));
    }

    #[test]
    fn places_request_tolerates_whitespace_and_ignores_other_keys() {
        assert!(places_request("a {{request}} b"));
        assert!(places_request("a {{  request  }} b"));
        assert!(!places_request("a {{requested}} b"));
        assert!(!places_request("a {{ticket.body}} b"));
        assert!(!places_request("unterminated {{request"));
    }

    #[test]
    fn a_duplicate_alias_is_refused_because_the_diff_keys_on_it() {
        let config: Config = toml::from_str(
            r"
[[pipeline.dup.agent]]
alias = 'a'
program = 'codex'

[[pipeline.dup.agent]]
alias = 'A'
program = 'claude'
",
        )
        .unwrap();
        let error = desired_agents("dup", &config.pipeline["dup"], &Vars::new())
            .unwrap_err()
            .to_string();
        assert!(error.contains("alias"), "{error}");
    }

    fn desired(aliases: &[&str]) -> Vec<DesiredAgent> {
        aliases
            .iter()
            .map(|alias| DesiredAgent {
                alias: (*alias).to_string(),
                program: "codex".into(),
                role: None,
                task: None,
                prompt: None,
                direction: None,
            })
            .collect()
    }

    fn existing(rows: &[(&str, Option<&str>)]) -> Vec<ExistingAgent> {
        existing_in(rows, Some(AgentState::Working))
    }

    fn existing_in(rows: &[(&str, Option<&str>)], state: Option<AgentState>) -> Vec<ExistingAgent> {
        rows.iter()
            .map(|(pane, alias)| ExistingAgent {
                pane: (*pane).to_string(),
                program: "codex".into(),
                alias: alias.map(str::to_string),
                state,
            })
            .collect()
    }

    #[test]
    fn an_empty_window_launches_everything() {
        let plan = plan(&desired(&["plan", "impl"]), &[], None);
        assert_eq!(plan.launches(), 2);
        assert!(!plan.converged());
    }

    #[test]
    fn re_running_a_staffed_window_is_a_no_op() {
        let plan = plan(
            &desired(&["plan", "impl"]),
            &existing(&[("%1", Some("plan")), ("%2", Some("impl"))]),
            None,
        );
        assert_eq!(plan.launches(), 0);
        assert!(plan.converged());
    }

    #[test]
    fn only_the_missing_alias_is_launched() {
        let plan = plan(
            &desired(&["plan", "impl", "review"]),
            &existing(&[("%1", Some("plan")), ("%2", Some("impl"))]),
            None,
        );
        assert_eq!(plan.launches(), 1);
        assert!(matches!(
            plan.steps.iter().find(|step| step.alias() == "review"),
            Some(PlanStep::Launch(_))
        ));
    }

    #[test]
    fn a_broadcast_reaches_live_panes_and_still_launches_missing_ones() {
        let plan = plan(
            &desired(&["plan", "review"]),
            &existing(&[("%1", Some("plan"))]),
            Some("rebase onto main first"),
        );
        assert_eq!(plan.reprompts(), 1);
        assert_eq!(plan.launches(), 1);
        assert!(matches!(
            &plan.steps[0],
            PlanStep::Reprompt { pane, prompt, .. }
                if pane == "%1" && prompt == "rebase onto main first"
        ));
    }

    #[test]
    fn an_agent_blocked_on_a_person_is_never_prompted_over() {
        // A prompt typed at an approval dialog is eaten by the dialog, and
        // the operator is left believing it was delivered.
        for state in [
            AgentState::WaitingInput,
            AgentState::WaitingChoice,
            AgentState::Error,
        ] {
            let plan = plan(
                &desired(&["impl"]),
                &existing_in(&[("%1", Some("impl"))], Some(state)),
                Some("carry on"),
            );
            assert_eq!(plan.reprompts(), 0, "{state:?} must not be prompted");
            assert_eq!(plan.attention(), 1, "{state:?}");
            assert!(!plan.converged(), "{state:?} is not converged");
        }
    }

    #[test]
    fn a_working_agent_is_kept_and_a_broadcast_still_reaches_it() {
        let working = existing_in(&[("%1", Some("impl"))], Some(AgentState::Working));
        assert!(plan(&desired(&["impl"]), &working, None).converged());
        assert_eq!(
            plan(&desired(&["impl"]), &working, Some("go")).reprompts(),
            1
        );

        // Idle is still staffed — it just has nothing to say for itself.
        // Distinguishing it from Working is the operator's call, not a
        // reason to relaunch.
        let idle = existing_in(&[("%1", Some("impl"))], Some(AgentState::Idle));
        assert!(plan(&desired(&["impl"]), &idle, None).converged());
        assert_eq!(plan(&desired(&["impl"]), &idle, Some("go")).reprompts(), 1);
    }

    #[test]
    fn an_unknown_state_is_treated_as_present_not_blocked() {
        // The daemon may simply not have this pane; that is not a reason to
        // claim a human is needed.
        assert!(!needs_a_human(None));
        assert!(!needs_a_human(Some(AgentState::Starting)));
        assert!(!needs_a_human(Some(AgentState::Stopped)));
    }

    #[test]
    fn a_pane_no_alias_claims_is_reported_and_left_alone() {
        let plan = plan(
            &desired(&["plan"]),
            &existing(&[("%1", Some("plan")), ("%9", None), ("%8", Some("retired"))]),
            None,
        );
        assert!(plan.converged());
        assert_eq!(plan.unclaimed.len(), 2);
        assert!(plan.unclaimed.iter().any(|agent| agent.pane == "%9"));
        assert!(plan.unclaimed.iter().any(|agent| agent.pane == "%8"));
    }

    #[test]
    fn the_default_worktree_path_sits_beside_the_repo_not_inside_it() {
        let config = WorktreeConfig {
            repo: "/home/june/workspace/callabo".into(),
            ..WorktreeConfig::default()
        };
        let vars = Vars::new().set("id", "cal-1234");
        let plan = worktree_plan(&config, &vars, &|value| value.to_string());
        assert_eq!(
            plan.path,
            Path::new("/home/june/workspace/callabo-worktrees/cal-1234")
        );
        assert_eq!(plan.branch, "cal-1234");
        assert!(!plan.path.starts_with(&plan.repo));
    }

    #[test]
    fn a_tracker_supplied_branch_beats_the_bare_work_id() {
        let config = WorktreeConfig {
            repo: "/repo".into(),
            ..WorktreeConfig::default()
        };
        let vars = Vars::new().set("id", "cal-7").with_ticket(&Ticket {
            id: "CAL-7".into(),
            branch: Some("june/cal-7-fix-reaper".into()),
            ..Ticket::default()
        });
        let plan = worktree_plan(&config, &vars, &|value| value.to_string());
        assert_eq!(plan.branch, "june/cal-7-fix-reaper");

        // No suggestion from the tracker: the work id is the branch.
        let bare = Vars::new().set("id", "cal-7");
        assert_eq!(
            worktree_plan(&config, &bare, &|value| value.to_string()).branch,
            "cal-7"
        );
    }

    #[test]
    fn an_explicit_worktree_path_and_branch_render_placeholders() {
        let config = WorktreeConfig {
            repo: "/repo".into(),
            path: Some("/wt/{{id}}".into()),
            branch: Some("june/{{id}}".into()),
            base: Some("origin/main".into()),
        };
        let vars = Vars::new().set("id", "cal-7");
        let plan = worktree_plan(&config, &vars, &|value| value.to_string());
        assert_eq!(plan.path, Path::new("/wt/cal-7"));
        assert_eq!(plan.branch, "june/cal-7");
        assert_eq!(plan.base.as_deref(), Some("origin/main"));
    }

    const GOOD: &str = r"
[[route]]
match = '^cal-'
pipeline = 'triad'

[pipeline.triad]
[[pipeline.triad.agent]]
alias = 'impl'
program = 'codex'
[[pipeline.triad.agent]]
alias = 'review'
program = 'claude'
";

    #[test]
    fn a_fenced_toml_block_is_lifted_out_of_the_prose_around_it() {
        let reply = format!("Sure, here you go:\n\n```toml{GOOD}```\n\nPaste that in.");
        let block = extract_toml_block(&reply).expect("block");
        assert!(block.contains("[pipeline.triad]"), "{block}");
        assert!(!block.contains("Paste that in"), "{block}");
    }

    #[test]
    fn bare_toml_with_no_fence_still_works() {
        let block = extract_toml_block(GOOD).expect("block");
        assert!(block.contains("[[route]]"));
        assert!(extract_toml_block("   ").is_none());
    }

    #[test]
    fn a_valid_proposal_reports_what_it_configures() {
        let (_, summary) = validate_proposal(GOOD).expect("valid");
        assert_eq!(summary.routes, vec!["^cal- → triad"]);
        assert_eq!(summary.pipelines, vec![("triad".to_string(), 2)]);
        assert!(summary.ticket_sources.is_empty());
    }

    #[test]
    fn an_invented_key_is_refused_before_it_reaches_the_config_file() {
        // Config denies unknown fields, so a model's typo would take the
        // daemon down at its next start. Catch it while it is still text.
        let error = validate_proposal(
            r"
[[route]]
match = '.*'
piepline = 'triad'
",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("does not parse"), "{error}");
    }

    #[test]
    fn a_route_naming_a_pipeline_nobody_defined_is_refused() {
        let error = validate_proposal(
            r"
[[route]]
match = '.*'
pipeline = 'ghost'

[pipeline.real]
[[pipeline.real.agent]]
alias = 'a'
program = 'codex'
",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("ghost"), "{error}");
    }

    #[test]
    fn a_program_that_is_not_an_agent_cli_is_refused() {
        let error = validate_proposal(
            r"
[[route]]
match = '.*'
pipeline = 'p'

[pipeline.p]
[[pipeline.p.agent]]
alias = 'a'
program = 'vim'
",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("vim"), "{error}");
        assert!(
            error.contains("claude"),
            "should list what is allowed: {error}"
        );
    }

    #[test]
    fn an_uncompilable_pattern_is_refused_with_the_pattern_named() {
        let error = validate_proposal(
            r"
[[route]]
match = 'cal-['
pipeline = 'p'

[pipeline.p]
[[pipeline.p.agent]]
alias = 'a'
program = 'codex'
",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("cal-["), "{error}");
    }

    #[test]
    fn a_proposal_that_configures_nothing_is_refused() {
        assert!(matches!(
            validate_proposal("[ticket]\nagent = 'claude'\n"),
            Err(PipelineError::EmptyProposal)
        ));
    }

    #[test]
    fn ticket_sources_pick_the_first_matching_pattern() {
        let config: Config = toml::from_str(
            r"
[ticket.source.linear]
match = '^cal-\d+$'
prompt = 'fetch {{id}}'

[ticket.source.github]
match = '^\d+$'
prompt = 'gh issue {{id}}'
",
        )
        .unwrap();
        let (name, source) = select_source(&config.ticket, "cal-12").unwrap().unwrap();
        assert_eq!(name, "linear");
        assert_eq!(source.prompt, "fetch {{id}}");
        assert!(select_source(&config.ticket, "nope-1").unwrap().is_none());
    }
}
