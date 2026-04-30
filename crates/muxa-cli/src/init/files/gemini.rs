//! `~/.gemini/settings.json` content layer.
//!
//! Same shape as `claude.rs` (JSON, hooks dict, append-with-dedupe),
//! different event names and no `statusLine`. The duplication is
//! intentional: each adapter's event list is small but specific, and
//! a generic "JSON hooks merger" would obscure the per-agent contract.

use crate::init::marker::Outcome;
use anyhow::{Context, Result};
use serde_json::{json, Map, Value};

const HOOK_EVENTS: &[(&str, &str)] = &[
    ("SessionStart", "session_start"),
    ("BeforeAgent", "before_agent"),
    ("AfterAgent", "after_agent"),
    ("BeforeTool", "before_tool"),
    ("AfterTool", "after_tool"),
    ("Notification", "notification"),
    ("SessionEnd", "session_end"),
];

const HOOK_CMD_PREFIX: &str = "muxa hook gemini";

pub fn default_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".gemini").join("settings.json"))
}

pub fn upsert(original: &str) -> Result<(String, Outcome)> {
    let mut root = parse_or_empty(original)?;
    let root_obj = ensure_object(&mut root);
    let changed = upsert_hooks(root_obj);
    let new_text = pretty_print(&root)?;
    let outcome = if original.is_empty() {
        Outcome::Inserted
    } else if changed || new_text.trim() != original.trim() {
        Outcome::Replaced
    } else {
        Outcome::Unchanged
    };
    Ok((new_text, outcome))
}

pub fn remove(original: &str) -> Result<(String, Outcome)> {
    if original.trim().is_empty() {
        return Ok((original.to_string(), Outcome::AlreadyAbsent));
    }
    let mut root = parse_or_empty(original)?;
    let root_obj = ensure_object(&mut root);

    let mut changed = false;
    if let Some(Value::Object(hooks)) = root_obj.get_mut("hooks") {
        let event_keys: Vec<String> = hooks.keys().cloned().collect();
        for ev in event_keys {
            let Some(Value::Array(arr)) = hooks.get_mut(&ev) else {
                continue;
            };
            let before = arr.len();
            arr.retain(|entry| !entry_owned_by_us(entry));
            if arr.len() != before {
                changed = true;
            }
            if arr.is_empty() {
                hooks.remove(&ev);
                changed = true;
            }
        }
        if hooks.is_empty() {
            root_obj.remove("hooks");
        }
    }

    let new_text = pretty_print(&root)?;
    Ok((
        new_text,
        if changed {
            Outcome::Removed
        } else {
            Outcome::AlreadyAbsent
        },
    ))
}

fn parse_or_empty(text: &str) -> Result<Value> {
    if text.trim().is_empty() {
        Ok(Value::Object(Map::new()))
    } else {
        serde_json::from_str(text).context("parsing gemini settings.json")
    }
}

fn ensure_object(v: &mut Value) -> &mut Map<String, Value> {
    if !v.is_object() {
        *v = Value::Object(Map::new());
    }
    v.as_object_mut().expect("just ensured object")
}

fn upsert_hooks(root: &mut Map<String, Value>) -> bool {
    let hooks = root
        .entry("hooks".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !hooks.is_object() {
        *hooks = Value::Object(Map::new());
    }
    let hooks_obj = hooks.as_object_mut().expect("just ensured object");

    let mut changed = false;
    for (event_name, flag) in HOOK_EVENTS {
        let arr = hooks_obj
            .entry((*event_name).to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        if !arr.is_array() {
            *arr = Value::Array(Vec::new());
            changed = true;
        }
        let arr = arr.as_array_mut().expect("just ensured array");
        let want_cmd = format!("muxa hook gemini --event {flag}");
        if arr.iter().any(|e| entry_command_eq(e, &want_cmd)) {
            continue;
        }
        arr.push(json!({
            "hooks": [{ "type": "command", "command": want_cmd }]
        }));
        changed = true;
    }
    changed
}

fn entry_owned_by_us(entry: &Value) -> bool {
    let Some(arr) = entry.get("hooks").and_then(Value::as_array) else {
        return false;
    };
    arr.iter().any(|h| {
        h.get("command")
            .and_then(Value::as_str)
            .is_some_and(|c| c.trim_start().starts_with(HOOK_CMD_PREFIX))
    })
}

fn entry_command_eq(entry: &Value, want_cmd: &str) -> bool {
    let Some(arr) = entry.get("hooks").and_then(Value::as_array) else {
        return false;
    };
    arr.iter().any(|h| {
        h.get("command")
            .and_then(Value::as_str)
            .is_some_and(|c| c.trim() == want_cmd)
    })
}

fn pretty_print(v: &Value) -> Result<String> {
    let mut s = serde_json::to_string_pretty(v)?;
    s.push('\n');
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_into_empty_creates_all_events() {
        let (out, o) = upsert("").unwrap();
        assert_eq!(o, Outcome::Inserted);
        let v: Value = serde_json::from_str(&out).unwrap();
        for (event, _) in HOOK_EVENTS {
            assert!(
                v["hooks"][event].as_array().is_some_and(|a| !a.is_empty()),
                "missing {event}"
            );
        }
    }

    #[test]
    fn upsert_is_idempotent() {
        let (first, _) = upsert("").unwrap();
        let (second, o) = upsert(&first).unwrap();
        assert_eq!(o, Outcome::Unchanged);
        assert_eq!(first, second);
    }

    #[test]
    fn upsert_preserves_user_hooks_and_extra_keys() {
        let user = r#"{
  "theme": "dark",
  "hooks": {
    "BeforeAgent": [
      { "hooks": [{ "type": "command", "command": "echo user" }] }
    ]
  }
}"#;
        let (out, _) = upsert(user).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["theme"], "dark");
        let arr = v["hooks"]["BeforeAgent"].as_array().unwrap();
        assert_eq!(arr.len(), 2, "user entry + muxa entry");
    }

    #[test]
    fn remove_strips_only_our_entries() {
        let user = r#"{
  "hooks": {
    "BeforeAgent": [
      { "hooks": [{ "type": "command", "command": "echo user" }] }
    ]
  }
}"#;
        let (with, _) = upsert(user).unwrap();
        let (after, o) = remove(&with).unwrap();
        assert_eq!(o, Outcome::Removed);
        let v: Value = serde_json::from_str(&after).unwrap();
        let arr = v["hooks"]["BeforeAgent"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert!(arr[0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("echo user"));
    }
}
