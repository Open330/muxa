//! `muxa automation` — read and steer the rule engine.
//!
//! Every subcommand talks to the running daemon rather than editing
//! `config.toml` directly. That is deliberate: the daemon reads config once
//! at startup and does not watch the file, so an edit made behind its back
//! would not take effect until the next restart. Routing through IPC means
//! `enable`, `pause`, `add`, and `remove` change the live engine *and* the
//! file, in that order, and a rule the merged file would not load is
//! refused before anything is written.
//!
//! Writing `[[automation.rule]]` by hand still works — it is just read at
//! the next daemon start.

use anyhow::{Context, Result};
use clap::Subcommand;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use muxa::automation::{
    parse_duration, AutomationLedgerEntry, AutomationRule, AutomationRules, AutomationTestReport,
};
use muxa::ipc::Client;

#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    action: AutomationCommand,
}

#[derive(Debug, Subcommand)]
enum AutomationCommand {
    /// List every rule with its effective timing, guards, and recent
    /// activity.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show what the engine has done: fired, skipped, and why.
    Log {
        /// Most recent entries to show.
        #[arg(long, value_name = "N", default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Turn one rule on.
    Enable {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Turn one rule off, leaving it in `config.toml`.
    Disable {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Hold every rule for a while. Nothing fires until it expires; no
    /// second command is needed to resume.
    Pause {
        /// How long to hold, e.g. `30m`, `2h`. Defaults to one hour.
        #[arg(long = "for", value_name = "DURATION")]
        duration: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Lift a pause immediately.
    Resume {
        #[arg(long)]
        json: bool,
    },
    /// Write or replace one rule from a JSON description. The shape is one
    /// `[[automation.rule]]`, with `name` required.
    Add {
        /// JSON file describing the rule, or `-` to read it from stdin.
        #[arg(long, value_name = "PATH")]
        from_json: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Remove one rule from `config.toml`.
    Remove {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Evaluate one rule against the agents running right now and print
    /// what it *would* do. Fires nothing and records nothing.
    Test {
        name: String,
        #[arg(long)]
        json: bool,
    },
}

pub(crate) async fn run(args: Args, client: &Client) -> Result<()> {
    match args.action {
        AutomationCommand::List { json } => {
            let rules = client
                .automation_list()
                .await
                .context("listing automation rules")?;
            emit(json, &rules, || render_rules(&rules))
        }
        AutomationCommand::Log { limit, json } => {
            let entries = client
                .automation_log(Some(limit))
                .await
                .context("reading the automation log")?;
            emit(json, &entries, || render_log(&entries))
        }
        AutomationCommand::Enable { name, json } => {
            let rules = client
                .automation_set_enabled(&name, true)
                .await
                .with_context(|| format!("enabling automation rule {name}"))?;
            emit(json, &rules, || format!("enabled {name}\n"))
        }
        AutomationCommand::Disable { name, json } => {
            let rules = client
                .automation_set_enabled(&name, false)
                .await
                .with_context(|| format!("disabling automation rule {name}"))?;
            emit(json, &rules, || format!("disabled {name}\n"))
        }
        AutomationCommand::Pause { duration, json } => {
            let span = match duration.as_deref() {
                Some(text) => parse_duration(text).map_err(anyhow::Error::msg)?,
                None => time::Duration::hours(1),
            };
            let until = time::OffsetDateTime::now_utc() + span;
            let rules = client
                .automation_pause(Some(until))
                .await
                .context("pausing automation")?;
            emit(json, &rules, || {
                format!("automation paused until {}\n", format_time(until))
            })
        }
        AutomationCommand::Resume { json } => {
            let rules = client
                .automation_pause(None)
                .await
                .context("resuming automation")?;
            emit(json, &rules, || "automation resumed\n".to_string())
        }
        AutomationCommand::Add { from_json, json } => {
            let rule = read_rule(&from_json)?;
            let name = rule.name.clone();
            let rules = client
                .automation_set_rule(&rule)
                .await
                .with_context(|| format!("writing automation rule {name}"))?;
            emit(json, &rules, || format!("wrote {name}\n"))
        }
        AutomationCommand::Remove { name, json } => {
            let rules = client
                .automation_remove_rule(&name)
                .await
                .with_context(|| format!("removing automation rule {name}"))?;
            emit(json, &rules, || format!("removed {name}\n"))
        }
        AutomationCommand::Test { name, json } => {
            let report = client
                .automation_test(&name)
                .await
                .with_context(|| format!("testing automation rule {name}"))?;
            emit(json, &report, || render_test(&report))
        }
    }
}

fn emit<T: serde::Serialize>(json: bool, value: &T, human: impl FnOnce() -> String) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        print!("{}", human());
    }
    Ok(())
}

/// Read the rule JSON, validating it here so an obvious mistake is named
/// before the daemon is asked to write it.
fn read_rule(source: &Path) -> Result<AutomationRule> {
    let text = if source == Path::new("-") {
        std::io::read_to_string(std::io::stdin()).context("reading the rule JSON from stdin")?
    } else {
        std::fs::read_to_string(source).with_context(|| format!("reading {}", source.display()))?
    };
    let rule: AutomationRule = serde_json::from_str(&text).context("parsing the rule JSON")?;
    rule.validate().map_err(anyhow::Error::msg)?;
    Ok(rule)
}

fn render_rules(rules: &AutomationRules) -> String {
    let mut out = String::new();
    let state = match (rules.enabled, rules.paused_until) {
        (false, _) => "off ([automation] enabled = false)".to_string(),
        (true, Some(until)) if until > time::OffsetDateTime::now_utc() => {
            format!("paused until {}", format_time(until))
        }
        (true, _) => "on".to_string(),
    };
    let _ = writeln!(out, "engine: {state}");
    if rules.rules.is_empty() {
        out.push_str("no automation rules\n");
        out.push_str(
            "add one with: muxa automation add --from-json rule.json \
             (see docs/AUTOMATION.md)\n",
        );
        return out;
    }
    for rule in &rules.rules {
        let _ = writeln!(
            out,
            "{}\t{}\ton {}\t{}",
            if rule.enabled { "on " } else { "off" },
            rule.name,
            rule.on,
            rule.action,
        );
        let _ = writeln!(
            out,
            "\twait {} (fallback {}, jitter {})\tcooldown {}\tmax {}/h\tstill {}",
            rule.wait,
            rule.fallback,
            rule.jitter,
            rule.cooldown,
            rule.max_per_hour,
            rule.only_if_still,
        );
        let _ = writeln!(out, "\tfilters {}", rule.filters);
        if let Some(payload) = rule.text.as_ref().or(rule.message.as_ref()) {
            let _ = writeln!(out, "\tpayload {payload:?}");
        }
        if let Some(at) = rule.last_fired_at {
            let _ = writeln!(
                out,
                "\tlast fired {} ({} in the last hour)",
                format_time(at),
                rule.fired_last_hour,
            );
        }
    }
    out
}

fn render_log(entries: &[AutomationLedgerEntry]) -> String {
    if entries.is_empty() {
        return "no automation activity recorded\n".into();
    }
    let mut out = String::new();
    for entry in entries {
        let _ = writeln!(
            out,
            "{}\t{}\t{}\t{}\t{}\t{}",
            format_time(entry.fired_at),
            entry.outcome,
            entry.rule,
            entry.pane,
            entry.action,
            entry.detail.as_deref().unwrap_or("-"),
        );
    }
    out
}

fn render_test(report: &AutomationTestReport) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        "{}: rule {}, engine {}",
        report.rule,
        if report.enabled {
            "enabled"
        } else {
            "disabled"
        },
        if report.engine_enabled { "on" } else { "off" },
    );
    if let Some(until) = report.paused_until {
        let _ = write!(out, ", paused until {}", format_time(until));
    }
    out.push('\n');
    if report.candidates.is_empty() {
        out.push_str("no agents to evaluate\n");
        return out;
    }
    for candidate in &report.candidates {
        let _ = write!(
            out,
            "{}\t{}\t{}\t{}",
            candidate.pane.as_deref().unwrap_or("-"),
            candidate.agent,
            candidate.state,
            candidate.decision,
        );
        if let Some(at) = candidate.fire_at {
            let _ = write!(out, "\tat {}", format_time(at));
        }
        if let Some(detail) = &candidate.detail {
            let _ = write!(out, "\t{detail:?}");
        }
        out.push('\n');
    }
    out.push_str("nothing was fired; this was a dry run\n");
    out
}

fn format_time(at: time::OffsetDateTime) -> String {
    at.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| at.unix_timestamp().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use muxa::automation::{
        AutomationAction, AutomationCondition, AutomationEvent, AutomationOutcome,
        AutomationRuleView, AutomationTestCandidate,
    };
    use muxa::{AgentKind, AgentState};

    #[derive(Debug, Parser)]
    struct Harness {
        #[command(subcommand)]
        cmd: Wrapper,
    }

    #[derive(Debug, Subcommand)]
    enum Wrapper {
        Automation(Args),
    }

    fn parse(command_line: &[&str]) -> AutomationCommand {
        let Wrapper::Automation(parsed) = Harness::parse_from(command_line).cmd;
        parsed.action
    }

    #[test]
    fn every_subcommand_parses_as_documented() {
        assert!(matches!(
            parse(&["muxa", "automation", "list", "--json"]),
            AutomationCommand::List { json: true }
        ));
        // `--limit` defaults rather than dumping the whole ledger.
        assert!(matches!(
            parse(&["muxa", "automation", "log"]),
            AutomationCommand::Log {
                limit: 20,
                json: false
            }
        ));
        assert!(matches!(
            parse(&["muxa", "automation", "log", "--limit", "5"]),
            AutomationCommand::Log { limit: 5, .. }
        ));
        assert!(matches!(
            parse(&["muxa", "automation", "enable", "resume-after-limit"]),
            AutomationCommand::Enable { name, .. } if name == "resume-after-limit"
        ));
        assert!(matches!(
            parse(&["muxa", "automation", "disable", "resume-after-limit"]),
            AutomationCommand::Disable { name, .. } if name == "resume-after-limit"
        ));
        assert!(matches!(
            parse(&["muxa", "automation", "pause", "--for", "2h"]),
            AutomationCommand::Pause { duration: Some(d), .. } if d == "2h"
        ));
        assert!(matches!(
            parse(&["muxa", "automation", "pause"]),
            AutomationCommand::Pause { duration: None, .. }
        ));
        assert!(matches!(
            parse(&["muxa", "automation", "resume"]),
            AutomationCommand::Resume { .. }
        ));
        assert!(matches!(
            parse(&["muxa", "automation", "add", "--from-json", "-"]),
            AutomationCommand::Add { from_json, .. } if from_json == Path::new("-")
        ));
        assert!(matches!(
            parse(&["muxa", "automation", "remove", "resume-after-limit"]),
            AutomationCommand::Remove { name, .. } if name == "resume-after-limit"
        ));
        assert!(matches!(
            parse(&["muxa", "automation", "test", "resume-after-limit", "--json"]),
            AutomationCommand::Test { json: true, .. }
        ));
    }

    #[test]
    fn add_requires_a_source() {
        assert!(Harness::try_parse_from(["muxa", "automation", "add"]).is_err());
    }

    #[test]
    fn read_rule_parses_and_validates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rule.json");
        std::fs::write(
            &path,
            r#"{
                "name": "resume-after-limit",
                "on": "rate_limited",
                "action": "send_prompt",
                "text": "continue",
                "wait": "reset+2m",
                "agent": ["claude", "codex"]
            }"#,
        )
        .unwrap();
        let rule = read_rule(&path).unwrap();
        assert_eq!(rule.name, "resume-after-limit");
        assert_eq!(rule.agent.len(), 2);

        // An invalid rule is named here rather than at the daemon.
        std::fs::write(
            &path,
            r#"{"name": "x", "on": "rate_limited", "action": "send_prompt"}"#,
        )
        .unwrap();
        let error = read_rule(&path).unwrap_err().to_string();
        assert!(error.contains("requires `text`"), "{error}");
    }

    fn view() -> AutomationRuleView {
        AutomationRuleView {
            name: "resume-after-limit".into(),
            on: AutomationEvent::RateLimited,
            enabled: true,
            action: AutomationAction::SendPrompt,
            wait: "reset+2m".into(),
            fallback: "20m".into(),
            jitter: "30s".into(),
            cooldown: "5m".into(),
            agent: Vec::new(),
            workspace: None,
            work: None,
            pane: None,
            host: None,
            scope: Vec::new(),
            for_: None,
            text: Some("continue".into()),
            message: None,
            submit: true,
            max_per_hour: 2,
            only_if_still: AutomationCondition::RateLimited,
            filters: "agent=claude_code".into(),
            fired_last_hour: 1,
            last_fired_at: Some(time::macros::datetime!(2026-09-03 11:00:00 UTC)),
        }
    }

    #[test]
    fn rules_render_engine_state_and_each_rule() {
        let rendered = render_rules(&AutomationRules {
            enabled: true,
            paused_until: None,
            rules: vec![view()],
            global_max_per_hour: muxa::automation::GLOBAL_MAX_PER_HOUR,
        });
        assert!(rendered.starts_with("engine: on\n"), "{rendered}");
        assert!(rendered.contains("resume-after-limit"), "{rendered}");
        assert!(rendered.contains("wait reset+2m"), "{rendered}");
        assert!(rendered.contains("max 2/h"), "{rendered}");

        let off = render_rules(&AutomationRules {
            enabled: false,
            paused_until: None,
            rules: Vec::new(),
            global_max_per_hour: muxa::automation::GLOBAL_MAX_PER_HOUR,
        });
        assert!(off.contains("enabled = false"), "{off}");
        // An empty engine says how to get started rather than nothing.
        assert!(off.contains("muxa automation add"), "{off}");
    }

    #[test]
    fn a_pause_in_the_past_reads_as_on() {
        let rendered = render_rules(&AutomationRules {
            enabled: true,
            paused_until: Some(time::OffsetDateTime::now_utc() - time::Duration::hours(1)),
            rules: Vec::new(),
            global_max_per_hour: muxa::automation::GLOBAL_MAX_PER_HOUR,
        });
        assert!(rendered.starts_with("engine: on\n"), "{rendered}");
    }

    #[test]
    fn the_log_names_the_skip_reason() {
        let rendered = render_log(&[AutomationLedgerEntry {
            rule: "resume-after-limit".into(),
            pane: "%42".into(),
            agent: AgentKind::ClaudeCode,
            fired_at: time::macros::datetime!(2026-09-03 12:00:00 UTC),
            action: AutomationAction::SendPrompt,
            outcome: AutomationOutcome::Skipped,
            detail: Some("condition_cleared".into()),
            episode: None,
        }]);
        assert!(rendered.contains("skipped"), "{rendered}");
        assert!(rendered.contains("condition_cleared"), "{rendered}");
        assert_eq!(render_log(&[]), "no automation activity recorded\n");
    }

    #[test]
    fn the_test_report_says_it_fired_nothing() {
        let rendered = render_test(&AutomationTestReport {
            rule: "resume-after-limit".into(),
            enabled: true,
            engine_enabled: true,
            paused_until: None,
            candidates: vec![AutomationTestCandidate {
                pane: Some("%42".into()),
                agent_session_id: "sess-1".into(),
                agent: AgentKind::ClaudeCode,
                state: AgentState::Error,
                decision: "fire".into(),
                fire_at: Some(time::macros::datetime!(2026-09-03 13:02:00 UTC)),
                detail: Some("continue".into()),
            }],
        });
        assert!(
            rendered.contains("%42\tclaude_code\terror\tfire"),
            "{rendered}"
        );
        assert!(rendered.contains("dry run"), "{rendered}");
    }
}
