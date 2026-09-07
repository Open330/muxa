//! `muxa work options` and `muxa work preset` — what a Work launcher needs
//! to know before it can start anything, and the built-in line-ups that
//! make a fresh config launchable without spending an agent turn.
//!
//! A GUI that offers "start Work" has to answer three questions the CLI
//! answers by reading `config.toml`: which routes and pipelines exist, which
//! message skills the request composer can expand, and what to offer when
//! nothing is configured yet. `options` answers all three in one JSON
//! document so the GUI never parses TOML itself; `preset apply` turns the
//! "nothing configured" answer into a configured one by writing a built-in
//! pipeline through `toml_edit`, leaving every other byte of the file alone.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

use muxa::config::{Config, PipelineAgentConfig, PipelineConfig, RouteConfig};
use muxa::pipeline::{self, Vars};
use muxa::work_presets::{self, PRESET_NAMES};

/// Longest skill summary `options` reports, in characters.
const SKILL_SUMMARY_CHARS: usize = 120;

#[derive(Debug, clap::Args)]
pub struct OptionsArgs {
    /// Emit JSON for GUI launchers.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct PresetArgs {
    #[command(subcommand)]
    command: PresetCommand,
}

#[derive(Debug, clap::Subcommand)]
enum PresetCommand {
    /// List the built-in presets.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Write a built-in preset into config.toml as `[pipeline.<name>]`.
    Apply {
        /// Preset name: solo, pair, or triad.
        name: String,
        /// Also append a `[[route]]` whose `match` is this regex and whose
        /// pipeline is the preset. Skipped when a route with the same
        /// `match` already exists.
        #[arg(long, value_name = "REGEX")]
        route: Option<String>,
        /// Replace an existing `[pipeline.<name>]` instead of refusing.
        #[arg(long)]
        overwrite: bool,
        #[arg(long)]
        json: bool,
    },
}

/// Everything a launcher needs to start Work, in one document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkOptions {
    pub config_path: PathBuf,
    /// `true` once at least one `[pipeline.*]` exists.
    pub configured: bool,
    /// `[[route]]` entries in config order.
    pub routes: Vec<RouteOption>,
    /// `[pipeline.*]` entries sorted by name.
    pub pipelines: Vec<PipelineOption>,
    /// `[message.skills]` entries sorted by name.
    pub skills: Vec<SkillOption>,
    /// Built-in presets in their fixed order.
    pub presets: Vec<PipelineOption>,
    /// `[ticket].agent` — the CLI the resolver and `work init` default to.
    pub ticket_agent: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RouteOption {
    #[serde(rename = "match")]
    pub pattern: String,
    pub workspace: Option<String>,
    pub pipeline: Option<String>,
    pub cwd: Option<String>,
    pub worktree: bool,
    pub prepare: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PipelineOption {
    pub name: String,
    pub description: Option<String>,
    pub layout: Option<String>,
    /// The raw `[pipeline.<name>].prompt` template, not rendered.
    pub prompt: Option<String>,
    pub agents: Vec<AgentOption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentOption {
    pub alias: String,
    pub program: String,
    pub role: Option<String>,
    pub task: Option<String>,
    /// The raw agent `prompt` template, not rendered.
    pub prompt: Option<String>,
    pub direction: Option<String>,
    pub after: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillOption {
    pub name: String,
    /// First non-empty line of the template, at most 120 characters.
    pub summary: String,
}

/// What `preset apply` did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PresetApplied {
    pub pipeline: String,
    /// The `--route` regex when one was given, whether or not it had to be
    /// added.
    pub route: Option<String>,
    /// `false` when `--route` named a `match` that already existed.
    pub route_added: bool,
    /// `true` when `--overwrite` replaced an existing pipeline.
    pub replaced: bool,
    pub config_path: PathBuf,
}

pub fn run_options(args: OptionsArgs, config: &Config, config_path: Option<PathBuf>) -> Result<()> {
    let path = resolve_path(config_path)?;
    let options = options(config, &path);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&options)?);
    } else {
        print!("{}", render_options(&options));
    }
    Ok(())
}

pub fn run_preset(args: PresetArgs, config_path: Option<PathBuf>) -> Result<()> {
    match args.command {
        PresetCommand::List { json } => {
            let presets = preset_options();
            if json {
                println!("{}", serde_json::to_string_pretty(&presets)?);
            } else {
                print!("{}", render_presets(&presets));
            }
            Ok(())
        }
        PresetCommand::Apply {
            name,
            route,
            overwrite,
            json,
        } => {
            let path = resolve_path(config_path)?;
            let applied = apply_preset(&path, &name, route.as_deref(), overwrite)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&applied)?);
            } else {
                print!("{}", render_applied(&applied));
            }
            Ok(())
        }
    }
}

pub(crate) fn resolve_path(config_path: Option<PathBuf>) -> Result<PathBuf> {
    config_path
        .or_else(muxa::paths::default_config_file)
        .context("no config directory is available on this system")
}

/// Project a loaded config onto the launcher contract.
#[must_use]
pub fn options(config: &Config, config_path: &Path) -> WorkOptions {
    WorkOptions {
        config_path: config_path.to_path_buf(),
        configured: !config.pipeline.is_empty(),
        routes: config.route.iter().map(route_option).collect(),
        pipelines: config
            .pipeline
            .iter()
            .map(|(name, pipeline)| pipeline_option(name, pipeline))
            .collect(),
        skills: config
            .message
            .skills
            .iter()
            .map(|(name, template)| SkillOption {
                name: name.clone(),
                summary: skill_summary(template),
            })
            .collect(),
        presets: preset_options(),
        ticket_agent: config.ticket.agent.clone(),
    }
}

/// The built-in presets in the same shape `options` uses for pipelines.
#[must_use]
pub fn preset_options() -> Vec<PipelineOption> {
    work_presets::builtin()
        .iter()
        .map(|preset| pipeline_option(preset.name, &preset.pipeline))
        .collect()
}

fn route_option(route: &RouteConfig) -> RouteOption {
    RouteOption {
        pattern: route.pattern.clone(),
        workspace: route.workspace.clone(),
        pipeline: route.pipeline.clone(),
        cwd: route.cwd.clone(),
        worktree: route.worktree.is_some(),
        prepare: route
            .prepare
            .as_deref()
            .is_some_and(|command| !command.trim().is_empty()),
    }
}

fn pipeline_option(name: &str, pipeline: &PipelineConfig) -> PipelineOption {
    PipelineOption {
        name: name.to_string(),
        description: pipeline.description.clone(),
        layout: pipeline.layout.clone(),
        prompt: pipeline.prompt.clone(),
        agents: pipeline.agent.iter().map(agent_option).collect(),
    }
}

fn agent_option(agent: &PipelineAgentConfig) -> AgentOption {
    AgentOption {
        alias: agent.alias.clone(),
        program: agent.program.clone(),
        role: agent.role.clone(),
        task: agent.task.clone(),
        prompt: agent.prompt.clone(),
        direction: agent.direction.clone(),
        after: agent.after.clone(),
    }
}

/// First non-empty line of a skill template, clipped to
/// [`SKILL_SUMMARY_CHARS`] with the cut marked.
fn skill_summary(template: &str) -> String {
    let line = template
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    if line.chars().count() <= SKILL_SUMMARY_CHARS {
        return line.to_string();
    }
    let mut kept: String = line.chars().take(SKILL_SUMMARY_CHARS - 1).collect();
    kept.push('…');
    kept
}

// ---------------------------------------------------------------------------
// preset apply
// ---------------------------------------------------------------------------

/// Write `[pipeline.<name>]` for a built-in preset into `path`, optionally
/// routing a work-id regex to it. Everything else in the file — comments,
/// ordering, unrelated sections — is preserved by editing the parsed
/// document rather than regenerating it.
///
/// Refuses when the pipeline already exists unless `overwrite`, and refuses
/// a `route` regex that does not compile; neither touches the file. The
/// merged document is parsed back as a [`Config`] and validated before it
/// is written, so a file this function produces is one muxad will load.
pub fn apply_preset(
    path: &Path,
    name: &str,
    route: Option<&str>,
    overwrite: bool,
) -> Result<PresetApplied> {
    let preset = work_presets::find(name).with_context(|| {
        format!(
            "unknown preset {name:?}; built-in presets are: {}",
            PRESET_NAMES.join(", ")
        )
    })?;
    let route = route.map(str::trim).filter(|pattern| !pattern.is_empty());
    if let Some(pattern) = route {
        // Compiling through the same path `work up` uses keeps the error
        // text identical to what a bad hand-written route would produce.
        pipeline::select_route(
            &[RouteConfig {
                pattern: pattern.to_string(),
                ..RouteConfig::default()
            }],
            "",
        )?;
    }

    let mut document = load_document(path)?;
    let existing: Config = toml::from_str(&document.to_string())
        .with_context(|| format!("parsing {}", path.display()))?;
    let replaced = existing.pipeline.contains_key(preset.name);
    if replaced && !overwrite {
        bail!(
            "pipeline {:?} already exists in {}; pass --overwrite to replace it",
            preset.name,
            path.display()
        );
    }
    let route_added = route.is_some_and(|pattern| {
        !existing
            .route
            .iter()
            .any(|existing| existing.pattern == pattern)
    });

    // Append rather than splice: new tables are pinned after every table
    // already in the file, so nothing that was next to a section header
    // ends up next to a different one.
    let mut next = last_position(&document) + 1;
    let pipelines = pipelines_table_mut(&mut document)?;
    pipelines.remove(preset.name);
    let mut item = toml_edit::Item::Table(pipeline_table(&preset.pipeline));
    next = place_after(&mut item, next);
    pipelines.insert(preset.name, item);
    if route_added {
        if let Some(pattern) = route {
            let mut table = toml_edit::Table::new();
            table.insert("match", toml_edit::value(pattern));
            table.insert("pipeline", toml_edit::value(preset.name));
            table.set_position(next);
            routes_mut(&mut document)?.push(table);
        }
    }

    let (text, updated) = validated(&document)?;
    let written = updated
        .pipeline
        .get(preset.name)
        .context("the written pipeline did not read back")?;
    pipeline::desired_agents(preset.name, written, &Vars::new())?;
    write_config(path, &text)?;
    Ok(PresetApplied {
        pipeline: preset.name.to_string(),
        route: route.map(str::to_string),
        route_added,
        replaced,
        config_path: path.to_path_buf(),
    })
}

/// Render an edited document and read it back as a full [`Config`], so a
/// file this module writes is one muxad will load. Returns the text that
/// was checked, which is the text to write.
pub(crate) fn validated(document: &toml_edit::DocumentMut) -> Result<(String, Config)> {
    let text = document.to_string();
    let config: Config = toml::from_str(&text).context("validating updated muxa config")?;
    config
        .validate()
        .context("validating updated muxa config")?;
    Ok((text, config))
}

/// Write config text atomically, creating the directory if this is the
/// first thing ever written there.
pub(crate) fn write_config(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    crate::init::apply::atomic_write(path, text)
        .with_context(|| format!("writing {}", path.display()))
}

pub(crate) fn load_document(path: &Path) -> Result<toml_edit::DocumentMut> {
    match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => text
            .parse::<toml_edit::DocumentMut>()
            .with_context(|| format!("parsing {}", path.display())),
        Ok(_) => Ok(toml_edit::DocumentMut::new()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(toml_edit::DocumentMut::new())
        }
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

/// The `[pipeline]` parent, created implicit so only `[pipeline.<name>]`
/// headers are ever rendered.
pub(crate) fn pipelines_table_mut(
    document: &mut toml_edit::DocumentMut,
) -> Result<&mut toml_edit::Table> {
    if document.get("pipeline").is_none() {
        let mut table = toml_edit::Table::new();
        table.set_implicit(true);
        document["pipeline"] = toml_edit::Item::Table(table);
    }
    document["pipeline"]
        .as_table_mut()
        .context("[pipeline] is not a table")
}

pub(crate) fn routes_mut(
    document: &mut toml_edit::DocumentMut,
) -> Result<&mut toml_edit::ArrayOfTables> {
    if document.get("route").is_none() {
        document["route"] = toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new());
    }
    document["route"]
        .as_array_of_tables_mut()
        .context("route is not an array of [[route]] tables")
}

/// Render a pipeline as the `[pipeline.<name>]` table the config parser
/// reads back, key for key.
pub(crate) fn pipeline_table(pipeline: &PipelineConfig) -> toml_edit::Table {
    let mut table = toml_edit::Table::new();
    insert_opt(&mut table, "description", pipeline.description.as_deref());
    insert_opt(&mut table, "layout", pipeline.layout.as_deref());
    insert_opt(&mut table, "prompt", pipeline.prompt.as_deref());
    let mut agents = toml_edit::ArrayOfTables::new();
    for agent in &pipeline.agent {
        let mut entry = toml_edit::Table::new();
        entry.insert("alias", toml_edit::value(&agent.alias));
        entry.insert("program", toml_edit::value(&agent.program));
        insert_opt(&mut entry, "role", agent.role.as_deref());
        insert_opt(&mut entry, "task", agent.task.as_deref());
        insert_opt(&mut entry, "prompt", agent.prompt.as_deref());
        insert_opt(&mut entry, "direction", agent.direction.as_deref());
        if !agent.after.is_empty() {
            let after: toml_edit::Array = agent.after.iter().map(String::as_str).collect();
            entry.insert("after", toml_edit::value(after));
        }
        agents.push(entry);
    }
    table.insert("agent", toml_edit::Item::ArrayOfTables(agents));
    table
}

fn insert_opt(table: &mut toml_edit::Table, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        table.insert(key, toml_edit::value(value));
    }
}

/// Highest render position any table in the document currently occupies.
pub(crate) fn last_position(document: &toml_edit::DocumentMut) -> usize {
    max_position(document.as_item()).unwrap_or(0)
}

/// Highest render position of an item's own table or any table under it.
/// `None` for a value, or for tables that were never placed.
pub(crate) fn max_position(item: &toml_edit::Item) -> Option<usize> {
    fn table(table: &toml_edit::Table) -> Option<usize> {
        table
            .position()
            .into_iter()
            .chain(table.iter().filter_map(|(_, child)| max_position(child)))
            .max()
    }
    match item {
        toml_edit::Item::Table(inner) => table(inner),
        toml_edit::Item::ArrayOfTables(array) => array.iter().filter_map(table).max(),
        _ => None,
    }
}

/// Pin an item's tables to render from `next` on, in declaration order,
/// returning the next free position. Positions are what `toml_edit` sorts
/// headers by, so a table built through the API has to be placed or it
/// inherits whatever position the parser last saw.
pub(crate) fn place_after(item: &mut toml_edit::Item, mut next: usize) -> usize {
    fn table(table: &mut toml_edit::Table, mut next: usize) -> usize {
        table.set_position(next);
        next += 1;
        for (_, child) in table.iter_mut() {
            next = place_after(child, next);
        }
        next
    }
    match item {
        toml_edit::Item::Table(inner) => next = table(inner, next),
        toml_edit::Item::ArrayOfTables(array) => {
            for inner in array.iter_mut() {
                next = table(inner, next);
            }
        }
        _ => {}
    }
    next
}

// ---------------------------------------------------------------------------
// human output
// ---------------------------------------------------------------------------

fn render_options(options: &WorkOptions) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "config    {}", options.config_path.display());
    let _ = writeln!(
        out,
        "status    {}",
        if options.configured {
            "configured"
        } else {
            "not configured — no [pipeline.*] yet; try `muxa work preset apply solo --route '.*'`"
        }
    );
    let _ = writeln!(out, "ticket    {}", options.ticket_agent);

    out.push_str("\nroutes\n");
    if options.routes.is_empty() {
        out.push_str("  (none)\n");
    } else {
        let rows: Vec<[String; 4]> = options
            .routes
            .iter()
            .map(|route| {
                let mut flags = Vec::new();
                if route.worktree {
                    flags.push("worktree");
                }
                if route.prepare {
                    flags.push("prepare");
                }
                [
                    route.pattern.clone(),
                    route.workspace.clone().unwrap_or_else(|| "-".into()),
                    route.pipeline.clone().unwrap_or_else(|| "-".into()),
                    match (route.cwd.as_deref(), flags.is_empty()) {
                        (Some(cwd), true) => cwd.to_string(),
                        (Some(cwd), false) => format!("{cwd} ({})", flags.join(", ")),
                        (None, true) => "-".into(),
                        (None, false) => format!("({})", flags.join(", ")),
                    },
                ]
            })
            .collect();
        out.push_str(&table(&["MATCH", "WORKSPACE", "PIPELINE", "CWD"], &rows));
    }

    out.push_str("\npipelines\n");
    if options.pipelines.is_empty() {
        out.push_str("  (none)\n");
    } else {
        let rows: Vec<[String; 4]> = options.pipelines.iter().map(pipeline_row).collect();
        out.push_str(&table(&["NAME", "AGENTS", "LAYOUT", "DESCRIPTION"], &rows));
    }

    out.push_str("\nskills    ");
    if options.skills.is_empty() {
        out.push_str("(none)\n");
    } else {
        let names: Vec<&str> = options
            .skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect();
        out.push_str(&names.join(", "));
        out.push('\n');
    }
    let presets: Vec<&str> = options
        .presets
        .iter()
        .map(|preset| preset.name.as_str())
        .collect();
    let _ = writeln!(out, "presets   {}", presets.join(", "));
    out
}

fn render_presets(presets: &[PipelineOption]) -> String {
    let rows: Vec<[String; 4]> = presets.iter().map(pipeline_row).collect();
    let mut out = table(&["NAME", "AGENTS", "LAYOUT", "DESCRIPTION"], &rows);
    out.push_str("\napply one:  muxa work preset apply <name> [--route '<regex>'] [--overwrite]\n");
    out
}

fn render_applied(applied: &PresetApplied) -> String {
    use std::fmt::Write as _;
    let mut out = format!(
        "{} [pipeline.{}] in {}\n",
        if applied.replaced {
            "replaced"
        } else {
            "wrote"
        },
        applied.pipeline,
        applied.config_path.display()
    );
    if let Some(route) = &applied.route {
        if applied.route_added {
            let _ = writeln!(
                out,
                "added [[route]] match = {route:?} → pipeline {}",
                applied.pipeline
            );
        } else {
            let _ = writeln!(
                out,
                "a [[route]] with match = {route:?} already exists; left it unchanged"
            );
        }
    }
    out.push_str("try it:  muxa work up <work-id> --dry-run\n");
    out
}

fn pipeline_row(pipeline: &PipelineOption) -> [String; 4] {
    let agents: Vec<String> = pipeline
        .agents
        .iter()
        .map(|agent| format!("{}:{}", agent.alias, agent.program))
        .collect();
    [
        pipeline.name.clone(),
        agents.join(" "),
        pipeline.layout.clone().unwrap_or_else(|| "-".into()),
        pipeline.description.clone().unwrap_or_else(|| "-".into()),
    ]
}

/// A two-space-indented text table with left-aligned columns.
fn table<const N: usize>(header: &[&str; N], rows: &[[String; N]]) -> String {
    use unicode_width::UnicodeWidthStr;
    let mut widths: [usize; N] = header.map(UnicodeWidthStr::width);
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.width());
        }
    }
    let line = |cells: [&str; N]| {
        let mut out = String::from(" ");
        for (index, (cell, width)) in cells.iter().zip(widths).enumerate() {
            out.push(' ');
            out.push_str(cell);
            if index + 1 < N {
                out.extend(std::iter::repeat_n(' ', width.saturating_sub(cell.width())));
            }
        }
        out.push('\n');
        out
    };
    let mut out = line(*header);
    for row in rows {
        out.push_str(&line(std::array::from_fn(|index| row[index].as_str())));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::collections::BTreeSet;

    fn keys(value: &Value) -> BTreeSet<&str> {
        value
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect()
    }

    const CONFIGURED: &str = r#"
[watch]
spinner = false

[message.skills]
review = """

  Review the current diff for correctness.
  Second line is not part of the summary.
"""
plan = "Write a plan first."

[ticket]
agent = "codex"

[[route]]
match = '^cal-'
workspace = 'callabo'
pipeline = 'triad'
prepare = 'mk-ws {{id}}'
[route.worktree]
repo = '~/workspace/callabo'

[[route]]
match = '.*'
cwd = '{{cwd}}'
pipeline = 'triad'

[pipeline.triad]
description = 'planner → implementer → reviewer'
layout = 'main-vertical'

[[pipeline.triad.agent]]
alias = 'plan'
program = 'codex'
role = 'planner'

[[pipeline.triad.agent]]
alias = 'impl'
program = 'codex'
role = 'implementer'
direction = 'down'
after = ['plan']
"#;

    #[test]
    fn options_on_an_empty_config_offer_only_presets() {
        let path = Path::new("/tmp/muxa-test/config.toml");
        let options = options(&Config::default(), path);
        let json = serde_json::to_value(&options).unwrap();
        assert_eq!(
            keys(&json),
            BTreeSet::from([
                "config_path",
                "configured",
                "routes",
                "pipelines",
                "skills",
                "presets",
                "ticket_agent",
            ])
        );
        assert_eq!(json["config_path"], "/tmp/muxa-test/config.toml");
        assert_eq!(json["configured"], false);
        assert_eq!(json["routes"], Value::Array(vec![]));
        assert_eq!(json["pipelines"], Value::Array(vec![]));
        assert_eq!(json["skills"], Value::Array(vec![]));
        assert_eq!(json["ticket_agent"], "claude");
        let presets = json["presets"].as_array().unwrap();
        let names: Vec<&str> = presets
            .iter()
            .map(|preset| preset["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, PRESET_NAMES);
        assert_eq!(
            keys(&presets[0]),
            BTreeSet::from(["name", "description", "layout", "prompt", "agents"])
        );
        assert_eq!(presets[0]["prompt"], "You are working on {{work}}.");
        let solo_agent = &presets[0]["agents"][0];
        assert_eq!(
            keys(solo_agent),
            BTreeSet::from([
                "alias",
                "program",
                "role",
                "task",
                "prompt",
                "direction",
                "after"
            ])
        );
        assert!(
            solo_agent["prompt"]
                .as_str()
                .is_some_and(|prompt| prompt.starts_with("You own the implementation")),
            "{solo_agent}"
        );
        assert_eq!(solo_agent["alias"], "claude");
        assert_eq!(solo_agent["direction"], Value::Null);
        assert_eq!(solo_agent["after"], Value::Array(vec![]));
        assert_eq!(presets[0]["layout"], Value::Null);
        assert_eq!(presets[2]["layout"], "main-vertical");
        assert_eq!(
            presets[2]["agents"][2]["after"],
            serde_json::json!(["impl"])
        );
    }

    #[test]
    fn options_report_routes_pipelines_and_skills_from_a_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, CONFIGURED).unwrap();
        let config = Config::load(&path).unwrap();
        let options = options(&config, &path);
        let json = serde_json::to_value(&options).unwrap();

        assert_eq!(json["configured"], true);
        assert_eq!(json["ticket_agent"], "codex");
        assert_eq!(json["config_path"], path.to_string_lossy().as_ref());

        let routes = json["routes"].as_array().unwrap();
        assert_eq!(routes.len(), 2);
        assert_eq!(
            keys(&routes[0]),
            BTreeSet::from([
                "match",
                "workspace",
                "pipeline",
                "cwd",
                "worktree",
                "prepare"
            ])
        );
        assert_eq!(routes[0]["match"], "^cal-");
        assert_eq!(routes[0]["workspace"], "callabo");
        assert_eq!(routes[0]["pipeline"], "triad");
        assert_eq!(routes[0]["cwd"], Value::Null);
        assert_eq!(routes[0]["worktree"], true);
        assert_eq!(routes[0]["prepare"], true);
        assert_eq!(routes[1]["match"], ".*");
        assert_eq!(routes[1]["workspace"], Value::Null);
        assert_eq!(routes[1]["cwd"], "{{cwd}}");
        assert_eq!(routes[1]["worktree"], false);
        assert_eq!(routes[1]["prepare"], false);

        let pipelines = json["pipelines"].as_array().unwrap();
        assert_eq!(pipelines.len(), 1);
        assert_eq!(pipelines[0]["name"], "triad");
        assert_eq!(
            pipelines[0]["description"],
            "planner → implementer → reviewer"
        );
        assert_eq!(pipelines[0]["layout"], "main-vertical");
        let agents = pipelines[0]["agents"].as_array().unwrap();
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0]["alias"], "plan");
        assert_eq!(agents[0]["task"], Value::Null);
        assert_eq!(agents[0]["prompt"], Value::Null);
        assert_eq!(pipelines[0]["prompt"], Value::Null);
        assert_eq!(agents[0]["after"], Value::Array(vec![]));
        assert_eq!(agents[1]["direction"], "down");
        assert_eq!(agents[1]["after"], serde_json::json!(["plan"]));

        let skills = json["skills"].as_array().unwrap();
        assert_eq!(skills.len(), 2);
        // BTreeMap order: plan before review.
        assert_eq!(skills[0]["name"], "plan");
        assert_eq!(skills[0]["summary"], "Write a plan first.");
        assert_eq!(skills[1]["name"], "review");
        assert_eq!(
            skills[1]["summary"],
            "Review the current diff for correctness."
        );

        let human = render_options(&options);
        assert!(human.contains("^cal-"), "{human}");
        assert!(human.contains("triad"), "{human}");
        assert!(human.contains("solo, pair, triad"), "{human}");
    }

    #[test]
    fn skill_summary_clips_long_first_lines() {
        let long = "x".repeat(200);
        let summary = skill_summary(&format!("\n\n{long}\nmore"));
        assert_eq!(summary.chars().count(), SKILL_SUMMARY_CHARS);
        assert!(summary.ends_with('…'));
        assert_eq!(skill_summary("  short  \n"), "short");
        assert_eq!(skill_summary("\n \n"), "");
    }

    #[test]
    fn apply_writes_a_preset_into_a_missing_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        let applied = apply_preset(&path, "solo", None, false).unwrap();
        assert_eq!(
            applied,
            PresetApplied {
                pipeline: "solo".into(),
                route: None,
                route_added: false,
                replaced: false,
                config_path: path.clone(),
            }
        );
        let json = serde_json::to_value(&applied).unwrap();
        assert_eq!(json["pipeline"], "solo");
        assert_eq!(json["route"], Value::Null);
        assert_eq!(json["config_path"], path.to_string_lossy().as_ref());

        let config = Config::load(&path).unwrap();
        assert_eq!(
            config.pipeline["solo"],
            work_presets::find("solo").unwrap().pipeline
        );
        assert!(config.route.is_empty());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[pipeline.solo]"), "{text}");
        assert!(text.contains("[[pipeline.solo.agent]]"), "{text}");
        assert!(!text.contains("\n[pipeline]\n"), "{text}");
    }

    #[test]
    fn apply_refuses_an_existing_pipeline_unless_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[pipeline.pair]\ndescription = 'mine'\n[[pipeline.pair.agent]]\nalias = 'x'\nprogram = 'gemini'\n",
        )
        .unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let error = apply_preset(&path, "pair", None, false).unwrap_err();
        assert!(error.to_string().contains("--overwrite"), "{error:#}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        let applied = apply_preset(&path, "pair", None, true).unwrap();
        assert!(applied.replaced);
        let config = Config::load(&path).unwrap();
        assert_eq!(
            config.pipeline["pair"],
            work_presets::find("pair").unwrap().pipeline
        );
        assert_eq!(config.pipeline.len(), 1);
    }

    #[test]
    fn apply_appends_a_route_once_and_keeps_the_rest_of_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, CONFIGURED).unwrap();

        let applied = apply_preset(&path, "pair", Some("^muxa-"), false).unwrap();
        assert_eq!(applied.route.as_deref(), Some("^muxa-"));
        assert!(applied.route_added);
        assert!(!applied.replaced);

        let text = std::fs::read_to_string(&path).unwrap();
        // Unrelated sections and the hand-written pipeline are untouched.
        assert!(text.starts_with("\n[watch]\nspinner = false\n"), "{text}");
        assert!(
            text.contains("[pipeline.triad]\ndescription = 'planner → implementer → reviewer'"),
            "{text}"
        );
        let config = Config::load(&path).unwrap();
        assert!(!config.watch.spinner);
        assert_eq!(config.ticket.agent, "codex");
        assert_eq!(config.message.skills.len(), 2);
        assert_eq!(config.pipeline.len(), 2);
        assert_eq!(
            config.pipeline["pair"],
            work_presets::find("pair").unwrap().pipeline
        );
        // Routes keep their order; the new one is last.
        let patterns: Vec<&str> = config
            .route
            .iter()
            .map(|route| route.pattern.as_str())
            .collect();
        assert_eq!(patterns, ["^cal-", ".*", "^muxa-"]);
        assert_eq!(config.route[2].pipeline.as_deref(), Some("pair"));
        assert!(config.route[0].worktree.is_some());

        // Same match again: reported, not duplicated.
        let again = apply_preset(&path, "pair", Some("^muxa-"), true).unwrap();
        assert!(!again.route_added);
        assert!(again.replaced);
        let config = Config::load(&path).unwrap();
        assert_eq!(config.route.len(), 3);

        // A match that already routes elsewhere is left pointing there.
        let existing = apply_preset(&path, "solo", Some(".*"), false).unwrap();
        assert!(!existing.route_added);
        let config = Config::load(&path).unwrap();
        assert_eq!(config.route.len(), 3);
        assert_eq!(config.route[1].pipeline.as_deref(), Some("triad"));
        assert!(config.pipeline.contains_key("solo"));
    }

    #[test]
    fn apply_refuses_bad_input_without_touching_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[watch]\nspinner = false\n").unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let error = apply_preset(&path, "quartet", None, false).unwrap_err();
        assert!(error.to_string().contains("solo, pair, triad"), "{error:#}");
        let error = apply_preset(&path, "solo", Some("(unclosed"), false).unwrap_err();
        assert!(error.to_string().contains("regex"), "{error:#}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        let missing = dir.path().join("absent.toml");
        assert!(apply_preset(&missing, "quartet", None, false).is_err());
        assert!(!missing.exists());
    }

    #[test]
    fn preset_list_matches_the_options_presets() {
        assert_eq!(
            preset_options(),
            options(&Config::default(), Path::new("x")).presets
        );
        let human = render_presets(&preset_options());
        assert!(human.contains("triad"), "{human}");
        assert!(
            human.contains("plan:codex impl:codex review:claude"),
            "{human}"
        );
    }
}
