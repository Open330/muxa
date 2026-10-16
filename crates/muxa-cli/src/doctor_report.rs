//! Layout for `muxa doctor`: checks grouped by section, sections folded to
//! one line when everything in them passes, and parallel checks (one per
//! provider, one per installed file) nested under a shared heading instead
//! of repeating it on every line.

use std::fmt::Write as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Level {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone)]
pub(crate) struct Check {
    pub level: Level,
    /// Parallel checks under one heading (a provider, an installed component).
    /// `None` sits directly under the section.
    pub group: Option<String>,
    pub message: String,
}

#[derive(Debug, Default)]
pub(crate) struct Report {
    sections: Vec<(&'static str, Vec<Check>)>,
}

impl Report {
    pub fn push(
        &mut self,
        section: &'static str,
        group: Option<&str>,
        level: Level,
        message: String,
    ) {
        let check = Check {
            level,
            group: group.map(str::to_string),
            message,
        };
        match self.sections.iter_mut().find(|(name, _)| *name == section) {
            Some((_, checks)) => checks.push(check),
            None => self.sections.push((section, vec![check])),
        }
    }

    pub fn issues(&self) -> usize {
        self.sections
            .iter()
            .flat_map(|(_, checks)| checks)
            .filter(|check| check.level != Level::Ok)
            .count()
    }

    /// The whole report. Without `verbose`, a passing section is one line
    /// and a section with issues lists the issues plus a count of what passed.
    pub fn render(&self, verbose: bool, paint: &dyn Fn(Level, &str) -> String) -> String {
        let mut out = String::new();
        for (name, checks) in &self.sections {
            render_section(&mut out, name, checks, verbose, paint);
        }
        out
    }
}

fn worst<'a>(checks: impl IntoIterator<Item = &'a Check>) -> Level {
    checks
        .into_iter()
        .map(|check| check.level)
        .max()
        .unwrap_or(Level::Ok)
}

fn glyph(level: Level) -> &'static str {
    match level {
        Level::Ok => "✔",
        Level::Warn => "⚠",
        Level::Fail => "✗",
    }
}

fn rail(level: Level) -> &'static str {
    match level {
        Level::Ok => "◆",
        Level::Warn => "▲",
        Level::Fail => "■",
    }
}

fn tally<'a>(checks: impl IntoIterator<Item = &'a Check>) -> String {
    let (ok, bad) = checks.into_iter().fold((0, 0), |(ok, bad), check| {
        if check.level == Level::Ok {
            (ok + 1, bad)
        } else {
            (ok, bad + 1)
        }
    });
    let issues = |n: usize| format!("{n} issue{}", if n == 1 { "" } else { "s" });
    match (bad, ok) {
        (0, ok) => format!("{ok} ok"),
        (bad, 0) => issues(bad),
        (bad, ok) => format!("{}, {ok} ok", issues(bad)),
    }
}

/// Groups in first-seen order, with ungrouped checks kept in place as their
/// own one-element entries.
fn entries(checks: &[Check]) -> Vec<(Option<&str>, Vec<&Check>)> {
    let mut entries: Vec<(Option<&str>, Vec<&Check>)> = Vec::new();
    for check in checks {
        match check.group.as_deref() {
            Some(group) => match entries.iter_mut().find(|(g, _)| *g == Some(group)) {
                Some((_, members)) => members.push(check),
                None => entries.push((Some(group), vec![check])),
            },
            None => entries.push((None, vec![check])),
        }
    }
    entries
}

fn render_section(
    out: &mut String,
    name: &str,
    checks: &[Check],
    verbose: bool,
    paint: &dyn Fn(Level, &str) -> String,
) {
    let level = worst(checks);
    if level == Level::Ok && !verbose {
        let detail = match checks {
            [only] if only.group.is_none() => only.message.clone(),
            _ => {
                let groups: Vec<&str> = entries(checks)
                    .iter()
                    .filter_map(|(group, _)| *group)
                    .collect();
                if groups.len() == entries(checks).len() {
                    groups.join(" · ")
                } else {
                    tally(checks)
                }
            }
        };
        let _ = writeln!(
            out,
            "{}  {} {name} — {detail}",
            paint(level, rail(level)),
            paint(level, glyph(level))
        );
        return;
    }
    let _ = writeln!(
        out,
        "{}  {} {name} — {}",
        paint(level, rail(level)),
        paint(level, glyph(level)),
        tally(checks)
    );
    let shown: Vec<(Option<&str>, Vec<&Check>)> = entries(checks)
        .into_iter()
        .filter(|(_, members)| verbose || members.iter().any(|c| c.level != Level::Ok))
        .collect();
    let hidden = if verbose { Vec::new() } else { folded(checks) };
    let hidden_ok = usize::from(!hidden.is_empty());
    let count = shown.len() + usize::from(hidden_ok > 0);
    for (index, (group, members)) in shown.iter().enumerate() {
        let last = index + 1 == count;
        let (branch, stem) = if last { ("└", " ") } else { ("├", "│") };
        match group {
            None => {
                let check = members[0];
                write_item(
                    out,
                    &format!("│  {branch} "),
                    &format!("│  {stem}   "),
                    check,
                    paint,
                );
            }
            Some(group) => {
                let group_level = worst(members.iter().copied());
                let _ = writeln!(
                    out,
                    "│  {branch} {} {group} — {}",
                    paint(group_level, glyph(group_level)),
                    tally(members.iter().copied())
                );
                let listed: Vec<&&Check> = members
                    .iter()
                    .filter(|c| verbose || c.level != Level::Ok)
                    .collect();
                for (inner, check) in listed.iter().enumerate() {
                    let inner_last = inner + 1 == listed.len();
                    let leaf = if inner_last { "└" } else { "├" };
                    let leaf_stem = if inner_last { " " } else { "│" };
                    write_item(
                        out,
                        &format!("│  {stem}  {leaf} "),
                        &format!("│  {stem}  {leaf_stem}   "),
                        check,
                        paint,
                    );
                }
            }
        }
    }
    if hidden_ok > 0 {
        let _ = writeln!(
            out,
            "│  └ {} {} ok (-v to list)",
            paint(Level::Ok, glyph(Level::Ok)),
            hidden.join(" · ")
        );
    }
}

/// What the folded passing checks were: whole groups by name, the rest as a
/// count.
fn folded(checks: &[Check]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut loose = 0;
    for (group, members) in entries(checks) {
        if members.iter().any(|c| c.level != Level::Ok) {
            continue;
        }
        match group {
            Some(group) => names.push(group.to_string()),
            None => loose += members.len(),
        }
    }
    if loose > 0 {
        names.push(if names.is_empty() {
            format!("{loose} more")
        } else {
            format!("{loose} other")
        });
    }
    names
}

/// One check, with continuation lines of a multi-line message kept inside
/// the tree instead of falling back to the left margin.
fn write_item(
    out: &mut String,
    lead: &str,
    continuation: &str,
    check: &Check,
    paint: &dyn Fn(Level, &str) -> String,
) {
    let mut lines = check.message.lines();
    let first = lines.next().unwrap_or_default();
    let _ = writeln!(
        out,
        "{lead}{} {first}",
        paint(check.level, glyph(check.level))
    );
    for line in lines {
        let _ = writeln!(out, "{continuation}{line}");
    }
}

/// `$HOME/...` as `~/...`, so paths stop dominating every line.
pub(crate) fn tilde(message: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && home != "/" => message.replace(&format!("{home}/"), "~/"),
        _ => message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(_: Level, text: &str) -> String {
        text.to_string()
    }

    fn sample() -> Report {
        let mut report = Report::default();
        report.push(
            "muxad",
            None,
            Level::Ok,
            "responding at /tmp/muxa.sock".into(),
        );
        report.push("Hooks", Some("Claude"), Level::Ok, "installed".into());
        report.push("Hooks", Some("Codex"), Level::Ok, "installed".into());
        report.push(
            "Agent integration",
            Some("instructions"),
            Level::Ok,
            "~/.claude/CLAUDE.md".into(),
        );
        report.push(
            "Agent integration",
            Some("instructions"),
            Level::Warn,
            "bootstrap.md — changed".into(),
        );
        report.push(
            "Agent integration",
            Some("mcp"),
            Level::Ok,
            "~/.claude.json".into(),
        );
        report.push(
            "Agent integration",
            None,
            Level::Warn,
            "2 stale servers\n• refresh A\n• refresh B".into(),
        );
        report
    }

    #[test]
    fn passing_sections_fold_and_issues_nest_under_their_group() {
        let text = sample().render(false, &plain);
        assert_eq!(
            text,
            "\
◆  ✔ muxad — responding at /tmp/muxa.sock
◆  ✔ Hooks — Claude · Codex
▲  ⚠ Agent integration — 2 issues, 2 ok
│  ├ ⚠ instructions — 1 issue, 1 ok
│  │  └ ⚠ bootstrap.md — changed
│  ├ ⚠ 2 stale servers
│  │   • refresh A
│  │   • refresh B
│  └ ✔ mcp ok (-v to list)
"
        );
        assert_eq!(sample().issues(), 2);
    }

    #[test]
    fn verbose_lists_every_check() {
        let text = sample().render(true, &plain);
        assert!(text.contains("│  │  ├ ✔ ~/.claude/CLAUDE.md"), "{text}");
        assert!(text.contains("│  ├ ✔ mcp — 1 ok"), "{text}");
        assert!(text.contains("├ ✔ Claude — 1 ok"), "{text}");
        assert!(!text.contains("more ok"), "{text}");
    }
}
