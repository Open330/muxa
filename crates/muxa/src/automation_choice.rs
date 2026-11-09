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
/// Any option row, selected or not: `1. …`, `› 2) …`.
static OPTION_ROW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[\t ]*(?:│[\t ]*)?(?:[>›❯][\t ]*)?\d+[.)][\t ]").expect("static option row regex")
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

/// The selected row's index, when the screen is a confirmation choice: a
/// footer, exactly one highlighted row, a label that is a one-time Yes, and
/// no wrapped continuation of that label on the next line.
fn selected_affirmative_row(visible: &[String]) -> Option<usize> {
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
    // A label that wrapped continues on the next line, indented and in
    // lower case ("ask again for …"); refuse it rather than guess.
    if let Some(next) = visible.get(index + 1) {
        let inner = next.trim_start_matches([' ', '\t', '│']);
        if !OPTION_ROW.is_match(next)
            && inner.len() < next.len()
            && inner.chars().next().is_some_and(char::is_lowercase)
        {
            return None;
        }
    }
    Some(index)
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
    let selected = selected_affirmative_row(&visible)?;
    let start = selected.saturating_sub(DIALOG_LINES);
    let headers: Vec<usize> = (start..selected)
        .filter(|&index| {
            let text = content(&visible[index]);
            CODEX_COMMAND_HEADER.is_match(text) || CLAUDE_COMMAND_HEADER.is_match(text)
        })
        .collect();
    let [header] = headers[..] else {
        return None;
    };
    // A box that closed between the heading and the options is another box.
    if visible[header + 1..selected]
        .iter()
        .any(|line| line.trim_start().starts_with(['╰', '└']))
    {
        return None;
    }
    if CODEX_COMMAND_HEADER.is_match(content(&visible[header])) {
        let body: Vec<usize> = (header + 1..selected)
            .filter(|&index| !content(&visible[index]).is_empty())
            .collect();
        let [command_line, after @ ..] = &body[..] else {
            return None;
        };
        let command = content(&visible[*command_line]).strip_prefix("$ ")?;
        // Nothing between the command and the options: no second line of
        // the command, no extra text.
        if after
            .iter()
            .any(|&index| !OPTION_ROW.is_match(&visible[index]))
        {
            return None;
        }
        return (!command.trim().is_empty()).then(|| command.trim().to_string());
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

/// Whether the screen is a confirmation of a shell command that the rule's
/// `screen` pattern matches in full: the pattern must cover the whole
/// command, every line of it, not a part.
#[must_use]
pub fn choice_dialog_matches(raw: &str, pattern: &Regex) -> bool {
    choice_command(raw).is_some_and(|command| full_match(pattern.as_str(), &command))
}

/// `^(?:pattern)$` over the whole text, so alternation cannot escape the
/// anchors and a partial match never counts.
fn full_match(pattern: &str, text: &str) -> bool {
    Regex::new(&format!(r"\A(?:{pattern})\z")).is_ok_and(|anchored| anchored.is_match(text))
}

/// [`choice_dialog_matches`] for a rule's pattern text; an invalid pattern
/// never matches (validation refuses one before it is saved).
#[must_use]
pub fn choice_dialog_matches_pattern(raw: &str, pattern: &str) -> bool {
    choice_command(raw).is_some_and(|command| full_match(pattern, &command))
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
    fn never_presses_an_option_that_grants_more_than_this_request() {
        for screen in [
            "  1. Yes\n❯ 2. Yes, and don't ask again for npm test commands\n  3. No\nEnter to confirm · Esc to cancel",
            "› Yes, allow all edits during this session\n  No\nPress enter to confirm or esc to cancel",
            "› Yes, always allow\n  No\nPress enter to confirm",
        ] {
            assert!(!affirmative_choice_visible(screen), "{screen}");
            assert!(choice_command(screen).is_none(), "{screen}");
        }
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

    #[test]
    fn reads_only_the_command_of_a_known_dialog() {
        let codex = "earlier output: npm test\nWould you like to run the following command?\n  $ cargo test -p muxa\n› 1. Yes, proceed (y)\n  2. Yes, and don't ask again for this command (p)\n  3. No (esc)\nPress enter to confirm or esc to cancel";
        assert_eq!(choice_command(codex).as_deref(), Some("cargo test -p muxa"));
        assert!(choice_dialog_matches_pattern(codex, r"cargo test -p muxa"));
        assert!(choice_dialog_matches_pattern(
            codex,
            r"cargo test( -p \S+)?"
        ));
        assert!(
            !choice_dialog_matches_pattern(codex, "cargo test"),
            "partial match"
        );
        assert!(
            !choice_dialog_matches_pattern(codex, "npm test"),
            "earlier output"
        );
        assert!(
            !choice_dialog_matches_pattern(codex, "x|cargo test"),
            "alternation stays anchored"
        );

        let claude = "earlier: ran rm -rf build\n\
╭──────────────────────────────╮\n\
│ Bash command                 │\n\
│                              │\n\
│   npm test                   │\n\
│   \x1b[2mRun the test suite\x1b[22m         │\n\
│                              │\n\
│ Do you want to proceed?      │\n\
│ ❯ 1. Yes                     │\n\
│   2. No                      │\n\
╰──────────────────────────────╯\n\
Enter to confirm · Esc to cancel";
        assert_eq!(choice_command(claude).as_deref(), Some("npm test"));
        assert!(choice_dialog_matches_pattern(claude, "npm test"));
        assert!(!choice_dialog_matches_pattern(claude, "Run the test suite"));
    }

    #[test]
    fn fails_closed_on_anything_but_a_complete_known_dialog() {
        // Unboxed Codex dialog with earlier output containing the allowed text.
        let earlier = "npm test\nWould you like to run the following command?\n  $ rm -rf /tmp/private\n› 1. Yes, proceed (y)\n  2. No (esc)\nPress enter to confirm or esc to cancel";
        assert!(!choice_dialog_matches_pattern(earlier, "npm test"));
        // No heading: no request at all.
        let headless = "npm test\n› 1. Yes, proceed (y)\n  2. No (esc)\nPress enter to confirm or esc to cancel";
        assert!(choice_command(headless).is_none());
        // Heading pushed out of the window by a long command.
        let mut long = String::from("Would you like to run the following command?\n  $ rm -rf /\n");
        for _ in 0..15 {
            long.push_str("  && echo filler\n");
        }
        long.push_str("  $ npm test\n› 1. Yes, proceed (y)\n  2. No (esc)\nPress enter to confirm or esc to cancel");
        assert!(choice_command(&long).is_none());
        // A second command line under the heading.
        let multi = "Would you like to run the following command?\n  $ npm test\n  rm -rf /\n› 1. Yes, proceed (y)\nPress enter to confirm or esc to cancel";
        assert!(choice_command(multi).is_none());
        // Two headings (an injected one in agent text) are ambiguous.
        let injected = "Bash command\n  rm -rf /\n  \x1b[2mBash command\x1b[22m\nDo you want to proceed?\n❯ 1. Yes\n  2. No\nEnter to confirm · Esc to cancel";
        assert!(choice_command(injected).is_none());
        // A Claude multi-line command with no faint description is the whole body.
        let claude_multi = "Bash command\n  npm test\n  rm -rf /\nDo you want to proceed?\n❯ 1. Yes\n  2. No\nEnter to confirm · Esc to cancel";
        assert_eq!(
            choice_command(claude_multi).as_deref(),
            Some("npm test\nrm -rf /")
        );
        assert!(!choice_dialog_matches_pattern(claude_multi, "npm test"));
        // A description that is not faint after a faint one is refused.
        let mixed = "Bash command\n  npm test\n  \x1b[2mRun tests\x1b[22m\n  rm -rf /\nDo you want to proceed?\n❯ 1. Yes\n  2. No\nEnter to confirm · Esc to cancel";
        assert!(choice_command(mixed).is_none());
    }

    #[test]
    fn a_wrapped_or_unlisted_label_is_not_a_one_time_yes() {
        for screen in [
            "Would you like to run the following command?\n  $ npm test\n› 2. Yes, and don't\n     ask again for commands that start with npm\n  3. No (esc)\nPress enter to confirm or esc to cancel",
            "› 1. Yes, and don\u{2019}t ask again\n  2. No\nPress enter to confirm",
            "› 1. Yes\n    and remember this choice\n  2. No\nPress enter to confirm",
            "› 1. Yes, allow\n  2. No\nPress enter to confirm",
        ] {
            assert!(!affirmative_choice_visible(screen), "{screen}");
        }
    }

    #[test]
    fn a_literal_escaped_like_the_app_matches_only_itself() {
        let screen = |command: &str| {
            format!("Would you like to run the following command?\n  $ {command}\n› 1. Yes, proceed (y)\n  2. No (esc)\nPress enter to confirm or esc to cancel")
        };
        // What FleetApprovalRules.escape produces for `rm -rf ./b (x)` and `file.\u{301}`.
        let literal = r"rm \-rf \./b \(x\)";
        assert!(choice_dialog_matches_pattern(
            &screen("rm -rf ./b (x)"),
            literal
        ));
        assert!(!choice_dialog_matches_pattern(
            &screen("rm -rf ./bb(x)"),
            literal
        ));
        let accented = "file\\.\u{301}";
        assert!(choice_dialog_matches_pattern(
            &screen("file.\u{301}"),
            accented
        ));
        assert!(!choice_dialog_matches_pattern(
            &screen("fileX\u{301}"),
            accented
        ));
    }
}
