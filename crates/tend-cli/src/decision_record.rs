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

/// Whether the daemon behind `client` advertises `capability`.
pub(crate) async fn daemon_supports(client: &tend::ipc::Client, capability: &str) -> bool {
    client
        .hello(std::time::Duration::from_secs(2))
        .await
        .is_ok_and(|hello| hello.capabilities.iter().any(|c| c == capability))
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
                tail: Some("ok".into())
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
        }));
        attention_decision(&mut args, false).unwrap();
        assert!(args.get("decision").is_none());
        assert_eq!(
            args["body"],
            "Merge #12?\n\nOptions:\n1. merge\n2. hold\n\nRecommendation: merge"
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
