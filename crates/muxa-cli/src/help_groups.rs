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
    ("Analytics", &["stats", "timeline"]),
    ("Save & restore", &["snapshot", "restore", "reload"]),
    ("Fleet", &["fleet", "host"]),
    ("Run & automate", &["proc", "automation"]),
    (
        "Setup",
        &[
            "init", "onboard", "doctor", "config", "upgrade", "daemon", "registry", "mcp",
        ],
    ),
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
        .flat_map(|delimiter| line.match_indices(delimiter).map(|(at, _)| at))
        .filter(|&at| is_sentence_break(&line[..at]))
        .min()
        .unwrap_or(line.len());
    let sentence = line[..end].trim_end_matches('.');
    if sentence.chars().count() <= SUMMARY_CHARS {
        return sentence.to_owned();
    }
    let cut: String = sentence.chars().take(SUMMARY_CHARS - 1).collect();
    format!("{}…", cut.trim_end())
}

/// A delimiter ends the summary unless it sits inside parentheses
/// ("(default: `$TMUX_PANE`)") or follows a one-word label ("Debug: …").
fn is_sentence_break(before: &str) -> bool {
    before.matches('(').count() == before.matches(')').count() && before.contains(' ')
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

    /// Former top-level names that now live under a parent command.
    const MOVED: &[&str] = &[
        "logs", "sync", "prune", "panes", "run", "attach", "detach", "register", "report",
        "activity",
    ];

    fn parse(argv: &[&str]) -> crate::Cmd {
        crate::Args::try_parse_from(argv)
            .unwrap_or_else(|error| panic!("{argv:?}: {error}"))
            .cmd
    }

    /// Commands that moved under a parent keep their old top-level name as a
    /// hidden alias, so scripts and hook configs written against it still run.
    #[test]
    fn moved_commands_keep_their_old_names() {
        let command = crate::Args::command();
        for old in MOVED {
            let sub = command.find_subcommand(old).expect(old);
            assert!(sub.is_hide_set(), "`{old}` moved under a parent; hide it");
        }

        let crate::Cmd::Logs(old) = parse(&["muxa", "logs", "-n", "5", "-N"]) else {
            panic!("`muxa logs` must still parse");
        };
        let crate::Cmd::Daemon {
            action: crate::daemon::Action::Logs(new),
        } = parse(&["muxa", "daemon", "logs", "-n", "5", "-N"])
        else {
            panic!("`muxa daemon logs` must parse");
        };
        assert_eq!(format!("{old:?}"), format!("{new:?}"));

        let prune = ["--older-than", "30m", "--all", "-y"];
        let old_argv: Vec<&str> = ["muxa", "prune"].iter().chain(&prune).copied().collect();
        let new_argv: Vec<&str> = ["muxa", "registry", "prune"]
            .iter()
            .chain(&prune)
            .copied()
            .collect();
        let crate::Cmd::Prune(old) = parse(&old_argv) else {
            panic!("`muxa prune` must still parse");
        };
        let crate::Cmd::Registry {
            action: crate::RegistryCmd::Prune(new),
        } = parse(&new_argv)
        else {
            panic!("`muxa registry prune` must parse");
        };
        assert_eq!(format!("{old:?}"), format!("{new:?}"));
        assert!(matches!(parse(&["muxa", "sync"]), crate::Cmd::Sync));
        assert!(matches!(
            parse(&["muxa", "registry", "sync"]),
            crate::Cmd::Registry {
                action: crate::RegistryCmd::Sync
            }
        ));
        assert!(matches!(parse(&["muxa", "panes"]), crate::Cmd::Panes));

        let run = ["--name", "job", "--detach", "--", "sleep", "-x"];
        let old_argv: Vec<&str> = ["muxa", "run"].iter().chain(&run).copied().collect();
        let new_argv: Vec<&str> = ["muxa", "proc", "run"]
            .iter()
            .chain(&run)
            .copied()
            .collect();
        let crate::Cmd::Run(old) = parse(&old_argv) else {
            panic!("`muxa run` must still parse");
        };
        let crate::Cmd::Proc {
            action: crate::ProcCmd::Run(new),
        } = parse(&new_argv)
        else {
            panic!("`muxa proc run` must parse");
        };
        assert_eq!(format!("{old:?}"), format!("{new:?}"));
        assert_eq!(new.command, ["sleep", "-x"]);

        let register = ["--name", "loop", "--pid", "42", "--pane", "%3"];
        let old_argv: Vec<&str> = ["muxa", "register"]
            .iter()
            .chain(&register)
            .copied()
            .collect();
        let new_argv: Vec<&str> = ["muxa", "proc", "register"]
            .iter()
            .chain(&register)
            .copied()
            .collect();
        let crate::Cmd::Register(old) = parse(&old_argv) else {
            panic!("`muxa register` must still parse");
        };
        let crate::Cmd::Proc {
            action: crate::ProcCmd::Register(new),
        } = parse(&new_argv)
        else {
            panic!("`muxa proc register` must parse");
        };
        assert_eq!(format!("{old:?}"), format!("{new:?}"));

        assert!(matches!(
            parse(&["muxa", "attach", "s1"]),
            crate::Cmd::Attach { session } if session == "s1"
        ));
        assert!(matches!(
            parse(&["muxa", "proc", "attach", "s1"]),
            crate::Cmd::Proc { action: crate::ProcCmd::Attach { session } } if session == "s1"
        ));
        assert!(matches!(
            parse(&["muxa", "detach", "s1"]),
            crate::Cmd::Detach { session } if session == "s1"
        ));
        assert!(matches!(
            parse(&["muxa", "proc", "detach", "s1"]),
            crate::Cmd::Proc { action: crate::ProcCmd::Detach { session } } if session == "s1"
        ));
    }

    #[test]
    fn stats_views_keep_their_old_names() {
        let report = ["--since", "week", "--limit", "3", "--markdown"];
        let old_argv: Vec<&str> = ["muxa", "report"].iter().chain(&report).copied().collect();
        let new_argv: Vec<&str> = ["muxa", "stats", "report"]
            .iter()
            .chain(&report)
            .copied()
            .collect();
        let crate::Cmd::Report(old) = parse(&old_argv) else {
            panic!("`muxa report` must still parse");
        };
        let crate::Cmd::Stats(new) = parse(&new_argv) else {
            panic!("`muxa stats report` must parse");
        };
        let Some(crate::stats::StatsCmd::Report(view_args)) = new.action() else {
            panic!("`muxa stats report` must select the report view");
        };
        assert_eq!(format!("{:?}", old.args), format!("{view_args:?}"));
        assert_eq!((old.window.since.as_str(), new.since()), ("week", "week"));

        let activity = ["--since", "today", "--type", "agent", "--limit", "5"];
        let old_argv: Vec<&str> = ["muxa", "activity"]
            .iter()
            .chain(&activity)
            .copied()
            .collect();
        let new_argv: Vec<&str> = ["muxa", "stats", "activity"]
            .iter()
            .chain(&activity)
            .copied()
            .collect();
        let crate::Cmd::Activity(old) = parse(&old_argv) else {
            panic!("`muxa activity` must still parse");
        };
        let crate::Cmd::Stats(new) = parse(&new_argv) else {
            panic!("`muxa stats activity` must parse");
        };
        let Some(crate::stats::StatsCmd::Activity(view_args)) = new.action() else {
            panic!("`muxa stats activity` must select the activity view");
        };
        assert_eq!(format!("{:?}", old.args), format!("{view_args:?}"));
        assert_eq!((old.window.since.as_str(), new.since()), ("today", "today"));
    }

    /// Shared options read the same on either side of the view name; options
    /// that only the summary understands are refused before a view instead
    /// of being dropped silently.
    #[test]
    fn stats_options_are_order_independent_or_refused() {
        let before = parse(&[
            "muxa", "stats", "--since", "week", "--theme", "mono", "report",
        ]);
        let after = parse(&[
            "muxa", "stats", "report", "--since", "week", "--theme", "mono",
        ]);
        let (crate::Cmd::Stats(before), crate::Cmd::Stats(after)) = (before, after) else {
            panic!("both spellings select `muxa stats`");
        };
        assert_eq!(format!("{before:?}"), format!("{after:?}"));
        assert_eq!(before.since(), "week");

        let placement = |argv: &[&str]| {
            let mut command = crate::Args::command();
            let matches = command.clone().try_get_matches_from(argv).expect(argv[2]);
            crate::stats::check_flag_placement(&mut command, &matches)
        };
        assert!(placement(&["muxa", "stats", "--graph"]).is_ok());
        assert!(placement(&["muxa", "stats", "--since", "week", "report"]).is_ok());
        assert!(placement(&["muxa", "stats", "report", "--limit", "3"]).is_ok());
        for misplaced in [["--graph", ""], ["--limit", "3"]] {
            let argv: Vec<&str> = ["muxa", "stats"]
                .into_iter()
                .chain(misplaced.into_iter().filter(|part| !part.is_empty()))
                .chain(["report"])
                .collect();
            let error = placement(&argv).expect_err("summary flag before a view");
            assert!(error.to_string().contains(misplaced[0]), "{error}");
        }
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
        assert_eq!(
            first_sentence("Show prompts for the pane (default: `$TMUX_PANE`)."),
            "Show prompts for the pane (default: `$TMUX_PANE`)"
        );
        assert_eq!(
            first_sentence("Debug: print the active pane-host inventory."),
            "Debug: print the active pane-host inventory"
        );
        assert!(first_sentence(&"word ".repeat(40)).chars().count() <= SUMMARY_CHARS);
    }
}
