//! Host-independent Work placement and configurable execution paths.
//! Policy is resolved by one configured coordinator, never by an LLM.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::fleet::{
    FleetHostSnapshot, FleetHostState, FleetSnapshot, HostAccessMode, LabelSelector, NodeId,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OrchestrationConfig {
    pub enabled: bool,
    /// Fleet alias of the authoritative coordinator. None means this node.
    pub coordinator: Option<String>,
    pub paths: ExecutionPaths,
    pub workspaces: BTreeMap<String, WorkspacePolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ExecutionPaths {
    pub root: String,
    pub repo: String,
    pub run: String,
    pub artifacts: String,
}
impl Default for ExecutionPaths {
    fn default() -> Self {
        Self {
            root: "~/workspace-muxa".into(),
            repo: "repos/{repo}".into(),
            run: "runs/{workspace}/{work}/{attempt}".into(),
            artifacts: "artifacts/{workspace}/{work}/{attempt}".into(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PathOverrides {
    pub root: Option<String>,
    pub repo: Option<String>,
    pub run: Option<String>,
    pub artifacts: Option<String>,
}
impl ExecutionPaths {
    pub fn overlay(&mut self, other: &PathOverrides) {
        if let Some(v) = &other.root {
            self.root.clone_from(v);
        }
        if let Some(v) = &other.repo {
            self.repo.clone_from(v);
        }
        if let Some(v) = &other.run {
            self.run.clone_from(v);
        }
        if let Some(v) = &other.artifacts {
            self.artifacts.clone_from(v);
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkspacePolicy {
    /// Logical repository ID, independent of checkout paths and SSH aliases.
    pub repo: String,
    pub url: String,
    pub pipeline: String,
    pub selector: String,
    pub paths: PathOverrides,
    /// Keys are stable `NodeId`s (preferred), or coordinator-local Fleet aliases.
    pub nodes: BTreeMap<String, PathOverrides>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DispatchRequest {
    /// Generated before dispatch. Reuse it on retries, including after timeout.
    pub dispatch_id: String,
    pub workspace: String,
    pub work: String,
    /// Full immutable Git object ID. Branch names deliberately do not qualify.
    pub commit: String,
    pub body: String,
    #[serde(default)]
    pub selector: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
}
impl DispatchRequest {
    pub fn validate(&self) -> Result<(), String> {
        let id =
            uuid::Uuid::parse_str(&self.dispatch_id).map_err(|_| "dispatch_id must be a UUID")?;
        if id.to_string() != self.dispatch_id {
            return Err("dispatch_id must be a canonical lowercase UUID".into());
        }
        component(&self.workspace)?;
        component(&self.work)?;
        if ![40, 64].contains(&self.commit.len())
            || !self.commit.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err("commit must be a full 40 or 64 digit Git object ID".into());
        }
        if self.body.trim().is_empty() || self.body.len() > 32 * 1024 {
            return Err("body must be 1..32768 bytes".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DispatchPlan {
    pub request: DispatchRequest,
    pub node_id: NodeId,
    pub host: String,
    pub repo: String,
    pub url: String,
    pub pipeline: String,
    pub paths: ExecutionPaths,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedPaths {
    pub repo: PathBuf,
    pub run: PathBuf,
    pub artifacts: PathBuf,
}

fn component(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || value == "."
        || value == ".."
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return Err(format!(
            "invalid path identity {value:?}; use 1..128 ASCII letters, digits, -, _ or ."
        ));
    }
    Ok(())
}

impl OrchestrationConfig {
    pub fn plan(
        &self,
        request: DispatchRequest,
        snapshot: &FleetSnapshot,
    ) -> Result<DispatchPlan, String> {
        request.validate()?;
        if !self.enabled {
            return Err("enable [orchestration].enabled before dispatching".into());
        }
        let policy = self
            .workspaces
            .get(&request.workspace)
            .ok_or("workspace is not registered in orchestration.workspaces")?;
        component(&policy.repo)?;
        if policy.url.trim().is_empty() || policy.pipeline.trim().is_empty() {
            return Err("workspace requires url and pipeline".into());
        }
        let selector: LabelSelector = policy.selector.parse()?;
        let extra: LabelSelector = request.selector.as_deref().unwrap_or("").parse()?;
        let mut candidates: Vec<&FleetHostSnapshot> = snapshot
            .hosts
            .iter()
            .filter(|h| {
                h.state == FleetHostState::Online
                    && h.mode == HostAccessMode::Control
                    && h.node_id.is_some()
                    && h.capabilities.iter().any(|c| c == "orchestration_v1")
                    && selector.matches(&h.labels)
                    && extra.matches(&h.labels)
                    && request.host.as_ref().is_none_or(|wanted| {
                        wanted == &h.alias
                            || h.node_id.as_ref().is_some_and(|id| id.as_str() == wanted)
                    })
            })
            .collect();
        candidates.sort_by_key(|h| {
            (
                h.remote.as_ref().map_or(usize::MAX, |r| {
                    r.agents
                        .iter()
                        .filter(|a| {
                            !matches!(
                                a.state,
                                crate::event::AgentState::Stopped | crate::event::AgentState::Idle
                            )
                        })
                        .count()
                }),
                h.node_id.clone(),
            )
        });
        let host = candidates.first().ok_or("no online control node matches workspace policy, requested selector and orchestration_v1 capability")?;
        let node_id = host.node_id.clone().ok_or("node identity unavailable")?;
        let mut paths = self.paths.clone();
        paths.overlay(&policy.paths);
        if let Some(overrides) = policy
            .nodes
            .get(node_id.as_str())
            .or_else(|| policy.nodes.get(&host.alias))
        {
            paths.overlay(overrides);
        }
        let plan = DispatchPlan { request, node_id, host: host.alias.clone(), repo: policy.repo.clone(), url: policy.url.clone(), pipeline: policy.pipeline.clone(), paths, reason: "online, control-authorized, workspace and request selectors matched; fewest active agents, then stable NodeId".into() };
        // Validate templates without expanding the controller's home on a remote node.
        plan.resolve_paths(Path::new("/placeholder-home"))?;
        Ok(plan)
    }
}

impl DispatchPlan {
    pub fn resolve_paths(&self, home: &Path) -> Result<ResolvedPaths, String> {
        self.request.validate()?;
        component(&self.repo)?;
        let expand = |template: &str| -> Result<PathBuf, String> {
            let mut value = template.to_string();
            for (key, replacement) in [
                ("repo", self.repo.as_str()),
                ("workspace", self.request.workspace.as_str()),
                ("work", self.request.work.as_str()),
                ("attempt", self.request.dispatch_id.as_str()),
            ] {
                value = value.replace(&format!("{{{key}}}"), replacement);
            }
            if value.contains(['{', '}']) {
                return Err(format!("unknown path template: {template}"));
            }
            let path = if let Some(tail) = value.strip_prefix("~/") {
                home.join(tail)
            } else {
                PathBuf::from(value)
            };
            if path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err("execution paths cannot contain ..".into());
            }
            Ok(path)
        };
        let root = expand(&self.paths.root)?;
        if !root.is_absolute() {
            return Err("execution root must be absolute or start with ~/".into());
        }
        let resolve = |s: &str| -> Result<PathBuf, String> {
            let p = expand(s)?;
            Ok(if p.is_absolute() { p } else { root.join(p) })
        };
        // Runs/artifacts must be attempt-specific so retries cannot overwrite another attempt.
        for template in [&self.paths.run, &self.paths.artifacts] {
            if !template.contains("{attempt}") {
                return Err("run and artifacts templates must include {attempt}".into());
            }
        }
        let paths = ResolvedPaths {
            repo: resolve(&self.paths.repo)?,
            run: resolve(&self.paths.run)?,
            artifacts: resolve(&self.paths.artifacts)?,
        };
        let values = [&paths.repo, &paths.run, &paths.artifacts];
        for (i, a) in values.iter().enumerate() {
            for b in values.iter().skip(i + 1) {
                if a.starts_with(b) || b.starts_with(a) {
                    return Err("repo, run and artifacts paths must not overlap".into());
                }
            }
        }
        Ok(paths)
    }
}

/// Only these owner-socket operations may be forwarded to the coordinator.
pub fn shared_ask_kind(kind: &str) -> bool {
    matches!(
        kind,
        "ask_send"
            | "ask_send_new"
            | "ask_status"
            | "ask_list"
            | "ask_conversation_list"
            | "ask_conversation_select"
            | "ask_agent"
            | "ask_reset"
            | "ask_clear"
            | "ask_delete"
            | "ask_providers"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;
    fn request() -> DispatchRequest {
        DispatchRequest {
            dispatch_id: uuid::Uuid::new_v4().to_string(),
            workspace: "muxa".into(),
            work: "fleet".into(),
            commit: "a".repeat(40),
            body: "test fleet".into(),
            selector: None,
            host: None,
        }
    }
    fn host(alias: &str, labels: &[(&str, &str)]) -> FleetHostSnapshot {
        FleetHostSnapshot {
            alias: alias.into(),
            local: alias == "local",
            ssh_target: alias.into(),
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            annotations: BTreeMap::new(),
            mode: HostAccessMode::Control,
            state: FleetHostState::Online,
            node_id: Some(NodeId::generate()),
            hostname: None,
            os: None,
            arch: None,
            muxa_version: None,
            protocol: None,
            capabilities: vec!["orchestration_v1".into()],
            daemon_generation: None,
            boot_id: None,
            latency_ms: None,
            last_seen_at: None,
            received_at: None,
            error: None,
            remote: None,
        }
    }
    fn policy() -> OrchestrationConfig {
        let mut cfg = OrchestrationConfig {
            enabled: true,
            ..Default::default()
        };
        cfg.workspaces.insert(
            "muxa".into(),
            WorkspacePolicy {
                repo: "open330-muxa".into(),
                url: "git@example:muxa.git".into(),
                pipeline: "solo".into(),
                selector: "organization=personal".into(),
                ..Default::default()
            },
        );
        cfg
    }
    fn snapshot(hosts: Vec<FleetHostSnapshot>) -> FleetSnapshot {
        FleetSnapshot {
            generated_at: OffsetDateTime::now_utc(),
            hosts,
        }
    }
    #[test]
    fn placement_intersects_policy_and_user_selector_and_rejects_unavailable_nodes() {
        let mut work = host("work", &[("organization", "work"), ("gpu", "true")]);
        let personal = host(
            "personal",
            &[("organization", "personal"), ("gpu", "false")],
        );
        let mut r = request();
        r.selector = Some("gpu=true".into());
        assert!(policy()
            .plan(r.clone(), &snapshot(vec![work.clone(), personal]))
            .is_err());
        work.labels.insert("organization".into(), "personal".into());
        for state in [FleetHostState::Offline, FleetHostState::Disabled] {
            work.state = state;
            assert!(policy()
                .plan(r.clone(), &snapshot(vec![work.clone()]))
                .is_err());
        }
        work.state = FleetHostState::Online;
        work.mode = HostAccessMode::Observe;
        assert!(policy()
            .plan(r.clone(), &snapshot(vec![work.clone()]))
            .is_err());
        work.mode = HostAccessMode::Control;
        assert_eq!(
            policy().plan(r, &snapshot(vec![work])).unwrap().host,
            "work"
        );
    }
    #[test]
    fn node_identity_paths_override_workspace_and_global_without_using_controller_home() {
        let h = host("mac", &[("organization", "personal")]);
        let id = h.node_id.clone().unwrap();
        let mut cfg = policy();
        cfg.paths.root = "~/base".into();
        let ws = cfg.workspaces.get_mut("muxa").unwrap();
        ws.paths.root = Some("~/workspace".into());
        ws.nodes.insert(
            id.to_string(),
            PathOverrides {
                root: Some("~/device".into()),
                ..Default::default()
            },
        );
        let plan = cfg.plan(request(), &snapshot(vec![h])).unwrap();
        let p = plan.resolve_paths(Path::new("/Users/june")).unwrap();
        assert_eq!(p.repo, Path::new("/Users/june/device/repos/open330-muxa"));
        assert!(p.run.starts_with("/Users/june/device/runs/muxa/fleet"));
        assert!(p.run.ends_with(&plan.request.dispatch_id));
    }
    #[test]
    fn reject_mutable_revision_path_escape_overlap_and_unknown_templates() {
        let h = host("local", &[("organization", "personal")]);
        let snap = snapshot(vec![h]);
        let mut r = request();
        r.commit = "main".into();
        assert!(policy().plan(r, &snap).is_err());
        let mut r = request();
        r.work = "../other".into();
        assert!(policy().plan(r, &snap).is_err());
        for template in [
            "runs/{bad}/{attempt}",
            "../runs/{attempt}",
            "runs/no-attempt",
        ] {
            let mut cfg = policy();
            cfg.paths.run = template.into();
            assert!(cfg.plan(request(), &snap).is_err());
        }
        let mut cfg = policy();
        cfg.paths.run = "repos/{repo}/{attempt}".into();
        assert!(cfg.plan(request(), &snap).is_err());
    }
    #[test]
    fn config_round_trip_and_shared_ask_allowlist() {
        let text = r#"[orchestration]
 enabled = true
 coordinator = "june-mbp"
 [orchestration.paths]
 root = "~/jobs"
 [orchestration.workspaces.muxa]
 repo = "muxa"
 url = "git@example:muxa.git"
 pipeline = "solo"
 [orchestration.workspaces.muxa.nodes.rtzr]
 root = "/srv/jobs"
 "#;
        let c: crate::Config = toml::from_str(text).unwrap();
        assert_eq!(c.orchestration.paths.root, "~/jobs");
        assert!(shared_ask_kind("ask_send"));
        assert!(!shared_ask_kind("work_up"));
        assert!(!shared_ask_kind("stop"));
    }
}
