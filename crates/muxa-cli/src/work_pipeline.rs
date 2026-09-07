//! `muxa work pipeline` and `muxa work route` — the edits a visual
//! pipeline editor makes, written through `toml_edit` so the rest of
//! `config.toml` keeps its comments, order, and unrelated sections.
//!
//! The editor never sees TOML. It reads `muxa work options --json`, lets
//! the operator move agents and edges around, and hands the result back
//! in the same JSON shape; `pipeline set` turns that into
//! `[pipeline.<name>]` and its `[[pipeline.<name>.agent]]` tables after
//! running the checks a hand-written pipeline would fail at launch:
//! allowlisted programs, unique aliases, `after` edges that resolve and do
//! not cycle. `route set` is the same idea for one `[[route]]`, keyed by
//! its `match` text. Nothing is written until the merged document reads
//! back as a full [`Config`](muxa::config::Config).

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

use muxa::config::RouteConfig;
use muxa::pipeline::{self, Vars};
use muxa::work_pipeline_spec::validate_spec;
pub use muxa::work_pipeline_spec::PipelineSpec;

use crate::work_options::{
    last_position, load_document, max_position, pipeline_table, pipelines_table_mut, place_after,
    resolve_path, routes_mut, validated, write_config,
};

#[derive(Debug, clap::Args)]
pub struct PipelineArgs {
    #[command(subcommand)]
    command: PipelineCommand,
}

#[derive(Debug, clap::Subcommand)]
enum PipelineCommand {
    /// Write or replace `[pipeline.<name>]` from a JSON description.
    Set {
        /// Pipeline name: letters, digits, `-`, and `_`.
        name: String,
        /// JSON file describing the pipeline, or `-` to read it from
        /// stdin. The shape is one `pipelines[]` entry of `muxa work
        /// options --json`, without its `name`.
        #[arg(long, value_name = "PATH")]
        from_json: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Remove `[pipeline.<name>]` and its agents.
    Remove {
        name: String,
        /// Also clear `pipeline` on every `[[route]]` that names it,
        /// instead of refusing.
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, clap::Args)]
pub struct RouteArgs {
    #[command(subcommand)]
    command: RouteCommand,
}

#[derive(Debug, clap::Subcommand)]
enum RouteCommand {
    /// Add a `[[route]]`, or update the one whose `match` is this regex.
    Set {
        #[command(flatten)]
        edit: RouteEdit,
        #[arg(long)]
        json: bool,
    },
    /// Remove the `[[route]]` whose `match` is this regex.
    Remove {
        /// The route's `match` regex, exactly as written in config.toml.
        #[arg(long = "match", value_name = "REGEX")]
        pattern: String,
        #[arg(long)]
        json: bool,
    },
}

/// What `route set` changes. Unset flags leave a field alone; the
/// `--clear-*` flags are how a field is removed.
#[derive(Debug, Clone, Default, PartialEq, Eq, clap::Args)]
pub struct RouteEdit {
    /// Work-id regex. The route with exactly this `match` is updated;
    /// otherwise a new one is added.
    #[arg(long = "match", value_name = "REGEX")]
    pub pattern: String,
    /// Pipeline to staff the work window with; must be defined.
    #[arg(long, value_name = "NAME", conflicts_with = "clear_pipeline")]
    pub pipeline: Option<String>,
    /// Workspace (tmux session) id.
    #[arg(long, value_name = "ID", conflicts_with = "clear_workspace")]
    pub workspace: Option<String>,
    /// Working directory for the work window.
    #[arg(long, value_name = "PATH", conflicts_with = "clear_cwd")]
    pub cwd: Option<String>,
    /// 0-based index among the routes. Routes match first-wins, so a
    /// specific rule goes above the catch-all; past the end means last.
    #[arg(long, value_name = "N")]
    pub position: Option<usize>,
    /// Remove `pipeline` from the route.
    #[arg(long)]
    pub clear_pipeline: bool,
    /// Remove `workspace` from the route.
    #[arg(long)]
    pub clear_workspace: bool,
    /// Remove `cwd` from the route.
    #[arg(long)]
    pub clear_cwd: bool,
}

/// What `pipeline set` did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PipelineSet {
    pub pipeline: String,
    /// `true` when a `[pipeline.<name>]` already existed and was replaced.
    pub replaced: bool,
    pub config_path: PathBuf,
    /// Agent count, for the human line only.
    #[serde(skip)]
    pub agents: usize,
}

/// What `pipeline remove` did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PipelineRemoved {
    pub pipeline: String,
    pub removed: bool,
    /// `match` of every `[[route]]` whose `pipeline` was cleared by
    /// `--force`, in config order.
    pub routes_cleared: Vec<String>,
    pub config_path: PathBuf,
}

/// What `route set` did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RouteSet {
    #[serde(rename = "match")]
    pub pattern: String,
    /// 0-based index the route now has among the `[[route]]` entries.
    pub position: usize,
    /// `false` when a route with this `match` already existed.
    pub created: bool,
    pub config_path: PathBuf,
}

/// What `route remove` did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RouteRemoved {
    #[serde(rename = "match")]
    pub pattern: String,
    pub removed: bool,
    pub config_path: PathBuf,
}

pub fn run_pipeline(args: PipelineArgs, config_path: Option<PathBuf>) -> Result<()> {
    let path = resolve_path(config_path)?;
    match args.command {
        PipelineCommand::Set {
            name,
            from_json,
            json,
        } => {
            let spec = read_spec(&from_json)?;
            let set = set_pipeline(&path, &name, spec)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&set)?);
            } else {
                print!("{}", render_set(&set));
            }
        }
        PipelineCommand::Remove { name, force, json } => {
            let removed = remove_pipeline(&path, &name, force)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&removed)?);
            } else {
                print!("{}", render_removed(&removed));
            }
        }
    }
    Ok(())
}

pub fn run_route(args: RouteArgs, config_path: Option<PathBuf>) -> Result<()> {
    let path = resolve_path(config_path)?;
    match args.command {
        RouteCommand::Set { edit, json } => {
            let set = set_route(&path, &edit)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&set)?);
            } else {
                print!("{}", render_route_set(&set));
            }
        }
        RouteCommand::Remove { pattern, json } => {
            let removed = remove_route(&path, &pattern)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&removed)?);
            } else {
                print!("{}", render_route_removed(&removed));
            }
        }
    }
    Ok(())
}

fn read_spec(source: &Path) -> Result<PipelineSpec> {
    let text = if source == Path::new("-") {
        std::io::read_to_string(std::io::stdin()).context("reading the pipeline JSON from stdin")?
    } else {
        std::fs::read_to_string(source).with_context(|| format!("reading {}", source.display()))?
    };
    serde_json::from_str(&text).context("parsing the pipeline JSON")
}

// ---------------------------------------------------------------------------
// pipeline set / remove
// ---------------------------------------------------------------------------

/// Write `[pipeline.<name>]` from `spec`: an existing pipeline is replaced
/// where it stands, keeping the comment above its header; a new one goes
/// after the last pipeline, or at the end of a file that has none.
///
/// Refuses, without touching the file, a name TOML cannot use as a bare
/// key, a JSON `name` that disagrees with the command line, and any
/// line-up `muxa work up` would refuse to launch — the same checks the
/// daemon's `work_compose` runs, from [`muxa::work_pipeline_spec`].
pub fn set_pipeline(path: &Path, name: &str, spec: PipelineSpec) -> Result<PipelineSet> {
    let pipeline = validate_spec(name, &spec)?;

    let mut document = load_document(path)?;
    let previous = document
        .get("pipeline")
        .and_then(toml_edit::Item::as_table)
        .and_then(|pipelines| pipelines.get(name))
        .map(|item| {
            let table = item.as_table();
            (
                table.and_then(toml_edit::Table::position),
                table.and_then(|table| table.decor().prefix().cloned()),
            )
        });
    let replaced = previous.is_some();
    let mut item = toml_edit::Item::Table(pipeline_table(&pipeline));
    if let Some((_, Some(prefix))) = &previous {
        if let Some(table) = item.as_table_mut() {
            table.decor_mut().set_prefix(prefix.clone());
        }
    }
    pipelines_table_mut(&mut document)?.remove(name);
    let anchor = previous
        .and_then(|(position, _)| position)
        .or_else(|| {
            document
                .get("pipeline")
                .and_then(max_position)
                .map(|last| last + 1)
        })
        .unwrap_or_else(|| last_position(&document) + 1);
    make_room(&mut document, anchor, &mut item);
    pipelines_table_mut(&mut document)?.insert(name, item);

    let (text, config) = validated(&document)?;
    let written = config
        .pipeline
        .get(name)
        .context("the written pipeline did not read back")?;
    pipeline::desired_agents(name, written, &Vars::new())?;
    write_config(path, &text)?;
    Ok(PipelineSet {
        pipeline: name.to_string(),
        replaced,
        config_path: path.to_path_buf(),
        agents: pipeline.agent.len(),
    })
}

/// Remove `[pipeline.<name>]`. A `[[route]]` still naming the pipeline
/// makes this refuse, since `muxa work up` on that route would then fail
/// with a pipeline nobody defines; `force` clears `pipeline` on those
/// routes instead, leaving the rest of each route (its worktree, its
/// prepare command) alone.
pub fn remove_pipeline(path: &Path, name: &str, force: bool) -> Result<PipelineRemoved> {
    let mut document = load_document(path)?;
    if !pipeline_defined(&document, name) {
        bail!("pipeline {name:?} is not defined in {}", path.display());
    }
    let naming: Vec<(usize, String)> = document
        .get("route")
        .and_then(toml_edit::Item::as_array_of_tables)
        .map(|routes| {
            routes
                .iter()
                .enumerate()
                .filter(|(_, route)| {
                    route.get("pipeline").and_then(toml_edit::Item::as_str) == Some(name)
                })
                .map(|(index, route)| (index, route_match(route).to_string()))
                .collect()
        })
        .unwrap_or_default();
    if !naming.is_empty() && !force {
        let matches: Vec<String> = naming
            .iter()
            .map(|(_, pattern)| format!("{pattern:?}"))
            .collect();
        bail!(
            "pipeline {name:?} is still named by {} [[route]] (match = {}); \
             pass --force to remove it and clear `pipeline` on those routes",
            naming.len(),
            matches.join(", ")
        );
    }
    if force {
        let routes = routes_mut(&mut document)?;
        for (index, _) in &naming {
            if let Some(route) = routes.get_mut(*index) {
                route.remove("pipeline");
            }
        }
    }
    let orphaned = {
        let pipelines = pipelines_table_mut(&mut document)?;
        pipelines.remove(name);
        pipelines.is_empty() && pipelines.is_implicit()
    };
    if orphaned {
        document.remove("pipeline");
    }
    let (text, _) = validated(&document)?;
    write_config(path, &text)?;
    Ok(PipelineRemoved {
        pipeline: name.to_string(),
        removed: true,
        routes_cleared: naming.into_iter().map(|(_, pattern)| pattern).collect(),
        config_path: path.to_path_buf(),
    })
}

fn pipeline_defined(document: &toml_edit::DocumentMut, name: &str) -> bool {
    document
        .get("pipeline")
        .and_then(toml_edit::Item::as_table_like)
        .is_some_and(|pipelines| pipelines.contains_key(name))
}

fn route_match(route: &toml_edit::Table) -> &str {
    route
        .get("match")
        .and_then(toml_edit::Item::as_str)
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// route set / remove
// ---------------------------------------------------------------------------

/// Add or update the `[[route]]` whose `match` is exactly `edit.pattern`.
/// Fields the edit does not mention stay as they are, `worktree` and
/// `prepare` included; a new route lands at `edit.position` or last, and
/// an existing one moves there when asked.
pub fn set_route(path: &Path, edit: &RouteEdit) -> Result<RouteSet> {
    let pattern = edit.pattern.as_str();
    if pattern.trim().is_empty() {
        bail!("--match needs a regex; `.*` is the catch-all");
    }
    // Compiling through the same path `work up` uses keeps the error text
    // identical to what a bad hand-written route would produce.
    pipeline::select_route(
        &[RouteConfig {
            pattern: pattern.to_string(),
            ..RouteConfig::default()
        }],
        "",
    )?;

    let mut document = load_document(path)?;
    if let Some(name) = edit.pipeline.as_deref() {
        if !pipeline_defined(&document, name) {
            bail!(
                "pipeline {name:?} is not defined in {}; `muxa work options` lists the ones that are",
                path.display()
            );
        }
    }

    let found = routes_mut(&mut document)?
        .iter()
        .position(|route| route_match(route) == pattern);
    let (position, created) = if let Some(index) = found {
        let moved = {
            let routes = routes_mut(&mut document)?;
            let route = routes.get_mut(index).context("the route disappeared")?;
            apply_fields(route, edit);
            match edit.position {
                Some(wanted) if wanted != index => {
                    let table = routes
                        .get(index)
                        .cloned()
                        .context("the route disappeared")?;
                    routes.remove(index);
                    Some((table, wanted))
                }
                _ => None,
            }
        };
        match moved {
            Some((table, wanted)) => (insert_route(&mut document, table, wanted)?, false),
            None => (index, false),
        }
    } else {
        let mut table = toml_edit::Table::new();
        table.insert("match", toml_edit::value(pattern));
        apply_fields(&mut table, edit);
        let last = routes_mut(&mut document)?.len();
        (
            insert_route(&mut document, table, edit.position.unwrap_or(last))?,
            true,
        )
    };

    let (text, _) = validated(&document)?;
    write_config(path, &text)?;
    Ok(RouteSet {
        pattern: pattern.to_string(),
        position,
        created,
        config_path: path.to_path_buf(),
    })
}

/// Remove the `[[route]]` whose `match` is exactly `pattern`.
pub fn remove_route(path: &Path, pattern: &str) -> Result<RouteRemoved> {
    let mut document = load_document(path)?;
    let index = document
        .get("route")
        .and_then(toml_edit::Item::as_array_of_tables)
        .and_then(|routes| {
            routes
                .iter()
                .position(|route| route_match(route) == pattern)
        });
    let Some(index) = index else {
        bail!("no [[route]] in {} has match = {pattern:?}", path.display());
    };
    let orphaned = {
        let routes = routes_mut(&mut document)?;
        routes.remove(index);
        routes.is_empty()
    };
    if orphaned {
        document.remove("route");
    }
    let (text, _) = validated(&document)?;
    write_config(path, &text)?;
    Ok(RouteRemoved {
        pattern: pattern.to_string(),
        removed: true,
        config_path: path.to_path_buf(),
    })
}

fn apply_fields(route: &mut toml_edit::Table, edit: &RouteEdit) {
    let fields = [
        ("workspace", edit.workspace.as_deref(), edit.clear_workspace),
        ("pipeline", edit.pipeline.as_deref(), edit.clear_pipeline),
        ("cwd", edit.cwd.as_deref(), edit.clear_cwd),
    ];
    for (key, value, clear) in fields {
        if clear {
            route.remove(key);
        } else if let Some(value) = value {
            set_string(route, key, value);
        }
    }
}

/// Replace a string value in place, keeping the key's spacing and any
/// comment on the line, or add the key when it is new.
fn set_string(table: &mut toml_edit::Table, key: &str, text: &str) {
    match table.get_mut(key).and_then(toml_edit::Item::as_value_mut) {
        Some(value) => {
            let decor = value.decor().clone();
            *value = toml_edit::Value::from(text);
            *value.decor_mut() = decor;
        }
        None => {
            table.insert(key, toml_edit::value(text));
        }
    }
}

/// Put `table` at `index` among the `[[route]]` tables, clamped to the
/// end, rendered beside its neighbours rather than at the bottom of the
/// file. Returns the index it landed at.
fn insert_route(
    document: &mut toml_edit::DocumentMut,
    table: toml_edit::Table,
    index: usize,
) -> Result<usize> {
    let (index, displaced) = {
        let routes = routes_mut(document)?;
        let index = index.min(routes.len());
        (
            index,
            routes.get(index).and_then(toml_edit::Table::position),
        )
    };
    let anchor = displaced
        .or_else(|| {
            document
                .get("route")
                .and_then(max_position)
                .map(|last| last + 1)
        })
        .unwrap_or_else(|| last_position(document) + 1);
    let mut item = toml_edit::Item::Table(table);
    make_room(document, anchor, &mut item);
    let table = item
        .into_table()
        .map_err(|_| anyhow::anyhow!("a route is a table"))?;
    let routes = routes_mut(document)?;
    let mut tables: Vec<toml_edit::Table> = routes.iter().cloned().collect();
    tables.insert(index, table);
    routes.clear();
    for table in tables {
        routes.push(table);
    }
    Ok(index)
}

// ---------------------------------------------------------------------------
// table positions
// ---------------------------------------------------------------------------

/// Make room at `anchor` for the tables in `item` and place them there,
/// pushing every table already at or past `anchor` down by as many slots.
/// Only header order changes; no value is touched.
fn make_room(document: &mut toml_edit::DocumentMut, anchor: usize, item: &mut toml_edit::Item) {
    shift_positions(document.as_table_mut(), anchor, count_tables(item));
    place_after(item, anchor);
}

/// Tables an item renders as headers: itself, and every table under it.
fn count_tables(item: &toml_edit::Item) -> usize {
    fn table(table: &toml_edit::Table) -> usize {
        1 + table
            .iter()
            .map(|(_, child)| count_tables(child))
            .sum::<usize>()
    }
    match item {
        toml_edit::Item::Table(inner) => table(inner),
        toml_edit::Item::ArrayOfTables(array) => array.iter().map(table).sum(),
        _ => 0,
    }
}

fn shift_positions(table: &mut toml_edit::Table, from: usize, by: usize) {
    fn shift(table: &mut toml_edit::Table, from: usize, by: usize) {
        if let Some(position) = table.position().filter(|position| *position >= from) {
            table.set_position(position + by);
        }
        shift_positions(table, from, by);
    }
    for (_, item) in table.iter_mut() {
        match item {
            toml_edit::Item::Table(child) => shift(child, from, by),
            toml_edit::Item::ArrayOfTables(array) => {
                for child in array.iter_mut() {
                    shift(child, from, by);
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// human output
// ---------------------------------------------------------------------------

fn render_set(set: &PipelineSet) -> String {
    format!(
        "{} [pipeline.{}] with {} agent{} in {}\n",
        if set.replaced { "replaced" } else { "wrote" },
        set.pipeline,
        set.agents,
        if set.agents == 1 { "" } else { "s" },
        set.config_path.display()
    )
}

fn render_removed(removed: &PipelineRemoved) -> String {
    use std::fmt::Write as _;
    let mut out = format!(
        "removed [pipeline.{}] from {}\n",
        removed.pipeline,
        removed.config_path.display()
    );
    if !removed.routes_cleared.is_empty() {
        let matches: Vec<String> = removed
            .routes_cleared
            .iter()
            .map(|pattern| format!("{pattern:?}"))
            .collect();
        let _ = writeln!(
            out,
            "cleared `pipeline` on {} [[route]]: match = {}",
            removed.routes_cleared.len(),
            matches.join(", ")
        );
    }
    out
}

fn render_route_set(set: &RouteSet) -> String {
    format!(
        "{} [[route]] match = {:?} at position {} in {}\n",
        if set.created { "added" } else { "updated" },
        set.pattern,
        set.position,
        set.config_path.display()
    )
}

fn render_route_removed(removed: &RouteRemoved) -> String {
    format!(
        "removed [[route]] match = {:?} from {}\n",
        removed.pattern,
        removed.config_path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_options::options;
    use muxa::config::Config;
    use serde_json::{json, Value};
    use std::collections::BTreeSet;

    /// A hand-written file with a comment, an unrelated section before and
    /// after the pipeline, a route carrying a worktree and a prepare
    /// command, and a trailing comment on a value.
    const CONFIGURED: &str = r"# hand-written by the operator
[watch]
spinner = false

[[route]]
match = '^cal-'
workspace = 'callabo'
pipeline = 'triad' # the main line-up
prepare = 'mk-ws {{id}}'
[route.worktree]
repo = '~/workspace/callabo'

[[route]]
match = '.*'
pipeline = 'triad'

# the line-up
[pipeline.triad]
description = 'planner → implementer → reviewer'
layout = 'main-vertical'

[[pipeline.triad.agent]]
alias = 'plan'
program = 'codex'

[[pipeline.triad.agent]]
alias = 'impl'
program = 'codex'
after = ['plan']

[ticket]
agent = 'codex'
";

    fn configured() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, CONFIGURED).unwrap();
        (dir, path)
    }

    fn spec(value: Value) -> PipelineSpec {
        serde_json::from_value(value).unwrap()
    }

    fn pair() -> Value {
        json!({
            "description": "implementer → reviewer",
            "layout": null,
            "prompt": "You are working on {{work}}.",
            "agents": [
                {"alias": "impl", "program": "claude", "role": "implementer",
                 "task": "Implement", "prompt": "Own the change.", "direction": null, "after": []},
                {"alias": "review", "program": "codex", "role": "reviewer",
                 "task": null, "prompt": null, "direction": "down", "after": ["impl"]}
            ]
        })
    }

    fn route_patterns(config: &Config) -> Vec<&str> {
        config
            .route
            .iter()
            .map(|route| route.pattern.as_str())
            .collect()
    }

    fn header_order(text: &str) -> Vec<&str> {
        text.lines().filter(|line| line.starts_with('[')).collect()
    }

    fn keys(value: &Value) -> BTreeSet<&str> {
        value
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect()
    }

    #[test]
    fn set_adds_a_pipeline_after_the_last_one_and_keeps_the_rest() {
        let (_dir, path) = configured();
        let set = set_pipeline(&path, "pair", spec(pair())).unwrap();
        assert_eq!(
            set,
            PipelineSet {
                pipeline: "pair".into(),
                replaced: false,
                config_path: path.clone(),
                agents: 2,
            }
        );
        assert_eq!(
            keys(&serde_json::to_value(&set).unwrap()),
            BTreeSet::from(["pipeline", "replaced", "config_path"])
        );

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("# hand-written by the operator\n[watch]\nspinner = false\n"),
            "{text}"
        );
        assert!(
            text.contains("pipeline = 'triad' # the main line-up"),
            "{text}"
        );
        assert!(text.contains("# the line-up\n[pipeline.triad]"), "{text}");
        assert_eq!(
            header_order(&text),
            [
                "[watch]",
                "[[route]]",
                "[route.worktree]",
                "[[route]]",
                "[pipeline.triad]",
                "[[pipeline.triad.agent]]",
                "[[pipeline.triad.agent]]",
                "[pipeline.pair]",
                "[[pipeline.pair.agent]]",
                "[[pipeline.pair.agent]]",
                "[ticket]",
            ],
            "{text}"
        );

        let config = Config::load(&path).unwrap();
        assert!(!config.watch.spinner);
        assert_eq!(config.ticket.agent, "codex");
        assert_eq!(config.pipeline.len(), 2);
        let written = &config.pipeline["pair"];
        assert_eq!(
            written.description.as_deref(),
            Some("implementer → reviewer")
        );
        assert_eq!(written.layout, None);
        assert_eq!(
            written.prompt.as_deref(),
            Some("You are working on {{work}}.")
        );
        assert_eq!(written.agent.len(), 2);
        assert_eq!(written.agent[0].alias, "impl");
        assert_eq!(written.agent[0].task.as_deref(), Some("Implement"));
        assert_eq!(written.agent[0].prompt.as_deref(), Some("Own the change."));
        assert!(written.agent[0].after.is_empty());
        assert_eq!(written.agent[1].direction.as_deref(), Some("down"));
        assert_eq!(written.agent[1].after, ["impl"]);
        assert_eq!(written.agent[1].task, None);
        assert_eq!(
            render_set(&set),
            format!(
                "wrote [pipeline.pair] with 2 agents in {}\n",
                path.display()
            )
        );
    }

    #[test]
    fn set_replaces_a_pipeline_where_it_stands_with_its_comment() {
        let (_dir, path) = configured();
        let mut triad = pair();
        triad["description"] = json!("three now");
        triad["agents"]
            .as_array_mut()
            .unwrap()
            .push(json!({"alias": "ship", "program": "gemini", "after": ["review"]}));
        let set = set_pipeline(&path, "triad", spec(triad)).unwrap();
        assert!(set.replaced);
        assert_eq!(set.agents, 3);

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# the line-up\n[pipeline.triad]\n"), "{text}");
        assert_eq!(
            header_order(&text),
            [
                "[watch]",
                "[[route]]",
                "[route.worktree]",
                "[[route]]",
                "[pipeline.triad]",
                "[[pipeline.triad.agent]]",
                "[[pipeline.triad.agent]]",
                "[[pipeline.triad.agent]]",
                "[ticket]",
            ],
            "{text}"
        );
        let config = Config::load(&path).unwrap();
        let written = &config.pipeline["triad"];
        assert_eq!(written.description.as_deref(), Some("three now"));
        let aliases: Vec<&str> = written
            .agent
            .iter()
            .map(|agent| agent.alias.as_str())
            .collect();
        assert_eq!(aliases, ["impl", "review", "ship"]);
        assert_eq!(written.agent[2].program, "gemini");
        assert_eq!(written.agent[2].after, ["review"]);
        assert_eq!(config.route[0].pipeline.as_deref(), Some("triad"));
        assert!(config.route[0].worktree.is_some());
        assert!(render_set(&set).starts_with("replaced [pipeline.triad] with 3 agents"));
    }

    #[test]
    fn set_creates_a_missing_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        let set = set_pipeline(&path, "solo-1_a", spec(pair())).unwrap();
        assert!(!set.replaced);
        let config = Config::load(&path).unwrap();
        assert_eq!(config.pipeline["solo-1_a"].agent.len(), 2);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("[pipeline.solo-1_a]\n"), "{text}");
        assert!(!text.contains("\n[pipeline]\n"), "{text}");
    }

    #[test]
    fn set_refuses_bad_input_without_touching_the_file() {
        let (_dir, path) = configured();
        let before = std::fs::read_to_string(&path).unwrap();
        let refused = |name: &str, value: Value, expected: &str| {
            let error = set_pipeline(&path, name, spec(value)).unwrap_err();
            let text = format!("{error:#}");
            assert!(text.contains(expected), "{name}: {text}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), before, "{name}");
        };

        let mut bad_program = pair();
        bad_program["agents"][0]["program"] = json!("vim");
        refused("pair", bad_program, "not an allowlisted agent CLI");

        let mut duplicate = pair();
        duplicate["agents"][1]["alias"] = json!("IMPL");
        refused("pair", duplicate, "uses alias \"impl\" twice");

        let mut dangling = pair();
        dangling["agents"][1]["after"] = json!(["plan"]);
        refused("pair", dangling, "waits on \"plan\", which is not an alias");

        let mut cycle = pair();
        cycle["agents"][0]["after"] = json!(["review"]);
        refused("pair", cycle, "has a cycle");

        let mut empty = pair();
        empty["agents"] = json!([]);
        refused("pair", empty, "declares no agents");

        let mut blank_alias = pair();
        blank_alias["agents"][0]["alias"] = json!("  ");
        refused("pair", blank_alias, "agent #1 has no alias");

        let mut sideways = pair();
        sideways["agents"][1]["direction"] = json!("left");
        refused("pair", sideways, "unknown direction \"left\"");

        let mut renamed = pair();
        renamed["name"] = json!("other");
        refused("pair", renamed, "names pipeline \"other\"");

        refused("pair.two", pair(), "cannot be a [pipeline.<name>] key");
        refused("", pair(), "cannot be a [pipeline.<name>] key");
        refused("pa ir", pair(), "cannot be a [pipeline.<name>] key");

        let typo: Result<PipelineSpec, _> =
            serde_json::from_value(json!({"agent": [{"alias": "x", "program": "claude"}]}));
        assert!(typo
            .unwrap_err()
            .to_string()
            .contains("unknown field `agent`"));
    }

    #[test]
    fn set_tolerates_a_matching_name_and_missing_optionals() {
        let (_dir, path) = configured();
        let set = set_pipeline(
            &path,
            "pair",
            spec(json!({"name": "pair", "agents": [{"alias": "x", "program": "Claude "}]})),
        )
        .unwrap();
        assert!(!set.replaced);
        let config = Config::load(&path).unwrap();
        let written = &config.pipeline["pair"];
        assert_eq!(written.description, None);
        assert_eq!(written.agent[0].program, "Claude ");
        assert_eq!(written.agent[0].role, None);
        assert!(written.agent[0].after.is_empty());
    }

    #[test]
    fn remove_refuses_while_a_route_names_the_pipeline() {
        let (_dir, path) = configured();
        let before = std::fs::read_to_string(&path).unwrap();
        let error = remove_pipeline(&path, "triad", false).unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("--force"), "{text}");
        assert!(text.contains("\"^cal-\", \".*\""), "{text}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        let error = remove_pipeline(&path, "quartet", false).unwrap_err();
        assert!(format!("{error:#}").contains("not defined"), "{error:#}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        let removed = remove_pipeline(&path, "triad", true).unwrap();
        assert_eq!(
            removed,
            PipelineRemoved {
                pipeline: "triad".into(),
                removed: true,
                routes_cleared: vec!["^cal-".into(), ".*".into()],
                config_path: path.clone(),
            }
        );
        let json = serde_json::to_value(&removed).unwrap();
        assert_eq!(json["routes_cleared"], json!(["^cal-", ".*"]));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("pipeline"), "{text}");
        assert!(text.contains("# hand-written by the operator"), "{text}");
        assert!(text.contains("prepare = 'mk-ws {{id}}'"), "{text}");
        let config = Config::load(&path).unwrap();
        assert!(config.pipeline.is_empty());
        assert_eq!(route_patterns(&config), ["^cal-", ".*"]);
        assert!(config.route.iter().all(|route| route.pipeline.is_none()));
        assert_eq!(config.route[0].workspace.as_deref(), Some("callabo"));
        assert!(config.route[0].worktree.is_some());
        assert!(config.route[0].prepare.is_some());
        assert_eq!(config.ticket.agent, "codex");
        let human = render_removed(&removed);
        assert!(human.contains("removed [pipeline.triad]"), "{human}");
        assert!(
            human.contains("cleared `pipeline` on 2 [[route]]"),
            "{human}"
        );
    }

    #[test]
    fn remove_of_an_unreferenced_pipeline_needs_no_force() {
        let (_dir, path) = configured();
        set_pipeline(&path, "pair", spec(pair())).unwrap();
        let removed = remove_pipeline(&path, "pair", false).unwrap();
        assert!(removed.routes_cleared.is_empty());
        let config = Config::load(&path).unwrap();
        assert_eq!(config.pipeline.len(), 1);
        assert!(config.pipeline.contains_key("triad"));
        assert_eq!(config.route[0].pipeline.as_deref(), Some("triad"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("pair"), "{text}");
        assert!(text.ends_with("[ticket]\nagent = 'codex'\n"), "{text}");
        assert_eq!(
            render_removed(&removed),
            format!("removed [pipeline.pair] from {}\n", path.display())
        );
    }

    #[test]
    fn route_set_appends_beside_the_other_routes() {
        let (_dir, path) = configured();
        let set = set_route(
            &path,
            &RouteEdit {
                pattern: "^muxa-".into(),
                pipeline: Some("triad".into()),
                workspace: Some("muxa".into()),
                cwd: Some("~/personal/muxa".into()),
                ..RouteEdit::default()
            },
        )
        .unwrap();
        assert_eq!(
            set,
            RouteSet {
                pattern: "^muxa-".into(),
                position: 2,
                created: true,
                config_path: path.clone(),
            }
        );
        let json = serde_json::to_value(&set).unwrap();
        assert_eq!(
            keys(&json),
            BTreeSet::from(["match", "position", "created", "config_path"])
        );
        assert_eq!(json["match"], "^muxa-");

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            header_order(&text)[..5],
            [
                "[watch]",
                "[[route]]",
                "[route.worktree]",
                "[[route]]",
                "[[route]]"
            ],
            "{text}"
        );
        assert!(text.contains("# the line-up\n[pipeline.triad]"), "{text}");
        let config = Config::load(&path).unwrap();
        assert_eq!(route_patterns(&config), ["^cal-", ".*", "^muxa-"]);
        let route = &config.route[2];
        assert_eq!(route.pipeline.as_deref(), Some("triad"));
        assert_eq!(route.workspace.as_deref(), Some("muxa"));
        assert_eq!(route.cwd.as_deref(), Some("~/personal/muxa"));
        assert_eq!(
            render_route_set(&set),
            format!(
                "added [[route]] match = \"^muxa-\" at position 2 in {}\n",
                path.display()
            )
        );
    }

    #[test]
    fn route_set_at_a_position_moves_the_others_down() {
        let (_dir, path) = configured();
        let set = set_route(
            &path,
            &RouteEdit {
                pattern: "^hot-".into(),
                pipeline: Some("triad".into()),
                position: Some(0),
                ..RouteEdit::default()
            },
        )
        .unwrap();
        assert_eq!(set.position, 0);
        assert!(set.created);
        let config = Config::load(&path).unwrap();
        assert_eq!(route_patterns(&config), ["^hot-", "^cal-", ".*"]);
        assert!(config.route[1].worktree.is_some());
        assert_eq!(config.route[1].prepare.as_deref(), Some("mk-ws {{id}}"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            header_order(&text)[..5],
            [
                "[watch]",
                "[[route]]",
                "[[route]]",
                "[route.worktree]",
                "[[route]]"
            ],
            "{text}"
        );

        // Past the end lands last and says so.
        let set = set_route(
            &path,
            &RouteEdit {
                pattern: "^cold-".into(),
                position: Some(99),
                ..RouteEdit::default()
            },
        )
        .unwrap();
        assert_eq!(set.position, 3);
        let config = Config::load(&path).unwrap();
        assert_eq!(route_patterns(&config), ["^hot-", "^cal-", ".*", "^cold-"]);
        assert_eq!(config.route[3].pipeline, None);
    }

    #[test]
    fn route_set_updates_fields_and_keeps_worktree_prepare_and_comments() {
        let (_dir, path) = configured();
        set_pipeline(&path, "pair", spec(pair())).unwrap();
        let set = set_route(
            &path,
            &RouteEdit {
                pattern: "^cal-".into(),
                pipeline: Some("pair".into()),
                cwd: Some("/tmp/cal".into()),
                clear_workspace: true,
                ..RouteEdit::default()
            },
        )
        .unwrap();
        assert_eq!(set.position, 0);
        assert!(!set.created);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("pipeline = \"pair\" # the main line-up"),
            "{text}"
        );
        assert!(text.contains("prepare = 'mk-ws {{id}}'"), "{text}");
        assert!(!text.contains("workspace = 'callabo'"), "{text}");
        let config = Config::load(&path).unwrap();
        assert_eq!(route_patterns(&config), ["^cal-", ".*"]);
        let route = &config.route[0];
        assert_eq!(route.pipeline.as_deref(), Some("pair"));
        assert_eq!(route.cwd.as_deref(), Some("/tmp/cal"));
        assert_eq!(route.workspace, None);
        assert_eq!(
            route
                .worktree
                .as_ref()
                .map(|worktree| worktree.repo.as_str()),
            Some("~/workspace/callabo")
        );
        assert_eq!(route.prepare.as_deref(), Some("mk-ws {{id}}"));
        // The other route is untouched.
        assert_eq!(config.route[1].pipeline.as_deref(), Some("triad"));

        // Clearing the rest leaves only match, worktree, and prepare.
        let set = set_route(
            &path,
            &RouteEdit {
                pattern: "^cal-".into(),
                clear_pipeline: true,
                clear_cwd: true,
                ..RouteEdit::default()
            },
        )
        .unwrap();
        assert!(!set.created);
        let config = Config::load(&path).unwrap();
        let route = &config.route[0];
        assert_eq!(route.pipeline, None);
        assert_eq!(route.cwd, None);
        assert!(route.worktree.is_some());
        assert!(
            render_route_set(&set).starts_with("updated [[route]] match = \"^cal-\" at position 0")
        );
    }

    #[test]
    fn route_set_moves_an_existing_route() {
        let (_dir, path) = configured();
        let set = set_route(
            &path,
            &RouteEdit {
                pattern: ".*".into(),
                position: Some(0),
                ..RouteEdit::default()
            },
        )
        .unwrap();
        assert_eq!(set.position, 0);
        assert!(!set.created);
        let config = Config::load(&path).unwrap();
        assert_eq!(route_patterns(&config), [".*", "^cal-"]);
        assert_eq!(config.route[0].pipeline.as_deref(), Some("triad"));
        assert!(config.route[1].worktree.is_some());
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            header_order(&text)[..4],
            ["[watch]", "[[route]]", "[[route]]", "[route.worktree]"],
            "{text}"
        );
        assert!(text.contains("# the line-up\n[pipeline.triad]"), "{text}");

        // Same position again is a no-op move.
        let set = set_route(
            &path,
            &RouteEdit {
                pattern: ".*".into(),
                position: Some(0),
                ..RouteEdit::default()
            },
        )
        .unwrap();
        assert_eq!(set.position, 0);
        let config = Config::load(&path).unwrap();
        assert_eq!(route_patterns(&config), [".*", "^cal-"]);

        // And moving the one with a worktree carries the worktree along.
        let set = set_route(
            &path,
            &RouteEdit {
                pattern: "^cal-".into(),
                position: Some(0),
                ..RouteEdit::default()
            },
        )
        .unwrap();
        assert_eq!(set.position, 0);
        let config = Config::load(&path).unwrap();
        assert_eq!(route_patterns(&config), ["^cal-", ".*"]);
        assert!(config.route[0].worktree.is_some());
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            header_order(&text)[..4],
            ["[watch]", "[[route]]", "[route.worktree]", "[[route]]"],
            "{text}"
        );
    }

    #[test]
    fn route_set_refuses_a_bad_regex_or_unknown_pipeline_without_touching_the_file() {
        let (_dir, path) = configured();
        let before = std::fs::read_to_string(&path).unwrap();
        let error = set_route(
            &path,
            &RouteEdit {
                pattern: "(unclosed".into(),
                ..RouteEdit::default()
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("regex"), "{error:#}");
        let error = set_route(
            &path,
            &RouteEdit {
                pattern: "^x-".into(),
                pipeline: Some("quartet".into()),
                ..RouteEdit::default()
            },
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("\"quartet\" is not defined"),
            "{error:#}"
        );
        let error = set_route(
            &path,
            &RouteEdit {
                pattern: "  ".into(),
                ..RouteEdit::default()
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("--match"), "{error:#}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn route_remove_drops_only_that_route() {
        let (_dir, path) = configured();
        let before = std::fs::read_to_string(&path).unwrap();
        let error = remove_route(&path, "^nope-").unwrap_err();
        assert!(format!("{error:#}").contains("no [[route]]"), "{error:#}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        let removed = remove_route(&path, ".*").unwrap();
        assert_eq!(
            removed,
            RouteRemoved {
                pattern: ".*".into(),
                removed: true,
                config_path: path.clone(),
            }
        );
        assert_eq!(serde_json::to_value(&removed).unwrap()["match"], ".*");
        let config = Config::load(&path).unwrap();
        assert_eq!(route_patterns(&config), ["^cal-"]);
        assert!(config.route[0].worktree.is_some());
        assert!(config.pipeline.contains_key("triad"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# the line-up\n[pipeline.triad]"), "{text}");
        assert_eq!(
            render_route_removed(&removed),
            format!("removed [[route]] match = \".*\" from {}\n", path.display())
        );

        let removed = remove_route(&path, "^cal-").unwrap();
        assert!(removed.removed);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("route"), "{text}");
        assert!(Config::load(&path).unwrap().route.is_empty());
    }

    #[test]
    fn options_json_round_trips_through_set() {
        let (_dir, path) = configured();
        // A pipeline with every field set, written the way an operator would.
        std::fs::write(
            &path,
            format!(
                "{CONFIGURED}
[pipeline.full]
description = 'everything set'
layout = 'tiled'
prompt = '''
You are working on {{{{work}}}}.
Second line.'''

[[pipeline.full.agent]]
alias = 'plan'
program = 'codex'
role = 'planner'
task = 'Plan it'
prompt = 'Plan, then run `muxa work done`.'

[[pipeline.full.agent]]
alias = 'impl'
program = 'claude'
role = 'implementer'
task = 'Build it'
prompt = 'Build {{{{request}}}}'
direction = 'down'
after = ['plan']

[[pipeline.full.agent]]
alias = 'review'
program = 'gemini'
direction = 'right'
after = ['impl', 'plan']
"
            ),
        )
        .unwrap();
        let before = Config::load(&path).unwrap();
        let printed = serde_json::to_value(options(&before, &path)).unwrap();
        let full = printed["pipelines"]
            .as_array()
            .unwrap()
            .iter()
            .find(|pipeline| pipeline["name"] == "full")
            .cloned()
            .unwrap();
        assert_eq!(
            keys(&full),
            BTreeSet::from(["name", "description", "layout", "prompt", "agents"])
        );
        assert_eq!(
            full["agents"][0]["prompt"],
            "Plan, then run `muxa work done`."
        );
        assert_eq!(full["agents"][2]["prompt"], Value::Null);

        // Feed the printed entry straight back, `name` and all.
        let set = set_pipeline(&path, "full", spec(full.clone())).unwrap();
        assert!(set.replaced);
        let after = Config::load(&path).unwrap();
        assert_eq!(after.pipeline["full"], before.pipeline["full"]);
        assert_eq!(after.route, before.route);
        assert_eq!(after.pipeline["triad"], before.pipeline["triad"]);
        assert_eq!(
            serde_json::to_value(options(&after, &path)).unwrap(),
            printed
        );

        // And the shape `set` accepts is the shape `options` printed.
        let mut without_name = full;
        without_name.as_object_mut().unwrap().remove("name");
        let set = set_pipeline(&path, "copy", spec(without_name)).unwrap();
        assert!(!set.replaced);
        let copied = Config::load(&path).unwrap();
        assert_eq!(copied.pipeline["copy"], before.pipeline["full"]);
    }
}
