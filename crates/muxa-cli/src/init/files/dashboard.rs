//! `$XDG_CONFIG_HOME/muxa/config.toml` dashboard block.
//!
//! Adds a `[dashboard]` table with `enabled = true`, a freshly-
//! generated 32-byte hex token, and `bind = "127.0.0.1:7878"`. We use
//! `toml_edit` so the rest of the config (potentially user-managed) is
//! preserved verbatim.
//!
//! Token policy: if a token is already present we keep it (rotating
//! invalidates any browser sessions the user has open). Only on first
//! install do we generate one.

use crate::init::marker::Outcome;
use anyhow::{Context, Result};
use rand::RngCore;
use std::path::PathBuf;
use toml_edit::{value, DocumentMut, Item, Table};

pub fn default_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("muxa").join("config.toml"))
}

/// Ensure `[dashboard]` is present with `enabled = true` and a token.
/// Returns the new file text + the token we ended up with (so the
/// orchestrator can print the URL at the end of the run).
pub fn upsert(original: &str) -> Result<(String, Outcome, String)> {
    let mut doc = parse_or_empty(original)?;
    let (changed, token) = upsert_dashboard(&mut doc);
    let new_text = doc.to_string();
    let outcome = if original.is_empty() {
        Outcome::Inserted
    } else if changed || new_text != original {
        Outcome::Replaced
    } else {
        Outcome::Unchanged
    };
    Ok((new_text, outcome, token))
}

/// Remove the `[dashboard]` table entirely. We also drop a
/// best-effort scrub of the token to keep the file from leaking it
/// after uninstall.
pub fn remove(original: &str) -> Result<(String, Outcome)> {
    if original.trim().is_empty() {
        return Ok((original.to_string(), Outcome::AlreadyAbsent));
    }
    let mut doc = parse_or_empty(original)?;
    let had = doc.remove("dashboard").is_some();
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

/// Returns `(changed, token)`. If a token already exists we reuse it.
fn upsert_dashboard(doc: &mut DocumentMut) -> (bool, String) {
    let mut changed = false;

    if !doc.contains_table("dashboard") {
        doc["dashboard"] = Item::Table(Table::new());
        changed = true;
    }
    let tbl = doc["dashboard"]
        .as_table_mut()
        .expect("table by construction");

    if !matches!(tbl.get("enabled"), Some(Item::Value(v)) if v.as_bool() == Some(true)) {
        tbl["enabled"] = value(true);
        changed = true;
    }
    if tbl.get("bind").is_none() {
        tbl["bind"] = value("127.0.0.1:7878");
        changed = true;
    }

    let token = match tbl.get("token").and_then(Item::as_str) {
        Some(t) if !t.trim().is_empty() => t.to_string(),
        _ => {
            let t = generate_token();
            tbl["token"] = value(t.clone());
            changed = true;
            t
        }
    };
    (changed, token)
}

fn generate_token() -> String {
    let mut buf = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut buf);
    let mut s = String::with_capacity(buf.len() * 2);
    for b in buf {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_into_empty_writes_full_block() {
        let (out, o, token) = upsert("").unwrap();
        assert_eq!(o, Outcome::Inserted);
        assert_eq!(token.len(), 64); // 32 bytes hex-encoded
        let doc: DocumentMut = out.parse().unwrap();
        assert_eq!(doc["dashboard"]["enabled"].as_bool(), Some(true));
        assert_eq!(doc["dashboard"]["bind"].as_str(), Some("127.0.0.1:7878"));
        assert_eq!(doc["dashboard"]["token"].as_str().unwrap().len(), 64);
    }

    #[test]
    fn upsert_preserves_existing_token() {
        let (first, _, token1) = upsert("").unwrap();
        let (second, _, token2) = upsert(&first).unwrap();
        assert_eq!(token1, token2);
        // No content change beyond whitespace tolerance.
        let d1: DocumentMut = first.parse().unwrap();
        let d2: DocumentMut = second.parse().unwrap();
        assert_eq!(d1.to_string(), d2.to_string());
    }

    #[test]
    fn upsert_keeps_user_sections_intact() {
        let user = r#"# user comment
[history]
enabled = false

[notifier]
backend = "libnotify"
"#;
        let (out, _, _) = upsert(user).unwrap();
        assert!(out.contains("# user comment"));
        assert!(out.contains("[history]"));
        assert!(out.contains("backend = \"libnotify\""));
        assert!(out.contains("[dashboard]"));
    }

    #[test]
    fn remove_drops_dashboard_only() {
        let user = r"[history]
enabled = false
";
        let (with, _, _) = upsert(user).unwrap();
        let (after, o) = remove(&with).unwrap();
        assert_eq!(o, Outcome::Removed);
        assert!(after.contains("[history]"));
        assert!(!after.contains("[dashboard]"));
    }

    #[test]
    fn token_is_random_per_install() {
        let (_, _, t1) = upsert("").unwrap();
        let (_, _, t2) = upsert("").unwrap();
        assert_ne!(
            t1, t2,
            "RNG should produce distinct tokens across invocations"
        );
    }
}
