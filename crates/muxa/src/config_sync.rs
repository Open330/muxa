//! Portable settings: carry the shareable part of `config.toml` from one
//! host to another, the way editor and dotfile sync tools do.
//!
//! The design follows established practice rather than inventing a model:
//!
//! - **Categories and machine scope** (VS Code Settings Sync): a bundle
//!   carries named categories, and settings that only make sense on one
//!   machine are never exported or overwritten. What is portable is an
//!   explicit allowlist, checked on export *and* on apply, so a hand-edited
//!   bundle cannot smuggle in a dashboard token or a local path.
//! - **A user ignore list** (`settingsSync.ignoredSettings`): `[config_sync]
//!   ignore` names items or whole categories this host keeps to itself.
//! - **Three-way apply** (`kubectl apply`): each host remembers what it last
//!   applied from a source. An item changes only where the local copy still
//!   matches that base; a local edit survives an unchanged upstream, an
//!   upstream deletion removes only what sync put there, and a local edit
//!   against an upstream change is a conflict, never silently resolved.
//! - **Preview first** (`terraform plan`, `chezmoi diff`): the plan is
//!   computed and shown before anything is written.
//!
//! This module is pure: it reads and edits TOML text. Reading the daemon's
//! file, persisting the base and writing through the daemon live in the CLI.
//!
//! Nothing here knows about account switchers or any other host tooling.
//! Settings whose meaning depends on such tooling (`[agent.*].account`,
//! `swap_account` rules) stay on the host that has it.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use toml_edit::{DocumentMut, Item, Table};

/// The bundle format this build reads and writes.
pub const SCHEMA_VERSION: i64 = 1;

/// Bundle keys that describe the bundle rather than carry settings.
const HEADER_KEYS: &[&str] = &["schema_version", "source", "source_label", "categories"];

/// Providers whose `[agent.<program>].options` a bundle may carry.
const AGENT_PROGRAMS: &[&str] = &["claude", "codex", "gemini", "agy", "opencode"];

/// `[watch]` keys that are preferences rather than per-machine state. The
/// composer's last kind and mode are written by the TUI as you use it.
const WATCH_KEYS: &[&str] = &[
    "theme",
    "columns",
    "widths",
    "inspector_split",
    "view",
    "layout",
    "collab_layout",
    "screen",
    "tree_expansion",
    "detail",
    "sort",
    "summary",
    "hide_paneless",
    "spinner",
    "preview",
];
const WATCH_LOCAL: &[&str] = &["collaboration_kind", "collaboration_mode"];

/// `[collaboration]` keys; `path` is where this host keeps its mailbox.
const COLLABORATION_KEYS: &[&str] = &[
    "enabled",
    "auto_spawn_workers",
    "worker_bypass_permissions",
    "wake",
    "wake_payload",
    "scope",
    "max_message_bytes",
    "retention_days",
];
const COLLABORATION_LOCAL: &[&str] = &["path"];

/// `[ticket]` scalars; `source.<name>` entries travel whole. The working
/// directory and extra roots are paths on this machine.
const TICKET_KEYS: &[&str] = &["agent", "permission_mode", "timeout_secs", "cache_secs"];
const TICKET_LOCAL: &[&str] = &["cwd", "additional_dirs"];

/// One category of portable settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Category {
    /// `[pipeline.<name>]`, each pipeline whole.
    Pipeline,
    /// `[message.skills]`, each skill whole.
    Skills,
    /// `[automation].enabled` and each `[[automation.rule]]` by name.
    Automation,
    /// `[agent.<program>].options`.
    Agent,
    /// `[watch]` preferences.
    Watch,
    /// `[collaboration]` policy.
    Collaboration,
    /// `[ticket]` settings and `[ticket.source.<name>]` entries.
    Ticket,
}

impl Category {
    pub const ALL: [Category; 7] = [
        Category::Pipeline,
        Category::Skills,
        Category::Automation,
        Category::Agent,
        Category::Watch,
        Category::Collaboration,
        Category::Ticket,
    ];

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Category::Pipeline => "pipeline",
            Category::Skills => "skills",
            Category::Automation => "automation",
            Category::Agent => "agent",
            Category::Watch => "watch",
            Category::Collaboration => "collaboration",
            Category::Ticket => "ticket",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, String> {
        Category::ALL
            .into_iter()
            .find(|category| category.name() == raw.trim())
            .ok_or_else(|| {
                format!(
                    "unknown category {raw:?}; expected one of {}",
                    Category::ALL.map(Category::name).join(", ")
                )
            })
    }

    /// The category a unit belongs to, read off its path.
    fn of(path: &[String]) -> Option<Self> {
        match path.first().map(String::as_str)? {
            "pipeline" => Some(Category::Pipeline),
            "message" => Some(Category::Skills),
            "automation" => Some(Category::Automation),
            "agent" => Some(Category::Agent),
            "watch" => Some(Category::Watch),
            "collaboration" => Some(Category::Collaboration),
            "ticket" => Some(Category::Ticket),
            _ => None,
        }
    }
}

/// `[config_sync]` in `config.toml`: this host's own sync choices. Machine
/// scope by definition, so it is never exported.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConfigSyncConfig {
    /// Categories this host takes part in. Unset means all of them.
    pub categories: Option<Vec<String>>,
    /// Items or whole categories this host keeps to itself, as dotted
    /// paths: `"pipeline.solo"`, `"automation.rule.confirm-yes"`,
    /// `"ticket"`. Quote a name containing a dot: `'pipeline."a.b"'`.
    pub ignore: Vec<String>,
}

impl ConfigSyncConfig {
    pub fn validate(&self) -> Result<(), String> {
        for category in self.categories.iter().flatten() {
            Category::parse(category).map_err(|e| format!("config_sync.categories: {e}"))?;
        }
        for pattern in &self.ignore {
            let path = parse_path(pattern).map_err(|e| format!("config_sync.ignore: {e}"))?;
            if Category::of(&path).is_none() && Category::parse(&path[0]).is_err() {
                return Err(format!(
                    "config_sync.ignore: {pattern:?} names no portable setting"
                ));
            }
        }
        Ok(())
    }

    /// The categories this host takes part in.
    pub fn selected(&self) -> Result<BTreeSet<Category>, String> {
        match &self.categories {
            None => Ok(Category::ALL.into_iter().collect()),
            Some(names) => names.iter().map(|name| Category::parse(name)).collect(),
        }
    }

    fn ignore_paths(&self) -> Result<Vec<Vec<String>>, String> {
        self.ignore
            .iter()
            .map(|pattern| {
                let mut path = parse_path(pattern)?;
                // `skills` is the category's name; its items live under
                // `message.skills`.
                if path[0] == "skills" {
                    path.splice(0..1, ["message".to_owned(), "skills".to_owned()]);
                }
                Ok(path)
            })
            .collect()
    }
}

/// Split a dotted path, honouring double-quoted segments.
fn parse_path(raw: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut chars = raw.trim().chars().peekable();
    loop {
        let mut segment = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            loop {
                match chars.next() {
                    Some('"') => break,
                    Some(c) => segment.push(c),
                    None => return Err(format!("unterminated quote in {raw:?}")),
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == '.' {
                    break;
                }
                segment.push(c);
                chars.next();
            }
        }
        if segment.is_empty() {
            return Err(format!("empty segment in {raw:?}"));
        }
        out.push(segment);
        match chars.next() {
            None => return Ok(out),
            Some('.') => {}
            Some(c) => return Err(format!("unexpected {c:?} in {raw:?}")),
        }
    }
}

/// A dotted path, quoting any segment that is not a bare TOML key.
#[must_use]
pub fn display_path(path: &[String]) -> String {
    path.iter()
        .map(|segment| {
            if !segment.is_empty()
                && segment
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                segment.clone()
            } else {
                format!("{segment:?}")
            }
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// One mergeable setting: a path and its value as written.
#[derive(Debug, Clone)]
pub struct Unit {
    pub path: Vec<String>,
    item: Item,
    value: toml::Value,
}

impl Unit {
    fn new(path: Vec<String>, item: Item) -> Result<Self, String> {
        let value = canonical(&item)
            .ok_or_else(|| format!("{} has an unreadable value", display_path(&path)))?;
        Ok(Self { path, item, value })
    }

    #[must_use]
    pub fn key(&self) -> String {
        display_path(&self.path)
    }

    #[must_use]
    pub fn value(&self) -> &toml::Value {
        &self.value
    }
}

/// The value an item holds, independent of how it is formatted.
fn canonical(item: &Item) -> Option<toml::Value> {
    let mut document = DocumentMut::new();
    let item = match item {
        // A bare value under a fresh key renders as `v = …`.
        Item::Value(value) => Item::Value(value.clone()),
        Item::Table(table) => Item::Table(table.clone()),
        Item::ArrayOfTables(array) => Item::ArrayOfTables(array.clone()),
        Item::None => return None,
    };
    document.insert("v", item);
    let mut parsed: toml::Table = toml::from_str(&document.to_string()).ok()?;
    parsed.remove("v")
}

/// An item left out of a bundle on purpose, with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Withheld {
    pub path: String,
    pub reason: String,
}

/// The portable units of a document, in document order.
#[derive(Debug, Clone, Default)]
pub struct Units {
    units: Vec<Unit>,
    pub withheld: Vec<Withheld>,
}

impl Units {
    fn push(&mut self, path: Vec<String>, item: &Item) -> Result<(), String> {
        let key = display_path(&path);
        if self.units.iter().any(|unit| unit.key() == key) {
            return Err(format!("{key} appears twice"));
        }
        self.units.push(Unit::new(path, item.clone())?);
        Ok(())
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Unit> {
        self.units.iter().find(|unit| unit.key() == key)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Unit> {
        self.units.iter()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.units.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.units.is_empty()
    }
}

fn table_like(item: Option<&Item>) -> Option<&dyn toml_edit::TableLike> {
    item.and_then(Item::as_table_like)
}

fn owned(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| (*p).to_owned()).collect()
}

/// Walks one document, collecting units for the selected categories.
struct Extractor<'a> {
    categories: &'a BTreeSet<Category>,
    strict: bool,
    units: Units,
}

impl Extractor<'_> {
    fn wants(&self, category: Category) -> bool {
        self.categories.contains(&category)
    }

    /// Something outside the allowlist: an error in a bundle, skipped in a
    /// host's own config.
    fn reject(&self, path: &str, why: &str) -> Result<(), String> {
        if self.strict {
            Err(format!("{path} {why}"))
        } else {
            Ok(())
        }
    }

    fn pipelines(&mut self, root: &Table) -> Result<(), String> {
        let Some(pipelines) = table_like(root.get("pipeline")) else {
            return Ok(());
        };
        if self.wants(Category::Pipeline) {
            for (name, item) in pipelines.iter() {
                self.units.push(owned(&["pipeline", name]), item)?;
            }
        }
        Ok(())
    }

    fn skills(&mut self, root: &Table) -> Result<(), String> {
        let Some(message) = table_like(root.get("message")) else {
            return Ok(());
        };
        for (key, item) in message.iter() {
            if key != "skills" {
                self.reject(&format!("message.{key}"), "is not portable")?;
                continue;
            }
            let Some(skills) = item.as_table_like() else {
                return Err("message.skills must be a table".into());
            };
            if self.wants(Category::Skills) {
                for (name, skill) in skills.iter() {
                    self.units
                        .push(owned(&["message", "skills", name]), skill)?;
                }
            }
        }
        Ok(())
    }

    fn automation(&mut self, root: &Table) -> Result<(), String> {
        let Some(automation) = table_like(root.get("automation")) else {
            return Ok(());
        };
        let wanted = self.wants(Category::Automation);
        for (key, item) in automation.iter() {
            match key {
                "enabled" if wanted => {
                    self.units.push(owned(&["automation", "enabled"]), item)?;
                }
                "rule" if wanted => {
                    for rule in rules(item)? {
                        self.rule(rule)?;
                    }
                }
                "enabled" | "rule" => {}
                other => self.reject(&format!("automation.{other}"), "is machine scope")?,
            }
        }
        Ok(())
    }

    fn rule(&mut self, rule: Table) -> Result<(), String> {
        let name = rule
            .get("name")
            .and_then(|n| n.as_str())
            .ok_or("every automation rule needs a name to be shared")?
            .to_owned();
        let path = owned(&["automation", "rule", &name]);
        // An account switch only means something on a host with the tool
        // that performs it. Stripping the account would change what the
        // rule does, so the rule stays home whole.
        if rule.get("action").and_then(|a| a.as_str()) == Some("swap_account")
            || rule.contains_key("account")
        {
            if self.strict {
                return Err(format!(
                    "{} switches accounts, which stays on the host that can",
                    display_path(&path)
                ));
            }
            self.units.withheld.push(Withheld {
                path: display_path(&path),
                reason: "switches accounts; depends on host tooling".into(),
            });
            return Ok(());
        }
        self.units.push(path, &Item::Table(rule))
    }

    fn agents(&mut self, root: &Table) -> Result<(), String> {
        let Some(agents) = table_like(root.get("agent")) else {
            return Ok(());
        };
        for (program, item) in agents.iter() {
            if !AGENT_PROGRAMS.contains(&program) {
                return Err(format!("agent.{program} is not a known provider"));
            }
            let Some(settings) = item.as_table_like() else {
                return Err(format!("agent.{program} must be a table"));
            };
            for (key, value) in settings.iter() {
                if key != "options" {
                    self.reject(&format!("agent.{program}.{key}"), "is machine scope")?;
                } else if self.wants(Category::Agent) {
                    self.units
                        .push(owned(&["agent", program, "options"]), value)?;
                }
            }
        }
        Ok(())
    }

    /// A table whose keys are each portable or machine scope.
    fn keyed(
        &mut self,
        root: &Table,
        table: &str,
        category: Category,
        portable: &[&str],
        local: &[&str],
    ) -> Result<(), String> {
        let Some(settings) = table_like(root.get(table)) else {
            return Ok(());
        };
        for (key, item) in settings.iter() {
            if table == "ticket" && key == "source" {
                let Some(sources) = item.as_table_like() else {
                    return Err("ticket.source must be a table".into());
                };
                if self.wants(category) {
                    for (name, source) in sources.iter() {
                        self.units
                            .push(owned(&["ticket", "source", name]), source)?;
                    }
                }
            } else if portable.contains(&key) {
                if self.wants(category) {
                    self.units.push(owned(&[table, key]), item)?;
                }
            } else if local.contains(&key) {
                self.reject(&format!("{table}.{key}"), "is machine scope")?;
            } else {
                // Unknown to this build: neither portable nor known-local.
                self.reject(&format!("{table}.{key}"), "is not a known portable setting")?;
            }
        }
        Ok(())
    }
}

/// Read the portable units of `document` for `categories`, leaving out
/// ignored paths. `strict` rejects anything outside the allowlist instead of
/// skipping it: a bundle must not carry what export would never write.
pub fn extract(
    document: &DocumentMut,
    categories: &BTreeSet<Category>,
    sync: &ConfigSyncConfig,
    strict: bool,
) -> Result<Units, String> {
    let ignored = sync.ignore_paths()?;
    let root = document.as_table();
    let mut walk = Extractor {
        categories,
        strict,
        units: Units::default(),
    };
    walk.pipelines(root)?;
    walk.skills(root)?;
    walk.automation(root)?;
    walk.agents(root)?;
    walk.keyed(root, "watch", Category::Watch, WATCH_KEYS, WATCH_LOCAL)?;
    walk.keyed(
        root,
        "collaboration",
        Category::Collaboration,
        COLLABORATION_KEYS,
        COLLABORATION_LOCAL,
    )?;
    walk.keyed(root, "ticket", Category::Ticket, TICKET_KEYS, TICKET_LOCAL)?;
    let mut units = walk.units;
    units
        .units
        .retain(|unit| !ignored.iter().any(|pattern| unit.path.starts_with(pattern)));
    Ok(units)
}

/// Each rule of `automation.rule`, written as tables or inline tables.
fn rules(item: &Item) -> Result<Vec<Table>, String> {
    match item {
        Item::ArrayOfTables(array) => Ok(array.iter().cloned().collect()),
        Item::Value(toml_edit::Value::Array(array)) => array
            .iter()
            .map(|value| {
                value
                    .as_inline_table()
                    .map(|inline| inline.clone().into_table())
                    .ok_or_else(|| "automation.rule entries must be tables".to_owned())
            })
            .collect(),
        _ => Err("automation.rule must be an array of tables".into()),
    }
}

/// A parsed bundle.
#[derive(Debug, Clone)]
pub struct Bundle {
    pub source: String,
    pub source_label: Option<String>,
    pub categories: BTreeSet<Category>,
    pub units: Units,
}

impl Bundle {
    /// Read a bundle, rejecting unknown versions, header keys, and anything
    /// outside the portable allowlist.
    pub fn parse(text: &str) -> Result<Self, String> {
        let document: DocumentMut = text
            .parse()
            .map_err(|e| format!("not a TOML bundle: {e}"))?;
        let root = document.as_table();
        let version = root
            .get("schema_version")
            .and_then(Item::as_integer)
            .ok_or("bundle has no schema_version")?;
        if version != SCHEMA_VERSION {
            return Err(format!(
                "bundle schema_version {version} is not supported (this muxa reads {SCHEMA_VERSION})"
            ));
        }
        let source = root
            .get("source")
            .and_then(Item::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or("bundle has no source")?
            .to_owned();
        let source_label = root
            .get("source_label")
            .and_then(Item::as_str)
            .map(str::to_owned);
        let categories = root
            .get("categories")
            .and_then(Item::as_array)
            .ok_or("bundle has no categories list")?
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .ok_or_else(|| "categories must be strings".to_owned())
                    .and_then(Category::parse)
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        for (key, _) in root {
            if HEADER_KEYS.contains(&key) {
                continue;
            }
            let settles = [
                "pipeline",
                "message",
                "automation",
                "agent",
                "watch",
                "collaboration",
                "ticket",
            ];
            if !settles.contains(&key) {
                return Err(format!("{key} is not portable and cannot be in a bundle"));
            }
        }
        let mut body = document.clone();
        for key in HEADER_KEYS {
            body.remove(key);
        }
        // Settings for a category the bundle does not declare would be
        // silently ignored; refuse them so the manifest stays truthful.
        let all: BTreeSet<Category> = Category::ALL.into_iter().collect();
        let units = extract(&body, &all, &ConfigSyncConfig::default(), true)?;
        if let Some(stray) = units
            .iter()
            .find(|unit| Category::of(&unit.path).is_some_and(|c| !categories.contains(&c)))
        {
            return Err(format!(
                "{} belongs to a category the bundle does not list",
                stray.key()
            ));
        }
        Ok(Self {
            source,
            source_label,
            categories,
            units,
        })
    }
}

/// Write a bundle of `units` for `categories`.
#[must_use]
pub fn render_bundle(
    source: &str,
    source_label: Option<&str>,
    categories: &BTreeSet<Category>,
    units: &Units,
) -> String {
    let mut document = DocumentMut::new();
    document.insert("schema_version", toml_edit::value(SCHEMA_VERSION));
    document.insert("source", toml_edit::value(source));
    if let Some(label) = source_label {
        document.insert("source_label", toml_edit::value(label));
    }
    let mut names = toml_edit::Array::new();
    for category in categories {
        names.push(category.name());
    }
    document.insert("categories", toml_edit::value(names));
    for unit in units.iter() {
        set_unit(&mut document, &unit.path, unit.item.clone());
    }
    let mut text = String::from(
        "# muxa portable settings. Apply on another host with:\n\
         #   muxa config diff <this file>   (preview)\n\
         #   muxa config apply <this file>\n\n",
    );
    text.push_str(&document.to_string());
    text
}

/// The base a host recorded the last time it applied from a source.
pub type Base = BTreeMap<String, toml::Value>;

/// What applying one unit does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Add,
    Update,
    Remove,
    /// First sync from this source over a different local value.
    Replace,
}

#[derive(Debug, Clone)]
pub struct Step {
    pub key: String,
    pub change: Change,
    pub path: Vec<String>,
    pub before: Option<toml::Value>,
    after: Option<Item>,
    pub after_value: Option<toml::Value>,
}

/// A unit changed both here and upstream since the last apply.
#[derive(Debug, Clone, PartialEq)]
pub struct Conflict {
    pub key: String,
    pub base: Option<toml::Value>,
    pub local: Option<toml::Value>,
    pub incoming: Option<toml::Value>,
}

/// How to settle conflicts when the operator chooses to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prefer {
    Local,
    Incoming,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub steps: Vec<Step>,
    pub conflicts: Vec<Conflict>,
    /// The base to record once the plan is applied.
    pub base: Base,
    /// Categories this apply manages: the bundle's, narrowed by the host's.
    pub categories: BTreeSet<Category>,
}

impl Plan {
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.steps.is_empty() && self.conflicts.is_empty()
    }
}

/// Plan a three-way apply of `bundle` onto `local`.
///
/// `base` is what this host last applied from the same source; `None` means
/// it never has, in which case nothing local is deleted and differing local
/// values are listed as replacements. The recorded base is the incoming
/// projection, never the merged result, so local-only values are not
/// claimed by sync.
pub fn plan(
    local_text: &str,
    bundle: &Bundle,
    base: Option<&Base>,
    sync: &ConfigSyncConfig,
    prefer: Option<Prefer>,
) -> Result<Plan, String> {
    let document: DocumentMut = local_text
        .parse()
        .map_err(|e| format!("config.toml does not parse: {e}"))?;
    let selected = sync.selected()?;
    let categories: BTreeSet<Category> =
        bundle.categories.intersection(&selected).copied().collect();
    let local = extract(&document, &categories, sync, false)?;
    let ignored = sync.ignore_paths()?;
    let managed = |path: &[String]| {
        Category::of(path).is_some_and(|c| categories.contains(&c))
            && !ignored.iter().any(|pattern| path.starts_with(pattern))
    };
    let incoming: Vec<&Unit> = bundle.units.iter().filter(|u| managed(&u.path)).collect();

    let mut keys: Vec<(String, Vec<String>)> = Vec::new();
    let mut seen = BTreeSet::new();
    for unit in local.iter().chain(incoming.iter().copied()) {
        if seen.insert(unit.key()) {
            keys.push((unit.key(), unit.path.clone()));
        }
    }
    // A base entry neither side still has needs no step, only forgetting.

    let mut steps = Vec::new();
    let mut conflicts = Vec::new();
    for (key, path) in keys {
        let l = local.get(&key);
        let r = incoming.iter().find(|unit| unit.key() == key).copied();
        let lv = l.map(Unit::value);
        let rv = r.map(Unit::value);
        if lv == rv {
            continue;
        }
        let take = |change: Change| Step {
            key: key.clone(),
            change,
            path: path.clone(),
            before: lv.cloned(),
            after: r.map(|unit| unit.item.clone()),
            after_value: rv.cloned(),
        };
        let Some(base) = base else {
            match (lv, rv) {
                (None, Some(_)) => steps.push(take(Change::Add)),
                (Some(_), Some(_)) => steps.push(take(Change::Replace)),
                // First sync never deletes what this host already had.
                (_, None) => {}
            }
            continue;
        };
        let bv = base.get(&key);
        let apply_incoming = || match (lv, rv) {
            (None, Some(_)) => Some(Change::Add),
            (Some(_), Some(_)) => Some(Change::Update),
            (Some(_), None) => Some(Change::Remove),
            (None, None) => None,
        };
        if lv == bv {
            if let Some(change) = apply_incoming() {
                steps.push(take(change));
            }
        } else if rv == bv {
            // Changed only here: keep it.
        } else {
            match prefer {
                Some(Prefer::Incoming) => {
                    if let Some(change) = apply_incoming() {
                        steps.push(take(change));
                    }
                }
                Some(Prefer::Local) => {}
                None => conflicts.push(Conflict {
                    key: key.clone(),
                    base: bv.cloned(),
                    local: lv.cloned(),
                    incoming: rv.cloned(),
                }),
            }
        }
    }

    // Managed, non-ignored entries take the incoming projection; anything
    // else keeps whatever the old base said, so narrowing the categories or
    // ignoring an item suspends ownership instead of forgetting it.
    let mut next: Base = base
        .into_iter()
        .flatten()
        .filter(|(key, _)| parse_path(key).map_or(true, |path| !managed(&path)))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for unit in incoming {
        next.insert(unit.key(), unit.value.clone());
    }

    Ok(Plan {
        steps,
        conflicts,
        base: next,
        categories,
    })
}

/// Apply `plan` to `local_text`, editing only the planned units so the rest
/// of the file, comments included, stays as it was.
pub fn apply(local_text: &str, plan: &Plan) -> Result<String, String> {
    if !plan.conflicts.is_empty() {
        return Err("refusing to apply a plan with conflicts".into());
    }
    let mut document: DocumentMut = local_text
        .parse()
        .map_err(|e| format!("config.toml does not parse: {e}"))?;
    for step in &plan.steps {
        match &step.after {
            Some(item) => set_unit(&mut document, &step.path, item.clone()),
            None => remove_unit(&mut document, &step.path),
        }
    }
    Ok(document.to_string())
}

/// The table at `path`, created as an implicit table when absent.
fn table_at<'a>(
    document: &'a mut DocumentMut,
    path: &[String],
) -> &'a mut dyn toml_edit::TableLike {
    let mut table: &mut dyn toml_edit::TableLike = document.as_table_mut();
    for segment in path {
        if !table.contains_key(segment) {
            let mut fresh = Table::new();
            fresh.set_implicit(true);
            table.insert(segment, Item::Table(fresh));
        }
        table = table
            .get_mut(segment)
            .and_then(Item::as_table_like_mut)
            .expect("portable parents are tables");
    }
    table
}

/// Drop the positions an item carried from the document it was read from,
/// so its tables render where they are inserted rather than interleaved by
/// another file's ordering.
fn without_positions(item: Item) -> Item {
    // A fresh table has no position; copy keys with their own decor.
    fn table(table: Table) -> Table {
        let mut fresh = Table::new();
        fresh.set_implicit(table.is_implicit());
        fresh.set_dotted(table.is_dotted());
        *fresh.decor_mut() = table.decor().clone();
        for (key, _) in &table {
            if let Some((key, item)) = table.get_key_value(key) {
                fresh.insert_formatted(key, without_positions(item.clone()));
            }
        }
        fresh
    }
    match item {
        Item::Table(t) => Item::Table(table(t)),
        Item::ArrayOfTables(array) => {
            let mut out = toml_edit::ArrayOfTables::new();
            for t in array {
                out.push(table(t));
            }
            Item::ArrayOfTables(out)
        }
        other => other,
    }
}

fn set_unit(document: &mut DocumentMut, path: &[String], item: Item) {
    let item = without_positions(item);
    if let [first, second, name] = path {
        if first == "automation" && second == "rule" {
            set_rule(document, name, item);
            return;
        }
    }
    let (last, parents) = path.split_last().expect("unit paths are not empty");
    // An inline parent converts the item to an inline value on insert.
    let table = table_at(document, parents);
    match (table.get_mut(last), item) {
        // Replacing a value keeps the comments around the old one.
        (Some(Item::Value(old)), Item::Value(mut new)) => {
            *new.decor_mut() = old.decor().clone();
            *old = new;
        }
        (_, item) => {
            table.insert(last, item);
        }
    }
}

fn remove_unit(document: &mut DocumentMut, path: &[String]) {
    if let [first, second, name] = path {
        if first == "automation" && second == "rule" {
            remove_rule(document, name);
            return;
        }
    }
    let (last, parents) = path.split_last().expect("unit paths are not empty");
    let mut table: &mut dyn toml_edit::TableLike = document.as_table_mut();
    for segment in parents {
        match table.get_mut(segment).and_then(Item::as_table_like_mut) {
            Some(next) => table = next,
            None => return,
        }
    }
    table.remove(last);
}

fn is_rule(table: &dyn toml_edit::TableLike, name: &str) -> bool {
    table.get("name").and_then(|n| n.as_str()) == Some(name)
}

fn set_rule(document: &mut DocumentMut, name: &str, item: Item) {
    let Item::Table(rule) = item else { return };
    let automation = table_at(document, &["automation".to_owned()]);
    match automation.get_mut("rule") {
        Some(Item::ArrayOfTables(array)) => {
            let at = array.iter().position(|t| is_rule(t, name));
            match at {
                Some(index) => {
                    *array.get_mut(index).expect("position is in range") = rule;
                }
                None => array.push(rule),
            }
        }
        Some(Item::Value(toml_edit::Value::Array(array))) => {
            let inline = toml_edit::Value::InlineTable(rule.into_inline_table());
            let at = array
                .iter()
                .position(|v| v.as_inline_table().is_some_and(|t| is_rule(t, name)));
            match at {
                Some(index) => {
                    array.replace(index, inline);
                }
                None => array.push(inline),
            }
        }
        _ => {
            let mut array = toml_edit::ArrayOfTables::new();
            array.push(rule);
            automation.insert("rule", Item::ArrayOfTables(array));
        }
    }
}

fn remove_rule(document: &mut DocumentMut, name: &str) {
    let Some(automation) = document
        .get_mut("automation")
        .and_then(Item::as_table_like_mut)
    else {
        return;
    };
    match automation.get_mut("rule") {
        Some(Item::ArrayOfTables(array)) => {
            let at = array.iter().position(|t| is_rule(t, name));
            if let Some(index) = at {
                array.remove(index);
            }
        }
        Some(Item::Value(toml_edit::Value::Array(array))) => {
            let at = array
                .iter()
                .position(|v| v.as_inline_table().is_some_and(|t| is_rule(t, name)));
            if let Some(index) = at {
                array.remove(index);
            }
        }
        _ => {}
    }
}

/// Checks the merged document must pass beyond `Config::validate`: a route
/// is local, but the pipeline it names may have just been removed by sync.
pub fn check_references(config: &crate::Config) -> Result<(), String> {
    for (index, route) in config.route.iter().enumerate() {
        if let Some(pipeline) = &route.pipeline {
            if !config.pipeline.contains_key(pipeline) {
                return Err(format!(
                    "route[{index}] names pipeline {pipeline:?}, which this apply would leave undefined"
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST_A: &str = r#"# top comment
[dashboard]
bind = "0.0.0.0:7878"
token = "secret"

[watch]
theme = "classic"
collaboration_kind = "task"

[collaboration]
enabled = true
path = "/home/a/collab.json"

[ticket]
agent = "claude"
cwd = "~"
timeout_secs = 300

[ticket.source.linear]
match = '^cal-\d+$'
prompt = "resolve {{id}}"

[message.skills]
review = "please review"

[pipeline.pair]
layout = "main-vertical" # keep this comment

[[pipeline.pair.agent]]
alias = "impl"
program = "claude"

[[automation.rule]]
name = "resume"
on = "rate_limited"
action = "send_prompt"
text = "continue"

[[automation.rule]]
name = "swap"
on = "rate_limited"
action = "swap_account"

[agent.claude]
options = ["--model", "x"]
account = "auto"
"#;

    fn all() -> BTreeSet<Category> {
        Category::ALL.into_iter().collect()
    }

    fn export(text: &str, sync: &ConfigSyncConfig) -> String {
        let document: DocumentMut = text.parse().unwrap();
        let units = extract(&document, &all(), sync, false).unwrap();
        render_bundle("node-a", Some("host-a"), &all(), &units)
    }

    fn keys(units: &Units) -> Vec<String> {
        units.iter().map(Unit::key).collect()
    }

    #[test]
    fn export_carries_only_the_allowlist_and_withholds_account_rules() {
        let document: DocumentMut = HOST_A.parse().unwrap();
        let units = extract(&document, &all(), &ConfigSyncConfig::default(), false).unwrap();
        assert_eq!(
            keys(&units),
            [
                "pipeline.pair",
                "message.skills.review",
                "automation.rule.resume",
                "agent.claude.options",
                "watch.theme",
                "collaboration.enabled",
                "ticket.agent",
                "ticket.timeout_secs",
                "ticket.source.linear",
            ]
        );
        assert_eq!(units.withheld.len(), 1);
        assert_eq!(units.withheld[0].path, "automation.rule.swap");
        let bundle = export(HOST_A, &ConfigSyncConfig::default());
        for secret in [
            "secret",
            "0.0.0.0",
            "collab.json",
            "account",
            "swap_account",
            "cwd",
        ] {
            assert!(!bundle.contains(secret), "{secret} leaked:\n{bundle}");
        }
    }

    #[test]
    fn a_bundle_round_trips_and_keeps_its_formatting() {
        let text = export(HOST_A, &ConfigSyncConfig::default());
        let bundle = Bundle::parse(&text).unwrap();
        assert_eq!(bundle.source, "node-a");
        assert_eq!(bundle.source_label.as_deref(), Some("host-a"));
        assert_eq!(bundle.categories, all());
        assert_eq!(bundle.units.len(), 9);
        assert!(text.contains("[[pipeline.pair.agent]]"), "{text}");
        assert!(text.contains("# keep this comment"), "{text}");
    }

    #[test]
    fn a_bundle_cannot_carry_machine_scope_or_unknown_settings() {
        let header = "schema_version = 1\nsource = \"s\"\ncategories = [\"watch\", \"agent\", \"collaboration\", \"ticket\", \"automation\"]\n";
        for body in [
            "[dashboard]\ntoken = \"x\"\n",
            "[agent.claude]\naccount = \"auto\"\n",
            "[collaboration]\npath = \"/x\"\n",
            "[ticket]\ncwd = \"/x\"\n",
            "[watch]\ncollaboration_kind = \"task\"\n",
            "[watch]\nbrand_new = 1\n",
            "[[automation.rule]]\nname = \"s\"\naction = \"swap_account\"\n",
            "[automation]\npaused_until = 1979-05-27T07:32:00Z\n",
            "[route]\nmatch = \"x\"\n",
        ] {
            let error = Bundle::parse(&format!("{header}{body}")).unwrap_err();
            assert!(!error.is_empty(), "{body}");
        }
        assert!(
            Bundle::parse("schema_version = 2\nsource = \"s\"\ncategories = []\n")
                .unwrap_err()
                .contains("not supported")
        );
        // Settings for an undeclared category would be dropped silently.
        assert!(Bundle::parse(
            "schema_version = 1\nsource = \"s\"\ncategories = [\"watch\"]\n[message.skills]\nx = \"y\"\n"
        )
        .unwrap_err()
        .contains("does not list"));
    }

    #[test]
    fn ignore_and_categories_narrow_export() {
        let sync = ConfigSyncConfig {
            categories: None,
            ignore: vec!["pipeline.pair".into(), "ticket".into(), "skills".into()],
        };
        let document: DocumentMut = HOST_A.parse().unwrap();
        let units = extract(&document, &all(), &sync, false).unwrap();
        assert!(keys(&units).iter().all(|k| !k.starts_with("pipeline")
            && !k.starts_with("ticket")
            && !k.starts_with("message")));
        assert!(sync.validate().is_ok());
        let bad = ConfigSyncConfig {
            categories: Some(vec!["nope".into()]),
            ignore: vec![],
        };
        assert!(bad.validate().is_err());
    }

    const HOST_B: &str = r#"# host b keeps this
[dashboard]
bind = "127.0.0.1:7878"

[watch]
theme = "classic" # b's comment

[agent.claude]
account = "pinned@claude"

[pipeline.local-only]
layout = "tiled"
"#;

    fn bundle_from(text: &str) -> Bundle {
        Bundle::parse(&export(text, &ConfigSyncConfig::default())).unwrap()
    }

    #[test]
    fn first_apply_adds_and_replaces_but_never_deletes() {
        let mut changed = HOST_A.replace("theme = \"classic\"", "theme = \"focus\"");
        changed = changed.replace("[pipeline.pair]", "[pipeline.pair]\ndescription = \"x\"");
        let bundle = bundle_from(&changed);
        let plan = plan(HOST_B, &bundle, None, &ConfigSyncConfig::default(), None).unwrap();
        assert!(plan.conflicts.is_empty());
        let replace: Vec<_> = plan
            .steps
            .iter()
            .filter(|s| s.change == Change::Replace)
            .map(|s| s.key.as_str())
            .collect();
        assert_eq!(replace, ["watch.theme"]);
        assert!(plan.steps.iter().all(|s| s.change != Change::Remove));
        let out = apply(HOST_B, &plan).unwrap();
        // Local-only pipeline, machine scope and comments all survive.
        assert!(out.contains("[pipeline.local-only]"), "{out}");
        assert!(out.contains("127.0.0.1:7878"), "{out}");
        assert!(out.contains("account = \"pinned@claude\""), "{out}");
        assert!(out.contains("# host b keeps this"), "{out}");
        assert!(out.contains("theme = \"focus\" # b's comment"), "{out}");
        // New tables land together, parents before children.
        let at = |needle: &str| {
            out.find(needle)
                .unwrap_or_else(|| panic!("{needle}:\n{out}"))
        };
        assert!(
            at("[pipeline.pair]") < at("[[pipeline.pair.agent]]"),
            "{out}"
        );
        assert!(at("[ticket]") < at("[ticket.source.linear]"), "{out}");
        assert!(out.contains("options = [\"--model\", \"x\"]"), "{out}");
        assert!(out.contains("[[pipeline.pair.agent]]"), "{out}");
        let parsed: crate::Config = toml::from_str(&out).unwrap();
        parsed.validate().unwrap();
        // The base records the incoming projection, not local-only items.
        assert!(plan.base.contains_key("pipeline.pair"));
        assert!(!plan.base.contains_key("pipeline.local-only"));
    }

    fn applied(local: &str, bundle: &Bundle, base: Option<&Base>) -> (String, Base) {
        let plan = plan(local, bundle, base, &ConfigSyncConfig::default(), None).unwrap();
        assert!(plan.conflicts.is_empty(), "{:?}", plan.conflicts);
        (apply(local, &plan).unwrap(), plan.base)
    }

    #[test]
    fn three_way_follows_upstream_keeps_local_edits_and_flags_conflicts() {
        let v1 = bundle_from(HOST_A);
        let (b1, base) = applied(HOST_B, &v1, None);

        // Upstream changes a skill; the local copy still matches the base.
        let v2 = bundle_from(&HOST_A.replace("please review", "review twice"));
        let (b2, base2) = applied(&b1, &v2, Some(&base));
        assert!(b2.contains("review twice"));

        // A local edit survives an unchanged upstream.
        let local = b2.replace("timeout_secs = 300", "timeout_secs = 900");
        let p = plan(
            &local,
            &v2,
            Some(&base2),
            &ConfigSyncConfig::default(),
            None,
        )
        .unwrap();
        assert!(
            p.is_noop(),
            "{:?}",
            p.steps.iter().map(|s| &s.key).collect::<Vec<_>>()
        );

        // Both changed: a conflict, never silently resolved.
        let v3 = bundle_from(
            &HOST_A
                .replace("please review", "review twice")
                .replace("timeout_secs = 300", "timeout_secs = 60"),
        );
        let p = plan(
            &local,
            &v3,
            Some(&base2),
            &ConfigSyncConfig::default(),
            None,
        )
        .unwrap();
        assert_eq!(p.conflicts.len(), 1);
        assert_eq!(p.conflicts[0].key, "ticket.timeout_secs");
        assert!(apply(&local, &p).is_err());
        let theirs = plan(
            &local,
            &v3,
            Some(&base2),
            &ConfigSyncConfig::default(),
            Some(Prefer::Incoming),
        )
        .unwrap();
        assert!(apply(&local, &theirs)
            .unwrap()
            .contains("timeout_secs = 60"));
        let ours = plan(
            &local,
            &v3,
            Some(&base2),
            &ConfigSyncConfig::default(),
            Some(Prefer::Local),
        )
        .unwrap();
        assert!(ours.is_noop());
    }

    #[test]
    fn upstream_deletion_removes_only_what_sync_owns() {
        let v1 = bundle_from(HOST_A);
        let (b1, base) = applied(HOST_B, &v1, None);
        let without = HOST_A
            .replace("[message.skills]\nreview = \"please review\"\n", "")
            .replace(
                "[[automation.rule]]\nname = \"resume\"\non = \"rate_limited\"\naction = \"send_prompt\"\ntext = \"continue\"\n",
                "",
            );
        let v2 = bundle_from(&without);
        // The skills category stays declared, so its emptiness is a deletion.
        assert!(v2.categories.contains(&Category::Skills));
        let (b2, _) = applied(&b1, &v2, Some(&base));
        assert!(!b2.contains("please review"), "{b2}");
        assert!(!b2.contains("name = \"resume\""), "{b2}");
        assert!(b2.contains("[pipeline.local-only]"), "{b2}");

        // Deleting upstream what was edited here is a conflict.
        let edited = b1.replace("please review", "local wording");
        let p = plan(
            &edited,
            &v2,
            Some(&base),
            &ConfigSyncConfig::default(),
            None,
        )
        .unwrap();
        assert!(p
            .conflicts
            .iter()
            .any(|c| c.key == "message.skills.review" && c.incoming.is_none()));
    }

    #[test]
    fn ignoring_or_narrowing_suspends_ownership_instead_of_deleting() {
        let v1 = bundle_from(HOST_A);
        let (b1, base) = applied(HOST_B, &v1, None);
        let sync = ConfigSyncConfig {
            categories: Some(vec!["watch".into()]),
            ignore: vec!["pipeline.pair".into()],
        };
        let empty = Bundle {
            units: Units::default(),
            ..v1.clone()
        };
        let p = plan(&b1, &empty, Some(&base), &sync, None).unwrap();
        // Only watch is managed; its upstream removal applies, nothing else moves.
        assert!(
            p.steps.iter().all(|s| s.key.starts_with("watch.")),
            "{:?}",
            p.steps.iter().map(|s| &s.key).collect::<Vec<_>>()
        );
        assert!(p.base.contains_key("pipeline.pair"));
        assert!(p.base.contains_key("message.skills.review"));
        assert!(!p.base.contains_key("watch.theme"));
    }

    #[test]
    fn rules_merge_by_name_in_inline_arrays_too() {
        let local = "[automation]\nrule = [{ name = \"resume\", on = \"error\", action = \"notify\", message = \"m\" }]\n";
        let v1 = bundle_from(HOST_A);
        let (out, _) = applied(local, &v1, None);
        let parsed: crate::Config = toml::from_str(&out).unwrap();
        assert_eq!(parsed.automation.rule.len(), 1);
        assert_eq!(parsed.automation.rule[0].text.as_deref(), Some("continue"));
    }

    #[test]
    fn a_route_must_still_find_its_pipeline() {
        let config: crate::Config =
            toml::from_str("[[route]]\nmatch = 'x'\npipeline = 'gone'\n").unwrap();
        assert!(check_references(&config).unwrap_err().contains("gone"));
    }

    #[test]
    fn paths_parse_quoted_segments_and_display_them_back() {
        let path = parse_path("pipeline.\"a.b\"").unwrap();
        assert_eq!(path, ["pipeline", "a.b"]);
        assert_eq!(display_path(&path), "pipeline.\"a.b\"");
        assert!(parse_path("a..b").is_err());
        assert!(parse_path("\"open").is_err());
    }
}
