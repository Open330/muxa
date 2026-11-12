//! Display-only launch provenance. This module never places, stops or routes agents.
use crate::event::{AgentId, AgentKind, AgentState};
use crate::state::Agent;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const CAPABILITY: &str = "launch_identity_v1";
pub const CREATE_FORMAT: &str = "#{pane_id}\t#{pane_pid}\t#{pid}\t#{start_time}";
const MAX_ARCHIVE_BYTES: u64 = 64 * 1024;
const MAX_CLAIMS: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessKey {
    pub pid: u32,
    pub start_ticks: String,
    pub boot_id: String,
}
impl ProcessKey {
    pub fn read(pid: u32) -> Option<Self> {
        Some(Self {
            pid,
            start_ticks: crate::agent_conversation::process_start(pid)?,
            boot_id: std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
                .ok()?
                .trim()
                .into(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchReceipt {
    pub launch_id: String,
    pub pane: String,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub process: Option<ProcessKey>,
    #[serde(default)]
    pub server: Option<ProcessKey>,
    #[serde(default)]
    pub server_start_time: Option<u64>,
    #[serde(default)]
    pub marked: bool,
}
impl LaunchReceipt {
    /// Missing/unsupported format fields degrade provenance, never repeat creation.
    pub fn parse(
        output: &str,
        launch_id: &str,
        endpoint: Option<String>,
        rmux: bool,
    ) -> Option<Self> {
        let line = output
            .lines()
            .find(|line| line.trim_start().starts_with('%'))?;
        let fields: Vec<_> = line.split('\t').collect();
        let pane = line.split_whitespace().next()?;
        if !pane.strip_prefix('%')?.chars().all(|c| c.is_ascii_digit()) || pane.len() < 2 {
            return None;
        }
        let fields_valid = fields.len() == 4 && fields[0] == pane;
        Some(Self {
            launch_id: launch_id.into(),
            pane: if rmux {
                format!("rmux:{pane}")
            } else {
                pane.into()
            },
            endpoint,
            process: fields_valid
                .then(|| fields[1].parse().ok().and_then(ProcessKey::read))
                .flatten(),
            server: fields_valid
                .then(|| fields[2].parse().ok().and_then(ProcessKey::read))
                .flatten(),
            server_start_time: fields_valid.then(|| fields[3].parse().ok()).flatten(),
            marked: false,
        })
    }
}

/// Additive metadata alongside the existing creator archive; old archives decode.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LaunchRecord {
    pub launch_id: String,
    #[serde(default)]
    pub provider: Option<AgentKind>,
    #[serde(default)]
    pub expected_session: Option<String>,
    #[serde(default)]
    pub wrapper_forks: bool,
    #[serde(default)]
    pub no_exec: bool,
    #[serde(default)]
    pub failed: bool,
    #[serde(default)]
    pub receipt: Option<LaunchReceipt>,
    #[serde(flatten)]
    pub creation: BTreeMap<String, serde_json::Value>,
}

pub fn archive_directory() -> Option<PathBuf> {
    Some(crate::paths::data_dir()?.join("tend").join("spawns"))
}
/// Remove terminal archives older than 30 days and intents that never
/// completed after 90 days. Anything referenced by a live row or an
/// unfinished worker ticket is retained.
/// Age is measured from the last archive write, including receipt completion.
pub fn prune_archives(
    directory: &Path,
    agents: &[Agent],
    protected: &std::collections::BTreeSet<String>,
    now: std::time::SystemTime,
) -> std::io::Result<usize> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let ttl = std::time::Duration::from_secs(30 * 24 * 60 * 60);
    // A launch that never wrote a receipt or a failure is kept longer, in
    // case its pane is still coming up, but not forever.
    let intent_ttl = std::time::Duration::from_secs(90 * 24 * 60 * 60);
    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if protected.contains(id) {
            continue;
        }
        let metadata = entry.metadata()?;
        let Some(age) = metadata
            .modified()
            .ok()
            .and_then(|at| now.duration_since(at).ok())
        else {
            continue;
        };
        if age < ttl {
            continue;
        }
        let Some(record) = read_archive(directory, id) else {
            continue;
        };
        if !record.failed && record.receipt.is_none() && age < intent_ttl {
            continue;
        }
        let referenced = agents
            .iter()
            .filter(|a| a.state != AgentState::Stopped)
            .any(|agent| {
                record.expected_session.as_deref() == Some(agent.session_id.as_str())
                    || record.receipt.as_ref().is_some_and(|receipt| {
                        agent.pane.as_deref() == Some(receipt.pane.as_str())
                            && match (agent.tmux_socket.as_deref(), receipt.endpoint.as_deref()) {
                                (Some(actual), Some(expected)) => {
                                    crate::backend::pane_endpoints_match(
                                        agent.pane.as_deref(),
                                        actual,
                                        expected,
                                    )
                                }
                                _ => true,
                            }
                    })
            });
        if !referenced {
            std::fs::remove_file(path)?;
            removed += 1;
        }
    }
    Ok(removed)
}

fn archive_path(directory: &Path, id: &str) -> std::io::Result<PathBuf> {
    uuid::Uuid::parse_str(id)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    Ok(directory.join(format!("{id}.json")))
}
pub fn read_archive(directory: &Path, id: &str) -> Option<LaunchRecord> {
    let path = archive_path(directory, id).ok()?;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_ARCHIVE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_ARCHIVE_BYTES {
        return None;
    }
    let record: LaunchRecord = serde_json::from_slice(&bytes).ok()?;
    (record.launch_id == id).then_some(record)
}
/// Intent is created exclusively before the single physical create attempt.
pub fn prepare(directory: &Path, record: &LaunchRecord) -> std::io::Result<()> {
    std::fs::create_dir_all(directory)?;
    let path = archive_path(directory, &record.launch_id)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(&serde_json::to_vec(record)?)?;
    file.sync_all()
}
/// Complete one existing intent; never overwrite another launch or its creator.
pub fn complete(directory: &Path, receipt: LaunchReceipt) -> std::io::Result<()> {
    let mut record = read_archive(directory, &receipt.launch_id)
        .ok_or_else(|| std::io::Error::other("launch intent missing"))?;
    if record.failed {
        return Err(std::io::Error::other("launch failed"));
    }
    if record.receipt.as_ref().is_some_and(|old| old != &receipt) {
        return Err(std::io::Error::other(
            "launch already has a different receipt",
        ));
    }
    record.receipt = Some(receipt);
    replace_archive(directory, &record)
}
pub fn failed(directory: &Path, id: &str) -> std::io::Result<()> {
    let mut record = read_archive(directory, id)
        .ok_or_else(|| std::io::Error::other("launch intent missing"))?;
    record.failed = true;
    replace_archive(directory, &record)
}
pub fn add_creation(
    directory: &Path,
    id: &str,
    creation: &serde_json::Value,
) -> std::io::Result<()> {
    let mut record = read_archive(directory, id)
        .ok_or_else(|| std::io::Error::other("launch intent missing"))?;
    if record.creation.contains_key("creator") {
        return Err(std::io::Error::other("creator already recorded"));
    }
    if let Some(fields) = creation.as_object() {
        for key in ["created_at", "creator", "child_pane", "child_socket"] {
            if let Some(value) = fields.get(key) {
                record.creation.insert(key.into(), value.clone());
            }
        }
    }
    replace_archive(directory, &record)
}
fn replace_archive(directory: &Path, record: &LaunchRecord) -> std::io::Result<()> {
    let path = archive_path(directory, &record.launch_id)?;
    let tmp = directory.join(format!(
        ".{}.{}.tmp",
        record.launch_id,
        uuid::Uuid::new_v4()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&tmp)?;
        file.write_all(&serde_json::to_vec(record)?)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Verification {
    Verified,
    Unknown { reason: String },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchIdentity {
    pub launch_id: String,
    #[serde(flatten)]
    pub verification: Verification,
}
fn unknown(reason: &str) -> Verification {
    Verification::Unknown {
        reason: reason.into(),
    }
}

trait Evidence {
    fn process(&self, pid: u32) -> Option<ProcessKey>;
    fn pane(&self, receipt: &LaunchReceipt) -> bool;
    fn conversation(&self, pid: u32) -> crate::agent_conversation::Conversation;
}
struct SystemEvidence;
impl Evidence for SystemEvidence {
    fn process(&self, pid: u32) -> Option<ProcessKey> {
        ProcessKey::read(pid)
    }
    fn conversation(&self, pid: u32) -> crate::agent_conversation::Conversation {
        crate::agent_conversation::claude_conversation(pid)
    }
    fn pane(&self, receipt: &LaunchReceipt) -> bool {
        let Some(endpoint) = receipt.endpoint.as_deref().filter(|s| !s.is_empty()) else {
            return false;
        };
        let fmt = format!("{CREATE_FORMAT}\t#{{@tend_launch_id}}\t#{{pane_dead}}");
        let target = receipt.pane.strip_prefix("rmux:").unwrap_or(&receipt.pane);
        let args = ["display-message", "-p", "-t", target, &fmt];
        let output = if receipt.pane.starts_with("rmux:") {
            crate::RmuxBackend::with_endpoint(endpoint)
                .capture_control(&args)
                .ok()
        } else {
            crate::tmux::capture_control_on(Some(endpoint), &args).ok()
        };
        let Some(output) = output else {
            return false;
        };
        let fields: Vec<_> = output.trim_end_matches(['\r', '\n']).split('\t').collect();
        fields.len() == 6
            && fields[0] == target
            && fields[4] == receipt.launch_id
            && fields[5] == "0"
            && fields[1].parse::<u32>().ok() == receipt.process.as_ref().map(|p| p.pid)
            && fields[2].parse::<u32>().ok() == receipt.server.as_ref().map(|p| p.pid)
            && fields[3].parse::<u64>().ok() == receipt.server_start_time
    }
}
fn verify(record: &LaunchRecord, id: &AgentId, evidence: &impl Evidence) -> Verification {
    if record.failed {
        return unknown("launch_failed");
    }
    if record.no_exec {
        return unknown("no_exec");
    }
    if record.wrapper_forks {
        return unknown("wrapper_forks");
    }
    if record.provider != Some(AgentKind::ClaudeCode) || id.kind != AgentKind::ClaudeCode {
        return unknown("unsupported_provider");
    }
    let Some(receipt) = &record.receipt else {
        return unknown("pending_receipt");
    };
    if receipt.launch_id != record.launch_id || !receipt.marked {
        return unknown("missing_mark");
    }
    let (Some(process), Some(server), Some(_)) =
        (&receipt.process, &receipt.server, receipt.server_start_time)
    else {
        return unknown("unsupported_receipt");
    };
    if id.pane.as_deref() != Some(receipt.pane.as_str()) {
        return unknown("pane_mismatch");
    }
    let (Some(actual), Some(expected)) = (id.tmux_socket.as_deref(), receipt.endpoint.as_deref())
    else {
        return unknown("endpoint_unknown");
    };
    if !crate::backend::pane_endpoints_match(id.pane.as_deref(), actual, expected) {
        return unknown("endpoint_mismatch");
    }
    if evidence.process(process.pid).as_ref() != Some(process)
        || evidence.process(server.pid).as_ref() != Some(server)
    {
        return unknown("process_changed");
    }
    if !evidence.pane(receipt) {
        return unknown("pane_changed");
    }
    if record
        .expected_session
        .as_ref()
        .is_some_and(|expected| expected != &id.session_id)
    {
        return unknown("session_mismatch");
    }
    if !matches!(evidence.conversation(process.pid), crate::agent_conversation::Conversation::Verified(ref session) if session == &id.session_id)
    {
        return unknown("provider_record_mismatch");
    }
    if evidence.process(process.pid).as_ref() != Some(process) {
        return unknown("process_changed");
    }
    Verification::Verified
}

/// Claims are separate from Agent's operational fields and are bounded even before receipt arrival.
#[derive(Clone, Default)]
pub struct LaunchIdentities {
    claims: HashMap<String, Claim>,
}
#[derive(Clone)]
struct Claim {
    id: AgentId,
    launch_id: String,
    conflict: bool,
}
impl LaunchIdentities {
    pub(crate) fn at_capacity(&self) -> bool {
        self.claims.len() >= MAX_CLAIMS
    }
    pub fn observe(&mut self, id: &AgentId, launch_id: &str, stopped: bool) {
        if stopped {
            self.claims.remove(&id.session_id);
            return;
        }
        if uuid::Uuid::parse_str(launch_id).is_err() || id.session_id.len() > 1024 {
            return;
        }
        if id.pane.as_ref().is_some_and(|value| value.len() > 256)
            || id
                .tmux_socket
                .as_ref()
                .is_some_and(|value| value.len() > 4096)
        {
            return;
        }
        let observed = AgentId {
            kind: id.kind,
            session_id: id.session_id.clone(),
            pane: id.pane.clone(),
            tmux_socket: id.tmux_socket.clone(),
            surface: None,
            cwd: None,
        };
        if let Some(old) = self.claims.get_mut(&id.session_id) {
            old.conflict |= old.launch_id != launch_id;
            old.id = observed;
        } else if self.claims.len() < MAX_CLAIMS {
            self.claims.insert(
                id.session_id.clone(),
                Claim {
                    id: observed,
                    launch_id: launch_id.into(),
                    conflict: false,
                },
            );
        }
    }
    pub(crate) fn protected_launches(&self) -> std::collections::BTreeSet<String> {
        self.claims
            .values()
            .map(|claim| claim.launch_id.clone())
            .collect()
    }
    pub fn remove(&mut self, session: &str) {
        self.claims.remove(session);
    }
    pub fn retain_live(&mut self, agents: &[Agent]) {
        self.claims.retain(|session, _| {
            agents
                .iter()
                .any(|a| a.session_id == *session && a.state != AgentState::Stopped)
        });
    }
    pub fn snapshot(
        &mut self,
        agents: &[Agent],
        directory: &Path,
    ) -> BTreeMap<String, LaunchIdentity> {
        self.snapshot_with(agents, directory, &SystemEvidence)
    }
    fn snapshot_with(
        &mut self,
        agents: &[Agent],
        directory: &Path,
        evidence: &impl Evidence,
    ) -> BTreeMap<String, LaunchIdentity> {
        self.retain_live(agents);
        self.claims
            .iter()
            .map(|(session, claim)| {
                let verification = if claim.conflict {
                    unknown("conflicting_claim")
                } else if let Some(record) = read_archive(directory, &claim.launch_id) {
                    if agents.iter().any(|agent| {
                        agent.session_id == *session
                            && (agent.pane != claim.id.pane
                                || !match (&agent.tmux_socket, &claim.id.tmux_socket) {
                                    (Some(actual), Some(claimed)) => {
                                        crate::backend::pane_endpoints_match(
                                            agent.pane.as_deref(),
                                            actual,
                                            claimed,
                                        )
                                    }
                                    (None, None) => true,
                                    _ => false,
                                }
                                || agent.kind != claim.id.kind)
                    }) {
                        unknown("row_changed")
                    } else {
                        verify(&record, &claim.id, evidence)
                    }
                } else {
                    unknown("pending_receipt")
                };
                (
                    session.clone(),
                    LaunchIdentity {
                        launch_id: claim.launch_id.clone(),
                        verification,
                    },
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_conversation::Conversation;
    use crate::event::AgentEvent;
    const LAUNCH: &str = "ca58dbee-96bd-46e0-9090-fc1ede12e009";
    const SESSION: &str = "c2d8ad39-2210-40c0-b748-37d23103c115";
    fn key(pid: u32) -> ProcessKey {
        ProcessKey {
            pid,
            start_ticks: "42".into(),
            boot_id: "boot".into(),
        }
    }
    fn id(session: &str) -> AgentId {
        AgentId {
            kind: AgentKind::ClaudeCode,
            session_id: session.into(),
            pane: Some("%4".into()),
            tmux_socket: Some("/tmp/scratch/default".into()),
            cwd: None,
            surface: None,
        }
    }
    fn record() -> LaunchRecord {
        LaunchRecord {
            launch_id: LAUNCH.into(),
            provider: Some(AgentKind::ClaudeCode),
            expected_session: Some(SESSION.into()),
            receipt: Some(LaunchReceipt {
                launch_id: LAUNCH.into(),
                pane: "%4".into(),
                endpoint: Some("/tmp/scratch/default".into()),
                process: Some(key(100)),
                server: Some(key(200)),
                server_start_time: Some(12),
                marked: true,
            }),
            ..Default::default()
        }
    }
    struct Fake {
        reused: bool,
        pane: bool,
        session: String,
    }
    impl Default for Fake {
        fn default() -> Self {
            Self {
                reused: false,
                pane: true,
                session: SESSION.into(),
            }
        }
    }
    impl Evidence for Fake {
        fn process(&self, pid: u32) -> Option<ProcessKey> {
            let mut k = key(pid);
            if self.reused {
                k.start_ticks = "43".into();
            }
            Some(k)
        }
        fn pane(&self, _: &LaunchReceipt) -> bool {
            self.pane
        }
        fn conversation(&self, _: u32) -> Conversation {
            Conversation::Verified(self.session.clone())
        }
    }
    #[test]
    fn proof_requires_exact_process_server_pane_and_conversation() {
        assert_eq!(
            verify(&record(), &id(SESSION), &Fake::default()),
            Verification::Verified
        );
        assert_eq!(
            verify(
                &record(),
                &id(SESSION),
                &Fake {
                    reused: true,
                    ..Fake::default()
                }
            ),
            unknown("process_changed")
        );
        assert_eq!(
            verify(
                &record(),
                &id(SESSION),
                &Fake {
                    pane: false,
                    ..Fake::default()
                }
            ),
            unknown("pane_changed")
        );
        assert_eq!(
            verify(&record(), &id("nested-claude"), &Fake::default()),
            unknown("session_mismatch")
        );
        assert_eq!(
            verify(
                &record(),
                &id(SESSION),
                &Fake {
                    session: "other".into(),
                    ..Fake::default()
                }
            ),
            unknown("provider_record_mismatch")
        );
        let mut wrapper = record();
        wrapper.wrapper_forks = true;
        assert_eq!(
            verify(&wrapper, &id(SESSION), &Fake::default()),
            unknown("wrapper_forks")
        );
        wrapper.no_exec = true;
        assert_eq!(
            verify(&wrapper, &id(SESSION), &Fake::default()),
            unknown("no_exec")
        );
    }
    #[tokio::test]
    async fn pending_join_is_order_independent_bounded_and_removed_when_stopped() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::Store::shared();
        let started = AgentEvent::Started {
            id: id(SESSION),
            at: time::OffsetDateTime::now_utc(),
        };
        store.apply(&started).await;
        let agents = store.snapshot().await;
        let mut early = LaunchIdentities::default();
        early.observe(&id(SESSION), LAUNCH, false);
        assert_eq!(
            early.snapshot_with(&agents, directory.path(), &Fake::default())[SESSION].verification,
            unknown("pending_receipt")
        );
        let mut intent = record();
        let receipt = intent.receipt.take().unwrap();
        prepare(directory.path(), &intent).unwrap();
        complete(directory.path(), receipt.clone()).unwrap();
        complete(directory.path(), receipt).unwrap(); // replay is idempotent
        assert_eq!(
            prepare(directory.path(), &intent).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        let mut late = LaunchIdentities::default();
        late.observe(&id(SESSION), LAUNCH, false);
        assert_eq!(
            early.snapshot_with(&agents, directory.path(), &Fake::default()),
            late.snapshot_with(&agents, directory.path(), &Fake::default())
        );
        assert_eq!(
            early.snapshot_with(&agents, directory.path(), &Fake::default())[SESSION].verification,
            Verification::Verified
        );
        let mut normalized = agents.clone();
        normalized[0].tmux_socket = Some("default".into());
        assert_eq!(
            early.snapshot_with(&normalized, directory.path(), &Fake::default())[SESSION]
                .verification,
            Verification::Verified
        );
        normalized[0].pane = Some("%8".into());
        assert_eq!(
            early.snapshot_with(&normalized, directory.path(), &Fake::default())[SESSION]
                .verification,
            unknown("row_changed")
        );
        for i in 0..MAX_CLAIMS + 10 {
            early.observe(&id(&format!("session-{i}")), LAUNCH, false);
        }
        assert_eq!(early.claims.len(), MAX_CLAIMS);
        store.observe_launch_claim(&started, Some(LAUNCH)).await;
        let stopped = AgentEvent::SessionEnded {
            id: id(SESSION),
            at: time::OffsetDateTime::now_utc(),
        };
        store.apply(&stopped).await;
        store.observe_launch_claim(&stopped, None).await;
        assert!(store
            .launch_identity_snapshot(&store.snapshot().await)
            .await
            .is_empty());
        early.snapshot_with(&store.snapshot().await, directory.path(), &Fake::default());
        assert!(early.claims.is_empty());
    }
    #[tokio::test]
    async fn full_claim_registry_reclaims_removed_rows_before_observing_a_live_one() {
        let store = crate::Store::shared();
        for i in 0..MAX_CLAIMS {
            let event = AgentEvent::Started {
                id: id(&format!("removed-{i}")),
                at: time::OffsetDateTime::now_utc(),
            };
            store.apply(&event).await;
            store.observe_launch_claim(&event, Some(LAUNCH)).await;
            assert!(store.forget_agent(&event.id().session_id).await);
        }
        let live = AgentEvent::Started {
            id: id(SESSION),
            at: time::OffsetDateTime::now_utc(),
        };
        store.apply(&live).await;
        store.observe_launch_claim(&live, Some(LAUNCH)).await;
        let identities = store
            .launch_identity_snapshot(&store.snapshot().await)
            .await;
        assert_eq!(identities.len(), 1);
        assert_eq!(identities[SESSION].launch_id, LAUNCH);
    }
    #[test]
    fn archives_are_additive_exclusive_and_reject_replacement_or_path_traversal() {
        let directory = tempfile::tempdir().unwrap();
        let old = serde_json::json!({"launch_id":LAUNCH,"created_at":"2026-11-06","creator":{"location":null,"agent_session_id":"creator","pane":"%1","socket":null,"label":"creator"},"child_pane":"%4","child_socket":null});
        std::fs::write(
            directory.path().join(format!("{LAUNCH}.json")),
            serde_json::to_vec(&old).unwrap(),
        )
        .unwrap();
        let archive = read_archive(directory.path(), LAUNCH).unwrap();
        assert!(archive.provider.is_none());
        assert!(archive.receipt.is_none());
        assert!(read_archive(directory.path(), "../../secrets").is_none());
        assert!(prepare(directory.path(), &record()).is_err());
        assert!(add_creation(directory.path(), LAUNCH, &old).is_err());
        let clean = tempfile::tempdir().unwrap();
        let mut intent = record();
        let receipt = intent.receipt.take().unwrap();
        prepare(clean.path(), &intent).unwrap();
        complete(clean.path(), receipt.clone()).unwrap();
        add_creation(clean.path(), LAUNCH, &old).unwrap();
        let saved = read_archive(clean.path(), LAUNCH).unwrap();
        assert_eq!(saved.receipt, Some(receipt.clone()));
        assert!(saved.creation.contains_key("creator"));
        let legacy: crate::tmux::layout::SpawnProvenance = serde_json::from_slice(
            &std::fs::read(clean.path().join(format!("{LAUNCH}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(legacy.launch_id, LAUNCH);
        let mut changed = receipt;
        changed.pane = "%8".into();
        assert!(complete(clean.path(), changed.clone()).is_err());
        failed(clean.path(), LAUNCH).unwrap();
        assert!(complete(clean.path(), changed).is_err());
        assert_eq!(
            verify(
                &read_archive(clean.path(), LAUNCH).unwrap(),
                &id(SESSION),
                &Fake::default()
            ),
            unknown("launch_failed")
        );
    }
    #[tokio::test]
    async fn archive_retention_preserves_live_pending_recent_and_unknown_records() {
        let directory = tempfile::tempdir().unwrap();
        let now = std::time::SystemTime::now();
        let future = now + std::time::Duration::from_secs(31 * 24 * 60 * 60);
        let mut terminal = record();
        prepare(directory.path(), &terminal).unwrap();
        // Recent terminal records survive.
        assert_eq!(
            prune_archives(
                directory.path(),
                &[],
                &std::collections::BTreeSet::default(),
                now
            )
            .unwrap(),
            0
        );
        let store = crate::Store::shared();
        store
            .apply(&AgentEvent::Started {
                id: id(SESSION),
                at: time::OffsetDateTime::now_utc(),
            })
            .await;
        let agents = store.snapshot().await;
        // Live rows and unfinished tickets independently protect old receipts.
        assert_eq!(
            prune_archives(
                directory.path(),
                &agents,
                &std::collections::BTreeSet::default(),
                future
            )
            .unwrap(),
            0
        );
        let protected = [LAUNCH.to_owned()].into();
        assert_eq!(
            prune_archives(directory.path(), &[], &protected, future).unwrap(),
            0
        );
        assert_eq!(
            prune_archives(
                directory.path(),
                &[],
                &std::collections::BTreeSet::default(),
                future
            )
            .unwrap(),
            1
        );
        // An unfinished intent outlives the terminal window...
        terminal.receipt = None;
        prepare(directory.path(), &terminal).unwrap();
        assert_eq!(
            prune_archives(
                directory.path(),
                &[],
                &std::collections::BTreeSet::default(),
                future
            )
            .unwrap(),
            0
        );
        // ...but is not kept forever, unless something still protects it.
        let far_future = now + std::time::Duration::from_secs(91 * 24 * 60 * 60);
        assert_eq!(
            prune_archives(directory.path(), &[], &protected, far_future).unwrap(),
            0
        );
        assert_eq!(
            prune_archives(
                directory.path(),
                &[],
                &std::collections::BTreeSet::default(),
                far_future
            )
            .unwrap(),
            1
        );
        prepare(directory.path(), &terminal).unwrap();
        failed(directory.path(), LAUNCH).unwrap();
        std::fs::write(directory.path().join("unknown.json"), b"not an archive").unwrap();
        std::fs::write(directory.path().join(".temporary.tmp"), b"partial").unwrap();
        assert_eq!(
            prune_archives(
                directory.path(),
                &[],
                &std::collections::BTreeSet::default(),
                future
            )
            .unwrap(),
            1
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
    }
    #[test]
    fn receipt_tab_fields_degrade_without_losing_the_created_pane() {
        let pid = std::process::id();
        for rmux in [false, true] {
            let receipt =
                LaunchReceipt::parse(&format!("%4\t{pid}\t{pid}\t123\n"), LAUNCH, None, rmux)
                    .unwrap();
            assert_eq!(receipt.pane, if rmux { "rmux:%4" } else { "%4" });
            assert_eq!(receipt.server_start_time, Some(123));
            #[cfg(target_os = "linux")]
            assert_eq!(receipt.process, ProcessKey::read(pid));
        }
        for bad in [
            "%4",
            "%4\t#{pane_pid}\t#{pid}\t#{start_time}",
            "%4\twrong\t0\tno",
            "%4 123 123 123",
            "  %4 123 123 123",
            "  %4\t123\t123\t123",
        ] {
            let receipt = LaunchReceipt::parse(bad, LAUNCH, None, true).unwrap();
            assert_eq!(receipt.pane, "rmux:%4");
            assert!(receipt.process.is_none());
            assert!(!receipt.marked);
        }
        assert!(LaunchReceipt::parse("not a pane", LAUNCH, None, false).is_none());
    }
}
