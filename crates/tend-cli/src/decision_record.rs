//! Structured decision fields on the agent side: parsing them out of MCP
//! arguments, and keeping them readable on a daemon too old to store them.

use serde_json::{Map, Value};
use std::fmt::Write as _;
use tend::decision::{clean_tail, Decision, Evidence};

/// JSON Schema properties shared by every tool that carries a decision.
pub(crate) fn decision_schema_properties() -> Value {
    serde_json::json!({
        "decision_needed": { "type": "string", "description": "The decision the person is asked to make, in one or two sentences." },
        "options": { "type": "array", "maxItems": tend::decision::MAX_OPTIONS, "items": { "type": "string" }, "description": "The choices on offer, one line each." },
        "recommendation": { "type": "string", "description": "What you would do, and why." },
        "evidence": {
            "type": "array",
            "maxItems": tend::decision::MAX_EVIDENCE,
            "description": "What the person can open to check. Each entry is a shorthand string (commit:SHA, pr:URL or a bare https URL, diff:RANGE, test:CMD, test-pass:CMD, test-fail:CMD, path:PATH) or an object {kind: commit|pull_request|diff|test|path, ...}: commit {sha, repo?}, pull_request {url}, diff {range, repo?}, test {command, passed?, tail?}, path {path}. repo is an absolute checkout path; tail is the last lines of output.",
            "items": { "type": ["string", "object"] }
        }
    })
}

/// Remove the decision fields from `args` and build a [`Decision`].
/// `None` when none were given. `repo` fills commit/diff shorthand.
pub(crate) fn take_decision(
    args: &mut Map<String, Value>,
    repo: Option<&str>,
) -> Result<Option<Decision>, String> {
    let needed = take_string(args, "decision_needed")?;
    let recommendation = take_string(args, "recommendation")?;
    let options = match args.remove("options") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .into_iter()
            .map(|item| match item {
                Value::String(option) => Ok(option),
                _ => Err("options must be an array of strings".to_owned()),
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err("options must be an array of strings".into()),
    };
    let evidence = match args.remove("evidence") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .into_iter()
            .map(|item| evidence_entry(item, repo))
            .collect::<Result<_, _>>()?,
        Some(_) => return Err("evidence must be an array".into()),
    };
    let decision = Decision {
        needed,
        options,
        recommendation,
        evidence,
    };
    decision.validate()?;
    Ok(decision.into_option())
}

fn take_string(args: &mut Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match args.remove(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.trim().is_empty() => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

fn evidence_entry(item: Value, repo: Option<&str>) -> Result<Evidence, String> {
    match item {
        Value::String(spec) => Evidence::parse(spec.trim(), repo),
        Value::Object(_) => {
            let mut evidence: Evidence = serde_json::from_value(item)
                .map_err(|error| format!("invalid evidence: {error}"))?;
            if let Evidence::Test { tail, .. } = &mut evidence {
                *tail = tail
                    .take()
                    .map(|raw| clean_tail(&raw))
                    .filter(|t| !t.is_empty());
            }
            Ok(evidence)
        }
        _ => Err("each evidence entry must be a string or an object".into()),
    }
}

/// The decision as plain text, for a daemon that cannot store it: the
/// person still sees the options and recommendation in the request body.
pub(crate) fn render_plain(decision: &Decision) -> String {
    let mut out = String::new();
    if let Some(needed) = &decision.needed {
        let _ = write!(out, "\n\nDecision needed: {needed}");
    }
    if !decision.options.is_empty() {
        out.push_str("\n\nOptions:");
        for (n, option) in decision.options.iter().enumerate() {
            let _ = write!(out, "\n{}. {option}", n + 1);
        }
    }
    if let Some(recommendation) = &decision.recommendation {
        let _ = write!(out, "\n\nRecommendation: {recommendation}");
    }
    if !decision.evidence.is_empty() {
        out.push_str("\n\nEvidence:");
        for evidence in &decision.evidence {
            let _ = write!(out, "\n- {}", evidence.describe());
            if let Evidence::Test {
                tail: Some(tail), ..
            } = evidence
            {
                for line in tail.lines() {
                    let _ = write!(out, "\n    {line}");
                }
            }
        }
    }
    out
}

/// Rewrite `tend_request_attention` arguments in place: the decision moves
/// to the `decision` field the daemon stores, or into `body` when the
/// daemon predates [`tend::decision::HUMAN_DECISION_CAPABILITY`].
pub(crate) fn attention_decision(
    args: &mut Map<String, Value>,
    daemon_stores_decisions: bool,
) -> Result<(), String> {
    let Some(decision) = take_decision(args, None)? else {
        return Ok(());
    };
    if daemon_stores_decisions {
        args.insert(
            "decision".into(),
            serde_json::to_value(&decision).map_err(|error| error.to_string())?,
        );
    } else if let Some(Value::String(body)) = args.get_mut("body") {
        body.push_str(&render_plain(&decision));
    }
    Ok(())
}

/// Whether the daemon behind `client` advertises `capability`. A failed
/// probe is an error, not "unsupported": choosing a fallback encoding on a
/// timeout would make a retry of the same dispatch look like a different
/// request.
pub(crate) async fn daemon_supports(
    client: &tend::ipc::Client,
    capability: &str,
) -> Result<bool, String> {
    client
        .hello(std::time::Duration::from_secs(2))
        .await
        .map(|hello| hello.capabilities.iter().any(|c| c == capability))
        .map_err(|error| {
            format!("could not ask tendd whether it stores {capability} ({error}); nothing was sent, retry the same call")
        })
}

/// Whether `args` carries any decision field.
pub(crate) fn has_decision_fields(args: &Map<String, Value>) -> bool {
    ["decision_needed", "options", "recommendation", "evidence"]
        .iter()
        .any(|key| args.get(*key).is_some_and(|value| !value.is_null()))
}

/// The `decision` field `attention_decision` left for the request builder.
pub(crate) fn request_decision(args: &Value) -> Result<Option<Decision>, String> {
    args.get("decision")
        .filter(|value| !value.is_null())
        .map(|value| {
            serde_json::from_value::<Decision>(value.clone())
                .map_err(|error| format!("invalid decision: {error}"))
        })
        .transpose()
}

/// The one agent-side entry point for a result record.
pub(crate) const FILE_RESULT_TOOL: &str = "tend_file_result";

/// MCP definition of [`FILE_RESULT_TOOL`].
pub(crate) fn definitions() -> Vec<Value> {
    let mut properties = serde_json::json!({
        "summary": { "type": "string", "description": "What you finished, in one paragraph the person can read without your pane." },
        "commit": { "type": "array", "items": { "type": "string" }, "description": "Commit SHAs holding the result, in your working directory's checkout." },
        "request_id": { "type": "string", "description": "File the record against this collaboration request's result instead of your own Work or session." },
        "key": { "type": "string", "description": "Retry key: the same key returns the same record instead of filing another. Reuse it when retrying one result." }
    });
    if let (Some(properties), Value::Object(decision)) =
        (properties.as_object_mut(), decision_schema_properties())
    {
        properties.extend(decision);
    }
    vec![serde_json::json!({
        "name": FILE_RESULT_TOOL,
        "description": "File the durable result record for finished work: what you did, the decision the person needs to make (if any), the options, your recommendation and evidence (commits, PR, tests). It becomes an open review item the person decides from (`tend review show`); it does not wait for an answer and is not approval. The record targets your managed Work alias when you run in one, else your session. If your Work declares checks, tend runs them first: a failure returns checks_failed with each check's output and files nothing, so fix and call again. Call it once when you finish a result someone must look at; inside a pipeline, `tend work done --summary ...` files the same record and completes the alias.",
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": ["summary"],
            "additionalProperties": false
        }
    })]
}

/// Run [`FILE_RESULT_TOOL`]. The caller is the agent behind this MCP
/// server, as the daemon identifies it, never a value from the arguments.
pub(crate) async fn call(client: &tend::ipc::Client, args: Value) -> Result<Value, String> {
    let Value::Object(mut args) = args else {
        return Err("arguments must be an object".into());
    };
    let summary = take_string(&mut args, "summary")?.ok_or("summary is required")?;
    let request_id = take_string(&mut args, "request_id")?;
    let key = take_string(&mut args, "key")?;
    let commits = match args.remove("commit") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(sha)) => vec![sha],
        Some(Value::Array(items)) => items
            .into_iter()
            .map(|item| match item {
                Value::String(sha) => Ok(sha),
                _ => Err("commit must be an array of SHAs".to_owned()),
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err("commit must be an array of SHAs".into()),
    };
    let origin = crate::mcp::resolve_collaboration_origin(client).await?;
    let me = client
        .collaboration_context(&origin)
        .await
        .map_err(|error| {
            format!("cannot identify this agent ({error}); use `tend review request` instead")
        })?
        .current;
    let repo = match me.cwd.clone() {
        Some(cwd) => tokio::task::spawn_blocking(move || {
            crate::review_cmd::checkout_of(std::path::Path::new(&cwd))
        })
        .await
        .ok()
        .flatten()
        .map(|path| path.to_string_lossy().into_owned()),
        None => None,
    };
    let mut evidence = commits
        .into_iter()
        .map(|sha| Value::String(format!("commit:{}", sha.trim())))
        .collect::<Vec<_>>();
    match args.remove("evidence") {
        None | Some(Value::Null) => {}
        Some(Value::Array(rest)) => evidence.extend(rest),
        Some(_) => return Err("evidence must be an array".into()),
    }
    if !evidence.is_empty() {
        args.insert("evidence".into(), Value::Array(evidence));
    }
    let decision = take_decision(&mut args, repo.as_deref())?;
    if let Some(unknown) = args.keys().next() {
        return Err(format!("unknown argument {unknown:?}"));
    }
    let target = match request_id {
        Some(request_id) => tend::review::ReviewTarget::Request {
            request_id,
            attempt_id: None,
        },
        None => own_target(&me).await,
    };
    let review = tend::review::NewReview {
        target,
        summary,
        anchor: None,
        idempotency_key: key,
        requested_by: tend::review::ReviewActor {
            kind: tend::review::ActorKind::Agent,
            label: Some(me.alias.clone().unwrap_or_else(|| me.pane.clone())),
        },
        decision,
    };
    let item = crate::review_cmd::file(client, &review)
        .await
        .map_err(|error| format!("{error:#}"))?;
    Ok(serde_json::json!({
        "record": item,
        "next": format!("The person decides with `tend review accept|return {}`; nothing waits on it here.", item.id),
    }))
}

/// The multiplexer server and bare pane id the daemon resolved for the
/// caller. `None` when either is unknown: the marks are then never read
/// from whatever server happens to be ambient, where the same `%N` can
/// name another pane.
fn caller_pane(me: &tend::collaboration::Participant) -> Option<(tend::BackendEndpoint, String)> {
    let host = tend::backend::pane_id_host_kind(&me.pane)
        .filter(|host| crate::mux_control::supported(*host))?;
    let socket = me
        .socket
        .clone()
        .filter(|socket| !socket.trim().is_empty())?;
    let pane = me.pane.strip_prefix("rmux:").unwrap_or(&me.pane).to_owned();
    Some((tend::BackendEndpoint { host, socket }, pane))
}

/// The caller's managed Work alias when its pane, on its own server,
/// carries the marks `tend work up` sets, else its session.
async fn own_target(me: &tend::collaboration::Participant) -> tend::review::ReviewTarget {
    let session = tend::review::ReviewTarget::Session {
        provider: Some(me.agent_kind.to_string()),
        session_id: me.agent_session_id.clone(),
    };
    let Some((endpoint, pane)) = caller_pane(me) else {
        return session;
    };
    let pipeline = tokio::task::spawn_blocking(move || {
        crate::mux_control::with_endpoint(Some(endpoint), || {
            crate::tmux_work::pane_pipeline(&pane, None, None).ok()
        })
    })
    .await
    .ok()
    .flatten();
    match pipeline {
        Some((identity, alias, generation)) => tend::review::ReviewTarget::Work {
            workspace: identity.workspace_id,
            work: identity.work_id,
            alias: Some(alias),
            pipeline_generation: Some(generation),
        },
        None => session,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn attention_fields_become_a_stored_decision() {
        let mut args = object(json!({
            "body": "Merge #12?",
            "options": ["merge", "hold"],
            "recommendation": "merge; CI is green",
            "evidence": [
                "https://github.com/o/r/pull/12",
                {"kind": "test", "command": "cargo test", "passed": true, "tail": "\u{1b}[32mok\u{1b}[0m"}
            ]
        }));
        attention_decision(&mut args, true).unwrap();
        assert!(
            args.get("options").is_none(),
            "fields are moved, not copied"
        );
        let decision = request_decision(&Value::Object(args.clone()))
            .unwrap()
            .unwrap();
        assert_eq!(decision.options, ["merge", "hold"]);
        assert_eq!(
            decision.evidence[1],
            Evidence::Test {
                command: "cargo test".into(),
                passed: Some(true),
                tail: Some("ok".into()),
                verified: false,
            }
        );
        assert_eq!(args["body"], "Merge #12?");
    }

    #[test]
    fn an_old_daemon_gets_the_decision_in_the_body() {
        let mut args = object(json!({
            "body": "Merge #12?",
            "options": ["merge", "hold"],
            "recommendation": "merge",
            "evidence": [{"kind": "test", "command": "cargo test", "passed": false, "tail": "boom\nat x.rs:3"}]
        }));
        assert!(has_decision_fields(&args));
        attention_decision(&mut args, false).unwrap();
        assert!(args.get("decision").is_none());
        assert_eq!(
            args["body"],
            "Merge #12?\n\nOptions:\n1. merge\n2. hold\n\nRecommendation: merge\n\nEvidence:\n- test FAILED: cargo test\n    boom\n    at x.rs:3"
        );
        assert!(!has_decision_fields(&object(
            json!({"body": "x", "options": null})
        )));
    }

    fn participant(pane: &str, socket: Option<&str>) -> tend::collaboration::Participant {
        serde_json::from_value(json!({
            "agent_kind": "codex", "agent_session_id": "s", "pane": pane, "socket": socket,
            "room": {"host": "tmux", "socket": socket, "window_id": "@1"},
            "state": "idle", "launch_purpose": "worker"
        }))
        .unwrap()
    }

    #[test]
    fn work_marks_are_read_only_from_the_callers_own_server() {
        let (endpoint, pane) = caller_pane(&participant("%7", Some("/tmp/b.sock"))).unwrap();
        assert_eq!(endpoint.host, tend::HostKind::Tmux);
        assert_eq!(endpoint.socket, "/tmp/b.sock");
        assert_eq!(pane, "%7");
        let (endpoint, pane) = caller_pane(&participant("rmux:%7", Some("/tmp/r.sock"))).unwrap();
        assert_eq!(endpoint.host, tend::HostKind::Rmux);
        assert_eq!(pane, "%7");
        // No recorded server: never fall back to the ambient one.
        assert!(caller_pane(&participant("%7", None)).is_none());
    }

    #[tokio::test]
    async fn an_unreachable_daemon_is_an_error_not_a_missing_capability() {
        let dir = tempfile::tempdir().unwrap();
        let client = tend::ipc::Client::new(dir.path().join("absent.sock"));
        assert!(
            daemon_supports(&client, tend::decision::HUMAN_DECISION_CAPABILITY)
                .await
                .is_err()
        );
    }

    #[test]
    fn no_fields_change_nothing_and_bad_fields_are_refused() {
        let mut args = object(json!({"body": "x"}));
        attention_decision(&mut args, true).unwrap();
        assert_eq!(Value::Object(args), json!({"body": "x"}));
        let mut bad = object(json!({"body": "x", "options": "merge"}));
        assert!(attention_decision(&mut bad, true).is_err());
        let mut bad = object(json!({"body": "x", "evidence": ["commit:nothex"]}));
        assert!(attention_decision(&mut bad, true).is_err());
    }
}
