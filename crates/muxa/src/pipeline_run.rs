//! Durable execution state for declarative Work pipelines.
//!
//! A pane is evidence that an agent process exists; it is not the pipeline
//! state itself. This store keeps the desired graph and one generation-aware
//! state per alias so completion survives pane and daemon restarts without a
//! stale completion opening a new generation's dependency edges.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::{watch, Mutex};

use crate::pipeline::DesiredAgent;
use crate::work::WorkIdentity;

pub const PIPELINE_RUN_SCHEMA_VERSION: u8 = 1;
const CLAIM_LEASE_SECONDS: i64 = 15;

#[cfg(unix)]
const STORE_FILE_MODE: u32 = 0o600;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineAliasStatus {
    Pending,
    Running,
    Blocked,
    Done,
    Failed,
}

impl std::fmt::Display for PipelineAliasStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Failed => "failed",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineAliasState {
    pub alias: String,
    pub status: PipelineAliasStatus,
    /// Generation in which this alias most recently changed state.
    pub generation: u64,
    /// Generation accepted by the atomic `done(alias, generation)` event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Whether this alias still needs the reconciler to launch or re-prompt
    /// it. Kept separate from the five public statuses so a gated pane can
    /// truthfully remain `blocked` without losing the pending restart.
    #[serde(default, skip_serializing_if = "is_false")]
    pub reconcile_pending: bool,
    /// Internal launch reservation. It deliberately does not add another
    /// public status; an abandoned reservation becomes claimable again after
    /// a short lease so daemon/CLI crashes cannot strand the Run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_started_at: Option<OffsetDateTime>,
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineRun {
    pub identity: WorkIdentity,
    pub pipeline: String,
    pub desired: Vec<DesiredAgent>,
    pub cwd: PathBuf,
    pub generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_id: Option<String>,
    pub aliases: BTreeMap<String, PipelineAliasState>,
    pub updated_at: OffsetDateTime,
}

/// Prompt-free projection used by list/watch surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineRunSummary {
    pub pipeline: String,
    pub generation: u64,
    pub aliases: Vec<PipelineAliasSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineAliasSummary {
    pub alias: String,
    pub status: PipelineAliasStatus,
    pub generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_generation: Option<u64>,
}

impl PipelineRun {
    #[must_use]
    pub fn desired_aliases(&self) -> Vec<String> {
        self.desired
            .iter()
            .map(|agent| agent.alias.clone())
            .collect()
    }

    #[must_use]
    pub fn completion(&self) -> (usize, usize) {
        (
            self.aliases
                .values()
                .filter(|state| state.status == PipelineAliasStatus::Done)
                .count(),
            self.desired.len(),
        )
    }

    #[must_use]
    pub fn has_ready_alias(&self) -> bool {
        let done: BTreeSet<&str> = self
            .aliases
            .values()
            .filter(|state| state.status == PipelineAliasStatus::Done)
            .map(|state| state.alias.as_str())
            .collect();
        let now = OffsetDateTime::now_utc();
        self.desired.iter().any(|agent| {
            self.aliases.get(&agent.alias).is_some_and(|state| {
                claimable(state, now)
                    && agent
                        .after
                        .iter()
                        .all(|dependency| done.contains(dependency.as_str()))
            })
        })
    }

    #[must_use]
    pub fn summary(&self) -> PipelineRunSummary {
        PipelineRunSummary {
            pipeline: self.pipeline.clone(),
            generation: self.generation,
            aliases: self
                .desired
                .iter()
                .filter_map(|agent| self.aliases.get(&agent.alias))
                .map(|state| PipelineAliasSummary {
                    alias: state.alias.clone(),
                    status: state.status,
                    generation: state.generation,
                    completion_generation: state.completion_generation,
                })
                .collect(),
        }
    }
}

/// Live-pane evidence supplied during `work up` reconciliation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineAliasObservation {
    pub alias: String,
    pub pane: String,
    pub status: PipelineAliasStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineRunRegistration {
    pub identity: WorkIdentity,
    pub pipeline: String,
    pub desired: Vec<DesiredAgent>,
    pub cwd: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_id: Option<String>,
    #[serde(default)]
    pub observed: Vec<PipelineAliasObservation>,
    /// Explicitly restarted or re-prompted aliases. Their transitive
    /// downstream closure loses completion in the new Run generation.
    #[serde(default)]
    pub invalidate: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineClaim {
    pub identity: WorkIdentity,
    pub pipeline: String,
    pub generation: u64,
    pub cwd: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_id: Option<String>,
    pub agent: DesiredAgent,
    /// Existing pane to re-prompt after an invalidation. `None` means launch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum PipelineRunError {
    #[error("pipeline run {0} was not found")]
    NotFound(String),
    #[error("pipeline alias {alias:?} does not exist in run {run}")]
    UnknownAlias { run: String, alias: String },
    #[error(
        "stale completion for {alias:?}: expected generation is {current}, event generation was {event}"
    )]
    StaleGeneration {
        alias: String,
        current: u64,
        event: u64,
    },
    #[error("invalid pipeline run: {0}")]
    Invalid(String),
    #[error("pipeline Run store I/O: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Serialize, Deserialize)]
struct PipelineRunFile {
    version: u8,
    runs: Vec<PipelineRun>,
}

#[derive(Debug)]
struct Inner {
    path: Option<PathBuf>,
    runs: BTreeMap<WorkIdentity, PipelineRun>,
}

#[derive(Debug)]
pub struct PipelineRunStore {
    inner: Mutex<Inner>,
    revision: watch::Sender<u64>,
}

impl PipelineRunStore {
    #[must_use]
    pub fn in_memory() -> std::sync::Arc<Self> {
        Self::from_runs(None, BTreeMap::new())
    }

    /// Load durable Runs. Missing state is a first start; malformed or
    /// unsupported state is fatal so muxad never silently replaces known
    /// completion with an empty scheduler.
    pub fn load(path: Option<PathBuf>) -> Result<std::sync::Arc<Self>, PipelineRunError> {
        let Some(path) = path else {
            return Ok(Self::in_memory());
        };
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::from_runs(Some(path), BTreeMap::new()));
            }
            Err(error) => return Err(error.into()),
        };
        #[cfg(unix)]
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(STORE_FILE_MODE))?;
        let file: PipelineRunFile = serde_json::from_slice(&bytes).map_err(|error| {
            PipelineRunError::Invalid(format!("cannot parse {}: {error}", path.display()))
        })?;
        if file.version != PIPELINE_RUN_SCHEMA_VERSION {
            return Err(PipelineRunError::Invalid(format!(
                "unsupported schema version {} in {}; expected {}",
                file.version,
                path.display(),
                PIPELINE_RUN_SCHEMA_VERSION
            )));
        }
        let mut runs = BTreeMap::new();
        for run in file.runs {
            let identity = run.identity.clone();
            if runs.insert(identity.clone(), run).is_some() {
                return Err(PipelineRunError::Invalid(format!(
                    "duplicate Run identity {} in {}",
                    identity.key(),
                    path.display()
                )));
            }
        }
        Ok(Self::from_runs(Some(path), runs))
    }

    fn from_runs(
        path: Option<PathBuf>,
        runs: BTreeMap<WorkIdentity, PipelineRun>,
    ) -> std::sync::Arc<Self> {
        let (revision, _) = watch::channel(0);
        std::sync::Arc::new(Self {
            inner: Mutex::new(Inner { path, runs }),
            revision,
        })
    }

    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.revision.subscribe()
    }

    pub async fn list(&self) -> Vec<PipelineRun> {
        self.inner.lock().await.runs.values().cloned().collect()
    }

    pub async fn get(&self, identity: &WorkIdentity) -> Option<PipelineRun> {
        self.inner.lock().await.runs.get(identity).cloned()
    }

    /// Register desired state and reconcile live-pane evidence in one durable
    /// transaction. A graph change starts a fresh generation. Explicit
    /// invalidation resets the named aliases and their downstream closure.
    #[allow(clippy::too_many_lines)] // one lock/commit intentionally covers the full reconciliation transaction
    pub async fn register(
        &self,
        registration: PipelineRunRegistration,
    ) -> Result<PipelineRun, PipelineRunError> {
        validate_registration(&registration)?;
        let mut inner = self.inner.lock().await;
        let previous = inner.runs.get(&registration.identity).cloned();
        let now = OffsetDateTime::now_utc();
        let mut run = previous.clone().unwrap_or_else(|| PipelineRun {
            identity: registration.identity.clone(),
            pipeline: registration.pipeline.clone(),
            desired: registration.desired.clone(),
            cwd: registration.cwd.clone(),
            generation: 1,
            window_id: registration.window_id.clone(),
            aliases: registration
                .desired
                .iter()
                .map(|agent| (agent.alias.clone(), pending_state(&agent.alias, 1, now)))
                .collect(),
            updated_at: now,
        });

        let topology_changed = run.pipeline != registration.pipeline
            || run.cwd != registration.cwd
            || !same_topology(&run.desired, &registration.desired);
        let definition_changed = run.desired != registration.desired;
        if topology_changed {
            run.generation = run.generation.saturating_add(1);
            run.aliases = registration
                .desired
                .iter()
                .map(|agent| {
                    (
                        agent.alias.clone(),
                        pending_state(&agent.alias, run.generation, now),
                    )
                })
                .collect();
        }
        run.pipeline = registration.pipeline;
        run.desired = registration.desired;
        run.cwd = registration.cwd;
        if registration.window_id.is_some() {
            run.window_id = registration.window_id;
        }

        let observed_by_alias: BTreeMap<&str, &PipelineAliasObservation> = registration
            .observed
            .iter()
            .map(|observation| (observation.alias.as_str(), observation))
            .collect();
        let vanished: Vec<String> = if topology_changed {
            Vec::new()
        } else {
            run.aliases
                .values()
                .filter(|state| {
                    state.status != PipelineAliasStatus::Done
                        && state.pane.is_some()
                        && !observed_by_alias.contains_key(state.alias.as_str())
                })
                .map(|state| state.alias.clone())
                .collect()
        };
        let vanished_set: BTreeSet<String> = vanished.iter().cloned().collect();
        let definition_roots = if definition_changed || topology_changed {
            run.desired_aliases()
        } else {
            Vec::new()
        };
        let invalidated_roots: Vec<String> = registration
            .invalidate
            .into_iter()
            .chain(definition_roots)
            .chain(vanished)
            .collect();
        let invalidated = downstream_closure(&run.desired, &invalidated_roots);
        if !invalidated.is_empty() && !topology_changed {
            run.generation = run.generation.saturating_add(1);
            for alias in &invalidated {
                if let Some(state) = run.aliases.get_mut(alias) {
                    state.status = PipelineAliasStatus::Pending;
                    state.generation = run.generation;
                    state.completion_generation = None;
                    state.error = None;
                    state.reconcile_pending = true;
                    state.claim_started_at = None;
                    state.updated_at = now;
                    // Keep the pane as a re-prompt target. The downstream
                    // agent is not considered running in the new generation
                    // until its dependencies are complete and it is claimed.
                    if vanished_set.contains(alias) {
                        state.pane = None;
                    }
                }
            }
        }

        for observation in registration.observed {
            let Some(state) = run.aliases.get_mut(&observation.alias) else {
                continue;
            };
            state.pane = Some(observation.pane);
            // Done is explicit and generation-bound. Pane observation must
            // never demote it; invalidation above is the only reset path.
            if state.status != PipelineAliasStatus::Done {
                if invalidated.contains(&observation.alias) {
                    // A human block remains visible and must not be prompted
                    // over. Once the daemon observes it running again,
                    // `observe_pane` moves it back to ready/pending.
                    if matches!(
                        observation.status,
                        PipelineAliasStatus::Blocked | PipelineAliasStatus::Failed
                    ) {
                        state.status = observation.status;
                    }
                } else {
                    state.status = observation.status;
                    state.generation = run.generation;
                    state.reconcile_pending = false;
                    state.error = None;
                    state.claim_started_at = None;
                }
                state.updated_at = now;
            }
        }
        run.updated_at = now;
        persist_run(&mut inner, run.clone(), previous).await?;
        self.bump_revision();
        Ok(run)
    }

    /// Atomically accept one completion claim. The generation comparison and
    /// state transition happen under the same store lock and disk commit.
    pub async fn done(
        &self,
        identity: &WorkIdentity,
        alias: &str,
        generation: u64,
    ) -> Result<PipelineRun, PipelineRunError> {
        let mut inner = self.inner.lock().await;
        let previous = inner
            .runs
            .get(identity)
            .cloned()
            .ok_or_else(|| PipelineRunError::NotFound(identity.key()))?;
        let mut run = previous.clone();
        let state = run
            .aliases
            .get_mut(alias)
            .ok_or_else(|| PipelineRunError::UnknownAlias {
                run: identity.key(),
                alias: alias.to_string(),
            })?;
        // Run generation is a monotonic revision, but independent branches
        // may still be working in an older generation. Validate the alias's
        // expected generation so restarting one branch does not invalidate an
        // unrelated branch's legitimate completion event.
        if state.generation != generation {
            return Err(PipelineRunError::StaleGeneration {
                alias: alias.to_string(),
                current: state.generation,
                event: generation,
            });
        }
        // A generation number alone is not proof that this alias was
        // materialized. Accept completion only after an observed run or an
        // active atomic claim; otherwise a caller could skip the scheduler
        // and open downstream work directly.
        if state.reconcile_pending && state.claim_started_at.is_none() {
            return Err(PipelineRunError::Invalid(format!(
                "alias {alias:?} has not started in generation {generation}"
            )));
        }
        let now = OffsetDateTime::now_utc();
        state.status = PipelineAliasStatus::Done;
        state.generation = generation;
        state.completion_generation = Some(generation);
        state.error = None;
        state.reconcile_pending = false;
        state.claim_started_at = None;
        state.updated_at = now;
        run.updated_at = now;
        persist_run(&mut inner, run.clone(), Some(previous)).await?;
        self.bump_revision();
        Ok(run)
    }

    /// Undo completion by starting a new generation and invalidating the
    /// alias plus everything transitively downstream.
    pub async fn invalidate(
        &self,
        identity: &WorkIdentity,
        alias: &str,
        generation: u64,
    ) -> Result<PipelineRun, PipelineRunError> {
        let mut inner = self.inner.lock().await;
        let previous = inner
            .runs
            .get(identity)
            .cloned()
            .ok_or_else(|| PipelineRunError::NotFound(identity.key()))?;
        if !previous.aliases.contains_key(alias) {
            return Err(PipelineRunError::UnknownAlias {
                run: identity.key(),
                alias: alias.to_string(),
            });
        }
        let expected = previous.aliases[alias].generation;
        if expected != generation {
            return Err(PipelineRunError::StaleGeneration {
                alias: alias.to_string(),
                current: expected,
                event: generation,
            });
        }
        let mut run = previous.clone();
        run.generation = run.generation.saturating_add(1);
        let now = OffsetDateTime::now_utc();
        for affected in downstream_closure(&run.desired, &[alias.to_string()]) {
            if let Some(state) = run.aliases.get_mut(&affected) {
                state.status = PipelineAliasStatus::Pending;
                state.generation = run.generation;
                state.completion_generation = None;
                state.error = None;
                state.reconcile_pending = true;
                state.claim_started_at = None;
                state.updated_at = now;
            }
        }
        run.updated_at = now;
        persist_run(&mut inner, run.clone(), Some(previous)).await?;
        self.bump_revision();
        Ok(run)
    }

    /// Claim every dependency-ready pending alias for one Run. Claiming and
    /// changing to `running` are atomic, so concurrent CLI/daemon reconcilers
    /// cannot launch the same alias twice.
    pub async fn claim_ready(
        &self,
        identity: &WorkIdentity,
        generation: u64,
    ) -> Result<Vec<PipelineClaim>, PipelineRunError> {
        let mut inner = self.inner.lock().await;
        let previous = inner
            .runs
            .get(identity)
            .cloned()
            .ok_or_else(|| PipelineRunError::NotFound(identity.key()))?;
        if previous.generation != generation {
            return Err(PipelineRunError::StaleGeneration {
                alias: "reconcile".to_string(),
                current: previous.generation,
                event: generation,
            });
        }
        let done: BTreeSet<&str> = previous
            .aliases
            .values()
            .filter(|state| state.status == PipelineAliasStatus::Done)
            .map(|state| state.alias.as_str())
            .collect();
        let now = OffsetDateTime::now_utc();
        let ready: Vec<DesiredAgent> = previous
            .desired
            .iter()
            .filter(|agent| {
                previous.aliases.get(&agent.alias).is_some_and(|state| {
                    claimable(state, now)
                        && agent
                            .after
                            .iter()
                            .all(|dependency| done.contains(dependency.as_str()))
                })
            })
            .cloned()
            .collect();
        if ready.is_empty() {
            return Ok(Vec::new());
        }
        let mut run = previous.clone();
        let mut claims = Vec::with_capacity(ready.len());
        for agent in ready {
            let state = run.aliases.get_mut(&agent.alias).expect("validated alias");
            state.status = PipelineAliasStatus::Running;
            state.generation = generation;
            state.error = None;
            state.reconcile_pending = true;
            state.claim_started_at = Some(now);
            state.updated_at = now;
            claims.push(PipelineClaim {
                identity: run.identity.clone(),
                pipeline: run.pipeline.clone(),
                generation,
                cwd: run.cwd.clone(),
                window_id: run.window_id.clone(),
                agent,
                pane: state.pane.clone(),
            });
        }
        run.updated_at = now;
        persist_run(&mut inner, run, Some(previous)).await?;
        self.bump_revision();
        Ok(claims)
    }

    #[allow(clippy::too_many_arguments)] // wire transition fields stay explicit at the atomic store boundary
    pub async fn report(
        &self,
        identity: &WorkIdentity,
        alias: &str,
        generation: u64,
        status: PipelineAliasStatus,
        pane: Option<String>,
        error: Option<String>,
        window_id: Option<String>,
    ) -> Result<PipelineRun, PipelineRunError> {
        if status == PipelineAliasStatus::Done || status == PipelineAliasStatus::Pending {
            return Err(PipelineRunError::Invalid(
                "report accepts running, blocked, or failed; use done/invalidate for other transitions"
                    .to_string(),
            ));
        }
        let mut inner = self.inner.lock().await;
        let previous = inner
            .runs
            .get(identity)
            .cloned()
            .ok_or_else(|| PipelineRunError::NotFound(identity.key()))?;
        let mut run = previous.clone();
        let state = run
            .aliases
            .get_mut(alias)
            .ok_or_else(|| PipelineRunError::UnknownAlias {
                run: identity.key(),
                alias: alias.to_string(),
            })?;
        if state.generation != generation {
            return Err(PipelineRunError::StaleGeneration {
                alias: alias.to_string(),
                current: state.generation,
                event: generation,
            });
        }
        let now = OffsetDateTime::now_utc();
        let preserve_human_gate = status == PipelineAliasStatus::Running
            && state.reconcile_pending
            && matches!(
                state.status,
                PipelineAliasStatus::Blocked | PipelineAliasStatus::Failed
            );
        if !preserve_human_gate {
            state.status = status;
            state.error = error;
            state.reconcile_pending = false;
        }
        state.generation = generation;
        if pane.is_some() {
            state.pane = pane;
        }
        state.claim_started_at = None;
        state.updated_at = now;
        if window_id.is_some() {
            run.window_id = window_id;
        }
        run.updated_at = now;
        persist_run(&mut inner, run.clone(), Some(previous)).await?;
        self.bump_revision();
        Ok(run)
    }

    /// Project an authoritative daemon agent transition onto the alias bound
    /// to `pane`. Explicit completion always wins: an idle/stopped pane must
    /// never demote `done`, because only `done(alias, generation)` can make or
    /// revoke that claim.
    pub async fn observe_pane(
        &self,
        pane: &str,
        status: PipelineAliasStatus,
    ) -> Result<Option<PipelineRun>, PipelineRunError> {
        if matches!(
            status,
            PipelineAliasStatus::Pending | PipelineAliasStatus::Done
        ) {
            return Err(PipelineRunError::Invalid(
                "pane observation accepts running, blocked, or failed".to_string(),
            ));
        }
        let mut inner = self.inner.lock().await;
        let Some(identity) = inner
            .runs
            .iter()
            .filter(|(_, run)| {
                run.aliases
                    .values()
                    .any(|state| state.pane.as_deref() == Some(pane))
            })
            .max_by_key(|(_, run)| run.updated_at)
            .map(|(identity, _)| identity.clone())
        else {
            return Ok(None);
        };
        let previous = inner.runs.get(&identity).cloned().expect("identity found");
        let mut run = previous.clone();
        let state = run
            .aliases
            .values_mut()
            .find(|state| state.pane.as_deref() == Some(pane))
            .expect("pane found");
        if state.status == PipelineAliasStatus::Done {
            return Ok(Some(run));
        }
        if state.reconcile_pending && state.claim_started_at.is_none() {
            let next = match status {
                PipelineAliasStatus::Running => PipelineAliasStatus::Pending,
                PipelineAliasStatus::Blocked => PipelineAliasStatus::Blocked,
                PipelineAliasStatus::Failed => PipelineAliasStatus::Failed,
                PipelineAliasStatus::Pending | PipelineAliasStatus::Done => unreachable!(),
            };
            if state.status == next {
                return Ok(Some(run));
            }
            let now = OffsetDateTime::now_utc();
            state.status = next;
            state.error = (next == PipelineAliasStatus::Failed)
                .then(|| "agent entered a failed/stopped state".to_string());
            state.updated_at = now;
            run.updated_at = now;
            persist_run(&mut inner, run.clone(), Some(previous)).await?;
            self.bump_revision();
            return Ok(Some(run));
        }
        let materialized_claim = state.claim_started_at.is_some();
        if state.status == status && !materialized_claim {
            return Ok(Some(run));
        }
        let now = OffsetDateTime::now_utc();
        state.status = status;
        state.reconcile_pending = state.reconcile_pending
            && matches!(
                status,
                PipelineAliasStatus::Blocked | PipelineAliasStatus::Failed
            );
        state.claim_started_at = None;
        state.error = (status == PipelineAliasStatus::Failed)
            .then(|| "agent entered a failed/stopped state".to_string());
        state.updated_at = now;
        run.updated_at = now;
        persist_run(&mut inner, run.clone(), Some(previous)).await?;
        self.bump_revision();
        Ok(Some(run))
    }

    fn bump_revision(&self) {
        let next = self.revision.borrow().saturating_add(1);
        self.revision.send_replace(next);
    }
}

fn validate_registration(registration: &PipelineRunRegistration) -> Result<(), PipelineRunError> {
    if registration.identity.workspace_id.trim().is_empty()
        || registration.identity.work_id.trim().is_empty()
        || registration.pipeline.trim().is_empty()
        || registration.desired.is_empty()
    {
        return Err(PipelineRunError::Invalid(
            "workspace, Work, pipeline, and at least one desired alias are required".to_string(),
        ));
    }
    let aliases: BTreeSet<&str> = registration
        .desired
        .iter()
        .map(|agent| agent.alias.as_str())
        .collect();
    if aliases.len() != registration.desired.len() {
        return Err(PipelineRunError::Invalid(
            "desired aliases must be unique".to_string(),
        ));
    }
    if aliases.iter().any(|alias| alias.trim().is_empty()) {
        return Err(PipelineRunError::Invalid(
            "desired aliases cannot be empty".to_string(),
        ));
    }
    for agent in &registration.desired {
        if let Some(dependency) = agent
            .after
            .iter()
            .find(|dependency| !aliases.contains(dependency.as_str()))
        {
            return Err(PipelineRunError::Invalid(format!(
                "alias {:?} depends on unknown alias {dependency:?}",
                agent.alias
            )));
        }
    }
    let mut settled = BTreeSet::new();
    for _ in 0..registration.desired.len() {
        let before = settled.len();
        for agent in &registration.desired {
            if agent
                .after
                .iter()
                .all(|dependency| settled.contains(dependency))
            {
                settled.insert(agent.alias.clone());
            }
        }
        if settled.len() == registration.desired.len() {
            break;
        }
        if settled.len() == before {
            return Err(PipelineRunError::Invalid(
                "pipeline dependencies contain a cycle".to_string(),
            ));
        }
    }
    let mut observed = BTreeSet::new();
    for observation in &registration.observed {
        if !aliases.contains(observation.alias.as_str()) {
            continue;
        }
        if !observed.insert(observation.alias.as_str()) {
            return Err(PipelineRunError::Invalid(format!(
                "alias {:?} has more than one live-pane observation",
                observation.alias
            )));
        }
        if matches!(
            observation.status,
            PipelineAliasStatus::Pending | PipelineAliasStatus::Done
        ) {
            return Err(PipelineRunError::Invalid(format!(
                "alias {:?} has an invalid observed status {}",
                observation.alias, observation.status
            )));
        }
    }
    if let Some(unknown) = registration
        .invalidate
        .iter()
        .find(|alias| !aliases.contains(alias.as_str()))
    {
        return Err(PipelineRunError::Invalid(format!(
            "cannot invalidate unknown alias {unknown:?}"
        )));
    }
    Ok(())
}

fn pending_state(alias: &str, generation: u64, now: OffsetDateTime) -> PipelineAliasState {
    PipelineAliasState {
        alias: alias.to_string(),
        status: PipelineAliasStatus::Pending,
        generation,
        completion_generation: None,
        pane: None,
        error: None,
        reconcile_pending: true,
        claim_started_at: None,
        updated_at: now,
    }
}

fn claimable(state: &PipelineAliasState, now: OffsetDateTime) -> bool {
    state.reconcile_pending
        && (state.status == PipelineAliasStatus::Pending
            || (state.status == PipelineAliasStatus::Running
                && state
                    .claim_started_at
                    .is_some_and(|started| (now - started).whole_seconds() >= CLAIM_LEASE_SECONDS)))
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde skip_serializing_if requires fn(&T) -> bool
const fn is_false(value: &bool) -> bool {
    !*value
}

fn same_topology(left: &[DesiredAgent], right: &[DesiredAgent]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.alias == right.alias
                && left.program == right.program
                && left.role == right.role
                && left.task == right.task
                && left.direction == right.direction
                && left.after == right.after
        })
}

fn downstream_closure(desired: &[DesiredAgent], roots: &[String]) -> BTreeSet<String> {
    let mut affected: BTreeSet<String> = roots.iter().cloned().collect();
    let mut queue: VecDeque<String> = roots.iter().cloned().collect();
    while let Some(done) = queue.pop_front() {
        for agent in desired
            .iter()
            .filter(|agent| agent.after.iter().any(|dependency| dependency == &done))
        {
            if affected.insert(agent.alias.clone()) {
                queue.push_back(agent.alias.clone());
            }
        }
    }
    affected
}

async fn persist_run(
    inner: &mut Inner,
    run: PipelineRun,
    previous: Option<PipelineRun>,
) -> Result<(), PipelineRunError> {
    let identity = run.identity.clone();
    inner.runs.insert(identity.clone(), run);
    let Some(path) = inner.path.as_ref() else {
        return Ok(());
    };
    let file = PipelineRunFile {
        version: PIPELINE_RUN_SCHEMA_VERSION,
        runs: inner.runs.values().cloned().collect(),
    };
    let body = serde_json::to_vec_pretty(&file)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    if let Err(error) = atomic_save(path, &body).await {
        if let Some(previous) = previous {
            inner.runs.insert(identity, previous);
        } else {
            inner.runs.remove(&identity);
        }
        return Err(error.into());
    }
    Ok(())
}

async fn atomic_save(path: &Path, body: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4().simple()));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        options.mode(STORE_FILE_MODE);
    }
    let mut file = options.open(&tmp).await?;
    let result = async {
        file.write_all(body).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&tmp, path).await
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(alias: &str, after: &[&str]) -> DesiredAgent {
        DesiredAgent {
            alias: alias.to_string(),
            program: "codex".to_string(),
            role: None,
            task: None,
            prompt: Some(format!("do {alias}")),
            direction: None,
            after: after.iter().map(|value| (*value).to_string()).collect(),
        }
    }

    fn registration(invalidate: Vec<String>) -> PipelineRunRegistration {
        PipelineRunRegistration {
            identity: WorkIdentity::new("ws", "WORK-1"),
            pipeline: "chain".to_string(),
            desired: vec![
                agent("plan", &[]),
                agent("impl", &["plan"]),
                agent("review", &["impl"]),
            ],
            cwd: PathBuf::from("/tmp/work"),
            window_id: Some("@1".to_string()),
            observed: Vec::new(),
            invalidate,
        }
    }

    #[tokio::test]
    async fn done_opens_only_the_immediate_ready_edge() {
        let store = PipelineRunStore::in_memory();
        let run = store.register(registration(Vec::new())).await.unwrap();
        let claims = store
            .claim_ready(&run.identity, run.generation)
            .await
            .unwrap();
        assert_eq!(
            claims
                .iter()
                .map(|claim| claim.agent.alias.as_str())
                .collect::<Vec<_>>(),
            vec!["plan"]
        );
        store
            .done(&run.identity, "plan", run.generation)
            .await
            .unwrap();
        let claims = store
            .claim_ready(&run.identity, run.generation)
            .await
            .unwrap();
        assert_eq!(
            claims
                .iter()
                .map(|claim| claim.agent.alias.as_str())
                .collect::<Vec<_>>(),
            vec!["impl"]
        );
    }

    #[tokio::test]
    async fn stale_done_cannot_open_a_new_generation() {
        let store = PipelineRunStore::in_memory();
        let run = store.register(registration(Vec::new())).await.unwrap();
        store
            .claim_ready(&run.identity, run.generation)
            .await
            .unwrap();
        store
            .done(&run.identity, "plan", run.generation)
            .await
            .unwrap();
        let next = store
            .register(registration(vec!["plan".to_string()]))
            .await
            .unwrap();
        assert_eq!(next.generation, run.generation + 1);
        let error = store
            .done(&run.identity, "plan", run.generation)
            .await
            .unwrap_err();
        assert!(matches!(error, PipelineRunError::StaleGeneration { .. }));
        assert_eq!(next.aliases["plan"].status, PipelineAliasStatus::Pending);
        assert_eq!(next.aliases["impl"].status, PipelineAliasStatus::Pending);
        assert_eq!(next.aliases["review"].status, PipelineAliasStatus::Pending);
    }

    #[tokio::test]
    async fn persistence_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pipeline-runs.json");
        let store = PipelineRunStore::load(Some(path.clone())).unwrap();
        let run = store.register(registration(Vec::new())).await.unwrap();
        store
            .claim_ready(&run.identity, run.generation)
            .await
            .unwrap();
        store
            .done(&run.identity, "plan", run.generation)
            .await
            .unwrap();
        drop(store);
        let restored = PipelineRunStore::load(Some(path)).unwrap();
        let run = restored
            .get(&WorkIdentity::new("ws", "WORK-1"))
            .await
            .unwrap();
        assert_eq!(run.pipeline, "chain");
        assert_eq!(run.desired_aliases(), vec!["plan", "impl", "review"]);
        assert_eq!(run.aliases["plan"].completion_generation, Some(1));
    }

    #[tokio::test]
    async fn concurrent_done_events_do_not_lose_each_other() {
        let store = PipelineRunStore::in_memory();
        let mut registration = registration(Vec::new());
        registration.desired = vec![agent("left", &[]), agent("right", &[])];
        let run = store.register(registration).await.unwrap();
        store
            .claim_ready(&run.identity, run.generation)
            .await
            .unwrap();
        let (left, right) = tokio::join!(
            store.done(&run.identity, "left", run.generation),
            store.done(&run.identity, "right", run.generation),
        );
        left.unwrap();
        right.unwrap();
        let final_run = store.get(&run.identity).await.unwrap();
        assert_eq!(final_run.completion(), (2, 2));
        assert_eq!(
            final_run.aliases["left"].completion_generation,
            Some(run.generation)
        );
        assert_eq!(
            final_run.aliases["right"].completion_generation,
            Some(run.generation)
        );
    }

    #[tokio::test]
    async fn invalidation_resets_only_the_alias_and_downstream_closure() {
        let store = PipelineRunStore::in_memory();
        let mut registration = registration(Vec::new());
        registration.desired.push(agent("docs", &[]));
        let run = store.register(registration).await.unwrap();
        let roots = store
            .claim_ready(&run.identity, run.generation)
            .await
            .unwrap();
        assert_eq!(roots.len(), 2);
        for alias in ["plan", "docs"] {
            store
                .done(&run.identity, alias, run.generation)
                .await
                .unwrap();
        }
        let impl_claim = store
            .claim_ready(&run.identity, run.generation)
            .await
            .unwrap();
        assert_eq!(impl_claim[0].agent.alias, "impl");
        store
            .done(&run.identity, "impl", run.generation)
            .await
            .unwrap();
        let review_claim = store
            .claim_ready(&run.identity, run.generation)
            .await
            .unwrap();
        assert_eq!(review_claim[0].agent.alias, "review");
        store
            .done(&run.identity, "review", run.generation)
            .await
            .unwrap();
        let next = store
            .invalidate(&run.identity, "impl", run.generation)
            .await
            .unwrap();
        assert_eq!(next.generation, run.generation + 1);
        assert_eq!(next.aliases["plan"].status, PipelineAliasStatus::Done);
        assert_eq!(next.aliases["docs"].status, PipelineAliasStatus::Done);
        assert_eq!(next.aliases["impl"].status, PipelineAliasStatus::Pending);
        assert_eq!(next.aliases["review"].status, PipelineAliasStatus::Pending);
        assert_eq!(next.aliases["impl"].completion_generation, None);
        assert_eq!(next.aliases["review"].completion_generation, None);
    }

    #[tokio::test]
    async fn changed_prompt_reprompts_existing_alias_in_a_new_generation() {
        let store = PipelineRunStore::in_memory();
        let mut initial = registration(Vec::new());
        initial.desired = vec![agent("impl", &[])];
        initial.observed = vec![PipelineAliasObservation {
            alias: "impl".to_string(),
            pane: "%1".to_string(),
            status: PipelineAliasStatus::Running,
        }];
        let first = store.register(initial.clone()).await.unwrap();
        store
            .done(&first.identity, "impl", first.generation)
            .await
            .unwrap();
        initial.desired[0].prompt = Some("do impl again".to_string());
        let next = store.register(initial).await.unwrap();
        assert_eq!(next.generation, first.generation + 1);
        assert_eq!(next.aliases["impl"].status, PipelineAliasStatus::Pending);
        let claims = store
            .claim_ready(&next.identity, next.generation)
            .await
            .unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].pane.as_deref(), Some("%1"));
        assert_eq!(claims[0].agent.prompt.as_deref(), Some("do impl again"));
    }

    #[tokio::test]
    async fn unrelated_branch_can_finish_after_another_branch_restarts() {
        let store = PipelineRunStore::in_memory();
        let mut registration = registration(Vec::new());
        registration.desired = vec![agent("left", &[]), agent("right", &[])];
        let first = store.register(registration).await.unwrap();
        let claims = store
            .claim_ready(&first.identity, first.generation)
            .await
            .unwrap();
        assert_eq!(claims.len(), 2);
        let next = store
            .invalidate(&first.identity, "left", first.generation)
            .await
            .unwrap();
        assert_eq!(next.aliases["left"].generation, next.generation);
        assert_eq!(next.aliases["right"].generation, first.generation);
        let stale = store
            .invalidate(&first.identity, "left", first.generation)
            .await
            .unwrap_err();
        assert!(matches!(stale, PipelineRunError::StaleGeneration { .. }));

        store
            .report(
                &first.identity,
                "right",
                first.generation,
                PipelineAliasStatus::Running,
                Some("%2".to_string()),
                None,
                Some("@1".to_string()),
            )
            .await
            .unwrap();
        let completed = store
            .done(&first.identity, "right", first.generation)
            .await
            .unwrap();
        assert_eq!(completed.aliases["right"].status, PipelineAliasStatus::Done);
        assert_eq!(
            completed.aliases["right"].completion_generation,
            Some(first.generation)
        );
        let restarted_right = store
            .invalidate(&first.identity, "right", first.generation)
            .await
            .unwrap();
        assert_eq!(
            restarted_right.aliases["right"].generation,
            restarted_right.generation
        );
    }

    #[tokio::test]
    async fn pane_observation_updates_status_but_never_demotes_done() {
        let store = PipelineRunStore::in_memory();
        let mut registration = registration(Vec::new());
        registration.desired = vec![agent("impl", &[])];
        registration.observed = vec![PipelineAliasObservation {
            alias: "impl".to_string(),
            pane: "%1".to_string(),
            status: PipelineAliasStatus::Running,
        }];
        let run = store.register(registration).await.unwrap();
        let blocked = store
            .observe_pane("%1", PipelineAliasStatus::Blocked)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(blocked.aliases["impl"].status, PipelineAliasStatus::Blocked);
        store
            .done(&run.identity, "impl", run.generation)
            .await
            .unwrap();
        let stopped = store
            .observe_pane("%1", PipelineAliasStatus::Failed)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stopped.aliases["impl"].status, PipelineAliasStatus::Done);
    }

    #[tokio::test]
    async fn invalidated_blocked_pane_waits_for_the_human_before_reprompt() {
        let store = PipelineRunStore::in_memory();
        let mut registration = registration(Vec::new());
        registration.desired = vec![agent("impl", &[])];
        registration.observed = vec![PipelineAliasObservation {
            alias: "impl".to_string(),
            pane: "%1".to_string(),
            status: PipelineAliasStatus::Blocked,
        }];
        store.register(registration.clone()).await.unwrap();
        registration.invalidate = vec!["impl".to_string()];
        let blocked = store.register(registration).await.unwrap();
        assert_eq!(blocked.aliases["impl"].status, PipelineAliasStatus::Blocked);
        assert!(!blocked.has_ready_alias());

        let ready = store
            .observe_pane("%1", PipelineAliasStatus::Running)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ready.aliases["impl"].status, PipelineAliasStatus::Pending);
        assert!(ready.has_ready_alias());
    }

    #[tokio::test]
    async fn current_generation_cannot_complete_an_alias_that_never_started() {
        let store = PipelineRunStore::in_memory();
        let run = store.register(registration(Vec::new())).await.unwrap();

        let error = store
            .done(&run.identity, "plan", run.generation)
            .await
            .unwrap_err();

        assert!(matches!(error, PipelineRunError::Invalid(_)));
        let unchanged = store.get(&run.identity).await.unwrap();
        assert_eq!(
            unchanged.aliases["plan"].status,
            PipelineAliasStatus::Pending
        );
        assert_eq!(unchanged.aliases["plan"].completion_generation, None);
    }

    #[test]
    fn malformed_durable_state_fails_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pipeline-runs.json");
        std::fs::write(&path, b"not json").unwrap();

        let error = PipelineRunStore::load(Some(path)).unwrap_err();

        assert!(matches!(error, PipelineRunError::Invalid(_)));
    }
}
