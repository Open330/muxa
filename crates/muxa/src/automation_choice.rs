//! Recognize an affirmative choice before a disabled-by-default Enter action.

use std::sync::LazyLock;

use regex::Regex;

static CURSOR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\t ]*[>›❯](?:[\t ].*)?$").expect("static cursor regex"));
static AFFIRMATIVE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^[\t ]*[>›❯][\t ]*(?:\d+[.)][\t ]*)?(?:yes|proceed)\b")
        .expect("static affirmative choice regex")
});
static FOOTER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:press enter|enter to (?:confirm|select|continue)|esc(?:ape)? to cancel)\b",
    )
    .expect("static confirmation footer regex")
});

/// Require one highlighted affirmative row and a confirmation footer on the
/// newest screen. Plain prose, No, free-text composers and ambiguous cursors
/// are not confirmation choices. ANSI styling is removed before matching.
#[must_use]
pub fn affirmative_choice_visible(raw: &str) -> bool {
    let screen = crate::screen::prepare_capture(raw, 30);
    if !FOOTER.is_match(&screen) {
        return false;
    }
    let mut cursors = screen.lines().filter(|line| CURSOR.is_match(line));
    let Some(selected) = cursors.next() else {
        return false;
    };
    cursors.next().is_none() && AFFIRMATIVE.is_match(selected)
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
}
