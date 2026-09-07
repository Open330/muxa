//! `~/.claude/settings.json` content layer.
//!
//! Strategy:
//!
//! - **Hooks**: for each event we care about, ensure there is at least
//!   one entry whose `hooks[].command` matches `muxa hook claude
//!   --event <e>`. Other entries (user's own hooks, other tools) are
//!   preserved verbatim. Adding ours twice is impossible — we dedupe
//!   by command-prefix match.
//! - **statusLine**: only set when absent or already ours. If the user
//!   already has a custom `statusLine` (e.g. ccstatusline), we leave
//!   it alone and surface that fact to the caller.
//! - **Uninstall**: strip every hook entry whose command starts with
//!   `muxa hook claude`, plus the statusLine if we own it. Empty
//!   arrays / objects are pruned to keep the file tidy.
//!
//! Returns `(new_text, Outcome)` so the caller (apply.rs) handles I/O,
//! backups, and diffing.

use crate::init::marker::Outcome;
use anyhow::{Context, Result};
use serde_json::{json, Map, Value};

/// (Claude event name in settings.json, hook flag passed to muxa)
const HOOK_EVENTS: &[(&str, &str)] = &[
    ("SessionStart", "session_start"),
    ("UserPromptSubmit", "user_prompt_submit"),
    ("PreToolUse", "pre_tool_use"),
    ("PostToolUse", "post_tool_use"),
    ("Notification", "notification"),
    ("Stop", "stop"),
    ("SessionEnd", "session_end"),
];

const STATUSLINE_CMD: &str = "muxa hook claude-statusline";
const HOOK_CMD_PREFIX: &str = "muxa hook claude";

/// Result of an upsert telling the caller whether we touched
/// `statusLine` or skipped it because the user owns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusLineDecision {
    /// We wrote our statusLine into the file.
    Ours,
    /// File already had ours — no-op.
    AlreadyOurs,
    /// Some other tool's statusLine was present; we left it alone.
    SkippedUserOwned,
}

#[derive(Debug)]
pub struct UpsertReport {
    pub outcome: Outcome,
    pub statusline: StatusLineDecision,
}

/// Default path: `~/.claude/settings.json`.
pub fn default_path() -> Option<std::path::PathBuf> {
    crate::init::integration::claude_home().map(|h| h.join("settings.json"))
}

/// Insert muxa hooks + statusLine into the JSON text. An empty input
/// (file missing) is treated as `{}`.
pub fn upsert(original: &str) -> Result<(String, UpsertReport)> {
    let mut root = parse_or_empty(original)?;
    let root_obj = ensure_object(&mut root);

    let hooks_changed = upsert_hooks(root_obj);
    let statusline = upsert_statusline(root_obj);

    let new_text = pretty_print(&root)?;
    let changed = hooks_changed
        || matches!(statusline, StatusLineDecision::Ours)
        || new_text.trim() != original.trim();
    let outcome = if original.is_empty() {
        Outcome::Inserted
    } else if changed {
        Outcome::Replaced
    } else {
        Outcome::Unchanged
    };
    Ok((
        new_text,
        UpsertReport {
            outcome,
            statusline,
        },
    ))
}

/// Strip every hook + statusLine that we own. Leaves the file intact
/// if it was empty, so a later install can recreate cleanly.
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
    if let Some(Value::Object(sl)) = root_obj.get("statusLine") {
        if statusline_is_ours(sl) {
            root_obj.remove("statusLine");
            changed = true;
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
        serde_json::from_str(text).context("parsing claude settings.json")
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
    let Value::Object(hooks_obj) = hooks else {
        // Replace non-object hooks field — that's malformed.
        *hooks = Value::Object(Map::new());
        return upsert_hooks(root);
    };

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
        let want_cmd = format!("muxa hook claude --event {flag}");
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

fn upsert_statusline(root: &mut Map<String, Value>) -> StatusLineDecision {
    match root.get("statusLine") {
        Some(Value::Object(sl)) if statusline_is_ours(sl) => StatusLineDecision::AlreadyOurs,
        Some(_) => StatusLineDecision::SkippedUserOwned,
        None => {
            root.insert(
                "statusLine".to_string(),
                json!({
                    "type": "command",
                    "command": STATUSLINE_CMD,
                    "refreshInterval": 5,
                }),
            );
            StatusLineDecision::Ours
        }
    }
}

fn statusline_is_ours(sl: &Map<String, Value>) -> bool {
    sl.get("command")
        .and_then(Value::as_str)
        .is_some_and(|c| c.trim_start().starts_with(STATUSLINE_CMD))
}

/// True iff the entry is one of muxa's hook entries (any event).
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
    fn upsert_into_empty_file_creates_full_config() {
        let (out, report) = upsert("").unwrap();
        assert_eq!(report.outcome, Outcome::Inserted);
        assert_eq!(report.statusline, StatusLineDecision::Ours);
        let v: Value = serde_json::from_str(&out).unwrap();
        // Every hook event has at least one entry.
        for (event, _) in HOOK_EVENTS {
            assert!(
                v["hooks"][event].as_array().is_some_and(|a| !a.is_empty()),
                "missing event {event}"
            );
        }
        assert_eq!(v["statusLine"]["command"], STATUSLINE_CMD);
    }

    #[test]
    fn upsert_is_idempotent() {
        let (first, _) = upsert("").unwrap();
        let (second, report) = upsert(&first).unwrap();
        assert_eq!(report.outcome, Outcome::Unchanged);
        assert_eq!(first, second);
    }

    #[test]
    fn upsert_preserves_user_hooks() {
        let user_settings = r#"{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          { "type": "command", "command": "echo hi from the user" }
        ]
      }
    ]
  }
}"#;
        let (out, _) = upsert(user_settings).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        let arr = v["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(arr.len(), 2, "user entry + muxa entry");
        // User's entry survived.
        assert!(arr.iter().any(|e| e["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("echo hi")));
        // Ours is present.
        assert!(arr.iter().any(|e| e["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .starts_with("muxa hook claude")));
    }

    #[test]
    fn upsert_skips_user_owned_statusline() {
        let user = r#"{ "statusLine": { "type": "command", "command": "npx ccstatusline" } }"#;
        let (out, report) = upsert(user).unwrap();
        assert_eq!(report.statusline, StatusLineDecision::SkippedUserOwned);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["statusLine"]["command"], "npx ccstatusline");
    }

    #[test]
    fn remove_strips_only_our_entries() {
        let user_settings = r#"{
  "hooks": {
    "SessionStart": [
      { "hooks": [{ "type": "command", "command": "echo user" }] }
    ]
  }
}"#;
        let (with, _) = upsert(user_settings).unwrap();
        let (after, o) = remove(&with).unwrap();
        assert_eq!(o, Outcome::Removed);
        let v: Value = serde_json::from_str(&after).unwrap();
        let arr = v["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert!(arr[0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("echo user"));
        // Other events were ours-only — they should be pruned away.
        assert!(v["hooks"].get("PreToolUse").is_none());
        // Our statusLine is gone.
        assert!(v.get("statusLine").is_none());
    }

    #[test]
    fn remove_on_clean_file_is_noop() {
        let user = r#"{ "hooks": { "SessionStart": [{ "hooks": [{ "type":"command", "command":"echo user" }] }] } }"#;
        let (out, o) = remove(user).unwrap();
        assert_eq!(o, Outcome::AlreadyAbsent);
        // Roundtrip-equivalent JSON (whitespace may differ).
        let before: Value = serde_json::from_str(user).unwrap();
        let after: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn remove_does_not_drop_user_statusline() {
        let user = r#"{ "statusLine": { "type": "command", "command": "npx ccstatusline" } }"#;
        let (out, _) = remove(user).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["statusLine"]["command"], "npx ccstatusline");
    }
}
