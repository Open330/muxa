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

#[derive(Debug, Clone, Copy)]
struct PrintableStep {
    meaning_en: &'static str,
    meaning_ko: &'static str,
    action_en: &'static str,
    action_ko: &'static str,
}

/// Written counterpart of `live::STEPS`: fifteen one-action steps in the same
/// order. It deliberately explains effects instead of reproducing a terminal.
const PRINTABLE_STEPS: &[PrintableStep] = &[
    PrintableStep {
        meaning_en: "Create a tmux session: a workspace that keeps running without you.",
        meaning_ko: "당신 없이도 계속 실행되는 workspace인 tmux session을 만듭니다.",
        action_en: "type:  tmux new-session -s muxa-onboarding",
        action_ko: "입력:  tmux new-session -s muxa-onboarding",
    },
    PrintableStep {
        meaning_en: "Create a second window. In Muxa, one window is one Work.",
        meaning_ko: "두 번째 window를 만듭니다. Muxa에서 window 하나는 Work 하나입니다.",
        action_en: "press: Ctrl-b, then c",
        action_ko: "입력:  Ctrl-b를 눌렀다 떼고 c",
    },
    PrintableStep {
        meaning_en: "Open the tree to see sessions and their windows, then close it.",
        meaning_ko: "tree를 열어 session과 그 안의 window를 확인한 뒤 닫습니다.",
        action_en: "press: Ctrl-b, then s; q closes the tree",
        action_ko: "입력:  Ctrl-b를 눌렀다 떼고 s; q로 tree 닫기",
    },
    PrintableStep {
        meaning_en: "Detach. The client leaves, while the session and work keep running.",
        meaning_ko: "detach합니다. client만 나가고 session과 작업은 계속 실행됩니다.",
        action_en: "press: Ctrl-b, then d",
        action_ko: "입력:  Ctrl-b를 눌렀다 떼고 d",
    },
    PrintableStep {
        meaning_en: "Confirm from your shell that the detached session is still running.",
        meaning_ko: "원래 shell에서 detach한 session이 계속 실행 중인지 확인합니다.",
        action_en: "type:  tmux ls",
        action_ko: "입력:  tmux ls",
    },
    PrintableStep {
        meaning_en: "Attach again; every window and pane is still there.",
        meaning_ko: "다시 attach합니다. 모든 window와 pane이 그대로 남아 있습니다.",
        action_en: "type:  tmux attach -t muxa-onboarding",
        action_ko: "입력:  tmux attach -t muxa-onboarding",
    },
    PrintableStep {
        meaning_en: "Split the window. In Muxa, one pane is one agent.",
        meaning_ko: "window를 나눕니다. Muxa에서 pane 하나는 agent 하나입니다.",
        action_en: "press: Ctrl-b, then % (\" splits top and bottom)",
        action_ko: "입력:  Ctrl-b를 눌렀다 떼고 % (\"는 상하 분할)",
    },
    PrintableStep {
        meaning_en: "Start an agent in the new pane. The sandbox supplies a safe CLI stand-in.",
        meaning_ko: "새 pane에서 agent를 시작합니다. sandbox가 안전한 CLI 대역을 제공합니다.",
        action_en: "type:  claude",
        action_ko: "입력:  claude",
    },
    PrintableStep {
        meaning_en: "See the whole checkout Work and its agents in the real Muxa watch.",
        meaning_ko: "실제 Muxa watch에서 checkout Work와 agent 전체를 확인합니다.",
        action_en: "run:   muxa watch (q leaves it)",
        action_ko: "입력:  muxa watch (q로 나가기)",
    },
    PrintableStep {
        meaning_en: "Jump to the agent that has been blocked longest.",
        meaning_ko: "가장 오래 막혀 있는 agent로 이동합니다.",
        action_en: "run:   muxa attend",
        action_ko: "입력:  muxa attend",
    },
    PrintableStep {
        meaning_en: "Return from the agent pane to the pane you were using before.",
        meaning_ko: "agent pane에서 직전에 사용하던 자신의 pane으로 돌아갑니다.",
        action_en: "press: Ctrl-b, then ; (Ctrl-b o cycles instead)",
        action_ko: "입력:  Ctrl-b를 눌렀다 떼고 ; (Ctrl-b o는 순환)",
    },
    PrintableStep {
        meaning_en: "Ask an agent a question without attaching to its pane.",
        meaning_ko: "agent pane에 attach하지 않고 질문을 보냅니다.",
        action_en: "run:   muxa msg send @claude \"how far along?\"",
        action_ko: "입력:  muxa msg send @claude \"어디까지 됐나요?\"",
    },
    PrintableStep {
        meaning_en: "List what you sent and the reply that came back.",
        meaning_ko: "보낸 메시지와 돌아온 답장을 함께 확인합니다.",
        action_en: "run:   muxa msg list",
        action_ko: "입력:  muxa msg list",
    },
    PrintableStep {
        meaning_en: "Claim requests addressed to you. Agents use the same Muxa mailbox.",
        meaning_ko: "당신 앞으로 온 요청을 가져옵니다. agent도 같은 Muxa mailbox를 사용합니다.",
        action_en: "run:   muxa msg inbox",
        action_ko: "입력:  muxa msg inbox",
    },
    PrintableStep {
        meaning_en: "Finish: session is a workspace, window is a Work, pane is an agent.",
        meaning_ko: "완료: session은 workspace, window는 Work, pane은 agent입니다.",
        action_en: "press: Ctrl-b, then d; the tour deletes the sandbox",
        action_ko: "입력:  Ctrl-b를 눌렀다 떼고 d; tour가 sandbox 삭제",
    },
];

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
        tr(
            language,
            "Muxa live onboarding · 15 steps",
            "Muxa 라이브 온보딩 · 15단계"
        )
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

    for (index, step) in PRINTABLE_STEPS.iter().enumerate() {
        let _ = writeln!(
            guide,
            "{:>2}. {}",
            index + 1,
            tr(language, step.meaning_en, step.meaning_ko)
        );
        let _ = writeln!(
            guide,
            "    {}",
            tr(language, step.action_en, step.action_ko)
        );
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
    fn printable_guide_has_the_live_tours_fifteen_steps_in_both_languages() {
        for language in [UiLanguage::En, UiLanguage::Ko] {
            let guide = printable_guide(language);
            let steps = numbered_steps(&guide);
            assert_eq!(steps.len(), 15);
            assert!(steps[0].starts_with(" 1."));
            assert!(steps[14].starts_with("15."));
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
        assert!(korean.contains("15단계"));
        assert!(korean.contains("어디까지 됐나요?"));
    }
}
