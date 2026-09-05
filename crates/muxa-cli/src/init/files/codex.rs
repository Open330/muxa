//! `~/.codex/config.toml` content layer.
//!
//! Codex's hook engine is a verbatim port of Claude Code's, so the
//! semantic shape is identical to `claude.rs` — different file format.
//! We use `toml_edit` to round-trip user comments and existing
//! formatting verbatim, only mutating the `hooks.<Event>` arrays.
//!
//! Returns `(new_text, Outcome)` so the caller (apply.rs) handles I/O,
//! backups, and diffing.

use crate::init::marker::Outcome;
use anyhow::{Context, Result};
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, Value};

const HOOK_EVENTS: &[(&str, &str)] = &[
    ("SessionStart", "session_start"),
    ("UserPromptSubmit", "user_prompt_submit"),
    ("PreToolUse", "pre_tool_use"),
    ("PostToolUse", "post_tool_use"),
    ("PermissionRequest", "permission_request"),
    ("Stop", "stop"),
];

const HOOK_CMD_PREFIX: &str = "muxa hook codex";

pub fn default_path() -> Option<std::path::PathBuf> {
    crate::init::integration::codex_home().map(|h| h.join("config.toml"))
}

pub fn upsert(original: &str) -> Result<(String, Outcome)> {
    let mut doc = parse_or_empty(original)?;
    let changed = upsert_hooks(&mut doc);
    let new_text = doc.to_string();

    let outcome = if original.is_empty() {
        Outcome::Inserted
    } else if changed || new_text != original {
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
    let mut doc = parse_or_empty(original)?;
    let changed = strip_our_hooks(&mut doc);
    Ok((
        doc.to_string(),
        if changed {
            Outcome::Removed
        } else {
            Outcome::AlreadyAbsent
        },
    ))
}

fn parse_or_empty(text: &str) -> Result<DocumentMut> {
    if text.trim().is_empty() {
        Ok(DocumentMut::new())
    } else {
        text.parse::<DocumentMut>()
            .context("parsing codex config.toml")
    }
}

fn upsert_hooks(doc: &mut DocumentMut) -> bool {
    let mut changed = false;

    // Make sure `hooks` is a table.
    if !doc.contains_table("hooks") {
        doc["hooks"] = Item::Table(Table::new());
        changed = true;
    }
    let hooks_tbl = doc["hooks"]
        .as_table_mut()
        .expect("hooks is a table by construction");
    hooks_tbl.set_implicit(true); // render as `[[hooks.X]]`, not `[hooks]\n[[hooks.X]]`

    for (event, flag) in HOOK_EVENTS {
        let want_cmd = format!("muxa hook codex --event {flag}");
        // hooks.<Event> is an array of tables — each table has a
        // nested `hooks = [{ type, command }, ...]`.
        if !hooks_tbl.contains_array_of_tables(event) {
            hooks_tbl.insert(event, Item::ArrayOfTables(ArrayOfTables::new()));
            changed = true;
        }
        let outer = hooks_tbl
            .get_mut(event)
            .and_then(Item::as_array_of_tables_mut)
            .expect("array of tables by construction");

        if outer.iter().any(|t| outer_table_has_command(t, &want_cmd)) {
            continue;
        }

        let mut entry = Table::new();
        let mut inner_arr = Array::new();
        let mut inner = toml_edit::InlineTable::new();
        inner.insert("type", Value::from("command"));
        inner.insert("command", Value::from(want_cmd));
        inner_arr.push(Value::InlineTable(inner));
        entry.insert("hooks", Item::Value(Value::Array(inner_arr)));
        outer.push(entry);
        changed = true;
    }
    changed
}

fn strip_our_hooks(doc: &mut DocumentMut) -> bool {
    let Some(hooks_tbl) = doc.get_mut("hooks").and_then(Item::as_table_mut) else {
        return false;
    };

    let mut changed = false;
    let event_keys: Vec<String> = hooks_tbl.iter().map(|(k, _)| k.to_string()).collect();
    for ev in event_keys {
        if let Some(outer) = hooks_tbl
            .get_mut(&ev)
            .and_then(Item::as_array_of_tables_mut)
        {
            let before = outer.len();
            outer.retain(|t| !outer_table_owned_by_us(t));
            if outer.len() != before {
                changed = true;
            }
            if outer.is_empty() {
                hooks_tbl.remove(&ev);
                changed = true;
            }
        }
    }
    if hooks_tbl.is_empty() {
        doc.remove("hooks");
    }
    changed
}

/// Does this outer-array entry contain an inner hook with the exact
/// expected command string?
fn outer_table_has_command(t: &Table, want_cmd: &str) -> bool {
    inner_hooks_iter(t).any(|cmd| cmd.trim() == want_cmd)
}

/// Does this outer-array entry contain *any* inner hook owned by us?
fn outer_table_owned_by_us(t: &Table) -> bool {
    inner_hooks_iter(t).any(|cmd| cmd.trim_start().starts_with(HOOK_CMD_PREFIX))
}

/// Iterate every `command` string inside a single outer table's
/// `hooks` array, regardless of whether the entries are inline tables
/// or normal tables.
fn inner_hooks_iter(t: &Table) -> impl Iterator<Item = &str> {
    let inner = t.get("hooks");
    inner
        .into_iter()
        .flat_map(|item| match item {
            Item::Value(Value::Array(arr)) => arr
                .iter()
                .filter_map(|v| match v {
                    Value::InlineTable(it) => it.get("command").and_then(Value::as_str),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            Item::ArrayOfTables(aot) => aot
                .iter()
                .filter_map(|inner| inner.get("command").and_then(Item::as_str))
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect::<Vec<_>>()
        .into_iter()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_into_empty_emits_all_events() {
        let (out, o) = upsert("").unwrap();
        assert_eq!(o, Outcome::Inserted);
        let doc: DocumentMut = out.parse().unwrap();
        for (event, flag) in HOOK_EVENTS {
            let aot = doc["hooks"][event].as_array_of_tables().unwrap();
            assert_eq!(aot.len(), 1, "expected one entry for {event}");
            let cmd = inner_hooks_iter(aot.get(0).unwrap())
                .next()
                .expect("command string");
            assert!(
                cmd.contains(&format!("--event {flag}")),
                "{cmd} should mention {flag}"
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
    fn upsert_preserves_user_table() {
        let user = r#"# my notes
[model]
name = "gpt-x"

[[hooks.SessionStart]]
  [[hooks.SessionStart.hooks]]
  type = "command"
  command = "echo user"
"#;
        let (out, _) = upsert(user).unwrap();
        assert!(out.contains("# my notes"));
        assert!(out.contains("echo user"));
        assert!(out.contains(r#"name = "gpt-x""#));
        assert!(out.contains("muxa hook codex --event session_start"));
    }

    #[test]
    fn remove_drops_only_our_entries() {
        let user = r#"
[[hooks.SessionStart]]
  [[hooks.SessionStart.hooks]]
  type = "command"
  command = "echo user"
"#;
        let (with_ours, _) = upsert(user).unwrap();
        let (after, o) = remove(&with_ours).unwrap();
        assert_eq!(o, Outcome::Removed);
        let doc: DocumentMut = after.parse().unwrap();
        let aot = doc["hooks"]["SessionStart"].as_array_of_tables().unwrap();
        assert_eq!(aot.len(), 1);
        let cmd = inner_hooks_iter(aot.get(0).unwrap()).next().unwrap();
        assert_eq!(cmd, "echo user");
        // Other events were ours-only — pruned.
        assert!(doc["hooks"].get("PreToolUse").is_none());
    }

    #[test]
    fn remove_on_clean_file_is_noop() {
        let user = r#"[model]
name = "gpt"
"#;
        let (out, o) = remove(user).unwrap();
        assert_eq!(o, Outcome::AlreadyAbsent);
        // toml_edit may add a trailing newline but content equivalent.
        assert!(out.contains(r#"name = "gpt""#));
    }
}
