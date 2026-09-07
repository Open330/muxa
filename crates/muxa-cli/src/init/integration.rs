//! Shared agent bundle, host adapters, and ownership-aware installation.
//! Planning performs reads only; apply owns every mutation, including symlinks.

use super::components::Component;
use super::detect::Detection;
use super::marker::{self, Outcome, Style};
use super::plan::{file_base, Action, Direction, Plan};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

pub const BOOTSTRAP: &str = include_str!("../../assets/agent-integration/bootstrap.md");
const SKILL: &str =
    include_str!("../../assets/agent-integration/skills/muxa-collaboration/SKILL.md");
const WORKFLOWS: &str = include_str!(
    "../../assets/agent-integration/skills/muxa-collaboration/references/workflows.md"
);
const SKILL_DIR: &str = "skills/muxa-collaboration";
const BLOCK: &str = "agent-instructions";
pub const CODEX_ENV: &[&str] = &["RMUX", "RMUX_PANE", "TMUX", "TMUX_PANE", "MUXA_SOCKET"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Host {
    Codex,
    Claude,
}

#[derive(Debug)]
struct HostPaths {
    host: Host,
    home: PathBuf,
    config: PathBuf,
    skill: PathBuf,
}

pub fn codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".codex")))
}

pub fn claude_home() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".claude")))
}

fn hosts(detect: &Detection) -> Result<Vec<HostPaths>> {
    let home = dirs::home_dir().context("cannot locate user home for agent integration")?;
    let mut out = Vec::new();
    if let Some(codex) = codex_home() {
        let codex = absolute(&codex)?;
        if detect.codex_config.is_some() || codex.is_dir() || which::which("codex").is_ok() {
            out.push(HostPaths {
                host: Host::Codex,
                config: codex.join("config.toml"),
                home: codex,
                skill: home.join(".agents/skills/muxa-collaboration"),
            });
        }
    }
    if let Some(claude) = claude_home() {
        let claude = absolute(&claude)?;
        if detect.claude_settings.is_some() || claude.is_dir() || which::which("claude").is_ok() {
            let config = if std::env::var_os("CLAUDE_CONFIG_DIR").is_some_and(|v| !v.is_empty()) {
                claude.join(".claude.json")
            } else {
                home.join(".claude.json")
            };
            out.push(HostPaths {
                host: Host::Claude,
                config,
                skill: claude.join("skills/muxa-collaboration"),
                home: claude,
            });
        }
    }
    Ok(out)
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    schema: u32,
    bundle_version: String,
    records: Vec<Record>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            schema: 1,
            bundle_version: env!("CARGO_PKG_VERSION").into(),
            records: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    component: String,
    path: PathBuf,
    #[serde(flatten)]
    kind: RecordKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RecordKind {
    Asset {
        installed: String,
        /// Last known installed bytes, retained across an interrupted upgrade.
        previous: Option<String>,
    },
    Instructions,
    Symlink {
        target: PathBuf,
        owned: bool,
    },
    Mcp {
        host: Host,
        previous: Option<Value>,
        installed: Value,
        /// Entry observed when preparing this write-ahead installation record.
        before_apply: Option<Value>,
    },
    /// Remember conflicts for doctor without claiming their files.
    Unmanaged {
        reason: String,
    },
}

fn load_manifest(path: &Path) -> Result<Manifest> {
    let manifest: Manifest = match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).context("reading agent integration manifest")?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Manifest::default(),
        Err(e) => return Err(e).context("reading agent integration manifest"),
    };
    if manifest.schema != 1 {
        bail!(
            "unsupported agent integration manifest schema {}",
            manifest.schema
        );
    }
    Ok(manifest)
}

fn absolute(path: &Path) -> Result<PathBuf> {
    Ok(if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    })
}

fn bundle_root(config: Option<&Path>) -> Result<PathBuf> {
    let config = config
        .map(Path::to_path_buf)
        .or_else(muxa::paths::default_config_file)
        .context("cannot locate muxa config for agent integration")?;
    let config = absolute(&config)?;
    Ok(config
        .parent()
        .context("muxa config has no parent")?
        .join("agent-integration"))
}

pub fn is_component(c: Component) -> bool {
    matches!(
        c,
        Component::AgentInstructions | Component::AgentSkills | Component::AgentMcp
    )
}

pub fn extend_plan(
    plan: &mut Plan,
    detect: &Detection,
    config: Option<&Path>,
    socket: &Path,
) -> Result<()> {
    if !plan.components.iter().any(|c| is_component(*c)) {
        return Ok(());
    }
    let root = bundle_root(config)?;
    let hosts = hosts(detect)?;
    plan_bundle(plan, &root, &hosts, config, socket)
}

fn plan_bundle(
    plan: &mut Plan,
    root: &Path,
    hosts: &[HostPaths],
    config: Option<&Path>,
    socket: &Path,
) -> Result<()> {
    let manifest_path = root.join("manifest.json");
    let mut manifest = load_manifest(&manifest_path)?;
    let start = plan.actions.len();
    if plan.direction == Direction::Uninstall {
        let mut retained = Vec::new();
        for record in manifest.records.drain(..) {
            let selected = plan.components.iter().any(|c| c.id() == record.component);
            if !selected || !uninstall_record(plan, &record)? {
                retained.push(record);
            }
        }
        manifest.records = retained;
    } else {
        if hosts.is_empty() {
            plan.warnings.push(
                "No Codex or Claude Code installation detected; agent integration skipped.".into(),
            );
            return Ok(());
        }
        if plan.components.contains(&Component::AgentInstructions) {
            asset(
                plan,
                &mut manifest,
                Component::AgentInstructions,
                root.join("bootstrap.md"),
                BOOTSTRAP,
            )?;
            for host in hosts {
                instructions(plan, &mut manifest, root, host)?;
            }
        }
        if plan.components.contains(&Component::AgentSkills) {
            asset(
                plan,
                &mut manifest,
                Component::AgentSkills,
                root.join(SKILL_DIR).join("SKILL.md"),
                SKILL,
            )?;
            asset(
                plan,
                &mut manifest,
                Component::AgentSkills,
                root.join(SKILL_DIR).join("references/workflows.md"),
                WORKFLOWS,
            )?;
            for host in hosts {
                skill_link(plan, &mut manifest, &host.skill, &root.join(SKILL_DIR))?;
            }
        }
        if plan.components.contains(&Component::AgentMcp) {
            for host in hosts {
                mcp(plan, &mut manifest, host, config, socket)?;
            }
        }
        manifest.bundle_version = env!("CARGO_PKG_VERSION").into();
    }
    if manifest.records.is_empty() && !manifest_path.exists() {
        return Ok(());
    }
    let after = format!("{}\n", serde_json::to_string_pretty(&manifest)?);
    edit(plan, Component::AgentInstructions, manifest_path, after)?;
    // Record ownership before install mutations, so a failed apply is recoverable
    // on re-run. Uninstall records are discarded only after removal succeeds.
    if plan.direction == Direction::Install {
        if let Some(action) = plan.actions.pop() {
            plan.actions.insert(start, action);
        }
    }
    Ok(())
}

fn remember(manifest: &mut Manifest, record: Record) {
    manifest
        .records
        .retain(|r| !(r.path == record.path && r.component == record.component));
    manifest.records.push(record);
    manifest
        .records
        .sort_by(|a, b| (&a.component, &a.path).cmp(&(&b.component, &b.path)));
}

fn edit(plan: &mut Plan, component: Component, path: PathBuf, after: String) -> Result<()> {
    let (before, original) = file_base(&plan.actions, &path)?;
    let outcome = if original == after {
        Outcome::Unchanged
    } else if before.is_none() {
        Outcome::Inserted
    } else {
        Outcome::Replaced
    };
    plan.actions.push(Action::EditFile {
        component,
        path,
        before,
        after,
        outcome,
    });
    Ok(())
}

fn warn_preserved(plan: &mut Plan, path: &Path, reason: &str) {
    plan.warnings
        .push(format!("Preserved {}: {reason}", path.display()));
}

fn conflict(
    plan: &mut Plan,
    manifest: &mut Manifest,
    component: Component,
    path: &Path,
    reason: &str,
) {
    warn_preserved(plan, path, reason);
    if !manifest
        .records
        .iter()
        .any(|r| r.path == path && r.component == component.id())
    {
        remember(
            manifest,
            Record {
                component: component.id().into(),
                path: path.to_path_buf(),
                kind: RecordKind::Unmanaged {
                    reason: reason.into(),
                },
            },
        );
    }
}

fn asset(
    plan: &mut Plan,
    manifest: &mut Manifest,
    component: Component,
    path: PathBuf,
    content: &str,
) -> Result<()> {
    if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        conflict(
            plan,
            manifest,
            component,
            &path,
            "bundle asset was replaced by a symlink",
        );
        return Ok(());
    }
    let (before, _) = file_base(&plan.actions, &path)?;
    let prior = manifest.records.iter().find(|r| r.path == path);
    let ours = prior.is_some_and(|r| matches!(&r.kind, RecordKind::Asset { installed, previous } if Some(installed) == before.as_ref() || (before.is_some() && previous.as_ref() == before.as_ref())));
    if before.is_some()
        && !ours
        && !(prior.is_some_and(|r| matches!(r.kind, RecordKind::Asset { .. }))
            && before.as_deref() == Some(content))
    {
        conflict(plan, manifest, component, &path, "bundle asset is user-owned or locally modified; use config.toml [mcp.guide] for preferences");
        return Ok(());
    }
    let previous = prior.and_then(|r| match &r.kind {
        RecordKind::Asset {
            installed,
            previous,
        } if installed == content => previous.clone(),
        RecordKind::Asset { installed, .. } => Some(installed.clone()),
        _ => None,
    });
    edit(plan, component, path.clone(), content.into())?;
    remember(
        manifest,
        Record {
            component: component.id().into(),
            path,
            kind: RecordKind::Asset {
                installed: content.into(),
                previous,
            },
        },
    );
    Ok(())
}

fn active_instructions(host: &HostPaths) -> Result<PathBuf> {
    if host.host == Host::Claude {
        return Ok(host.home.join("CLAUDE.md"));
    }
    let override_path = host.home.join("AGENTS.override.md");
    match fs::read_to_string(&override_path) {
        Ok(text) if !text.trim().is_empty() => Ok(override_path),
        Ok(_) => Ok(host.home.join("AGENTS.md")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(host.home.join("AGENTS.md")),
        Err(e) => Err(e).context("reading Codex global override"),
    }
}

fn instruction_edit(original: &str, body: Option<&str>) -> Result<String> {
    let open = "<!-- >>> muxa managed (agent-instructions) >>> -->";
    let close = "<!-- <<< muxa managed (agent-instructions) <<< -->";
    let opens = original.lines().filter(|line| *line == open).count();
    let closes = original.lines().filter(|line| *line == close).count();
    if opens != closes || opens > 1 || (opens == 1 && original.find(open) > original.find(close)) {
        bail!("malformed or duplicate Muxa instruction block; repair the markers before re-running init");
    }
    Ok(match body {
        Some(body) => marker::upsert_styled(original, BLOCK, body, Style::Html).0,
        None => marker::remove_styled(original, BLOCK, Style::Html).0,
    })
}

fn instructions(
    plan: &mut Plan,
    manifest: &mut Manifest,
    root: &Path,
    host: &HostPaths,
) -> Result<()> {
    let path = active_instructions(host)?;
    // An override may have appeared since the last install. Remove our old
    // entry point so removing that override later cannot resurrect stale links.
    let old: Vec<_> = manifest
        .records
        .iter()
        .filter(|r| {
            matches!(r.kind, RecordKind::Instructions)
                && r.path.parent() == Some(host.home.as_path())
                && r.path != path
        })
        .cloned()
        .collect();
    for record in old {
        uninstall_record(plan, &record)?;
        manifest.records.retain(|r| r.path != record.path);
    }
    let (_, original) = file_base(&plan.actions, &path)?;
    let body = format!("For Muxa peer collaboration, @peer/@muxa-peer requests, or Muxa tmux\nworkspace/work/agent layout, read the shared entry point at:\n{}\nUse these instructions only for Muxa work; keep the user's existing task scope.", root.join("bootstrap.md").display());
    let after = instruction_edit(&original, Some(&body))?;
    edit(plan, Component::AgentInstructions, path.clone(), after)?;
    remember(
        manifest,
        Record {
            component: Component::AgentInstructions.id().into(),
            path,
            kind: RecordKind::Instructions,
        },
    );
    Ok(())
}

fn skill_link(plan: &mut Plan, manifest: &mut Manifest, path: &Path, target: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) if fs::read_link(path).ok().as_deref() == Some(target) => {
            // An identical link predating this installation stays user-owned.
            if !manifest
                .records
                .iter()
                .any(|r| r.path == path && matches!(r.kind, RecordKind::Symlink { .. }))
            {
                remember(
                    manifest,
                    Record {
                        component: Component::AgentSkills.id().into(),
                        path: path.to_path_buf(),
                        kind: RecordKind::Symlink {
                            target: target.to_path_buf(),
                            owned: false,
                        },
                    },
                );
            }
            return Ok(());
        }
        Ok(_) => {
            conflict(
                plan,
                manifest,
                Component::AgentSkills,
                path,
                "skill name already exists and is not this bundle's symlink",
            );
            return Ok(());
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).context("inspecting agent skill link"),
    }
    plan.actions.push(Action::ManageSymlink {
        path: path.to_path_buf(),
        target: target.to_path_buf(),
        remove: false,
    });
    remember(
        manifest,
        Record {
            component: Component::AgentSkills.id().into(),
            path: path.to_path_buf(),
            kind: RecordKind::Symlink {
                target: target.to_path_buf(),
                owned: true,
            },
        },
    );
    Ok(())
}

fn server_key(host: Host) -> &'static str {
    match host {
        Host::Codex => "mcp_servers",
        Host::Claude => "mcpServers",
    }
}

fn read_server(original: &str, host: Host) -> Result<Option<Value>> {
    let root: Value = if original.trim().is_empty() {
        json!({})
    } else {
        match host {
            Host::Codex => serde_json::to_value(
                toml::from_str::<toml::Value>(original).context("parsing Codex MCP config")?,
            )?,
            Host::Claude => serde_json::from_str(original).context("parsing Claude MCP config")?,
        }
    };
    if !root.is_object() {
        bail!("agent MCP config must be an object/table");
    }
    if let Some(servers) = root.get(server_key(host)) {
        if !servers.is_object() {
            bail!("MCP servers must be an object/table");
        }
    }
    Ok(root
        .get(server_key(host))
        .and_then(|v| v.get("muxa"))
        .cloned())
}

fn write_server(original: &str, host: Host, server: Option<&Value>) -> Result<String> {
    if read_server(original, host)?.as_ref() == server {
        return Ok(original.into());
    }
    match host {
        Host::Claude => {
            let mut root: Value = if original.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(original)?
            };
            let obj = root
                .as_object_mut()
                .context("Claude config must be an object")?;
            if let Some(server) = server {
                obj.entry("mcpServers")
                    .or_insert_with(|| json!({}))
                    .as_object_mut()
                    .context("mcpServers must be an object")?
                    .insert("muxa".into(), server.clone());
            } else if let Some(servers) = obj.get_mut("mcpServers").and_then(Value::as_object_mut) {
                servers.remove("muxa");
            }
            Ok(format!("{}\n", serde_json::to_string_pretty(&root)?))
        }
        Host::Codex => {
            let mut doc = original
                .parse::<toml_edit::DocumentMut>()
                .context("parsing Codex config")?;
            if let Some(server) = server {
                let value: toml::Value = serde_json::from_value(server.clone())?;
                let fragment = toml::to_string(&value)?.parse::<toml_edit::DocumentMut>()?;
                if doc.get("mcp_servers").is_none() {
                    let mut table = toml_edit::Table::new();
                    table.set_implicit(true);
                    doc["mcp_servers"] = toml_edit::Item::Table(table);
                }
                let servers = doc["mcp_servers"]
                    .as_table_like_mut()
                    .context("mcp_servers must be a table")?;
                if servers.get("muxa").is_none() {
                    servers.insert("muxa", toml_edit::Item::Table(fragment.as_table().clone()));
                } else {
                    let table = servers
                        .get_mut("muxa")
                        .and_then(toml_edit::Item::as_table_like_mut)
                        .context("muxa MCP entry must be a table")?;
                    // Preserve comments/formatting on untouched keys.
                    let existing = read_server(original, host)?.unwrap_or_default();
                    let keys: Vec<_> = table.iter().map(|(k, _)| k.to_owned()).collect();
                    for key in keys {
                        if server.get(&key).is_none() {
                            table.remove(&key);
                        }
                    }
                    for (key, item) in fragment.iter() {
                        if existing.get(key) != server.get(key) {
                            table.insert(key, item.clone());
                        }
                    }
                }
            } else if let Some(servers) = doc
                .get_mut("mcp_servers")
                .and_then(toml_edit::Item::as_table_like_mut)
            {
                servers.remove("muxa");
            }
            Ok(doc.to_string())
        }
    }
}

fn new_mcp_server(config: Option<&Path>, socket: &Path) -> Result<Value> {
    let mut args = Vec::new();
    // Default socket remains environment-routed, so pane-local sockets work.
    if socket != muxa::paths::default_socket() {
        args.extend([
            "--socket".to_owned(),
            absolute(socket)?.to_string_lossy().into_owned(),
        ]);
    }
    if let Some(config) = config {
        args.extend([
            "--config".to_owned(),
            absolute(config)?.to_string_lossy().into_owned(),
        ]);
    }
    args.push("mcp".into());
    Ok(json!({"command": "muxa", "args": args}))
}

fn is_muxa_server(server: &Value) -> bool {
    let command = server
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default();
    Path::new(command).file_name().and_then(|s| s.to_str()) == Some("muxa")
        && server
            .get("args")
            .and_then(Value::as_array)
            .is_some_and(|a| a.last().and_then(Value::as_str) == Some("mcp"))
        && server.get("url").is_none()
}

fn mcp(
    plan: &mut Plan,
    manifest: &mut Manifest,
    host: &HostPaths,
    config: Option<&Path>,
    socket: &Path,
) -> Result<()> {
    let (_, original) = file_base(&plan.actions, &host.config)?;
    let current = read_server(&original, host.host)?;
    let prior = manifest
        .records
        .iter()
        .find(|r| r.path == host.config && matches!(r.kind, RecordKind::Mcp { .. }))
        .cloned();
    if let Some(Record {
        kind:
            RecordKind::Mcp {
                installed,
                previous,
                before_apply,
                ..
            },
        ..
    }) = &prior
    {
        if current.as_ref() != Some(installed)
            && current != *previous
            && current != *before_apply
            && current.is_some()
        {
            warn_preserved(plan, &host.config, "Muxa MCP registration changed since installation; reconcile it before re-running agent-mcp");
            return Ok(());
        }
    }
    let mut desired = current.clone().unwrap_or_else(|| json!({}));
    if current.is_some() {
        if !is_muxa_server(&desired) {
            conflict(
                plan,
                manifest,
                Component::AgentMcp,
                &host.config,
                "MCP name muxa belongs to a different command/transport",
            );
            return Ok(());
        }
    } else {
        desired = new_mcp_server(config, socket)?;
    }
    if host.host == Host::Codex {
        let obj = desired
            .as_object_mut()
            .context("muxa MCP entry must be an object")?;
        let vars = obj
            .entry("env_vars")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .context("Codex env_vars must be an array")?;
        for name in CODEX_ENV {
            if !vars.iter().any(|v| v.as_str() == Some(name)) {
                vars.push(json!(name));
            }
        }
    } else if desired.get("type").is_none() {
        desired["type"] = json!("stdio");
    }
    let (previous, before_apply) = match prior {
        Some(Record {
            kind:
                RecordKind::Mcp {
                    previous,
                    installed,
                    before_apply,
                    ..
                },
            ..
        }) => (
            previous,
            if installed == desired {
                before_apply
            } else {
                current.clone()
            },
        ),
        _ => (current.clone(), current.clone()),
    };
    let after = write_server(&original, host.host, Some(&desired))?;
    edit(plan, Component::AgentMcp, host.config.clone(), after)?;
    // A pre-existing, fully configured server needs no ownership claim.
    // Track even an unchanged pre-existing registration for diagnostics. Its
    // previous and installed values are equal, so uninstall leaves it intact.
    remember(
        manifest,
        Record {
            component: Component::AgentMcp.id().into(),
            path: host.config.clone(),
            kind: RecordKind::Mcp {
                host: host.host,
                previous,
                installed: desired,
                before_apply,
            },
        },
    );
    Ok(())
}

fn uninstall_record(plan: &mut Plan, record: &Record) -> Result<bool> {
    let component = Component::parse(&record.component).context("unknown manifest component")?;
    match &record.kind {
        RecordKind::Symlink { owned: false, .. } | RecordKind::Unmanaged { .. } => {}
        RecordKind::Symlink {
            target,
            owned: true,
        } => match fs::symlink_metadata(&record.path) {
            Ok(_) if fs::read_link(&record.path).ok().as_ref() == Some(target) => {
                plan.actions.push(Action::ManageSymlink {
                    path: record.path.clone(),
                    target: target.clone(),
                    remove: true,
                });
            }
            Ok(_) => {
                warn_preserved(plan, &record.path, "skill link changed since installation");
                return Ok(false);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("inspecting installed skill link"),
        },
        RecordKind::Asset {
            installed,
            previous,
        } => {
            if fs::symlink_metadata(&record.path).is_ok_and(|m| m.file_type().is_symlink()) {
                warn_preserved(plan, &record.path, "bundle asset was replaced by a symlink");
                return Ok(false);
            }
            let (before, _) = file_base(&plan.actions, &record.path)?;
            if let Some(before) = before {
                if before != *installed && Some(&before) != previous.as_ref() {
                    warn_preserved(plan, &record.path, "bundle asset has local changes");
                    return Ok(false);
                }
                plan.actions.push(Action::RemoveOwnedFile {
                    path: record.path.clone(),
                    expected: before,
                });
            }
        }
        RecordKind::Instructions => {
            let (_, original) = file_base(&plan.actions, &record.path)?;
            let after = instruction_edit(&original, None)?;
            edit(plan, component, record.path.clone(), after)?;
        }
        RecordKind::Mcp {
            host,
            previous,
            installed,
            before_apply,
        } => {
            let (_, original) = file_base(&plan.actions, &record.path)?;
            let current = read_server(&original, *host)?;
            if current.as_ref() == Some(installed)
                || (current.is_some() && current == *before_apply)
            {
                let after = write_server(&original, *host, previous.as_ref())?;
                edit(plan, component, record.path.clone(), after)?;
            } else if current.is_some() && current != *previous {
                warn_preserved(
                    plan,
                    &record.path,
                    "Muxa MCP registration has local changes; left installed",
                );
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Read-only checks shared by init verification and doctor. No agent is spawned.
pub fn diagnostics(config: Option<&Path>) -> Result<Vec<(bool, String)>> {
    let root = bundle_root(config)?;
    let manifest_path = root.join("manifest.json");
    if !manifest_path.exists() {
        return Ok(vec![(false, "Agent integration not installed; run muxa init --component agent-instructions,agent-skills,agent-mcp".into())]);
    }
    let manifest = load_manifest(&manifest_path)?;
    let mut out = Vec::new();
    for record in manifest.records {
        let healthy = match &record.kind {
            RecordKind::Asset { installed, .. } => {
                fs::read_to_string(&record.path).is_ok_and(|s| s == *installed)
            }
            RecordKind::Symlink { target, .. } => {
                fs::read_link(&record.path).is_ok_and(|p| p == *target)
                    && record.path.join("SKILL.md").is_file()
            }
            RecordKind::Instructions => {
                let content = fs::read_to_string(&record.path).unwrap_or_default();
                let present = instruction_edit(&content, None).is_ok_and(|s| s != content);
                let shadowed = record.path.file_name().is_some_and(|n| n == "AGENTS.md")
                    && record.path.parent().is_some_and(|p| {
                        fs::read_to_string(p.join("AGENTS.override.md"))
                            .is_ok_and(|s| !s.trim().is_empty())
                    });
                present && !shadowed
            }
            RecordKind::Mcp {
                host, installed, ..
            } => {
                fs::read_to_string(&record.path)
                    .ok()
                    .and_then(|s| read_server(&s, *host).ok().flatten())
                    .as_ref()
                    == Some(installed)
                    && installed.get("enabled").and_then(Value::as_bool) != Some(false)
            }
            RecordKind::Unmanaged { .. } => false,
        };
        out.push((healthy, format!("{}: {}{}", record.component, record.path.display(), if healthy { "" } else { " — missing, changed, or shadowed; re-run the component or reconcile local changes" })));
    }
    if out.is_empty() {
        out.push((
            true,
            "Agent integration is uninstalled (empty ownership manifest)".into(),
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::apply;

    struct Fixture {
        dir: tempfile::TempDir,
        root: PathBuf,
        hosts: Vec<HostPaths>,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("config with spaces/agent-integration");
            let hosts = vec![
                HostPaths {
                    host: Host::Codex,
                    home: dir.path().join("custom codex"),
                    config: dir.path().join("custom codex/config.toml"),
                    skill: dir.path().join(".agents/skills/muxa-collaboration"),
                },
                HostPaths {
                    host: Host::Claude,
                    home: dir.path().join("custom claude"),
                    config: dir.path().join("custom claude/.claude.json"),
                    skill: dir.path().join("custom claude/skills/muxa-collaboration"),
                },
            ];
            Self { dir, root, hosts }
        }

        fn plan(&self, direction: Direction, components: &[Component]) -> Plan {
            let mut plan = Plan {
                direction,
                components: components.to_vec(),
                actions: vec![],
                warnings: vec![],
            };
            let config = self.root.parent().unwrap().join("config.toml");
            plan_bundle(
                &mut plan,
                &self.root,
                &self.hosts,
                Some(&config),
                &self.dir.path().join("custom.sock"),
            )
            .unwrap();
            plan
        }

        fn all(&self, direction: Direction) -> Plan {
            self.plan(
                direction,
                &[
                    Component::AgentInstructions,
                    Component::AgentSkills,
                    Component::AgentMcp,
                ],
            )
        }

        fn checks(&self) -> Vec<(bool, String)> {
            diagnostics(Some(&self.root.parent().unwrap().join("config.toml"))).unwrap()
        }
    }

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn installs_idempotently_and_uninstalls_only_owned_content() {
        let f = Fixture::new();
        let codex = &f.hosts[0];
        let claude = &f.hosts[1];
        write(
            &codex.home.join("AGENTS.md"),
            "Keep my global instructions.\n",
        );
        write(
            &codex.config,
            "# keep my comment\nmodel = 'my-model'\n[mcp_servers.other]\ncommand = 'other'\n",
        );
        write(
            &claude.config,
            r#"{"projects":{"project":{"trusted":true}},"mcpServers":{"other":{"command":"other"}}}"#,
        );
        let plan = f.all(Direction::Install);
        assert!(plan.warnings.is_empty());
        assert!(plan.has_changes());
        apply::run(&plan, false).unwrap();
        assert_eq!(fs::read_link(&codex.skill).unwrap(), f.root.join(SKILL_DIR));
        assert_eq!(
            fs::read_to_string(claude.skill.join("SKILL.md")).unwrap(),
            SKILL
        );
        let server = read_server(&fs::read_to_string(&codex.config).unwrap(), Host::Codex)
            .unwrap()
            .unwrap();
        assert_eq!(server["env_vars"], json!(CODEX_ENV));
        assert!(server["args"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a.as_str().is_some_and(|s| s.contains("config with spaces"))));
        assert!(f.checks().iter().all(|(ok, _)| *ok));
        let again = f.all(Direction::Install);
        assert!(!again.has_changes(), "{}", apply::render_dry_run(&again));
        apply::run(&f.all(Direction::Uninstall), false).unwrap();
        assert_eq!(
            fs::read_to_string(codex.home.join("AGENTS.md")).unwrap(),
            "Keep my global instructions.\n"
        );
        assert!(fs::symlink_metadata(&codex.skill).is_err());
        assert!(!f.root.join(SKILL_DIR).join("SKILL.md").exists());
        let config = fs::read_to_string(&codex.config).unwrap();
        assert!(config.contains("# keep my comment"));
        assert!(config.contains("model = 'my-model'"));
        assert!(read_server(&config, Host::Codex).unwrap().is_none());
        let claude_config: Value =
            serde_json::from_str(&fs::read_to_string(&claude.config).unwrap()).unwrap();
        assert_eq!(claude_config["projects"]["project"]["trusted"], true);
        assert_eq!(claude_config["mcpServers"]["other"]["command"], "other");
        assert!(!f.all(Direction::Uninstall).has_changes());
    }

    #[test]
    fn dry_run_writes_nothing_and_does_not_disclose_config_values() {
        let f = Fixture::new();
        write(
            &f.hosts[1].config,
            r#"{"privateToken":"sensitive-test-value"}"#,
        );
        let plan = f.all(Direction::Install);
        assert!(!apply::render_dry_run(&plan).contains("sensitive-test-value"));
        apply::run(&plan, true).unwrap();
        assert!(!f.root.exists());
        assert!(!f.hosts[0].home.exists());
        assert!(!f.hosts[1].skill.exists());
    }

    #[test]
    fn restores_preexisting_mcp_entry_and_preserves_user_environment() {
        let f = Fixture::new();
        let original = "# user preference\n[mcp_servers.muxa]\ncommand = '/opt/bin/muxa'\nargs = ['--socket', '/custom.sock', 'mcp']\nenv_vars = ['MY_VAR']\nstartup_timeout_sec = 90\n[mcp_servers.muxa.env]\nMY_TOKEN = 'test-secret'\n";
        write(&f.hosts[0].config, original);
        let previous = read_server(original, Host::Codex).unwrap();
        apply::run(&f.plan(Direction::Install, &[Component::AgentMcp]), false).unwrap();
        let installed = read_server(
            &fs::read_to_string(&f.hosts[0].config).unwrap(),
            Host::Codex,
        )
        .unwrap()
        .unwrap();
        assert_eq!(installed["args"], previous.as_ref().unwrap()["args"]);
        assert_eq!(installed["env_vars"][0], "MY_VAR");
        assert_eq!(installed["env"]["MY_TOKEN"], "test-secret");
        apply::run(&f.plan(Direction::Uninstall, &[Component::AgentMcp]), false).unwrap();
        assert_eq!(
            read_server(
                &fs::read_to_string(&f.hosts[0].config).unwrap(),
                Host::Codex
            )
            .unwrap(),
            previous
        );
    }

    #[test]
    fn unowned_mcp_name_and_skill_directory_are_preserved() {
        let f = Fixture::new();
        write(
            &f.hosts[0].config,
            "[mcp_servers.muxa]\ncommand = 'some-other-tool'\nargs = []\n",
        );
        write(&f.hosts[0].skill.join("SKILL.md"), "My skill");
        let plan = f.all(Direction::Install);
        assert_eq!(plan.warnings.len(), 2);
        apply::run(&plan, false).unwrap();
        apply::run(&f.all(Direction::Uninstall), false).unwrap();
        assert_eq!(
            fs::read_to_string(f.hosts[0].skill.join("SKILL.md")).unwrap(),
            "My skill"
        );
        assert!(fs::read_to_string(&f.hosts[0].config)
            .unwrap()
            .contains("some-other-tool"));
    }

    #[test]
    fn local_changes_survive_upgrade_and_uninstall() {
        let f = Fixture::new();
        apply::run(&f.all(Direction::Install), false).unwrap();
        let skill = f.root.join(SKILL_DIR).join("SKILL.md");
        write(&skill, "My modified skill");
        let path = &f.hosts[1].config;
        let original = fs::read_to_string(path).unwrap();
        let mut changed = read_server(&original, Host::Claude).unwrap().unwrap();
        changed["args"] = json!(["--socket", "/new.sock", "mcp"]);
        write(
            path,
            &write_server(&original, Host::Claude, Some(&changed)).unwrap(),
        );
        let reinstall = f.all(Direction::Install);
        assert!(reinstall.warnings.len() >= 2);
        apply::run(&reinstall, false).unwrap();
        let uninstall = f.all(Direction::Uninstall);
        assert!(uninstall.warnings.len() >= 2);
        apply::run(&uninstall, false).unwrap();
        assert_eq!(fs::read_to_string(&skill).unwrap(), "My modified skill");
        assert_eq!(
            read_server(&fs::read_to_string(path).unwrap(), Host::Claude).unwrap(),
            Some(changed)
        );
    }

    #[test]
    fn override_is_active_and_reinstall_migrates_the_entry_point() {
        let f = Fixture::new();
        apply::run(
            &f.plan(Direction::Install, &[Component::AgentInstructions]),
            false,
        )
        .unwrap();
        let override_path = f.hosts[0].home.join("AGENTS.override.md");
        write(&override_path, "Override instructions\n");
        assert!(f
            .checks()
            .iter()
            .any(|(ok, text)| !ok && text.contains("AGENTS.md")));
        apply::run(
            &f.plan(Direction::Install, &[Component::AgentInstructions]),
            false,
        )
        .unwrap();
        assert!(fs::read_to_string(&override_path)
            .unwrap()
            .contains("muxa managed"));
        assert!(!fs::read_to_string(f.hosts[0].home.join("AGENTS.md"))
            .unwrap()
            .contains("muxa managed"));
        assert!(f.checks().iter().all(|(ok, _)| *ok));
        apply::run(
            &f.plan(Direction::Uninstall, &[Component::AgentInstructions]),
            false,
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(&override_path).unwrap(),
            "Override instructions\n"
        );
    }

    #[test]
    fn component_uninstall_keeps_other_components_and_remembers_missing_hosts() {
        let f = Fixture::new();
        apply::run(&f.all(Direction::Install), false).unwrap();
        let mut plan = Plan {
            direction: Direction::Uninstall,
            components: vec![Component::AgentMcp],
            actions: vec![],
            warnings: vec![],
        };
        plan_bundle(&mut plan, &f.root, &[], None, Path::new("/unused.sock")).unwrap();
        apply::run(&plan, false).unwrap();
        assert!(f.hosts[0].skill.join("SKILL.md").is_file());
        assert!(fs::read_to_string(f.hosts[0].home.join("AGENTS.md"))
            .unwrap()
            .contains("muxa managed"));
        assert!(read_server(
            &fs::read_to_string(&f.hosts[0].config).unwrap(),
            Host::Codex
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn rejects_invalid_configs_before_any_writes() {
        for original in ["not-json", "[]", r#"{"mcpServers":[]}"#] {
            let f = Fixture::new();
            write(&f.hosts[1].config, original);
            let mut plan = Plan {
                direction: Direction::Install,
                components: vec![Component::AgentMcp],
                actions: vec![],
                warnings: vec![],
            };
            assert!(plan_bundle(&mut plan, &f.root, &f.hosts, None, Path::new("/socket")).is_err());
            assert!(!f.root.exists());
            assert!(!f.hosts[0].config.exists());
        }
    }

    #[test]
    fn malformed_instruction_markers_are_not_duplicated_or_removed() {
        let open = "<!-- >>> muxa managed (agent-instructions) >>> -->\n";
        assert!(instruction_edit(open, Some("new")).is_err());
        assert!(instruction_edit(open, None).is_err());
        let installed = instruction_edit("user\n", Some("body")).unwrap();
        assert_eq!(
            instruction_edit(&installed, Some("body")).unwrap(),
            installed
        );
        assert!(instruction_edit(&format!("{installed}{installed}"), None).is_err());
    }

    #[test]
    fn mcp_edits_compose_with_planned_hooks() {
        let f = Fixture::new();
        let baseline = "# original user config\nmodel='preferred'\n";
        write(&f.hosts[0].config, baseline);
        let mut plan = Plan {
            direction: Direction::Install,
            components: vec![Component::AgentMcp],
            actions: vec![],
            warnings: vec![],
        };
        let (hooks, _) = super::super::files::codex::upsert(baseline).unwrap();
        edit(
            &mut plan,
            Component::CodexHooks,
            f.hosts[0].config.clone(),
            hooks.clone(),
        )
        .unwrap();
        plan_bundle(&mut plan, &f.root, &f.hosts, None, Path::new("/socket")).unwrap();
        let report = apply::run(&plan, false).unwrap();
        let backups: Vec<_> = report
            .backups
            .iter()
            .filter(|p| p.parent() == Some(f.hosts[0].home.as_path()))
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(fs::read_to_string(backups[0]).unwrap(), baseline);
        let installed = fs::read_to_string(&f.hosts[0].config).unwrap();
        assert!(installed.contains("muxa hook codex"));
        assert!(read_server(&installed, Host::Codex).unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_dotfiles_keep_their_link_and_target() {
        let f = Fixture::new();
        let target = f.dir.path().join("dotfiles/global.md");
        write(&target, "My shared instructions\n");
        let path = f.hosts[0].home.join("AGENTS.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        apply::run(
            &f.plan(Direction::Install, &[Component::AgentInstructions]),
            false,
        )
        .unwrap();
        assert_eq!(fs::read_link(&path).unwrap(), target);
        assert!(fs::read_to_string(&target)
            .unwrap()
            .contains("muxa managed"));
        apply::run(
            &f.plan(Direction::Uninstall, &[Component::AgentInstructions]),
            false,
        )
        .unwrap();
        assert_eq!(fs::read_link(&path).unwrap(), target);
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "My shared instructions\n"
        );
    }

    #[test]
    fn refuses_config_changes_between_plan_and_apply() {
        let f = Fixture::new();
        write(&f.hosts[0].config, "model='first'\n");
        let plan = f.plan(Direction::Install, &[Component::AgentMcp]);
        write(&f.hosts[0].config, "model='changed-by-user'\n");
        assert!(apply::run(&plan, false).is_err());
        assert_eq!(
            fs::read_to_string(&f.hosts[0].config).unwrap(),
            "model='changed-by-user'\n"
        );
        // Ownership was recorded before the interrupted install; retry recovers.
        apply::run(&f.plan(Direction::Install, &[Component::AgentMcp]), false).unwrap();
        assert!(fs::read_to_string(&f.hosts[0].config)
            .unwrap()
            .contains("changed-by-user"));
    }

    #[cfg(unix)]
    #[test]
    fn modified_link_and_replaced_asset_are_not_removed_after_planning() {
        let f = Fixture::new();
        apply::run(
            &f.plan(Direction::Install, &[Component::AgentSkills]),
            false,
        )
        .unwrap();
        let plan = f.plan(Direction::Uninstall, &[Component::AgentSkills]);
        let skill = f.root.join(SKILL_DIR).join("SKILL.md");
        write(&skill, "Changed after planning");
        assert!(apply::run(&plan, false).is_err());
        assert_eq!(fs::read_to_string(skill).unwrap(), "Changed after planning");
        if fs::symlink_metadata(&f.hosts[0].skill).is_ok() {
            fs::remove_file(&f.hosts[0].skill).unwrap();
        }
        let other = f.dir.path().join("other-skill");
        fs::create_dir(&other).unwrap();
        std::os::unix::fs::symlink(&other, &f.hosts[0].skill).unwrap();
        let plan = f.plan(Direction::Uninstall, &[Component::AgentSkills]);
        apply::run(&plan, false).unwrap();
        assert_eq!(fs::read_link(&f.hosts[0].skill).unwrap(), other);
    }

    #[test]
    fn toml_inline_mcp_tables_round_trip() {
        let original = "mcp_servers = { muxa = { command = 'muxa', args = ['mcp'] }, other = { command = 'other' } }\n";
        let mut server = read_server(original, Host::Codex).unwrap().unwrap();
        server["env_vars"] = json!(CODEX_ENV);
        let updated = write_server(original, Host::Codex, Some(&server)).unwrap();
        assert_eq!(read_server(&updated, Host::Codex).unwrap(), Some(server));
        let removed = write_server(&updated, Host::Codex, None).unwrap();
        assert!(read_server(&removed, Host::Codex).unwrap().is_none());
        assert!(removed.contains("other"));
    }

    #[cfg(unix)]
    #[test]
    fn both_agents_can_share_one_symlinked_global_instruction_file() {
        let f = Fixture::new();
        let target = f.dir.path().join("shared-instructions.md");
        write(&target, "Shared user rules\n");
        for (host, name) in f.hosts.iter().zip(["AGENTS.md", "CLAUDE.md"]) {
            fs::create_dir_all(&host.home).unwrap();
            std::os::unix::fs::symlink(&target, host.home.join(name)).unwrap();
        }
        apply::run(
            &f.plan(Direction::Install, &[Component::AgentInstructions]),
            false,
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(&target)
                .unwrap()
                .matches("<!-- >>> muxa managed")
                .count(),
            1
        );
        apply::run(
            &f.plan(Direction::Uninstall, &[Component::AgentInstructions]),
            false,
        )
        .unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "Shared user rules\n");
    }

    #[test]
    fn interrupted_bundle_and_mcp_upgrade_recovers_from_the_write_ahead_manifest() {
        let f = Fixture::new();
        apply::run(&f.all(Direction::Install), false).unwrap();
        let manifest_path = f.root.join("manifest.json");
        let mut manifest = load_manifest(&manifest_path).unwrap();
        manifest.bundle_version = "older-version".into();
        for record in &mut manifest.records {
            match &mut record.kind {
                RecordKind::Asset { installed, .. } if record.path.ends_with("SKILL.md") => {
                    *installed = "Previous bundled skill\n".into();
                    write(&record.path, installed);
                }
                RecordKind::Mcp {
                    host: Host::Codex,
                    installed,
                    ..
                } => {
                    installed["env_vars"] = json!(["TMUX"]);
                    let original = fs::read_to_string(&record.path).unwrap();
                    write(
                        &record.path,
                        &write_server(&original, Host::Codex, Some(installed)).unwrap(),
                    );
                }
                _ => {}
            }
        }
        write(
            &manifest_path,
            &serde_json::to_string_pretty(&manifest).unwrap(),
        );
        let mut interrupted = f.all(Direction::Install);
        let manifest_action = interrupted.actions.remove(0);
        interrupted.actions = vec![manifest_action];
        apply::run(&interrupted, false).unwrap();
        assert_ne!(
            fs::read_to_string(f.root.join(SKILL_DIR).join("SKILL.md")).unwrap(),
            SKILL
        );
        let retry = f.all(Direction::Install);
        assert!(retry.warnings.is_empty(), "{:?}", retry.warnings);
        apply::run(&retry, false).unwrap();
        assert!(f.checks().iter().all(|(ok, _)| *ok));
        assert_eq!(
            fs::read_to_string(f.hosts[0].skill.join("SKILL.md")).unwrap(),
            SKILL
        );
        assert!(!f.all(Direction::Install).has_changes());
    }

    #[test]
    fn doctor_reports_skipped_collisions_and_disabled_servers() {
        let f = Fixture::new();
        write(
            &f.hosts[0].config,
            "[mcp_servers.muxa]\ncommand='muxa'\nargs=['mcp']\nenabled=false\n",
        );
        write(&f.hosts[1].skill.join("SKILL.md"), "User skill");
        apply::run(&f.all(Direction::Install), false).unwrap();
        let checks = f.checks();
        assert!(checks
            .iter()
            .any(|(ok, message)| !ok && message.contains("agent-mcp")));
        assert!(checks
            .iter()
            .any(|(ok, message)| !ok && message.contains("agent-skills")));
        let config = fs::read_to_string(&f.hosts[0].config).unwrap();
        assert_eq!(
            read_server(&config, Host::Codex).unwrap().unwrap()["enabled"],
            false
        );
    }
}
