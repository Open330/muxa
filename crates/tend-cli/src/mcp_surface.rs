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
    ("tend_agent_find", "tend_status"),
    ("tend_capture_pane", "tend_status"),
    ("tend_recent_prompts", "tend_status"),
    ("tend_fleet_status", "tend_status"),
    ("tend_fleet_capture", "tend_status"),
    ("tend_fleet_send_prompt", "tend_send_prompt"),
    ("tend_dispatch_work", "tend_start_work"),
    ("tend_agent_read", "tend_agent_search"),
    ("tend_send_message", "tend_call_peer"),
    ("tend_fleet_call_peer", "tend_call_peer"),
    ("tend_list_messages", "tend_inbox"),
    ("tend_peer_report", "tend_inbox"),
    ("tend_fleet_update_request", "tend_update_request"),
    ("tend_cancel_message", "tend_update_request"),
    ("tend_withdraw_reply_wake", "tend_update_request"),
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
            "description": "Orientation; call once. topic=collaboration (default): your identity, peers, launch preferences, guardrails (workflows=true adds examples). room: refresh peers and unread counts. launch: launch preferences.",
            "inputSchema": {"type": "object", "properties": {
                "topic": {"type": "string", "enum": ["collaboration", "room", "launch"]},
                "workflows": {"type": "boolean" }
            }, "additionalProperties": false}
        }),
        json!({
            "name": "tend_wait",
            "description": "Wait on one handle: a request id (its reply), a swap job (until it ends), a dispatch UUID (current status, at once), or a pane's next state change (pane, no id). A timeout changes nothing. Prefer wakes; wait 30-60 s, not 1 s probes.",
            "inputSchema": {"type": "object", "properties": {
                "id": {"type": "string" },
                "kind": {"type": "string", "enum": ["request", "dispatch", "swap"] },
                "timeout_secs": {"type": "integer", "minimum": 1, "maximum": 600 },
                "after_update": {"type": "integer", "minimum": 0, "description": "Fleet: return on newer progress."},
                "host": {"type": "string", "description": "Where it runs (default local); a Fleet request sent elsewhere also needs pane_key."},
                "pane_key": {"type": "object"},
                "pane": {"type": "string" },
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
        "tend_status" => route_status(args),
        "tend_send_prompt" if remote_host(&args).is_some() => route_fleet_prompt(&args),
        "tend_start_work" if args.get("commit").is_some() => route_dispatch(&args),
        "tend_call_peer" => Ok(route_call_peer(args)),
        "tend_inbox" => route_inbox(args),
        "tend_update_request" => route_update(args),
        "tend_agent_search" if args.get("event_id").is_some() => Ok(routed(
            "tend_agent_read",
            copy(
                &args,
                &[
                    "event_id",
                    "before",
                    "after",
                    "include_tool_pair",
                    "text_offset",
                    "max_chars",
                ],
            ),
        )),
        _ => Ok(Routed {
            name: name.to_string(),
            args,
            replaced_by: None,
        }),
    }
}

/// The kind of a wait handle, from its id alone (where it runs is `host`'s
/// business): a mailbox request id (`req_…`, or `dispatch-<uuid>` for an
/// idempotent send, Fleet ones included), a remembered Fleet request, a
/// swap job (`swap-<uuid>`), or a bare dispatch UUID. Anything else is taken
/// as a caller-chosen swap `request_key`.
fn handle_kind(id: &str) -> &'static str {
    if id.starts_with("req_") || id.starts_with("dispatch-") || fleet_handle(id).is_some() {
        "request"
    } else if is_uuid(id) {
        "dispatch"
    } else {
        "swap"
    }
}

fn plain(name: &str, args: serde_json::Map<String, Value>) -> Routed {
    Routed {
        name: name.to_string(),
        args: Value::Object(args),
        replaced_by: None,
    }
}

/// `tend_send_prompt` on a Fleet host.
fn route_fleet_prompt(args: &Value) -> Result<Routed, String> {
    // The Fleet route cannot enforce these yet; refuse rather than
    // send without them.
    for unsupported in ["session_id", "socket"] {
        if args.get(unsupported).is_some() {
            return Err(format!(
                "{unsupported} is not supported with host yet; nothing was sent"
            ));
        }
    }
    Ok(plain(
        "tend_fleet_send_prompt",
        copy(args, &["host", "pane", "text", "submit"]),
    ))
}

/// `tend_start_work` with a commit: a Fleet dispatch.
fn route_dispatch(args: &Value) -> Result<Routed, String> {
    let mut out = copy(
        args,
        &[
            "dispatch_id",
            "workspace",
            "work",
            "commit",
            "body",
            "selector",
            "host",
            "plan",
        ],
    );
    // dry_run is the start_work spelling of the dispatch plan.
    if let Some(dry_run) = args.get("dry_run") {
        if out.get("plan").is_some_and(|plan| plan != dry_run) {
            return Err("dry_run and plan disagree; nothing was dispatched".into());
        }
        out.insert("plan".into(), dry_run.clone());
    }
    for unsupported in [
        "pipeline",
        "cwd",
        "skill",
        "context",
        "external",
        "no_ticket",
        "refresh",
    ] {
        if args.get(unsupported).is_some() {
            return Err(format!(
                    "{unsupported} is not supported with commit (Fleet dispatch); nothing was dispatched"
                ));
        }
    }
    Ok(plain("tend_dispatch_work", out))
}

/// `host` naming a Fleet host other than this one.
fn remote_host(args: &Value) -> Option<&str> {
    args.get("host")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|host| !host.is_empty() && *host != "local")
}

fn route_status(mut args: Value) -> Result<Routed, String> {
    let routed = |name: &str, args: serde_json::Map<String, Value>| Routed {
        name: name.to_string(),
        args: Value::Object(args),
        replaced_by: None,
    };
    match args.get("host").and_then(Value::as_str).map(str::trim) {
        // The whole Fleet, explicitly.
        Some("all") => {
            if args.get("pane").is_some() {
                return Err("host=all reads the Fleet summary; name a host to read a pane".into());
            }
            return Ok(routed(
                "tend_fleet_status",
                copy(&args, &["selector", "full"]),
            ));
        }
        Some("local" | "") => {
            if let Some(object) = args.as_object_mut() {
                object.remove("host");
            }
        }
        Some(_) => {
            if args.get("pane").is_none() {
                return Err(
                    "name a pane on that host, or use host=all for the Fleet summary".into(),
                );
            }
            return Ok(routed("tend_fleet_capture", copy(&args, &["host", "pane"])));
        }
        None => {}
    }
    if args.get("query").is_some() {
        return Ok(routed(
            "tend_agent_find",
            copy(&args, &["query", "limit", "refresh", "exclude_sessions"]),
        ));
    }
    if args.get("pane").is_none() && args.get("history_limit").is_some() {
        let mut out = copy(&args, &["full"]);
        if let Some(limit) = args.get("history_limit") {
            out.insert("limit".into(), limit.clone());
        }
        return Ok(routed("tend_recent_prompts", out));
    }
    Ok(Routed {
        name: "tend_status".into(),
        args,
        replaced_by: None,
    })
}

/// `tend_call_peer` is every durable request to an agent:
/// - `host` (not local): one exact pane there (`pane`, or `target` as the
///   pane) through the Fleet route;
/// - `exact=true` or `intent=notice`: exact mailbox delivery to `target`,
///   queued on a protected primary rather than rerouted to a worker;
/// - otherwise the colleague route that selects or creates a worker.
///
/// It no longer waits by default: the reply arrives as a wake.
fn route_call_peer(mut args: Value) -> Routed {
    let object = args.as_object_mut().expect("tool arguments are an object");
    object.entry("wait").or_insert(json!(false));
    if remote_host(&args).is_some() {
        if args.get("pane").is_none() {
            if let Some(target) = args.get("target").cloned() {
                args["pane"] = target;
            }
        }
        if let Some(object) = args.as_object_mut() {
            object.remove("target");
            object.remove("exact");
        }
        return Routed {
            name: "tend_fleet_call_peer".into(),
            args,
            replaced_by: None,
        };
    }
    let notice = args.get("intent").and_then(Value::as_str) == Some("notice");
    let exact = args.get("exact").and_then(Value::as_bool).unwrap_or(false);
    if notice || exact {
        let mut out = args.as_object().cloned().unwrap_or_default();
        out.remove("exact");
        out.remove("wait");
        out.remove("timeout_secs");
        let kind = out.remove("intent").unwrap_or(json!("review"));
        out.insert("kind".into(), kind);
        let execute = out
            .remove("execute")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        out.insert(
            "work_mode".into(),
            json!(if execute { "execute" } else { "read_only" }),
        );
        for spawn_only in [
            "skill",
            "context",
            "spawn_if_missing",
            "context_bound",
            "spawn_agent",
            "spawn_timeout_secs",
        ] {
            out.remove(spawn_only);
        }
        return Routed {
            name: "tend_send_message".into(),
            args: Value::Object(out),
            replaced_by: None,
        };
    }
    if let Some(object) = args.as_object_mut() {
        object.remove("exact");
    }
    Routed {
        name: "tend_call_peer".into(),
        args,
        replaced_by: None,
    }
}

/// `tend_inbox`: claim incoming work (default), or read history without
/// claiming (`view=incoming|sent|all`), or the newest peer reply
/// (`view=report`).
fn route_inbox(args: Value) -> Result<Routed, String> {
    let view = args.get("view").and_then(Value::as_str).map(str::to_string);
    let routed = |name: &str, args: serde_json::Map<String, Value>| Routed {
        name: name.to_string(),
        args: Value::Object(args),
        replaced_by: None,
    };
    match view.as_deref() {
        None => Ok(routed("tend_inbox", copy(&args, &["request_id", "full"]))),
        Some(mailbox @ ("incoming" | "sent" | "all")) => {
            let mut out = copy(
                &args,
                &["request_id", "limit", "offset", "include_body", "after_update", "full"],
            );
            out.insert("mailbox".into(), json!(mailbox));
            Ok(routed("tend_list_messages", out))
        }
        Some("report") => Ok(routed(
            "tend_peer_report",
            copy(&args, &["target", "request_id", "full"]),
        )),
        Some(other) => Err(format!(
            "unknown inbox view {other:?}; omit it to claim work, or use incoming, sent, all or report"
        )),
    }
}

/// `tend_update_request`: a progress/guidance `body` (Fleet requests sent
/// by this process need no `host` or `pane_key`), `action=cancel` while queued,
/// or `action=withdraw_wake` for your own requests' reply wakes.
fn route_update(mut args: Value) -> Result<Routed, String> {
    let routed = |name: &str, args: serde_json::Map<String, Value>| Routed {
        name: name.to_string(),
        args: Value::Object(args),
        replaced_by: None,
    };
    match args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("update")
    {
        "update" => {
            if let Some(object) = args.as_object_mut() {
                object.remove("action");
            }
            let id = args
                .get("request_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if remote_host(&args).is_none() && args.get("host").is_none() {
                if let Some((host, pane_key)) = fleet_handle(id) {
                    args["host"] = json!(host);
                    args["pane_key"] = pane_key;
                }
            }
            if remote_host(&args).is_some() {
                return Ok(Routed {
                    name: "tend_fleet_update_request".into(),
                    args,
                    replaced_by: None,
                });
            }
            Ok(Routed {
                name: "tend_update_request".into(),
                args,
                replaced_by: None,
            })
        }
        "cancel" => Ok(routed("tend_cancel_message", copy(&args, &["request_id"]))),
        "withdraw_wake" => {
            let mut out = copy(&args, &["request_ids"]);
            if !out.contains_key("request_ids") {
                if let Some(id) = args.get("request_id") {
                    out.insert("request_ids".into(), json!([id]));
                }
            }
            Ok(routed("tend_withdraw_reply_wake", out))
        }
        other => Err(format!(
            "unknown update action {other:?}; use update (default), cancel or withdraw_wake"
        )),
    }
}

/// Ids the daemon stamps from the caller's pane. Still accepted, not
/// advertised.
const DERIVED_IDS: &[&str] = &["thread_id", "workspace_id", "work_id", "run_id"];

/// Fold the absorbed tools' arguments into the listed schemas, tersely.
fn schema_extension(name: &str) -> Option<(Value, &'static str)> {
    Some(match name {
    "tend_status" => (
        json!({
            "query": {"type": "string", "description": "Find agents by task: keywords or an identifier. Ranked panes with evidence; not proof of ownership."},
            "limit": {"type": "integer", "minimum": 1, "maximum": 50 },
            "refresh": {"type": "boolean" },
            "exclude_sessions": {"type": "array", "items": {"type": "string"}, "description": "query: skip these (yours)."},
            "host": {"type": "string", "description": "A Fleet host with pane: its screen there. all: the Fleet summary (selector filters)."},
            "selector": {"type": "string" }
        }),
        "",
    ),
    "tend_send_prompt" => (
        json!({"host": {"type": "string", "description": "Control-mode Fleet host (not gated on Idle yet)."}}),
        "",
    ),
    "tend_start_work" => (
        json!({
            "commit": {"type": "string", "description": "Dispatch via the Fleet coordinator: isolated worktree at this commit (with dispatch_id, workspace, body; dry_run plans). Launched means started, not done."},
            "dispatch_id": {"type": "string" },
            "host": {"type": "string"},
            "selector": {"type": "string"},
            "plan": {"type": "boolean" }
        }),
        "",
    ),
    "tend_call_peer" => (
        json!({
            "intent": {"type": "string", "enum": ["review", "question", "task", "notice"], "description": "Default review. notice is one-way, delivered exactly to target."},
            "exact": {"type": "boolean", "description": "Deliver to exactly target (queued on a protected primary) instead of selecting or creating a worker."},
            "notify": {"type": "boolean" },
            "expects_reply": {"type": "boolean" },
            "host": {"type": "string", "description": "Control-mode Fleet host: one exact pane (pane or target); never auto-selects remotely."},
            "pane": {"type": "string" },
            "dispatch_id": {"type": "string", "description": "host: UUID, reuse on retry."}
        }),
        " Returns at once by default (wait=false); the reply arrives as a wake, read it with tend_inbox(request_id) or tend_wait(id).",
    ),
    "tend_inbox" => (
        json!({
            "view": {"type": "string", "enum": ["incoming", "sent", "all", "report"], "description": "Read without claiming: history, or report (newest peer reply; target filters)."},
            "target": {"type": "string"},
            "limit": {"type": "integer", "minimum": 1, "maximum": 100},
            "offset": {"type": "integer", "minimum": 0},
            "include_body": {"type": "boolean"},
            "after_update": {"type": "integer", "minimum": 0}
        }),
        "",
    ),
    "tend_update_request" => (
        json!({
            "action": {"type": "string", "enum": ["update", "cancel", "withdraw_wake"], "description": "update (default): append body. cancel: yours, while queued. withdraw_wake: no wake for your request_ids' late replies."},
            "request_ids": {"type": "array", "maxItems": 16, "items": {"type": "string"}},
            "host": {"type": "string", "description": "Fleet request sent elsewhere; with pane_key."},
            "pane_key": {"type": "object"}
        }),
        "",
    ),
    "tend_agent_search" => (
        json!({
            "event_id": {"type": "string", "description": "Expand one hit: events around it with paired tool results."},
            "before": {"type": "integer", "minimum": 0, "maximum": 50},
            "after": {"type": "integer", "minimum": 0, "maximum": 50},
            "include_tool_pair": {"type": "boolean"},
            "text_offset": {"type": "integer", "minimum": 0},
            "max_chars": {"type": "integer", "minimum": 1, "maximum": 64000}
        }),
        "",
    ),
        _ => return None,
    })
}

pub(crate) fn extend_schemas(tools: &mut [Value]) {
    for tool in tools {
        let Some(name) = tool["name"].as_str().map(str::to_string) else {
            continue;
        };
        let Some((extra, description)) = schema_extension(&name) else {
            continue;
        };
        if let (Some(properties), Value::Object(extra)) =
            (tool["inputSchema"]["properties"].as_object_mut(), extra)
        {
            properties.extend(extra);
        }
        if let Some(properties) = tool["inputSchema"]["properties"].as_object_mut() {
            for id in DERIVED_IDS {
                if matches!(name.as_str(), "tend_call_peer" | "tend_request_attention") {
                    properties.remove(*id);
                }
            }
        }
        if matches!(name.as_str(), "tend_update_request" | "tend_agent_search") {
            // Each action needs its own argument now; the handler checks.
            if let Some(schema) = tool["inputSchema"].as_object_mut() {
                schema.remove("required");
            }
        }
        if name == "tend_agent_search" {
            // event_id alone is a valid call now.
            if let Some(schema) = tool["inputSchema"].as_object_mut() {
                schema.remove("required");
            }
        }
        if !description.is_empty() {
            if let Some(text) = tool["description"].as_str() {
                tool["description"] = json!(format!("{text}{description}"));
            }
        }
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
        .unwrap_or_else(|| handle_kind(id));
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
            // An omitted host may use the remembered Fleet handle; an
            // explicit `local` always means this daemon.
            let host = args.get("host").and_then(Value::as_str).map(str::trim);
            if host == Some("local") {
                out.remove("after_update");
                return Ok(routed("tend_wait_reply", out));
            }
            let explicit = host
                .filter(|host| !host.is_empty())
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
    fn real_handle_formats_are_typed_from_the_id_and_host_only_picks_where() {
        // Idempotent sends (Fleet ones always) get `dispatch-<uuid>` ids.
        let id = "dispatch-2843585f-9b23-40c9-8b7d-f129aadf6974";
        assert_eq!(
            route_ok("tend_wait", json!({"id": id})).name,
            "tend_wait_reply"
        );
        remember_fleet_request(id, "mini", &json!({"pane": "%2"}));
        let remote = route_ok("tend_wait", json!({"id": id}));
        assert_eq!(remote.name, "tend_fleet_wait_reply");
        assert_eq!(remote.args["host"], "mini");
        // An explicit local never follows the remembered remote handle.
        let local = route_ok(
            "tend_wait",
            json!({"id": id, "kind": "request", "host": "local"}),
        );
        assert_eq!(local.name, "tend_wait_reply");
        assert!(local.args.get("host").is_none());
        // A swap job stays a swap job wherever it runs.
        let swap = "swap-2843585f-9b23-40c9-8b7d-f129aadf6974";
        for host in [None, Some("local"), Some("mini")] {
            let mut args = json!({"id": swap});
            if let Some(host) = host {
                args["host"] = json!(host);
            }
            let routed = route_ok("tend_wait", args);
            assert_eq!(routed.name, "tend_swap_account", "{host:?}");
            assert_eq!(routed.args.get("host").and_then(Value::as_str), host);
        }
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
    fn merged_observation_and_control_tools_route_to_their_handlers() {
        assert_eq!(
            route_ok("tend_status", json!({"query": "ENG-1"})).name,
            "tend_agent_find"
        );
        assert_eq!(
            route_ok("tend_status", json!({"host": "mini", "pane": "%3"})).name,
            "tend_fleet_capture"
        );
        assert_eq!(
            route_ok(
                "tend_status",
                json!({"host": "all", "selector": "env=prod"})
            )
            .name,
            "tend_fleet_status"
        );
        assert!(route("tend_status", json!({"host": "mini"})).is_err());
        let local = route_ok(
            "tend_status",
            json!({"host": "local", "pane": "%1", "include_capture": true}),
        );
        assert_eq!(local.name, "tend_status");
        assert_eq!(local.args, json!({"pane": "%1", "include_capture": true}));
        assert!(route(
            "tend_send_prompt",
            json!({"host": "mini", "pane": "%1", "text": "x", "session_id": "s"})
        )
        .is_err());
        let plan = route_ok(
            "tend_start_work",
            json!({"work": "W", "commit": "abc", "dry_run": true}),
        );
        assert_eq!(plan.args["plan"], true);
        assert!(route(
            "tend_start_work",
            json!({"work": "W", "commit": "abc", "dry_run": true, "plan": false})
        )
        .is_err());
        assert!(route(
            "tend_start_work",
            json!({"work": "W", "commit": "abc", "pipeline": "p"})
        )
        .is_err());
        let prompts = route_ok("tend_status", json!({"history_limit": 5}));
        assert_eq!(prompts.name, "tend_recent_prompts");
        assert_eq!(prompts.args, json!({"limit": 5}));
        let pane = route_ok("tend_status", json!({"pane": "%1", "history_limit": 5}));
        assert_eq!(pane.name, "tend_status");
        assert_eq!(
            route_ok(
                "tend_send_prompt",
                json!({"host": "mini", "pane": "%1", "text": "x"})
            )
            .name,
            "tend_fleet_send_prompt"
        );
        assert_eq!(
            route_ok(
                "tend_send_prompt",
                json!({"host": "local", "pane": "%1", "text": "x"})
            )
            .name,
            "tend_send_prompt"
        );
        assert_eq!(
            route_ok("tend_start_work", json!({"work": "W", "commit": "abc"})).name,
            "tend_dispatch_work"
        );
        assert_eq!(
            route_ok("tend_agent_search", json!({"event_id": "e1"})).name,
            "tend_agent_read"
        );
        assert_eq!(
            route_ok("tend_agent_search", json!({"query": "x"})).name,
            "tend_agent_search"
        );
    }

    #[test]
    fn call_peer_folds_exact_notice_and_fleet_sends_and_does_not_wait() {
        let peer = route_ok("tend_call_peer", json!({"body": "review"}));
        assert_eq!(peer.name, "tend_call_peer");
        assert_eq!(peer.args["wait"], false);
        let kept = route_ok("tend_call_peer", json!({"body": "x", "wait": true}));
        assert_eq!(kept.args["wait"], true);
        let notice = route_ok(
            "tend_call_peer",
            json!({"target": "%2", "intent": "notice", "body": "fyi"}),
        );
        assert_eq!(notice.name, "tend_send_message");
        assert_eq!(
            notice.args,
            json!({"target": "%2", "kind": "notice", "body": "fyi", "work_mode": "read_only"})
        );
        let exact = route_ok(
            "tend_call_peer",
            json!({"target": "%2", "exact": true, "intent": "task", "execute": true, "body": "do", "skill": "s"}),
        );
        assert_eq!(exact.name, "tend_send_message");
        assert_eq!(exact.args["kind"], "task");
        assert_eq!(exact.args["work_mode"], "execute");
        assert!(exact.args.get("skill").is_none());
        let fleet = route_ok(
            "tend_call_peer",
            json!({"host": "mini", "target": "%4", "body": "r"}),
        );
        assert_eq!(fleet.name, "tend_fleet_call_peer");
        assert_eq!(fleet.args["pane"], "%4");
        assert!(fleet.args.get("target").is_none());
    }

    #[test]
    fn inbox_views_and_update_actions() {
        assert_eq!(
            route_ok("tend_inbox", json!({"request_id": "r"})).name,
            "tend_inbox"
        );
        let sent = route_ok("tend_inbox", json!({"view": "sent", "limit": 5}));
        assert_eq!(sent.name, "tend_list_messages");
        assert_eq!(sent.args, json!({"mailbox": "sent", "limit": 5}));
        assert_eq!(
            route_ok("tend_inbox", json!({"view": "report"})).name,
            "tend_peer_report"
        );
        assert!(route("tend_inbox", json!({"view": "nope"})).is_err());
        assert_eq!(
            route_ok(
                "tend_update_request",
                json!({"request_id": "r", "body": "b"})
            )
            .name,
            "tend_update_request"
        );
        assert_eq!(
            route_ok(
                "tend_update_request",
                json!({"request_id": "r", "action": "cancel"})
            )
            .name,
            "tend_cancel_message"
        );
        let withdraw = route_ok(
            "tend_update_request",
            json!({"request_id": "r", "action": "withdraw_wake"}),
        );
        assert_eq!(withdraw.name, "tend_withdraw_reply_wake");
        assert_eq!(withdraw.args, json!({"request_ids": ["r"]}));
        remember_fleet_request("req_remote", "mini", &json!({"pane": "%7"}));
        let remote = route_ok(
            "tend_update_request",
            json!({"request_id": "req_remote", "body": "b"}),
        );
        assert_eq!(remote.name, "tend_fleet_update_request");
        assert_eq!(remote.args["pane_key"], json!({"pane": "%7"}));
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
