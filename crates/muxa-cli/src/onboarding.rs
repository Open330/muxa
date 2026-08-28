//! Fullscreen and printable Muxa onboarding.
//!
//! Interactive onboarding is one inert shell → tmux → Muxa scenario. It first
//! preserves virtual tmux command and layout effects, then hands the same
//! fullscreen terminal to a stable `muxa watch` mock with location-aware
//! dialogs. `--print` remains available for scripts and accessibility.

mod live;
mod tmux;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use muxa::AgentState;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::io::{self, IsTerminal, Stdout, Write};
use std::time::Duration;

#[derive(Debug, Clone, clap::Args, Default)]
pub struct Args {
    /// Print the complete guide without interactive prompts.
    #[arg(long)]
    pub print: bool,
    /// Skip hands-on shortcut gates while keeping every visual explanation.
    #[arg(long)]
    pub no_quiz: bool,
    /// Compatibility alias; onboarding now always includes tmux and Muxa.
    #[arg(long, hide = true)]
    pub tmux: bool,
    /// Display language: auto, en, or ko. / 표시 언어: auto, en, ko.
    #[arg(long, value_enum, default_value_t)]
    pub lang: Language,
    /// Which tour to run: the built-in simulation, or a throwaway muxa.
    #[arg(long, value_enum, default_value_t)]
    pub tour: Tour,
    /// Machine-readable dump for tooling, printed instead of the tour.
    #[arg(long, value_enum, hide = true)]
    pub emit: Option<Emit>,
}

/// Which onboarding to run.
///
/// A value rather than a `--live` flag because `Args` is already at clippy's
/// bool ceiling, and because the default is the thing that moves once the live
/// tour covers both acts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Tour {
    /// Draw the scenario; change nothing, need nothing.
    #[default]
    Simulated,
    /// Run the real muxa against a sandbox on its own tmux server.
    Live,
}

/// Machine-readable dumps `muxa onboard` can print instead of running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Emit {
    /// The key each of the twenty steps waits for, as TSV.
    StepTable,
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

const ACTION_COLOR: Color = Color::LightYellow;

fn action_style() -> Style {
    Style::default()
        .fg(ACTION_COLOR)
        .add_modifier(Modifier::BOLD)
}

fn action_line(text: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(text.into(), action_style()))
}

fn highlighted_actions(text: impl Into<String>, tokens: &[&str]) -> Line<'static> {
    let text = text.into();
    let mut spans = Vec::new();
    let mut cursor = 0;

    while cursor < text.len() {
        let mut next = None;
        for token in tokens.iter().copied().filter(|token| !token.is_empty()) {
            for (offset, _) in text[cursor..].match_indices(token) {
                let start = cursor + offset;
                let end = start + token.len();
                if token_boundary(&text, start, end) {
                    if next.is_none_or(|(best_start, best_end, _)| {
                        (start, std::cmp::Reverse(end - start))
                            < (best_start, std::cmp::Reverse(best_end - best_start))
                    }) {
                        next = Some((start, end, token));
                    }
                    break;
                }
            }
        }

        let Some((start, end, token)) = next else {
            spans.push(Span::raw(text[cursor..].to_string()));
            break;
        };
        if start > cursor {
            spans.push(Span::raw(text[cursor..start].to_string()));
        }
        spans.push(Span::styled(token.to_string(), action_style()));
        cursor = end;
    }

    Line::from(spans)
}

fn token_boundary(text: &str, start: usize, end: usize) -> bool {
    let before = text[..start].chars().next_back();
    let after = text[end..].chars().next();
    !before.is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        && !after.is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Interactive,
    Print,
}

impl Mode {
    fn detect(force_print: bool) -> Self {
        if force_print || !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            Self::Print
        } else {
            Self::Interactive
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Section {
    title_en: &'static str,
    body_en: &'static str,
    title_ko: &'static str,
    body_ko: &'static str,
}

const POLICY: &str = "\
session = Workspace binding\n\
window  = current Run binding\n\
pane    = agent session binding\n\
\n\
Muxa owns tmux lifecycle, location, state, and collaboration routing.\n\
Agents own files, code, Git, tests, and reasoning.\n\
A Work is durable, may link external issues, and reuses its current Run window.";

const WORKFLOW: &str = "\
1. Start the work with its first agent:\n\
   muxa work start muxa-onboarding --workspace muxa --cwd /repo --agent codex --role implementer --prompt \"Implement muxa-onboarding\"\n\
\n\
2. Add another agent to the same work:\n\
   muxa agent start --workspace muxa --work muxa-onboarding --agent claude --role reviewer --prompt \"Review the current changes\"\n\
\n\
3. Inspect or operate it:\n\
   muxa workspace list\n\
   muxa work show muxa-onboarding --workspace muxa\n\
   muxa watch\n\
   muxa agent control --pane %42 --action interrupt\n\
\n\
4. Close explicitly when the work is finished:\n\
   muxa work close muxa-onboarding --workspace muxa";

const MCP_PATTERN: &str = "\
Use the existing muxa MCP server; do not add a second tmux MCP.\n\
\n\
muxa_start_agent(workspace=\"muxa\", work=\"muxa-onboarding\", agent=\"codex\", role=\"reviewer\", prompt=\"Review ...\")\n\
muxa_wait_for_change(pane=\"%42\", until=\"settled\", include_capture=true)\n\
muxa_status(pane=\"%42\", include_capture=true, history_limit=1)\n\
muxa_manage_tmux(action=\"interrupt_agent\", pane=\"%42\")\n\
\n\
terminate_agent, close_work, and close_workspace require confirm=true.";

const SAFETY: &str = "\
Muxa deliberately does not expose arbitrary shell or generic tmux commands.\n\
Use exact pane ids or native PTY session ids for agent control. Destructive actions require confirmation.\n\
Use collaboration review + read_only by default; grant execute only with narrow paths.";

const POLICY_KO: &str = "\
session = Workspace binding\n\
window  = current Run binding\n\
pane    = agent session binding\n\
\n\
Muxa는 tmux 생명주기, 위치, 상태, 협업 routing을 관리합니다.\n\
Agent는 파일, 코드, Git, 테스트와 추론을 담당합니다.\n\
Work는 지속되며 외부 이슈를 연결할 수 있고 현재 Run window를 재사용합니다.";

const WORKFLOW_KO: &str = "\
1. 첫 agent와 함께 work 시작:\n\
   muxa work start muxa-onboarding --workspace muxa --cwd /repo --agent codex --role implementer --prompt \"Implement muxa-onboarding\"\n\
\n\
2. 같은 work에 다른 agent 추가:\n\
   muxa agent start --workspace muxa --work muxa-onboarding --agent claude --role reviewer --prompt \"Review the current changes\"\n\
\n\
3. 조회 또는 제어:\n\
   muxa workspace list\n\
   muxa work show muxa-onboarding --workspace muxa\n\
   muxa watch\n\
   muxa agent control --pane %42 --action interrupt\n\
\n\
4. work가 끝나면 명시적으로 닫기:\n\
   muxa work close muxa-onboarding --workspace muxa";

const MCP_PATTERN_KO: &str = "\
별도 tmux MCP를 추가하지 말고 기존 Muxa MCP server를 사용합니다.\n\
\n\
muxa_start_agent(workspace=\"muxa\", work=\"muxa-onboarding\", agent=\"codex\", role=\"reviewer\", prompt=\"Review ...\")\n\
muxa_wait_for_change(pane=\"%42\", until=\"settled\", include_capture=true)\n\
muxa_status(pane=\"%42\", include_capture=true, history_limit=1)\n\
muxa_manage_tmux(action=\"interrupt_agent\", pane=\"%42\")\n\
\n\
terminate_agent, close_work, close_workspace는 confirm=true가 필요합니다.";

const SAFETY_KO: &str = "\
Muxa는 임의 shell 실행이나 범용 tmux 명령을 노출하지 않습니다.\n\
Agent 제어에는 정확한 pane id 또는 native PTY session id를 사용하고 파괴적 동작은 확인을 요구합니다.\n\
협업은 review + read_only를 기본으로 하고 execute는 좁은 경로에만 허용합니다.";

const SECTIONS: &[Section] = &[
    Section {
        title_en: "2 · Mental model",
        body_en: POLICY,
        title_ko: "2 · 운영 모델",
        body_ko: POLICY_KO,
    },
    Section {
        title_en: "3 · Default workflow",
        body_en: WORKFLOW,
        title_ko: "3 · 기본 작업 흐름",
        body_ko: WORKFLOW_KO,
    },
    Section {
        title_en: "4 · Agent-facing MCP pattern",
        body_en: MCP_PATTERN,
        title_ko: "4 · Agent용 MCP 패턴",
        body_ko: MCP_PATTERN_KO,
    },
    Section {
        title_en: "5 · Safety boundary",
        body_en: SAFETY,
        title_ko: "5 · 안전 경계",
        body_ko: SAFETY_KO,
    },
];

pub fn run(args: Args) -> Result<()> {
    if args.emit == Some(Emit::StepTable) {
        print!("{}", step_table_tsv());
        return Ok(());
    }
    apply_icon_preference();
    if args.tour == Tour::Live && !args.print {
        return live::run(args.lang.resolve(), args.no_quiz);
    }
    let mode = Mode::detect(args.print);
    let language = args.lang.resolve();
    match mode {
        Mode::Print => print_guide(language),
        Mode::Interactive => interactive_guide(args.no_quiz, language)?,
    }
    Ok(())
}

/// Onboarding stays available when config parsing fails, but a valid config
/// should still make its state glyphs match live watch (`unicode` vs `ascii`).
fn apply_icon_preference() {
    let path = std::env::var_os("MUXA_CONFIG")
        .map(std::path::PathBuf::from)
        .or_else(muxa::paths::default_config_file);
    if let Ok(config) = muxa::config::Config::load_or_default(path.as_deref()) {
        crate::set_icon_set(config.ui.icons);
    }
}

fn print_guide(language: UiLanguage) {
    println!(
        "{}",
        if language == UiLanguage::Ko {
            "Muxa 통합 온보딩"
        } else {
            "Muxa unified onboarding"
        }
    );
    println!("======================");
    tmux::print_guide(language);
    for section in SECTIONS {
        let (title, body) = if language == UiLanguage::Ko {
            (section.title_ko, section.body_ko)
        } else {
            (section.title_en, section.body_en)
        };
        println!("\n{}\n{}\n", title, "-".repeat(title.chars().count()));
        println!("{body}");
    }
    if language == UiLanguage::Ko {
        println!("\n6 · muxa watch 단축키\n----------------------");
        for line in korean_watch_help() {
            println!("{line}");
        }
        println!("\n다음: muxa watch를 실행하거나 tmux prefix+s를 누르세요.");
    } else {
        println!("\n6 · muxa watch shortcuts\n------------------------");
        for line in crate::watch::help_overlay_text() {
            println!("{line}");
        }
        println!("\nNext: run muxa watch or press tmux prefix+s.");
    }
}

fn korean_watch_help() -> &'static [&'static str] {
    &[
        "이동",
        "  ↑/↓ · j/k       work/agent 이동",
        "  ←/→ · h/l       상위 work / 첫 agent",
        "  Enter           선택한 pane에 attach",
        "  n               workspace/work + agent 생성/재사용",
        "",
        "조회와 협업",
        "  o / Alt-P       preview 열기",
        "  m / M           선택한 agent에 메시지 / mailbox",
        "  a / A           ask / history",
        "  Alt-S/L/D/T     workspace / latest / duration / state 정렬",
        "  ? / F1          전체 도움말",
    ]
}

fn interactive_guide(no_quiz: bool, language: UiLanguage) -> Result<()> {
    let terminal = setup_terminal()?;
    let mut guard = TerminalGuard::new(terminal);
    guard.terminal_mut().hide_cursor()?;
    let Some(language) = tmux::interactive_guide(guard.terminal_mut(), no_quiz, language)? else {
        return Ok(());
    };
    let mut app = TourApp::with_language(no_quiz, language);

    while !app.done {
        guard
            .terminal_mut()
            .draw(|frame| render_tour(frame, &app))?;
        if let Some(key) = read_key_event()? {
            handle_key(&mut app, key);
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TourStep {
    Work,
    Agents,
    States,
    Preview,
    Shortcuts,
    NewWork,
    Collaboration,
    Mcp,
    Finish,
}

impl TourStep {
    const ALL: [Self; 9] = [
        Self::Work,
        Self::Agents,
        Self::States,
        Self::Preview,
        Self::Shortcuts,
        Self::NewWork,
        Self::Collaboration,
        Self::Mcp,
        Self::Finish,
    ];
}

const UNIFIED_STEP_COUNT: usize = tmux::STEP_COUNT + TourStep::ALL.len();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MockOverlay {
    None,
    Preview,
    Help,
    NewWorkForm,
    Message,
    Mailbox,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MockSelection {
    WorkOnboarding,
    WorkSandbox,
    Codex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NewWorkStage {
    Shortcut,
    Form,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CollaborationStage {
    Message,
    Composer,
    Mailbox,
    MailboxOpen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TourMode {
    Guided,
    SkipQuiz,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MockSort {
    Latest,
    State,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MockPanel {
    None,
    Preview,
    Help,
}

#[derive(Debug)]
struct TourApp {
    step: usize,
    mode: TourMode,
    language: UiLanguage,
    selection: MockSelection,
    sort: MockSort,
    panel: MockPanel,
    new_work_stage: NewWorkStage,
    collaboration_stage: CollaborationStage,
    blocked_attempts: u8,
    done: bool,
}

impl TourApp {
    #[cfg(test)]
    fn new(no_quiz: bool) -> Self {
        Self::with_language(no_quiz, UiLanguage::En)
    }

    fn with_language(no_quiz: bool, language: UiLanguage) -> Self {
        Self {
            step: 0,
            mode: if no_quiz {
                TourMode::SkipQuiz
            } else {
                TourMode::Guided
            },
            language,
            // The first exercise moves from the idle work to muxa-onboarding with
            // the same `j`/Down navigation used by live watch.
            selection: MockSelection::WorkSandbox,
            sort: MockSort::Latest,
            panel: MockPanel::None,
            new_work_stage: NewWorkStage::Shortcut,
            collaboration_stage: CollaborationStage::Message,
            blocked_attempts: 0,
            done: false,
        }
    }

    fn current(&self) -> TourStep {
        TourStep::ALL[self.step]
    }

    fn guided(&self) -> bool {
        self.mode == TourMode::Guided
    }

    fn blocked_hint(&self) -> bool {
        self.blocked_attempts > 0
    }

    fn note_blocked(&mut self) {
        self.blocked_attempts = self.blocked_attempts.saturating_add(1);
    }

    /// `Alt-T` is the only gate without an `Alt`-free equivalent, so a terminal
    /// that composes Option instead of sending Meta can strand the tour here.
    fn alt_only_gate(&self) -> bool {
        self.current() == TourStep::States
    }

    /// Surface the terminal fix and a way past the gate once the learner has
    /// visibly tried and failed, instead of teaching the workaround up front.
    fn offer_alt_bypass(&self) -> bool {
        self.alt_only_gate() && self.blocked_attempts >= 2
    }

    fn ko(&self) -> bool {
        self.language == UiLanguage::Ko
    }

    fn toggle_language(&mut self) {
        self.language = match self.language {
            UiLanguage::En => UiLanguage::Ko,
            UiLanguage::Ko => UiLanguage::En,
        };
        self.blocked_attempts = 0;
    }

    fn advance(&mut self) {
        self.blocked_attempts = 0;
        if self.step + 1 == TourStep::ALL.len() {
            self.done = true;
        } else {
            self.step += 1;
        }
    }

    fn previous(&mut self) {
        self.blocked_attempts = 0;
        self.step = self.step.saturating_sub(1);
    }
}

/// macOS composes `Option` into a glyph unless the terminal is told to send
/// Meta, so the `Alt-T` this tour teaches arrives as `†` carrying no modifier
/// at all and the gate can never be satisfied on a stock macOS terminal. Map
/// the US-layout compose output back to the letter so the walkthrough still
/// advances; `docs/WATCH.md` covers the terminal settings that restore the real
/// chord for live watch.
const OPTION_COMPOSED_LETTERS: [(char, char); 28] = [
    ('å', 'a'),
    ('∫', 'b'),
    ('ç', 'c'),
    ('∂', 'd'),
    ('´', 'e'),
    ('ƒ', 'f'),
    ('©', 'g'),
    ('˙', 'h'),
    ('ˆ', 'i'),
    ('∆', 'j'),
    ('˚', 'k'),
    ('¬', 'l'),
    ('µ', 'm'),
    ('˜', 'n'),
    ('ø', 'o'),
    ('π', 'p'),
    ('œ', 'q'),
    ('®', 'r'),
    ('ß', 's'),
    ('†', 't'),
    ('¨', 'u'),
    ('√', 'v'),
    ('∑', 'w'),
    ('≈', 'x'),
    ('¥', 'y'),
    ('Ω', 'z'),
    // The hints spell the chords with a capital letter, so learners reach for
    // Shift and compose the shifted glyph instead.
    ('ˇ', 't'),
    ('∏', 'p'),
];

fn option_composed_letter(ch: char) -> Option<char> {
    OPTION_COMPOSED_LETTERS
        .iter()
        .find(|(composed, _)| *composed == ch)
        .map(|(_, letter)| *letter)
}

/// True when `key` is the `Alt-<letter>` chord the tour teaches, whether the
/// terminal sends a real Meta modifier or only the macOS compose glyph.
fn is_alt_chord(key: KeyEvent, letter: char) -> bool {
    match key.code {
        KeyCode::Char(ch) if key.modifiers.contains(KeyModifiers::ALT) => {
            ch.eq_ignore_ascii_case(&letter)
        }
        KeyCode::Char(ch)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            option_composed_letter(ch) == Some(letter)
        }
        _ => false,
    }
}

/// crossterm reports a bare `Esc` the moment a read returns a lone `\x1b` with
/// nothing else in the same syscall, so an escape sequence split across reads —
/// an arrow key relayed through tmux, or a slow pty — surfaces as a phantom
/// quit. Wait this long before trusting `Esc`.
const ESC_SEQUENCE_GRACE: Duration = Duration::from_millis(100);

/// Read one key, dropping the phantom `Esc` of a split escape sequence.
pub(super) fn read_key_event() -> Result<Option<KeyEvent>> {
    let Event::Key(key) = event::read().context("reading onboarding input")? else {
        return Ok(None);
    };
    if key.code != KeyCode::Esc
        || key.kind != KeyEventKind::Press
        || !event::poll(ESC_SEQUENCE_GRACE).context("checking for a split escape sequence")?
    {
        return Ok(Some(key));
    }
    resolve_split_escape()
}

/// The leading `\x1b` was already consumed, so the rest of the sequence arrives
/// as plain characters. `[` and `O` introduce a CSI/SS3 tail that has to be
/// swallowed whole; anything else is the two-byte form of an `Alt` chord.
fn resolve_split_escape() -> Result<Option<KeyEvent>> {
    let Some(next) = next_pending_key()? else {
        return Ok(Some(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    };
    let KeyCode::Char(introducer) = next.code else {
        return Ok(Some(next));
    };
    if introducer != '[' && introducer != 'O' {
        return Ok(Some(KeyEvent::new(
            next.code,
            next.modifiers | KeyModifiers::ALT,
        )));
    }
    // Parameter bytes precede the final byte that terminates the sequence.
    // Swallowing the tail and returning nothing drops the *key* along with the
    // phantom `Esc`, which is the same dead end this function exists to
    // remove: `Step::Panes` accepts nothing but `Right`, so a transport that
    // splits `Esc` `[C` across reads leaves the learner pressing an arrow that
    // never arrives. The final byte says which key it was.
    let mut final_byte = None;
    while let Some(key) = next_pending_key()? {
        let KeyCode::Char(byte) = key.code else {
            break;
        };
        if ('@'..='~').contains(&byte) {
            final_byte = Some(byte);
            break;
        }
    }
    Ok(final_byte
        .and_then(csi_key)
        .map(|code| KeyEvent::new(code, KeyModifiers::NONE)))
}

/// The key a CSI/SS3 sequence terminating in this byte stands for.
///
/// Only the keys the tour gates on. Anything else stays swallowed: reporting a
/// wrong key is worse than reporting none, because a gate that advances on the
/// wrong press teaches the wrong thing.
fn csi_key(final_byte: char) -> Option<KeyCode> {
    match final_byte {
        'A' => Some(KeyCode::Up),
        'B' => Some(KeyCode::Down),
        'C' => Some(KeyCode::Right),
        'D' => Some(KeyCode::Left),
        'H' => Some(KeyCode::Home),
        'F' => Some(KeyCode::End),
        _ => None,
    }
}

fn next_pending_key() -> Result<Option<KeyEvent>> {
    while event::poll(Duration::ZERO).context("draining a split escape sequence")? {
        if let Event::Key(key) = event::read().context("draining a split escape sequence")? {
            if key.kind == KeyEventKind::Press {
                return Ok(Some(key));
            }
        }
    }
    Ok(None)
}

/// The key the current stage is waiting for, as a bare token.
///
/// `required_action` phrases the same thing for the learner; keeping both on
/// one source means the published table cannot promise a key the gate refuses.
fn expected_key(app: &TourApp) -> &'static str {
    match app.current() {
        TourStep::Work => "j",
        TourStep::Agents | TourStep::Mcp => "l",
        TourStep::States => "Alt-T",
        TourStep::Preview => "o",
        TourStep::Shortcuts if app.panel == MockPanel::Preview => "o",
        TourStep::Shortcuts => "?",
        TourStep::NewWork if app.new_work_stage == NewWorkStage::Shortcut => "n",
        TourStep::NewWork => "Esc",
        TourStep::Collaboration => match app.collaboration_stage {
            CollaborationStage::Message => "m",
            CollaborationStage::Composer => "Backspace",
            _ => "M",
        },
        TourStep::Finish => "q",
    }
}

pub(super) fn key_for_token(token: &str) -> KeyEvent {
    match token {
        "Alt-T" => KeyEvent::new(KeyCode::Char('t'), KeyModifiers::ALT),
        "Esc" => KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        "Backspace" => KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        "Enter" => KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        "→" => KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
        "↓" => KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        other => {
            let ch = other.chars().next().unwrap_or('?');
            let modifiers = if ch.is_ascii_uppercase() || ch == '?' {
                KeyModifiers::SHIFT
            } else {
                KeyModifiers::NONE
            };
            KeyEvent::new(KeyCode::Char(ch), modifiers)
        }
    }
}

/// Walk of the guided watch track; see `tmux::step_table` for the contract.
fn watch_step_table() -> Vec<Vec<String>> {
    let mut app = TourApp::with_language(false, UiLanguage::En);
    let mut table: Vec<Vec<String>> = vec![Vec::new(); TourStep::ALL.len()];
    while !app.done {
        let index = app.step;
        while app.step == index && !app.done && table[index].len() < 8 {
            let token = expected_key(&app).to_string();
            table[index].push(token.clone());
            handle_key(&mut app, key_for_token(&token));
        }
        if app.step == index && !app.done {
            break;
        }
    }
    table
}

/// `<step number>\t<key>…` for all 20 unified steps — the contract the shell
/// fallback in `scripts/onboard.sh` is held to.
pub(super) fn step_table_tsv() -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    for (index, keys) in tmux::step_table()
        .into_iter()
        .chain(watch_step_table())
        .enumerate()
    {
        let _ = writeln!(out, "{}\t{}", index + 1, keys.join("\t"));
    }
    out
}

fn handle_key(app: &mut TourApp, key: KeyEvent) {
    if key.kind != KeyEventKind::Press {
        return;
    }
    if key.code == KeyCode::F(2) {
        app.toggle_language();
        return;
    }
    // Live watch lets the active modal consume Esc before the main TUI sees
    // it. Preserve that precedence instead of accidentally quitting the tour.
    if app.guided() && key.code == KeyCode::Esc {
        match app.current() {
            TourStep::Shortcuts if app.panel == MockPanel::Preview => {
                app.panel = MockPanel::None;
            }
            TourStep::Shortcuts if app.panel == MockPanel::Help => {
                app.panel = MockPanel::None;
                app.advance();
            }
            TourStep::NewWork if app.new_work_stage == NewWorkStage::Form => {
                app.advance();
            }
            TourStep::Collaboration if app.collaboration_stage == CollaborationStage::Composer => {
                app.collaboration_stage = CollaborationStage::Mailbox;
            }
            TourStep::Collaboration
                if app.collaboration_stage == CollaborationStage::MailboxOpen =>
            {
                app.collaboration_stage = CollaborationStage::Message;
                app.advance();
            }
            _ => {
                app.done = true;
                return;
            }
        }
        app.blocked_attempts = 0;
        return;
    }
    if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
        app.done = true;
        return;
    }
    if app.guided() {
        if handle_guided_key(app, key) {
            return;
        }
        match key.code {
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => app.previous(),
            _ => app.note_blocked(),
        }
        return;
    }
    match key.code {
        KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => app.previous(),
        KeyCode::Right | KeyCode::Char('l' | ' ') | KeyCode::Enter => app.advance(),
        KeyCode::Home => app.step = 0,
        KeyCode::End => app.step = TourStep::ALL.len() - 1,
        _ => {}
    }
}

fn handle_guided_key(app: &mut TourApp, key: KeyEvent) -> bool {
    let plain = !key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
    match app.current() {
        TourStep::Work
            if key.code == KeyCode::Down || (plain && key.code == KeyCode::Char('j')) =>
        {
            app.selection = MockSelection::WorkOnboarding;
            app.advance();
            true
        }
        TourStep::Agents
            if key.code == KeyCode::Right || (plain && key.code == KeyCode::Char('l')) =>
        {
            app.selection = MockSelection::Codex;
            app.advance();
            true
        }
        TourStep::States if is_alt_chord(key, 't') => {
            app.sort = MockSort::State;
            app.advance();
            true
        }
        TourStep::States
            if app.offer_alt_bypass() && matches!(key.code, KeyCode::Right | KeyCode::Enter) =>
        {
            app.sort = MockSort::State;
            app.advance();
            true
        }
        TourStep::Preview
            if (plain && key.code == KeyCode::Char('o')) || is_alt_chord(key, 'p') =>
        {
            app.panel = MockPanel::Preview;
            app.advance();
            true
        }
        TourStep::Shortcuts
            if app.panel == MockPanel::Preview && plain && key.code == KeyCode::Char('o') =>
        {
            app.panel = MockPanel::None;
            app.blocked_attempts = 0;
            true
        }
        TourStep::Shortcuts
            if app.panel == MockPanel::None
                && matches!(key.code, KeyCode::Char('?') | KeyCode::F(1)) =>
        {
            app.panel = MockPanel::Help;
            app.blocked_attempts = 0;
            true
        }
        TourStep::Shortcuts
            if app.panel == MockPanel::Help
                && matches!(key.code, KeyCode::Char('?') | KeyCode::F(1)) =>
        {
            app.panel = MockPanel::None;
            app.advance();
            true
        }
        TourStep::NewWork => handle_new_work_shortcut(app, key, plain),
        TourStep::Collaboration => handle_collaboration_shortcut(app, key, plain),
        TourStep::Mcp if matches!(key.code, KeyCode::Right | KeyCode::Char('l')) => {
            app.advance();
            true
        }
        TourStep::Finish if key.code == KeyCode::Char('q') => {
            app.done = true;
            true
        }
        _ => false,
    }
}

fn handle_new_work_shortcut(app: &mut TourApp, key: KeyEvent, plain: bool) -> bool {
    if app.new_work_stage != NewWorkStage::Shortcut || !plain || key.code != KeyCode::Char('n') {
        return false;
    }
    app.new_work_stage = NewWorkStage::Form;
    app.blocked_attempts = 0;
    true
}

fn handle_collaboration_shortcut(app: &mut TourApp, key: KeyEvent, plain: bool) -> bool {
    match (app.collaboration_stage, key.code) {
        (CollaborationStage::Message, KeyCode::Char('m')) if plain => {
            app.collaboration_stage = CollaborationStage::Composer;
        }
        (CollaborationStage::Composer, KeyCode::Backspace) => {
            app.collaboration_stage = CollaborationStage::Mailbox;
        }
        (CollaborationStage::Mailbox, KeyCode::Char('M')) => {
            app.collaboration_stage = CollaborationStage::MailboxOpen;
        }
        (CollaborationStage::MailboxOpen, KeyCode::Char('M')) => {
            app.collaboration_stage = CollaborationStage::Message;
            app.advance();
            return true;
        }
        _ => return false,
    }
    app.blocked_attempts = 0;
    true
}

fn render_tour(frame: &mut Frame<'_>, app: &TourApp) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(Color::Rgb(8, 12, 20))),
        area,
    );
    if area.width < 68 || area.height < 20 {
        render_small_terminal(frame, area, app.language);
        return;
    }

    // Live watch is three header rows, one full-height body, and a one-row
    // contextual footer. Tutorial dialogs are the only extra chrome.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);
    render_mock_header(frame, rows[0], app);
    if rows[1].width >= 120 {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[1]);
        render_mock_sessions(frame, columns[0], app);
        render_mock_inspector(frame, columns[1], app);
    } else {
        render_mock_sessions(frame, rows[1], app);
    }
    render_mock_footer(frame, rows[2], app);

    let overlay = match app.panel {
        MockPanel::Help => MockOverlay::Help,
        MockPanel::Preview => MockOverlay::Preview,
        MockPanel::None => match (app.current(), app.new_work_stage, app.collaboration_stage) {
            (TourStep::NewWork, NewWorkStage::Form, _) => MockOverlay::NewWorkForm,
            (TourStep::Collaboration, _, CollaborationStage::Composer) => MockOverlay::Message,
            (TourStep::Collaboration, _, CollaborationStage::MailboxOpen) => MockOverlay::Mailbox,
            _ => MockOverlay::None,
        },
    };
    render_mock_overlay(frame, rows[1], overlay, app.language);
    render_callout(frame, area, app);
}

fn render_small_terminal(frame: &mut Frame<'_>, area: Rect, language: UiLanguage) {
    let popup = centered_rect(
        area,
        area.width.saturating_sub(4).min(62),
        9.min(area.height),
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from(Span::styled(
                tr(
                    language,
                    "Muxa onboarding needs a little more room",
                    "Muxa 온보딩을 표시할 공간이 부족합니다",
                ),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(tr(
                language,
                "Resize to at least 68 × 20.",
                "터미널을 최소 68 × 20으로 키워주세요.",
            )),
            Line::from(tr(
                language,
                "Esc or q closes the tour.",
                "Esc 또는 q로 온보딩을 닫습니다.",
            )),
        ]))
        .alignment(Alignment::Center)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::Cyan)),
        ),
        popup,
    );
}

fn render_mock_header(frame: &mut Frame<'_>, area: Rect, app: &TourApp) {
    let sort = if app.sort == MockSort::State {
        "ST"
    } else {
        "LATEST"
    };
    let header = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                " muxa watch ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  2 works  ",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            mock_state_span(AgentState::WaitingInput, Color::Reset),
            Span::raw(" "),
            mock_state_span(AgentState::Working, Color::Reset),
            Span::raw(" "),
            mock_state_span(AgentState::Idle, Color::Reset),
            Span::styled("  mail 0/1", Style::default().fg(Color::Yellow)),
            Span::styled(
                format!("   sort {sort}   10:37:32 UTC"),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(Span::styled(
            "j/k move  ·  type or / filter  ·  : commands  ·  ? help",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        )),
    ])
    .block(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(Color::Rgb(43, 57, 78))),
    );
    frame.render_widget(header, area);
}

fn render_mock_sessions(frame: &mut Frame<'_>, area: Rect, app: &TourApp) {
    let mut lines = vec![Line::from(Span::styled(
        "  WORKSPACE › WORK         DUR    ACT    SUMMARY",
        Style::default().fg(Color::DarkGray),
    ))];
    if app.sort == MockSort::State {
        lines.extend(mock_onboarding_rows(app));
        lines.push(mock_sandbox_row(app));
    } else {
        lines.push(mock_sandbox_row(app));
        lines.extend(mock_onboarding_rows(app));
    }

    let border = if matches!(
        app.current(),
        TourStep::Work | TourStep::Agents | TourStep::States
    ) {
        Color::Cyan
    } else {
        Color::Rgb(43, 57, 78)
    };
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(
            Block::default()
                .title(" Workspace › work › agent ")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(border)),
        ),
        area,
    );
}

fn mock_onboarding_rows(app: &TourApp) -> Vec<Line<'static>> {
    let selected = app.selection == MockSelection::WorkOnboarding;
    let bg = if selected {
        Color::Rgb(18, 83, 108)
    } else {
        Color::Reset
    };
    let style = row_style(selected);
    let mut rows = vec![Line::from(vec![
        Span::styled(if selected { "> " } else { "  " }, style),
        mock_state_span(AgentState::WaitingInput, bg),
        Span::styled(" ", style),
        mock_state_span(AgentState::Working, bg),
        Span::styled(
            "   muxa › onboarding  12m    8s     harden checkout auth",
            style,
        ),
    ])];
    if matches!(
        app.selection,
        MockSelection::WorkOnboarding | MockSelection::Codex
    ) {
        let codex_selected = app.selection == MockSelection::Codex;
        rows.push(Line::from(Span::styled(
            if codex_selected {
                ">   └─ muxa-onboarding:0.0   -      8s     implement checkout hardening"
            } else {
                "    └─ muxa-onboarding:0.0   -      8s     implement checkout hardening"
            },
            row_style(codex_selected),
        )));
        rows.push(Line::from(Span::styled(
            "    └─ muxa-onboarding:1.0   -      2m     review public-read boundary",
            Style::default().fg(Color::Gray),
        )));
    }
    rows
}

fn mock_sandbox_row(app: &TourApp) -> Line<'static> {
    let selected = app.selection == MockSelection::WorkSandbox;
    let bg = if selected {
        Color::Rgb(18, 83, 108)
    } else {
        Color::Reset
    };
    let style = row_style(selected);
    Line::from(vec![
        Span::styled(if selected { "> " } else { "  " }, style),
        mock_state_span(AgentState::Idle, bg),
        Span::styled(
            "     muxa › sandbox     31m    1m     release checks complete",
            style,
        ),
    ])
}

fn row_style(selected: bool) -> Style {
    if selected {
        selected_style()
    } else {
        Style::default().fg(Color::White)
    }
}

/// Use the same canonical glyph source as `muxa watch` and its Classic state
/// palette. Work summaries put it in the left edge of the WORK cell,
/// matching watch's fixed six-cell state gutter.
fn mock_state_span(state: AgentState, background: Color) -> Span<'static> {
    let foreground = match state {
        AgentState::Idle => Color::Green,
        AgentState::Working | AgentState::WaitingInput => Color::Yellow,
        AgentState::WaitingChoice => Color::LightYellow,
        AgentState::Error => Color::Red,
        AgentState::Starting => Color::Cyan,
        AgentState::Stopped => Color::DarkGray,
    };
    let mut style = Style::default().fg(foreground).add_modifier(Modifier::BOLD);
    if background != Color::Reset {
        style = style.bg(background);
    }
    Span::styled(crate::state_icon(state), style)
}

fn render_mock_inspector(frame: &mut Frame<'_>, area: Rect, app: &TourApp) {
    let (pane, state, age, kind, latest) = match app.selection {
        MockSelection::WorkSandbox => (
            "muxa-sandbox:0.0",
            "IDLE",
            "1m",
            "codex",
            "release checks complete",
        ),
        MockSelection::WorkOnboarding => (
            "muxa-onboarding:1.0",
            "WAIT",
            "2m",
            "claude_code",
            "review public-read boundary",
        ),
        MockSelection::Codex => (
            "muxa-onboarding:0.0",
            "WORK",
            "8s",
            "codex",
            "implement checkout hardening",
        ),
    };
    let title = format!(" Inspector · {pane} · {state} {age} ");
    let lines = vec![
        Line::from(vec![
            Span::styled("kind ", Style::default().fg(Color::DarkGray)),
            Span::raw(kind),
            Span::styled("  model ", Style::default().fg(Color::DarkGray)),
            Span::raw("—"),
        ]),
        Line::from(vec![
            Span::styled("latest ", Style::default().fg(Color::DarkGray)),
            Span::raw(latest),
        ]),
        Line::from(Span::styled(
            "────────────────────────────────────────────────────────",
            Style::default().fg(Color::Rgb(43, 57, 78)),
        )),
        Line::from(Span::styled("● codex", Style::default().fg(Color::Cyan))),
        Line::from(""),
        Line::from("› implement checkout hardening"),
        Line::from(""),
        Line::from(Span::styled(
            "  ⚙ editing  crates/muxa-cli/src/watch.rs",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
        Line::from(Span::styled(
            if state == "WAIT" {
                "  ▶ waiting for input"
            } else {
                "  ● working…"
            },
            Style::default().fg(Color::Yellow),
        )),
    ];
    let border = if matches!(app.current(), TourStep::Preview | TourStep::Mcp) {
        Color::Cyan
    } else {
        Color::Rgb(43, 57, 78)
    };
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(border)),
            ),
        area,
    );
}

fn render_mock_footer(frame: &mut Frame<'_>, area: Rect, app: &TourApp) {
    let key = Style::default()
        .fg(Color::Cyan)
        .bg(Color::Rgb(19, 61, 80))
        .add_modifier(Modifier::BOLD);
    let text = Style::default().fg(Color::DarkGray);
    let spans = if app.panel == MockPanel::Preview {
        vec![
            Span::styled(" o/Esc ", key),
            Span::styled(tr(app.language, "close  ", "닫기  "), text),
            Span::styled(" [/] ", key),
            Span::styled(tr(app.language, "agent  ", "agent  "), text),
            Span::styled(" f ", key),
            Span::styled(tr(app.language, "fullscreen  ", "전체화면  "), text),
            Span::styled(" c ", key),
            Span::styled(tr(app.language, "content", "내용"), text),
        ]
    } else if app.panel == MockPanel::Help {
        vec![
            Span::styled(" ?/F1/Esc ", key),
            Span::styled(tr(app.language, "close help  ", "도움말 닫기  "), text),
            Span::styled(" q/Ctrl-C ", key),
            Span::styled(tr(app.language, "quit watch", "watch 종료"), text),
        ]
    } else if app.current() == TourStep::NewWork && app.new_work_stage == NewWorkStage::Form {
        vec![
            Span::styled(" Enter ", key),
            Span::styled(tr(app.language, "launch  ", "실행  "), text),
            Span::styled(" Tab/↑/↓ ", key),
            Span::styled(tr(app.language, "field  ", "항목  "), text),
            Span::styled(" ←/→ ", key),
            Span::styled("agent  ", text),
            Span::styled(" Esc ", key),
            Span::styled(tr(app.language, "cancel", "취소"), text),
        ]
    } else if app.current() == TourStep::Collaboration
        && app.collaboration_stage == CollaborationStage::Composer
    {
        vec![
            Span::styled(" Enter ", key),
            Span::styled(tr(app.language, "send  ", "보내기  "), text),
            Span::styled(" Tab ", key),
            Span::styled(tr(app.language, "contract  ", "계약  "), text),
            Span::styled(" Esc/empty ⌫ ", key),
            Span::styled(tr(app.language, "cancel", "취소"), text),
        ]
    } else if app.current() == TourStep::Collaboration
        && app.collaboration_stage == CollaborationStage::MailboxOpen
    {
        vec![
            Span::styled(" M/Esc ", key),
            Span::styled(tr(app.language, "close  ", "닫기  "), text),
            Span::styled(" i/e ", key),
            Span::styled(tr(app.language, "claim/reply  ", "수락/응답  "), text),
            Span::styled(" Tab ", key),
            Span::styled(tr(app.language, "incoming/sent", "받은/보낸"), text),
        ]
    } else {
        vec![
            Span::styled(" j/k ", key),
            Span::styled(tr(app.language, "move  ", "이동  "), text),
            Span::styled(" h/l ", key),
            Span::styled(tr(app.language, "tree  ", "트리  "), text),
            Span::styled(" / ", key),
            Span::styled(tr(app.language, "filter  ", "필터  "), text),
            Span::styled(" : ", key),
            Span::styled(tr(app.language, "commands  ", "명령  "), text),
            Span::styled(" ⏎ ", key),
            Span::styled(tr(app.language, "prompt  ", "prompt  "), text),
            Span::styled(" o ", key),
            Span::styled(tr(app.language, "preview  ", "미리보기  "), text),
            Span::styled(" m ", key),
            Span::styled(tr(app.language, "message  ", "메시지  "), text),
            Span::styled(" M ", key),
            Span::styled(tr(app.language, "mailbox  ", "메일함  "), text),
            Span::styled(" ? ", key),
            Span::styled(tr(app.language, "help", "도움말"), text),
        ]
    };
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_mock_overlay(
    frame: &mut Frame<'_>,
    area: Rect,
    overlay: MockOverlay,
    language: UiLanguage,
) {
    match overlay {
        MockOverlay::None => {}
        MockOverlay::Preview => render_preview_overlay(frame, area),
        MockOverlay::Help => render_help_overlay(frame, area, language),
        MockOverlay::NewWorkForm => render_new_work_overlay(frame, area),
        MockOverlay::Message => render_message_overlay(frame, area),
        MockOverlay::Mailbox => {
            render_compact_popup(
                frame,
                area,
                " mailbox ",
                tr(
                    language,
                    "incoming 0   sent 1\n\n▸ review · read_only · claude@%43\n  Review the current changes\n\nM/Esc close · Enter inspect",
                    "받은 요청 0   보낸 요청 1\n\n▸ review · read_only · claude@%43\n  현재 변경사항을 검토해주세요\n\nM/Esc 닫기 · Enter 자세히",
                ),
            );
        }
    }
}

fn render_preview_overlay(frame: &mut Frame<'_>, area: Rect) {
    let popup = centered_rect(
        area,
        area.width.saturating_mul(80) / 100,
        area.height.saturating_mul(70) / 100,
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from(vec![
                Span::styled("● codex", Style::default().fg(Color::Cyan)),
                Span::styled("  muxa-onboarding", Style::default().fg(Color::DarkGray)),
            ]),
            Line::from(""),
            Line::from("› implement checkout hardening"),
            Line::from(""),
            Line::from("  ⚙ read     crates/muxa/src/tmux_work.rs"),
            Line::from("  ⚙ editing  crates/muxa-cli/src/watch.rs"),
            Line::from(""),
            Line::from(Span::styled(
                "  ● working…",
                Style::default().fg(Color::Yellow),
            )),
        ]))
        .block(dialog_block(
            " Preview · muxa-onboarding:0.0 · live pane ",
            Color::Cyan,
        )),
        popup,
    );
}

fn render_help_overlay(frame: &mut Frame<'_>, area: Rect, language: UiLanguage) {
    let popup = centered_rect(
        area,
        area.width.saturating_sub(8).min(104),
        area.height.saturating_sub(2).min(25),
    );
    frame.render_widget(Clear, popup);
    let lines = if language == UiLanguage::Ko {
        vec![
            Line::from(Span::styled(
                "필터와 이동",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            highlighted_actions("  ↑/↓ · j/k       work/agent 이동", &["↑/↓", "j/k"]),
            highlighted_actions("  ←/→ · h/l       상위 work / 첫 agent", &["←/→", "h/l"]),
            highlighted_actions("  Enter           선택한 pane에 attach", &["Enter"]),
            highlighted_actions(
                "  n               workspace/work + agent 생성/재사용",
                &["n"],
            ),
            Line::from(""),
            Line::from(Span::styled(
                "조회와 협업",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            highlighted_actions("  o / Alt-P       preview 열기", &["o", "Alt-P"]),
            highlighted_actions(
                "  m / M           선택한 agent에 메시지 / mailbox",
                &["m", "M"],
            ),
            highlighted_actions("  a / A           ask / history", &["a", "A"]),
            highlighted_actions(
                "  Alt-S/L/D/T     workspace / latest / duration / state 정렬",
                &["Alt-S/L/D/T"],
            ),
            Line::from(""),
            Line::from(Span::styled(
                "상태 표시",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from("  ● 작업 중  ▶ 입력 필요  ◆ 선택 필요  ■ 오류  ○ 대기"),
        ]
    } else {
        vec![
            Line::from(Span::styled(
                "Filter & navigation",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            highlighted_actions("  ↑/↓ · j/k       move works/agents", &["↑/↓", "j/k"]),
            highlighted_actions(
                "  ←/→ · h/l       parent work / first agent",
                &["←/→", "h/l"],
            ),
            highlighted_actions("  Enter           attach to selected pane", &["Enter"]),
            highlighted_actions(
                "  n               new/reused workspace/work + agent",
                &["n"],
            ),
            Line::from(""),
            Line::from(Span::styled(
                "Commands & inspection",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            highlighted_actions("  o / Alt-P       open preview overlay", &["o", "Alt-P"]),
            highlighted_actions(
                "  m / M           message selected agent / mailbox",
                &["m", "M"],
            ),
            highlighted_actions("  a / A           ask / history", &["a", "A"]),
            highlighted_actions(
                "  Alt-S/L/D/T     workspace / latest / duration / state",
                &["Alt-S/L/D/T"],
            ),
            Line::from(""),
            Line::from(Span::styled(
                "State markers",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from("  ● working  ▶ input  ◆ choice  ■ error  ○ idle"),
        ]
    };
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(dialog_block(
            tr(
                language,
                " help · ?/F1 closes · q/Ctrl-C quits watch ",
                " 도움말 · ?/F1 닫기 · q/Ctrl-C watch 종료 ",
            ),
            Color::Cyan,
        )),
        popup,
    );
}

fn render_new_work_overlay(frame: &mut Frame<'_>, area: Rect) {
    let height = area.height.min(8);
    let popup = Rect::new(
        area.x,
        area.y + area.height.saturating_sub(height),
        area.width,
        height,
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from("> dir    /home/june/personal/muxa"),
            Line::from("  space  muxa  (from directory)"),
            Line::from("  ticket muxa-onboarding"),
            Line::from("  agent  ◂  codex  ▸"),
            Line::from("  prompt Implement muxa-onboarding"),
        ]))
        .block(dialog_block(" new work + agent ", Color::Cyan)),
        popup,
    );
}

fn render_message_overlay(frame: &mut Frame<'_>, area: Rect) {
    let height = area.height.min(4);
    let popup = Rect::new(
        area.x,
        area.y + area.height.saturating_sub(height),
        area.width,
        height,
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new("> █").block(dialog_block(
            " message → claude@%43 · review · read_only ",
            Color::Magenta,
        )),
        popup,
    );
}

fn render_compact_popup(frame: &mut Frame<'_>, area: Rect, title: &str, body: &str) {
    let popup = centered_rect(area, area.width.min(70), area.height.min(11));
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .block(dialog_block(title, Color::Magenta)),
        popup,
    );
}

fn render_callout(frame: &mut Frame<'_>, area: Rect, app: &TourApp) {
    let step = app.current();
    let popup = callout_rect(area, app);
    frame.render_widget(Clear, popup);
    let title = format!(
        " {}/{} · {} ",
        tmux::STEP_COUNT + app.step + 1,
        UNIFIED_STEP_COUNT,
        step_title(step, app.language)
    );
    let body = step_body(app);
    let footer = highlighted_actions(
        callout_footer(app),
        &[
            "←/Backspace",
            "Enter/→",
            "j/↓",
            "l/→",
            "Alt-T",
            "?/F1",
            "Backspace",
            "Enter",
            "F2",
            "Esc",
            "←",
            "o",
            "n",
            "m",
            "M",
            "q",
        ],
    );
    frame.render_widget(
        Paragraph::new(body).wrap(Wrap { trim: false }).block(
            dialog_block(&title, Color::Cyan).title_bottom(footer.alignment(Alignment::Center)),
        ),
        popup,
    );
}

fn callout_footer(app: &TourApp) -> &'static str {
    if !app.guided() {
        return tr(
            app.language,
            " ←/Backspace back · Enter/→ next · F2 한국어 · Esc quit ",
            " ←/Backspace 이전 · Enter/→ 다음 · F2 English · Esc 종료 ",
        );
    }
    let en = match app.current() {
        TourStep::Work => " j/↓ move to muxa-onboarding · F2 한국어 · Esc to quit ",
        TourStep::Agents => " l/→ move to the first agent · ← back · Esc to quit ",
        TourStep::States => " Alt-T sort by state · ← back · Esc to quit ",
        TourStep::Preview => " o open preview · ← back · Esc to quit ",
        TourStep::Shortcuts if app.panel == MockPanel::Preview => {
            " o close preview · Esc to quit the onboarding "
        }
        TourStep::Shortcuts if app.panel == MockPanel::Help => " ?/F1 close help and continue ",
        TourStep::Shortcuts => " ?/F1 open full help · ← back · Esc to quit ",
        TourStep::NewWork if app.new_work_stage == NewWorkStage::Shortcut => {
            " n open the new-work form · ← back · Esc to quit "
        }
        TourStep::NewWork if app.new_work_stage == NewWorkStage::Form => {
            " Esc close the practice form and continue "
        }
        TourStep::NewWork => " n open form · Esc close form and continue ",
        TourStep::Collaboration if app.collaboration_stage == CollaborationStage::Message => {
            " m message the selected agent · ← back · Esc to quit "
        }
        TourStep::Collaboration if app.collaboration_stage == CollaborationStage::Composer => {
            " Backspace closes the empty composer "
        }
        TourStep::Collaboration if app.collaboration_stage == CollaborationStage::Mailbox => {
            " M open mailbox "
        }
        TourStep::Collaboration => " M close mailbox and continue ",
        TourStep::Mcp => " l/→ continue · ← back · Esc to quit ",
        TourStep::Finish => " q finish · q also quits watch ",
    };
    let ko = match app.current() {
        TourStep::Work => " j/↓ muxa-onboarding으로 이동 · F2 English · Esc로 종료 ",
        TourStep::Agents => " l/→ 첫 agent로 이동 · ← 이전 · Esc로 종료 ",
        TourStep::States => " Alt-T 상태순 정렬 · ← 이전 · Esc로 종료 ",
        TourStep::Preview => " o preview 열기 · ← 이전 · Esc로 종료 ",
        TourStep::Shortcuts if app.panel == MockPanel::Preview => {
            " o preview 닫기 · Esc 온보딩 종료 "
        }
        TourStep::Shortcuts if app.panel == MockPanel::Help => " ?/F1 도움말 닫고 계속 ",
        TourStep::Shortcuts => " ?/F1 전체 도움말 · ← 이전 · Esc로 종료 ",
        TourStep::NewWork if app.new_work_stage == NewWorkStage::Shortcut => {
            " n 새 work form 열기 · ← 이전 · Esc로 종료 "
        }
        TourStep::NewWork if app.new_work_stage == NewWorkStage::Form => {
            " Esc 연습 form 닫고 계속 "
        }
        TourStep::NewWork => " n form 열기 · Esc 닫고 계속 ",
        TourStep::Collaboration if app.collaboration_stage == CollaborationStage::Message => {
            " m 선택한 agent에게 메시지 · ← 이전 · Esc로 종료 "
        }
        TourStep::Collaboration if app.collaboration_stage == CollaborationStage::Composer => {
            " 빈 composer는 Backspace로 닫기 "
        }
        TourStep::Collaboration if app.collaboration_stage == CollaborationStage::Mailbox => {
            " M mailbox 열기 "
        }
        TourStep::Collaboration => " M mailbox 닫고 계속 ",
        TourStep::Mcp => " l/→ 계속 · ← 이전 · Esc로 종료 ",
        TourStep::Finish => " q 완료 · q는 watch 종료 키 ",
    };
    tr(app.language, en, ko)
}

fn step_title(step: TourStep, language: UiLanguage) -> &'static str {
    let en = match step {
        TourStep::Work => "navigate between work windows",
        TourStep::Agents => "open a work to see its agents",
        TourStep::States => "use state to choose the next action",
        TourStep::Preview => "preview a pane before attaching",
        TourStep::Shortcuts => "find actions in the footer and Help",
        TourStep::NewWork => "create a work and its first agent",
        TourStep::Collaboration => "message an agent and review the mailbox",
        TourStep::Mcp => "let agents use Muxa through MCP",
        TourStep::Finish => "ready for the live watch",
    };
    let ko = match step {
        TourStep::Work => "work window 사이 이동하기",
        TourStep::Agents => "work를 열어 agent 확인하기",
        TourStep::States => "상태를 보고 다음 행동 정하기",
        TourStep::Preview => "attach 전에 pane 미리 보기",
        TourStep::Shortcuts => "footer와 도움말에서 동작 찾기",
        TourStep::NewWork => "work와 첫 agent 만들기",
        TourStep::Collaboration => "agent에게 메시지 보내고 mailbox 확인하기",
        TourStep::Mcp => "MCP로 agent에게 Muxa 맡기기",
        TourStep::Finish => "실제 watch를 사용할 준비 완료",
    };
    tr(language, en, ko)
}

fn step_body(app: &TourApp) -> Text<'static> {
    let mut lines = step_lines(app);
    if app.blocked_hint() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(
                "{}: {}",
                tr(app.language, "Expected", "필요한 입력"),
                required_action(app)
            ),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
    }
    if app.offer_alt_bypass() {
        for hint in alt_bypass_hint(app.language) {
            lines.push(Line::from(Span::styled(
                *hint,
                Style::default().fg(Color::Yellow),
            )));
        }
    }
    Text::from(lines)
}

/// Shown only after the learner has actually pressed the wrong key twice, so
/// the terminal caveat stays out of the way of everyone it does not affect.
fn alt_bypass_hint(language: UiLanguage) -> &'static [&'static str] {
    if language == UiLanguage::Ko {
        &[
            "Alt이 안 눌리나요? macOS는 Option을 조합 키로 씁니다.",
            "docs/WATCH.md의 터미널 설정을 보거나, →로 넘어가세요.",
        ]
    } else {
        &[
            "Alt not arriving? macOS composes Option instead of sending Meta.",
            "See docs/WATCH.md for the terminal setting, or press → to skip.",
        ]
    }
}

fn required_action(app: &TourApp) -> &'static str {
    let en = match app.current() {
        TourStep::Work => "press j or Down",
        TourStep::Agents | TourStep::Mcp => "press l or Right",
        TourStep::States => "press Alt-T",
        TourStep::Preview => "press o",
        TourStep::Shortcuts if app.panel == MockPanel::Preview => "press o to close preview",
        TourStep::Shortcuts if app.panel == MockPanel::Help => "press ? or F1 to close help",
        TourStep::Shortcuts => "press ? or F1 to open help",
        TourStep::NewWork if app.new_work_stage == NewWorkStage::Shortcut => "press n",
        TourStep::NewWork if app.new_work_stage == NewWorkStage::Form => "press Esc",
        TourStep::NewWork => "press n or Esc",
        TourStep::Collaboration if app.collaboration_stage == CollaborationStage::Message => {
            "press m"
        }
        TourStep::Collaboration if app.collaboration_stage == CollaborationStage::Composer => {
            "press Backspace while the composer is empty"
        }
        TourStep::Collaboration if app.collaboration_stage == CollaborationStage::Mailbox => {
            "press M"
        }
        TourStep::Collaboration => "press M again to close the mailbox",
        TourStep::Finish => "press q to finish",
    };
    let ko = match app.current() {
        TourStep::Work => "j 또는 ↓를 누르세요",
        TourStep::Agents | TourStep::Mcp => "l 또는 →를 누르세요",
        TourStep::States => "Alt-T를 누르세요",
        TourStep::Preview => "o를 누르세요",
        TourStep::Shortcuts if app.panel == MockPanel::Preview => "o를 눌러 preview를 닫으세요",
        TourStep::Shortcuts if app.panel == MockPanel::Help => "? 또는 F1으로 도움말을 닫으세요",
        TourStep::Shortcuts => "? 또는 F1으로 도움말을 여세요",
        TourStep::NewWork if app.new_work_stage == NewWorkStage::Shortcut => "n을 누르세요",
        TourStep::NewWork if app.new_work_stage == NewWorkStage::Form => "Esc를 누르세요",
        TourStep::NewWork => "n 또는 Esc를 누르세요",
        TourStep::Collaboration if app.collaboration_stage == CollaborationStage::Message => {
            "m을 누르세요"
        }
        TourStep::Collaboration if app.collaboration_stage == CollaborationStage::Composer => {
            "빈 composer에서 Backspace를 누르세요"
        }
        TourStep::Collaboration if app.collaboration_stage == CollaborationStage::Mailbox => {
            "M을 누르세요"
        }
        TourStep::Collaboration => "M을 다시 눌러 mailbox를 닫으세요",
        TourStep::Finish => "q를 눌러 완료하세요",
    };
    tr(app.language, en, ko)
}

fn step_lines(app: &TourApp) -> Vec<Line<'static>> {
    if app.ko() {
        return step_lines_ko(app);
    }
    match app.current() {
        TourStep::Work => vec![
            callout_label("← MOVE BETWEEN WORK WINDOWS"),
            Line::from(""),
            highlighted_actions(
                "The cursor is on muxa-sandbox. Press j or ↓ to select muxa-onboarding.",
                &["j", "↓"],
            ),
            Line::from("Each row represents one work window in the muxa workspace session."),
            Line::from("Starting the same work again reuses its window and adds an agent pane."),
        ],
        TourStep::Agents => vec![
            callout_label("← OPEN THE SELECTED WORK"),
            Line::from(""),
            Line::from("Expanding a work reveals the agent panes running inside it."),
            highlighted_actions("Press l or → to select the first agent.", &["l", "→"]),
            highlighted_actions(
                "Use h or ← whenever you want to return to the work window.",
                &["h", "←"],
            ),
        ],
        TourStep::States => vec![
            callout_label("← READ THE STATE BEFORE YOU INTERRUPT AN AGENT"),
            Line::from(""),
            state_legend_line(AgentState::Working, "working — leave it alone"),
            state_legend_line(AgentState::WaitingInput, "waiting — it needs input"),
            state_legend_line(AgentState::Idle, "idle — its turn has settled"),
            state_legend_line(AgentState::Error, "error — inspect the pane"),
            highlighted_actions(
                "Press Alt-T to sort the watch by state and attention.",
                &["Alt-T"],
            ),
        ],
        TourStep::Preview => vec![
            callout_label("→ CHECK A PANE WITHOUT ATTACHING"),
            Line::from(""),
            Line::from("On a wide screen, the inspector sits beside the work list."),
            highlighted_actions("Press o to preview the selected pane.", &["o"]),
            highlighted_actions(
                "Use Enter only when you need to attach to the real terminal.",
                &["Enter"],
            ),
        ],
        TourStep::Shortcuts if app.panel == MockPanel::Preview => vec![
            callout_label("THE PREVIEW OPENS OVER THE WATCH"),
            Line::from(""),
            Line::from("The work list remains behind it, so your place is preserved."),
            highlighted_actions("Press o again to close the preview.", &["o"]),
        ],
        TourStep::Shortcuts if app.panel == MockPanel::Help => vec![
            callout_label("? OPENS THE COMPLETE SHORTCUT MAP"),
            Line::from(""),
            Line::from("This is the same shortcut reference used by the live watch."),
            highlighted_actions(
                "Press ? or F1 again to close it and continue.",
                &["?", "F1"],
            ),
        ],
        TourStep::Shortcuts => vec![
            callout_label("↓ THE FOOTER SHOWS WHAT YOU CAN DO HERE"),
            Line::from(""),
            Line::from("Its actions change with the current selection and open panel."),
            highlighted_actions(
                "Press ? or F1 whenever you need the complete shortcut map.",
                &["?", "F1"],
            ),
        ],
        TourStep::NewWork => new_work_step_lines(app),
        TourStep::Collaboration => collaboration_step_lines(app),
        TourStep::Mcp => vec![
            callout_label("LET AGENTS MANAGE TMUX THROUGH MUXA"),
            Line::from(""),
            Line::from("muxa_start_agent creates or reuses the expected window and pane."),
            Line::from("settled + capture returns the useful final screen."),
            Line::from("Agents do not need a separate tmux MCP or handwritten tmux script."),
            highlighted_actions("Press l or → to continue.", &["l", "→"]),
        ],
        TourStep::Finish => vec![
            callout_label("THE MODEL TO REMEMBER"),
            Line::from(""),
            policy_line("SESSION", "workspace / project"),
            policy_line("WINDOW", "work / ticket"),
            policy_line("PANE", "agent"),
            Line::from(""),
            highlighted_actions(
                "✓ muxa watch — press q to finish (q also quits watch)",
                &["q"],
            ),
        ],
    }
}

fn step_lines_ko(app: &TourApp) -> Vec<Line<'static>> {
    match app.current() {
        TourStep::Work => vec![
            callout_label("← WORK WINDOW 사이를 이동합니다"),
            Line::from(""),
            highlighted_actions(
                "cursor는 muxa-sandbox에 있습니다. j 또는 ↓로 muxa-onboarding을 선택하세요.",
                &["j", "↓"],
            ),
            Line::from("각 row는 muxa workspace session 안의 work window 하나를 나타냅니다."),
            Line::from("같은 work를 다시 시작하면 window를 재사용하고 agent pane을 추가합니다."),
        ],
        TourStep::Agents => vec![
            callout_label("← 선택한 WORK를 펼칩니다"),
            Line::from(""),
            Line::from("work를 펼치면 그 안에서 실행 중인 agent pane이 보입니다."),
            highlighted_actions("l 또는 →로 첫 번째 agent를 선택하세요.", &["l", "→"]),
            highlighted_actions("work window로 돌아가려면 h 또는 ←를 누르세요.", &["h", "←"]),
        ],
        TourStep::States => vec![
            callout_label("← AGENT를 방해하기 전에 상태부터 확인합니다"),
            Line::from(""),
            state_legend_line(AgentState::Working, "작업 중 — 그대로 두세요"),
            state_legend_line(AgentState::WaitingInput, "입력 대기 — 응답이 필요합니다"),
            state_legend_line(AgentState::Idle, "대기 — turn이 끝났습니다"),
            state_legend_line(AgentState::Error, "오류 — pane을 확인하세요"),
            highlighted_actions(
                "Alt-T를 눌러 state와 attention이 필요한 순서로 정렬하세요.",
                &["Alt-T"],
            ),
        ],
        TourStep::Preview => vec![
            callout_label("→ ATTACH하지 않고 PANE을 확인합니다"),
            Line::from(""),
            Line::from("넓은 화면에서는 inspector가 work 목록 옆에 표시됩니다."),
            highlighted_actions("o를 눌러 선택한 pane의 preview를 여세요.", &["o"]),
            highlighted_actions(
                "실제 terminal 조작이 필요할 때만 Enter로 attach하세요.",
                &["Enter"],
            ),
        ],
        TourStep::Shortcuts if app.panel == MockPanel::Preview => vec![
            callout_label("PREVIEW는 WATCH 위에 열립니다"),
            Line::from(""),
            Line::from("뒤에 work 목록이 남아 있어 현재 위치가 유지됩니다."),
            highlighted_actions("o를 다시 눌러 preview를 닫으세요.", &["o"]),
        ],
        TourStep::Shortcuts if app.panel == MockPanel::Help => vec![
            callout_label("?로 전체 단축키 지도를 확인합니다"),
            Line::from(""),
            Line::from("실제 watch와 같은 단축키 설명을 볼 수 있습니다."),
            highlighted_actions("? 또는 F1을 다시 눌러 닫고 계속하세요.", &["?", "F1"]),
        ],
        TourStep::Shortcuts => vec![
            callout_label("↓ FOOTER에서 지금 가능한 동작을 확인합니다"),
            Line::from(""),
            Line::from("footer의 동작은 현재 선택과 열린 panel에 맞춰 바뀝니다."),
            highlighted_actions("전체 단축키가 필요하면 ? 또는 F1을 누르세요.", &["?", "F1"]),
        ],
        TourStep::NewWork => new_work_step_lines(app),
        TourStep::Collaboration => collaboration_step_lines(app),
        TourStep::Mcp => vec![
            callout_label("AGENT도 MUXA를 통해 TMUX를 관리합니다"),
            Line::from(""),
            Line::from("muxa_start_agent가 정해진 window와 pane을 생성하거나 재사용합니다."),
            Line::from("settled + capture를 사용하면 작업이 끝난 화면까지 확인할 수 있습니다."),
            Line::from("별도 tmux MCP나 agent가 직접 작성한 tmux script는 필요하지 않습니다."),
            highlighted_actions("l 또는 →로 계속하세요.", &["l", "→"]),
        ],
        TourStep::Finish => vec![
            callout_label("기억할 운영 모델"),
            Line::from(""),
            policy_line("SESSION", "workspace / project"),
            policy_line("WINDOW", "work / ticket"),
            policy_line("PANE", "agent"),
            Line::from(""),
            highlighted_actions(
                "✓ 준비 완료 — q로 끝내세요. q는 실제 watch 종료 키이기도 합니다.",
                &["q"],
            ),
        ],
    }
}

fn new_work_step_lines(app: &TourApp) -> Vec<Line<'static>> {
    if app.ko() {
        return match app.new_work_stage {
            NewWorkStage::Shortcut => vec![
                callout_label("새 WORK와 첫 AGENT를 함께 만듭니다"),
                Line::from(""),
                highlighted_actions(
                    "n을 누르면 work와 agent를 만드는 안내 form이 열립니다.",
                    &["n"],
                ),
                Line::from("directory, ticket, agent, 첫 prompt를 차례로 입력할 수 있습니다."),
            ],
            NewWorkStage::Form => vec![
                callout_label("FORM에서 생성할 WORK를 확인합니다"),
                Line::from(""),
                highlighted_actions(
                    "Tab/↑/↓로 항목을 바꾸고 ←/→로 agent를 바꿉니다.",
                    &["Tab/↑/↓", "←/→"],
                ),
                highlighted_actions(
                    "연습이므로 Esc로 form을 닫고 다음 단계로 이동하세요.",
                    &["Esc"],
                ),
            ],
        };
    }
    match app.new_work_stage {
        NewWorkStage::Shortcut => vec![
            callout_label("CREATE A WORK AND ITS FIRST AGENT TOGETHER"),
            Line::from(""),
            highlighted_actions(
                "Press n to open the guided form for a work and its agent.",
                &["n"],
            ),
            Line::from("It collects the directory, ticket, agent, and first prompt."),
        ],
        NewWorkStage::Form => vec![
            callout_label("REVIEW THE WORK YOU ARE ABOUT TO CREATE"),
            Line::from(""),
            highlighted_actions(
                "Use Tab/↑/↓ to change fields and ←/→ to change the agent.",
                &["Tab/↑/↓", "←/→"],
            ),
            highlighted_actions(
                "This is practice, so press Esc to close the form and continue.",
                &["Esc"],
            ),
        ],
    }
}

fn collaboration_step_lines(app: &TourApp) -> Vec<Line<'static>> {
    if app.ko() {
        return match app.collaboration_stage {
            CollaborationStage::Message => vec![
                callout_label("선택한 AGENT에게 메시지를 보냅니다"),
                Line::from(""),
                Line::from("m은 watch cursor가 가리키는 agent 한 명을 대상으로 합니다."),
                highlighted_actions("m을 눌러 해당 agent의 request composer를 여세요.", &["m"]),
            ],
            CollaborationStage::Composer => vec![
                callout_label("COMPOSER에서 요청 방식을 확인합니다"),
                Line::from(""),
                Line::from("kind와 mode는 화면에 표시되고 다음 메시지에도 유지됩니다."),
                highlighted_actions(
                    "지금은 내용이 비어 있으므로 Backspace를 눌러 닫으세요.",
                    &["Backspace"],
                ),
            ],
            CollaborationStage::Mailbox => vec![
                callout_label("MAILBOX에서 요청 이력을 확인합니다"),
                Line::from(""),
                Line::from("M은 받은 요청과 보낸 요청을 함께 엽니다. b도 같은 기능입니다."),
                highlighted_actions("M을 눌러 mailbox를 여세요.", &["M"]),
            ],
            CollaborationStage::MailboxOpen => vec![
                callout_label("보낸 요청도 MAILBOX에 남습니다"),
                Line::from(""),
                Line::from("보낸 요청 tab에는 한 agent에게 전송한 request도 남습니다."),
                highlighted_actions("M을 다시 눌러 닫고 계속하세요.", &["M"]),
            ],
        };
    }
    match app.collaboration_stage {
        CollaborationStage::Message => vec![
            callout_label("MESSAGE THE SELECTED AGENT"),
            Line::from(""),
            Line::from("m targets the single agent under the watch cursor."),
            highlighted_actions(
                "Press m to open the request composer for that agent.",
                &["m"],
            ),
        ],
        CollaborationStage::Composer => vec![
            callout_label("REVIEW HOW THE REQUEST WILL BE SENT"),
            Line::from(""),
            Line::from("kind and mode stay visible and are remembered for the next message."),
            highlighted_actions(
                "The body is empty, so press Backspace to close the composer.",
                &["Backspace"],
            ),
        ],
        CollaborationStage::Mailbox => vec![
            callout_label("REVIEW REQUEST HISTORY IN THE MAILBOX"),
            Line::from(""),
            Line::from("M opens incoming and sent requests together; b is an alias."),
            highlighted_actions("Press M to open the mailbox.", &["M"]),
        ],
        CollaborationStage::MailboxOpen => vec![
            callout_label("SENT REQUESTS STAY IN THE MAILBOX"),
            Line::from(""),
            Line::from("The sent tab also keeps requests addressed to one agent."),
            highlighted_actions("Press M again to close it and continue.", &["M"]),
        ],
    }
}

fn callout_label(text: &'static str) -> Line<'static> {
    Line::from(Span::styled(
        text,
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))
}

fn policy_line(label: &'static str, value: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{label:<9}"),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("= "),
        Span::styled(value, Style::default().fg(Color::White)),
    ])
}

fn state_legend_line(state: AgentState, meaning: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        mock_state_span(state, Color::Reset),
        Span::raw("  "),
        Span::styled(meaning, Style::default().fg(Color::White)),
    ])
}

fn selected_style() -> Style {
    Style::default()
        .fg(Color::White)
        .bg(Color::Rgb(18, 83, 108))
        .add_modifier(Modifier::BOLD)
}

fn dialog_block(title: &str, color: Color) -> Block<'_> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color))
        .style(Style::default().fg(Color::White).bg(Color::Rgb(12, 19, 31)))
        .padding(ratatui::widgets::Padding::horizontal(1))
}

fn callout_rect(area: Rect, app: &TourApp) -> Rect {
    let step = app.current();
    let compact = area.width < 100 || area.height < 23;
    // The terminal-setup hint only appears after repeated misses, so the
    // callout grows for it rather than reserving the rows on every step.
    let bypass_rows = if app.offer_alt_bypass() { 5 } else { 0 };
    if compact {
        let height = (if step == TourStep::Finish { 12 } else { 10 } + bypass_rows)
            .min(area.height.saturating_sub(2));
        let y = if step == TourStep::Shortcuts {
            area.y + 2
        } else {
            area.y + area.height.saturating_sub(height + 1)
        };
        return Rect::new(area.x + 2, y, area.width.saturating_sub(4), height);
    }
    if step == TourStep::Finish {
        return centered_rect(
            area,
            area.width.saturating_sub(6).min(78),
            area.height.saturating_sub(4).min(15),
        );
    }
    let width = area.width.saturating_mul(42) / 100;
    let height = 12;
    match step {
        TourStep::Work | TourStep::Agents | TourStep::States => Rect::new(
            area.x + area.width - width - 2,
            area.y + 5,
            width,
            (height + bypass_rows).min(area.height.saturating_sub(6)),
        ),
        TourStep::Preview | TourStep::Mcp => {
            Rect::new(area.x + 2, area.y + 8, width.max(52), height)
        }
        TourStep::Shortcuts => centered_rect(area, area.width.min(78), 11),
        TourStep::NewWork if app.new_work_stage == NewWorkStage::Form => {
            Rect::new(area.x + 4, area.y + 4, area.width - 8, 8)
        }
        TourStep::Collaboration
            if matches!(
                app.collaboration_stage,
                CollaborationStage::Composer | CollaborationStage::MailboxOpen
            ) =>
        {
            Rect::new(area.x + 4, area.y + 4, area.width - 8, 9)
        }
        TourStep::NewWork | TourStep::Collaboration => {
            Rect::new(area.x + 4, area.y + area.height - 10, area.width - 8, 9)
        }
        TourStep::Finish => unreachable!(),
    }
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

struct TerminalGuard<B: Backend + Write> {
    terminal: Option<Terminal<B>>,
}

impl<B: Backend + Write> TerminalGuard<B> {
    fn new(terminal: Terminal<B>) -> Self {
        Self {
            terminal: Some(terminal),
        }
    }

    fn terminal_mut(&mut self) -> &mut Terminal<B> {
        self.terminal.as_mut().expect("terminal present")
    }
}

impl<B: Backend + Write> Drop for TerminalGuard<B> {
    fn drop(&mut self) {
        if let Some(mut terminal) = self.terminal.take() {
            let _ = disable_raw_mode();
            let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
            let _ = terminal.show_cursor();
        }
    }
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().context("enabling raw terminal mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)
        .inspect_err(|_| {
            let _ = execute!(stdout, LeaveAlternateScreen);
            let _ = disable_raw_mode();
        })
        .context("entering alternate terminal screen")?;
    Terminal::new(CrosstermBackend::new(stdout))
        .inspect_err(|_| {
            let mut stdout = io::stdout();
            let _ = execute!(stdout, LeaveAlternateScreen);
            let _ = disable_raw_mode();
        })
        .context("initializing onboarding terminal")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn rendered(app: &TourApp, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render_tour(frame, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>()
    }

    #[test]
    fn split_csi_sequences_preserve_the_key_the_tour_is_waiting_for() {
        assert_eq!(csi_key('A'), Some(KeyCode::Up));
        assert_eq!(csi_key('B'), Some(KeyCode::Down));
        assert_eq!(csi_key('C'), Some(KeyCode::Right));
        assert_eq!(csi_key('D'), Some(KeyCode::Left));
        assert_eq!(csi_key('H'), Some(KeyCode::Home));
        assert_eq!(csi_key('F'), Some(KeyCode::End));
        assert_eq!(csi_key('~'), None);
    }

    #[test]
    fn interactive_input_tokens_use_the_shared_action_color() {
        let line = highlighted_actions("Press q, not quiet.", &["q"]);
        assert_eq!(line.spans.len(), 3);
        assert_eq!(line.spans[1].content.as_ref(), "q");
        assert_eq!(line.spans[1].style.fg, Some(ACTION_COLOR));
        assert!(line.spans[1].style.add_modifier.contains(Modifier::BOLD));

        let command = action_line("tmux new-session -s muxa-onboarding");
        assert_eq!(command.spans[0].style.fg, Some(ACTION_COLOR));
    }

    #[test]
    fn unified_step_twelve_enters_watch_without_a_second_welcome_gate() {
        let app = TourApp::with_language(false, UiLanguage::Ko);
        assert_eq!(app.current(), TourStep::Work);
        assert_eq!(app.selection, MockSelection::WorkSandbox);
        let screen = rendered(&app, 130, 32).replace(' ', "");
        assert!(screen.contains("12/20"));
        assert!(screen.contains("muxa-sandbox"));
        assert!(screen.contains("cursor는muxa-sandbox에있습니다"));
    }

    #[test]
    fn onboarding_policy_and_workflow_pin_the_domain_model() {
        assert!(POLICY.contains("session = Workspace binding"));
        assert!(POLICY.contains("window  = current Run binding"));
        assert!(POLICY.contains("pane    = agent session binding"));
        assert!(WORKFLOW.contains("muxa work start muxa-onboarding"));
        assert!(WORKFLOW.contains("muxa agent start --workspace muxa --work muxa-onboarding"));
        assert!(POLICY_KO.contains("외부 이슈"));
        assert!(WORKFLOW_KO.contains("같은 work에 다른 agent 추가"));
    }

    #[test]
    fn onboarding_reuses_the_canonical_watch_shortcuts() {
        let shortcuts = crate::watch::help_overlay_text().join("\n");
        assert!(shortcuts.contains("m / M"));
        assert!(shortcuts.contains("a / A"));
        assert!(shortcuts.contains("Alt-K"));
        assert!(shortcuts.contains("n / w / R      new window/pane / work up / rename the row"));
    }

    #[test]
    fn mcp_training_uses_focused_settled_observation() {
        assert!(MCP_PATTERN.contains("until=\"settled\""));
        assert!(MCP_PATTERN.contains("include_capture=true"));
        assert!(MCP_PATTERN.contains("confirm=true"));
    }

    #[test]
    fn fullscreen_tour_connects_dialogs_to_the_mock_watch_regions() {
        let mut app = TourApp::new(false);
        app.step = TourStep::ALL
            .iter()
            .position(|step| *step == TourStep::Work)
            .unwrap();
        let work = rendered(&app, 120, 34);
        assert!(work.contains("muxa watch"));
        assert!(work.contains("Workspace › work › agent"));
        assert!(work.contains("WORKSPACE › WORK"));
        assert!(!work.contains("AGENTS  STATE"));
        assert!(work.contains("muxa-onboarding"));
        assert!(work.contains("navigate between work windows"));
        assert!(work.contains("MOVE BETWEEN WORK WINDOWS"));

        app.step = TourStep::ALL
            .iter()
            .position(|step| *step == TourStep::Preview)
            .unwrap();
        let preview = rendered(&app, 120, 34);
        assert!(preview.contains("Inspector"));
        assert!(preview.contains("CHECK A PANE WITHOUT ATTACHING"));
    }

    #[test]
    fn new_work_practice_uses_the_live_key_and_form_without_command_typing() {
        let mut app = TourApp::new(false);
        app.step = TourStep::ALL
            .iter()
            .position(|step| *step == TourStep::NewWork)
            .unwrap();
        let original = app.step;

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Right, crossterm::event::KeyModifiers::NONE),
        );
        assert_eq!(app.step, original);
        assert!(app.blocked_hint());

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('n'), crossterm::event::KeyModifiers::NONE),
        );
        assert_eq!(app.new_work_stage, NewWorkStage::Form);
        assert!(rendered(&app, 120, 34).contains("new work + agent"));
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Esc, crossterm::event::KeyModifiers::NONE),
        );
        assert_eq!(app.current(), TourStep::Collaboration);
    }

    #[test]
    fn finish_uses_q_without_a_command_prompt() {
        let mut app = TourApp::new(false);
        app.step = TourStep::ALL.len() - 1;
        let finish = rendered(&app, 120, 34);
        assert!(finish.contains("THE MODEL TO REMEMBER"));
        assert!(!finish.contains("ONE LAST COMMAND"));
        assert!(!finish.contains("$ █"));
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Backspace, crossterm::event::KeyModifiers::NONE),
        );
        assert_eq!(app.current(), TourStep::Mcp);

        app.step = TourStep::ALL.len() - 1;
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('q'), crossterm::event::KeyModifiers::NONE),
        );
        assert!(app.done);
    }

    #[test]
    fn mock_session_gutter_uses_canonical_watch_icons_on_the_left() {
        let mut app = TourApp::new(true);
        app.step = TourStep::ALL
            .iter()
            .position(|step| *step == TourStep::States)
            .unwrap();
        let screen = rendered(&app, 120, 34);
        for state in [
            AgentState::Working,
            AgentState::WaitingInput,
            AgentState::Idle,
            AgentState::Error,
        ] {
            assert!(screen.contains(crate::state_icon(state)));
        }
        assert!(screen.contains("▶ ●   muxa › onboarding"));
        assert!(screen.contains("○     muxa › sandbox"));
        assert!(!screen.contains("AGENTS  STATE"));
        assert!(!screen.contains("WORKING"));
        assert!(!screen.contains("WAITING"));
    }

    #[test]
    fn collaboration_keys_preview_message_and_mailbox_dialogs() {
        let mut app = TourApp::new(false);
        app.step = TourStep::ALL
            .iter()
            .position(|step| *step == TourStep::Collaboration)
            .unwrap();

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('m'), crossterm::event::KeyModifiers::NONE),
        );
        assert!(rendered(&app, 120, 34).contains("message → claude@%43"));
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Backspace, crossterm::event::KeyModifiers::NONE),
        );
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('M'), crossterm::event::KeyModifiers::SHIFT),
        );
        assert!(rendered(&app, 120, 34).contains("sent 1"));
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('M'), crossterm::event::KeyModifiers::SHIFT),
        );
        assert_eq!(app.current(), TourStep::Mcp);
    }

    fn at_state_step() -> TourApp {
        let mut app = TourApp::new(false);
        app.step = TourStep::ALL
            .iter()
            .position(|step| *step == TourStep::States)
            .unwrap();
        app
    }

    #[test]
    fn step_table_walks_every_gate_with_its_published_key() {
        let table: Vec<Vec<String>> = tmux::step_table()
            .into_iter()
            .chain(watch_step_table())
            .collect();
        assert_eq!(table.len(), UNIFIED_STEP_COUNT);
        for (index, keys) in table.iter().enumerate() {
            assert!(!keys.is_empty(), "step {} publishes no key", index + 1);
            // The walk caps a step at eight stages, which it only reaches when
            // a gate refuses the very key it told the learner to press.
            assert!(
                keys.len() < 8,
                "step {} never accepted its published key: {keys:?}",
                index + 1
            );
        }
        // The gate that stranded the tour in issue #76 — the contract the
        // shell fallback is held to by scripts/onboarding-parity.py.
        assert_eq!(table[13], vec!["Alt-T".to_string()]);
        assert_eq!(table[5], vec!["\u{2192}".to_string()]);
    }

    #[test]
    fn step_table_tsv_lists_one_numbered_line_per_step() {
        let tsv = step_table_tsv();
        let lines: Vec<&str> = tsv.lines().collect();
        assert_eq!(lines.len(), UNIFIED_STEP_COUNT);
        for (index, line) in lines.iter().enumerate() {
            let mut fields = line.split('\t');
            assert_eq!(fields.next(), Some((index + 1).to_string().as_str()));
            assert!(fields.next().is_some(), "step {} has no key", index + 1);
        }
        assert!(tsv.contains("14\tAlt-T\n"));
    }

    #[test]
    fn state_step_accepts_the_macos_option_compose_glyph() {
        // A stock macOS terminal composes Option+T into `†` and sends no
        // modifier at all, which used to leave the tour with no way forward.
        let mut app = at_state_step();
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('†'), KeyModifiers::NONE),
        );
        assert_eq!(app.current(), TourStep::Preview);
        assert_eq!(app.sort, MockSort::State);
    }

    #[test]
    fn state_step_accepts_the_shifted_compose_glyph() {
        let mut app = at_state_step();
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('ˇ'), KeyModifiers::SHIFT),
        );
        assert_eq!(app.current(), TourStep::Preview);
    }

    #[test]
    fn state_step_still_refuses_a_bare_t() {
        // The tour teaches the real watch binding, so an unmodified `t` — which
        // live watch does not bind — must not stand in for the chord.
        let mut app = at_state_step();
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
        );
        assert_eq!(app.current(), TourStep::States);
        assert!(app.blocked_hint());
    }

    #[test]
    fn state_step_offers_a_bypass_only_after_repeated_misses() {
        let mut app = at_state_step();
        handle_key(&mut app, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(app.current(), TourStep::States);
        assert!(!app.offer_alt_bypass());

        handle_key(&mut app, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert!(app.offer_alt_bypass());
        let screen = rendered(&app, 120, 34);
        assert!(screen.contains("docs/WATCH.md"));

        handle_key(&mut app, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(app.current(), TourStep::Preview);
        assert_eq!(app.sort, MockSort::State);
    }

    #[test]
    fn preview_step_accepts_the_option_compose_glyph() {
        let mut app = TourApp::new(false);
        app.step = TourStep::ALL
            .iter()
            .position(|step| *step == TourStep::Preview)
            .unwrap();
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('π'), KeyModifiers::NONE),
        );
        assert_eq!(app.panel, MockPanel::Preview);
        assert_eq!(app.current(), TourStep::Shortcuts);
    }

    #[test]
    fn alt_chord_matching_covers_meta_and_compose_forms() {
        assert!(is_alt_chord(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::ALT),
            't'
        ));
        assert!(is_alt_chord(
            KeyEvent::new(KeyCode::Char('T'), KeyModifiers::ALT | KeyModifiers::SHIFT),
            't'
        ));
        assert!(is_alt_chord(
            KeyEvent::new(KeyCode::Char('†'), KeyModifiers::NONE),
            't'
        ));
        assert!(!is_alt_chord(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
            't'
        ));
        assert!(!is_alt_chord(
            KeyEvent::new(KeyCode::Char('π'), KeyModifiers::NONE),
            't'
        ));
        // Ctrl-composed input never stands in for a compose glyph.
        assert!(!is_alt_chord(
            KeyEvent::new(KeyCode::Char('†'), KeyModifiers::CONTROL),
            't'
        ));
    }

    #[test]
    fn option_compose_table_is_unambiguous() {
        for (index, (composed, _)) in OPTION_COMPOSED_LETTERS.iter().enumerate() {
            assert!(
                !composed.is_ascii(),
                "{composed} would shadow a plain tour key"
            );
            assert!(
                OPTION_COMPOSED_LETTERS
                    .iter()
                    .skip(index + 1)
                    .all(|(other, _)| other != composed),
                "{composed} is mapped twice"
            );
        }
    }

    #[test]
    fn guided_tour_advances_with_the_live_watch_keys() {
        let mut app = TourApp::new(false);
        let press = |app: &mut TourApp, code, modifiers| {
            handle_key(app, KeyEvent::new(code, modifiers));
        };

        // Enter is deliberately not a generic "next" key once a live
        // watch action is being taught.
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.current(), TourStep::Work);
        assert!(app.blocked_hint());

        press(&mut app, KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.current(), TourStep::Agents);
        assert_eq!(app.selection, MockSelection::WorkOnboarding);
        press(&mut app, KeyCode::Char('l'), KeyModifiers::NONE);
        assert_eq!(app.current(), TourStep::States);
        assert_eq!(app.selection, MockSelection::Codex);
        press(&mut app, KeyCode::Char('t'), KeyModifiers::ALT);
        assert_eq!(app.current(), TourStep::Preview);
        assert_eq!(app.sort, MockSort::State);

        press(&mut app, KeyCode::Char('o'), KeyModifiers::NONE);
        assert_eq!(app.current(), TourStep::Shortcuts);
        assert_eq!(app.panel, MockPanel::Preview);
        press(&mut app, KeyCode::Char('o'), KeyModifiers::NONE);
        assert_eq!(app.panel, MockPanel::None);
        press(&mut app, KeyCode::Char('?'), KeyModifiers::SHIFT);
        assert_eq!(app.panel, MockPanel::Help);
        press(&mut app, KeyCode::Char('?'), KeyModifiers::SHIFT);
        assert_eq!(app.current(), TourStep::NewWork);

        press(&mut app, KeyCode::Char('n'), KeyModifiers::NONE);
        assert_eq!(app.new_work_stage, NewWorkStage::Form);
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.current(), TourStep::Collaboration);

        press(&mut app, KeyCode::Char('m'), KeyModifiers::NONE);
        assert_eq!(app.collaboration_stage, CollaborationStage::Composer);
        press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(app.collaboration_stage, CollaborationStage::Mailbox);
        press(&mut app, KeyCode::Char('M'), KeyModifiers::SHIFT);
        assert_eq!(app.collaboration_stage, CollaborationStage::MailboxOpen);
        press(&mut app, KeyCode::Char('M'), KeyModifiers::SHIFT);
        assert_eq!(app.current(), TourStep::Mcp);
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(app.current(), TourStep::Finish);
        press(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(app.done);
    }

    #[test]
    fn korean_locale_and_f2_switch_localize_the_tour() {
        assert_eq!(language_from_locale("ko_KR.UTF-8"), UiLanguage::Ko);
        assert_eq!(language_from_locale("ko-KR"), UiLanguage::Ko);
        assert_eq!(language_from_locale("C.UTF-8"), UiLanguage::En);

        let mut app = TourApp::with_language(false, UiLanguage::Ko);
        let korean = rendered(&app, 120, 34);
        let compact_korean = korean.replace(' ', "");
        assert!(compact_korean.contains("WORKWINDOW사이를이동합니다"));
        assert!(korean.contains("F2 English"));

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::F(2), crossterm::event::KeyModifiers::NONE),
        );
        assert_eq!(app.language, UiLanguage::En);
        assert!(rendered(&app, 120, 34).contains("MOVE BETWEEN WORK WINDOWS"));
    }

    #[test]
    fn compact_terminals_render_a_resize_message_without_panicking() {
        let screen = rendered(&TourApp::new(false), 60, 16);
        assert!(screen.contains("needs a little more room"));
        assert!(screen.contains("68 × 20"));

        let compact = rendered(&TourApp::new(false), 80, 24);
        assert!(compact.contains("muxa watch"));
        assert!(compact.contains("muxa-onboarding"));
        assert!(compact.contains("navigate between work windows"));
    }
}
