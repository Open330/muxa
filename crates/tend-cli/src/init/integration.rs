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
    include_str!("../../assets/agent-integration/skills/tend-collaboration/SKILL.md");
const WORKFLOWS: &str = include_str!(
    "../../assets/agent-integration/skills/tend-collaboration/references/workflows.md"
);
const SKILL_DIR: &str = "skills/tend-collaboration";
const BLOCK: &str = "agent-instructions";
pub const CODEX_ENV: &[&str] = &["RMUX", "RMUX_PANE", "TMUX", "TMUX_PANE", "TEND_SOCKET"];

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
    /// Gets the MCP server only (another account's home).
    mcp_only: bool,
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
                skill: home.join(".agents/skills/tend-collaboration"),
                mcp_only: false,
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
                skill: claude.join("skills/tend-collaboration"),
                home: claude,
                mcp_only: false,
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
        .or_else(tend::paths::default_config_file)
        .context("cannot locate tend config for agent integration")?;
    let config = absolute(&config)?;
    Ok(config
        .parent()
        .context("tend config has no parent")?
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
    all_homes: bool,
) -> Result<()> {
    if !plan.components.iter().any(|c| is_component(*c)) {
        return Ok(());
    }
    let root = bundle_root(config)?;
    let mut hosts = hosts(detect)?;
    if plan.direction == Direction::Install && plan.components.contains(&Component::AgentMcp) {
        let extra = mcp_only_hosts(plan, &hosts, config, all_homes)?;
        hosts.extend(extra);
    }
    plan_bundle(plan, &root, &hosts, config, socket)
}

/// Provider homes besides the active ones that also get the MCP server:
/// every configured account's home, and with `all_homes` every home a
/// running agent reads. They get the MCP entry only; instructions and the
/// skill stay with the active home.
fn mcp_only_hosts(
    plan: &mut Plan,
    active: &[HostPaths],
    config: Option<&Path>,
    all_homes: bool,
) -> Result<Vec<HostPaths>> {
    let user_home = dirs::home_dir().context("cannot locate user home for agent integration")?;
    let active: Vec<(Host, &'static str, PathBuf)> = active
        .iter()
        .map(|h| (h.host, home_var(h.host), h.config.clone()))
        .collect();
    let mut unchecked = Unchecked::default();
    let accounts = account_configs(config, &user_home, &mut unchecked);
    let live = if all_homes {
        live_agent_configs(&mut unchecked)
    } else {
        Vec::new()
    };
    let mut out = Vec::new();
    for home in other_homes(&active, accounts, live) {
        if !active.iter().any(|(host, _, _)| *host == home.host) {
            continue;
        }
        // One spelling per file, so repeated installs through different
        // aliases keep one ownership record.
        let config_path = canonical(&home.config);
        if mcp_state(home.host, &config_path) == McpState::Unreadable {
            plan.warnings.push(format!(
                "{} cannot be read or parsed; skipped its tend MCP registration",
                config_path.display()
            ));
            continue;
        }
        let dir = config_path.parent().unwrap_or(&config_path).to_path_buf();
        out.push(HostPaths {
            host: home.host,
            skill: dir.join(SKILL_DIR),
            home: dir,
            config: config_path,
            mcp_only: true,
        });
    }
    if unchecked.accounts > 0 || unchecked.processes > 0 {
        plan.warnings.push(format!(
            "Not every provider home could be checked: {} account(s) set theirs through the shell or an unreadable env_file, {} provider process(es) had an unreadable environment",
            unchecked.accounts, unchecked.processes
        ));
    }
    Ok(out)
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
            for host in hosts.iter().filter(|h| !h.mcp_only) {
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
            for host in hosts.iter().filter(|h| !h.mcp_only) {
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
    let open = "<!-- >>> tend managed (agent-instructions) >>> -->";
    let close = "<!-- <<< tend managed (agent-instructions) <<< -->";
    let opens = original.lines().filter(|line| *line == open).count();
    let closes = original.lines().filter(|line| *line == close).count();
    if opens != closes || opens > 1 || (opens == 1 && original.find(open) > original.find(close)) {
        bail!("malformed or duplicate Tend instruction block; repair the markers before re-running init");
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
    let body = format!("For Tend peer collaboration, @peer/@tend-peer requests, or Tend tmux\nworkspace/work/agent layout, read the shared entry point at:\n{}\nUse these instructions only for Tend work; keep the user's existing task scope.", root.join("bootstrap.md").display());
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
        .and_then(|v| v.get("tend"))
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
                    .insert("tend".into(), server.clone());
            } else if let Some(servers) = obj.get_mut("mcpServers").and_then(Value::as_object_mut) {
                servers.remove("tend");
            }
            // Keep the file in the style it was written in: Claude Code and
            // other tools may keep it on one line, and a pretty rewrite of a
            // long file turns a one-entry edit into a thousand-line diff.
            let compact = !original.trim().is_empty() && !original.trim_end().contains('\n');
            Ok(if compact {
                format!("{}\n", serde_json::to_string(&root)?)
            } else {
                format!("{}\n", serde_json::to_string_pretty(&root)?)
            })
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
                if servers.get("tend").is_none() {
                    servers.insert("tend", toml_edit::Item::Table(fragment.as_table().clone()));
                } else {
                    let table = servers
                        .get_mut("tend")
                        .and_then(toml_edit::Item::as_table_like_mut)
                        .context("tend MCP entry must be a table")?;
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
                servers.remove("tend");
            }
            Ok(doc.to_string())
        }
    }
}

fn new_mcp_server(config: Option<&Path>, socket: &Path) -> Result<Value> {
    let mut args = Vec::new();
    // Default socket remains environment-routed, so pane-local sockets work.
    if socket != tend::paths::default_socket() {
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
    Ok(json!({"command": "tend", "args": args}))
}

fn is_tend_server(server: &Value) -> bool {
    let command = server
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default();
    Path::new(command).file_name().and_then(|s| s.to_str()) == Some("tend")
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
        if !crate::mcp_revision::entries_equal(current.as_ref(), Some(installed))
            && !crate::mcp_revision::entries_equal(current.as_ref(), previous.as_ref())
            && !crate::mcp_revision::entries_equal(current.as_ref(), before_apply.as_ref())
            && current.is_some()
        {
            warn_preserved(plan, &host.config, "Tend MCP registration changed since installation; reconcile it before re-running agent-mcp");
            return Ok(());
        }
    }
    let mut desired = current.clone().unwrap_or_else(|| json!({}));
    if current.is_some() {
        if !is_tend_server(&desired) {
            conflict(
                plan,
                manifest,
                Component::AgentMcp,
                &host.config,
                "MCP name tend belongs to a different command/transport",
            );
            return Ok(());
        }
    } else {
        desired = new_mcp_server(config, socket)?;
    }
    if host.host == Host::Codex {
        let obj = desired
            .as_object_mut()
            .context("tend MCP entry must be an object")?;
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
            if current.is_some()
                && (crate::mcp_revision::entries_equal(current.as_ref(), Some(installed))
                    || crate::mcp_revision::entries_equal(current.as_ref(), before_apply.as_ref()))
            {
                let after = write_server(&original, *host, previous.as_ref())?;
                edit(plan, component, record.path.clone(), after)?;
            } else if current.is_some()
                && !crate::mcp_revision::entries_equal(current.as_ref(), previous.as_ref())
            {
                warn_preserved(
                    plan,
                    &record.path,
                    "Tend MCP registration has local changes; left installed",
                );
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Read-only checks shared by init verification and doctor. No agent is spawned.
pub fn diagnostics(config: Option<&Path>) -> Result<Vec<(bool, String)>> {
    let active = active_mcp_configs();
    let mut out = diagnostics_with(config, active.clone())?;
    if let Some(user_home) = dirs::home_dir() {
        let mut unchecked = Unchecked::default();
        let accounts = account_configs(config, &user_home, &mut unchecked);
        let live = live_agent_configs(&mut unchecked);
        let homes = other_homes(&active, accounts, live);
        let recorded: Vec<PathBuf> = bundle_root(config)
            .ok()
            .and_then(|root| load_manifest(&root.join("manifest.json")).ok())
            .map(|m| m.records.into_iter().map(|r| r.path).collect())
            .unwrap_or_default();
        out.extend(other_home_gaps(&homes, &recorded, &unchecked));
    }
    Ok(out)
}

/// [`diagnostics`] against an explicit set of active provider configs.
fn diagnostics_with(
    config: Option<&Path>,
    active: Vec<(Host, &'static str, PathBuf)>,
) -> Result<Vec<(bool, String)>> {
    let root = bundle_root(config)?;
    let manifest_path = root.join("manifest.json");
    if !manifest_path.exists() {
        return Ok(vec![(false, "Agent integration not installed; run tend init --component agent-instructions,agent-skills,agent-mcp".into())]);
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
                    .is_some_and(|current| {
                        crate::mcp_revision::entries_equal(Some(current), Some(installed))
                    })
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
    out.extend(active_home_gaps(&manifest_path, active));
    Ok(out)
}

/// The config file each provider reads in this environment. A provider's
/// own variable picks its home (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`), so the
/// home a session runs under can differ from the one `tend init` last
/// wrote; tend only follows those variables and knows nothing about what
/// set them.
fn active_mcp_configs() -> Vec<(Host, &'static str, PathBuf)> {
    let mut out = Vec::new();
    if let Some(home) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        out.push((
            Host::Claude,
            "CLAUDE_CONFIG_DIR",
            PathBuf::from(home).join(".claude.json"),
        ));
    }
    if let Some(home) = std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()) {
        out.push((
            Host::Codex,
            "CODEX_HOME",
            PathBuf::from(home).join("config.toml"),
        ));
    }
    out
}

/// Active provider homes whose config has no tend MCP server and that the
/// ownership manifest does not cover: the manifest only re-checks files it
/// installed, so a home that was never set up would otherwise pass.
fn active_home_gaps(
    manifest_path: &Path,
    configs: Vec<(Host, &'static str, PathBuf)>,
) -> Vec<(bool, String)> {
    let recorded: Vec<PathBuf> = load_manifest(manifest_path)
        .map(|m| m.records.into_iter().map(|r| r.path).collect())
        .unwrap_or_default();
    configs
        .into_iter()
        .filter(|(_, _, path)| !recorded.contains(path))
        .filter_map(|(host, var, path)| {
            let home = path.parent().unwrap_or(&path).display().to_string();
            let problem = match mcp_state(host, &path) {
                McpState::Healthy => return None,
                McpState::Missing => format!(
                    "has no tend server, and it is the active {var}; run `{var}={home} tend init --component agent-mcp`"
                ),
                McpState::Disabled => format!(
                    "has the tend server disabled, and it is the active {var}; re-enable it, or run `{var}={home} tend init --component agent-mcp`"
                ),
                McpState::Conflict => format!(
                    "has a `tend` server that does not run `tend mcp`, and it is the active {var}; reconcile that entry by hand"
                ),
                McpState::Unreadable => {
                    format!("cannot be read or parsed, and it is the active {var}; fix the file")
                }
            };
            Some((false, format!("agent-mcp: {} {problem}", path.display())))
        })
        .collect()
}

/// Why tend knows about a provider home other than the active one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HomeSource {
    /// `[[accounts.<provider>]]` names it in config.toml.
    Account,
    /// A running agent of this user reads it.
    Live,
}

/// A provider config other agents read, beyond the active home.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OtherHome {
    host: Host,
    /// The provider's config file (`.claude.json`, `config.toml`).
    config: PathBuf,
    source: HomeSource,
    /// Running agents seen reading it.
    agents: usize,
}

fn home_var(host: Host) -> &'static str {
    match host {
        Host::Claude => "CLAUDE_CONFIG_DIR",
        Host::Codex => "CODEX_HOME",
    }
}

/// The config file a provider reads with `var` set to `home`, or unset.
fn provider_config(host: Host, home: Option<&Path>, user_home: &Path) -> PathBuf {
    match (host, home) {
        (Host::Claude, Some(home)) => home.join(".claude.json"),
        (Host::Claude, None) => user_home.join(".claude.json"),
        (Host::Codex, Some(home)) => home.join("config.toml"),
        (Host::Codex, None) => user_home.join(".codex/config.toml"),
    }
}

/// One path per file, whatever symlinks lead to it: aas profiles often link
/// their files to a shared one. A missing file resolves through its nearest
/// existing ancestor, so a home that does not exist yet keeps one spelling.
fn canonical(path: &Path) -> PathBuf {
    if let Ok(real) = fs::canonicalize(path) {
        return real;
    }
    let mut rest = Vec::new();
    let mut cur = path;
    while let Some(parent) = cur.parent() {
        if let Some(name) = cur.file_name() {
            rest.push(name.to_owned());
        }
        if let Ok(real) = fs::canonicalize(parent) {
            return rest.iter().rev().fold(real, |acc, name| acc.join(name));
        }
        cur = parent;
    }
    path.to_path_buf()
}

/// The value of one shell assignment's right-hand side, when it is a plain
/// literal: an unquoted word, `'…'`, or `"…"` without expansions, optionally
/// followed by a `# comment`. `None` for anything the shell would expand or
/// that this subset does not cover; such a value is never guessed.
fn shell_literal(raw: &str) -> Option<(String, bool)> {
    // `KEY= word` runs `word` as a command with KEY empty; it is not an
    // assignment of `word`. Only blanks or a comment may follow a bare `=`.
    if raw.starts_with(char::is_whitespace) {
        let rest = raw.trim_start();
        return (rest.is_empty() || rest.starts_with('#')).then(|| (String::new(), false));
    }
    let (value, rest) = if let Some(body) = raw.strip_prefix('\'') {
        let end = body.find('\'')?;
        ((body[..end].to_owned(), true), &body[end + 1..])
    } else if let Some(body) = raw.strip_prefix('"') {
        let end = body.find('"')?;
        let value = &body[..end];
        if value.contains(['$', '`', '\\']) {
            return None;
        }
        ((value.to_owned(), true), &body[end + 1..])
    } else {
        let end = raw.find(char::is_whitespace).unwrap_or(raw.len());
        let value = &raw[..end];
        if value.contains([
            '$', '`', '\\', '\'', '"', '#', ';', '&', '|', '(', ')', '<', '>',
        ]) {
            return None;
        }
        ((value.to_owned(), false), &raw[end..])
    };
    // Anything after the value must be blanks, or blanks then a comment:
    // `'a'#b` is the single word `a#b` to the shell.
    let rest_ok = rest.trim().is_empty()
        || (rest.starts_with(char::is_whitespace) && rest.trim_start().starts_with('#'));
    rest_ok.then_some(value)
}

/// The provider home an account selects, read without running anything.
/// Launch sources the `env_file` and then applies `env`, so an `env` key
/// decides whenever present, even empty. In the file the last assignment
/// wins, as in the shell. `Ok(None)`: the account leaves the home to the
/// launch environment (no key, or an empty value, which providers treat as
/// unset). `Err`: the value depends on the shell, is relative, or the file
/// cannot be read; it is reported as not checked, never guessed.
fn account_home(
    profile: &tend::accounts::AccountProfile,
    var: &str,
) -> Result<Option<PathBuf>, ()> {
    let resolve = |value: String| -> Result<Option<PathBuf>, ()> {
        if value.is_empty() {
            return Ok(None);
        }
        let path = PathBuf::from(tend::accounts::expand_home(&value));
        if path.is_absolute() {
            Ok(Some(path))
        } else {
            Err(())
        }
    };
    if let Some(value) = profile.env.get(var) {
        if value.contains(['$', '`']) {
            return Err(());
        }
        return resolve(value.clone());
    }
    let Some(file) = &profile.env_file else {
        return Ok(None);
    };
    let text = fs::read_to_string(tend::accounts::expand_home(file)).map_err(|_| ())?;
    // Only a file of plain assignments is read: any other line (a
    // conditional, a function, a heredoc, a continuation) could decide
    // whether an assignment runs at all, so the whole file is unchecked.
    let mut found = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let (key, raw) = line.split_once('=').ok_or(())?;
        if key.is_empty()
            || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            || key.starts_with(|c: char| c.is_ascii_digit())
            || raw.ends_with('\\')
        {
            return Err(());
        }
        let (value, quoted) = shell_literal(raw).ok_or(())?;
        if key == var {
            found = Some((value, quoted));
        }
    }
    match found {
        None => Ok(None),
        Some((value, _)) if value.is_empty() => Ok(None),
        // The shell expands an unquoted leading `~/` only; a quoted one
        // stays a relative path.
        Some((value, quoted)) => {
            let value = if quoted {
                value
            } else {
                tend::accounts::expand_home(&value)
            };
            let path = PathBuf::from(value);
            if path.is_absolute() {
                Ok(Some(path))
            } else {
                Err(())
            }
        }
    }
}

/// What discovery could not check, reported as incomplete coverage.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Unchecked {
    /// Accounts whose home depends on the shell or an unreadable `env_file`.
    accounts: usize,
    /// Own provider processes whose environment could not be read.
    processes: usize,
}

/// Provider homes the configured accounts select.
fn account_configs(
    config: Option<&Path>,
    user_home: &Path,
    unchecked: &mut Unchecked,
) -> Vec<(Host, PathBuf)> {
    let Ok(cfg) = tend::config::Config::load_or_default(config) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (host, provider) in [(Host::Claude, "claude"), (Host::Codex, "codex")] {
        for profile in cfg.accounts.for_provider(provider) {
            match account_home(profile, home_var(host)) {
                Ok(Some(home)) => out.push((host, provider_config(host, Some(&home), user_home))),
                Ok(None) => {}
                Err(()) => unchecked.accounts += 1,
            }
        }
    }
    out
}

/// The provider a process runs, from its command line: the program, or the
/// script a `node` launcher runs. Codex app-servers count (they run attached
/// threads' MCP with their own `CODEX_HOME`); helpers that share a Claude
/// session's config (its background daemon) do not.
fn agent_host(args: &str) -> Option<Host> {
    let mut words = args.split_whitespace();
    let mut program = words.next()?;
    let base = |word: &str| word.rsplit('/').next().unwrap_or(word).to_owned();
    if base(program) == "node" {
        program = words.next()?;
    }
    let next = words.next().unwrap_or_default();
    if matches!(next, "daemon" | "bg-pty-host" | "mcp") {
        return None;
    }
    match base(program).as_str() {
        "claude" => Some(Host::Claude),
        "codex" => Some(Host::Codex),
        _ => None,
    }
}

/// The config a process with environment `environ` reads: its provider
/// variable when absolute, else its own `HOME`'s default. `None` when that
/// cannot be told (a relative home, no `HOME`).
fn process_config(host: Host, environ: &[(String, String)]) -> Option<PathBuf> {
    let get = |key: &str| {
        environ
            .iter()
            .find(|(k, v)| k == key && !v.is_empty())
            .map(|(_, v)| PathBuf::from(v))
    };
    match get(home_var(host)) {
        Some(home) if home.is_absolute() => {
            Some(provider_config(host, Some(&home), Path::new("/")))
        }
        Some(_) => None,
        None => {
            let home = get("HOME").filter(|h| h.is_absolute())?;
            Some(provider_config(host, None, &home))
        }
    }
}

/// `(host, config file)` for each running provider process of this user, in
/// one process snapshot. Only the provider variable and `HOME` are read from
/// each environment; nothing is kept or printed.
#[cfg(unix)]
fn live_agent_configs(unchecked: &mut Unchecked) -> Vec<(Host, PathBuf)> {
    let uid = std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let Some(uid) = uid else {
        return Vec::new();
    };
    let Ok(output) = std::process::Command::new("ps")
        .args(["-U", &uid, "-o", "pid=,args="])
        .output()
    else {
        return Vec::new();
    };
    let agents: Vec<(u32, Host)> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let (pid, args) = line.trim_start().split_once(char::is_whitespace)?;
            Some((pid.parse().ok()?, agent_host(args.trim_start())?))
        })
        .collect();
    let pids: Vec<u32> = agents.iter().map(|(pid, _)| *pid).collect();
    let environs = tend::accounts::processes_environ_raw(&pids);
    let mut out = Vec::new();
    for (pid, host) in agents {
        match environs
            .get(&pid)
            .cloned()
            .flatten()
            .map(|raw| tend::accounts::parse_environ(&raw))
            .filter(|environ| !environ.is_empty())
            .and_then(|environ| process_config(host, &environ))
        {
            Some(config) => out.push((host, config)),
            None => unchecked.processes += 1,
        }
    }
    out
}

#[cfg(not(unix))]
fn live_agent_configs(_unchecked: &mut Unchecked) -> Vec<(Host, PathBuf)> {
    Vec::new()
}

/// Configured and live homes that are not one of `active`, one entry per
/// config file, with the number of running agents reading each.
fn other_homes(
    active: &[(Host, &'static str, PathBuf)],
    accounts: Vec<(Host, PathBuf)>,
    live: Vec<(Host, PathBuf)>,
) -> Vec<OtherHome> {
    let active: Vec<PathBuf> = active.iter().map(|(_, _, path)| canonical(path)).collect();
    let mut out: Vec<OtherHome> = Vec::new();
    let mut add = |host: Host, config: PathBuf, source: HomeSource, agent: bool| {
        let key = canonical(&config);
        if active.contains(&key) {
            return;
        }
        if let Some(seen) = out
            .iter_mut()
            .find(|home| home.host == host && canonical(&home.config) == key)
        {
            seen.agents += usize::from(agent);
            return;
        }
        out.push(OtherHome {
            host,
            config,
            source,
            agents: usize::from(agent),
        });
    };
    for (host, config) in accounts {
        add(host, config, HomeSource::Account, false);
    }
    for (host, config) in live {
        add(host, config, HomeSource::Live, true);
    }
    out
}

/// What is wrong with a home's tend MCP registration, if anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpState {
    Healthy,
    Missing,
    Disabled,
    /// A server named `tend` that runs something else.
    Conflict,
    /// The config cannot be read or parsed.
    Unreadable,
}

fn mcp_state(host: Host, config: &Path) -> McpState {
    let text = match fs::read_to_string(config) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return McpState::Missing,
        Err(_) => return McpState::Unreadable,
    };
    match read_server(&text, host) {
        Err(_) => McpState::Unreadable,
        Ok(None) => McpState::Missing,
        Ok(Some(server)) if !is_tend_server(&server) => McpState::Conflict,
        Ok(Some(server)) if server.get("enabled").and_then(Value::as_bool) == Some(false) => {
            McpState::Disabled
        }
        Ok(Some(_)) => McpState::Healthy,
    }
}

/// Other homes whose tend MCP registration is missing or broken. A home the
/// manifest records is already re-checked by the manifest lines, so it only
/// shows here when it is broken and processes depend on it.
fn other_home_gaps(
    homes: &[OtherHome],
    recorded: &[PathBuf],
    unchecked: &Unchecked,
) -> Vec<(bool, String)> {
    let recorded: Vec<PathBuf> = recorded.iter().map(|p| canonical(p)).collect();
    let mut out: Vec<(bool, String)> = homes
        .iter()
        .filter_map(|home| {
            let state = mcp_state(home.host, &home.config);
            let is_recorded = recorded.contains(&canonical(&home.config));
            if state == McpState::Healthy || (is_recorded && home.agents == 0) {
                return None;
            }
            let why = match (home.source, home.agents) {
                (HomeSource::Account, 0) => "a configured account uses it".to_owned(),
                (_, n) => format!("{n} running provider process(es) read it"),
            };
            let (what, fix) = match state {
                McpState::Missing => (
                    "has no tend server",
                    "run `tend init --component agent-mcp --all-homes`",
                ),
                McpState::Disabled => (
                    "has the tend server disabled",
                    "re-enable it, or run `tend init --component agent-mcp --all-homes`",
                ),
                McpState::Conflict => (
                    "has a `tend` server that does not run `tend mcp`",
                    "reconcile that entry by hand; tend init leaves it alone",
                ),
                McpState::Unreadable => (
                    "cannot be read or parsed",
                    "fix the file; tend init skips it",
                ),
                McpState::Healthy => unreachable!(),
            };
            let restart = if home.agents > 0 && state != McpState::Unreadable {
                "; running agents load it on their next start"
            } else {
                ""
            };
            Some((
                false,
                format!(
                    "agent-mcp: {} {what}, and {why}; {fix}{restart}",
                    home.config.display()
                ),
            ))
        })
        .collect();
    if unchecked.accounts > 0 || unchecked.processes > 0 {
        out.push((
            true,
            format!(
                "agent-mcp: provider homes not checked: {} account(s) set theirs through the shell or an unreadable env_file, {} own provider process(es) had an unreadable environment",
                unchecked.accounts, unchecked.processes
            ),
        ));
    }
    out
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
                    skill: dir.path().join(".agents/skills/tend-collaboration"),
                    mcp_only: false,
                },
                HostPaths {
                    host: Host::Claude,
                    home: dir.path().join("custom claude"),
                    config: dir.path().join("custom claude/.claude.json"),
                    skill: dir.path().join("custom claude/skills/tend-collaboration"),
                    mcp_only: false,
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
            // The caller's own provider homes are not part of a fixture.
            diagnostics_with(
                Some(&self.root.parent().unwrap().join("config.toml")),
                Vec::new(),
            )
            .unwrap()
        }
    }

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn a_compact_claude_config_stays_compact_and_a_pretty_one_stays_pretty() {
        let server = json!({"type": "stdio", "command": "tend", "args": ["mcp"]});
        let compact = "{\"theme\":\"dark\",\"projects\":{}}";
        let out = write_server(compact, Host::Claude, Some(&server)).unwrap();
        assert_eq!(out.trim_end().lines().count(), 1, "{out}");
        assert!(out.contains("\"tend\""), "{out}");
        let pretty = "{\n  \"theme\": \"dark\"\n}\n";
        let out = write_server(pretty, Host::Claude, Some(&server)).unwrap();
        assert!(out.lines().count() > 3, "{out}");
        let fresh = write_server("", Host::Claude, Some(&server)).unwrap();
        assert!(fresh.lines().count() > 3, "{fresh}");
    }

    #[test]
    fn agent_processes_are_told_apart_from_their_helpers() {
        assert_eq!(
            agent_host("/Users/u/.local/bin/claude --resume abc"),
            Some(Host::Claude)
        );
        assert_eq!(agent_host("claude"), Some(Host::Claude));
        assert_eq!(
            agent_host("/opt/homebrew/bin/codex --yolo resume x"),
            Some(Host::Codex)
        );
        assert_eq!(
            agent_host("node /usr/lib/node_modules/@openai/codex/bin/codex"),
            Some(Host::Codex)
        );
        assert_eq!(
            agent_host("/Users/u/.local/bin/claude daemon run --origin transient"),
            None
        );
        assert_eq!(
            agent_host("claude bg-pty-host --bg-pty-host /tmp/x.sock"),
            None
        );
        assert_eq!(agent_host("/opt/homebrew/bin/tend mcp"), None);
        assert_eq!(agent_host("zsh -c claude"), None);
    }

    #[test]
    fn other_homes_count_agents_once_per_file_and_skip_the_active_home() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared claude");
        fs::create_dir_all(&shared).unwrap();
        fs::write(shared.join(".claude.json"), "{}").unwrap();
        // An aas-style profile whose config links to the shared file.
        let linked = dir.path().join("profile b");
        fs::create_dir_all(&linked).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(shared.join(".claude.json"), linked.join(".claude.json"))
            .unwrap();
        let active_dir = dir.path().join("active");
        fs::create_dir_all(&active_dir).unwrap();
        let active = vec![(
            Host::Claude,
            "CLAUDE_CONFIG_DIR",
            active_dir.join(".claude.json"),
        )];
        let homes = other_homes(
            &active,
            vec![(Host::Claude, shared.join(".claude.json"))],
            vec![
                (Host::Claude, shared.join(".claude.json")),
                (Host::Claude, linked.join(".claude.json")),
                (Host::Claude, active_dir.join(".claude.json")),
                (Host::Codex, dir.path().join("codex/config.toml")),
            ],
        );
        assert_eq!(homes.len(), 2, "{homes:?}");
        assert_eq!(homes[0].config, shared.join(".claude.json"));
        assert_eq!(homes[0].source, HomeSource::Account);
        assert_eq!(homes[0].agents, if cfg!(unix) { 2 } else { 1 });
        assert_eq!(homes[1].host, Host::Codex);
        assert_eq!(homes[1].agents, 1);
    }

    #[test]
    fn other_homes_without_tend_are_reported_with_one_fix() {
        let dir = tempfile::tempdir().unwrap();
        let bare = dir.path().join("bare/.claude.json");
        let wired = dir.path().join("wired/.claude.json");
        fs::create_dir_all(bare.parent().unwrap()).unwrap();
        fs::create_dir_all(wired.parent().unwrap()).unwrap();
        fs::write(&bare, "{}").unwrap();
        fs::write(
            &wired,
            "{\"mcpServers\":{\"tend\":{\"command\":\"tend\",\"args\":[\"mcp\"]}}}",
        )
        .unwrap();
        let account = dir.path().join("account/.claude.json");
        let homes = vec![
            OtherHome {
                host: Host::Claude,
                config: bare.clone(),
                source: HomeSource::Live,
                agents: 14,
            },
            OtherHome {
                host: Host::Claude,
                config: wired,
                source: HomeSource::Live,
                agents: 3,
            },
            OtherHome {
                host: Host::Claude,
                config: account.clone(),
                source: HomeSource::Account,
                agents: 0,
            },
        ];
        let gaps = other_home_gaps(&homes, &[], &Unchecked::default());
        assert_eq!(gaps.len(), 2, "{gaps:?}");
        assert!(
            gaps[0].1.contains(&bare.display().to_string()),
            "{}",
            gaps[0].1
        );
        assert!(
            gaps[0]
                .1
                .contains("14 running provider process(es) read it"),
            "{}",
            gaps[0].1
        );
        assert!(
            gaps[0]
                .1
                .contains("`tend init --component agent-mcp --all-homes`"),
            "{}",
            gaps[0].1
        );
        assert!(
            gaps[0]
                .1
                .ends_with("running agents load it on their next start"),
            "{}",
            gaps[0].1
        );
        assert!(
            gaps[1].1.contains("a configured account uses it"),
            "{}",
            gaps[1].1
        );
        assert!(gaps[1].1.contains(&account.display().to_string()));
    }

    #[test]
    fn account_homes_are_read_literally_with_launch_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("acct.env");
        fs::write(
            &file,
            "export CLAUDE_CONFIG_DIR=\"/from/file\"\nCLAUDE_CODE_OAUTH_TOKEN=x\n",
        )
        .unwrap();
        let profile =
            |env: &[(&str, &str)], env_file: Option<&Path>| tend::accounts::AccountProfile {
                name: "a".into(),
                env: env
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
                env_file: env_file.map(|p| p.display().to_string()),
            };
        let var = "CLAUDE_CONFIG_DIR";
        // An explicit entry wins over the sourced file, as at launch.
        assert_eq!(
            account_home(&profile(&[(var, "/explicit")], Some(&file)), var),
            Ok(Some("/explicit".into()))
        );
        assert_eq!(
            account_home(&profile(&[], Some(&file)), var),
            Ok(Some("/from/file".into()))
        );
        // A token-only account inherits the launch environment: no home.
        assert_eq!(
            account_home(&profile(&[("CLAUDE_CODE_OAUTH_TOKEN", "t")], None), var),
            Ok(None)
        );
        // Shell-dependent or relative values are not guessed, and nothing is run.
        assert_eq!(
            account_home(&profile(&[(var, "$HOME/x")], None), var),
            Err(())
        );
        assert_eq!(
            account_home(&profile(&[(var, "rel/x")], None), var),
            Err(())
        );
        assert_eq!(
            account_home(&profile(&[], Some(&dir.path().join("missing.env"))), var),
            Err(())
        );
        let mut n = 0;
        let mut from = |text: &str| {
            n += 1;
            let path = dir.path().join(format!("f{n}.env"));
            fs::write(&path, text).unwrap();
            account_home(&profile(&[], Some(&path)), var)
        };
        // A trailing comment is not part of the value.
        assert_eq!(
            from("CLAUDE_CONFIG_DIR=/p/a # account home\n"),
            Ok(Some("/p/a".into()))
        );
        assert_eq!(
            from("CLAUDE_CONFIG_DIR='/p/a b' # home\n"),
            Ok(Some("/p/a b".into()))
        );
        // Shell forms this reader does not follow are not guessed.
        assert_eq!(from("CLAUDE_CONFIG_DIR='/p/a'#x\n"), Err(()));
        assert_eq!(from("CLAUDE_CONFIG_DIR=\"$HOME/p\"\n"), Err(()));
        assert_eq!(from("CLAUDE_CONFIG_DIR=/p/$(whoami)\n"), Err(()));
        assert_eq!(from("[ -n x ] && CLAUDE_CONFIG_DIR=/p/c\n"), Err(()));
        // The last assignment wins, and an empty one is not undone by an
        // earlier value.
        assert_eq!(
            from("CLAUDE_CONFIG_DIR=/p/1\nCLAUDE_CONFIG_DIR=/p/2\n"),
            Ok(Some("/p/2".into()))
        );
        assert_eq!(
            from("CLAUDE_CONFIG_DIR=/p/1\nCLAUDE_CONFIG_DIR=\n"),
            Ok(None)
        );
        // `KEY= word` is a command, not an assignment.
        assert_eq!(from("CLAUDE_CONFIG_DIR= /tmp/command\n"), Err(()));
        assert_eq!(from("CLAUDE_CONFIG_DIR= '/tmp/command'\n"), Err(()));
        assert_eq!(
            from("CLAUDE_CONFIG_DIR=/p/1\nCLAUDE_CONFIG_DIR=   # cleared\n"),
            Ok(None)
        );
        // Control flow decides whether an assignment runs: not followed.
        assert_eq!(
            from("if false; then\nCLAUDE_CONFIG_DIR=/tmp/never\nfi\n"),
            Err(())
        );
        assert_eq!(from("f() {\nCLAUDE_CONFIG_DIR=/tmp/never\n}\n"), Err(()));
        assert_eq!(
            from("cat <<EOF\nCLAUDE_CONFIG_DIR=/tmp/never\nEOF\n"),
            Err(())
        );
        assert_eq!(from("OTHER=a \\\nCLAUDE_CONFIG_DIR=/tmp/never\n"), Err(()));
        // Only an unquoted `~/` is the home directory.
        let home = std::env::var("HOME").unwrap();
        assert_eq!(
            from("CLAUDE_CONFIG_DIR=~/acct\n"),
            Ok(Some(PathBuf::from(format!("{home}/acct"))))
        );
        assert_eq!(from("CLAUDE_CONFIG_DIR='~/acct'\n"), Err(()));
        assert_eq!(from("CLAUDE_CONFIG_DIR=\"~/acct\"\n"), Err(()));
        // An explicit env entry decides even when empty: launch applies it
        // after sourcing the file.
        assert_eq!(
            account_home(&profile(&[(var, "")], Some(&file)), var),
            Ok(None)
        );
    }

    #[test]
    fn a_process_config_comes_from_its_own_environment() {
        let env = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        };
        assert_eq!(
            process_config(
                Host::Claude,
                &env(&[("HOME", "/h"), ("CLAUDE_CONFIG_DIR", "/p/a b")])
            ),
            Some(PathBuf::from("/p/a b/.claude.json"))
        );
        // Unset: that process's HOME, not doctor's.
        assert_eq!(
            process_config(Host::Claude, &env(&[("HOME", "/other")])),
            Some(PathBuf::from("/other/.claude.json"))
        );
        assert_eq!(
            process_config(Host::Codex, &env(&[("HOME", "/other")])),
            Some(PathBuf::from("/other/.codex/config.toml"))
        );
        // Setting the variable to the default directory is a different file
        // from leaving it unset.
        assert_eq!(
            process_config(
                Host::Claude,
                &env(&[("HOME", "/h"), ("CLAUDE_CONFIG_DIR", "/h/.claude")])
            ),
            Some(PathBuf::from("/h/.claude/.claude.json"))
        );
        assert_eq!(
            process_config(Host::Claude, &env(&[("CLAUDE_CONFIG_DIR", "rel")])),
            None
        );
        assert_eq!(
            process_config(Host::Claude, &env(&[("PATH", "/bin")])),
            None
        );
    }

    #[test]
    fn broken_registrations_are_told_apart() {
        let dir = tempfile::tempdir().unwrap();
        let file = |name: &str, text: &str| {
            let path = dir.path().join(name);
            fs::write(&path, text).unwrap();
            path
        };
        let homes = [
            (
                "disabled",
                file(
                    "disabled.json",
                    r#"{"mcpServers":{"tend":{"command":"tend","args":["mcp"],"enabled":false}}}"#,
                ),
            ),
            (
                "conflict",
                file(
                    "conflict.json",
                    r#"{"mcpServers":{"tend":{"command":"other","args":["serve"]}}}"#,
                ),
            ),
            ("malformed", file("malformed.json", "{not json")),
            (
                "healthy",
                file(
                    "healthy.json",
                    r#"{"mcpServers":{"tend":{"command":"/opt/bin/tend","args":["mcp"]}}}"#,
                ),
            ),
        ]
        .map(|(_, config)| OtherHome {
            host: Host::Claude,
            config,
            source: HomeSource::Live,
            agents: 1,
        });
        let gaps = other_home_gaps(
            &homes,
            &[],
            &Unchecked {
                accounts: 1,
                processes: 2,
            },
        );
        assert_eq!(gaps.len(), 4, "{gaps:?}");
        assert!(gaps[0].1.contains("tend server disabled"), "{}", gaps[0].1);
        assert!(
            gaps[1].1.contains("reconcile that entry by hand"),
            "{}",
            gaps[1].1
        );
        assert!(
            gaps[2].1.contains("cannot be read or parsed"),
            "{}",
            gaps[2].1
        );
        assert!(
            gaps[3].0
                && gaps[3].1.contains("1 account(s)")
                && gaps[3].1.contains("2 own provider process(es)"),
            "{}",
            gaps[3].1
        );
        // A recorded home nobody runs under is left to the manifest lines.
        let idle = [OtherHome {
            agents: 0,
            ..homes[0].clone()
        }];
        assert!(
            other_home_gaps(&idle, &[homes[0].config.clone()], &Unchecked::default()).is_empty()
        );
    }

    #[test]
    fn a_missing_config_under_a_symlinked_home_keeps_one_spelling() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        fs::create_dir_all(&real).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&real, dir.path().join("alias")).unwrap();
            assert_eq!(
                canonical(&dir.path().join("alias/sub/.claude.json")),
                canonical(&real).join("sub/.claude.json")
            );
        }
        assert_eq!(
            canonical(&real.join(".claude.json")),
            canonical(&real).join(".claude.json")
        );
    }

    #[test]
    fn an_unrecorded_active_home_with_a_broken_tend_entry_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("manifest.json");
        fs::write(
            &manifest,
            serde_json::to_string(&Manifest::default()).unwrap(),
        )
        .unwrap();
        let cfg = |name: &str, text: &str| {
            let path = dir.path().join(name).join(".claude.json");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, text).unwrap();
            (Host::Claude, "CLAUDE_CONFIG_DIR", path)
        };
        let gaps = active_home_gaps(
            &manifest,
            vec![
                cfg(
                    "disabled",
                    r#"{"mcpServers":{"tend":{"command":"tend","args":["mcp"],"enabled":false}}}"#,
                ),
                cfg("conflict", r#"{"mcpServers":{"tend":{"command":"other"}}}"#),
                cfg(
                    "healthy",
                    r#"{"mcpServers":{"tend":{"command":"tend","args":["mcp"]}}}"#,
                ),
            ],
        );
        assert_eq!(gaps.len(), 2, "{gaps:?}");
        assert!(gaps[0].1.contains("tend server disabled"), "{}", gaps[0].1);
        assert!(
            gaps[1].1.contains("does not run `tend mcp`"),
            "{}",
            gaps[1].1
        );
    }

    #[test]
    fn another_accounts_home_gets_the_mcp_server_only() {
        let mut f = Fixture::new();
        let other = f.dir.path().join("profiles/claude-b");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join(".claude.json"), "{\"theme\":\"dark\"}").unwrap();
        f.hosts.push(HostPaths {
            host: Host::Claude,
            home: other.clone(),
            config: other.join(".claude.json"),
            skill: other.join(SKILL_DIR),
            mcp_only: true,
        });
        let plan = f.all(Direction::Install);
        apply::run(&plan, false).unwrap();
        let text = fs::read_to_string(other.join(".claude.json")).unwrap();
        assert!(
            read_server(&text, Host::Claude).unwrap().is_some(),
            "{text}"
        );
        assert!(text.contains("\"theme\""), "{text}");
        assert!(!other.join(SKILL_DIR).exists());
        assert!(!other.join("CLAUDE.md").exists());
        let manifest = load_manifest(&f.root.join("manifest.json")).unwrap();
        assert!(manifest
            .records
            .iter()
            .any(|r| r.path == other.join(".claude.json")));
        // Uninstall removes it again, keeping the user's own keys.
        let undo = f.all(Direction::Uninstall);
        apply::run(&undo, false).unwrap();
        let text = fs::read_to_string(other.join(".claude.json")).unwrap();
        assert!(
            read_server(&text, Host::Claude).unwrap().is_none(),
            "{text}"
        );
        assert!(text.contains("\"theme\""), "{text}");
    }

    #[test]
    fn an_active_provider_home_without_tend_is_reported_with_its_fix() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("manifest.json");
        fs::write(
            &manifest,
            serde_json::to_string(&Manifest::default()).unwrap(),
        )
        .unwrap();
        let bare = dir.path().join("profile-a");
        fs::create_dir_all(&bare).unwrap();
        fs::write(bare.join(".claude.json"), "{\"theme\":\"dark\"}").unwrap();
        let wired = dir.path().join("profile-b");
        fs::create_dir_all(&wired).unwrap();
        fs::write(
            wired.join(".claude.json"),
            "{\"mcpServers\":{\"tend\":{\"command\":\"tend\",\"args\":[\"mcp\"]}}}",
        )
        .unwrap();
        let gaps = active_home_gaps(
            &manifest,
            vec![
                (Host::Claude, "CLAUDE_CONFIG_DIR", bare.join(".claude.json")),
                (
                    Host::Claude,
                    "CLAUDE_CONFIG_DIR",
                    wired.join(".claude.json"),
                ),
            ],
        );
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        let (healthy, message) = &gaps[0];
        assert!(!healthy);
        assert!(message.starts_with("agent-mcp: "), "{message}");
        assert!(
            message.contains(&format!(
                "CLAUDE_CONFIG_DIR={} tend init --component agent-mcp",
                bare.display()
            )),
            "{message}"
        );
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
    fn binary_revision_remains_owned_and_uninstallable() {
        let f = Fixture::new();
        apply::run(&f.plan(Direction::Install, &[Component::AgentMcp]), false).unwrap();
        let path = &f.hosts[0].config;
        let original = fs::read_to_string(path).unwrap();
        let mut server = read_server(&original, Host::Codex).unwrap().unwrap();
        server["env"][crate::mcp_revision::ENV_KEY] = json!("sha256:new");
        write(
            path,
            &write_server(&original, Host::Codex, Some(&server)).unwrap(),
        );
        apply::run(&f.plan(Direction::Uninstall, &[Component::AgentMcp]), false).unwrap();
        assert!(read_server(&fs::read_to_string(path).unwrap(), Host::Codex)
            .unwrap()
            .is_none());
    }

    #[test]
    fn restores_preexisting_mcp_entry_and_preserves_user_environment() {
        let f = Fixture::new();
        let original = "# user preference\n[mcp_servers.tend]\ncommand = '/opt/bin/tend'\nargs = ['--socket', '/custom.sock', 'mcp']\nenv_vars = ['MY_VAR']\nstartup_timeout_sec = 90\n[mcp_servers.tend.env]\nMY_TOKEN = 'test-secret'\n";
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
            "[mcp_servers.tend]\ncommand = 'some-other-tool'\nargs = []\n",
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
            .contains("tend managed"));
        assert!(!fs::read_to_string(f.hosts[0].home.join("AGENTS.md"))
            .unwrap()
            .contains("tend managed"));
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
            .contains("tend managed"));
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
        let open = "<!-- >>> tend managed (agent-instructions) >>> -->\n";
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
        assert!(installed.contains("tend hook codex"));
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
            .contains("tend managed"));
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
        let original = "mcp_servers = { tend = { command = 'tend', args = ['mcp'] }, other = { command = 'other' } }\n";
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
                .matches("<!-- >>> tend managed")
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
            "[mcp_servers.tend]\ncommand='tend'\nargs=['mcp']\nenabled=false\n",
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
