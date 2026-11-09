//! Recognize an affirmative choice before a disabled-by-default Enter action,
//! and read the shell command a confirmation asks about.
//!
//! Everything here fails closed: a screen that is not exactly a known,
//! complete confirmation gives no Yes and no request, so no Enter is sent.

use std::sync::LazyLock;

use regex::Regex;

static CURSOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[\t ]*(?:│[\t ]*)?[>›❯](?:[\t ].*)?$").expect("static cursor regex")
});
/// The selected row, split into its label: cursor, optional number, label.
static SELECTED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[\t ]*(?:│[\t ]*)?[>›❯][\t ]*(?:\d+[.)][\t ]*)?(?P<label>.*?)[\t ]*│?[\t ]*$")
        .expect("static selected row regex")
});
/// The complete labels of an option that confirms this one request and
/// grants nothing more. A positive list: anything else, including a label
/// that goes on ("Yes, and don't ask again"), is not a one-time Yes.
static ONE_TIME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:yes|proceed|continue|yes,? proceed|yes,? continue)(?:[\t ]*\([a-z]\))?$")
        .expect("static one-time label regex")
});
/// Any option row, selected or not: `1. …`, `› 2) …`, with its number.
static OPTION_ROW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[\t ]*(?:│[\t ]*)?(?:[>›❯][\t ]*)?(?P<number>\d+)[.)][\t ]")
        .expect("static option row regex")
});
/// The unselected rows an unnumbered confirmation may list under its Yes.
static SHORT_OPTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:no|cancel|exit|quit|skip|no, (?:exit|quit|cancel))$")
        .expect("static short option regex")
});
static FOOTER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:press enter|enter to (?:confirm|select|continue)|esc(?:ape)? to cancel)\b",
    )
    .expect("static confirmation footer regex")
});
static CODEX_COMMAND_HEADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^would you like to run the following command\?$")
        .expect("static codex header regex")
});
static CLAUDE_COMMAND_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^bash command$").expect("static claude header regex"));
static CLAUDE_QUESTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^do you want to proceed\?$").expect("static claude question regex")
});
static SGR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[([0-9;:]*)m").expect("static sgr regex"));

/// How far above the selected row a command dialog may start.
const DIALOG_LINES: usize = 14;

/// The newest screen as raw lines (styling kept) and visible lines, with
/// the blank padding below the last content dropped.
fn screen_lines(raw: &str) -> (Vec<String>, Vec<String>) {
    let mut raw_lines: Vec<String> = raw.lines().map(str::to_string).collect();
    while raw_lines
        .last()
        .is_some_and(|line| crate::screen::strip_ansi(line).trim().is_empty())
    {
        raw_lines.pop();
    }
    let start = raw_lines.len().saturating_sub(30);
    let raw_lines = raw_lines.split_off(start);
    let visible = raw_lines
        .iter()
        .map(|line| crate::screen::strip_ansi(line))
        .collect();
    (raw_lines, visible)
}

/// A line's text inside a dialog box: box sides and padding removed.
fn content(line: &str) -> &str {
    line.trim().trim_matches(|c| c == '│' || c == '|').trim()
}

/// The number on an option row, if it is one.
fn option_number(line: &str) -> Option<u32> {
    OPTION_ROW
        .captures(line)?
        .name("number")?
        .as_str()
        .parse()
        .ok()
}

/// Whether a line ends the options block: blank, the footer, or a box's
/// bottom edge.
fn ends_options(line: &str) -> bool {
    content(line).is_empty() || FOOTER.is_match(line) || line.trim_start().starts_with(['╰', '└'])
}

/// The selected row's index and number, when the screen is a confirmation
/// choice: a footer, exactly one highlighted row whose label is a complete
/// one-time Yes, and under it only the remaining options (numbered on in
/// order, or short unnumbered labels) up to the footer. Any other line
/// there, such as a label wrapped onto the next line, refuses the screen.
fn selected_affirmative_row(visible: &[String]) -> Option<(usize, Option<u32>)> {
    if !FOOTER.is_match(&visible.join("\n")) {
        return None;
    }
    let mut cursors = visible
        .iter()
        .enumerate()
        .filter(|(_, line)| CURSOR.is_match(line));
    let (index, selected) = cursors.next()?;
    if cursors.next().is_some() {
        return None;
    }
    let label = SELECTED.captures(selected)?.name("label")?.as_str();
    if !ONE_TIME.is_match(label) {
        return None;
    }
    let number = option_number(selected);
    let mut expected = number.map(|n| n + 1);
    for line in &visible[index + 1..] {
        if ends_options(line) {
            break;
        }
        match expected {
            Some(next) if option_number(line) == Some(next) => expected = Some(next + 1),
            None if SHORT_OPTION.is_match(content(line)) => {}
            _ => return None,
        }
    }
    Some((index, number))
}

/// Require one highlighted one-time Yes and a confirmation footer on the
/// newest screen. Plain prose, No, free-text composers, ambiguous cursors
/// and every option that grants more than this request are refused. ANSI
/// styling is removed before matching.
#[must_use]
pub fn affirmative_choice_visible(raw: &str) -> bool {
    let (_, visible) = screen_lines(raw);
    selected_affirmative_row(&visible).is_some()
}

/// Whether a raw line's text starts out faint: dim (`2`), bright black
/// (`90`), or a mid grey truecolor/256-color foreground.
fn starts_faint(raw_line: &str) -> bool {
    let mut faint = false;
    let mut rest = raw_line;
    loop {
        let next_sgr = SGR.captures(rest);
        let text_end = next_sgr.as_ref().map_or(rest.len(), |captures| {
            captures.get(0).map_or(rest.len(), |m| m.start())
        });
        if rest[..text_end]
            .chars()
            .any(|c| !c.is_whitespace() && c != '│' && c != '|')
        {
            return faint;
        }
        let Some(captures) = next_sgr else {
            return faint;
        };
        let params: Vec<&str> = captures[1].split([';', ':']).collect();
        let mut index = 0;
        while index < params.len() {
            match params[index] {
                "" | "0" | "22" | "39" => faint = false,
                "2" | "90" => faint = true,
                "38" if params.get(index + 1) == Some(&"5") => {
                    let color: u16 = params
                        .get(index + 2)
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    faint = color == 8 || (240..=250).contains(&color);
                    index += 2;
                }
                "38" if params.get(index + 1) == Some(&"2") => {
                    let rgb: Vec<i32> = (2..5)
                        .filter_map(|offset| params.get(index + offset)?.parse().ok())
                        .collect();
                    faint = rgb.len() == 3
                        && (rgb[0] - rgb[1]).abs() < 16
                        && (rgb[1] - rgb[2]).abs() < 16
                        && (80..=200).contains(&rgb[0]);
                    index += 4;
                }
                _ => {}
            }
            index += 1;
        }
        rest = &rest[captures.get(0).map_or(rest.len(), |m| m.end())..];
    }
}

/// The shell command a confirmation choice asks to run, or `None` unless
/// the screen is exactly one known command dialog with a one-time Yes
/// selected:
///
/// - Codex: one `Would you like to run the following command?` heading,
///   one `$ …` line under it, followed directly by the options.
/// - Claude Code: one `Bash command` heading, then the command, then its
///   faint description, then `Do you want to proceed?`. Lines before the
///   faint description are the command; with no faint line at all, every
///   line counts as the command.
///
/// Text outside the dialog, and an agent-written description, are never
/// part of the command.
#[must_use]
pub fn choice_command(raw: &str) -> Option<String> {
    let (raw_lines, visible) = screen_lines(raw);
    // A command dialog opens with its first option selected.
    let (selected, Some(1)) = selected_affirmative_row(&visible)? else {
        return None;
    };
    // Exactly one heading on the whole capture, close enough above the
    // options that the dialog is complete on screen.
    let headers: Vec<usize> = (0..selected)
        .filter(|&index| {
            let text = content(&visible[index]);
            CODEX_COMMAND_HEADER.is_match(text) || CLAUDE_COMMAND_HEADER.is_match(text)
        })
        .collect();
    let [header] = headers[..] else {
        return None;
    };
    if selected - header > DIALOG_LINES {
        return None;
    }
    // A box that closed between the heading and the options is another box.
    if visible[header + 1..selected]
        .iter()
        .any(|line| line.trim_start().starts_with(['╰', '└']))
    {
        return None;
    }
    if CODEX_COMMAND_HEADER.is_match(content(&visible[header])) {
        // Exactly the heading, one `$ …` line, then the selected option.
        let body: Vec<&str> = visible[header + 1..selected]
            .iter()
            .map(|line| content(line))
            .filter(|line| !line.is_empty())
            .collect();
        let [line] = body[..] else {
            return None;
        };
        let command = line.strip_prefix("$ ")?.trim();
        return (!command.is_empty()).then(|| command.to_string());
    }
    let questions: Vec<usize> = (header + 1..selected)
        .filter(|&index| CLAUDE_QUESTION.is_match(content(&visible[index])))
        .collect();
    let [question] = questions[..] else {
        return None;
    };
    let body: Vec<usize> = (header + 1..question)
        .filter(|&index| !content(&visible[index]).is_empty())
        .collect();
    let first_faint = body
        .iter()
        .position(|&index| starts_faint(&raw_lines[index]));
    let command_lines = match first_faint {
        Some(0) => return None,
        Some(split) => {
            // Everything after the description starts must be description.
            if !body[split..]
                .iter()
                .all(|&index| starts_faint(&raw_lines[index]))
            {
                return None;
            }
            &body[..split]
        }
        None => &body[..],
    };
    if command_lines.is_empty() {
        return None;
    }
    // A Codex-style options block cannot appear inside Claude's dialog body.
    if command_lines
        .iter()
        .any(|&index| OPTION_ROW.is_match(&visible[index]))
    {
        return None;
    }
    Some(
        command_lines
            .iter()
            .map(|&index| content(&visible[index]))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// `\A(?:pattern)\z` over the whole text, so alternation cannot escape the
/// anchors and a partial match never counts.
fn full_match(pattern: &str, text: &str) -> bool {
    Regex::new(&format!(r"\A(?:{pattern})\z")).is_ok_and(|anchored| anchored.is_match(text))
}

/// Whether a `screen`-filtered `press_enter` rule may confirm the dialog on
/// screen. Two independent readings must agree: the command the agent's
/// hook reported for its pending shell call, and the command read from the
/// recognized dialog. Only a single-line command qualifies, compared and
/// matched exactly as the hook reported it: in shell source, blank lines,
/// line breaks and whitespace can change what runs, so nothing is trimmed
/// or rejoined to make two readings agree. A missing hook command, a
/// multi-line one, an unrecognized dialog, any difference from the screen,
/// or a partial match refuses.
#[must_use]
pub fn approval_matches(raw: &str, hook_command: Option<&str>, pattern: &str) -> bool {
    let Some(hook) = hook_command else {
        return false;
    };
    if hook.is_empty() || hook.contains(['\n', '\r']) || hook.trim() != hook {
        return false;
    }
    let Some(screen) = choice_command(raw) else {
        return false;
    };
    screen == hook && full_match(pattern, hook)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_selected_affirmative_confirmation_rows() {
        for screen in [
            "› 1. Yes, continue\n  2. No, quit\nPress enter to continue",
            "❯ 1. Yes\n  2. No\nEnter to confirm · Esc to cancel",
            "\x1b[1m> Proceed\x1b[0m\n  Cancel\nPress Enter to continue",
        ] {
            assert!(affirmative_choice_visible(screen), "{screen}");
        }
        for screen in [
            "  1. Yes\n› 2. No\nPress enter to continue",
            "› Yes, continue",
            "Yes, proceed\nPress enter to continue",
            "› Enter your reply\nPress enter to continue",
            "› Yesterday's output\nPress enter to continue",
            "› Yes\nPress enter to continue\n› Ask Codex to do anything",
            "› Yes\nPress enter to continue\n›",
        ] {
            assert!(!affirmative_choice_visible(screen), "{screen}");
        }
    }

    #[test]
    fn enter_rules_cannot_bypass_waiting_state_or_type_a_payload() {
        use crate::automation::{
            AutomationAction, AutomationCondition, AutomationEvent, AutomationRule,
        };
        let mut rule = AutomationRule::new(
            "confirm",
            AutomationEvent::WaitingInput,
            AutomationAction::PressEnter,
        );
        rule.validate().unwrap();
        rule.only_if_still = Some(AutomationCondition::Any);
        assert!(rule.validate().is_err());
        rule.only_if_still = None;
        rule.on = AutomationEvent::Error;
        assert!(rule.validate().is_err());
        rule.on = AutomationEvent::WaitingInput;
        rule.text = Some("yes".into());
        assert!(rule.validate().is_err());
    }

    #[test]
    fn a_screen_filter_belongs_to_press_enter_and_must_compile() {
        use crate::automation::{AutomationAction, AutomationEvent, AutomationRule};
        let mut rule = AutomationRule::new(
            "approve-tests",
            AutomationEvent::WaitingInput,
            AutomationAction::PressEnter,
        );
        rule.screen = Some(r"(?m)^npm test$".into());
        rule.validate().unwrap();
        let parsed: AutomationRule = toml::from_str(
            "name = \"approve-tests\"\non = \"waiting_input\"\naction = \"press_enter\"\nscreen = \"npm test\"\n",
        )
        .unwrap();
        assert_eq!(parsed.screen.as_deref(), Some("npm test"));
        for bad in ["[", "  "] {
            rule.screen = Some(bad.into());
            assert!(rule.validate().unwrap_err().contains("screen"), "{bad:?}");
        }
        let mut prompt = AutomationRule::new(
            "nudge",
            AutomationEvent::WaitingInput,
            AutomationAction::SendPrompt,
        );
        prompt.text = Some("continue".into());
        prompt.screen = Some("npm test".into());
        assert!(prompt.validate().unwrap_err().contains("screen"));
    }

    const CODEX_TAIL: &str = "› 1. Yes, proceed (y)\n  2. Yes, and don't ask again for this command (p)\n  3. No, and tell Codex what to do differently (esc)\nPress enter to confirm or esc to cancel";

    fn codex(body: &str) -> String {
        format!("Would you like to run the following command?\n{body}\n{CODEX_TAIL}")
    }

    fn claude(body: &str) -> String {
        format!(
            "╭──────────────────────────────╮\n│ Bash command                 │\n│                              │\n{body}\n│                              │\n│ Do you want to proceed?      │\n│ ❯ 1. Yes                     │\n│   2. No                      │\n╰──────────────────────────────╯\nEnter to confirm · Esc to cancel"
        )
    }

    #[test]
    fn approves_only_when_hook_and_screen_agree_and_the_pattern_covers_it() {
        let screen = format!(
            "earlier output: npm test\n{}",
            codex("  $ cargo test -p muxa")
        );
        assert_eq!(
            choice_command(&screen).as_deref(),
            Some("cargo test -p muxa")
        );
        let hook = Some("cargo test -p muxa");
        assert!(approval_matches(&screen, hook, "cargo test -p muxa"));
        assert!(approval_matches(&screen, hook, r"cargo test( -p \S+)?"));
        assert!(
            !approval_matches(&screen, hook, "cargo test"),
            "partial match"
        );
        assert!(
            !approval_matches(&screen, hook, "x|cargo test"),
            "alternation stays anchored"
        );
        assert!(
            !approval_matches(&screen, None, "cargo test -p muxa"),
            "no hook command"
        );
        assert!(
            !approval_matches(&screen, Some("rm -rf /"), "cargo test -p muxa"),
            "hook disagrees"
        );

        let boxed = claude(
            "│   npm test                   │\n│   \x1b[2mRun the test suite\x1b[22m         │",
        );
        assert_eq!(choice_command(&boxed).as_deref(), Some("npm test"));
        assert!(approval_matches(&boxed, Some("npm test"), "npm test"));
        assert!(!approval_matches(
            &boxed,
            Some("npm test"),
            "Run the test suite"
        ));
    }

    #[test]
    fn fails_closed_on_anything_but_a_complete_known_dialog() {
        // Earlier output containing the allowed text.
        let earlier = format!("npm test\n{}", codex("  $ rm -rf /tmp/private"));
        assert!(!approval_matches(&earlier, Some("npm test"), "npm test"));
        assert!(!approval_matches(
            &earlier,
            Some("rm -rf /tmp/private"),
            "npm test"
        ));
        // No heading.
        assert!(choice_command(&format!("npm test\n{CODEX_TAIL}")).is_none());
        // A line between the command and the options, even one that looks like an option.
        assert!(choice_command(&codex("  $ npm test\n  1. ; rm -rf /tmp/private")).is_none());
        assert!(choice_command(&codex("  $ npm test\n  rm -rf /")).is_none());
        // Heading too far above the options.
        let mut long = String::from("  $ npm test");
        for _ in 0..14 {
            long.push_str("\n  # filler");
        }
        assert!(choice_command(&codex(&long)).is_none());
        // A fake heading inside a long command: two headings on the capture.
        let mut fake = String::from("│ rm -rf /tmp/private\n");
        for _ in 0..15 {
            fake.push_str("│ # filler\n");
        }
        fake.push_str("│ Bash command\n│ npm test\n│ \x1b[2mRun tests\x1b[22m");
        let fake = claude(&fake);
        assert!(choice_command(&fake).is_none());
        // Even if the real heading had scrolled away, the hook's command differs.
        assert!(!approval_matches(
            &fake,
            Some("rm -rf /tmp/private\n# filler\nBash command\nnpm test"),
            "npm test"
        ));
        // A Claude multi-line command with no faint description is the whole body.
        let multi = claude("│   npm test                   │\n│   rm -rf /                   │");
        assert_eq!(
            choice_command(&multi).as_deref(),
            Some("npm test\nrm -rf /")
        );
        assert!(!approval_matches(
            &multi,
            Some("npm test\nrm -rf /"),
            "npm test"
        ));
        // A non-faint line after the faint description.
        let mixed = claude("│   npm test\n│   \x1b[2mRun tests\x1b[22m\n│   rm -rf /");
        assert!(choice_command(&mixed).is_none());
        // A dialog whose Yes is not the first option.
        let second = "Would you like to run the following command?\n  $ npm test\n  1. No\n› 2. Yes\nPress enter to confirm";
        assert!(choice_command(second).is_none());
    }

    #[test]
    fn a_wrapped_or_unlisted_label_is_not_a_one_time_yes() {
        for screen in [
            codex_with_selected("› 2. Yes, and don't\n     ask again for commands that start with npm\n  3. No (esc)"),
            codex_with_selected("› 1. Yes\n     Always allow this command for this session\n  2. No"),
            codex_with_selected("› 1. Yes\n     (and don't ask again)\n  2. No"),
            "› 1. Yes, and don\u{2019}t ask again\n  2. No\nPress enter to confirm".to_string(),
            "› 1. Yes, allow\n  2. No\nPress enter to confirm".to_string(),
            "› 1. Yes\n  3. No\nPress enter to confirm".to_string(),
            "> Proceed\n  Always\nPress enter to continue".to_string(),
        ] {
            assert!(!affirmative_choice_visible(&screen), "{screen}");
            assert!(!approval_matches(&screen, Some("npm test"), "npm test"), "{screen}");
        }
    }

    fn codex_with_selected(options: &str) -> String {
        format!("Would you like to run the following command?\n  $ npm test\n{options}\nPress enter to confirm or esc to cancel")
    }

    #[test]
    fn a_literal_escaped_like_the_app_matches_only_itself() {
        let check = |command: &str, pattern: &str| {
            approval_matches(&codex(&format!("  $ {command}")), Some(command), pattern)
        };
        // What FleetApprovalRules.escape produces for `rm -rf ./b (x)` and `file.\u{301}`.
        assert!(check("rm -rf ./b (x)", r"rm \-rf \./b \(x\)"));
        assert!(!check("rm -rf ./bb(x)", r"rm \-rf \./b \(x\)"));
        assert!(check("file.\u{301}", "file\\.\u{301}"));
        assert!(!check("fileX\u{301}", "file\\.\u{301}"));
    }

    #[test]
    fn codex_shell_argv_reads_as_the_dialog_shows_it() {
        use crate::adapters::codex::shell_command;
        let read = |value: serde_json::Value| {
            shell_command(Some(&serde_json::json!({ "command": value })))
        };
        assert_eq!(
            read(serde_json::json!("npm test")).as_deref(),
            Some("npm test")
        );
        assert_eq!(
            read(serde_json::json!(["bash", "-lc", "npm test"])).as_deref(),
            Some("npm test")
        );
        assert_eq!(
            read(serde_json::json!(["/bin/zsh", "-c", "ls"])).as_deref(),
            Some("ls")
        );
        assert_eq!(
            read(serde_json::json!(["git", "status"])).as_deref(),
            Some("git status")
        );
        assert_eq!(read(serde_json::json!([1, 2])), None);
        assert_eq!(shell_command(None), None);
    }

    #[test]
    fn only_a_single_line_command_exactly_as_reported_can_be_approved() {
        // Backslash-newline continues `echo`; a blank line after it ends the
        // command, so `rm` runs. The two must never compare equal.
        let approved = "echo safe \\\nrm -rf /tmp/private";
        let actual = "echo safe \\\n\nrm -rf /tmp/private";
        let screen = claude("│   echo safe \\\n│\n│   rm -rf /tmp/private");
        let literal = regex::escape(approved);
        assert!(!approval_matches(&screen, Some(actual), &literal));
        assert!(!approval_matches(&screen, Some(approved), &literal));
        // Quoted multi-line text is multi-line too.
        let quoted = claude("│   echo \"a\n│   b\"");
        assert!(!approval_matches(&quoted, Some("echo \"a\n  b\""), ".*"));
        // Whitespace is never trimmed to make a match.
        let padded = codex("  $ npm test");
        assert!(!approval_matches(&padded, Some(" npm test"), "npm test"));
        assert!(!approval_matches(&padded, Some("npm test "), "npm test"));
        assert!(!approval_matches(&padded, Some("npm test\r"), "npm test"));
        assert!(approval_matches(&padded, Some("npm test"), "npm test"));
    }
}
