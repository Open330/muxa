//! Opt-in task contracts. Validation checks attribution and shape, never whether
//! an agent's evidence is true. References are data: Muxa never opens or runs them.
use super::{CollaborationError, CollaborationRequest, RequestKind, RequestStatus, WorkMode};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const TASK_CONTRACT_CAPABILITY: &str = "collaboration_task_contract_v1";

pub fn task_contract_schema() -> serde_json::Value {
    use serde_json::json;
    json!({"type":"object","additionalProperties":false,
        "required":["version","task_id","objective","acceptance_criteria"],
        "properties":{
            "version":{"const":1},
            "task_id":{"type":"string","minLength":1,"maxLength":128},
            "objective":{"type":"string","minLength":1,"maxLength":8192},
            "acceptance_criteria":{"type":"array","minItems":1,"maxItems":32,"uniqueItems":true,"items":{"type":"string","minLength":1,"maxLength":1024}},
            "basis_revision":revision_schema(),
            "inputs":{"type":"array","maxItems":16,"items":evidence_schema()}
        },"description":"Immutable v1 task contract. References and reported checks do not authorize execution or prove success. basis_revision is the exact starting/reviewed commit; output commits belong in evidence."})
}

pub fn task_report_schema() -> serde_json::Value {
    use serde_json::json;
    json!({"type":"object","additionalProperties":false,"required":["attempt_id"],
        "properties":{
            "attempt_id":{"type":"string","description":"Echo the daemon-minted request.attempt.id."},
            "basis_revision":revision_schema(),
            "criteria":{"type":"array","maxItems":32,"items":{"type":"object","additionalProperties":false,"required":["criterion","outcome"],"properties":{
                "criterion":{"type":"integer","minimum":0,"maximum":31},
                "outcome":{"enum":["pass","fail","not_run"]},
                "evidence":{"type":"array","maxItems":16,"items":evidence_schema()}
            }}},
            "evidence":{"type":"array","maxItems":16,"items":evidence_schema()}
        },"description":"Recipient-reported evidence, not independent verification. completed requires every criterion to be reported as pass with evidence and the attempt/revision to match."})
}

fn revision_schema() -> serde_json::Value {
    serde_json::json!({"type":"string","pattern":"^(?:[0-9a-fA-F]{40}|[0-9a-fA-F]{64})$"})
}

fn evidence_schema() -> serde_json::Value {
    serde_json::json!({"type":"object","additionalProperties":false,"required":["kind","locator"],"properties":{
        "kind":{"enum":["agent_event","commit","artifact","test_run"]},
        "locator":{"type":"string","minLength":1,"maxLength":2048,"description":"Reference only. Muxa never opens or executes this value."},
        "sha256":{"type":"string","pattern":"^[0-9a-fA-F]{64}$"}
    }})
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvidenceReference {
    pub kind: EvidenceKind,
    pub locator: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    AgentEvent,
    Commit,
    Artifact,
    TestRun,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskContract {
    pub version: u32,
    pub task_id: String,
    pub objective: String,
    pub acceptance_criteria: Vec<String>,
    /// Exact starting/reviewed Git commit, not an output commit or moving ref.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basis_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<EvidenceReference>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskAttempt {
    pub id: String,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basis_revision: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskReport {
    pub attempt_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basis_revision: Option<String>,
    #[serde(default)]
    pub criteria: Vec<CriterionReport>,
    #[serde(default)]
    pub evidence: Vec<EvidenceReference>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CriterionReport {
    /// Zero-based index into the immutable contract's `acceptance_criteria`.
    pub criterion: usize,
    pub outcome: CriterionOutcome,
    #[serde(default)]
    pub evidence: Vec<EvidenceReference>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CriterionOutcome {
    Pass,
    Fail,
    NotRun,
}

fn invalid(reason: impl Into<String>) -> CollaborationError {
    CollaborationError::InvalidTaskContract(reason.into())
}

fn text(value: &str, max: usize, name: &str) -> Result<(), CollaborationError> {
    if value.trim().is_empty() || value.len() > max || value.chars().any(char::is_control) {
        return Err(invalid(format!(
            "{name} must be nonblank, contain no control characters, and fit {max} bytes"
        )));
    }
    Ok(())
}

fn revision(value: Option<&str>) -> Result<(), CollaborationError> {
    if value
        .is_some_and(|v| !matches!(v.len(), 40 | 64) || !v.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(invalid(
            "basis_revision must be a full 40- or 64-character Git object id",
        ));
    }
    Ok(())
}

fn references(values: &[EvidenceReference]) -> Result<(), CollaborationError> {
    if values.len() > 16 {
        return Err(invalid("at most 16 evidence references per list"));
    }
    for value in values {
        text(&value.locator, 2048, "evidence locator")?;
        if value
            .sha256
            .as_ref()
            .is_some_and(|s| s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(invalid("evidence sha256 must be 64 hexadecimal characters"));
        }
    }
    Ok(())
}

impl TaskContract {
    pub fn validate(&self) -> Result<(), CollaborationError> {
        if self.version != 1 {
            return Err(invalid("unsupported task contract version"));
        }
        text(&self.task_id, 128, "task_id")?;
        text(&self.objective, 8192, "objective")?;
        if self.acceptance_criteria.is_empty() || self.acceptance_criteria.len() > 32 {
            return Err(invalid("acceptance_criteria must contain 1..32 items"));
        }
        let mut seen = HashSet::new();
        for criterion in &self.acceptance_criteria {
            text(criterion, 1024, "acceptance criterion")?;
            if !seen.insert(criterion.trim()) {
                return Err(invalid("duplicate acceptance criterion"));
            }
        }
        revision(self.basis_revision.as_deref())?;
        references(&self.inputs)?;
        if serde_json::to_vec(self)
            .map_err(|e| invalid(e.to_string()))?
            .len()
            > 16 * 1024
        {
            return Err(invalid("serialized task contract exceeds 16 KiB"));
        }
        Ok(())
    }

    pub(super) fn attempt(&self) -> TaskAttempt {
        TaskAttempt {
            id: uuid::Uuid::new_v4().to_string(),
            task_id: self.task_id.clone(),
            basis_revision: self.basis_revision.clone(),
        }
    }
}

pub(super) fn validate_report(
    request: &CollaborationRequest,
    status: RequestStatus,
    report: Option<&TaskReport>,
) -> Result<(), CollaborationError> {
    let Some(contract) = &request.task_contract else {
        return if report.is_some() {
            Err(invalid("task report requires a contracted request"))
        } else {
            Ok(())
        };
    };
    if status == RequestStatus::Completed && request.status != RequestStatus::Claimed {
        return Err(invalid(
            "contracted work must be claimed before reporting completion",
        ));
    }
    let Some(report) = report else {
        return if status == RequestStatus::Completed {
            Err(invalid(format!("completed contracted work requires a task report; read only this request with muxa msg accept {} --json if needed, then muxa msg reply {} BODY --report FILE (or reload MCP)",request.id,request.id)))
        } else {
            Ok(())
        };
    };
    let attempt = request
        .attempt
        .as_ref()
        .ok_or_else(|| invalid("contracted request has no attempt identity"))?;
    if report.attempt_id != attempt.id {
        return Err(invalid(
            "task report attempt_id does not match this execution attempt",
        ));
    }
    revision(report.basis_revision.as_deref())?;
    if report
        .basis_revision
        .as_ref()
        .map(|r| r.to_ascii_lowercase())
        != contract
            .basis_revision
            .as_ref()
            .map(|r| r.to_ascii_lowercase())
    {
        return Err(invalid(
            "task report basis_revision does not match the requested revision",
        ));
    }
    references(&report.evidence)?;
    if report.criteria.len() > 32 {
        return Err(invalid("too many criterion reports"));
    }
    let mut seen = HashSet::new();
    for check in &report.criteria {
        if check.criterion >= contract.acceptance_criteria.len() || !seen.insert(check.criterion) {
            return Err(invalid(
                "criterion reports must name distinct in-range criterion indices",
            ));
        }
        references(&check.evidence)?;
        if status == RequestStatus::Completed
            && (check.outcome != CriterionOutcome::Pass
                || (check.evidence.is_empty() && report.evidence.is_empty()))
        {
            return Err(invalid(
                "completed work requires a reported pass with evidence for every criterion",
            ));
        }
    }
    if status == RequestStatus::Completed && seen.len() != contract.acceptance_criteria.len() {
        return Err(invalid(
            "completed work must report every acceptance criterion",
        ));
    }
    Ok(())
}

pub(super) fn validate_supersession(
    old: &CollaborationRequest,
    from: &super::Participant,
    input: &super::NewRequest,
) -> Result<(), CollaborationError> {
    let matching_task = old
        .task_contract
        .as_ref()
        .zip(input.task_contract.as_ref())
        .is_some_and(|(a, b)| a.task_id == b.task_id);
    if !matching_task
        || old.superseded_by.is_some()
        || !old.from.same_session(from)
        || (old.work_id.is_none() && old.from.room != from.room)
        || (old.from.console
            && match (&old.initiator, &input.initiator) {
                (None, None) => false,
                (Some(a), Some(b)) => !a.same_session(b),
                _ => true,
            })
        || old.work_mode != WorkMode::ReadOnly
        || input.work_mode != WorkMode::ReadOnly
        || old.kind == RequestKind::Notice
        || old.human_action.is_some()
        || old.to.console
        || old.workspace_id.as_deref()
            != input
                .workspace_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
        || old.work_id.as_deref()
            != input
                .work_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
    {
        return Err(invalid("supersession requires the same sender, room, work and task_id, and two read-only task requests; an already superseded request cannot be replaced again"));
    }
    Ok(())
}

/// Strict input validation is separate from forward-readable persisted models.
fn reject_unknown_keys(
    value: &serde_json::Value,
    schema: &serde_json::Value,
    path: &str,
) -> Result<(), String> {
    if let (Some(object), Some(properties)) = (
        value.as_object(),
        schema
            .get("properties")
            .and_then(serde_json::Value::as_object),
    ) {
        for (key, child) in object {
            let child_schema = properties
                .get(key)
                .ok_or_else(|| format!("unknown field {path}.{key}"))?;
            reject_unknown_keys(child, child_schema, &format!("{path}.{key}"))?;
        }
    }
    if let (Some(array), Some(item_schema)) = (value.as_array(), schema.get("items")) {
        for child in array {
            reject_unknown_keys(child, item_schema, path)?;
        }
    }
    Ok(())
}

impl TaskContract {
    pub fn from_input(value: serde_json::Value) -> Result<Self, String> {
        reject_unknown_keys(&value, &task_contract_schema(), "task_contract")?;
        let contract: Self = serde_json::from_value(value).map_err(|e| e.to_string())?;
        contract.validate().map_err(|e| e.to_string())?;
        Ok(contract)
    }
}

impl TaskReport {
    pub fn from_input(value: serde_json::Value) -> Result<Self, String> {
        reject_unknown_keys(&value, &task_report_schema(), "report")?;
        serde_json::from_value(value).map_err(|e| e.to_string())
    }
}

pub(crate) fn deserialize_contract_input<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<TaskContract>, D::Error> {
    Option::<serde_json::Value>::deserialize(deserializer)?
        .map(TaskContract::from_input)
        .transpose()
        .map_err(serde::de::Error::custom)
}

pub(crate) fn deserialize_report_input<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<TaskReport>, D::Error> {
    Option::<serde_json::Value>::deserialize(deserializer)?
        .map(TaskReport::from_input)
        .transpose()
        .map_err(serde::de::Error::custom)
}

pub(super) fn supersession_notice(
    old: &CollaborationRequest,
    new: &CollaborationRequest,
    now: time::OffsetDateTime,
) -> CollaborationRequest {
    let mut notice = old.clone();
    notice.id = super::next_request_id(now);
    notice.dispatch_id = None;
    notice.source_node_id = None;
    notice.delegation_parent = None;
    notice.interruption = None;
    notice.human_action = None;
    notice.from = new.from.clone();
    notice.initiator.clone_from(&new.initiator);
    notice.provenance = None;
    notice.kind = RequestKind::Notice;
    notice.body=format!("Read-only request {} was superseded by {}. Its results will not be used for the newer attempt. Any late report is retained only as history; it cannot complete the newer attempt. Read the old request for details.",old.id,new.id);
    notice.expects_reply = false;
    notice.notify = Some(true);
    notice.work_mode = WorkMode::ReadOnly;
    notice.parent_request_id = Some(old.id.clone());
    notice.thread_id.clone_from(&new.thread_id);
    notice.created_at = now;
    notice.status = RequestStatus::Queued;
    notice.claimed_at = None;
    notice.wake_delivery = None;
    notice.notified_at = None;
    notice.reply_notified_at = None;
    notice.reply_read_at = None;
    notice.reply_wake = None;
    notice.reply = None;
    notice.paths.clear();
    notice.artifacts.clear();
    notice.links.clear();
    notice.air_artifacts.clear();
    notice.task_contract = None;
    notice.attempt = None;
    notice.supersedes_request_id = None;
    notice.superseded_by = None;
    notice.late_replies.clear();
    notice.updates.clear();
    notice.to_slot = None;
    notice.rebound_from.clear();
    notice.withdrew_reply_wakes.clear();
    notice
}

impl super::NewRequest {
    /// Fleet forwards as the remote console; retain its initiating agent.
    /// This validates attribution only and is not caller authentication.
    pub fn validate_fleet_contract(&self) -> Result<(), String> {
        if self.task_contract.is_some()
            && self.initiator.as_ref().is_none_or(|p| {
                p.console
                    || p.agent_session_id.trim().is_empty()
                    || super::is_pending_session(&p.agent_session_id)
            })
        {
            return Err(
                "contracted Fleet dispatch requires a resolved initiating agent session".into(),
            );
        }
        Ok(())
    }
}
