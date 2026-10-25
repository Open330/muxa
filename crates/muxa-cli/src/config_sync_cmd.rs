//! `muxa config export / diff / apply` — carry portable settings between
//! hosts through any transport: a dotfiles repo, scp, a gist.
//!
//! The planning lives in [`muxa::config_sync`]; this module reads the
//! daemon's file, keeps the per-config base a three-way apply needs, and
//! writes through the daemon so the result is validated and conditional on
//! the text the plan was made from. It shares Fleet sync's lock, so the two
//! writers never interleave.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::{IsTerminal as _, Read as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use muxa::config_sync::{self, Base, Bundle, Category, Change, ConfigSyncConfig, Plan, Prefer};
use muxa::ipc::Client;
use serde::{Deserialize, Serialize};

use crate::fleet_sync::{ensure_parent, lock_config, private_options, write_journal};

/// Largest bundle accepted, matching Fleet sync's transfer limit.
const LIMIT: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub(crate) enum PreferArg {
    /// Keep this host's value for every conflicting item.
    Local,
    /// Take the bundle's value for every conflicting item.
    Incoming,
}

impl From<PreferArg> for Prefer {
    fn from(value: PreferArg) -> Self {
        match value {
            PreferArg::Local => Prefer::Local,
            PreferArg::Incoming => Prefer::Incoming,
        }
    }
}

/// What this host last applied from one source, kept per target config.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    target: PathBuf,
    source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_label: Option<String>,
    #[serde(default)]
    units: Base,
}

/// An apply in flight: enough to finish or abandon it after a crash.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    before: String,
    after: String,
    backup: Option<PathBuf>,
    state: String,
}

pub(crate) fn journal_path(config: &Path) -> PathBuf {
    config.with_extension("toml.config-sync-pending.json")
}

fn state_path(config: &Path) -> Result<PathBuf> {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(config.as_os_str().as_encoded_bytes());
    let name = digest[..12].iter().fold(String::new(), |mut name, byte| {
        let _ = write!(name, "{byte:02x}");
        name
    });
    Ok(muxa::paths::data_dir()
        .context("no data directory for the config sync base")?
        .join("muxa")
        .join("config-sync")
        .join(format!("{name}.toml")))
}

fn load_state(config: &Path) -> Result<Option<State>> {
    let path = state_path(config)?;
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let state: State =
                toml::from_str(&text).with_context(|| format!("reading {}", path.display()))?;
            if state.target != config {
                bail!(
                    "{} records a base for {}, not {}",
                    path.display(),
                    state.target.display(),
                    config.display()
                );
            }
            Ok(Some(state))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

/// Atomically replace the base, private to this user.
fn store_state(config: &Path, text: &str) -> Result<()> {
    let path = state_path(config)?;
    let parent = path.parent().context("state path has a parent")?;
    std::fs::create_dir_all(parent)?;
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = private_options().create_new(true).open(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, &path)?;
        // The rename, and a directory created for it, must reach the disk
        // before the journal that could redo them is removed.
        sync_dir(parent)?;
        if let Some(grandparent) = parent.parent() {
            sync_dir(grandparent)?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    let _ = dir;
    Ok(())
}

/// Remove a finished journal, durably.
fn remove_journal(journal: &Path) -> Result<()> {
    std::fs::remove_file(journal)?;
    if let Some(parent) = journal.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}

fn host_sync(text: &str) -> Result<ConfigSyncConfig> {
    let config: muxa::Config =
        toml::from_str(text).context("the daemon's config.toml does not parse")?;
    Ok(config.config_sync)
}

/// Read a bundle from a path, or standard input for `-`, bounded.
fn read_bundle(path: &Path) -> Result<Bundle> {
    let mut bytes = Vec::new();
    if path == Path::new("-") {
        std::io::stdin()
            .take((LIMIT + 1) as u64)
            .read_to_end(&mut bytes)?;
    } else {
        std::fs::File::open(path)
            .with_context(|| format!("reading {}", path.display()))?
            .take((LIMIT + 1) as u64)
            .read_to_end(&mut bytes)?;
    }
    if bytes.len() > LIMIT {
        bail!("bundle exceeds {LIMIT} bytes");
    }
    let text = String::from_utf8(bytes).context("bundle is not UTF-8")?;
    Bundle::parse(&text).map_err(anyhow::Error::msg)
}

pub(crate) async fn export(
    client: &Client,
    output: Option<&Path>,
    categories: &[String],
) -> Result<()> {
    let document = client.config_read().await?;
    let sync = host_sync(&document.text)?;
    let categories: BTreeSet<Category> = if categories.is_empty() {
        sync.selected().map_err(anyhow::Error::msg)?
    } else {
        categories
            .iter()
            .map(|c| Category::parse(c))
            .collect::<Result<_, _>>()
            .map_err(anyhow::Error::msg)?
    };
    let parsed: toml_edit::DocumentMut = document.text.parse()?;
    let units =
        config_sync::extract(&parsed, &categories, &sync, false).map_err(anyhow::Error::msg)?;
    let source = muxa::fleet::load_or_create_node_id(
        &muxa::paths::default_node_id_file().context("no node identity path")?,
    )?
    .to_string();
    let label = muxa::fleet::local_hostname();
    let text = config_sync::render_bundle(&source, label.as_deref(), &categories, &units);
    for withheld in &units.withheld {
        eprintln!("withheld {}: {}", withheld.path, withheld.reason);
    }
    match output {
        Some(path) if path != Path::new("-") => {
            std::fs::write(path, &text).with_context(|| format!("writing {}", path.display()))?;
            eprintln!(
                "exported {} settings in {} categories to {}",
                units.len(),
                categories.len(),
                path.display()
            );
        }
        // Standard output carries only the bundle, so it can be piped.
        _ => print!("{text}"),
    }
    Ok(())
}

/// The plan for `bundle` against the daemon's current file, and the
/// document it would produce, already validated. `diff` and `apply` share
/// this, so a preview never promises what apply would refuse.
struct Planned {
    document: muxa::config_file::ConfigDocument,
    plan: Plan,
    /// The resulting text; `None` while conflicts are unresolved.
    next: Option<String>,
    state_changed: bool,
}

async fn planned(
    client: &Client,
    bundle: &Bundle,
    prefer: Option<PreferArg>,
    switch_source: bool,
) -> Result<Planned> {
    let document = client.config_read().await?;
    let sync = host_sync(&document.text)?;
    let state = load_state(&document.path)?;
    let base = match &state {
        Some(state) if state.source == bundle.source => Some(&state.units),
        Some(state) if !switch_source => bail!(
            "this config last applied settings from {} ({}); this bundle comes from {} ({}). \
             Pass --switch-source to make it the source; items are then treated as a first sync",
            state.source,
            state.source_label.as_deref().unwrap_or("unknown host"),
            bundle.source,
            bundle.source_label.as_deref().unwrap_or("unknown host"),
        ),
        _ => None,
    };
    let plan = config_sync::plan(&document.text, bundle, base, &sync, prefer.map(Into::into))
        .map_err(anyhow::Error::msg)?;
    let next = if plan.conflicts.is_empty() {
        let next = config_sync::apply(&document.text, &plan).map_err(anyhow::Error::msg)?;
        muxa::config_file::check(&next).context("the merged config would not load")?;
        let merged: muxa::Config = toml::from_str(&next)?;
        config_sync::check_references(&merged).map_err(anyhow::Error::msg)?;
        Some(next)
    } else {
        None
    };
    let state_changed = state
        .as_ref()
        .is_none_or(|state| state.source != bundle.source || state.units != plan.base);
    Ok(Planned {
        document,
        plan,
        next,
        state_changed,
    })
}

fn value_lines(value: &toml::Value) -> Vec<String> {
    match value {
        toml::Value::Table(table) => toml::to_string(table)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect(),
        other => vec![other.to_string()],
    }
}

/// The plan as a reviewable list: only changed items, no neighbouring
/// context, so unrelated private settings never reach the screen.
fn render(plan: &Plan) -> String {
    let mut out = String::new();
    for step in &plan.steps {
        let (mark, label) = match step.change {
            Change::Add => ('+', "add"),
            Change::Update => ('~', "update"),
            Change::Remove => ('-', "remove"),
            Change::Replace => ('~', "replace (first sync)"),
        };
        let _ = writeln!(out, "{mark} {} ({label})", step.key);
        for line in step.before.iter().flat_map(value_lines) {
            let _ = writeln!(out, "    - {line}");
        }
        for line in step.after_value.iter().flat_map(value_lines) {
            let _ = writeln!(out, "    + {line}");
        }
    }
    for key in &plan.protected {
        let _ = writeln!(
            out,
            "= {key} (kept: this host's rule of that name switches accounts)"
        );
    }
    for conflict in &plan.conflicts {
        let _ = writeln!(
            out,
            "! {} (conflict: changed here and upstream)",
            conflict.key
        );
        for (side, value) in [
            ("base", &conflict.base),
            ("local", &conflict.local),
            ("incoming", &conflict.incoming),
        ] {
            match value {
                Some(value) => {
                    for line in value_lines(value) {
                        let _ = writeln!(out, "    {side}: {line}");
                    }
                }
                None => {
                    let _ = writeln!(out, "    {side}: (absent)");
                }
            }
        }
    }
    muxa::fleet::sanitize_terminal_text(&out)
}

/// `diff` exits like `kubectl diff`: 0 nothing to change, 1 changes, 2 error
/// or conflict. Nothing is written.
pub(crate) async fn diff(
    client: &Client,
    bundle: &Path,
    prefer: Option<PreferArg>,
    switch_source: bool,
) -> i32 {
    let result = async {
        let bundle = read_bundle(bundle)?;
        let path = client.config_read().await?.path;
        if journal_path(&path).exists() {
            bail!("an unfinished `muxa config apply` holds this config; rerun apply to finish it");
        }
        let planned = planned(client, &bundle, prefer, switch_source).await?;
        print!("{}", render(&planned.plan));
        Ok::<_, anyhow::Error>(if planned.plan.conflicts.is_empty() {
            i32::from(!planned.plan.steps.is_empty())
        } else {
            2
        })
    }
    .await;
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("Error: {error:#}");
            2
        }
    }
}

pub(crate) async fn apply(
    client: &Client,
    bundle_path: &Path,
    yes: bool,
    prefer: Option<PreferArg>,
    switch_source: bool,
) -> Result<()> {
    let bundle = read_bundle(bundle_path)?;
    // Lock, then finish any interrupted apply, and only then read the base
    // and plan: a plan made before recovery would overwrite the base that
    // recovery records.
    let path = client.config_read().await?.path;
    ensure_parent(&path)?;
    let _lock = lock_config(&path).await?;
    if crate::fleet_sync::pending_path(&path).exists() {
        bail!("an unfinished Fleet sync holds this config; rerun `muxa fleet sync --apply` first");
    }
    let journal = journal_path(&path);
    if journal.exists() {
        recover(&client.config_read().await?, &journal)?;
    }
    let Planned {
        document,
        plan,
        next,
        state_changed,
    } = planned(client, &bundle, prefer, switch_source).await?;
    print!("{}", render(&plan));
    let Some(next) = next else {
        bail!(
            "{} conflicting item(s); resolve them locally, ignore them in [config_sync], \
             or rerun with --prefer local|incoming",
            plan.conflicts.len()
        );
    };
    if plan.steps.is_empty() && !state_changed {
        println!("already up to date");
        return Ok(());
    }
    if !plan.steps.is_empty() && !yes {
        // The bundle may be on stdin; confirmation must come from a terminal.
        if bundle_path == Path::new("-") || !std::io::stdin().is_terminal() {
            bail!("pass --yes to apply without a terminal to confirm on");
        }
        if !cliclack::confirm(format!("Apply {} change(s)?", plan.steps.len()))
            .initial_value(false)
            .interact()?
        {
            bail!("cancelled");
        }
    }
    let state = toml::to_string(&State {
        target: document.path.clone(),
        source: bundle.source.clone(),
        source_label: bundle.source_label.clone(),
        units: plan.base.clone(),
    })?;

    // The lock keeps other sync writers out; the conditional write below
    // refuses if anything else edited the file since it was read.
    let backup = if next == document.text {
        None
    } else {
        let backup = document
            .path
            .with_extension(format!("toml.config-sync-{}.bak", uuid::Uuid::new_v4()));
        let mut file = private_options().create_new(true).open(&backup)?;
        file.write_all(document.text.as_bytes())?;
        file.sync_all()?;
        Some(backup)
    };
    write_journal(
        &journal,
        &Journal {
            before: document.text.clone(),
            after: next.clone(),
            backup: backup.clone(),
            state: state.clone(),
        },
    )?;
    if next != document.text {
        // Keep the journal if the response is lost: the write may have
        // landed, and the next apply finishes it under the same lock.
        client
            .config_write(&next, Some(&document.text))
            .await
            .context("config changed while planning, or the write failed; run apply again")?;
    }
    store_state(&document.path, &state)?;
    remove_journal(&journal)?;

    match backup {
        Some(backup) => {
            println!(
                "applied {} change(s); backup {}",
                plan.steps.len(),
                backup.display()
            );
            eprintln!("restart muxad to apply: muxa daemon restart");
        }
        None => println!("recorded the base from {}", bundle.source),
    }
    Ok(())
}

/// Finish or abandon an apply that stopped part-way.
fn recover(current: &muxa::config_file::ConfigDocument, journal: &Path) -> Result<()> {
    let pending: Journal = serde_json::from_slice(&std::fs::read(journal)?)?;
    if current.text == pending.after {
        // The write landed; only the base was not recorded.
        store_state(&current.path, &pending.state)?;
    } else if current.text != pending.before {
        bail!(
            "an unfinished config apply conflicts with a later edit; inspect {}{}",
            journal.display(),
            pending
                .backup
                .as_ref()
                .map(|b| format!(" and backup {}", b.display()))
                .unwrap_or_default()
        );
    }
    remove_journal(journal)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_base_lives_per_config_path_under_the_data_dir() {
        let a = state_path(Path::new("/a/config.toml")).unwrap();
        let b = state_path(Path::new("/b/config.toml")).unwrap();
        assert_ne!(a, b);
        assert!(a.ends_with(a.file_name().unwrap()));
        assert!(a.to_string_lossy().contains("config-sync"));
    }

    #[test]
    fn rendering_names_only_changed_items_and_conflicts() {
        let bundle = Bundle::parse(
            "schema_version = 1\nsource = \"s\"\ncategories = [\"watch\", \"skills\"]\n\
             [watch]\ntheme = \"focus\"\n[message.skills]\nnew = \"x\"\n",
        )
        .unwrap();
        let local = "[dashboard]\ntoken = \"secret\"\n[watch]\ntheme = \"classic\"\n";
        let plan =
            config_sync::plan(local, &bundle, None, &ConfigSyncConfig::default(), None).unwrap();
        let text = render(&plan);
        assert!(
            text.contains("~ watch.theme (replace (first sync))"),
            "{text}"
        );
        assert!(text.contains("- \"classic\""), "{text}");
        assert!(text.contains("+ message.skills.new (add)"), "{text}");
        assert!(!text.contains("secret"), "{text}");
    }
}
