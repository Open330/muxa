//! Provider-specific records become evidence events. Embedded JSON stays quoted text.
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::LazyLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub event_id: String,
    pub provider: String,
    pub session_id: String,
    pub cwd: Option<String>,
    pub timestamp: Option<String>,
    pub kind: String,
    pub text: String,
    pub tool_name: Option<String>,
    pub call_id: Option<String>,
    /// Only structured provider fields, never inferred from prose.
    pub is_error: Option<bool>,
    pub path: String,
    pub line: u64,
    pub byte_offset: u64,
    pub part: usize,
    pub truncated: bool,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Context {
    pub session_id: String,
    pub cwd: Option<String>,
}

static SIGNED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)\?X-Amz-[^\s"'<>\\]+"#).unwrap());
static SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r#"(?i)((?:[A-Z_-]*(?:SECRET[-_]ACCESS[-_]KEY|ACCESS[-_]KEY[-_]ID|TOKEN|API[-_]KEY|PASSWORD|SECRET|SECRET[-_]KEY|PRIVATE[-_]KEY)|authorization)(?:\\*["'])?\s*[:=]\s*(?:\\*["'])?)([^\s,"';\}\r\n]+)"#).unwrap()
});

static BEARER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(bearer\s+)[A-Za-z0-9._~+/=-]+").unwrap());
static AUTH_HEADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(authorization(?:\\*["'])?\s*[:=]\s*(?:\\*["'])?)(?:bearer|basic|token)\s+[^\s,"'\r\n}]+"#)
        .unwrap()
});
static QUOTED_SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r#"(?i)((?:[A-Z_-]*(?:SECRET[-_]ACCESS[-_]KEY|ACCESS[-_]KEY[-_]ID|TOKEN|API[-_]KEY|PASSWORD|SECRET|SECRET[-_]KEY|PRIVATE[-_]KEY)|authorization)(?:\\*["'])?\s*[:=]\s*)("(?:\\.|[^"\\])*"|'[^']*')"#).unwrap()
});
static ESCAPED_SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r#"(?i)((?:[A-Z_-]*(?:SECRET[-_]ACCESS[-_]KEY|ACCESS[-_]KEY[-_]ID|TOKEN|API[-_]KEY|PASSWORD|SECRET|SECRET[-_]KEY|PRIVATE[-_]KEY)|authorization)(?:\\*["'])?\s*[:=]\s*)(\\+"[^"\r\n]*\\+"|\\+'[^'\r\n]*\\+')"#).unwrap()
});
static KEY_MATERIAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?s)-----BEGIN (?:[A-Z]+ )?PRIVATE KEY-----.*?-----END (?:[A-Z]+ )?PRIVATE KEY-----",
    )
    .unwrap()
});
static TOKEN_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r"\b(?:gh[pousr]_[A-Za-z0-9_]{20,}|github_pat_[A-Za-z0-9_]{20,}|sk-(?:ant-|proj-)?[A-Za-z0-9_-]{16,}|xox[baprs]-[A-Za-z0-9-]{10,}|(?:ntn_|secret_)[A-Za-z0-9]{20,}|AKIA[A-Z0-9]{16})").unwrap()
});

static URL_USERINFO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)([a-z][a-z0-9+.-]*://)[^\s/?#@]+@([^\s/?#]+)").unwrap());
static MYSQL_PASSWORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(\bmysql(?:admin|dump)?\b[^\r\n;]*?\s-p)([^\s"';]+)"#).unwrap()
});
static ENV_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)((?:\\*["'])name(?:\\*["'])\s*:\s*(?:\\*["'])[A-Z0-9_-]*(?:PASSWORD|TOKEN|API[-_]KEY|SECRET|PRIVATE[-_]KEY)(?:\\*["'])\s*,\s*(?:\\*["'])value(?:\\*["'])\s*:\s*)(\\+"[^"\r\n]*\\+"|"(?:\\.|[^"\\])*"|'[^']*'|[^\s,}]+)"#).unwrap()
});

pub fn redact(text: &str) -> String {
    let text = KEY_MATERIAL.replace_all(text, "[private-key-redacted]");
    let text = SIGNED.replace_all(&text, "?[signed-query-redacted]");
    let text = URL_USERINFO.replace_all(&text, "${1}[credentials-redacted]@${2}");
    let text = MYSQL_PASSWORD.replace_all(&text, "${1}[redacted]");
    let text = ENV_VALUE.replace_all(&text, "${1}\"[redacted]\"");
    let text = ESCAPED_SECRET.replace_all(&text, "${1}[redacted]");
    let text = AUTH_HEADER.replace_all(&text, "${1}[redacted]");
    let text = BEARER.replace_all(&text, "${1}[redacted]");
    let text = QUOTED_SECRET.replace_all(&text, "${1}\"[redacted]\"");
    let text = SECRET.replace_all(&text, "${1}[redacted]");
    TOKEN_VALUE
        .replace_all(&text, "[token-redacted]")
        .into_owned()
}

pub fn bounded(text: &str, limit: usize) -> (String, bool) {
    let mut chars = text.char_indices();
    if let Some((end, _)) = chars.nth(limit) {
        (text[..end].to_owned(), true)
    } else {
        (text.to_owned(), false)
    }
}

fn string(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn display(v: &Value) -> String {
    render(v, 0)
}

fn sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_uppercase().replace('-', "_");
    [
        "TOKEN",
        "PASSWORD",
        "SECRET",
        "API_KEY",
        "SECRET_KEY",
        "PRIVATE_KEY",
        "SECRET_ACCESS_KEY",
        "ACCESS_KEY_ID",
        "AUTHORIZATION",
    ]
    .iter()
    .any(|suffix| key == *suffix || key.ends_with(&format!("_{suffix}")))
}

/// Decode structured tool arguments before rendering: JSON escapes must not
/// become FTS word prefixes (\\naws -> naws), or hide credential delimiters.
fn render(v: &Value, depth: usize) -> String {
    if depth >= 16 {
        return "[nested value omitted]".into();
    }
    match v {
        Value::String(text) => {
            if let Ok(decoded @ (Value::Object(_) | Value::Array(_))) =
                serde_json::from_str::<Value>(text)
            {
                render(&decoded, depth + 1)
            } else {
                redact(text)
            }
        }
        Value::Object(fields) => fields
            .iter()
            .map(|(key, value)| {
                format!(
                    "{}: {}",
                    redact(key),
                    if sensitive_key(key)
                        || (key == "value"
                            && fields
                                .get("name")
                                .and_then(Value::as_str)
                                .is_some_and(sensitive_key))
                    {
                        "[redacted]".into()
                    } else {
                        render(value, depth + 1)
                    }
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Array(parts) => parts
            .iter()
            .map(|p| render(p, depth + 1))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => v.to_string(),
    }
}

fn result_text(value: &Value) -> String {
    if let Some(parts) = value.as_array() {
        parts
            .iter()
            .filter(|p| {
                matches!(
                    p["type"].as_str(),
                    Some("text" | "input_text" | "output_text")
                )
            })
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        display(value)
    }
}

fn instruction(text: &str) -> bool {
    let t = text.trim_start();
    [
        "<environment_context>",
        "<skills_instructions>",
        "# AGENTS.md instructions",
        "Base directory for this skill:",
        "# AGENTS.md\n",
        "<system-reminder>",
    ]
    .iter()
    .any(|p| t.starts_with(p))
}

#[allow(clippy::too_many_lines)] // Provider wire variants are deliberately explicit and auditable.
pub fn events(v: &Value, context: &mut Context) -> Vec<Event> {
    let typ = v.get("type").and_then(Value::as_str).unwrap_or("");
    if typ == "session_meta" {
        if let Some(p) = v.get("payload") {
            if let Some(id) = string(p, "id") {
                context.session_id = id;
            }
            context.cwd = string(p, "cwd").or_else(|| context.cwd.clone());
        }
        return Vec::new();
    }
    if let Some(id) = string(v, "sessionId") {
        context.session_id = id;
    }
    context.cwd = string(v, "cwd").or_else(|| context.cwd.clone());
    if typ == "turn_context" {
        context.cwd = v
            .get("payload")
            .and_then(|p| string(p, "cwd"))
            .or_else(|| context.cwd.clone());
        return Vec::new();
    }
    let provider = if v.get("message").is_some() {
        "claude"
    } else {
        "codex"
    };
    let base = Event {
        event_id: String::new(),
        provider: provider.into(),
        session_id: context.session_id.clone(),
        cwd: context.cwd.clone(),
        timestamp: string(v, "timestamp"),
        kind: String::new(),
        text: String::new(),
        tool_name: None,
        call_id: None,
        is_error: None,
        path: String::new(),
        line: 0,
        byte_offset: 0,
        part: 0,
        truncated: false,
    };
    let mut result = Vec::new();
    if provider == "claude" && matches!(typ, "assistant" | "user") {
        let message = &v["message"];
        if let Some(text) = message["content"].as_str() {
            push(&mut result, &base, typ, text.into(), None, None, None);
        } else if let Some(parts) = message["content"].as_array() {
            for part in parts {
                match part["type"].as_str().unwrap_or("") {
                    "text" => push(
                        &mut result,
                        &base,
                        typ,
                        part["text"].as_str().unwrap_or("").into(),
                        None,
                        None,
                        None,
                    ),
                    "tool_use" => push(
                        &mut result,
                        &base,
                        "tool_call",
                        display(&part["input"]),
                        string(part, "name"),
                        string(part, "id"),
                        None,
                    ),
                    "tool_result" => push(
                        &mut result,
                        &base,
                        "tool_result",
                        result_text(&part["content"]),
                        None,
                        string(part, "tool_use_id"),
                        part["is_error"].as_bool(),
                    ),
                    _ => {} // reasoning, signatures, images and binary payloads are not indexed
                }
            }
        }
    } else if typ == "response_item" {
        let p = &v["payload"];
        match p["type"].as_str().unwrap_or("") {
            "message" => {
                let role = p["role"].as_str().unwrap_or("");
                if matches!(role, "user" | "assistant" | "developer" | "system") {
                    let kind = if matches!(role, "developer" | "system") {
                        "instruction"
                    } else {
                        role
                    };
                    let text = p["content"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|c| {
                            matches!(
                                c["type"].as_str(),
                                Some("input_text" | "output_text" | "text")
                            )
                        })
                        .filter_map(|c| c["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n");
                    push(&mut result, &base, kind, text, None, None, None);
                }
            }
            "function_call" | "custom_tool_call" => {
                let input = p
                    .get("arguments")
                    .or_else(|| p.get("input"))
                    .map(display)
                    .unwrap_or_default();
                push(
                    &mut result,
                    &base,
                    "tool_call",
                    input,
                    string(p, "name"),
                    string(p, "call_id"),
                    None,
                );
            }
            "function_call_output" | "custom_tool_call_output" => {
                push(
                    &mut result,
                    &base,
                    "tool_result",
                    result_text(&p["output"]),
                    None,
                    string(p, "call_id"),
                    None,
                );
            }
            _ => {}
        }
    }
    // Codex event_msg mirrors response_item messages/calls. Index the canonical
    // response_item once, not the lifecycle/progress copy or api-request blobs.
    result
}

/// Explicit exclusions are metadata/mirrors/binary blocks; unknown nested wire
/// variants must remain visible as coverage gaps, even with a known outer type.
pub fn unsupported(v: &Value) -> u64 {
    match v["type"].as_str().unwrap_or("") {
        "response_item" => match v["payload"]["type"].as_str().unwrap_or("") {
            "message" => {
                let p = &v["payload"];
                if !matches!(
                    p["role"].as_str(),
                    Some("user" | "assistant" | "developer" | "system")
                ) {
                    return 1;
                }
                blocks_unsupported(
                    &p["content"],
                    &[
                        "input_text",
                        "output_text",
                        "text",
                        "input_image",
                        "image",
                        "image_url",
                        "input_audio",
                        "audio",
                    ],
                )
            }
            "function_call"
            | "custom_tool_call"
            | "function_call_output"
            | "custom_tool_call_output"
            | "reasoning"
            | "ghost_snapshot"
            | "compaction" => 0,
            _ => 1,
        },
        "assistant" | "user" => claude_unsupported(&v["message"]["content"]),
        "event_msg" => u64::from(!matches!(
            v["payload"]["type"].as_str(),
            Some(
                "task_started"
                    | "task_complete"
                    | "turn_aborted"
                    | "item_started"
                    | "item_completed"
                    | "token_count"
                    | "user_message"
                    | "agent_message"
                    | "agent_reasoning"
                    | "thread_settings_applied"
                    | "warning"
                    | "error"
                    | "context_compacted"
                    | "shutdown_complete"
                    | "exec_command_begin"
                    | "exec_command_end"
                    | "mcp_tool_call_begin"
                    | "mcp_tool_call_end"
                    | "web_search_begin"
                    | "web_search_end"
                    | "patch_apply_begin"
                    | "patch_apply_end"
                    | "entered_review_mode"
                    | "exited_review_mode"
            )
        )),
        "session_meta"
        | "turn_context"
        | "token_usage_record"
        | "inter_agent_communication_metadata"
        | "compacted"
        | "system"
        | "progress"
        | "queue-operation"
        | "file-history-snapshot"
        | "summary"
        | "last-prompt"
        | "attachment"
        | "api-request-blob"
        | "api-request"
        | "api-request-shape"
        | "mode"
        | "permission-mode"
        | "atis-latch"
        | "ai-title"
        | "file-history-delta"
        | "world_state"
        | "pr-link"
        | "frame-link"
        | "cost-state"
        | "isolation-latch"
        | "tools-used" => 0,
        _ => 1,
    }
}

fn claude_unsupported(content: &Value) -> u64 {
    if content.is_string() {
        0
    } else {
        let mut n = blocks_unsupported(
            content,
            &[
                "text",
                "tool_use",
                "tool_result",
                "thinking",
                "redacted_thinking",
                "image",
                "image_url",
                "document",
                "audio",
            ],
        );
        if let Some(parts) = content.as_array() {
            for part in parts {
                if part["type"] == "tool_result" && part["content"].is_array() {
                    n += blocks_unsupported(
                        &part["content"],
                        &[
                            "text",
                            "input_text",
                            "output_text",
                            "tool_reference",
                            "image",
                            "image_url",
                            "audio",
                        ],
                    );
                }
            }
        }
        n
    }
}

fn blocks_unsupported(value: &Value, known: &[&str]) -> u64 {
    let Some(parts) = value.as_array() else {
        return 1;
    };
    parts
        .iter()
        .filter(|p| !known.contains(&p["type"].as_str().unwrap_or("")))
        .count() as u64
}

fn push(
    out: &mut Vec<Event>,
    base: &Event,
    kind: &str,
    text: String,
    tool_name: Option<String>,
    call_id: Option<String>,
    is_error: Option<bool>,
) {
    if text.trim().is_empty() {
        return;
    }
    let mut event = base.clone();
    event.kind = if instruction(&text) {
        "instruction"
    } else {
        kind
    }
    .into();
    let rendered_gap = text.contains("[nested value omitted]");
    let (text, truncated) = bounded(&redact(&text), 262_144);
    event.text = text;
    event.truncated = truncated || rendered_gap;
    event.tool_name = tool_name;
    event.call_id = call_id;
    event.is_error = is_error;
    event.part = out.len();
    out.push(event);
}
