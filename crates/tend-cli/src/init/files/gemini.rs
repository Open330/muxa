//! `~/.gemini/settings.json` cleanup for the retired Gemini CLI.
//!
//! tend no longer supports the Gemini CLI, but older installs wrote
//! `tend hook gemini --event …` entries into the `hooks` key of this file.
//! This module only ever removes those entries, and leaves everything else in
//! the file alone. The Antigravity CLI shares the `~/.gemini` tree but keeps
//! its hooks in `~/.gemini/config/hooks.json` (see [`super::antigravity`]),
//! which this module never touches.

use crate::init::marker::Outcome;
use anyhow::{Context, Result};
use serde_json::Value;

const HOOK_CMD_PREFIX: &str = "tend hook gemini";

pub fn default_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".gemini").join("settings.json"))
}

/// Does `settings_text` still carry a hook entry an older tend wrote?
pub fn has_tend_hooks(settings_text: &str) -> bool {
    let Ok(Value::Object(root)) = serde_json::from_str::<Value>(settings_text) else {
        return false;
    };
    let Some(Value::Object(hooks)) = root.get("hooks") else {
        return false;
    };
    hooks
        .values()
        .filter_map(Value::as_array)
        .flatten()
        .any(entry_owned_by_us)
}

/// Strip every tend-owned hook entry, dropping event arrays (and the `hooks`
/// key) that end up empty.
pub fn remove(original: &str) -> Result<(String, Outcome)> {
    if original.trim().is_empty() {
        return Ok((original.to_string(), Outcome::AlreadyAbsent));
    }
    let mut root: Value = serde_json::from_str(original).context("parsing gemini settings.json")?;
    let Some(root_obj) = root.as_object_mut() else {
        return Ok((original.to_string(), Outcome::AlreadyAbsent));
    };

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
                if arr.is_empty() {
                    hooks.remove(&ev);
                }
            }
        }
        if changed && hooks.is_empty() {
            root_obj.remove("hooks");
        }
    }
    if !changed {
        return Ok((original.to_string(), Outcome::AlreadyAbsent));
    }
    let mut new_text = serde_json::to_string_pretty(&root)?;
    new_text.push('\n');
    Ok((new_text, Outcome::Removed))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// What an older `tend init` left behind, beside a user's own hook.
    const LEFTOVER: &str = r#"{
  "theme": "dark",
  "hooks": {
    "SessionStart": [
      { "hooks": [{ "type": "command", "command": "tend hook gemini --event session_start" }] }
    ],
    "BeforeAgent": [
      { "hooks": [{ "type": "command", "command": "echo user" }] },
      { "hooks": [{ "type": "command", "command": "tend hook gemini --event before_agent" }] }
    ]
  }
}"#;

    #[test]
    fn remove_strips_only_our_entries() {
        assert!(has_tend_hooks(LEFTOVER));
        let (after, o) = remove(LEFTOVER).unwrap();
        assert_eq!(o, Outcome::Removed);
        assert!(!has_tend_hooks(&after));
        let v: Value = serde_json::from_str(&after).unwrap();
        assert_eq!(v["theme"], "dark");
        assert!(v["hooks"].get("SessionStart").is_none());
        let arr = v["hooks"]["BeforeAgent"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["hooks"][0]["command"], "echo user");
    }

    #[test]
    fn a_file_without_tend_hooks_is_left_byte_for_byte() {
        let user = "{\n  \"hooks\": {},\n  \"theme\": \"light\"\n}\n";
        assert!(!has_tend_hooks(user));
        let (after, o) = remove(user).unwrap();
        assert_eq!(o, Outcome::AlreadyAbsent);
        assert_eq!(after, user);
        assert_eq!(remove("").unwrap().1, Outcome::AlreadyAbsent);
        assert!(remove("{ not json").is_err());
        assert!(!has_tend_hooks("{ not json"));
    }

    #[test]
    fn only_tend_hooks_leaves_no_hooks_key() {
        let ours = r#"{"hooks":{"AfterAgent":[{"hooks":[{"type":"command","command":"tend hook gemini --event after_agent"}]}]}}"#;
        let (after, _) = remove(ours).unwrap();
        let v: Value = serde_json::from_str(&after).unwrap();
        assert!(v.get("hooks").is_none(), "{after}");
    }
}
