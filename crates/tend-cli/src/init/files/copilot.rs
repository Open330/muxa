//! `$COPILOT_HOME/hooks/tend.json` — GitHub Copilot CLI hooks.
//!
//! Copilot loads every `*.json` file under `$COPILOT_HOME/hooks/` (default
//! `~/.copilot/hooks/`) as user hooks, which load without the folder-trust
//! prompt repository hooks need. tend owns this one file outright: install
//! writes it, uninstall deletes it, and the user's other hook files are
//! never opened.
//!
//! The shape (checked on Copilot CLI 1.0.110): `{"version": 1, "hooks":
//! {EVENT: [{"type": "command", "bash": CMD, "timeoutSec": N}]}}`, with the
//! camelCase event names whose payloads [`tend::adapters::copilot`] reads.

use crate::init::marker::Outcome;
use serde_json::{json, Map, Value};
use std::path::PathBuf;

/// Per-handler timeout, in seconds. The hooks run inside Copilot's loop;
/// `tend hook copilot` returns within the IPC client's own bound, so this
/// fires only when something is badly wrong.
const TIMEOUT_SECS: u64 = 10;

/// `$COPILOT_HOME`, or `~/.copilot`.
pub fn home_dir() -> Option<PathBuf> {
    home_dir_from(
        std::env::var_os("COPILOT_HOME").map(PathBuf::from),
        dirs::home_dir(),
    )
}

fn home_dir_from(copilot_home: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    copilot_home
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| home.map(|home| home.join(".copilot")))
}

pub fn default_path() -> Option<PathBuf> {
    home_dir().map(|dir| dir.join("hooks").join("tend.json"))
}

/// The file tend writes.
fn body() -> String {
    let mut hooks = Map::new();
    for (name, flag) in tend::adapters::copilot::EVENTS {
        hooks.insert(
            (*name).to_string(),
            json!([{
                "type": "command",
                "bash": format!("tend hook copilot --event {flag}"),
                "powershell": format!("tend hook copilot --event {flag}"),
                "timeoutSec": TIMEOUT_SECS,
            }]),
        );
    }
    let mut text =
        serde_json::to_string_pretty(&json!({"version": 1, "hooks": Value::Object(hooks)}))
            .expect("static JSON serializes");
    text.push('\n');
    text
}

pub fn upsert(original: &str) -> (String, Outcome) {
    let want = body();
    let outcome = if original.trim().is_empty() {
        Outcome::Inserted
    } else if original == want {
        Outcome::Unchanged
    } else {
        Outcome::Replaced
    };
    (want, outcome)
}

/// Uninstall empties the file, which the planner turns into a delete.
pub fn remove(original: &str) -> (String, Outcome) {
    if original.trim().is_empty() {
        (String::new(), Outcome::Unchanged)
    } else {
        (String::new(), Outcome::Replaced)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_names_every_wired_event_in_copilots_shape() {
        let value: Value = serde_json::from_str(&body()).unwrap();
        assert_eq!(value["version"], 1);
        let hooks = value["hooks"].as_object().unwrap();
        assert_eq!(hooks.len(), tend::adapters::copilot::EVENTS.len());
        let start = &hooks["sessionStart"][0];
        assert_eq!(start["type"], "command");
        assert_eq!(start["bash"], "tend hook copilot --event session_start");
        assert_eq!(start["timeoutSec"], TIMEOUT_SECS);
        // `permissionRequest` is not a wait and is not wired.
        assert!(!hooks.contains_key("permissionRequest"));
        assert!(hooks
            .keys()
            .all(|k| k.chars().next().unwrap().is_lowercase()));
    }

    #[test]
    fn install_is_idempotent_and_uninstall_empties() {
        let (text, outcome) = upsert("");
        assert_eq!(outcome, Outcome::Inserted);
        assert_eq!(upsert(&text).1, Outcome::Unchanged);
        assert_eq!(upsert("{\"version\":1}").1, Outcome::Replaced);
        assert_eq!(remove(&text), (String::new(), Outcome::Replaced));
        assert_eq!(remove("").1, Outcome::Unchanged);
    }

    #[test]
    fn copilot_home_wins_over_the_home_default() {
        let home = Some(PathBuf::from("/h"));
        assert_eq!(
            home_dir_from(None, home.clone()),
            Some(PathBuf::from("/h/.copilot"))
        );
        assert_eq!(
            home_dir_from(Some(PathBuf::from("/c")), home.clone()),
            Some(PathBuf::from("/c"))
        );
        assert_eq!(
            home_dir_from(Some(PathBuf::new()), home),
            Some(PathBuf::from("/h/.copilot"))
        );
    }
}
