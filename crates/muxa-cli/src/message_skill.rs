//! Reusable templates for the interactive message composers.
//!
//! Skills are deliberately plain text. Selecting one inserts it into the
//! current draft but never submits it, so the operator can inspect or edit the
//! expanded prompt before it reaches another agent.

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use muxa::config::MessageConfig;
use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::Path;

#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    action: SkillCommand,
}

#[derive(Debug, Subcommand)]
enum SkillCommand {
    /// Add a skill, or replace an existing skill with the same name.
    Add {
        /// Name typed after `/` in the message composer.
        name: String,
        /// Text inserted into the composer. Use `-` to read it from stdin.
        prompt: String,
    },
    /// List registered skills.
    List,
    /// Print the full prompt for one skill.
    Show { name: String },
    /// Remove one registered skill.
    Remove { name: String },
}

pub(crate) fn run(args: Args, config: &MessageConfig, config_path: Option<&Path>) -> Result<()> {
    match args.action {
        SkillCommand::List => {
            list(&config.skills);
            Ok(())
        }
        SkillCommand::Show { name } => show(&config.skills, &name),
        SkillCommand::Add { name, prompt } => {
            let path = config_path.context("no config directory is available on this system")?;
            let prompt = prompt_from_arg(prompt)?;
            validate_name(&name)?;
            if prompt.trim().is_empty() {
                bail!("skill prompt cannot be empty");
            }
            let existed = config.skills.contains_key(&name);
            upsert(path, &name, &prompt)?;
            println!(
                "{} /{} in {}",
                if existed { "updated" } else { "added" },
                name,
                path.display()
            );
            Ok(())
        }
        SkillCommand::Remove { name } => {
            let path = config_path.context("no config directory is available on this system")?;
            if !config.skills.contains_key(&name) {
                bail!("message skill /{name} is not registered");
            }
            remove(path, &name)?;
            println!("removed /{} from {}", name, path.display());
            Ok(())
        }
    }
}

fn prompt_from_arg(prompt: String) -> Result<String> {
    if prompt != "-" {
        return Ok(prompt);
    }
    let mut text = String::new();
    io::stdin()
        .read_to_string(&mut text)
        .context("reading skill prompt from stdin")?;
    Ok(text.trim_end_matches(['\r', '\n']).to_string())
}

pub(crate) fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("skill name cannot be empty");
    }
    if name.chars().count() > 64 {
        bail!("skill name must be at most 64 characters");
    }
    if name
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == '/')
    {
        bail!("skill name cannot contain whitespace, control characters, or '/'");
    }
    Ok(())
}

fn list(skills: &BTreeMap<String, String>) {
    if skills.is_empty() {
        println!("no message skills registered");
        println!("add one with: muxa skill add <name> <prompt>");
        return;
    }
    for (name, prompt) in skills {
        let summary = prompt.lines().next().unwrap_or_default();
        println!("/{name}\t{}", truncate_chars(summary, 88));
    }
}

fn show(skills: &BTreeMap<String, String>, name: &str) -> Result<()> {
    let prompt = skills
        .get(name)
        .with_context(|| format!("message skill /{name} is not registered"))?;
    println!("{prompt}");
    Ok(())
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out = text.chars().take(max.saturating_sub(1)).collect::<String>();
    out.push('…');
    out
}

fn load_document(path: &Path) -> Result<toml_edit::DocumentMut> {
    let original = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    if original.trim().is_empty() {
        Ok(toml_edit::DocumentMut::new())
    } else {
        original
            .parse::<toml_edit::DocumentMut>()
            .with_context(|| format!("parsing {}", path.display()))
    }
}

fn skills_table_mut(doc: &mut toml_edit::DocumentMut) -> Result<&mut toml_edit::Table> {
    match doc.get("message") {
        Some(toml_edit::Item::Table(_)) | None => {}
        Some(_) => bail!("[message] is not a table"),
    }
    if doc.get("message").is_none() {
        doc["message"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let message = doc["message"]
        .as_table_mut()
        .context("[message] is not a table")?;
    match message.get("skills") {
        Some(toml_edit::Item::Table(_)) | None => {}
        Some(_) => bail!("[message.skills] is not a table"),
    }
    if message.get("skills").is_none() {
        message["skills"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    message["skills"]
        .as_table_mut()
        .context("[message.skills] is not a table")
}

fn write_document(path: &Path, doc: &toml_edit::DocumentMut) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, doc.to_string()).with_context(|| format!("writing {}", path.display()))
}

pub(crate) fn upsert(path: &Path, name: &str, prompt: &str) -> Result<()> {
    validate_name(name)?;
    if prompt.trim().is_empty() {
        bail!("skill prompt cannot be empty");
    }
    let mut doc = load_document(path)?;
    skills_table_mut(&mut doc)?.insert(name, toml_edit::value(prompt));
    write_document(path, &doc)
}

pub(crate) fn remove(path: &Path, name: &str) -> Result<()> {
    let mut doc = load_document(path)?;
    let removed = skills_table_mut(&mut doc)?.remove(name);
    if removed.is_none() {
        bail!("message skill /{name} is not registered");
    }
    write_document(path, &doc)
}

/// UI state shared by the watch and dashboard message composers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Palette {
    pub query: String,
    pub selected: usize,
}

impl Palette {
    pub fn insert(&mut self, c: char) {
        self.query.push(c);
        self.selected = 0;
    }

    pub fn backspace(&mut self) -> bool {
        let removed = self.query.pop().is_some();
        self.selected = 0;
        removed
    }

    pub fn move_selection(&mut self, delta: isize, skills: &BTreeMap<String, String>) {
        let len = matching_skills(skills, &self.query).len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(len.saturating_sub(1));
    }

    pub fn selected_prompt(&self, skills: &BTreeMap<String, String>) -> Option<String> {
        matching_skills(skills, &self.query)
            .get(self.selected)
            .map(|(_, prompt)| (*prompt).clone())
    }
}

pub(crate) fn matching_skills<'a>(
    skills: &'a BTreeMap<String, String>,
    query: &str,
) -> Vec<(&'a String, &'a String)> {
    let query = query.to_lowercase();
    skills
        .iter()
        .filter(|(name, prompt)| {
            query.is_empty()
                || name.to_lowercase().contains(&query)
                || prompt.to_lowercase().contains(&query)
        })
        .collect()
}

/// Insert a whole-prompt template at a character cursor without discarding the
/// surrounding draft. Skills are paragraph-sized rather than word completion,
/// so non-whitespace neighbours receive a blank-line boundary. Existing
/// whitespace is respected and an empty composer remains the simple
/// replacement case.
pub(crate) fn insert_prompt(input: &mut String, cursor: &mut usize, prompt: &str) {
    *cursor = (*cursor).min(input.chars().count());
    let byte_cursor = input
        .char_indices()
        .nth(*cursor)
        .map_or(input.len(), |(index, _)| index);
    let before = &input[..byte_cursor];
    let after = &input[byte_cursor..];
    let prefix = if before.trim().is_empty() || before.ends_with("\n\n") {
        ""
    } else if before.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    let suffix = if after.trim().is_empty() || after.starts_with("\n\n") {
        ""
    } else if after.starts_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    let inserted = format!("{prefix}{prompt}{suffix}");
    input.insert_str(byte_cursor, &inserted);
    *cursor += inserted.chars().count();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_filters_and_selects_without_submitting() {
        let skills = BTreeMap::from([
            ("agent-review".into(), "review with codex".into()),
            ("summarize".into(), "summarize the changes".into()),
        ]);
        let mut palette = Palette::default();
        palette.insert('r');
        palette.insert('e');
        assert_eq!(matching_skills(&skills, &palette.query).len(), 1);
        assert_eq!(
            palette.selected_prompt(&skills).as_deref(),
            Some("review with codex")
        );
    }

    #[test]
    fn prompt_insertion_preserves_a_non_empty_unicode_draft() {
        let mut input = "앞쪽뒤쪽".to_string();
        let mut cursor = 2;

        insert_prompt(&mut input, &mut cursor, "review this");

        assert_eq!(input, "앞쪽\n\nreview this\n\n뒤쪽");
        assert_eq!(cursor, "앞쪽\n\nreview this\n\n".chars().count());
    }

    #[test]
    fn prompt_insertion_does_not_duplicate_existing_whitespace() {
        let mut input = "context \nnext".to_string();
        let mut cursor = "context ".chars().count();

        insert_prompt(&mut input, &mut cursor, "review this");

        assert_eq!(input, "context \n\nreview this\n\nnext");
    }

    #[test]
    fn upsert_and_remove_preserve_unrelated_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "# mine\n[ui]\ntheme = \"classic\"\n").unwrap();
        upsert(&path, "agent-review", "ask codex to review").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# mine"));
        let cfg = muxa::Config::load(&path).unwrap();
        assert_eq!(
            cfg.message.skills.get("agent-review").map(String::as_str),
            Some("ask codex to review")
        );
        remove(&path, "agent-review").unwrap();
        let cfg = muxa::Config::load(&path).unwrap();
        assert!(cfg.message.skills.is_empty());
    }
}
