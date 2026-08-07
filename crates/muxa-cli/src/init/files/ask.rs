//! `$XDG_CONFIG_HOME/muxa/config.toml` ask block.
//!
//! Adds an `[ask]` table with `enabled = true` and an agent. Like the
//! collaboration editor it goes through `toml_edit`, so the rest of a
//! hand-maintained config survives verbatim.
//!
//! Why a component rather than a compiled default: enabling ask lets the
//! daemon spawn an agent CLI that bills the user's account. That is a
//! grant, and it belongs at the moment the user is already deciding what
//! muxa may touch.
//!
//! An existing `agent` is left alone — someone who set `codex` did so on
//! purpose, and re-running `muxa init` must not quietly point their
//! questions somewhere else.

use crate::init::marker::Outcome;
use anyhow::{Context, Result};
use std::path::PathBuf;
use toml_edit::{value, DocumentMut, Item, Table};

pub fn default_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("muxa").join("config.toml"))
}

/// Ensure `[ask]` is present and enabled. Returns the new file text plus
/// what changed.
pub fn upsert(original: &str) -> Result<(String, Outcome)> {
    let mut doc = parse_or_empty(original)?;
    let changed = upsert_ask(&mut doc);
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

/// Remove the `[ask]` table. History lives in `ask.json`, so revoking the
/// grant does not destroy past answers.
pub fn remove(original: &str) -> Result<(String, Outcome)> {
    if original.trim().is_empty() {
        return Ok((original.to_string(), Outcome::AlreadyAbsent));
    }
    let mut doc = parse_or_empty(original)?;
    let had = doc.remove("ask").is_some();
    Ok((
        doc.to_string(),
        if had {
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
        text.parse::<DocumentMut>().context("parsing config.toml")
    }
}

fn upsert_ask(doc: &mut DocumentMut) -> bool {
    let mut changed = false;

    if !doc.contains_table("ask") {
        doc["ask"] = Item::Table(Table::new());
        changed = true;
    }
    let tbl = doc["ask"].as_table_mut().expect("table by construction");

    if !matches!(tbl.get("enabled"), Some(Item::Value(v)) if v.as_bool() == Some(true)) {
        tbl["enabled"] = value(true);
        changed = true;
    }
    // Absent only: a configured `codex` is a deliberate choice.
    if tbl.get("agent").is_none() {
        tbl["agent"] = value("claude");
        changed = true;
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_into_empty_writes_the_block() {
        let (out, o) = upsert("").unwrap();
        assert_eq!(o, Outcome::Inserted);
        let doc: DocumentMut = out.parse().unwrap();
        assert_eq!(doc["ask"]["enabled"].as_bool(), Some(true));
        assert_eq!(doc["ask"]["agent"].as_str(), Some("claude"));
    }

    #[test]
    fn upsert_is_idempotent() {
        let (first, _) = upsert("").unwrap();
        let (second, outcome) = upsert(&first).unwrap();
        assert_eq!(outcome, Outcome::Unchanged);
        assert_eq!(first, second);
    }

    #[test]
    fn upsert_preserves_a_chosen_agent() {
        let user = "[ask]\nenabled = false\nagent = \"codex\"\n";
        let (out, _) = upsert(user).unwrap();
        let doc: DocumentMut = out.parse().unwrap();
        // The grant is what init is for; who answers is not.
        assert_eq!(doc["ask"]["enabled"].as_bool(), Some(true));
        assert_eq!(doc["ask"]["agent"].as_str(), Some("codex"));
    }

    #[test]
    fn upsert_keeps_user_sections_intact() {
        let user = "# mine\n[watch]\ntheme = \"classic\"\n";
        let (out, _) = upsert(user).unwrap();
        assert!(out.contains("# mine"));
        let doc: DocumentMut = out.parse().unwrap();
        assert_eq!(doc["watch"]["theme"].as_str(), Some("classic"));
        assert_eq!(doc["ask"]["enabled"].as_bool(), Some(true));
    }

    #[test]
    fn remove_drops_the_table_and_leaves_the_rest() {
        let (installed, _) = upsert("[watch]\ntheme = \"classic\"\n").unwrap();
        let (out, outcome) = remove(&installed).unwrap();
        assert_eq!(outcome, Outcome::Removed);
        let doc: DocumentMut = out.parse().unwrap();
        assert!(doc.get("ask").is_none());
        assert_eq!(doc["watch"]["theme"].as_str(), Some("classic"));
    }

    #[test]
    fn remove_on_absent_block_is_a_no_op() {
        let (_, outcome) = remove("[watch]\ntheme = \"classic\"\n").unwrap();
        assert_eq!(outcome, Outcome::AlreadyAbsent);
    }
}
