//! `muxa work init` — describe a work pipeline in words and let an agent
//! write the config.
//!
//! `[ticket]`, `[[route]]`, and `[pipeline.*]` are the most structured
//! configuration muxa asks for, and the least guessable: nested tables, an
//! ordered array of routes, regexes, placeholder templates. Hand-editing it
//! from documentation is exactly the friction that stops the feature being
//! used at all.
//!
//! So muxa does here what it already does for tickets: it asks an agent.
//! The operator says "callabo tickets get a codex planner, a codex
//! implementer and a claude reviewer", one headless turn turns that into
//! TOML, and muxa's job is to be the part that does *not* trust the answer
//! — parse it, compile every regex, check every pipeline a route names
//! exists and every agent it lists could actually launch, show what it
//! would change, and only then write.
//!
//! Nothing outside those three keys is touched, and the file is rewritten
//! through `toml_edit`, so comments and hand-tuned sections survive.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::time::Duration;

use muxa::config::Config;
use muxa::pipeline::{self, ProposalSummary};

/// The three keys this command owns. Anything else in the file is left
/// exactly as it was.
const OWNED_KEYS: [&str; 3] = ["ticket", "route", "pipeline"];

#[derive(Debug, clap::Args)]
pub struct InitArgs {
    /// Describe the setup in your own words. Omit to be asked.
    #[arg(long)]
    pub describe: Option<String>,
    /// Resolver agent: claude or codex. Defaults to `[ticket].agent`.
    #[arg(long)]
    pub agent: Option<String>,
    /// Print the proposed config and change nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Skip the confirmation prompt.
    #[arg(long, short = 'y')]
    pub yes: bool,
}

/// What the model is told about the shape it must produce. Kept next to the
/// command rather than in the docs directory so the two cannot drift: if
/// the schema changes, this is in the same diff.
const SCHEMA: &str = r#"
muxa work pipeline configuration. Three top-level keys, all optional except
as noted:

[ticket]                      # how a work id becomes ticket context
agent = "claude"              # or "codex" — the resolver CLI
cwd = "~"                     # where the resolver runs
timeout_secs = 300
cache_secs = 900              # 0 disables the cache
additional_dirs = ["/path"]   # extra roots the resolver may read

[ticket.source.<name>]        # tried in sorted-key order, first match wins
match = '^cal-\d+$'           # regex against the work id, case-insensitive
prompt = '''...'''            # asks an agent to answer with ticket JSON.
                              # {{id}} is the lowercased work id. muxa reads
                              # id/identifier/key, title/name/summary,
                              # body/description, url, state, branch.

[[route]]                     # REQUIRED: ordered, first match wins
match     = '^cal-'           # regex against the work id
workspace = 'callabo'         # the tmux session; defaults to the cwd name
pipeline  = 'triad'           # must name a [pipeline.*] below
cwd       = '~/src/{{id}}'    # optional; omit to use the current directory
[route.worktree]              # optional: a git worktree per work item
repo   = '~/src/repo'
branch = '{{id}}'

[pipeline.<name>]             # REQUIRED: at least one
layout = 'main-vertical'      # tmux layout, applied once every pane exists
prompt = '''...'''            # context every agent in this pipeline gets

[[pipeline.<name>.agent]]     # one per pane, at least one
alias   = 'impl'              # unique within the pipeline; keys the pane diff
program = 'codex'             # ONLY claude, codex, gemini, or opencode
role    = 'implementer'       # optional; peers address it as role:<role>
prompt  = '...'               # optional; this agent's own instructions
direction = 'right'           # optional: right (default) or down

Placeholders, usable in any prompt/path/workspace string:
{{id}} lowercased work id, {{work}} as muxa stores it, {{workspace}},
{{cwd}}, {{alias}}, {{role}}, {{program}}, {{request}} (the caller's
--body/--skill/--context), and {{ticket.title|body|url|state|id|branch}}.

Unknown keys are a hard error, so do not invent any.
"#;

pub async fn run(args: InitArgs, config: &Config, config_path: Option<PathBuf>) -> Result<()> {
    let path = config_path
        .or_else(muxa::paths::default_config_file)
        .context("no config directory is available on this system")?;
    let describe = match args.describe.as_deref().map(str::trim) {
        Some(text) if !text.is_empty() => text.to_string(),
        _ => ask_operator()?,
    };

    let agent = args
        .agent
        .as_deref()
        .unwrap_or(config.ticket.agent.as_str())
        .to_string();
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let prompt = compose_prompt(&describe, &existing);

    println!("asking {agent} to write the config…");
    let answer = muxa::ask::one_shot(muxa::ask::OneShot {
        agent: &agent,
        prompt: &prompt,
        cwd: &dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")),
        permission_mode: config.ticket.permission_mode,
        additional_dirs: &config.ticket.additional_dirs,
        timeout: Duration::from_secs(config.ticket.timeout_secs.max(60)),
    })
    .await
    .context("asking an agent to write the pipeline config")?;

    let block = pipeline::extract_toml_block(&answer.text)
        .ok_or_else(|| anyhow::anyhow!("{agent} answered without any TOML"))?
        .trim()
        .to_string();
    // Refuse before writing: Config denies unknown fields, so a model's
    // typo would take the daemon down at its next start.
    let (_, summary) = pipeline::validate_proposal(&block)
        .with_context(|| format!("{agent} produced a config muxa will not write"))?;

    let (merged, replaced) = merge(&existing, &block)?;
    report(&block, &summary, &replaced, &path);

    if args.dry_run {
        println!("\ndry run: nothing was written.");
        return Ok(());
    }
    if !args.yes && !confirm(&replaced)? {
        println!("left {} unchanged.", path.display());
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    crate::init::apply::atomic_write(&path, &merged)
        .with_context(|| format!("writing {}", path.display()))?;
    println!("wrote {}", path.display());
    println!("try it:  muxa work up <ticket-id> --dry-run");
    Ok(())
}

fn ask_operator() -> Result<String> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        bail!("no description given; pass --describe \"...\" when stdin is not a terminal");
    }
    let text: String = cliclack::input("Describe the work pipeline you want")
        .placeholder("cal-* tickets: codex planner, codex implementer, claude reviewer")
        .interact()?;
    let text = text.trim().to_string();
    if text.is_empty() {
        bail!("description cannot be empty");
    }
    Ok(text)
}

fn compose_prompt(describe: &str, existing: &str) -> String {
    // The current file goes in so the model extends what is there rather
    // than proposing a config that contradicts it — and so it can see which
    // of the three keys already exist.
    let current = if existing.trim().is_empty() {
        "(the config file is empty or absent)".to_string()
    } else {
        format!("Current config.toml:\n```toml\n{}\n```", existing.trim())
    };
    format!(
        "You are writing muxa work pipeline configuration.\n\n\
         SCHEMA\n{SCHEMA}\n\n\
         {current}\n\n\
         WHAT THE OPERATOR WANTS\n{describe}\n\n\
         Answer with ONE ```toml block containing only the [ticket], [[route]], and\n\
         [pipeline.*] sections. Do not repeat other sections of the current config.\n\
         Include at least one [[route]] and one [pipeline.*]. End routes with a\n\
         catch-all `match = '.*'` unless the operator said otherwise. Prefer omitting\n\
         `cwd` so the work runs where the operator invoked it. No prose outside the\n\
         block."
    )
}

/// Copy the three owned keys from the proposal into the existing document,
/// leaving every other key and all formatting alone. Returns the merged
/// text and which keys were overwritten rather than added.
fn merge(existing: &str, proposal: &str) -> Result<(String, Vec<String>)> {
    let mut doc: toml_edit::DocumentMut = if existing.trim().is_empty() {
        toml_edit::DocumentMut::new()
    } else {
        existing
            .parse()
            .context("your existing config.toml does not parse")?
    };
    let incoming: toml_edit::DocumentMut = proposal
        .parse()
        .context("the proposed config does not parse")?;
    let mut replaced = Vec::new();
    for key in OWNED_KEYS {
        if let Some(item) = incoming.get(key) {
            if doc.contains_key(key) {
                replaced.push(key.to_string());
            }
            doc.insert(key, item.clone());
        }
    }
    Ok((doc.to_string(), replaced))
}

fn report(block: &str, summary: &ProposalSummary, replaced: &[String], path: &std::path::Path) {
    println!("\n{block}\n");
    println!("this configures:");
    if !summary.ticket_sources.is_empty() {
        println!("  ticket sources  {}", summary.ticket_sources.join(", "));
    }
    for route in &summary.routes {
        println!("  route           {route}");
    }
    for (name, agents) in &summary.pipelines {
        println!("  pipeline        {name} ({agents} agents)");
    }
    println!("  target          {}", path.display());
    if !replaced.is_empty() {
        println!(
            "  REPLACES        existing [{}] — everything else in the file is kept",
            replaced.join("], [")
        );
    }
}

fn confirm(replaced: &[String]) -> Result<bool> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("confirmation requires an interactive terminal; pass --yes");
    }
    let prompt = if replaced.is_empty() {
        "Write this to your config?".to_string()
    } else {
        format!("Replace existing [{}] with this?", replaced.join("], ["))
    };
    Ok(cliclack::confirm(prompt).initial_value(false).interact()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROPOSAL: &str = r"
[[route]]
match = '.*'
pipeline = 'solo'

[pipeline.solo]
[[pipeline.solo.agent]]
alias = 'main'
program = 'claude'
";

    #[test]
    fn merging_keeps_every_section_it_does_not_own() {
        let existing = "[watch]\ntheme = \"classic\"\n\n[ask]\nenabled = true\n";
        let (merged, replaced) = merge(existing, PROPOSAL).expect("merge");
        assert!(replaced.is_empty());
        // Untouched sections survive verbatim, comments and all.
        assert!(merged.contains("theme = \"classic\""), "{merged}");
        assert!(merged.contains("[ask]"), "{merged}");
        assert!(merged.contains("[pipeline.solo]"), "{merged}");
        // And the result is still a config muxa can load.
        let parsed: Config = toml::from_str(&merged).expect("merged config parses");
        assert_eq!(parsed.route.len(), 1);
        assert!(parsed.ask.enabled);
    }

    #[test]
    fn merging_reports_which_owned_keys_it_overwrites() {
        let existing = "[pipeline.old]\n[[pipeline.old.agent]]\nalias = 'x'\nprogram = 'codex'\n";
        let (merged, replaced) = merge(existing, PROPOSAL).expect("merge");
        assert_eq!(replaced, vec!["pipeline".to_string()]);
        // Replaced wholesale, not merged key-by-key: a half-old, half-new
        // pipeline set is harder to reason about than a stated replacement.
        assert!(!merged.contains("pipeline.old"), "{merged}");
    }

    #[test]
    fn a_comment_in_the_operators_config_survives() {
        let existing = "# my notes\n[watch]\ntheme = \"classic\"\n";
        let (merged, _) = merge(existing, PROPOSAL).expect("merge");
        assert!(merged.contains("# my notes"), "{merged}");
    }

    #[test]
    fn a_broken_existing_config_is_reported_not_overwritten() {
        let error = merge("[watch\n", PROPOSAL).unwrap_err().to_string();
        assert!(error.contains("existing config.toml"), "{error}");
    }

    #[test]
    fn the_prompt_carries_the_schema_and_the_current_file() {
        let prompt = compose_prompt("cal tickets get three agents", "[watch]\ntheme = \"ops\"\n");
        assert!(
            prompt.contains("[[pipeline.<name>.agent]]"),
            "schema missing"
        );
        assert!(prompt.contains("theme = \"ops\""), "current config missing");
        assert!(prompt.contains("cal tickets get three agents"));
        // An empty file says so rather than sending an empty fence.
        assert!(compose_prompt("x", "  ").contains("empty or absent"));
    }
}
