//! The listed MCP surface and the compatibility layer behind it.
//!
//! `tools/list` advertises one tool per job (`docs/proposals/MCP_SURFACE.md`).
//! Tools merged into another keep working under their old names for agents
//! that already hold the old schema: they are routed to the same handler and
//! their result names the replacement. New names are thin routes onto the
//! existing handlers, so behaviour is identical behind either name.
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;

/// Retired tool names and the listed tool that replaces each. Accepted, not
/// listed, for at least one release.
pub(crate) const RETIRED: &[(&str, &str)] = &[
    ("tend_wait_reply", "tend_wait"),
    ("tend_fleet_wait_reply", "tend_wait"),
    ("tend_wait_for_change", "tend_wait"),
    ("tend_dispatch_status", "tend_wait"),
    ("tend_collaboration_guide", "tend_guide"),
    ("tend_room_context", "tend_guide"),
];

pub(crate) fn replacement(name: &str) -> Option<&'static str> {
    RETIRED
        .iter()
        .find(|(old, _)| *old == name)
        .map(|(_, new)| *new)
}

pub(crate) fn is_listed(name: &str) -> bool {
    replacement(name).is_none()
}

/// Definitions of the tools this module introduces or redefines.
pub(crate) fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "tend_guide",
            "description": "Orientation. topic=collaboration (default): your identity (room.self), same-window peers, launch preferences and messaging guardrails; call once (workflows=true adds detailed examples). topic=room: a cheap refresh of peers and unread counts. topic=launch: the user's agent launch preferences only.",
            "inputSchema": {"type": "object", "properties": {
                "topic": {"type": "string", "enum": ["collaboration", "room", "launch"]},
                "workflows": {"type": "boolean", "description": "collaboration only: include detailed workflow examples."}
            }, "additionalProperties": false}
        }),
        json!({
            "name": "tend_wait",
            "description": "Wait on one handle and return its result: a request id (its final reply; a Fleet request needs no other argument when this process sent it), a dispatch UUID (its status), a swap job, or a pane's next state change (pane, no id). A timeout changes nothing. Prefer wakes over waiting; use 30-60 s foreground waits, not 1 s probes.",
            "inputSchema": {"type": "object", "properties": {
                "id": {"type": "string", "description": "Request id, dispatch UUID or swap job key."},
                "kind": {"type": "string", "enum": ["request", "dispatch", "swap"], "description": "Only when the id alone is ambiguous."},
                "timeout_secs": {"type": "integer", "minimum": 1, "maximum": 600, "description": "Default 30."},
                "after_update": {"type": "integer", "minimum": 0, "description": "Fleet request: return on progress newer than this sequence."},
                "host": {"type": "string", "description": "Fleet host of a request sent from another process; needs pane_key."},
                "pane_key": {"type": "object"},
                "pane": {"type": "string", "description": "Wait for this pane's state change instead of an id."},
                "until": {"type": "string", "enum": ["any", "settled", "idle", "blocked", "stopped"]},
                "include_capture": {"type": "boolean"},
                "max_capture_lines": {"type": "integer", "minimum": 1, "maximum": 400}
            }, "additionalProperties": false}
        }),
    ]
}

/// The Fleet handle (`host`, `pane_key`) of each request this MCP process
/// sent, so `tend_wait(id)` needs only the id.
static FLEET_HANDLES: Mutex<Option<HashMap<String, (String, Value)>>> = Mutex::new(None);

pub(crate) fn remember_fleet_request(request_id: &str, host: &str, pane_key: &Value) {
    if let Ok(mut handles) = FLEET_HANDLES.lock() {
        handles
            .get_or_insert_with(HashMap::new)
            .insert(request_id.to_string(), (host.to_string(), pane_key.clone()));
    }
}

fn fleet_handle(request_id: &str) -> Option<(String, Value)> {
    FLEET_HANDLES
        .lock()
        .ok()?
        .as_ref()?
        .get(request_id)
        .cloned()
}

/// A call after routing: the handler name to run and its arguments, plus
/// the replacement note for a retired name.
pub(crate) struct Routed {
    pub name: String,
    pub args: Value,
    pub replaced_by: Option<&'static str>,
}

fn is_uuid(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok()
}

fn copy(args: &Value, keys: &[&str]) -> serde_json::Map<String, Value> {
    keys.iter()
        .filter_map(|key| {
            args.get(*key)
                .map(|value| ((*key).to_string(), value.clone()))
        })
        .collect()
}

/// Route a listed tool onto its handler; a retired name runs its own
/// handler and is marked. Errors are user-facing argument problems.
pub(crate) fn route(name: &str, args: Value) -> Result<Routed, String> {
    if let Some(new) = replacement(name) {
        return Ok(Routed {
            name: name.to_string(),
            args,
            replaced_by: Some(new),
        });
    }
    let routed = |name: &str, args: serde_json::Map<String, Value>| Routed {
        name: name.to_string(),
        args: Value::Object(args),
        replaced_by: None,
    };
    match name {
        "tend_guide" => {
            let topic = args
                .get("topic")
                .and_then(Value::as_str)
                .unwrap_or("collaboration");
            match topic {
                "collaboration" => Ok(routed(
                    "tend_collaboration_guide",
                    copy(&args, &["workflows"]),
                )),
                "room" => Ok(routed("tend_room_context", serde_json::Map::new())),
                "launch" => Ok(routed("tend_launch_guide", serde_json::Map::new())),
                other => Err(format!(
                    "unknown guide topic {other:?}; use collaboration, room or launch"
                )),
            }
        }
        "tend_wait" => route_wait(&args),
        _ => Ok(Routed {
            name: name.to_string(),
            args,
            replaced_by: None,
        }),
    }
}

fn route_wait(args: &Value) -> Result<Routed, String> {
    let routed = |name: &str, args: serde_json::Map<String, Value>| Routed {
        name: name.to_string(),
        args: Value::Object(args),
        replaced_by: None,
    };
    let id = args.get("id").and_then(Value::as_str).map(str::trim);
    let Some(id) = id.filter(|id| !id.is_empty()) else {
        if args.get("pane").is_some() {
            return Ok(routed(
                "tend_wait_for_change",
                copy(
                    args,
                    &[
                        "pane",
                        "until",
                        "include_capture",
                        "max_capture_lines",
                        "timeout_secs",
                    ],
                ),
            ));
        }
        return Err("tend_wait needs an id (request, dispatch or swap job) or a pane".into());
    };
    let kind = args
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or(if is_uuid(id) {
            "dispatch"
        } else if id.starts_with("req_") || args.get("host").is_some() {
            "request"
        } else {
            "swap"
        });
    match kind {
        "dispatch" => Ok(routed("tend_dispatch_status", {
            let mut out = serde_json::Map::new();
            out.insert("dispatch_id".into(), json!(id));
            out
        })),
        "swap" => {
            let mut out = serde_json::Map::new();
            out.insert("action".into(), json!("status"));
            out.insert("job".into(), json!(id));
            let wait = args
                .get("timeout_secs")
                .and_then(Value::as_u64)
                .unwrap_or(30)
                .min(120);
            out.insert("wait_secs".into(), json!(wait));
            if let Some(host) = args.get("host") {
                out.insert("host".into(), host.clone());
            }
            Ok(routed("tend_swap_account", out))
        }
        "request" => {
            let mut out = copy(args, &["timeout_secs", "after_update"]);
            out.insert("request_id".into(), json!(id));
            let explicit = args
                .get("host")
                .and_then(Value::as_str)
                .filter(|host| *host != "local")
                .map(|host| (host.to_string(), args.get("pane_key").cloned()));
            match explicit {
                Some((host, Some(pane_key))) => {
                    out.insert("host".into(), json!(host));
                    out.insert("pane_key".into(), pane_key);
                    Ok(routed("tend_fleet_wait_reply", out))
                }
                Some((_, None)) => Err(
                    "a Fleet request from another process needs the pane_key its call returned"
                        .into(),
                ),
                None => {
                    if let Some((host, pane_key)) = fleet_handle(id) {
                        out.insert("host".into(), json!(host));
                        out.insert("pane_key".into(), pane_key);
                        Ok(routed("tend_fleet_wait_reply", out))
                    } else {
                        out.remove("after_update");
                        Ok(routed("tend_wait_reply", out))
                    }
                }
            }
        }
        other => Err(format!(
            "unknown wait kind {other:?}; use request, dispatch or swap"
        )),
    }
}

/// Add the replacement note to a retired tool's result.
pub(crate) fn annotate(mut result: Value, replaced_by: Option<&str>, old: &str) -> Value {
    if let Some(new) = replaced_by {
        if let Some(content) = result.get_mut("content").and_then(Value::as_array_mut) {
            content.push(json!({
                "type": "text",
                "text": format!("{{\"replaced_by\":\"{new}\",\"note\":\"{old} is retired and no longer listed; it keeps working for now. Use {new}.\"}}"),
            }));
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route_ok(name: &str, args: Value) -> Routed {
        route(name, args).unwrap()
    }

    #[test]
    fn tend_wait_routes_each_handle_to_its_handler() {
        let request = route_ok("tend_wait", json!({"id": "req_1", "timeout_secs": 40}));
        assert_eq!(request.name, "tend_wait_reply");
        assert_eq!(
            request.args,
            json!({"request_id": "req_1", "timeout_secs": 40})
        );
        let uuid = "2843585f-9b23-40c9-8b7d-f129aadf6974";
        assert_eq!(
            route_ok("tend_wait", json!({"id": uuid})).name,
            "tend_dispatch_status"
        );
        let swap = route_ok(
            "tend_wait",
            json!({"id": "nightly-swap", "timeout_secs": 500}),
        );
        assert_eq!(swap.name, "tend_swap_account");
        assert_eq!(swap.args["action"], "status");
        assert_eq!(swap.args["wait_secs"], 120);
        let pane = route_ok("tend_wait", json!({"pane": "%3", "until": "settled"}));
        assert_eq!(pane.name, "tend_wait_for_change");
        assert!(route("tend_wait", json!({})).is_err());
        assert!(route("tend_wait", json!({"id": "req_2", "host": "mini"})).is_err());
    }

    #[test]
    fn a_fleet_request_sent_here_is_waited_on_by_id_alone() {
        remember_fleet_request("req_fleet", "mini", &json!({"pane": "%9"}));
        let routed = route_ok("tend_wait", json!({"id": "req_fleet", "after_update": 2}));
        assert_eq!(routed.name, "tend_fleet_wait_reply");
        assert_eq!(routed.args["host"], "mini");
        assert_eq!(routed.args["pane_key"], json!({"pane": "%9"}));
        assert_eq!(routed.args["after_update"], 2);
    }

    #[test]
    fn guide_topics_and_retired_names() {
        assert_eq!(
            route_ok("tend_guide", json!({})).name,
            "tend_collaboration_guide"
        );
        assert_eq!(
            route_ok("tend_guide", json!({"topic": "room"})).name,
            "tend_room_context"
        );
        assert_eq!(
            route_ok("tend_guide", json!({"topic": "launch"})).name,
            "tend_launch_guide"
        );
        let old = route_ok("tend_room_context", json!({}));
        assert_eq!(old.name, "tend_room_context");
        assert_eq!(old.replaced_by, Some("tend_guide"));
        let noted = annotate(
            json!({"content": [{"type": "text", "text": "{}"}]}),
            old.replaced_by,
            "tend_room_context",
        );
        assert!(noted["content"][1]["text"]
            .as_str()
            .unwrap()
            .contains("\"replaced_by\":\"tend_guide\""));
        assert!(!is_listed("tend_wait_reply"));
        assert!(is_listed("tend_wait"));
    }
}
