//! Safe tmux-first steps 1–11 of the unified `muxa onboard` scenario.
//!
//! The tour begins in an inert shell, enters a virtual tmux session, then
//! detects one real prefix-only press. Inside tmux, the mock observes the
//! current client's transition to the prefix key table and immediately returns
//! that client to the root table before asking for any suffix. Later drills use
//! suffix keys only, so no live binding is executed.

use super::{
    action_line, action_style, centered_rect, dialog_block, highlighted_actions, tr, UiLanguage,
};
use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use muxa::AgentState;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::env;
use std::io::Stdout;
use std::process::Command;
use std::time::Duration;

const NEW_SESSION_COMMAND: &str = "tmux new-session -s muxa-onboarding";
const ATTACH_COMMAND: &str = "tmux attach -t muxa-onboarding";

#[derive(Debug, Clone)]
struct DetectedPrefix {
    tmux_key: String,
    display: String,
}

fn detect_tmux_prefix() -> DetectedPrefix {
    let tmux_key = Command::new("tmux")
        .args(["show-options", "-gv", "prefix"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|raw| raw.trim().to_string())
        .filter(|prefix| !prefix.is_empty())
        .unwrap_or_else(|| "C-b".to_string());
    DetectedPrefix {
        display: humanize_prefix(&tmux_key),
        tmux_key,
    }
}

fn humanize_prefix(raw: &str) -> String {
    let mut parts = raw.split('-');
    let Some(modifier) = parts.next() else {
        return raw.to_string();
    };
    let rest = parts.collect::<Vec<_>>().join("-");
    match (modifier, rest.is_empty()) {
        ("C", false) => format!("Ctrl-{rest}"),
        ("M", false) => format!("Alt-{rest}"),
        _ => raw.to_string(),
    }
}

fn prefix_key_matches(prefix: &str, key: KeyEvent) -> bool {
    let (modifier, name) = if let Some(name) = prefix.strip_prefix("C-") {
        (Some(KeyModifiers::CONTROL), name)
    } else if let Some(name) = prefix.strip_prefix("M-") {
        (Some(KeyModifiers::ALT), name)
    } else {
        (None, prefix)
    };
    match modifier {
        Some(required) if !key.modifiers.contains(required) => return false,
        None if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            return false;
        }
        _ => {}
    }
    match name {
        "Space" => matches!(key.code, KeyCode::Char(' ') | KeyCode::Null),
        "BSpace" => key.code == KeyCode::Backspace,
        "Enter" => key.code == KeyCode::Enter,
        "Escape" => key.code == KeyCode::Esc,
        name if name.starts_with('F') => name[1..]
            .parse::<u8>()
            .is_ok_and(|number| key.code == KeyCode::F(number)),
        name => {
            let mut chars = name.chars();
            let Some(expected) = chars.next() else {
                return false;
            };
            chars.next().is_none()
                && matches!(key.code, KeyCode::Char(actual) if actual.eq_ignore_ascii_case(&expected))
        }
    }
}

pub(super) fn print_guide(language: UiLanguage) {
    let prefix = detect_tmux_prefix();
    if language == UiLanguage::Ko {
        println!("\nMuxa 온보딩에 오신 것을 환영합니다");
        println!("----------------------------------");
        println!("\n현재 prefix: {}", prefix.display);
        println!("\n먼저 Muxa의 바탕이 되는 tmux 기본 흐름부터 익힙니다.");
        println!("tmux session은 관련 terminal 화면을 하나의 작업 공간으로 유지합니다.");
        println!("연습용 session을 만들고 들어가려면 가상 shell에서 다음 명령을 입력하세요.");
        println!("  tmux new-session -s muxa-onboarding");
        println!(
            "\nsession = 계속 실행되는 terminal 작업 공간\nwindow = session 안의 독립된 화면\npane = window를 나눈 terminal 영역"
        );
        println!("\n기본 조합");
        println!("  prefix+w       session/window tree");
        println!("  prefix+c       새 window");
        println!("  prefix+% / \"  좌우 / 상하 pane 분할");
        println!("  prefix+방향키  pane 이동");
        println!("  prefix+z       pane zoom toggle");
        println!("  prefix+[       copy mode, q로 종료");
        println!("  prefix+d       client detach; session은 계속 실행");
        println!("  tmux attach -t muxa-onboarding  detach 뒤 가상 session에 재접속");
        println!("\ntmux 명령은 prefix를 누르고 뗀 뒤 명령 키를 누르는 순서로 실행합니다.");
        println!("동시에 누르는 조합이 아닙니다. 예: prefix → c, prefix → %, prefix → d");
        println!("\nsession에 들어간 뒤에는 감지된 prefix만 누르고 화면 전환을 기다리세요.");
        println!("이후 실습에서는 실제 tmux 동작을 막기 위해 명령 키만 입력합니다.");
        println!("c, %, \"와 d의 결과는 가상 shell/window/pane 화면에 계속 반영됩니다.");
    } else {
        println!("\nWelcome to the Muxa onboarding");
        println!("-------------------------------");
        println!("\nCurrent prefix: {}", prefix.display);
        println!("\nWe will begin with the tmux fundamentals that Muxa builds on.");
        println!("A tmux session keeps related terminal screens running as one workspace.");
        println!("Create and enter the practice session from the virtual shell:");
        println!("  tmux new-session -s muxa-onboarding");
        println!("\nsession = persistent terminal workspace\nwindow = independent screen\npane = split terminal region");
        println!("\nCore combinations");
        println!("  prefix+w       session/window tree");
        println!("  prefix+c       new window");
        println!("  prefix+% / \"  left-right / top-bottom pane splits");
        println!("  prefix+Arrow   move between panes");
        println!("  prefix+z       toggle pane zoom");
        println!("  prefix+[       copy mode; q exits");
        println!("  prefix+d       detach the client; sessions keep running");
        println!("  tmux attach -t muxa-onboarding  reattach after the detach drill");
        println!(
            "\nEvery tmux command starts by pressing and releasing the prefix, then its command key."
        );
        println!("It is a sequence, not a simultaneous chord: prefix → c, prefix → %, prefix → d");
        println!(
            "\nOnce inside the session, press only the detected prefix and wait for the next step."
        );
        println!("Later exercises accept command keys only, so live tmux bindings do not run.");
        println!("The simulated shell, windows, and panes preserve the effects of c, %, \" and d.");
    }
}

pub(super) fn interactive_guide(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    no_quiz: bool,
    language: UiLanguage,
) -> Result<Option<UiLanguage>> {
    let prefix = detect_tmux_prefix();
    let prefix_probe = TmuxPrefixProbe::detect();
    let prefix_capture = if prefix_probe.is_some() {
        PrefixCapture::TmuxClient
    } else {
        PrefixCapture::Direct
    };
    let mut app = TmuxApp::new(no_quiz, language, prefix, prefix_capture);

    while !app.done {
        terminal.draw(|frame| render_tour(frame, &app))?;
        if app.guided() && app.current() == Step::Prefix {
            if let Some(probe) = prefix_probe.as_ref() {
                if probe.consume_prefix_press()? {
                    app.advance();
                    continue;
                }
                if !event::poll(Duration::from_millis(35))
                    .context("polling for tmux prefix onboarding input")?
                {
                    continue;
                }
            }
        }
        if let Event::Key(key) = event::read().context("reading tmux onboarding input")? {
            if key.kind == KeyEventKind::Press && key.code == KeyCode::Esc {
                return Ok(None);
            }
            handle_key(&mut app, key);
        }
    }
    Ok(Some(app.language))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TmuxClientSnapshot {
    tty: String,
    pane: String,
    key_table: String,
}

impl TmuxClientSnapshot {
    fn parse(raw: &str) -> Option<Self> {
        let mut fields = raw.trim().split('|');
        let snapshot = Self {
            tty: fields.next()?.to_string(),
            pane: fields.next()?.to_string(),
            key_table: fields.next()?.to_string(),
        };
        if fields.next().is_some()
            || snapshot.tty.is_empty()
            || snapshot.pane.is_empty()
            || snapshot.key_table.is_empty()
        {
            return None;
        }
        Some(snapshot)
    }
}

#[derive(Debug)]
struct TmuxPrefixProbe {
    pane: String,
    client_tty: String,
}

impl TmuxPrefixProbe {
    fn detect() -> Option<Self> {
        env::var_os("TMUX").filter(|value| !value.is_empty())?;
        let pane = env::var("TMUX_PANE")
            .ok()
            .filter(|value| !value.is_empty())?;
        let snapshot = Self::snapshot(&pane)?;
        (snapshot.pane == pane).then_some(Self {
            pane,
            client_tty: snapshot.tty,
        })
    }

    fn snapshot(pane: &str) -> Option<TmuxClientSnapshot> {
        let output = Command::new("tmux")
            .args([
                "display-message",
                "-p",
                "-t",
                pane,
                "#{client_tty}|#{pane_id}|#{client_key_table}",
            ])
            .output()
            .ok()?;
        output.status.success().then_some(())?;
        TmuxClientSnapshot::parse(std::str::from_utf8(&output.stdout).ok()?)
    }

    fn consume_prefix_press(&self) -> Result<bool> {
        let Some(snapshot) = Self::snapshot(&self.pane) else {
            return Ok(false);
        };
        if snapshot.pane != self.pane
            || snapshot.tty != self.client_tty
            || snapshot.key_table != "prefix"
        {
            return Ok(false);
        }
        let output = Command::new("tmux")
            .args(["switch-client", "-t", &self.client_tty, "-T", "root"])
            .output()
            .context("returning the tmux onboarding client to the root key table")?;
        if !output.status.success() {
            anyhow::bail!(
                "could not safely release tmux prefix table: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(true)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Shell,
    Prefix,
    Model,
    Windows,
    Splits,
    Panes,
    Zoom,
    CopyMode,
    Detach,
    Reattach,
    Muxa,
}

impl Step {
    const ALL: [Self; 11] = [
        Self::Shell,
        Self::Prefix,
        Self::Model,
        Self::Windows,
        Self::Splits,
        Self::Panes,
        Self::Zoom,
        Self::CopyMode,
        Self::Detach,
        Self::Reattach,
        Self::Muxa,
    ];
}

pub(super) const STEP_COUNT: usize = Step::ALL.len();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TourMode {
    Guided,
    SkipQuiz,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SplitStage {
    LeftRight,
    TopBottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CopyStage {
    Enter,
    Exit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ZoomStage {
    In,
    Out,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MuxaStage {
    Watch,
    Peek,
    Complete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrefixCapture {
    Direct,
    TmuxClient,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientLocation {
    Shell,
    Tmux,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TreeVisibility {
    Hidden,
    Visible,
}

#[derive(Debug)]
struct TmuxApp {
    step: usize,
    mode: TourMode,
    language: UiLanguage,
    prefix: String,
    prefix_key: String,
    prefix_capture: PrefixCapture,
    split_stage: SplitStage,
    zoom_stage: ZoomStage,
    copy_stage: CopyStage,
    muxa_stage: MuxaStage,
    shell_input: String,
    client_location: ClientLocation,
    tree_visibility: TreeVisibility,
    windows: usize,
    active_window: usize,
    panes: usize,
    selected_pane: usize,
    zoomed: bool,
    blocked_hint: bool,
    done: bool,
}

impl TmuxApp {
    fn new(
        no_quiz: bool,
        language: UiLanguage,
        prefix: DetectedPrefix,
        prefix_capture: PrefixCapture,
    ) -> Self {
        Self {
            step: 0,
            mode: if no_quiz {
                TourMode::SkipQuiz
            } else {
                TourMode::Guided
            },
            language,
            prefix: prefix.display,
            prefix_key: prefix.tmux_key,
            prefix_capture,
            split_stage: SplitStage::LeftRight,
            zoom_stage: ZoomStage::In,
            copy_stage: CopyStage::Enter,
            muxa_stage: MuxaStage::Watch,
            shell_input: String::new(),
            client_location: ClientLocation::Shell,
            tree_visibility: TreeVisibility::Hidden,
            windows: 1,
            active_window: 0,
            panes: 1,
            selected_pane: 0,
            zoomed: false,
            blocked_hint: false,
            done: false,
        }
    }

    fn current(&self) -> Step {
        Step::ALL[self.step]
    }

    fn guided(&self) -> bool {
        self.mode == TourMode::Guided
    }

    fn ko(&self) -> bool {
        self.language == UiLanguage::Ko
    }

    fn attached(&self) -> bool {
        self.client_location == ClientLocation::Tmux
    }

    fn tree_open(&self) -> bool {
        self.tree_visibility == TreeVisibility::Visible
    }

    fn advance(&mut self) {
        self.blocked_hint = false;
        if self.step + 1 == Step::ALL.len() {
            self.done = true;
        } else {
            self.step += 1;
        }
    }

    fn previous(&mut self) {
        self.blocked_hint = false;
        self.step = self.step.saturating_sub(1);
        self.reset_exercises();
        self.sync_scene_to_step();
    }

    fn reset_exercises(&mut self) {
        self.split_stage = SplitStage::LeftRight;
        self.zoom_stage = ZoomStage::In;
        self.copy_stage = CopyStage::Enter;
        self.muxa_stage = MuxaStage::Watch;
    }

    fn apply_skipped_step(&mut self) {
        match self.current() {
            Step::Shell | Step::Reattach => self.client_location = ClientLocation::Tmux,
            Step::Model => self.tree_visibility = TreeVisibility::Visible,
            Step::Windows => {
                self.tree_visibility = TreeVisibility::Hidden;
                self.windows = 2;
                self.active_window = 1;
                self.panes = 1;
                self.selected_pane = 0;
            }
            Step::Splits => self.panes = 3,
            Step::Panes => self.selected_pane = 1,
            Step::Detach => self.client_location = ClientLocation::Shell,
            _ => {}
        }
    }

    fn sync_scene_to_step(&mut self) {
        self.shell_input.clear();
        self.zoomed = false;
        self.tree_visibility = TreeVisibility::Hidden;
        self.client_location = if self.current() == Step::Shell || self.current() == Step::Reattach
        {
            ClientLocation::Shell
        } else {
            ClientLocation::Tmux
        };
        if self.step < Step::Windows as usize {
            self.windows = 1;
            self.active_window = 0;
            self.panes = 1;
            self.selected_pane = 0;
        } else if self.current() == Step::Windows {
            self.windows = 1;
            self.active_window = 0;
            self.panes = 1;
            self.selected_pane = 0;
            self.tree_visibility = TreeVisibility::Visible;
        } else {
            self.windows = 2;
            self.active_window = 1;
            self.panes = if self.current() == Step::Splits { 1 } else { 3 };
            self.selected_pane = usize::from(self.step >= Step::Zoom as usize);
        }
    }

    fn toggle_language(&mut self) {
        self.language = match self.language {
            UiLanguage::En => UiLanguage::Ko,
            UiLanguage::Ko => UiLanguage::En,
        };
        self.blocked_hint = false;
    }
}

fn handle_key(app: &mut TmuxApp, key: KeyEvent) {
    if key.kind != KeyEventKind::Press {
        return;
    }
    if key.code == KeyCode::F(2) {
        app.toggle_language();
        return;
    }
    if key.code == KeyCode::Esc {
        app.done = true;
        return;
    }
    if !app.guided() {
        match key.code {
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => app.previous(),
            KeyCode::Right | KeyCode::Char('l' | ' ') | KeyCode::Enter => {
                app.apply_skipped_step();
                app.advance();
            }
            KeyCode::Home => {
                app.step = 0;
                app.sync_scene_to_step();
            }
            KeyCode::End => {
                app.step = Step::ALL.len() - 1;
                app.sync_scene_to_step();
            }
            _ => {}
        }
        return;
    }
    if handle_guided_key(app, key) {
        return;
    }
    match key.code {
        KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => app.previous(),
        _ => app.blocked_hint = true,
    }
}

fn handle_guided_key(app: &mut TmuxApp, key: KeyEvent) -> bool {
    let plain = !key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
    if matches!(app.current(), Step::Shell | Step::Reattach) {
        return handle_shell_command(app, key, plain);
    }
    match app.current() {
        Step::Prefix if prefix_key_matches(&app.prefix_key, key) => {
            app.advance();
        }
        Step::Model if plain && key.code == KeyCode::Char('w') => {
            app.tree_visibility = TreeVisibility::Visible;
            app.advance();
        }
        Step::Windows if plain && key.code == KeyCode::Char('c') => {
            app.tree_visibility = TreeVisibility::Hidden;
            app.windows = 2;
            app.active_window = 1;
            app.panes = 1;
            app.selected_pane = 0;
            app.advance();
        }
        Step::Splits
            if app.split_stage == SplitStage::LeftRight && key.code == KeyCode::Char('%') =>
        {
            app.panes = 2;
            app.split_stage = SplitStage::TopBottom;
            app.blocked_hint = false;
        }
        Step::Splits
            if app.split_stage == SplitStage::TopBottom && key.code == KeyCode::Char('"') =>
        {
            app.panes = 3;
            app.advance();
        }
        Step::Panes if key.code == KeyCode::Right => {
            app.selected_pane = 1;
            app.advance();
        }
        Step::Zoom
            if app.zoom_stage == ZoomStage::In && plain && key.code == KeyCode::Char('z') =>
        {
            app.zoomed = true;
            app.zoom_stage = ZoomStage::Out;
            app.blocked_hint = false;
        }
        Step::Zoom
            if app.zoom_stage == ZoomStage::Out && plain && key.code == KeyCode::Char('z') =>
        {
            app.zoomed = false;
            app.advance();
        }
        Step::CopyMode if app.copy_stage == CopyStage::Enter && key.code == KeyCode::Char('[') => {
            app.copy_stage = CopyStage::Exit;
            app.blocked_hint = false;
        }
        Step::CopyMode
            if app.copy_stage == CopyStage::Exit && plain && key.code == KeyCode::Char('q') =>
        {
            app.advance();
        }
        Step::Detach if plain && key.code == KeyCode::Char('d') => {
            app.client_location = ClientLocation::Shell;
            app.advance();
        }
        Step::Muxa
            if app.muxa_stage == MuxaStage::Watch && plain && key.code == KeyCode::Char('s') =>
        {
            app.muxa_stage = MuxaStage::Peek;
            app.blocked_hint = false;
        }
        Step::Muxa
            if app.muxa_stage == MuxaStage::Peek && plain && key.code == KeyCode::Char('q') =>
        {
            app.muxa_stage = MuxaStage::Complete;
            app.blocked_hint = false;
        }
        Step::Muxa
            if app.muxa_stage == MuxaStage::Complete && plain && key.code == KeyCode::Char('s') =>
        {
            app.advance();
        }
        _ => return false,
    }
    true
}

fn handle_shell_command(app: &mut TmuxApp, key: KeyEvent, plain: bool) -> bool {
    match key.code {
        KeyCode::Char(ch) if plain && !ch.is_control() && app.shell_input.chars().count() < 80 => {
            app.shell_input.push(ch);
            app.blocked_hint = false;
        }
        KeyCode::Backspace => {
            app.shell_input.pop();
            app.blocked_hint = false;
        }
        KeyCode::Enter => {
            if !shell_command_matches(app.current(), &app.shell_input) {
                app.blocked_hint = true;
                return true;
            }
            app.shell_input.clear();
            app.client_location = ClientLocation::Tmux;
            app.advance();
        }
        _ => return false,
    }
    true
}

fn shell_command_matches(step: Step, input: &str) -> bool {
    let normalized = input.split_whitespace().collect::<Vec<_>>();
    match step {
        Step::Shell => matches!(
            normalized.as_slice(),
            ["tmux", "new-session" | "new", "-s", "muxa-onboarding"]
        ),
        Step::Reattach => matches!(
            normalized.as_slice(),
            ["tmux", "attach" | "attach-session", "-t", "muxa-onboarding"]
        ),
        _ => false,
    }
}

fn render_tour(frame: &mut Frame<'_>, app: &TmuxApp) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(Color::Rgb(7, 11, 18))),
        area,
    );
    if area.width < 68 || area.height < 20 {
        render_small_terminal(frame, area, app.language);
        return;
    }
    if !app.attached() {
        render_shell_terminal(frame, area, app);
        render_callout(frame, area, app);
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(4), Constraint::Length(1)])
        .split(area);
    render_mock_terminal(frame, rows[0], app);
    render_status_line(frame, rows[1], app);
    if app.tree_open() {
        render_window_tree(frame, rows[0], app);
    }
    if app.current() == Step::CopyMode && app.copy_stage == CopyStage::Exit {
        render_copy_mode(frame, rows[0], app);
    }
    if app.current() == Step::Muxa {
        match app.muxa_stage {
            MuxaStage::Watch => {}
            MuxaStage::Peek => render_muxa_watch(frame, area, app),
            MuxaStage::Complete => {
                frame.render_widget(Clear, area);
                render_mock_terminal(frame, area, app);
                render_muxa_peek(frame, area, app);
            }
        }
    }
    render_callout(frame, area, app);
}

fn render_shell_terminal(frame: &mut Frame<'_>, area: Rect, app: &TmuxApp) {
    let prompt = Line::from(vec![
        Span::styled(
            "june@devbox:~/personal/muxa$ ",
            Style::default().fg(Color::Green),
        ),
        Span::styled(app.shell_input.clone(), action_style()),
        Span::styled("█", Style::default().fg(Color::Cyan)),
    ]);
    let lines = if app.current() == Step::Reattach {
        vec![
            Line::from("june@devbox:~/personal/muxa$"),
            Line::from("[detached (from session muxa-onboarding)]"),
            Line::from(""),
            prompt,
        ]
    } else {
        vec![
            Line::from(tr(
                app.language,
                "Muxa onboarding · safe shell simulation",
                "Muxa 온보딩 · 안전한 가상 shell",
            )),
            Line::from(""),
            prompt,
        ]
    };
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(
                Block::default()
                    .title(" shell · outside tmux ")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Plain)
                    .border_style(Style::default().fg(Color::Rgb(45, 57, 75))),
            )
            .style(Style::default().bg(Color::Rgb(7, 11, 18))),
        area,
    );
}

fn render_window_tree(frame: &mut Frame<'_>, area: Rect, app: &TmuxApp) {
    let width = area.width.saturating_sub(6).min(52);
    let height = 10.min(area.height.saturating_sub(2));
    let popup = Rect::new(area.x + 3, area.y + 2, width, height);
    frame.render_widget(Clear, popup);
    let body = if app.ko() {
        vec![
            Line::from("muxa-onboarding: 1 windows (created Tue Aug 11)"),
            Line::from("└─ 0: shell* (1 panes) [132x43]"),
            Line::from("   └─ 0: zsh  june@devbox:~/personal/muxa"),
            Line::from(""),
            Line::from("w로 연 session/window tree · c로 새 window 생성"),
        ]
    } else {
        vec![
            Line::from("muxa-onboarding: 1 windows (created Tue Aug 11)"),
            Line::from("└─ 0: shell* (1 panes) [132x43]"),
            Line::from("   └─ 0: zsh  june@devbox:~/personal/muxa"),
            Line::from(""),
            Line::from("session/window tree opened by w · c creates a window"),
        ]
    };
    frame.render_widget(
        Paragraph::new(Text::from(body)).block(
            Block::default()
                .title(" choose-tree -Zw ")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::Yellow)),
        ),
        popup,
    );
}

fn render_mock_terminal(frame: &mut Frame<'_>, area: Rect, app: &TmuxApp) {
    if app.zoomed {
        render_pane(frame, area, app, app.selected_pane, true);
        return;
    }
    for (index, pane_area) in mock_pane_areas(area, app.panes).into_iter().enumerate() {
        render_pane(frame, pane_area, app, index, app.selected_pane == index);
    }
}

fn mock_pane_areas(area: Rect, panes: usize) -> Vec<Rect> {
    if panes <= 1 {
        return vec![area];
    }
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(area);
    if panes == 2 {
        return vec![columns[0], columns[1]];
    }
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(columns[0]);
    vec![left[0], columns[1], left[1]]
}

fn render_pane(frame: &mut Frame<'_>, area: Rect, app: &TmuxApp, index: usize, selected: bool) {
    let (title, lines_en, lines_ko) = match (app.active_window, index) {
        (0, _) => (
            " shell ",
            [
                "june@devbox ~/personal/muxa",
                "",
                "$ tmux display-message -p '#S:#I.#P'",
                "muxa-onboarding:0.0",
            ],
            [
                "june@devbox ~/personal/muxa",
                "",
                "$ tmux display-message -p '#S:#I.#P'",
                "muxa-onboarding:0.0",
            ],
        ),
        (1, 0) => (
            " review · shell ",
            [
                "june@devbox ~/personal/muxa",
                "",
                "$",
                "new window ready · split this layout next",
            ],
            [
                "june@devbox ~/personal/muxa",
                "",
                "$",
                "새 window 준비 완료 · 이제 이 layout을 분할합니다",
            ],
        ),
        (1, 1) => (
            " codex · agent ",
            [
                "› implement muxa-onboarding",
                "",
                "  ● working",
                "  editing tmux onboarding",
            ],
            [
                "› muxa-onboarding 구현",
                "",
                "  ● 작업 중",
                "  tmux onboarding 편집 중",
            ],
        ),
        _ => (
            " reviewer · agent ",
            [
                "› review the current changes",
                "",
                "  ▶ waiting for input",
                "  findings: 0",
            ],
            [
                "› 현재 변경사항 검토",
                "",
                "  ▶ 입력 대기",
                "  발견 사항: 0",
            ],
        ),
    };
    let lines = if app.ko() { lines_ko } else { lines_en };
    let border = if selected {
        Color::Cyan
    } else {
        Color::Rgb(45, 57, 75)
    };
    let title = if selected {
        format!("{title}* ")
    } else {
        title.to_string()
    };
    frame.render_widget(
        Paragraph::new(Text::from(
            lines.into_iter().map(Line::from).collect::<Vec<_>>(),
        ))
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_type(BorderType::Plain)
                .border_style(Style::default().fg(border)),
        ),
        area,
    );
}

fn render_status_line(frame: &mut Frame<'_>, area: Rect, app: &TmuxApp) {
    let windows = if app.windows == 1 {
        vec!["0:shell*"]
    } else {
        vec!["0:shell", "1:review*"]
    };
    let left = format!(" [muxa-onboarding] {}", windows.join("  "));
    let right = format!(" prefix {} · {} panes ", app.prefix, app.panes);
    let padding =
        usize::from(area.width).saturating_sub(left.chars().count() + right.chars().count());
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                left,
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " ".repeat(padding),
                Style::default().bg(Color::Rgb(35, 43, 56)),
            ),
            Span::styled(
                right,
                Style::default().fg(Color::White).bg(Color::Rgb(35, 43, 56)),
            ),
        ])),
        area,
    );
}

fn render_copy_mode(frame: &mut Frame<'_>, area: Rect, app: &TmuxApp) {
    let popup = centered_rect(area, area.width.saturating_mul(72) / 100, 10);
    frame.render_widget(Clear, popup);
    let lines = if app.ko() {
        vec![
            Line::from("$ cargo test -p muxa-cli onboarding"),
            Line::from("running 11 tests"),
            Line::from("test result: ok. 11 passed"),
            Line::from(""),
            highlighted_actions("↑/↓ 스크롤 · / 검색 · q 종료", &["↑/↓", "/", "q"]),
        ]
    } else {
        vec![
            Line::from("$ cargo test -p muxa-cli onboarding"),
            Line::from("running 11 tests"),
            Line::from("test result: ok. 11 passed"),
            Line::from(""),
            highlighted_actions("↑/↓ scroll · / search · q exit", &["↑/↓", "/", "q"]),
        ]
    };
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(dialog_block(
            tr(
                app.language,
                " copy mode · [0/120] ",
                " copy mode · [0/120] ",
            ),
            Color::Yellow,
        )),
        popup,
    );
}

fn render_muxa_watch(frame: &mut Frame<'_>, area: Rect, app: &TmuxApp) {
    frame.render_widget(Clear, area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);
    render_muxa_watch_header(frame, rows[0]);
    if rows[1].width >= 120 {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[1]);
        render_muxa_watch_sessions(frame, columns[0], app);
        render_muxa_watch_inspector(frame, columns[1], app);
    } else {
        render_muxa_watch_sessions(frame, rows[1], app);
    }
    render_muxa_watch_footer(frame, rows[2]);
}

fn render_muxa_watch_header(frame: &mut Frame<'_>, area: Rect) {
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
            watch_state_span(AgentState::WaitingInput),
            Span::raw("1  "),
            watch_state_span(AgentState::Working),
            Span::raw("1  "),
            watch_state_span(AgentState::Idle),
            Span::styled("1", Style::default().fg(Color::Green)),
            Span::styled("  mail 0/1", Style::default().fg(Color::Yellow)),
            Span::styled(
                "   sort LATEST   10:37:32 UTC",
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

fn render_muxa_watch_sessions(frame: &mut Frame<'_>, area: Rect, app: &TmuxApp) {
    let selected = Style::default()
        .fg(Color::White)
        .bg(Color::Rgb(18, 83, 108));
    let summary = tr(
        app.language,
        "implement and test tmux onboarding",
        "tmux onboarding 구현 및 테스트",
    );
    let done = tr(app.language, "release checks complete", "release 점검 완료");
    let lines = vec![
        Line::from(Span::styled(
            "  WORKSPACE › WORK         DUR    ACT    SUMMARY",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(vec![
            Span::styled("> ", selected),
            watch_state_span_with_bg(AgentState::WaitingInput, selected),
            Span::styled(" ", selected),
            watch_state_span_with_bg(AgentState::Working, selected),
            Span::styled(
                format!("   muxa › onboarding  18m    14m    {summary}"),
                selected,
            ),
        ]),
        Line::from("      └─ muxa-onboarding:1.0   -      9m     codex · editing onboarding"),
        Line::from(Span::styled(
            "      └─ muxa-onboarding:1.1   -      4m     reviewer · waiting for input",
            Style::default().fg(Color::Gray),
        )),
        Line::from(vec![
            Span::raw("  "),
            watch_state_span(AgentState::Idle),
            Span::raw(format!("     muxa › sandbox     7m     2m     {done}")),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(
                Block::default()
                    .title(" Workspace › work › agent ")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::Rgb(43, 57, 78))),
            )
            .style(Style::default().bg(Color::Rgb(7, 11, 18))),
        area,
    );
}

fn render_muxa_watch_inspector(frame: &mut Frame<'_>, area: Rect, app: &TmuxApp) {
    let latest = tr(
        app.language,
        "implement and test tmux onboarding",
        "tmux onboarding 구현 및 테스트",
    );
    let activity = tr(
        app.language,
        "editing crates/muxa-cli/src/onboarding/tmux.rs",
        "crates/muxa-cli/src/onboarding/tmux.rs 편집 중",
    );
    let lines = vec![
        Line::from(vec![
            Span::styled("kind ", Style::default().fg(Color::DarkGray)),
            Span::raw("codex"),
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
        Line::from("› improve the tmux onboarding simulation"),
        Line::from(""),
        Line::from(Span::styled(
            format!("  ⚙ {activity}"),
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  ● working…",
            Style::default().fg(Color::Yellow),
        )),
    ];
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .title(" Inspector · muxa-onboarding:1.0 · WORK 8s ")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::Rgb(43, 57, 78))),
            ),
        area,
    );
}

fn render_muxa_watch_footer(frame: &mut Frame<'_>, area: Rect) {
    let key = Style::default()
        .fg(Color::Cyan)
        .bg(Color::Rgb(19, 61, 80))
        .add_modifier(Modifier::BOLD);
    let text = Style::default().fg(Color::DarkGray);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" j/k ", key),
            Span::styled("move  ", text),
            Span::styled(" h/l ", key),
            Span::styled("tree  ", text),
            Span::styled(" / ", key),
            Span::styled("filter  ", text),
            Span::styled(" ⏎ ", key),
            Span::styled("prompt  ", text),
            Span::styled(" o ", key),
            Span::styled("preview  ", text),
            Span::styled(" m ", key),
            Span::styled("message  ", text),
            Span::styled(" M ", key),
            Span::styled("mailbox  ", text),
            Span::styled(" ? ", key),
            Span::styled("help", text),
        ])),
        area,
    );
}

fn watch_state_span(state: AgentState) -> Span<'static> {
    watch_state_span_with_bg(state, Style::default())
}

fn watch_state_span_with_bg(state: AgentState, background: Style) -> Span<'static> {
    let color = match state {
        AgentState::Idle => Color::Green,
        AgentState::Working | AgentState::WaitingInput => Color::Yellow,
        AgentState::WaitingChoice => Color::LightYellow,
        AgentState::Error => Color::Red,
        AgentState::Starting => Color::Cyan,
        AgentState::Stopped => Color::DarkGray,
    };
    Span::styled(
        crate::state_icon(state),
        background.fg(color).add_modifier(Modifier::BOLD),
    )
}

fn render_muxa_peek(frame: &mut Frame<'_>, area: Rect, app: &TmuxApp) {
    let states = if app.ko() {
        [
            (
                " 1 · editor · ○ IDLE ",
                vec![
                    Line::from("최근 prompt 없음"),
                    highlighted_actions("Enter/숫자: pane 이동", &["Enter/숫자"]),
                ],
            ),
            (
                " 2 · codex · ● WORKING ",
                vec![
                    Line::from("tmux onboarding 편집 중"),
                    Line::from("마지막 prompt: 방금 전"),
                ],
            ),
            (
                " 3 · reviewer · ▶ INPUT ",
                vec![
                    Line::from("변경사항 검토 대기"),
                    Line::from("마지막 prompt: 4분 전"),
                ],
            ),
        ]
    } else {
        [
            (
                " 1 · editor · ○ IDLE ",
                vec![
                    Line::from("no recent prompt"),
                    highlighted_actions("Enter/digit: jump pane", &["Enter/digit"]),
                ],
            ),
            (
                " 2 · codex · ● WORKING ",
                vec![
                    Line::from("editing tmux onboarding"),
                    Line::from("last prompted: just now"),
                ],
            ),
            (
                " 3 · reviewer · ▶ INPUT ",
                vec![
                    Line::from("waiting to review changes"),
                    Line::from("last prompted: 4m ago"),
                ],
            ),
        ]
    };
    for (pane_area, (title, body)) in mock_pane_areas(area, app.panes).into_iter().zip(states) {
        let popup = centered_rect(
            pane_area,
            pane_area.width.saturating_sub(4).min(38),
            8.min(pane_area.height.saturating_sub(2)),
        );
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(Text::from(body))
                .wrap(Wrap { trim: false })
                .block(
                    Block::default()
                        .title(title)
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(Color::Yellow)),
                ),
            popup,
        );
    }
}

fn render_callout(frame: &mut Frame<'_>, area: Rect, app: &TmuxApp) {
    let popup = callout_rect(area, app);
    frame.render_widget(Clear, popup);
    let title = format!(
        " {}/{} · {} ",
        app.step + 1,
        super::UNIFIED_STEP_COUNT,
        step_title(app.current(), app.language)
    );
    let mut lines = step_lines(app);
    if app.blocked_hint {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(
                "{}: {}",
                tr(app.language, "Expected", "필요한 입력"),
                expected_key(app)
            ),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .block(
                dialog_block(&title, Color::Cyan)
                    .title_bottom(callout_footer_line(app).alignment(Alignment::Center)),
            ),
        popup,
    );
}

fn callout_rect(area: Rect, app: &TmuxApp) -> Rect {
    let step = app.current();
    if step == Step::Muxa && app.muxa_stage == MuxaStage::Complete {
        let width = area.width.saturating_mul(45) / 100;
        let height = 13.min(area.height.saturating_sub(2));
        return Rect::new(
            area.x + area.width.saturating_sub(width),
            area.y + area.height.saturating_sub(height + 2),
            width,
            height,
        );
    }
    let width = area.width.saturating_sub(6).min(82);
    let height = if step == Step::Shell { 16 } else { 13 }.min(area.height.saturating_sub(2));
    match step {
        Step::Shell | Step::Prefix | Step::Zoom | Step::Detach | Step::Reattach => {
            centered_rect(area, width, height)
        }
        Step::Model | Step::Windows | Step::Splits | Step::Panes | Step::CopyMode | Step::Muxa => {
            Rect::new(
                area.x + area.width.saturating_sub(width) / 2,
                area.y + area.height.saturating_sub(height + 2),
                width,
                height,
            )
        }
    }
}

fn step_title(step: Step, language: UiLanguage) -> &'static str {
    let en = match step {
        Step::Shell => "welcome — begin with a tmux session",
        Step::Prefix => "every tmux command begins with the prefix",
        Step::Model => "how session, window, and pane fit together",
        Step::Windows => "create another window",
        Step::Splits => "split a window into panes",
        Step::Panes => "move between panes",
        Step::Zoom => "zoom a pane temporarily",
        Step::CopyMode => "review terminal history with copy mode",
        Step::Detach => "detach without stopping the session",
        Step::Reattach => "reattach to the running session",
        Step::Muxa => "add Muxa to the tmux workflow",
    };
    let ko = match step {
        Step::Shell => "환영합니다 — tmux session부터 시작합니다",
        Step::Prefix => "모든 tmux 명령은 prefix로 시작합니다",
        Step::Model => "session, window, pane의 관계",
        Step::Windows => "새 window 만들기",
        Step::Splits => "window를 pane으로 나누기",
        Step::Panes => "pane 사이 이동하기",
        Step::Zoom => "pane을 잠시 확대하기",
        Step::CopyMode => "copy mode로 이전 화면 살펴보기",
        Step::Detach => "session은 유지한 채 detach하기",
        Step::Reattach => "실행 중인 session에 다시 attach하기",
        Step::Muxa => "tmux workflow에 Muxa 더하기",
    };
    tr(language, en, ko)
}

fn step_lines(app: &TmuxApp) -> Vec<Line<'static>> {
    if app.ko() {
        return step_lines_ko(app);
    }
    match app.current() {
        Step::Shell => shell_step_lines(app.language),
        Step::Prefix => prefix_lines(app),
        Step::Model => vec![
            label("↓ SEE HOW THE THREE LEVELS FIT TOGETHER"),
            Line::from(""),
            mapping_line("SESSION", "persistent terminal workspace"),
            mapping_line("WINDOW", "independent screen in a session"),
            mapping_line("PANE", "split terminal region in a window"),
            Line::from(""),
            highlighted_actions("Press w to open tmux’s session and window tree.", &["w"]),
        ],
        Step::Windows => vec![
            label("↓ CREATE AN INDEPENDENT SCREEN IN THIS SESSION"),
            Line::from(""),
            Line::from("A window is an independent work screen inside one session."),
            highlighted_actions(
                "Press c to create a new window and move to that screen.",
                &["c"],
            ),
            Line::from("The simulated client will move to the new 1:review screen."),
        ],
        Step::Splits if app.split_stage == SplitStage::LeftRight => vec![
            label("SPLIT THE CURRENT SCREEN LEFT AND RIGHT"),
            Line::from(""),
            Line::from("A pane lets you view another terminal without leaving this window."),
            highlighted_actions(
                "Press % (usually Shift-5) to split the selected pane.",
                &["%", "Shift-5"],
            ),
        ],
        Step::Splits => vec![
            label("SPLIT THE SELECTED PANE TOP AND BOTTOM"),
            Line::from(""),
            Line::from("You now have two panes in the same window."),
            highlighted_actions(
                "Press \" (usually Shift-apostrophe) to add a third pane.",
                &["\"", "Shift-apostrophe"],
            ),
        ],
        Step::Panes => vec![
            label("MOVE FOCUS TO THE PANE YOU WANT TO USE"),
            Line::from(""),
            Line::from("Focus determines which pane receives your keyboard input."),
            highlighted_actions(
                "Use Arrow keys by direction, or o to cycle through panes.",
                &["Arrow", "o"],
            ),
            highlighted_actions("Press → to select the pane on the right.", &["→"]),
        ],
        Step::Zoom if app.zoom_stage == ZoomStage::In => vec![
            label("ZOOM ONE PANE WITHOUT LOSING THE LAYOUT"),
            Line::from(""),
            Line::from("prefix+z toggles the selected pane fullscreen."),
            Line::from("The underlying split layout remains intact."),
            highlighted_actions("Press z to enlarge the selected pane.", &["z"]),
        ],
        Step::Zoom => vec![
            label("RESTORE THE SPLIT LAYOUT"),
            Line::from(""),
            Line::from("The pane is fullscreen but the split layout still exists."),
            highlighted_actions("Press z again to restore every pane.", &["z"]),
        ],
        Step::CopyMode if app.copy_stage == CopyStage::Enter => vec![
            label("OPEN COPY MODE TO REVIEW EARLIER OUTPUT"),
            Line::from(""),
            Line::from("Copy mode lets you scroll and search a pane’s terminal history."),
            highlighted_actions("Press [ to open the simulated history view.", &["["]),
        ],
        Step::CopyMode => vec![
            label("LEAVE COPY MODE WHEN YOU ARE DONE"),
            Line::from(""),
            highlighted_actions(
                "Use arrows/PageUp/PageDown to scroll and / to search.",
                &["arrows", "PageUp", "PageDown", "/"],
            ),
            highlighted_actions("Press q to leave copy mode and return to the pane.", &["q"]),
        ],
        Step::Detach => vec![
            label("DETACH LEAVES THE SESSION RUNNING"),
            Line::from(""),
            Line::from("prefix+d detaches this client from the tmux server."),
            Line::from("The session and the programs in its panes keep running."),
            highlighted_actions("Press d to return to the simulated shell.", &["d"]),
        ],
        Step::Reattach => vec![
            label("THE SESSION IS STILL AVAILABLE FROM THE SHELL"),
            Line::from(""),
            Line::from("The [detached] notice means only this client left tmux."),
            Line::from("The muxa-onboarding session and its panes are still running."),
            Line::from("Return to that session with:"),
            action_line(ATTACH_COMMAND),
            highlighted_actions("Press Enter to reattach and continue.", &["Enter"]),
        ],
        Step::Muxa => muxa_lines(app),
    }
}

fn step_lines_ko(app: &TmuxApp) -> Vec<Line<'static>> {
    match app.current() {
        Step::Shell => shell_step_lines(app.language),
        Step::Prefix => prefix_lines(app),
        Step::Model => vec![
            label("↓ 세 단계가 어떻게 연결되는지 살펴봅니다"),
            Line::from(""),
            mapping_line("SESSION", "계속 실행되는 terminal 작업 공간"),
            mapping_line("WINDOW", "session 안의 독립된 작업 화면"),
            mapping_line("PANE", "window를 나눈 terminal 영역"),
            Line::from(""),
            highlighted_actions("w를 눌러 tmux의 session/window tree를 여세요.", &["w"]),
        ],
        Step::Windows => vec![
            label("↓ SESSION 안에 독립된 작업 화면을 만듭니다"),
            Line::from(""),
            Line::from("window는 한 session 안의 독립된 작업 화면입니다."),
            highlighted_actions(
                "c를 눌러 새 window를 만들고 그 화면으로 이동하세요.",
                &["c"],
            ),
            Line::from("가상 client가 새 1:review 화면으로 이동합니다."),
        ],
        Step::Splits if app.split_stage == SplitStage::LeftRight => vec![
            label("현재 화면을 좌우로 나눕니다"),
            Line::from(""),
            Line::from("pane을 나누면 이 window를 벗어나지 않고 다른 terminal을 볼 수 있습니다."),
            highlighted_actions(
                "%를 눌러(보통 Shift-5) 선택한 pane을 나누세요.",
                &["%", "Shift-5"],
            ),
        ],
        Step::Splits => vec![
            label("선택한 PANE을 상하로 나눕니다"),
            Line::from(""),
            Line::from("이제 같은 window 안에 pane이 두 개 있습니다."),
            highlighted_actions(
                "\"를 눌러(보통 Shift-apostrophe) 세 번째 pane을 만드세요.",
                &["\"", "Shift-apostrophe"],
            ),
        ],
        Step::Panes => vec![
            label("사용할 PANE으로 FOCUS를 옮깁니다"),
            Line::from(""),
            Line::from("focus가 있는 pane이 keyboard 입력을 받습니다."),
            highlighted_actions(
                "방향키로 이동하거나 o로 pane을 순서대로 전환할 수 있습니다.",
                &["방향키", "o"],
            ),
            highlighted_actions("→를 눌러 오른쪽 pane을 선택하세요.", &["→"]),
        ],
        Step::Zoom if app.zoom_stage == ZoomStage::In => vec![
            label("LAYOUT은 유지한 채 PANE 하나를 확대합니다"),
            Line::from(""),
            Line::from("prefix+z는 선택 pane의 fullscreen을 toggle합니다."),
            Line::from("기존 split layout은 그대로 유지됩니다."),
            highlighted_actions("z를 눌러 선택한 pane을 확대하세요.", &["z"]),
        ],
        Step::Zoom => vec![
            label("분할된 LAYOUT으로 돌아갑니다"),
            Line::from(""),
            Line::from("pane은 fullscreen이지만 기존 split layout은 남아 있습니다."),
            highlighted_actions("z를 다시 눌러 모든 pane을 복원하세요.", &["z"]),
        ],
        Step::CopyMode if app.copy_stage == CopyStage::Enter => vec![
            label("COPY MODE에서 이전 출력을 살펴봅니다"),
            Line::from(""),
            Line::from("copy mode에서는 pane의 terminal 기록을 scroll하고 검색할 수 있습니다."),
            highlighted_actions("[를 눌러 가상의 기록 화면을 여세요.", &["["]),
        ],
        Step::CopyMode => vec![
            label("확인을 마치면 COPY MODE를 닫습니다"),
            Line::from(""),
            highlighted_actions(
                "방향키/PageUp/PageDown으로 scroll하고 /로 검색합니다.",
                &["방향키", "PageUp", "PageDown", "/"],
            ),
            highlighted_actions("q를 눌러 copy mode를 닫고 pane으로 돌아가세요.", &["q"]),
        ],
        Step::Detach => vec![
            label("DETACH해도 SESSION은 계속 실행됩니다"),
            Line::from(""),
            Line::from("prefix+d는 이 client만 tmux server에서 분리합니다."),
            Line::from("session과 pane 안의 program은 계속 실행됩니다."),
            highlighted_actions("d를 눌러 가상 shell로 돌아가세요.", &["d"]),
        ],
        Step::Reattach => vec![
            label("SHELL에서도 실행 중인 SESSION에 다시 들어갈 수 있습니다"),
            Line::from(""),
            Line::from("[detached] 표시는 이 client만 tmux에서 나왔다는 뜻입니다."),
            Line::from("muxa-onboarding session과 그 안의 pane은 계속 실행 중입니다."),
            Line::from("다시 들어가려면 다음 명령을 입력하세요:"),
            action_line(ATTACH_COMMAND),
            highlighted_actions(
                "Enter를 눌러 attach한 뒤 다음 단계로 이동하세요.",
                &["Enter"],
            ),
        ],
        Step::Muxa => muxa_lines(app),
    }
}

fn shell_step_lines(language: UiLanguage) -> Vec<Line<'static>> {
    if language == UiLanguage::Ko {
        vec![
            label("MUXA 온보딩에 오신 것을 환영합니다"),
            Line::from(""),
            Line::from("먼저 Muxa의 바탕이 되는 tmux 기본 흐름부터 익힙니다."),
            Line::from("tmux session은 관련 terminal을 하나의 작업 공간으로 유지합니다."),
            Line::from("muxa-onboarding이라는 연습용 session을 만들고 들어갑니다:"),
            action_line(NEW_SESSION_COMMAND),
            Line::from("안전한 가상 화면이므로 실제 tmux 설정은 바뀌지 않습니다."),
            highlighted_actions("F2를 누르면 English로 전환됩니다.", &["F2"]),
        ]
    } else {
        vec![
            label("WELCOME TO THE MUXA ONBOARDING"),
            Line::from(""),
            Line::from("We’ll begin with the tmux foundation that Muxa builds on."),
            Line::from("A tmux session keeps related terminals running as one workspace."),
            Line::from("Create and enter the practice session named muxa-onboarding:"),
            action_line(NEW_SESSION_COMMAND),
            Line::from("This safe simulation will not change your real tmux setup."),
            highlighted_actions("Press F2 for 한국어.", &["F2"]),
        ]
    }
}

fn prefix_lines(app: &TmuxApp) -> Vec<Line<'static>> {
    let mut lines = vec![
        label(tr(
            app.language,
            "BEGIN WITH THE DETECTED PREFIX",
            "감지한 PREFIX부터 익혀봅니다",
        )),
        Line::from(""),
        highlighted_actions(
            format!(
                "{}: {}",
                tr(app.language, "Detected prefix", "감지한 prefix"),
                app.prefix
            ),
            &[app.prefix.as_str()],
        ),
        Line::from(tr(
            app.language,
            "Every tmux shortcut begins with this prefix.",
            "모든 tmux 단축키는 이 prefix로 시작합니다.",
        )),
        Line::from(tr(
            app.language,
            "Press and release it first, then press a command key; the keys are sequential.",
            "prefix를 먼저 누르고 뗀 뒤 명령 키를 누릅니다. 두 키는 순서대로 입력합니다.",
        )),
        highlighted_actions(
            tr(
                app.language,
                "Examples: prefix → c new window · prefix → % split pane · prefix → d detach",
                "예: prefix → c 새 window · prefix → % pane 분할 · prefix → d detach",
            ),
            &["prefix → c", "prefix → %", "prefix → d"],
        ),
        Line::from(""),
    ];
    if app.prefix_capture == PrefixCapture::TmuxClient {
        lines.extend([
            Line::from(tr(
                app.language,
                "Press and release it once. This walkthrough will recognize it and continue.",
                "한 번 누르고 손을 떼세요. 온보딩이 입력을 감지해 다음 단계로 이동합니다.",
            )),
            highlighted_actions(
                tr(
                    app.language,
                    "Wait before pressing w; the live tmux key table is cancelled safely.",
                    "아직 w는 누르지 마세요. 실제 tmux key table은 안전하게 해제됩니다.",
                ),
                &["w"],
            ),
        ]);
    } else {
        lines.push(Line::from(tr(
            app.language,
            "Press it once. The simulation reads the prefix directly in this terminal.",
            "한 번 누르세요. 이 terminal에서는 가상 화면이 prefix를 직접 인식합니다.",
        )));
    }
    lines
}

fn muxa_lines(app: &TmuxApp) -> Vec<Line<'static>> {
    let (done, key, description_en, description_ko) = match app.muxa_stage {
        MuxaStage::Watch => (
            "",
            "s",
            "prefix+s opens muxa watch to follow work and collaborate with agents.",
            "prefix+s는 작업을 살피고 agent와 협업하는 muxa watch를 엽니다.",
        ),
        MuxaStage::Peek => (
            "✓ prefix+s  ",
            "q",
            "prefix+q shows each agent’s state and jump number directly on its pane.",
            "prefix+q는 각 pane에 agent 상태와 이동 번호를 바로 표시합니다.",
        ),
        MuxaStage::Complete => (
            "✓ prefix+s  ✓ prefix+q",
            "s",
            "The state overlay now follows the pane layout you created with % and \".",
            "상태 overlay가 앞에서 %와 \"로 만든 pane layout을 그대로 따릅니다.",
        ),
    };
    let mut lines = vec![
        label(tr(
            app.language,
            "CONNECT THE TMUX MODEL TO MUXA",
            "TMUX의 구조를 MUXA 작업 흐름으로 연결합니다",
        )),
        Line::from(""),
    ];
    if app.muxa_stage == MuxaStage::Watch {
        lines.extend([
            mapping_line("SESSION", "workspace / project"),
            mapping_line("WINDOW", "work / ticket"),
            mapping_line("PANE", "agent"),
            Line::from(""),
        ]);
    }
    let binding = format!("prefix+{key}");
    let description_actions = if app.muxa_stage == MuxaStage::Complete {
        vec!["%", "\""]
    } else {
        vec![binding.as_str()]
    };
    lines.extend([
        Line::from(Span::styled(
            done,
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )),
        highlighted_actions(
            tr(app.language, description_en, description_ko),
            &description_actions,
        ),
        if app.muxa_stage == MuxaStage::Complete {
            highlighted_actions(
                tr(
                    app.language,
                    "Press s to continue directly into the simulated muxa watch.",
                    "s를 눌러 가상의 muxa watch 실습으로 바로 이어가세요.",
                ),
                &["s"],
            )
        } else {
            highlighted_actions(
                format!(
                    "{} {}.",
                    tr(app.language, "Press", "다음 키를 누르세요:"),
                    key
                ),
                &[key],
            )
        },
    ]);
    lines
}

fn expected_key(app: &TmuxApp) -> String {
    match app.current() {
        Step::Shell => return NEW_SESSION_COMMAND.to_string(),
        Step::Reattach => return ATTACH_COMMAND.to_string(),
        Step::Prefix => return app.prefix.clone(),
        Step::Model => "w",
        Step::Windows => "c",
        Step::Splits if app.split_stage == SplitStage::LeftRight => "%",
        Step::Splits => "\"",
        Step::Panes => "→",
        Step::Zoom => "z",
        Step::CopyMode if app.copy_stage == CopyStage::Enter => "[",
        Step::CopyMode => "q",
        Step::Detach => "d",
        Step::Muxa if app.muxa_stage == MuxaStage::Watch => "s",
        Step::Muxa if app.muxa_stage == MuxaStage::Peek => "q",
        Step::Muxa => "s",
    }
    .to_string()
}

fn callout_footer(app: &TmuxApp) -> String {
    if !app.guided() {
        return format!(
            " {} · {} ",
            tr(
                app.language,
                "← back · Enter/→ next · F2 한국어 · Esc quit",
                "← 이전 · Enter/→ 다음 · F2 English · Esc 종료"
            ),
            tr(app.language, "quiz skipped", "실습 생략")
        );
    }
    if matches!(app.current(), Step::Shell | Step::Reattach) {
        return tr(
            app.language,
            " type the command above · Enter to run · Backspace to edit · Esc to quit ",
            " 위 명령 입력 · Enter로 실행 · Backspace로 수정 · Esc로 종료 ",
        )
        .to_string();
    }
    if app.current() == Step::Muxa && app.muxa_stage == MuxaStage::Complete {
        return tr(
            app.language,
            " s open watch and continue · F2 한국어 · Esc to quit ",
            " s watch를 열고 계속 · F2 English · Esc로 종료 ",
        )
        .to_string();
    }
    if app.current() == Step::Prefix {
        return format!(
            " {} {} · {} ",
            tr(app.language, "press and release", "누른 뒤 떼기"),
            app.prefix,
            tr(
                app.language,
                "wait to continue · Esc to quit",
                "다음 단계 대기 · Esc로 종료"
            )
        );
    }
    if app.ko() {
        format!(
            " prefix {}는 가상 입력 · {} 누르기 · ← 이전 · Esc로 종료 ",
            app.prefix,
            expected_key(app)
        )
    } else {
        format!(
            " prefix {} is simulated · press {} · ← back · Esc to quit ",
            app.prefix,
            expected_key(app)
        )
    }
}

fn callout_footer_line(app: &TmuxApp) -> Line<'static> {
    let footer = callout_footer(app);
    let expected = expected_key(app);
    highlighted_actions(
        footer,
        &[
            expected.as_str(),
            app.prefix.as_str(),
            "Enter/→",
            "Backspace",
            "Enter",
            "F2",
            "Esc",
            "←",
        ],
    )
}

fn mapping_line(label_text: &'static str, value: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{label_text:<9}"),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("= "),
        Span::styled(value, Style::default().fg(Color::White)),
    ])
}

fn label(text: &'static str) -> Line<'static> {
    Line::from(Span::styled(
        text,
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))
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
                    "tmux onboarding needs a little more room",
                    "tmux 온보딩을 표시할 공간이 부족합니다",
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
                "Esc closes the tour.",
                "Esc로 온보딩을 닫습니다.",
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn rendered(app: &TmuxApp, width: u16, height: u16) -> String {
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

    fn press(app: &mut TmuxApp, code: KeyCode) {
        handle_key(app, KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn type_text(app: &mut TmuxApp, text: &str) {
        for ch in text.chars() {
            press(app, KeyCode::Char(ch));
        }
    }

    fn detected(raw: &str) -> DetectedPrefix {
        DetectedPrefix {
            tmux_key: raw.to_string(),
            display: humanize_prefix(raw),
        }
    }

    fn test_app(language: UiLanguage) -> TmuxApp {
        TmuxApp::new(false, language, detected("C-b"), PrefixCapture::Direct)
    }

    #[test]
    fn prefix_display_humanizes_tmux_notation() {
        assert_eq!(humanize_prefix("C-b"), "Ctrl-b");
        assert_eq!(humanize_prefix("M-a"), "Alt-a");
        assert_eq!(humanize_prefix("F12"), "F12");
    }

    #[test]
    fn prefix_matcher_accepts_tmux_notation_and_rejects_plain_suffixes() {
        assert!(prefix_key_matches(
            "C-b",
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)
        ));
        assert!(prefix_key_matches(
            "M-a",
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT)
        ));
        assert!(prefix_key_matches(
            "F12",
            KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE)
        ));
        assert!(!prefix_key_matches(
            "C-b",
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)
        ));
    }

    #[test]
    fn client_snapshot_parser_requires_exact_safe_fields() {
        assert_eq!(
            TmuxClientSnapshot::parse("/dev/pts/68|%903|prefix\n"),
            Some(TmuxClientSnapshot {
                tty: "/dev/pts/68".into(),
                pane: "%903".into(),
                key_table: "prefix".into(),
            })
        );
        assert_eq!(TmuxClientSnapshot::parse("missing|fields"), None);
        assert_eq!(TmuxClientSnapshot::parse("tty|%1|root|extra"), None);
    }

    #[test]
    fn guided_track_requires_prefix_checkpoint_then_real_suffix_keys() {
        let mut app = test_app(UiLanguage::En);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.current(), Step::Shell);
        assert!(app.blocked_hint);
        type_text(&mut app, NEW_SESSION_COMMAND);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.current(), Step::Prefix);

        press(&mut app, KeyCode::Enter);
        assert_eq!(app.current(), Step::Prefix);
        assert!(app.blocked_hint);

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
        );
        assert_eq!(app.current(), Step::Model);

        press(&mut app, KeyCode::Char('w'));
        assert!(app.tree_open());
        press(&mut app, KeyCode::Char('c'));
        assert_eq!(app.windows, 2);
        assert_eq!(app.active_window, 1);
        assert!(!app.tree_open());
        press(&mut app, KeyCode::Char('%'));
        assert_eq!(app.panes, 2);
        press(&mut app, KeyCode::Char('"'));
        assert_eq!(app.panes, 3);
        press(&mut app, KeyCode::Right);
        assert_eq!(app.selected_pane, 1);
        press(&mut app, KeyCode::Char('z'));
        assert!(app.zoomed);
        assert_eq!(app.current(), Step::Zoom);
        press(&mut app, KeyCode::Char('z'));
        assert!(!app.zoomed);
        press(&mut app, KeyCode::Char('['));
        assert_eq!(app.copy_stage, CopyStage::Exit);
        press(&mut app, KeyCode::Char('q'));
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(app.current(), Step::Reattach);
        assert!(!app.attached());
        type_text(&mut app, ATTACH_COMMAND);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.current(), Step::Muxa);
        assert!(app.attached());
        press(&mut app, KeyCode::Char('s'));
        press(&mut app, KeyCode::Char('q'));
        assert_eq!(app.current(), Step::Muxa);
        assert_eq!(app.muxa_stage, MuxaStage::Complete);
        press(&mut app, KeyCode::Char('s'));
        assert!(app.done);
    }

    #[test]
    fn localized_welcome_explains_why_the_practice_session_is_created() {
        let app = TmuxApp::new(
            false,
            UiLanguage::Ko,
            detected("C-a"),
            PrefixCapture::TmuxClient,
        );
        let screen = rendered(&app, 130, 32).replace(' ', "");
        assert!(screen.contains("MUXA온보딩에오신것을환영합니다"));
        assert!(screen.contains("tmuxsession은관련terminal을하나의작업공간으로유지합니다"));
        assert!(screen.contains("연습용session을만들고들어갑니다"));
        assert!(screen.contains("실제tmux설정은바뀌지않습니다"));

        let english = rendered(&test_app(UiLanguage::En), 130, 32).replace(' ', "");
        assert!(english.contains("WELCOMETOTHEMUXAONBOARDING"));
        assert!(english.contains("Atmuxsessionkeepsrelatedterminalsrunningasoneworkspace"));
        assert!(english.contains("Createandenterthepracticesession"));
        assert!(english.contains("willnotchangeyourrealtmuxsetup"));

        let mut model = app;
        model.step = 1;
        model.client_location = ClientLocation::Tmux;
        let screen = rendered(&model, 130, 32).replace(' ', "");
        assert!(screen.contains("모든tmux단축키는이prefix로시작합니다"));
        assert!(screen.contains("두키는순서대로입력합니다"));
        assert!(screen.contains("prefix→c새window"));

        model.step = 2;
        let screen = rendered(&model, 130, 32).replace(' ', "");
        assert!(screen.contains("SESSION=계속실행되는terminal작업공간"));
        assert!(screen.contains("WINDOW=session안의독립된작업화면"));
        assert!(screen.contains("PANE=window를나눈terminal영역"));
    }

    #[test]
    fn f2_switches_language_without_advancing() {
        let mut app = test_app(UiLanguage::En);
        press(&mut app, KeyCode::F(2));
        assert_eq!(app.language, UiLanguage::Ko);
        assert_eq!(app.current(), Step::Shell);
    }

    #[test]
    fn shell_commands_accept_tmux_aliases_and_reject_unrelated_input() {
        assert!(shell_command_matches(Step::Shell, NEW_SESSION_COMMAND));
        assert!(shell_command_matches(
            Step::Shell,
            "tmux   new  -s   muxa-onboarding"
        ));
        assert!(shell_command_matches(Step::Reattach, ATTACH_COMMAND));
        assert!(shell_command_matches(
            Step::Reattach,
            "tmux attach-session -t muxa-onboarding"
        ));
        assert!(!shell_command_matches(Step::Shell, "tmux attach"));
        assert!(!shell_command_matches(
            Step::Reattach,
            "rm -rf muxa-onboarding"
        ));
    }

    #[test]
    fn shell_window_tree_new_window_and_detach_are_persistent_scenes() {
        let mut app = test_app(UiLanguage::Ko);
        let shell = rendered(&app, 130, 32);
        assert!(shell.contains("shell · outside tmux"));
        assert!(shell.contains("tmux new-session -s muxa-onboarding"));

        type_text(&mut app, NEW_SESSION_COMMAND);
        assert_eq!(app.shell_input, NEW_SESSION_COMMAND);
        let typed = rendered(&app, 130, 32);
        assert!(typed.contains(NEW_SESSION_COMMAND));
        press(&mut app, KeyCode::Enter);
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
        );
        press(&mut app, KeyCode::Char('w'));
        let tree = rendered(&app, 130, 32);
        assert!(tree.contains("choose-tree -Zw"));
        assert!(tree.contains("0: shell* (1 panes)"));

        press(&mut app, KeyCode::Char('c'));
        let created = rendered(&app, 130, 32);
        assert!(created.contains("review · shell *"));
        assert!(created.contains("1:review*"));
        assert!(
            created.replace(' ', "").contains("새window준비완료"),
            "{created}"
        );

        app.step = 8;
        press(&mut app, KeyCode::Char('d'));
        let detached = rendered(&app, 130, 32);
        assert!(detached.contains("shell · outside tmux"));
        assert!(detached.contains("[detached (from session muxa-onboarding)]"));
        assert!(detached.contains(ATTACH_COMMAND));
        assert!(!detached.contains("1:review*"));

        press(&mut app, KeyCode::Left);
        let restored = rendered(&app, 130, 32);
        assert_eq!(app.current(), Step::Detach);
        assert!(app.attached());
        assert!(restored.contains("1:review*"));
    }

    #[test]
    fn split_and_copy_mode_render_the_mock_effects() {
        let mut app = test_app(UiLanguage::En);
        app.step = 4;
        app.client_location = ClientLocation::Tmux;
        app.active_window = 1;
        press(&mut app, KeyCode::Char('%'));
        let split = rendered(&app, 130, 32);
        assert!(split.contains("codex · agent"));
        assert!(split.contains("SPLIT THE SELECTED PANE TOP AND BOTTOM"));

        app.step = 7;
        app.copy_stage = CopyStage::Exit;
        let copy = rendered(&app, 130, 32);
        assert!(copy.contains("copy mode · [0/120]"));
        assert!(copy.contains("q exit"));
    }

    #[test]
    fn muxa_shortcuts_flow_from_watch_to_pane_peek_and_back_to_watch() {
        let mut app = test_app(UiLanguage::Ko);
        app.step = 10;
        app.client_location = ClientLocation::Tmux;
        app.panes = 3;

        press(&mut app, KeyCode::Char('s'));
        let watch = rendered(&app, 130, 32);
        assert!(watch.contains("muxa watch"));
        assert!(watch.contains("WORKSPACE › WORK"));
        assert!(watch.contains("Inspector · muxa-onboarding:1.0 · WORK 8s"));
        assert!(watch.contains("j/k move"));
        assert!(!watch.contains("prefix Ctrl-b · 3 panes"));

        press(&mut app, KeyCode::Char('q'));
        let peek = rendered(&app, 130, 32);
        assert!(peek.contains("2 · codex · ● WORKING"));
        assert!(peek.contains("3 · reviewer · ▶ INPUT"));
        assert_eq!(app.muxa_stage, MuxaStage::Complete);
        assert!(peek.replace(' ', "").contains("%와\"로만든panelayout"));
        assert!(!peek.contains("prefix+D"));

        press(&mut app, KeyCode::Char('s'));
        assert!(app.done);
    }

    #[test]
    fn muxa_peek_reuses_the_percent_then_quote_pane_geometry() {
        let panes = mock_pane_areas(Rect::new(0, 0, 120, 30), 3);
        assert_eq!(panes.len(), 3);
        assert_eq!(panes[0].x, panes[2].x);
        assert_eq!(panes[0].width, panes[2].width);
        assert!(panes[2].y > panes[0].y);
        assert!(panes[1].x > panes[0].x);
        assert_eq!(panes[1].height, 30);
    }

    #[test]
    fn compact_terminal_requests_resize() {
        let app = test_app(UiLanguage::En);
        let screen = rendered(&app, 60, 16);
        assert!(screen.contains("needs a little more room"));
        assert!(screen.contains("68 × 20"));
    }
}
