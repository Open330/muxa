//! Live and printable Muxa onboarding.
//!
//! Interactive onboarding runs real tmux and Muxa components in a throwaway
//! sandbox. `--print` describes the same fifteen actions without starting it.

mod live;

use anyhow::Result;

#[derive(Debug, Clone, clap::Args, Default)]
pub struct Args {
    /// Print the complete guide without starting the live sandbox.
    #[arg(long)]
    pub print: bool,
    /// Offer the live tour's skip key from the first step.
    #[arg(long)]
    pub no_quiz: bool,
    /// Compatibility alias; onboarding always includes tmux and Muxa.
    #[arg(long, hide = true)]
    pub tmux: bool,
    /// Which tour to run. Only the live sandbox remains.
    #[arg(long, value_enum, default_value_t)]
    pub tour: Tour,
    /// Display language: auto, en, or ko. / 표시 언어: auto, en, ko.
    #[arg(long, value_enum, default_value_t)]
    pub lang: Language,
}

/// Which onboarding to run.
///
/// `--tour live` remains accepted for scripts written while both tours existed;
/// the simulation is no longer a valid value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Tour {
    /// Run Muxa against a sandbox on its own tmux server.
    #[default]
    Live,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Language {
    /// Detect Korean from `LC_ALL`, `LC_MESSAGES`, or `LANG`.
    #[default]
    Auto,
    /// English.
    En,
    /// 한국어.
    Ko,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiLanguage {
    En,
    Ko,
}

impl Language {
    fn resolve(self) -> UiLanguage {
        match self {
            Self::En => UiLanguage::En,
            Self::Ko => UiLanguage::Ko,
            Self::Auto => ["LC_ALL", "LC_MESSAGES", "LANG"]
                .into_iter()
                .find_map(|key| std::env::var(key).ok().filter(|value| !value.is_empty()))
                .map_or(UiLanguage::En, |locale| language_from_locale(&locale)),
        }
    }
}

fn language_from_locale(locale: &str) -> UiLanguage {
    let locale = locale.trim().to_ascii_lowercase();
    if locale == "ko"
        || locale.starts_with("ko_")
        || locale.starts_with("ko-")
        || locale.starts_with("ko.")
    {
        UiLanguage::Ko
    } else {
        UiLanguage::En
    }
}

fn tr(language: UiLanguage, en: &'static str, ko: &'static str) -> &'static str {
    match language {
        UiLanguage::En => en,
        UiLanguage::Ko => ko,
    }
}

pub fn run(args: Args) -> Result<()> {
    let language = args.lang.resolve();
    if args.print {
        print!("{}", printable_guide(language));
        return Ok(());
    }

    match args.tour {
        Tour::Live => live::run(language, args.no_quiz),
    }
}

fn printable_guide(language: UiLanguage) -> String {
    use std::fmt::Write as _;

    let mut guide = String::new();
    let _ = writeln!(
        guide,
        "{}",
        match language {
            UiLanguage::En => format!("Muxa live onboarding · {} steps", live::step_count()),
            UiLanguage::Ko => format!("Muxa 라이브 온보딩 · {}단계", live::step_count()),
        }
    );
    let _ = writeln!(guide, "================================");
    let _ = writeln!(guide);
    let _ = writeln!(
        guide,
        "{}",
        tr(
            language,
            "The interactive tour uses a private tmux server, muxad, and mailbox in a throwaway sandbox.",
            "interactive tour는 일회용 sandbox 안의 전용 tmux server, muxad, mailbox를 사용합니다."
        )
    );
    let _ = writeln!(
        guide,
        "{}",
        tr(
            language,
            "It installs nothing and removes the sandbox on every exit path.",
            "아무것도 설치하지 않으며 어떤 종료 경로에서도 sandbox를 삭제합니다."
        )
    );
    let _ = writeln!(guide);

    // Walked from the tour's own steps rather than kept beside them. The
    // simulation had a hand-written copy of its curriculum and a whole parity
    // harness to hold the two together; a second copy here drifted the moment
    // a step was inserted — the guide claimed fifteen while the tour ran
    // sixteen, and its test asserted the stale number so nothing failed.
    for (index, step) in live::steps().iter().enumerate() {
        let _ = writeln!(
            guide,
            "{:>2}. {}",
            index + 1,
            tr(language, step.title_en, step.title_ko)
        );
        let _ = writeln!(guide, "    {}", tr(language, step.cue_en, step.cue_ko));
    }

    let _ = writeln!(guide);
    let _ = writeln!(
        guide,
        "{}",
        tr(
            language,
            "During the live tour: F2 switches language; F12 skips a stuck step after it is offered.",
            "라이브 tour 중 F2는 언어를 바꾸고, 안내된 뒤 F12를 누르면 막힌 단계를 건너뜁니다."
        )
    );
    let _ = writeln!(
        guide,
        "{}",
        tr(
            language,
            "Run `muxa onboard --no-quiz` to offer F12 from the first step.",
            "처음부터 F12를 표시하려면 `muxa onboard --no-quiz`를 실행하세요."
        )
    );
    guide
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered_steps(guide: &str) -> Vec<&str> {
        guide
            .lines()
            .filter(|line| {
                let line = line.trim_start();
                line.split_once(". ")
                    .is_some_and(|(number, _)| number.parse::<usize>().is_ok())
            })
            .collect()
    }

    #[test]
    fn locale_detection_keeps_live_tour_language_selection() {
        assert_eq!(language_from_locale("ko_KR.UTF-8"), UiLanguage::Ko);
        assert_eq!(language_from_locale("ko-KR"), UiLanguage::Ko);
        assert_eq!(language_from_locale("ko"), UiLanguage::Ko);
        assert_eq!(language_from_locale("C.UTF-8"), UiLanguage::En);
        assert_eq!(language_from_locale("en_US.UTF-8"), UiLanguage::En);
    }

    #[test]
    fn printable_guide_lists_exactly_the_tours_steps_in_both_languages() {
        for language in [UiLanguage::En, UiLanguage::Ko] {
            let guide = printable_guide(language);
            let steps = numbered_steps(&guide);
            // Against the tour, not against a number: a hand-kept count went
            // stale the first time a step was inserted, and its test passed.
            assert_eq!(steps.len(), live::step_count());
            assert!(steps[0].starts_with(" 1."));
            assert!(
                guide.contains(&format!("{} steps", live::step_count()))
                    || guide.contains(&format!("{}단계", live::step_count()))
            );
        }
    }

    #[test]
    fn printable_guide_teaches_live_commands_not_simulated_watch_keys() {
        let english = printable_guide(UiLanguage::En);
        for command in [
            "tmux new-session -s muxa-onboarding",
            "tmux ls",
            "tmux attach -t muxa-onboarding",
            "claude",
            "muxa watch",
            "muxa attend",
            "muxa msg send @claude",
            "muxa msg list",
            "muxa msg inbox",
        ] {
            assert!(english.contains(command), "missing live action: {command}");
        }
        for retired in ["20 steps", "Alt-T", "new-work form", "simulation"] {
            assert!(
                !english.contains(retired),
                "retired lesson remains: {retired}"
            );
        }

        let korean = printable_guide(UiLanguage::Ko);
        assert!(korean.contains(&format!("{}단계", live::step_count())));
        assert!(korean.contains("어디까지 됐나요?"));
    }
}
