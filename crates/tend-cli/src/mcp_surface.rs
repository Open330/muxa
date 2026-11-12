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
            "description": "Wait on one handle and return its result: a request id (its final reply; a Fleet request needs no other argument when this process sent it), a dispatch UUID (its current status, returned at once), a swap job (until it ends or timeout), or a pane's next state change (pane, no id). A timeout changes nothing. Prefer wakes over waiting; use 30-60 s foreground waits, not 1 s probes.",
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
        "tend_status" => Ok(route_status(args)),
        "tend_send_prompt" if remote_host(&args).is_some() => Ok(routed(
            "tend_fleet_send_prompt",
            copy(&args, &["host", "pane", "text", "submit"]),
        )),
        "tend_start_work" if args.get("commit").is_some() => Ok(routed(
            "tend_dispatch_work",
            copy(
                &args,
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
            ),
        )),
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

/// `host` naming a Fleet host other than this one.
fn remote_host(args: &Value) -> Option<&str> {
    args.get("host")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|host| !host.is_empty() && *host != "local")
}

fn route_status(args: Value) -> Routed {
    let routed = |name: &str, args: serde_json::Map<String, Value>| Routed {
        name: name.to_string(),
        args: Value::Object(args),
        replaced_by: None,
    };
    if args.get("host").is_some() && args.get("pane").is_some() {
        return routed("tend_fleet_capture", copy(&args, &["host", "pane"]));
    }
    if args.get("host").is_some() {
        return routed("tend_fleet_status", copy(&args, &["selector", "full"]));
    }
    if args.get("query").is_some() {
        return routed(
            "tend_agent_find",
            copy(&args, &["query", "limit", "refresh", "exclude_sessions"]),
        );
    }
    if args.get("pane").is_none() && args.get("history_limit").is_some() {
        let mut out = copy(&args, &["full"]);
        if let Some(limit) = args.get("history_limit") {
            out.insert("limit".into(), limit.clone());
        }
        return routed("tend_recent_prompts", out);
    }
    Routed {
        name: "tend_status".into(),
        args,
        replaced_by: None,
    }
}

/// Fold the absorbed tools' arguments into the listed schemas, tersely.
pub(crate) fn extend_schemas(tools: &mut [Value]) {
    for tool in tools {
        let Some(name) = tool["name"].as_str().map(str::to_string) else {
            continue;
        };
        let (extra, description): (Value, &str) = match name.as_str() {
            "tend_status" => (
                json!({
                    "query": {"type": "string", "description": "Find where agents working on a task are: a few keywords or an identifier, not a question. Returns ranked panes with evidence; matches do not prove ownership."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 50, "description": "query: default 10."},
                    "refresh": {"type": "boolean", "description": "query: re-observe inventory now."},
                    "exclude_sessions": {"type": "array", "items": {"type": "string"}, "description": "query: session ids to skip (yours)."},
                    "host": {"type": "string", "description": "A Fleet host: with pane, capture that pane there; without, the cached Fleet summary (selector filters hosts)."},
                    "selector": {"type": "string", "description": "host: label selector, e.g. env=prod."}
                }),
                " history_limit without pane lists recent prompts across panes. host reads a Fleet host; query finds agents by task.",
            ),
            "tend_send_prompt" => (
                json!({"host": {"type": "string", "description": "A control-mode Fleet host; pane is that host's pane id or session/window/%N. Not gated on Idle yet."}}),
                "",
            ),
            "tend_start_work" => (
                json!({
                    "commit": {"type": "string", "description": "Dispatch through the Fleet coordinator instead: prepare an isolated worktree at this commit and launch the pipeline (needs dispatch_id, workspace, body)."},
                    "dispatch_id": {"type": "string", "description": "commit: UUID; reuse it on retries."},
                    "host": {"type": "string"},
                    "selector": {"type": "string"},
                    "plan": {"type": "boolean", "description": "commit: read-only plan."}
                }),
                "",
            ),
            "tend_agent_search" => (
                json!({
                    "event_id": {"type": "string", "description": "Expand one hit instead of searching: events around it with paired tool calls/results."},
                    "before": {"type": "integer", "minimum": 0, "maximum": 50},
                    "after": {"type": "integer", "minimum": 0, "maximum": 50},
                    "include_tool_pair": {"type": "boolean"},
                    "text_offset": {"type": "integer", "minimum": 0},
                    "max_chars": {"type": "integer", "minimum": 1, "maximum": 64000}
                }),
                "",
            ),
            _ => continue,
        };
        if let (Some(properties), Value::Object(extra)) =
            (tool["inputSchema"]["properties"].as_object_mut(), extra)
        {
            properties.extend(extra);
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
                json!({"host": "mini", "selector": "env=prod"})
            )
            .name,
            "tend_fleet_status"
        );
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
