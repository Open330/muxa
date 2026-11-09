//! Recognize an affirmative choice before a disabled-by-default Enter action.

use std::sync::LazyLock;

use regex::Regex;

static CURSOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[\t ]*(?:│[\t ]*)?[>›❯](?:[\t ].*)?$").expect("static cursor regex")
});
static AFFIRMATIVE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^[\t ]*(?:│[\t ]*)?[>›❯][\t ]*(?:\d+[.)][\t ]*)?(?:yes|proceed)\b")
        .expect("static affirmative choice regex")
});
static FOOTER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:press enter|enter to (?:confirm|select|continue)|esc(?:ape)? to cancel)\b",
    )
    .expect("static confirmation footer regex")
});

/// An option that grants more than this one request. Automation confirms a
/// single request only, so these are never pressed even when highlighted.
static PERSISTENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:don'?t ask again|do not ask again|always|remember|for (?:this|the) session|during this session|all (?:future|edits|commands|tools))\b",
    )
    .expect("static persistent grant regex")
});
/// A dialog's top edge: a box corner or a horizontal rule.
static BOUNDARY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[\t ]*(?:[╭┌].*|[─━═\-]{6,}.*)$").expect("static dialog boundary regex")
});

/// How far above the selected row the request text may start.
const DIALOG_LINES: usize = 14;

/// The selected row's index in `screen`, when the screen is a confirmation
/// choice: one highlighted affirmative row for this request only, and a
/// confirmation footer.
fn selected_affirmative_row(screen: &str) -> Option<usize> {
    if !FOOTER.is_match(screen) {
        return None;
    }
    let mut cursors = screen
        .lines()
        .enumerate()
        .filter(|(_, line)| CURSOR.is_match(line));
    let (index, selected) = cursors.next()?;
    if cursors.next().is_some() || !AFFIRMATIVE.is_match(selected) || PERSISTENT.is_match(selected)
    {
        return None;
    }
    Some(index)
}

/// Require one highlighted affirmative row and a confirmation footer on the
/// newest screen. Plain prose, No, free-text composers, ambiguous cursors and
/// options that grant more than this request ("don't ask again") are not
/// confirmation choices. ANSI styling is removed before matching.
#[must_use]
pub fn affirmative_choice_visible(raw: &str) -> bool {
    let screen = crate::screen::prepare_capture(raw, 30);
    selected_affirmative_row(&screen).is_some()
}

/// The request a confirmation choice asks about: the lines above the
/// selected Yes, up to the dialog's top edge (a box corner or a rule) and at
/// most [`DIALOG_LINES`] lines, with box sides removed. `None` unless the
/// screen is a confirmation choice. Output printed before the dialog stays
/// outside, so a `screen` rule cannot match an earlier command.
#[must_use]
pub fn choice_dialog_text(raw: &str) -> Option<String> {
    let screen = crate::screen::prepare_capture(raw, 30);
    let selected = selected_affirmative_row(&screen)?;
    let lines: Vec<&str> = screen.lines().collect();
    let mut start = selected.saturating_sub(DIALOG_LINES);
    for index in (start..selected).rev() {
        if BOUNDARY.is_match(lines[index]) {
            start = index + 1;
            break;
        }
    }
    let text = lines[start..selected]
        .iter()
        .map(|line| line.trim().trim_matches('│').trim_matches('|').trim())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    Some(text)
}

/// Whether a confirmation choice is on screen and its request matches the
/// rule's `screen` pattern.
#[must_use]
pub fn choice_dialog_matches(raw: &str, pattern: &Regex) -> bool {
    choice_dialog_text(raw).is_some_and(|text| pattern.is_match(&text))
}

/// [`choice_dialog_matches`] for a rule's pattern text; an invalid pattern
/// never matches (validation refuses one before it is saved).
#[must_use]
pub fn choice_dialog_matches_pattern(raw: &str, pattern: &str) -> bool {
    Regex::new(pattern).is_ok_and(|pattern| choice_dialog_matches(raw, &pattern))
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
            assert!(choice_dialog_text(screen).is_none(), "{screen}");
        }
    }

    #[test]
    fn dialog_text_is_the_request_above_the_selected_yes() {
        let claude = "earlier: ran rm -rf build\n\
╭──────────────────────────────╮\n\
│ Bash command                 │\n\
│                              │\n\
│   npm test                   │\n\
│   Run the test suite         │\n\
│                              │\n\
│ Do you want to proceed?      │\n\
│ ❯ 1. Yes                     │\n\
│   2. No                      │\n\
╰──────────────────────────────╯\n\
Enter to confirm · Esc to cancel";
        let text = choice_dialog_text(claude).unwrap();
        assert!(text.contains("npm test"), "{text}");
        assert!(!text.contains("rm -rf"), "{text}");
        let npm = Regex::new(r"(?m)^npm (test|run lint)$").unwrap();
        assert!(choice_dialog_matches(claude, &npm));
        assert!(!choice_dialog_matches(
            claude,
            &Regex::new("rm -rf").unwrap()
        ));

        let codex = "Would you like to run the following command?\n  $ cargo test -p muxa\n› 1. Yes, proceed (y)\n  2. Yes, and don't ask again for this command (p)\n  3. No (esc)\nPress enter to confirm or esc to cancel";
        assert!(choice_dialog_matches(
            codex,
            &Regex::new(r"\$ cargo test\b").unwrap()
        ));

        // Not a confirmation: nothing to match, even if the text is there.
        assert!(!choice_dialog_matches("npm test\n› Ask anything", &npm));
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
}
