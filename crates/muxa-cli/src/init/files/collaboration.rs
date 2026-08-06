//! `$XDG_CONFIG_HOME/muxa/config.toml` collaboration block.
//!
//! Adds a `[collaboration]` table with `enabled = true` and
//! `wake = "idle_only"`. Like the dashboard editor we go through
//! `toml_edit` so the rest of the config — which the user may hand-
//! maintain — survives verbatim.
//!
//! Why this is a component and not a compiled default: enabling
//! collaboration lets a peer agent's request wake this pane by typing a
//! short notification prompt into it. That is a grant, so it belongs at
//! the moment the user is already choosing what muxa may touch, not in
//! `Default for CollaborationConfig`.
//!
//! Wake policy: an existing `wake` is left alone. A user who has
//! deliberately set `wake = "never"` wants the mailbox without pane
//! injection, and re-running `muxa init` must not quietly re-arm it.

use crate::init::marker::Outcome;
use anyhow::{Context, Result};
use std::path::PathBuf;
use toml_edit::{value, DocumentMut, Item, Table};

pub fn default_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("muxa").join("config.toml"))
}

/// Ensure `[collaboration]` is present with `enabled = true` and a wake
/// policy. Returns the new file text plus what changed.
pub fn upsert(original: &str) -> Result<(String, Outcome)> {
    let mut doc = parse_or_empty(original)?;
    let changed = upsert_collaboration(&mut doc);
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

/// Remove the `[collaboration]` table entirely. Mailbox contents live in
/// `collaboration.json`, not here, so this only revokes the grant.
pub fn remove(original: &str) -> Result<(String, Outcome)> {
    if original.trim().is_empty() {
        return Ok((original.to_string(), Outcome::AlreadyAbsent));
    }
    let mut doc = parse_or_empty(original)?;
    let had = doc.remove("collaboration").is_some();
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

fn upsert_collaboration(doc: &mut DocumentMut) -> bool {
    let mut changed = false;

    if !doc.contains_table("collaboration") {
        doc["collaboration"] = Item::Table(Table::new());
        changed = true;
    }
    let tbl = doc["collaboration"]
        .as_table_mut()
        .expect("table by construction");

    if !matches!(tbl.get("enabled"), Some(Item::Value(v)) if v.as_bool() == Some(true)) {
        tbl["enabled"] = value(true);
        changed = true;
    }
    // Absent only. `never` is a deliberate "mailbox, but keep out of my
    // panes" and re-running init must not overwrite it.
    if tbl.get("wake").is_none() {
        tbl["wake"] = value("idle_only");
        changed = true;
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_into_empty_writes_full_block() {
        let (out, o) = upsert("").unwrap();
        assert_eq!(o, Outcome::Inserted);
        let doc: DocumentMut = out.parse().unwrap();
        assert_eq!(doc["collaboration"]["enabled"].as_bool(), Some(true));
        assert_eq!(doc["collaboration"]["wake"].as_str(), Some("idle_only"));
    }

    #[test]
    fn upsert_is_idempotent() {
        let (first, _) = upsert("").unwrap();
        let (second, outcome) = upsert(&first).unwrap();
        assert_eq!(outcome, Outcome::Unchanged);
        assert_eq!(first, second);
    }

    #[test]
    fn upsert_preserves_a_deliberate_never_wake() {
        let user = "[collaboration]\nenabled = false\nwake = \"never\"\n";
        let (out, _) = upsert(user).unwrap();
        let doc: DocumentMut = out.parse().unwrap();
        // The grant is what init is for; the injection policy is not.
        assert_eq!(doc["collaboration"]["enabled"].as_bool(), Some(true));
        assert_eq!(doc["collaboration"]["wake"].as_str(), Some("never"));
    }

    #[test]
    fn upsert_keeps_user_sections_intact() {
        let user = "# user comment\n[watch]\ntheme = \"classic\"\n";
        let (out, _) = upsert(user).unwrap();
        assert!(out.contains("# user comment"));
        let doc: DocumentMut = out.parse().unwrap();
        assert_eq!(doc["watch"]["theme"].as_str(), Some("classic"));
        assert_eq!(doc["collaboration"]["enabled"].as_bool(), Some(true));
    }

    #[test]
    fn remove_drops_the_table_and_leaves_the_rest() {
        let (installed, _) = upsert("[watch]\ntheme = \"classic\"\n").unwrap();
        let (out, outcome) = remove(&installed).unwrap();
        assert_eq!(outcome, Outcome::Removed);
        let doc: DocumentMut = out.parse().unwrap();
        assert!(doc.get("collaboration").is_none());
        assert_eq!(doc["watch"]["theme"].as_str(), Some("classic"));
    }

    #[test]
    fn remove_on_absent_block_is_a_no_op() {
        let (_, outcome) = remove("[watch]\ntheme = \"classic\"\n").unwrap();
        assert_eq!(outcome, Outcome::AlreadyAbsent);
    }

    #[test]
    fn remove_on_empty_text_is_a_no_op() {
        let (out, outcome) = remove("").unwrap();
        assert_eq!(outcome, Outcome::AlreadyAbsent);
        assert_eq!(out, "");
    }
}
