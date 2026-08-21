//! `~/.gemini/config/hooks.json` content layer — Antigravity CLI (`agy`).
//!
//! agy shares the `~/.gemini` tree with the CLI it replaced but reads none of
//! the same files: hooks live in their own `hooks.json` under a *customization
//! root*, not in the `hooks` key of `settings.json`. muxa writes the global
//! root (`~/.gemini/config/hooks.json`) — the same file agy's own `/hooks`
//! command manages, and the one shared with the Antigravity backend.
//!
//! ## Why this is simpler than [`super::gemini`]
//!
//! agy's `hooks.json` is keyed by **hook name**, so muxa owns exactly one
//! top-level key (`muxa`) instead of merging entries into per-event arrays.
//! Install is "set our key", uninstall is "drop our key", and a user's own
//! hooks — hand-written, plugin-installed, or added through `/hooks` — are
//! untouched by construction.
//!
//! ## Shape
//!
//! `PreToolUse`/`PostToolUse` take the grouped `{matcher, hooks[]}` form;
//! `SessionStart`, `PreInvocation`, `PostInvocation` and `Stop` take a flat
//! list of handlers. Getting this wrong is silent — agy logs
//! `loaded 0 named hooks` and carries on — so [`tests`] pins both shapes.

use crate::init::marker::Outcome;
use anyhow::{Context, Result};
use serde_json::{json, Map, Value};

/// Our top-level key in `hooks.json`. Uninstall keys off this exact string,
/// so renaming it would orphan every previously written block.
const HOOK_NAME: &str = "muxa";

/// Events taking the flat handler-list form, paired with the `--event` flag
/// [`muxa::adapters::antigravity`] parses.
const FLAT_EVENTS: &[(&str, &str)] = &[
    ("SessionStart", "session_start"),
    ("PreInvocation", "pre_invocation"),
    ("PostInvocation", "post_invocation"),
    ("Stop", "stop"),
];

/// Events taking the grouped `{matcher, hooks[]}` form. `*` matches every
/// tool — muxa observes them all and gates none.
const TOOL_EVENTS: &[(&str, &str)] = &[
    ("PreToolUse", "pre_tool_use"),
    ("PostToolUse", "post_tool_use"),
];

/// Per-handler timeout, in seconds.
///
/// Well above the 750 ms bound `Client::ingest` already applies, so it can
/// only fire if something is pathologically wrong — at which point a bounded
/// stall beats agy's 30 s default, because these hooks run synchronously
/// inside the agent's loop.
const TIMEOUT_SECS: u64 = 10;

pub fn default_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".gemini").join("config").join("hooks.json"))
}

pub fn upsert(original: &str) -> Result<(String, Outcome)> {
    let mut root = parse_or_empty(original)?;
    let obj = ensure_object(&mut root);

    let want = muxa_hook_spec();
    let changed = obj.get(HOOK_NAME) != Some(&want);
    obj.insert(HOOK_NAME.to_string(), want);

    let new_text = pretty_print(&root)?;
    let outcome = if original.trim().is_empty() {
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
    let obj = ensure_object(&mut root);
    if obj.remove(HOOK_NAME).is_none() {
        return Ok((original.to_string(), Outcome::AlreadyAbsent));
    }
    Ok((pretty_print(&root)?, Outcome::Removed))
}

/// The full `muxa` hook spec, exactly as agy expects to read it back.
fn muxa_hook_spec() -> Value {
    let mut spec = Map::new();
    for (event, flag) in FLAT_EVENTS {
        spec.insert((*event).to_string(), json!([handler(flag)]));
    }
    for (event, flag) in TOOL_EVENTS {
        spec.insert(
            (*event).to_string(),
            json!([{ "matcher": "*", "hooks": [handler(flag)] }]),
        );
    }
    Value::Object(spec)
}

fn handler(flag: &str) -> Value {
    json!({
        "type": "command",
        "command": format!("muxa hook agy --event {flag}"),
        "timeout": TIMEOUT_SECS,
    })
}

fn parse_or_empty(text: &str) -> Result<Value> {
    if text.trim().is_empty() {
        Ok(Value::Object(Map::new()))
    } else {
        serde_json::from_str(text).context("parsing agy hooks.json")
    }
}

fn ensure_object(v: &mut Value) -> &mut Map<String, Value> {
    if !v.is_object() {
        *v = Value::Object(Map::new());
    }
    v.as_object_mut().expect("just ensured object")
}

fn pretty_print(v: &Value) -> Result<String> {
    let mut s = serde_json::to_string_pretty(v)?;
    s.push('\n');
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use muxa::adapters::{antigravity::AntigravityAdapter, HookAdapter};

    fn parse(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn upsert_into_empty_writes_every_event() {
        let (out, o) = upsert("").unwrap();
        assert_eq!(o, Outcome::Inserted);
        let v = parse(&out);
        for (event, _) in FLAT_EVENTS.iter().chain(TOOL_EVENTS) {
            assert!(
                v[HOOK_NAME][event]
                    .as_array()
                    .is_some_and(|a| !a.is_empty()),
                "missing {event}",
            );
        }
    }

    /// agy parses the two families differently: a `PreToolUse` written flat
    /// (or a `Stop` written grouped) is dropped without an error message.
    #[test]
    fn tool_events_are_grouped_and_lifecycle_events_are_flat() {
        let v = parse(&upsert("").unwrap().0);

        let stop = &v[HOOK_NAME]["Stop"][0];
        assert_eq!(stop["type"], "command");
        assert!(stop.get("matcher").is_none(), "Stop must not be grouped");

        let pre_tool = &v[HOOK_NAME]["PreToolUse"][0];
        assert_eq!(pre_tool["matcher"], "*");
        assert_eq!(pre_tool["hooks"][0]["type"], "command");
    }

    /// Every `--event` flag written here must be one the adapter parses; a
    /// mismatch surfaces only as a silently ignored hook inside agy.
    #[test]
    fn every_written_flag_is_understood_by_the_adapter() {
        let v = parse(&upsert("").unwrap().0);
        let mut checked = 0;

        for (event, _) in FLAT_EVENTS {
            let cmd = v[HOOK_NAME][event][0]["command"].as_str().unwrap();
            assert!(
                AntigravityAdapter::parse_event(flag_of(cmd)).is_ok(),
                "{cmd}"
            );
            checked += 1;
        }
        for (event, _) in TOOL_EVENTS {
            let cmd = v[HOOK_NAME][event][0]["hooks"][0]["command"]
                .as_str()
                .unwrap();
            assert!(
                AntigravityAdapter::parse_event(flag_of(cmd)).is_ok(),
                "{cmd}"
            );
            checked += 1;
        }
        assert_eq!(checked, FLAT_EVENTS.len() + TOOL_EVENTS.len());
    }

    fn flag_of(command: &str) -> &str {
        command.rsplit("--event ").next().unwrap().trim()
    }

    #[test]
    fn upsert_is_idempotent() {
        let (first, _) = upsert("").unwrap();
        let (second, o) = upsert(&first).unwrap();
        assert_eq!(o, Outcome::Unchanged);
        assert_eq!(first, second);
    }

    /// The file is shared with agy's own `/hooks` command, plugins, and
    /// anything the user wrote by hand. Owning one named key means none of
    /// that can be clobbered.
    #[test]
    fn upsert_preserves_other_named_hooks() {
        let user = r#"{
  "lint-checker": {
    "PostToolUse": [
      { "matcher": "run_command", "hooks": [{ "type": "command", "command": "./lint.sh" }] }
    ]
  }
}"#;
        let v = parse(&upsert(user).unwrap().0);
        assert_eq!(
            v["lint-checker"]["PostToolUse"][0]["hooks"][0]["command"],
            "./lint.sh",
        );
        assert!(v[HOOK_NAME].is_object());
    }

    /// A stale block from an older muxa is rewritten to the current spec
    /// rather than left half-configured.
    #[test]
    fn upsert_replaces_an_outdated_block() {
        let stale =
            r#"{"muxa":{"Stop":[{"type":"command","command":"muxa hook agy --event stop"}]}}"#;
        let (out, o) = upsert(stale).unwrap();
        assert_eq!(o, Outcome::Replaced);
        let v = parse(&out);
        assert!(v[HOOK_NAME]["PreToolUse"].is_array());
        assert_eq!(v[HOOK_NAME]["Stop"][0]["timeout"], TIMEOUT_SECS);
    }

    #[test]
    fn remove_strips_only_our_key() {
        let user = r#"{"lint-checker":{"PostToolUse":[]}}"#;
        let (with, _) = upsert(user).unwrap();
        let (after, o) = remove(&with).unwrap();
        assert_eq!(o, Outcome::Removed);
        let v = parse(&after);
        assert!(v.get(HOOK_NAME).is_none());
        assert!(v.get("lint-checker").is_some());
    }

    /// With nothing but our key, removal leaves an inert `{}` rather than
    /// deleting the file. `push_edit_or_delete`'s delete demotion keys on a
    /// *blank* remainder, and `{}` is not blank — deliberately, because agy's
    /// own `/hooks` command owns this path too and may have created it. An
    /// empty object is a valid hooks.json that loads zero hooks.
    #[test]
    fn remove_from_a_muxa_only_file_leaves_an_empty_object() {
        let (with, _) = upsert("").unwrap();
        let (after, o) = remove(&with).unwrap();
        assert_eq!(o, Outcome::Removed);
        assert_eq!(parse(&after), json!({}));
    }

    #[test]
    fn remove_is_idempotent_and_reports_absence() {
        assert_eq!(remove("").unwrap().1, Outcome::AlreadyAbsent);
        assert_eq!(remove(r#"{"other":{}}"#).unwrap().1, Outcome::AlreadyAbsent,);
    }

    #[test]
    fn malformed_json_is_an_error_not_a_silent_overwrite() {
        assert!(upsert("{ not json").is_err());
        assert!(remove("{ not json").is_err());
    }
}
