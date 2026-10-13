//! The top-level `muxa --help`, grouped by what each command is for.
//!
//! clap lists subcommands in one flat block, which at ~45 commands buries the
//! handful used every day among setup and debugging tools. The help body is
//! built from [`GROUPS`]; a test fails when a visible subcommand is missing
//! from it, so new commands cannot silently drop out of the help.

use clap::Command;
use std::fmt::Write as _;

/// Every visible top-level subcommand, by purpose, in display order.
pub(crate) const GROUPS: &[(&str, &[&str])] = &[
    (
        "Everyday",
        &["status", "watch", "dashboard", "attend", "peek", "recap"],
    ),
    ("Collaborate", &["peers", "msg", "identity", "ask", "skill"]),
    ("Work & layout", &["work", "workspace", "window", "agent"]),
    ("Analytics", &["stats", "report", "timeline", "activity"]),
    ("Save & restore", &["snapshot", "restore", "reload"]),
    ("PTY sessions", &["run", "attach", "detach", "register"]),
    ("Fleet", &["fleet", "host"]),
    ("Automation", &["automation"]),
    (
        "Setup",
        &[
            "init", "onboard", "doctor", "config", "upgrade", "daemon", "logs", "mcp",
        ],
    ),
    ("Maintenance", &["sync", "prune", "panes"]),
];

/// The grouped command list, as `{tab}`-free text for the help template.
pub(crate) fn grouped_commands(command: &Command) -> String {
    let width = GROUPS
        .iter()
        .flat_map(|(_, names)| names.iter())
        .map(|name| display_name(command, name).chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for (heading, names) in GROUPS {
        let _ = writeln!(out, "{heading}:");
        for name in *names {
            let about = command
                .find_subcommand(name)
                .and_then(|sub| sub.get_about())
                .map(|about| first_sentence(&about.to_string()))
                .unwrap_or_default();
            let _ = writeln!(out, "  {:<width$}  {about}", display_name(command, name));
        }
        out.push('\n');
    }
    out
}

/// `attend (go)`: the name plus its visible aliases.
fn display_name(command: &Command, name: &str) -> String {
    let aliases: Vec<&str> = command
        .find_subcommand(name)
        .map(|sub| sub.get_visible_aliases().collect())
        .unwrap_or_default();
    if aliases.is_empty() {
        name.to_owned()
    } else {
        format!("{name} ({})", aliases.join(", "))
    }
}

/// Longest summary shown per command; the full text stays in
/// `muxa <command> --help`.
const SUMMARY_CHARS: usize = 72;

/// The help line up to its first sentence or dash clause, whichever comes
/// first, capped at [`SUMMARY_CHARS`].
fn first_sentence(about: &str) -> String {
    let line = about.lines().next().unwrap_or_default().trim();
    let end = [" — ", ". ", ": "]
        .iter()
        .filter_map(|delimiter| line.find(delimiter))
        .min()
        .unwrap_or(line.len());
    let sentence = line[..end].trim_end_matches('.');
    if sentence.chars().count() <= SUMMARY_CHARS {
        return sentence.to_owned();
    }
    let cut: String = sentence.chars().take(SUMMARY_CHARS - 1).collect();
    format!("{}…", cut.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};
    use std::collections::HashSet;

    #[test]
    fn every_visible_command_is_in_exactly_one_group() {
        let command = crate::Args::command();
        let visible: HashSet<&str> = command
            .get_subcommands()
            .filter(|sub| !sub.is_hide_set())
            .map(Command::get_name)
            .collect();
        let mut grouped = HashSet::new();
        for (_, names) in GROUPS {
            for name in *names {
                assert!(grouped.insert(*name), "`{name}` is listed twice");
                assert!(
                    visible.contains(name),
                    "`{name}` is grouped but not a visible command"
                );
            }
        }
        let missing: Vec<_> = visible.difference(&grouped).collect();
        assert!(
            missing.is_empty(),
            "add these to help_groups::GROUPS: {missing:?}"
        );
    }

    #[test]
    fn internal_entrypoints_are_hidden_but_still_parse() {
        let command = crate::Args::command();
        for name in ["hook", "relay", "status-line"] {
            let sub = command.find_subcommand(name).expect(name);
            assert!(sub.is_hide_set(), "`{name}` is invoked by tools, not typed");
        }
        assert!(crate::Args::try_parse_from(["muxa", "status-line", "--needs-attention"]).is_ok());
        let help = grouped_commands(&command);
        assert!(help.contains("attend (go)"));
        assert!(!help.contains("relay"));
    }

    #[test]
    fn about_is_cut_to_its_first_sentence() {
        assert_eq!(
            first_sentence("Jump to the agent — focus it. More"),
            "Jump to the agent"
        );
        assert_eq!(first_sentence("List peers. Then more."), "List peers");
        assert_eq!(first_sentence("Single line."), "Single line");
        assert_eq!(first_sentence("Ask it. The rest — more"), "Ask it");
        assert_eq!(
            first_sentence("Update muxa: `git pull` → install"),
            "Update muxa"
        );
        assert!(first_sentence(&"word ".repeat(40)).chars().count() <= SUMMARY_CHARS);
    }
}
