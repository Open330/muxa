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

use crate::work_up::expand_tilde;
use muxa::ask::AskProviderKind;
use muxa::config::{AskProviderConfig, Config};
use muxa::pipeline::{self, ProposalSummary};
use muxa::work_compose::{config_prompt, installed_programs};
use std::collections::BTreeMap;

/// The three keys this command owns. Anything else in the file is left
/// exactly as it was.
const OWNED_KEYS: [&str; 3] = ["ticket", "route", "pipeline"];

#[derive(Debug, clap::Args)]
pub struct InitArgs {
    /// Describe the setup in your own words. Omit to be asked.
    #[arg(long)]
    pub describe: Option<String>,
    /// Resolver: claude, codex, gemini, anthropic, or openai. Defaults to
    /// `[ticket].agent`.
    #[arg(long)]
    pub agent: Option<String>,
    /// Print the proposed config and write nothing. The agent turn still
    /// runs and is still billed — only the file write is skipped.
    #[arg(long)]
    pub dry_run: bool,
    /// Skip the confirmation prompt.
    #[arg(long, short = 'y')]
    pub yes: bool,
}

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
    // The schema, the prompt shape, and the "which programs are installed"
    // hint are shared with `work compose` so the two cannot drift.
    let prompt = config_prompt(&describe, &existing, &installed_programs());

    let resolver_cwd = config
        .ticket
        .cwd
        .clone()
        .map(|cwd| PathBuf::from(expand_tilde(&cwd.to_string_lossy())))
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    announce(&resolver_cwd, config, args.dry_run);
    let agent = if args.yes {
        agent
    } else {
        let Some(chosen) = choose_agent(&agent, &config.ask.providers)? else {
            println!("nothing was called.");
            return Ok(());
        };
        chosen
    };
    let answer = muxa::ask::one_shot_configured(
        muxa::ask::OneShot {
            agent: &agent,
            prompt: &prompt,
            cwd: &resolver_cwd,
            permission_mode: config.ticket.permission_mode,
            additional_dirs: &config.ticket.additional_dirs,
            timeout: Duration::from_secs(config.ticket.timeout_secs.max(60)),
        },
        config.ask.providers.get(&agent),
    )
    .await
    .context("asking an agent to write the pipeline config")?;
    if let Some(cost) = answer.cost_usd {
        println!("that turn cost ${cost:.4}.");
    }

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

/// Say what is about to be spawned, and that it costs money, before
/// spawning it.
///
/// `--dry-run` is the reason this exists rather than being obvious: it
/// skips the *file write*, not the agent turn, so "dry run" must not be
/// read as "nothing happens". The expensive half happens either way.
fn announce(cwd: &std::path::Path, config: &Config, dry_run: bool) {
    for line in notice_lines(cwd, config, dry_run) {
        println!("{line}");
    }
}

fn notice_lines(cwd: &std::path::Path, config: &Config, dry_run: bool) -> Vec<String> {
    let mut lines = vec![
        "muxa is about to run one headless agent turn to write your config.".to_string(),
        format!("  runs in   {}", cwd.display()),
        format!(
            "  policy    {:?} permissions, {}s ceiling",
            config.ticket.permission_mode, config.ticket.timeout_secs
        ),
        "  cost      billed to that agent's account, like any other turn".to_string(),
    ];
    if dry_run {
        lines.push(
            "  note      --dry-run skips writing the file; the turn still runs and still bills"
                .to_string(),
        );
    }
    lines
}

/// Which agent to spend the turn on — and, by answering, whether to spend
/// it at all. One prompt rather than "confirm?" then "which?": picking the
/// agent is already the decision.
///
/// Only providers that can actually do the job are listed: a CLI has to
/// be on PATH, and an API provider needs a key the process can already
/// resolve. Offering one that would fail after the operator picked it is
/// worse than not offering it at all.
fn choose_agent(
    default: &str,
    providers: &BTreeMap<String, AskProviderConfig>,
) -> Result<Option<String>> {
    use std::io::IsTerminal;
    let available = available_agents(providers, &|name| which::which(name).is_ok(), &|name| {
        std::env::var(name).ok()
    });
    if available.is_empty() {
        bail!(
            "no headless-capable provider is usable; install one of the agent CLIs or set an              API key for one of: {}",
            muxa::ask::supported_agents().join(", ")
        );
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("this spends a billed agent turn; pass --yes to confirm non-interactively");
    }
    let mut select = cliclack::select("Spend one agent turn on…");
    for name in &available {
        select = select.item(
            Some(name.clone()),
            name.as_str(),
            if name == default { "configured" } else { "" },
        );
    }
    select = select.item(None, "Cancel", "call nothing");
    if available.iter().any(|name| name == default) {
        select = select.initial_value(Some(default.to_string()));
    }
    Ok(select.interact()?)
}

/// Supported by the bridge *and* usable on this machine, in the bridge's
/// preference order: CLIs that are installed, APIs whose key resolves.
fn available_agents(
    providers: &BTreeMap<String, AskProviderConfig>,
    installed: &dyn Fn(&str) -> bool,
    env: &dyn Fn(&str) -> Option<String>,
) -> Vec<String> {
    muxa::ask::provider_infos(providers, "", env)
        .into_iter()
        .filter(|info| match info.kind {
            AskProviderKind::Cli => installed(info.executable.as_deref().unwrap_or(&info.id)),
            AskProviderKind::Api => info.credential_present,
        })
        .map(|info| info.id)
        .collect()
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
    let mut next = last_position(&doc) + 1;
    for key in OWNED_KEYS {
        if let Some(item) = incoming.get(key) {
            if doc.contains_key(key) {
                replaced.push(key.to_string());
            }
            let mut item = item.clone();
            // Append rather than splice. A comment written before a header
            // belongs to that header and lives in the *previous* section's
            // span; inserting a table between the two silently re-homes it,
            // so `# allow_work_start = true` under `[dashboard]` would come
            // back as a key of `[[route]]` the moment someone uncommented
            // it — and unknown keys are a hard error.
            next = place_after(&mut item, next);
            doc.insert(key, item);
        }
    }
    Ok((doc.to_string(), replaced))
}

/// Highest render position any top-level table currently occupies.
fn last_position(doc: &toml_edit::DocumentMut) -> usize {
    doc.iter()
        .filter_map(|(_, item)| match item {
            toml_edit::Item::Table(table) => table.position(),
            toml_edit::Item::ArrayOfTables(array) => {
                array.iter().filter_map(toml_edit::Table::position).max()
            }
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

/// Pin an incoming item to render after everything already in the file,
/// returning the next free position.
fn place_after(item: &mut toml_edit::Item, mut next: usize) -> usize {
    match item {
        toml_edit::Item::Table(table) => {
            table.set_position(next);
            next += 1;
            for (_, child) in table.iter_mut() {
                next = place_after(child, next);
            }
        }
        toml_edit::Item::ArrayOfTables(array) => {
            for table in array.iter_mut() {
                table.set_position(next);
                next += 1;
            }
        }
        _ => {}
    }
    next
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
    fn the_schema_documents_every_key_a_route_accepts() {
        // A key the schema omits is a key the model will not use — it is
        // told not to invent any, and it obeys. `prepare` shipped once
        // without being documented here and simply never appeared in a
        // generated config, so this locks the two together.
        let route = serde_json::to_value(muxa::config::RouteConfig::default())
            .expect("RouteConfig serializes");
        for key in route.as_object().expect("an object").keys() {
            assert!(
                muxa::work_compose::CONFIG_SCHEMA.contains(key.as_str()),
                "route key `{key}` is not in the schema the model is shown"
            );
        }
    }

    #[test]
    fn the_schema_documents_every_key_a_pipeline_agent_accepts() {
        let agent = serde_json::to_value(muxa::config::PipelineAgentConfig::default())
            .expect("PipelineAgentConfig serializes");
        for key in agent.as_object().expect("an object").keys() {
            assert!(
                muxa::work_compose::CONFIG_SCHEMA.contains(key.as_str()),
                "pipeline agent key `{key}` is not in the schema the model is shown"
            );
        }
    }

    #[test]
    fn the_notice_names_the_command_and_that_it_costs_money() {
        let config = Config::default();
        let lines = notice_lines(std::path::Path::new("/home/june"), &config, false);
        let text = lines.join("\n");
        assert!(text.contains("/home/june"), "{text}");
        assert!(text.contains("billed"), "{text}");
        // Nothing about dry-run when this is a real run.
        assert!(!text.contains("--dry-run"), "{text}");
    }

    #[test]
    fn dry_run_says_the_turn_still_bills() {
        // The whole point: --dry-run skips the file write, not the agent
        // turn, so it must not read as "nothing happens".
        let config = Config::default();
        let text = notice_lines(std::path::Path::new("/tmp"), &config, true).join("\n");
        assert!(text.contains("--dry-run skips writing the file"), "{text}");
        assert!(text.contains("still runs and still bills"), "{text}");
    }

    #[test]
    fn a_non_interactive_run_refuses_before_spending_anything() {
        // Tests run without a tty, which is exactly the case that must not
        // silently spend a turn. Whether it lands on the "no agent
        // installed" or the "not a terminal" refusal depends on the
        // machine; both stop before anything is spawned.
        let error = choose_agent("claude", &BTreeMap::new())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("--yes") || error.contains("no headless-capable provider"),
            "{error}"
        );
    }

    #[test]
    fn only_providers_that_could_actually_run_are_offered() {
        let none = BTreeMap::new();
        // Every CLI installed, no key anywhere: the three CLIs, in order.
        let all = available_agents(&none, &|_| true, &|_| None);
        assert_eq!(all, ["claude", "codex", "gemini"]);
        // The launcher knows agy/opencode; the headless bridge does not, so
        // they must never appear here however installed.
        assert!(!all.iter().any(|name| name == "agy"), "{all:?}");
        assert!(!all.iter().any(|name| name == "opencode"), "{all:?}");

        // Supported but absent from PATH is still not offered: picking it
        // would fail after the operator chose it.
        assert_eq!(
            available_agents(&none, &|name| name == "codex", &|_| None),
            ["codex"]
        );
        assert!(available_agents(&none, &|_| false, &|_| None).is_empty());

        // An API provider joins once its key resolves — from its own
        // variable or the one `[ask.providers.<id>]` names.
        let openai_key = |name: &str| (name == "OPENAI_API_KEY").then(|| "k".to_string());
        assert_eq!(available_agents(&none, &|_| false, &openai_key), ["openai"]);
        let mut configured = BTreeMap::new();
        configured.insert(
            "anthropic".to_string(),
            AskProviderConfig {
                api_key_env: Some("WORK_KEY".into()),
                ..AskProviderConfig::default()
            },
        );
        let work_key = |name: &str| (name == "WORK_KEY").then(|| "k".to_string());
        // A configured provider leads the list; the built-ins follow.
        assert_eq!(
            available_agents(&configured, &|name| name == "claude", &work_key),
            ["anthropic", "claude"]
        );
    }

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
    fn a_comment_keeps_the_section_it_was_written_for() {
        // A comment before a header belongs to that header. Inserting a new
        // table between them leaves the comment stranded under the new
        // section, where uncommenting it would mean something else entirely
        // — and, for a key the new section does not accept, break config
        // loading outright.
        let existing =
            "[watch]\ntheme = \"classic\"\n\n# allow_work_start = true\n[ask]\nenabled = true\n";
        let (merged, _) = merge(existing, PROPOSAL).expect("merge");
        let comment = merged.find("# allow_work_start").expect("comment survives");
        let ask = merged.find("[ask]").expect("[ask] survives");
        // The comment must stay inside the span of the section it was
        // written for, which means nothing new may be spliced above it.
        let route = merged.find("[[route]]").expect("route written");
        assert!(
            route > ask,
            "new sections must be appended, not spliced above existing ones:\n{merged}"
        );
        let between = &merged[comment..ask];
        assert!(!between.contains('['), "{merged}");
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
}
