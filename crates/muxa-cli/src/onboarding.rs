//! Fullscreen and printable Muxa onboarding.
//!
//! Interactive onboarding is one inert shell → tmux → Muxa scenario. It first
//! preserves virtual tmux command and layout effects, then hands the same
//! fullscreen terminal to a stable `muxa watch` mock with location-aware
//! dialogs. `--print` remains available for scripts and accessibility.

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
session = work/ticket\n\
pane    = agent\n\
window  = layout only\n\
\n\
Muxa owns tmux lifecycle, location, state, and collaboration routing.\n\
Agents own files, code, Git, tests, and reasoning.\n\
One ticket reuses one managed session; it never silently becomes ticket-2.";

const WORKFLOW: &str = "\
1. Start the work with its first agent:\n\
   muxa work start CAL-7041 --cwd /repo --agent codex --role implementer --prompt \"Implement CAL-7041\"\n\
\n\
2. Add another agent to the same work:\n\
   muxa agent start --work CAL-7041 --agent claude --role reviewer --prompt \"Review the current changes\"\n\
\n\
3. Inspect or operate it:\n\
   muxa work list\n\
   muxa work show CAL-7041\n\
   muxa watch\n\
   muxa agent control --pane %42 --action interrupt\n\
\n\
4. Close explicitly when the work is finished:\n\
   muxa work close CAL-7041";

const MCP_PATTERN: &str = "\
Use the existing muxa MCP server; do not add a second tmux MCP.\n\
\n\
muxa_start_agent(work=\"CAL-7041\", agent=\"codex\", role=\"reviewer\", prompt=\"Review ...\")\n\
muxa_wait_for_change(pane=\"%42\", until=\"settled\", include_capture=true)\n\
muxa_status(pane=\"%42\", include_capture=true, history_limit=1)\n\
muxa_manage_tmux(action=\"interrupt_agent\", pane=\"%42\")\n\
\n\
terminate_agent and close_work require confirm=true. Muxa refuses to terminate unmanaged panes.";

const SAFETY: &str = "\
Muxa deliberately does not expose arbitrary shell or generic tmux commands.\n\
Use exact pane ids for agent control. Destructive actions require confirmation.\n\
Use collaboration review + read_only by default; grant execute only with narrow paths.";

const POLICY_KO: &str = "\
session = work/ticket\n\
pane    = agent\n\
window  = 화면 배치 전용\n\
\n\
Muxa는 tmux 생명주기, 위치, 상태, 협업 routing을 관리합니다.\n\
Agent는 파일, 코드, Git, 테스트와 추론을 담당합니다.\n\
같은 ticket은 하나의 managed session을 재사용하며 ticket-2를 조용히 만들지 않습니다.";

const WORKFLOW_KO: &str = "\
1. 첫 agent와 함께 work 시작:\n\
   muxa work start CAL-7041 --cwd /repo --agent codex --role implementer --prompt \"Implement CAL-7041\"\n\
\n\
2. 같은 work에 다른 agent 추가:\n\
   muxa agent start --work CAL-7041 --agent claude --role reviewer --prompt \"Review the current changes\"\n\
\n\
3. 조회 또는 제어:\n\
   muxa work list\n\
   muxa work show CAL-7041\n\
   muxa watch\n\
   muxa agent control --pane %42 --action interrupt\n\
\n\
4. work가 끝나면 명시적으로 닫기:\n\
   muxa work close CAL-7041";

const MCP_PATTERN_KO: &str = "\
별도 tmux MCP를 추가하지 말고 기존 Muxa MCP server를 사용합니다.\n\
\n\
muxa_start_agent(work=\"CAL-7041\", agent=\"codex\", role=\"reviewer\", prompt=\"Review ...\")\n\
muxa_wait_for_change(pane=\"%42\", until=\"settled\", include_capture=true)\n\
muxa_status(pane=\"%42\", include_capture=true, history_limit=1)\n\
muxa_manage_tmux(action=\"interrupt_agent\", pane=\"%42\")\n\
\n\
terminate_agent와 close_work는 confirm=true가 필요하며 unmanaged pane은 종료하지 않습니다.";

const SAFETY_KO: &str = "\
Muxa는 임의 shell 실행이나 범용 tmux 명령을 노출하지 않습니다.\n\
Agent 제어에는 정확한 pane id를 사용하고 파괴적 동작은 확인을 요구합니다.\n\
협업은 review + read_only를 기본으로 하고 execute는 좁은 경로에만 허용합니다.";

const SECTIONS: &[Section] = &[
    Section {
        title_en: "1 · Mental model",
        body_en: POLICY,
        title_ko: "1 · 운영 모델",
        body_ko: POLICY_KO,
    },
    Section {
        title_en: "2 · Default workflow",
        body_en: WORKFLOW,
        title_ko: "2 · 기본 작업 흐름",
        body_ko: WORKFLOW_KO,
    },
    Section {
        title_en: "3 · Agent-facing MCP pattern",
        body_en: MCP_PATTERN,
        title_ko: "3 · Agent용 MCP 패턴",
        body_ko: MCP_PATTERN_KO,
    },
    Section {
        title_en: "4 · Safety boundary",
        body_en: SAFETY,
        title_ko: "4 · 안전 경계",
        body_ko: SAFETY_KO,
    },
];

pub fn run(args: Args) -> Result<()> {
    apply_icon_preference();
    let mode = Mode::detect(args.print);
    let language = args.lang.resolve();
    match mode {
        Mode::Print => {
            tmux::print_guide(language);
            println!("\n{}\n", "=".repeat(72));
            print_guide(language);
        }
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
            "Muxa 온보딩"
        } else {
            "Muxa onboarding"
        }
    );
    println!("===============");
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
        println!("\n5 · muxa watch 단축키\n----------------------");
        for line in korean_watch_help() {
            println!("{line}");
        }
        println!("\n다음: muxa watch를 실행하거나 tmux prefix+s를 누르세요.");
    } else {
        println!("\n5 · muxa watch shortcuts\n------------------------");
        for line in crate::watch::help_overlay_text() {
            println!("{line}");
        }
        println!("\nNext: run muxa watch or press tmux prefix+s.");
    }
}

fn korean_watch_help() -> &'static [&'static str] {
    &[
        "이동",
        "  ↑/↓ · j/k       session/child 이동",
        "  ←/→ · h/l       parent / 첫 child agent",
        "  Enter           선택한 pane에 attach",
        "  n               work session + agent 생성/재사용",
        "",
        "조회와 협업",
        "  o / Alt-P       preview 열기",
        "  m / M           선택한 agent에 메시지 / mailbox",
        "  a / A           ask / history",
        "  Alt-S/L/D/T     session / latest / duration / state 정렬",
        "  ? / F1          전체 도움말",
    ]
}

fn interactive_guide(no_quiz: bool, language: UiLanguage) -> Result<()> {
    let terminal = setup_terminal()?;
    let mut guard = TerminalGuard::new(terminal);
    guard.terminal_mut().hide_cursor()?;
    if !tmux::interactive_guide(guard.terminal_mut(), no_quiz, language)? {
        return Ok(());
    }
    let mut app = TourApp::after_tmux(no_quiz, language);

    while !app.done {
        guard
            .terminal_mut()
            .draw(|frame| render_tour(frame, &app))?;
        if let Event::Key(key) = event::read().context("reading onboarding input")? {
            handle_key(&mut app, key);
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TourStep {
    Welcome,
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
    const ALL: [Self; 10] = [
        Self::Welcome,
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
    Work7041,
    Work7088,
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
    blocked_hint: bool,
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
            // The first exercise moves from the idle work to CAL-7041 with
            // the same `j`/Down navigation used by live watch.
            selection: MockSelection::Work7088,
            sort: MockSort::Latest,
            panel: MockPanel::None,
            new_work_stage: NewWorkStage::Shortcut,
            collaboration_stage: CollaborationStage::Message,
            blocked_hint: false,
            done: false,
        }
    }

    fn after_tmux(no_quiz: bool, language: UiLanguage) -> Self {
        let mut app = Self::with_language(no_quiz, language);
        app.step = 1;
        app
    }

    fn current(&self) -> TourStep {
        TourStep::ALL[self.step]
    }

    fn guided(&self) -> bool {
        self.mode == TourMode::Guided
    }

    fn ko(&self) -> bool {
        self.language == UiLanguage::Ko
    }

    fn toggle_language(&mut self) {
        self.language = match self.language {
            UiLanguage::En => UiLanguage::Ko,
            UiLanguage::Ko => UiLanguage::En,
        };
        self.blocked_hint = false;
    }

    fn advance(&mut self) {
        self.blocked_hint = false;
        if self.step + 1 == TourStep::ALL.len() {
            self.done = true;
        } else {
            self.step += 1;
        }
    }

    fn previous(&mut self) {
        self.blocked_hint = false;
        self.step = self.step.saturating_sub(1);
    }
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
        app.blocked_hint = false;
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
            _ => app.blocked_hint = true,
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
        TourStep::Welcome
            if matches!(
                key.code,
                KeyCode::Enter | KeyCode::Right | KeyCode::Char('l')
            ) =>
        {
            app.advance();
            true
        }
        TourStep::Work
            if key.code == KeyCode::Down || (plain && key.code == KeyCode::Char('j')) =>
        {
            app.selection = MockSelection::Work7041;
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
        TourStep::States
            if matches!(key.code, KeyCode::Char('t' | 'T'))
                && key.modifiers.contains(KeyModifiers::ALT) =>
        {
            app.sort = MockSort::State;
            app.advance();
            true
        }
        TourStep::Preview
            if (plain && key.code == KeyCode::Char('o'))
                || (key.code == KeyCode::Char('p')
                    && key.modifiers.contains(KeyModifiers::ALT)) =>
        {
            app.panel = MockPanel::Preview;
            app.advance();
            true
        }
        TourStep::Shortcuts
            if app.panel == MockPanel::Preview && plain && key.code == KeyCode::Char('o') =>
        {
            app.panel = MockPanel::None;
            app.blocked_hint = false;
            true
        }
        TourStep::Shortcuts
            if app.panel == MockPanel::None
                && matches!(key.code, KeyCode::Char('?') | KeyCode::F(1)) =>
        {
            app.panel = MockPanel::Help;
            app.blocked_hint = false;
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
    app.blocked_hint = false;
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
    app.blocked_hint = false;
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
                "  2 sessions  ",
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
        "  SESSION                 DUR    ACT    SUMMARY",
        Style::default().fg(Color::DarkGray),
    ))];
    if app.sort == MockSort::State {
        lines.extend(mock_cal_7041_rows(app));
        lines.push(mock_cal_7088_row(app));
    } else {
        lines.push(mock_cal_7088_row(app));
        lines.extend(mock_cal_7041_rows(app));
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
                .title(" Sessions ")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(border)),
        ),
        area,
    );
}

fn mock_cal_7041_rows(app: &TourApp) -> Vec<Line<'static>> {
    let selected = app.selection == MockSelection::Work7041;
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
            "   CAL-7041          12m    8s     harden checkout auth",
            style,
        ),
    ])];
    if matches!(
        app.selection,
        MockSelection::Work7041 | MockSelection::Codex
    ) {
        let codex_selected = app.selection == MockSelection::Codex;
        rows.push(Line::from(Span::styled(
            if codex_selected {
                ">   └─ cal-7041:0.0   -      8s     implement checkout hardening"
            } else {
                "    └─ cal-7041:0.0   -      8s     implement checkout hardening"
            },
            row_style(codex_selected),
        )));
        rows.push(Line::from(Span::styled(
            "    └─ cal-7041:1.0   -      2m     review public-read boundary",
            Style::default().fg(Color::Gray),
        )));
    }
    rows
}

fn mock_cal_7088_row(app: &TourApp) -> Line<'static> {
    let selected = app.selection == MockSelection::Work7088;
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
            "     CAL-7088          31m    1m     dashboard authentication",
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
/// palette. Session summaries put it in the left edge of the SESSION cell,
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
        MockSelection::Work7088 => (
            "cal-7088:0.0",
            "IDLE",
            "1m",
            "codex",
            "dashboard authentication",
        ),
        MockSelection::Work7041 => (
            "cal-7041:1.0",
            "WAIT",
            "2m",
            "claude_code",
            "review public-read boundary",
        ),
        MockSelection::Codex => (
            "cal-7041:0.0",
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
            "  ⚙ editing  crates/muxa/src/dashboard/server.rs",
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
                Span::styled("  CAL-7041", Style::default().fg(Color::DarkGray)),
            ]),
            Line::from(""),
            Line::from("› implement checkout hardening"),
            Line::from(""),
            Line::from("  ⚙ read     crates/muxa/src/dashboard/server.rs"),
            Line::from("  ⚙ editing  crates/muxa/src/dashboard/auth.rs"),
            Line::from(""),
            Line::from(Span::styled(
                "  ● working…",
                Style::default().fg(Color::Yellow),
            )),
        ]))
        .block(dialog_block(
            " Preview · cal-7041:0.0 · live pane ",
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
            Line::from("  ↑/↓ · j/k       session/child 이동"),
            Line::from("  ←/→ · h/l       parent / 첫 child agent"),
            Line::from("  Enter           선택한 pane에 attach"),
            Line::from("  n               work session + agent 생성/재사용"),
            Line::from(""),
            Line::from(Span::styled(
                "조회와 협업",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from("  o / Alt-P       preview 열기"),
            Line::from("  m / M           선택한 agent에 메시지 / mailbox"),
            Line::from("  a / A           ask / history"),
            Line::from("  Alt-S/L/D/T     session / latest / duration / state 정렬"),
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
            Line::from("  ↑/↓ · j/k       move sessions/children"),
            Line::from("  ←/→ · h/l       parent / first child agent"),
            Line::from("  Enter           attach to selected pane"),
            Line::from("  n               new/reused work session + agent"),
            Line::from(""),
            Line::from(Span::styled(
                "Commands & inspection",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from("  o / Alt-P       open preview overlay"),
            Line::from("  m / M           message selected agent / mailbox"),
            Line::from("  a / A           ask / history"),
            Line::from("  Alt-S/L/D/T     session / latest / duration / state"),
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
    let height = area.height.min(7);
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
            Line::from("  ticket CAL-7041"),
            Line::from("  agent  ◂  codex  ▸"),
            Line::from("  prompt Implement CAL-7041"),
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
        app.step + 1,
        TourStep::ALL.len(),
        step_title(step, app.language)
    );
    let body = step_body(app);
    let footer = callout_footer(app);
    frame.render_widget(
        Paragraph::new(body).wrap(Wrap { trim: false }).block(
            dialog_block(&title, Color::Cyan)
                .title_bottom(Line::from(footer).alignment(Alignment::Center)),
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
        TourStep::Welcome => " Enter begin · F2 한국어 · Esc quit ",
        TourStep::Work => " j/↓ move to CAL-7041 · ← back · Esc quit ",
        TourStep::Agents => " l/→ enter child agent · ← back · Esc quit ",
        TourStep::States => " Alt-T sort by state · ← back · Esc quit ",
        TourStep::Preview => " o open preview · ← back · Esc quit ",
        TourStep::Shortcuts if app.panel == MockPanel::Preview => {
            " o close preview · Esc quits tour "
        }
        TourStep::Shortcuts if app.panel == MockPanel::Help => " ?/F1 close help and continue ",
        TourStep::Shortcuts => " ?/F1 open full help · ← back · Esc quit ",
        TourStep::NewWork if app.new_work_stage == NewWorkStage::Shortcut => {
            " n open new-work form · ← back · Esc quit "
        }
        TourStep::NewWork if app.new_work_stage == NewWorkStage::Form => {
            " Esc close mock form and continue "
        }
        TourStep::NewWork => " n open form · Esc close form and continue ",
        TourStep::Collaboration if app.collaboration_stage == CollaborationStage::Message => {
            " m open selected-peer composer · ← back · Esc quit "
        }
        TourStep::Collaboration if app.collaboration_stage == CollaborationStage::Composer => {
            " empty Backspace close composer "
        }
        TourStep::Collaboration if app.collaboration_stage == CollaborationStage::Mailbox => {
            " M open mailbox "
        }
        TourStep::Collaboration => " M close mailbox and continue ",
        TourStep::Mcp => " l/→ continue · ← back · Esc quit ",
        TourStep::Finish => " q finish · q also quits watch ",
    };
    let ko = match app.current() {
        TourStep::Welcome => " Enter 시작 · F2 English · Esc 종료 ",
        TourStep::Work => " j/↓ CAL-7041로 이동 · ← 이전 · Esc 종료 ",
        TourStep::Agents => " l/→ child agent 선택 · ← 이전 · Esc 종료 ",
        TourStep::States => " Alt-T 상태순 정렬 · ← 이전 · Esc 종료 ",
        TourStep::Preview => " o preview 열기 · ← 이전 · Esc 종료 ",
        TourStep::Shortcuts if app.panel == MockPanel::Preview => {
            " o preview 닫기 · Esc 온보딩 종료 "
        }
        TourStep::Shortcuts if app.panel == MockPanel::Help => " ?/F1 도움말 닫고 계속 ",
        TourStep::Shortcuts => " ?/F1 전체 도움말 · ← 이전 · Esc 종료 ",
        TourStep::NewWork if app.new_work_stage == NewWorkStage::Shortcut => {
            " n 새 work form 열기 · ← 이전 · Esc 종료 "
        }
        TourStep::NewWork if app.new_work_stage == NewWorkStage::Form => {
            " Esc mock form을 닫고 계속 "
        }
        TourStep::NewWork => " n form 열기 · Esc 닫고 계속 ",
        TourStep::Collaboration if app.collaboration_stage == CollaborationStage::Message => {
            " m 선택 peer composer · ← 이전 · Esc 종료 "
        }
        TourStep::Collaboration if app.collaboration_stage == CollaborationStage::Composer => {
            " 빈 composer에서 Backspace로 닫기 "
        }
        TourStep::Collaboration if app.collaboration_stage == CollaborationStage::Mailbox => {
            " M mailbox 열기 "
        }
        TourStep::Collaboration => " M mailbox 닫고 계속 ",
        TourStep::Mcp => " l/→ 계속 · ← 이전 · Esc 종료 ",
        TourStep::Finish => " q 완료 · q는 watch 종료 키 ",
    };
    tr(app.language, en, ko)
}

fn step_title(step: TourStep, language: UiLanguage) -> &'static str {
    let en = match step {
        TourStep::Welcome => "learn on a safe mock",
        TourStep::Work => "one session = one work",
        TourStep::Agents => "one pane = one agent",
        TourStep::States => "state tells you what to do",
        TourStep::Preview => "inspect without attaching",
        TourStep::Shortcuts => "actions stay at the bottom",
        TourStep::NewWork => "new work: real key and form",
        TourStep::Collaboration => "message and mailbox muscle memory",
        TourStep::Mcp => "agents use the same control plane",
        TourStep::Finish => "ready for live watch",
    };
    let ko = match step {
        TourStep::Welcome => "안전한 mock에서 배우기",
        TourStep::Work => "session 하나 = work 하나",
        TourStep::Agents => "pane 하나 = agent 하나",
        TourStep::States => "상태가 다음 행동을 알려줍니다",
        TourStep::Preview => "attach 없이 확인하기",
        TourStep::Shortcuts => "하단에 보이는 상황별 동작",
        TourStep::NewWork => "실제 키로 새 work form 열기",
        TourStep::Collaboration => "메시지와 mailbox 익히기",
        TourStep::Mcp => "agent도 같은 control plane 사용",
        TourStep::Finish => "live watch를 사용할 준비 완료",
    };
    tr(language, en, ko)
}

fn step_body(app: &TourApp) -> Text<'static> {
    let mut lines = step_lines(app);
    if app.blocked_hint {
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
    Text::from(lines)
}

fn required_action(app: &TourApp) -> &'static str {
    let en = match app.current() {
        TourStep::Welcome => "press Enter to begin",
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
        TourStep::Welcome => "Enter를 눌러 시작하세요",
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
        TourStep::Welcome => vec![
            callout_label("↙ THIS IS A SAFE MUXA WATCH REPLICA"),
            Line::from(""),
            Line::from("Nothing here touches your real tmux sessions."),
            Line::from("Press Enter once; later steps require the real watch key."),
            Line::from("Press F2 at any time to switch to 한국어."),
            Line::from(""),
            policy_line("SESSION", "work / ticket"),
            policy_line("PANE", "agent"),
            policy_line("WINDOW", "layout only"),
        ],
        TourStep::Work => vec![
            callout_label("← MOVE THE REAL WATCH CURSOR"),
            Line::from(""),
            Line::from("CAL-7088 is selected. Press j or ↓ to reach CAL-7041."),
            Line::from("Each session row is one work/ticket identity."),
            Line::from("Starting CAL-7041 again reuses that same session."),
        ],
        TourStep::Agents => vec![
            callout_label("← ENTER THE SESSION TREE"),
            Line::from(""),
            Line::from("The selected work expanded to its agent panes."),
            Line::from("Press l or → to select its first child agent."),
            Line::from("h/← returns to the parent; windows are layout only."),
        ],
        TourStep::States => vec![
            callout_label("← STATE LIVES LEFT OF THE SESSION NAME"),
            Line::from(""),
            state_legend_line(AgentState::Working, "working — leave it alone"),
            state_legend_line(AgentState::WaitingInput, "waiting — it needs input"),
            state_legend_line(AgentState::Idle, "idle — its turn settled"),
            state_legend_line(AgentState::Error, "error — inspect the pane"),
            Line::from("Press Alt-T to apply watch's state/attention sort."),
        ],
        TourStep::Preview => vec![
            callout_label("→ INSPECT WITHOUT ATTACHING"),
            Line::from(""),
            Line::from("The wide inspector stays beside the 50/50 session list."),
            Line::from("Press o now to open the selected pane preview."),
            Line::from("Enter attaches when you need the real terminal."),
        ],
        TourStep::Shortcuts if app.panel == MockPanel::Preview => vec![
            callout_label("PREVIEW IS AN ACTUAL WATCH OVERLAY"),
            Line::from(""),
            Line::from("The table remains behind it so you keep your place."),
            Line::from("Press o again to close the preview."),
        ],
        TourStep::Shortcuts if app.panel == MockPanel::Help => vec![
            callout_label("? OPENS THE COMPLETE LIVE KEY MAP"),
            Line::from(""),
            Line::from("The help content comes from watch's shortcut source."),
            Line::from("Press ? or F1 again to close it and continue."),
        ],
        TourStep::Shortcuts => vec![
            callout_label("↓ THE FOOTER TEACHES CONTEXTUAL ACTIONS"),
            Line::from(""),
            Line::from("The strip now matches the live one-line watch footer."),
            Line::from("Press ? or F1 to open the complete shortcut map."),
        ],
        TourStep::NewWork => new_work_step_lines(app),
        TourStep::Collaboration => collaboration_step_lines(app),
        TourStep::Mcp => vec![
            callout_label("AGENTS USE THE SAME MUXA CONTROL PLANE"),
            Line::from(""),
            Line::from("muxa_start_agent creates/reuses tmux deterministically."),
            Line::from("settled + capture returns the useful final screen."),
            Line::from("No separate tmux MCP or model-written tmux script."),
            Line::from("Press l or → to continue."),
        ],
        TourStep::Finish => vec![
            callout_label("THE MODEL TO REMEMBER"),
            Line::from(""),
            policy_line("SESSION", "work / ticket"),
            policy_line("PANE", "agent"),
            policy_line("WINDOW", "layout only"),
            Line::from(""),
            Line::from(Span::styled(
                "✓ muxa watch — press q to finish (q also quits watch)",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
        ],
    }
}

fn step_lines_ko(app: &TourApp) -> Vec<Line<'static>> {
    match app.current() {
        TourStep::Welcome => vec![
            callout_label("↙ 안전한 MUXA WATCH 복제 화면입니다"),
            Line::from(""),
            Line::from("실제 tmux session에는 아무 변화도 주지 않습니다."),
            Line::from("처음만 Enter를 누르고 이후에는 실제 watch 키로 진행합니다."),
            Line::from("F2를 누르면 언제든 English로 전환할 수 있습니다."),
            Line::from(""),
            policy_line("SESSION", "work / ticket"),
            policy_line("PANE", "agent"),
            policy_line("WINDOW", "화면 배치 전용"),
        ],
        TourStep::Work => vec![
            callout_label("← 실제 WATCH CURSOR를 움직여보세요"),
            Line::from(""),
            Line::from("CAL-7088이 선택되어 있습니다. j 또는 ↓로 CAL-7041로 이동하세요."),
            Line::from("각 session row는 하나의 work/ticket입니다."),
            Line::from("CAL-7041을 다시 시작하면 같은 session을 재사용합니다."),
        ],
        TourStep::Agents => vec![
            callout_label("← SESSION TREE 안으로 들어가세요"),
            Line::from(""),
            Line::from("선택한 work 아래에 agent pane이 펼쳐졌습니다."),
            Line::from("l 또는 →로 첫 child agent를 선택하세요."),
            Line::from("h/←는 parent로 돌아가며 window는 화면 배치 전용입니다."),
        ],
        TourStep::States => vec![
            callout_label("← STATE는 SESSION 이름 왼쪽에 있습니다"),
            Line::from(""),
            state_legend_line(AgentState::Working, "작업 중 — 그대로 두세요"),
            state_legend_line(AgentState::WaitingInput, "입력 대기 — 응답이 필요합니다"),
            state_legend_line(AgentState::Idle, "대기 — turn이 끝났습니다"),
            state_legend_line(AgentState::Error, "오류 — pane을 확인하세요"),
            Line::from("Alt-T로 state/attention 순 정렬을 적용하세요."),
        ],
        TourStep::Preview => vec![
            callout_label("→ ATTACH하지 않고 확인하세요"),
            Line::from(""),
            Line::from("넓은 화면에서는 Inspector가 session 목록 옆에 50/50으로 표시됩니다."),
            Line::from("o를 눌러 선택한 pane의 preview를 여세요."),
            Line::from("실제 terminal이 필요할 때는 Enter로 attach합니다."),
        ],
        TourStep::Shortcuts if app.panel == MockPanel::Preview => vec![
            callout_label("PREVIEW는 실제 WATCH와 같은 OVERLAY입니다"),
            Line::from(""),
            Line::from("뒤에 table이 남아 있어 현재 위치를 잃지 않습니다."),
            Line::from("o를 다시 눌러 preview를 닫으세요."),
        ],
        TourStep::Shortcuts if app.panel == MockPanel::Help => vec![
            callout_label("?로 전체 단축키 지도를 엽니다"),
            Line::from(""),
            Line::from("실제 watch에서 사용하는 주요 키를 한글로 설명합니다."),
            Line::from("? 또는 F1을 다시 눌러 닫고 계속하세요."),
        ],
        TourStep::Shortcuts => vec![
            callout_label("↓ FOOTER에서 현재 가능한 동작을 확인하세요"),
            Line::from(""),
            Line::from("한 줄 footer는 실제 watch와 같은 위치에 있습니다."),
            Line::from("? 또는 F1으로 전체 단축키 도움말을 여세요."),
        ],
        TourStep::NewWork => new_work_step_lines(app),
        TourStep::Collaboration => collaboration_step_lines(app),
        TourStep::Mcp => vec![
            callout_label("AGENT도 같은 MUXA CONTROL PLANE을 사용합니다"),
            Line::from(""),
            Line::from("muxa_start_agent가 tmux를 결정적으로 생성하거나 재사용합니다."),
            Line::from("settled + capture로 유용한 마지막 화면을 받습니다."),
            Line::from("별도 tmux MCP나 model이 작성한 tmux script는 필요하지 않습니다."),
            Line::from("l 또는 →로 계속하세요."),
        ],
        TourStep::Finish => vec![
            callout_label("기억할 운영 모델"),
            Line::from(""),
            policy_line("SESSION", "work / ticket"),
            policy_line("PANE", "agent"),
            policy_line("WINDOW", "화면 배치 전용"),
            Line::from(""),
            Line::from(Span::styled(
                "✓ 준비 완료 — q로 끝내세요. q는 실제 watch 종료 키이기도 합니다.",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
        ],
    }
}

fn new_work_step_lines(app: &TourApp) -> Vec<Line<'static>> {
    if app.ko() {
        return match app.new_work_stage {
            NewWorkStage::Shortcut => vec![
                callout_label("실제 새 WORK 단축키를 눌러보세요"),
                Line::from(""),
                Line::from("n을 누르면 watch의 work + agent 안내 form이 열립니다."),
                Line::from("directory, ticket, agent와 첫 prompt를 입력하는 form입니다."),
            ],
            NewWorkStage::Form => vec![
                callout_label("FORM은 실제 WATCH처럼 하단에 붙습니다"),
                Line::from(""),
                Line::from("Tab/↑/↓로 항목을 바꾸고 ←/→로 agent를 바꿉니다."),
                Line::from("Esc로 이 mock form을 닫고 계속하세요."),
            ],
        };
    }
    match app.new_work_stage {
        NewWorkStage::Shortcut => vec![
            callout_label("PRESS THE REAL NEW-WORK KEY"),
            Line::from(""),
            Line::from("Press n to open watch's guided work + agent form."),
            Line::from("It asks for directory, ticket, agent, and first prompt."),
        ],
        NewWorkStage::Form => vec![
            callout_label("THE FORM IS BOTTOM-ANCHORED LIKE WATCH"),
            Line::from(""),
            Line::from("Tab/↑/↓ changes fields; ←/→ changes the agent."),
            Line::from("Press Esc to close this mock form and continue."),
        ],
    }
}

fn collaboration_step_lines(app: &TourApp) -> Vec<Line<'static>> {
    if app.ko() {
        return match app.collaboration_stage {
            CollaborationStage::Message => vec![
                callout_label("선택한 PEER에게 보내려면 m을 누르세요"),
                Line::from(""),
                Line::from("m은 watch cursor 아래의 정확한 agent를 대상으로 합니다."),
                Line::from("m을 눌러 request composer를 여세요."),
            ],
            CollaborationStage::Composer => vec![
                callout_label("COMPOSER는 빈 상태로 시작합니다"),
                Line::from(""),
                Line::from("kind와 mode는 화면에 보이며 다음에도 유지됩니다."),
                Line::from("내용이 비어 있을 때 Backspace를 눌러 닫으세요."),
            ],
            CollaborationStage::Mailbox => vec![
                callout_label("MAILBOX를 열려면 M을 누르세요"),
                Line::from(""),
                Line::from("M은 받은 요청과 보낸 요청을 열며 b도 alias로 동작합니다."),
                Line::from("지금 M을 누르세요."),
            ],
            CollaborationStage::MailboxOpen => vec![
                callout_label("MAILBOX 열림 — 다시 눌러 닫으세요"),
                Line::from(""),
                Line::from("보낸 요청 tab에는 한 peer에게 보낸 request가 남습니다."),
                Line::from("M을 다시 눌러 닫고 계속하세요."),
            ],
        };
    }
    match app.collaboration_stage {
        CollaborationStage::Message => vec![
            callout_label("PRESS m FOR THE SELECTED PEER"),
            Line::from(""),
            Line::from("m targets the exact agent under the watch cursor."),
            Line::from("Press m to open its request composer."),
        ],
        CollaborationStage::Composer => vec![
            callout_label("THE COMPOSER STARTS EMPTY"),
            Line::from(""),
            Line::from("kind and mode remain visible and remembered."),
            Line::from("Press Backspace while empty to close it."),
        ],
        CollaborationStage::Mailbox => vec![
            callout_label("PRESS M FOR THE MAILBOX"),
            Line::from(""),
            Line::from("M opens incoming and sent requests; b is an alias."),
            Line::from("Press M now."),
        ],
        CollaborationStage::MailboxOpen => vec![
            callout_label("MAILBOX OPEN — TOGGLE IT CLOSED"),
            Line::from(""),
            Line::from("The sent tab keeps the request addressed to one peer."),
            Line::from("Press M again to close it and continue."),
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
    if compact {
        let height = if matches!(step, TourStep::Welcome | TourStep::Finish) {
            12
        } else {
            10
        }
        .min(area.height.saturating_sub(2));
        let y = if step == TourStep::Shortcuts {
            area.y + 2
        } else {
            area.y + area.height.saturating_sub(height + 1)
        };
        return Rect::new(area.x + 2, y, area.width.saturating_sub(4), height);
    }
    if matches!(step, TourStep::Welcome | TourStep::Finish) {
        return centered_rect(
            area,
            area.width.saturating_sub(6).min(78),
            area.height.saturating_sub(4).min(15),
        );
    }
    let width = area.width.saturating_mul(42) / 100;
    let height = 12;
    match step {
        TourStep::Work | TourStep::Agents | TourStep::States => {
            Rect::new(area.x + area.width - width - 2, area.y + 5, width, height)
        }
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
        TourStep::Welcome | TourStep::Finish => unreachable!(),
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
    fn unified_handoff_enters_watch_without_a_second_welcome_gate() {
        let app = TourApp::after_tmux(false, UiLanguage::Ko);
        assert_eq!(app.current(), TourStep::Work);
        assert_eq!(app.selection, MockSelection::Work7088);
        let screen = rendered(&app, 130, 32).replace(' ', "");
        assert!(screen.contains("CAL-7088"));
        assert!(screen.contains("CAL-7088이선택"));
    }

    #[test]
    fn onboarding_policy_and_workflow_pin_the_domain_model() {
        assert!(POLICY.contains("session = work/ticket"));
        assert!(POLICY.contains("pane    = agent"));
        assert!(POLICY.contains("window  = layout only"));
        assert!(WORKFLOW.contains("muxa work start CAL-7041"));
        assert!(WORKFLOW.contains("muxa agent start --work CAL-7041"));
        assert!(POLICY_KO.contains("같은 ticket"));
        assert!(WORKFLOW_KO.contains("같은 work에 다른 agent 추가"));
    }

    #[test]
    fn onboarding_reuses_the_canonical_watch_shortcuts() {
        let shortcuts = crate::watch::help_overlay_text().join("\n");
        assert!(shortcuts.contains("m / M"));
        assert!(shortcuts.contains("a / A"));
        assert!(shortcuts.contains("Alt-K"));
        assert!(shortcuts.contains("n              new/reused work session + agent"));
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
        assert!(work.contains("Sessions"));
        assert!(work.contains("SESSION                 DUR    ACT    SUMMARY"));
        assert!(!work.contains("AGENTS  STATE"));
        assert!(work.contains("CAL-7041"));
        assert!(work.contains("one session = one work"));
        assert!(work.contains("MOVE THE REAL WATCH CURSOR"));

        app.step = TourStep::ALL
            .iter()
            .position(|step| *step == TourStep::Preview)
            .unwrap();
        let preview = rendered(&app, 120, 34);
        assert!(preview.contains("Inspector"));
        assert!(preview.contains("INSPECT WITHOUT ATTACHING"));
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
        assert!(app.blocked_hint);

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
        assert!(screen.contains("▶ ●   CAL-7041"));
        assert!(screen.contains("○     CAL-7088"));
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

    #[test]
    fn guided_tour_advances_with_the_live_watch_keys() {
        let mut app = TourApp::new(false);
        let press = |app: &mut TourApp, code, modifiers| {
            handle_key(app, KeyEvent::new(code, modifiers));
        };

        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.current(), TourStep::Work);

        // Enter is deliberately not a generic "next" key once a live
        // watch action is being taught.
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.current(), TourStep::Work);
        assert!(app.blocked_hint);

        press(&mut app, KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.current(), TourStep::Agents);
        assert_eq!(app.selection, MockSelection::Work7041);
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
        assert!(compact_korean.contains("안전한MUXAWATCH복제화면"));
        assert!(korean.contains("F2 English"));

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::F(2), crossterm::event::KeyModifiers::NONE),
        );
        assert_eq!(app.language, UiLanguage::En);
        assert!(rendered(&app, 120, 34).contains("SAFE MUXA WATCH REPLICA"));
    }

    #[test]
    fn compact_terminals_render_a_resize_message_without_panicking() {
        let screen = rendered(&TourApp::new(false), 60, 16);
        assert!(screen.contains("needs a little more room"));
        assert!(screen.contains("68 × 20"));

        let compact = rendered(&TourApp::new(false), 80, 24);
        assert!(compact.contains("muxa watch"));
        assert!(compact.contains("CAL-7041"));
        assert!(compact.contains("learn on a safe mock"));
    }
}
