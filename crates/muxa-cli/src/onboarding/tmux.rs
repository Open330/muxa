//! Safe, fullscreen tmux fundamentals track for `muxa onboard --tmux`.
//!
//! The tour begins in an inert shell, enters a virtual tmux session, then
//! detects one real prefix-only press. Inside tmux, the mock observes the
//! current client's transition to the prefix key table and immediately returns
//! that client to the root table before asking for any suffix. Later drills use
//! suffix keys only, so no live binding is executed.

use super::{centered_rect, dialog_block, setup_terminal, tr, Mode, TerminalGuard, UiLanguage};
use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use muxa::AgentState;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
#[cfg(test)]
use ratatui::Terminal;
use std::env;
use std::process::Command;
use std::time::Duration;

#[derive(Debug, Clone)]
struct DetectedPrefix {
    tmux_key: String,
    display: String,
}

pub(super) fn run(mode: Mode, no_quiz: bool, language: UiLanguage) -> Result<()> {
    let prefix = detect_tmux_prefix();
    match mode {
        Mode::Print => print_guide(language, &prefix),
        Mode::Interactive => interactive_guide(no_quiz, language, prefix)?,
    }
    Ok(())
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

fn print_guide(language: UiLanguage, prefix: &DetectedPrefix) {
    if language == UiLanguage::Ko {
        println!("tmux 온보딩");
        println!("============");
        println!("\n현재 prefix: {}", prefix.display);
        println!("\n가상 기본 shell에서 tmux new-session -s CAL-7041로 시작합니다.");
        println!("\nsession = work/ticket\nwindow = layout/room\npane = process/agent");
        println!("\n기본 조합");
        println!("  prefix+w       session/window tree");
        println!("  prefix+c       새 window");
        println!("  prefix+% / \"  좌우 / 상하 pane 분할");
        println!("  prefix+방향키  pane 이동");
        println!("  prefix+z       pane zoom toggle");
        println!("  prefix+[       copy mode, q로 종료");
        println!("  prefix+d       client detach; session은 계속 실행");
        println!("  Enter          가상 shell의 tmux attach-session 실행");
        println!("\nMuxa binding");
        println!("  prefix+s       muxa watch");
        println!("  prefix+q       muxa peek");
        println!("  prefix+D       muxa dashboard");
        println!("\nsession 진입 후 감지된 prefix만 직접 누르고 확인을 기다립니다.");
        println!("이후에는 live binding 실행을 막기 위해 suffix key만 입력합니다.");
        println!("c, %, \" 및 d의 결과는 shell/window/pane 장면에 누적되어 표시됩니다.");
    } else {
        println!("tmux onboarding");
        println!("===============");
        println!("\nCurrent prefix: {}", prefix.display);
        println!("\nStart in a virtual shell with tmux new-session -s CAL-7041.");
        println!("\nsession = work/ticket\nwindow = layout/room\npane = process/agent");
        println!("\nCore combinations");
        println!("  prefix+w       session/window tree");
        println!("  prefix+c       new window");
        println!("  prefix+% / \"  left-right / top-bottom pane splits");
        println!("  prefix+Arrow   move between panes");
        println!("  prefix+z       toggle pane zoom");
        println!("  prefix+[       copy mode; q exits");
        println!("  prefix+d       detach the client; sessions keep running");
        println!("  Enter          run the prepared tmux attach-session in the mock shell");
        println!("\nMuxa bindings");
        println!("  prefix+s       muxa watch");
        println!("  prefix+q       muxa peek");
        println!("  prefix+D       muxa dashboard");
        println!("\nAfter entering the session, press only the detected prefix and wait.");
        println!("Later drills accept suffix keys only, preventing live bindings.");
        println!("The shell/window/pane scene keeps the visible effects of c, %, \" and d.");
    }
}

fn interactive_guide(no_quiz: bool, language: UiLanguage, prefix: DetectedPrefix) -> Result<()> {
    let prefix_probe = TmuxPrefixProbe::detect();
    let terminal = setup_terminal()?;
    let mut guard = TerminalGuard::new(terminal);
    guard.terminal_mut().hide_cursor()?;
    let prefix_capture = if prefix_probe.is_some() {
        PrefixCapture::TmuxClient
    } else {
        PrefixCapture::Direct
    };
    let mut app = TmuxApp::new(no_quiz, language, prefix, prefix_capture);

    while !app.done {
        guard
            .terminal_mut()
            .draw(|frame| render_tour(frame, &app))?;
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
            handle_key(&mut app, key);
        }
    }
    Ok(())
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
    Finish,
}

impl Step {
    const ALL: [Self; 12] = [
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
        Self::Finish,
    ];
}

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
    Dashboard,
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
    match app.current() {
        Step::Shell | Step::Reattach if key.code == KeyCode::Enter => {
            app.client_location = ClientLocation::Tmux;
            app.advance();
        }
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
            app.muxa_stage = MuxaStage::Dashboard;
            app.blocked_hint = false;
        }
        Step::Muxa if app.muxa_stage == MuxaStage::Dashboard && key.code == KeyCode::Char('D') => {
            app.muxa_stage = MuxaStage::Complete;
            app.blocked_hint = false;
        }
        Step::Muxa if app.muxa_stage == MuxaStage::Complete && key.code == KeyCode::Enter => {
            app.advance();
        }
        Step::Finish if key.code == KeyCode::Enter => {
            app.done = true;
        }
        _ => return false,
    }
    true
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
            MuxaStage::Dashboard => {
                frame.render_widget(Clear, area);
                render_mock_terminal(frame, area, app);
                render_muxa_peek(frame, area, app);
            }
            MuxaStage::Complete => render_muxa_dashboard(frame, rows[0], app),
        }
    }
    render_callout(frame, area, app);
}

fn render_shell_terminal(frame: &mut Frame<'_>, area: Rect, app: &TmuxApp) {
    let lines = if app.current() == Step::Reattach {
        vec![
            Line::from("june@devbox:~/personal/muxa$"),
            Line::from("[detached (from session CAL-7041)]"),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    "june@devbox:~/personal/muxa$ ",
                    Style::default().fg(Color::Green),
                ),
                Span::styled(
                    "tmux attach-session -t CAL-7041",
                    Style::default().fg(Color::White),
                ),
                Span::styled("█", Style::default().fg(Color::Cyan)),
            ]),
        ]
    } else {
        vec![
            Line::from("Muxa tmux onboarding · inert shell mock"),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    "june@devbox:~/personal/muxa$ ",
                    Style::default().fg(Color::Green),
                ),
                Span::styled(
                    "tmux new-session -s CAL-7041",
                    Style::default().fg(Color::White),
                ),
                Span::styled("█", Style::default().fg(Color::Cyan)),
            ]),
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
            Line::from("CAL-7041: 1 windows (created Tue Aug 11)"),
            Line::from("└─ 0: shell* (1 panes) [132x43]"),
            Line::from("   └─ 0: zsh  june@devbox:~/personal/muxa"),
            Line::from(""),
            Line::from("w로 연 session/window tree · c로 새 window 생성"),
        ]
    } else {
        vec![
            Line::from("CAL-7041: 1 windows (created Tue Aug 11)"),
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
    match app.panes {
        1 => render_pane(frame, area, app, 0, true),
        2 => {
            let panes = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
                .split(area);
            render_pane(frame, panes[0], app, 0, app.selected_pane == 0);
            render_pane(frame, panes[1], app, 1, app.selected_pane == 1);
        }
        _ => {
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
                .split(area);
            let left = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
                .split(columns[0]);
            render_pane(frame, left[0], app, 0, app.selected_pane == 0);
            render_pane(frame, columns[1], app, 1, app.selected_pane == 1);
            render_pane(frame, left[1], app, 2, app.selected_pane == 2);
        }
    }
}

fn render_pane(frame: &mut Frame<'_>, area: Rect, app: &TmuxApp, index: usize, selected: bool) {
    let (title, lines_en, lines_ko) = match (app.active_window, index) {
        (0, _) => (
            " shell ",
            [
                "june@devbox ~/personal/muxa",
                "",
                "$ tmux display-message -p '#S:#I.#P'",
                "CAL-7041:0.0",
            ],
            [
                "june@devbox ~/personal/muxa",
                "",
                "$ tmux display-message -p '#S:#I.#P'",
                "CAL-7041:0.0",
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
                "› implement CAL-7041",
                "",
                "  ● working",
                "  editing tmux onboarding",
            ],
            [
                "› CAL-7041 구현",
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
    let left = format!(" [CAL-7041] {}", windows.join("  "));
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
            Line::from("↑/↓ 스크롤 · / 검색 · q 종료"),
        ]
    } else {
        vec![
            Line::from("$ cargo test -p muxa-cli onboarding"),
            Line::from("running 11 tests"),
            Line::from("test result: ok. 11 passed"),
            Line::from(""),
            Line::from("↑/↓ scroll · / search · q exit"),
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
                "  2 sessions  ",
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
    let done = tr(
        app.language,
        "dashboard authentication complete",
        "dashboard 인증 완료",
    );
    let lines = vec![
        Line::from(Span::styled(
            "  SESSION                 DUR    ACT    SUMMARY",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(vec![
            Span::styled("> ", selected),
            watch_state_span_with_bg(AgentState::WaitingInput, selected),
            Span::styled(" ", selected),
            watch_state_span_with_bg(AgentState::Working, selected),
            Span::styled(
                format!("   CAL-7041          18m    14m    {summary}"),
                selected,
            ),
        ]),
        Line::from("      └─ CAL-7041:1.0   -      9m     codex · editing onboarding"),
        Line::from(Span::styled(
            "      └─ CAL-7041:1.1   -      4m     reviewer · waiting for input",
            Style::default().fg(Color::Gray),
        )),
        Line::from(vec![
            Span::raw("  "),
            watch_state_span(AgentState::Idle),
            Span::raw(format!("     CAL-7088          7m     2m     {done}")),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(
                Block::default()
                    .title(" Sessions ")
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
                    .title(" Inspector · CAL-7041:1.0 · WORK 8s ")
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
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .margin(2)
        .constraints([
            Constraint::Percentage(34),
            Constraint::Percentage(33),
            Constraint::Percentage(33),
        ])
        .split(area);
    let states = if app.ko() {
        [
            (
                " 1 · editor · ○ IDLE ",
                "최근 prompt 없음\nEnter/숫자: pane 이동",
            ),
            (
                " 2 · codex · ● WORKING ",
                "tmux onboarding 편집 중\n마지막 prompt: 방금 전",
            ),
            (
                " 3 · reviewer · ▶ INPUT ",
                "변경사항 검토 대기\n마지막 prompt: 4분 전",
            ),
        ]
    } else {
        [
            (
                " 1 · editor · ○ IDLE ",
                "no recent prompt\nEnter/digit: jump pane",
            ),
            (
                " 2 · codex · ● WORKING ",
                "editing tmux onboarding\nlast prompted: just now",
            ),
            (
                " 3 · reviewer · ▶ INPUT ",
                "waiting to review changes\nlast prompted: 4m ago",
            ),
        ]
    };
    for (column, (title, body)) in columns.iter().zip(states) {
        let height = 8.min(column.height);
        let popup = Rect::new(column.x, column.y + 1, column.width, height);
        frame.render_widget(
            Paragraph::new(body).wrap(Wrap { trim: false }).block(
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

fn render_muxa_dashboard(frame: &mut Frame<'_>, area: Rect, app: &TmuxApp) {
    frame.render_widget(Clear, area);
    let cards = Layout::default()
        .direction(Direction::Horizontal)
        .margin(1)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    let first = vec![
        Line::from("● 2 agents · 1 working · 1 waiting"),
        Line::from("ACT 14m · WACT 9m"),
        Line::from(""),
        Line::from("codex      ● WORKING"),
        Line::from("reviewer   ▶ INPUT"),
    ];
    let second = if app.ko() {
        vec![
            Line::from("○ 1 agent · idle"),
            Line::from("ACT 2m · WACT 2m"),
            Line::from(""),
            Line::from("claude     ○ IDLE"),
            Line::from("README 업데이트 완료"),
        ]
    } else {
        vec![
            Line::from("○ 1 agent · idle"),
            Line::from("ACT 2m · WACT 2m"),
            Line::from(""),
            Line::from("claude     ○ IDLE"),
            Line::from("README update complete"),
        ]
    };
    for (area, title, lines) in [
        (cards[0], " CAL-7041 · session card ", first),
        (cards[1], " CAL-7088 · session card ", second),
    ] {
        frame.render_widget(
            Paragraph::new(Text::from(lines)).block(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::Cyan)),
            ),
            area,
        );
    }
}

fn render_callout(frame: &mut Frame<'_>, area: Rect, app: &TmuxApp) {
    let popup = callout_rect(area, app.current());
    frame.render_widget(Clear, popup);
    let title = format!(
        " {}/{} · {} ",
        app.step + 1,
        Step::ALL.len(),
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
                    .title_bottom(Line::from(callout_footer(app)).alignment(Alignment::Center)),
            ),
        popup,
    );
}

fn callout_rect(area: Rect, step: Step) -> Rect {
    let width = area.width.saturating_sub(6).min(82);
    let height = if matches!(step, Step::Shell | Step::Finish) {
        16
    } else {
        13
    }
    .min(area.height.saturating_sub(2));
    match step {
        Step::Shell | Step::Prefix | Step::Finish | Step::Zoom | Step::Detach | Step::Reattach => {
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
        Step::Shell => "start outside tmux",
        Step::Prefix => "press the detected prefix safely",
        Step::Model => "session, window, pane",
        Step::Windows => "windows organize one session",
        Step::Splits => "split the layout into panes",
        Step::Panes => "move focus between panes",
        Step::Zoom => "zoom without changing layout",
        Step::CopyMode => "scroll and search history",
        Step::Detach => "client is not the server",
        Step::Reattach => "the shell remains after detach",
        Step::Muxa => "Muxa extends the prefix table",
        Step::Finish => "tmux muscle memory ready",
    };
    let ko = match step {
        Step::Shell => "tmux 밖의 shell에서 시작",
        Step::Prefix => "감지한 prefix를 안전하게 입력",
        Step::Model => "session, window, pane",
        Step::Windows => "하나의 session을 구성하는 window",
        Step::Splits => "layout을 pane으로 분할",
        Step::Panes => "pane 사이 focus 이동",
        Step::Zoom => "layout을 바꾸지 않는 zoom",
        Step::CopyMode => "화면 기록 scroll과 검색",
        Step::Detach => "client와 server는 다릅니다",
        Step::Reattach => "detach 뒤에도 shell은 남아 있습니다",
        Step::Muxa => "Muxa가 확장하는 prefix table",
        Step::Finish => "tmux 기본 동작 준비 완료",
    };
    tr(language, en, ko)
}

fn step_lines(app: &TmuxApp) -> Vec<Line<'static>> {
    if app.ko() {
        return step_lines_ko(app);
    }
    match app.current() {
        Step::Shell => vec![
            label("START FROM A VIRTUAL SHELL OUTSIDE TMUX"),
            Line::from(""),
            Line::from("The shell mock has prepared tmux new-session -s CAL-7041."),
            Line::from("Press Enter to run it and enter the virtual tmux client."),
            Line::from(format!("Configured prefix detected: {}", app.prefix)),
            Line::from("All later windows, panes, and detach actions stay inert."),
            Line::from("F2 switches to 한국어."),
        ],
        Step::Prefix => prefix_lines(app),
        Step::Model => vec![
            label("↓ READ TMUX AS A HIERARCHY"),
            Line::from(""),
            mapping_line("SESSION", "work / ticket"),
            mapping_line("WINDOW", "layout / room"),
            mapping_line("PANE", "process / agent"),
            Line::from(""),
            Line::from("prefix+w opens the session/window tree. Press w."),
        ],
        Step::Windows => vec![
            label("↓ WINDOWS ORGANIZE A SESSION"),
            Line::from(""),
            Line::from("prefix+c creates another window in the current session."),
            Line::from("A window is layout, not another Muxa work identity."),
            Line::from("Press c; a new 1:review shell will fill the client."),
        ],
        Step::Splits if app.split_stage == SplitStage::LeftRight => vec![
            label("SPLIT LEFT AND RIGHT"),
            Line::from(""),
            Line::from("prefix+% splits the selected pane left/right."),
            Line::from("Press % (usually Shift-5). The mock only redraws itself."),
        ],
        Step::Splits => vec![
            label("NOW SPLIT TOP AND BOTTOM"),
            Line::from(""),
            Line::from("prefix+\" splits the selected pane top/bottom."),
            Line::from("Press \" (usually Shift-apostrophe)."),
        ],
        Step::Panes => vec![
            label("MOVE THE ACTIVE PANE BORDER"),
            Line::from(""),
            Line::from("prefix+Arrow moves focus spatially; prefix+o cycles panes."),
            Line::from("Press → to select the agent pane on the right."),
        ],
        Step::Zoom if app.zoom_stage == ZoomStage::In => vec![
            label("ZOOM IS REVERSIBLE"),
            Line::from(""),
            Line::from("prefix+z toggles the selected pane fullscreen."),
            Line::from("The underlying split layout remains intact."),
            Line::from("Press z to zoom the selected pane in this mock."),
        ],
        Step::Zoom => vec![
            label("TOGGLE ZOOM BACK OUT"),
            Line::from(""),
            Line::from("The pane is fullscreen but the split layout still exists."),
            Line::from("Press z again to restore every pane."),
        ],
        Step::CopyMode if app.copy_stage == CopyStage::Enter => vec![
            label("ENTER COPY MODE"),
            Line::from(""),
            Line::from("prefix+[ freezes navigation into pane history."),
            Line::from("Press [ to open a mock scroll/search view."),
        ],
        Step::CopyMode => vec![
            label("COPY MODE HAS ITS OWN KEY TABLE"),
            Line::from(""),
            Line::from("Use arrows/PageUp/PageDown to scroll and / to search."),
            Line::from("Press q to leave copy mode and return to the pane."),
        ],
        Step::Detach => vec![
            label("DETACH DOES NOT STOP THE WORK"),
            Line::from(""),
            Line::from("prefix+d detaches this client from the tmux server."),
            Line::from("Sessions, panes, and agents continue running."),
            Line::from("Press d to simulate a safe detach and reattach."),
        ],
        Step::Reattach => vec![
            label("YOU ARE BACK IN THE ORIGINAL SHELL"),
            Line::from(""),
            Line::from("The [detached] notice proves only the client left tmux."),
            Line::from("CAL-7041 and every pane continue in the tmux server."),
            Line::from("The shell mock prepared tmux attach-session -t CAL-7041."),
            Line::from("Press Enter to reattach and continue with Muxa keys."),
        ],
        Step::Muxa => muxa_lines(app),
        Step::Finish => vec![
            label("KEEP TMUX FOR LAYOUT, MUXA FOR AGENTS"),
            Line::from(""),
            mapping_line("SESSION", "work / ticket"),
            mapping_line("WINDOW", "layout / room"),
            mapping_line("PANE", "process / agent"),
            Line::from(""),
            Line::from("Use Muxa controls for managed-agent lifecycle and safety."),
            Line::from("Press Enter to finish; no real tmux state was changed."),
        ],
    }
}

fn step_lines_ko(app: &TmuxApp) -> Vec<Line<'static>> {
    match app.current() {
        Step::Shell => vec![
            label("TMUX 밖의 가상 기본 SHELL에서 시작합니다"),
            Line::from(""),
            Line::from("shell mock에 tmux new-session -s CAL-7041이 준비되어 있습니다."),
            Line::from("Enter를 누르면 실행되어 가상 tmux client로 들어갑니다."),
            Line::from(format!("설정에서 감지한 prefix: {}", app.prefix)),
            Line::from("이후 window, pane, detach 동작은 모두 mock 안에서만 일어납니다."),
            Line::from("F2는 English 전환입니다."),
        ],
        Step::Prefix => prefix_lines(app),
        Step::Model => vec![
            label("↓ TMUX를 계층 구조로 이해하세요"),
            Line::from(""),
            mapping_line("SESSION", "work / ticket"),
            mapping_line("WINDOW", "layout / room"),
            mapping_line("PANE", "process / agent"),
            Line::from(""),
            Line::from("prefix+w는 session/window tree를 엽니다. w를 누르세요."),
        ],
        Step::Windows => vec![
            label("↓ WINDOW는 SESSION 안을 구성합니다"),
            Line::from(""),
            Line::from("prefix+c는 현재 session에 window를 하나 만듭니다."),
            Line::from("window는 layout이며 별도의 Muxa work identity가 아닙니다."),
            Line::from("c를 누르면 새 1:review shell이 client 전체에 표시됩니다."),
        ],
        Step::Splits if app.split_stage == SplitStage::LeftRight => vec![
            label("좌우로 분할하세요"),
            Line::from(""),
            Line::from("prefix+%는 선택한 pane을 좌우로 분할합니다."),
            Line::from("%를 누르세요(보통 Shift-5). mock 화면만 다시 그립니다."),
        ],
        Step::Splits => vec![
            label("이제 상하로 분할하세요"),
            Line::from(""),
            Line::from("prefix+\"는 선택한 pane을 상하로 분할합니다."),
            Line::from("\"를 누르세요(보통 Shift-apostrophe)."),
        ],
        Step::Panes => vec![
            label("ACTIVE PANE BORDER를 옮겨보세요"),
            Line::from(""),
            Line::from("prefix+방향키는 공간 방향으로, prefix+o는 순서대로 이동합니다."),
            Line::from("→를 눌러 오른쪽 agent pane을 선택하세요."),
        ],
        Step::Zoom if app.zoom_stage == ZoomStage::In => vec![
            label("ZOOM은 원래대로 되돌릴 수 있습니다"),
            Line::from(""),
            Line::from("prefix+z는 선택 pane의 fullscreen을 toggle합니다."),
            Line::from("기존 split layout은 그대로 유지됩니다."),
            Line::from("z를 눌러 선택 pane을 mock에서 확대하세요."),
        ],
        Step::Zoom => vec![
            label("ZOOM을 다시 원래대로 돌리세요"),
            Line::from(""),
            Line::from("pane은 fullscreen이지만 기존 split layout은 남아 있습니다."),
            Line::from("z를 다시 눌러 모든 pane을 복원하세요."),
        ],
        Step::CopyMode if app.copy_stage == CopyStage::Enter => vec![
            label("COPY MODE로 들어가세요"),
            Line::from(""),
            Line::from("prefix+[는 pane의 이전 화면을 탐색하는 mode로 전환합니다."),
            Line::from("[를 눌러 mock scroll/search 화면을 여세요."),
        ],
        Step::CopyMode => vec![
            label("COPY MODE는 별도의 KEY TABLE을 사용합니다"),
            Line::from(""),
            Line::from("방향키/PageUp/PageDown으로 scroll하고 /로 검색합니다."),
            Line::from("q를 눌러 copy mode를 닫고 pane으로 돌아가세요."),
        ],
        Step::Detach => vec![
            label("DETACH해도 작업은 중단되지 않습니다"),
            Line::from(""),
            Line::from("prefix+d는 이 client만 tmux server에서 분리합니다."),
            Line::from("session, pane, agent는 계속 실행됩니다."),
            Line::from("d를 눌러 안전한 detach/reattach를 시뮬레이션하세요."),
        ],
        Step::Reattach => vec![
            label("원래의 기본 SHELL로 돌아왔습니다"),
            Line::from(""),
            Line::from("[detached] 표시는 이 client만 tmux에서 나온 결과입니다."),
            Line::from("CAL-7041 session과 모든 pane은 server에서 계속 실행됩니다."),
            Line::from("shell mock에 tmux attach-session -t CAL-7041이 준비되었습니다."),
            Line::from("Enter를 눌러 다시 붙은 뒤 Muxa key를 실습하세요."),
        ],
        Step::Muxa => muxa_lines(app),
        Step::Finish => vec![
            label("TMUX는 LAYOUT, MUXA는 AGENT 관리에 사용하세요"),
            Line::from(""),
            mapping_line("SESSION", "work / ticket"),
            mapping_line("WINDOW", "layout / room"),
            mapping_line("PANE", "process / agent"),
            Line::from(""),
            Line::from("managed agent lifecycle과 안전 제어에는 Muxa를 사용하세요."),
            Line::from("Enter로 마칩니다. 실제 tmux 상태는 전혀 바뀌지 않았습니다."),
        ],
    }
}

fn prefix_lines(app: &TmuxApp) -> Vec<Line<'static>> {
    let mut lines = vec![
        label(tr(
            app.language,
            "PRESS THE PREFIX BY ITSELF",
            "PREFIX만 단독으로 누르세요",
        )),
        Line::from(""),
        Line::from(format!(
            "{}: {}",
            tr(app.language, "Detected prefix", "감지한 prefix"),
            app.prefix
        )),
    ];
    if app.prefix_capture == PrefixCapture::TmuxClient {
        lines.extend([
            Line::from(tr(
                app.language,
                "Press it once, release it, and wait for this dialog to advance.",
                "한 번 누르고 손을 뗀 뒤 이 dialog가 넘어갈 때까지 기다리세요.",
            )),
            Line::from(tr(
                app.language,
                "Do not press w yet. Muxa will immediately cancel the live prefix table.",
                "아직 w를 누르지 마세요. Muxa가 live prefix table을 즉시 해제합니다.",
            )),
        ]);
    } else {
        lines.push(Line::from(tr(
            app.language,
            "Press it once. This terminal is outside tmux, so the mock receives it directly.",
            "한 번 누르세요. 현재 tmux 밖이므로 mock이 prefix를 직접 받습니다.",
        )));
    }
    lines
}

fn muxa_lines(app: &TmuxApp) -> Vec<Line<'static>> {
    let (done, key, description_en, description_ko) = match app.muxa_stage {
        MuxaStage::Watch => (
            "",
            "s",
            "prefix+s opens muxa watch for observation and collaboration.",
            "prefix+s는 관측과 협업을 위한 muxa watch를 엽니다.",
        ),
        MuxaStage::Peek => (
            "✓ prefix+s  ",
            "q",
            "prefix+q overlays every pane with its agent state and jump digit.",
            "prefix+q는 모든 pane 위에 agent 상태와 이동 숫자를 표시합니다.",
        ),
        MuxaStage::Dashboard => (
            "✓ prefix+s  ✓ prefix+q  ",
            "D",
            "prefix+D opens the richer session-card dashboard.",
            "prefix+D는 상세한 session-card dashboard를 엽니다.",
        ),
        MuxaStage::Complete => (
            "✓ prefix+s  ✓ prefix+q  ✓ prefix+D",
            "Enter",
            "The mock now shows session cards with agent state and ACT/WACT.",
            "mock에 agent 상태와 ACT/WACT가 있는 session card가 표시됩니다.",
        ),
    };
    vec![
        label(tr(
            app.language,
            "MUXA ADDS THREE MANAGED PREFIX KEYS",
            "MUXA가 세 개의 MANAGED PREFIX KEY를 추가합니다",
        )),
        Line::from(""),
        Line::from(Span::styled(
            done,
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(tr(app.language, description_en, description_ko)),
        Line::from(if app.muxa_stage == MuxaStage::Complete {
            tr(
                app.language,
                "Press Enter after inspecting the dashboard.",
                "dashboard를 확인한 뒤 Enter를 누르세요.",
            )
            .to_string()
        } else {
            format!(
                "{} {}.",
                tr(app.language, "Press suffix", "suffix를 누르세요:"),
                key
            )
        }),
    ]
}

fn expected_key(app: &TmuxApp) -> String {
    match app.current() {
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
        Step::Muxa if app.muxa_stage == MuxaStage::Dashboard => "D",
        Step::Shell | Step::Reattach | Step::Finish | Step::Muxa => "Enter",
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
    if matches!(app.current(), Step::Shell | Step::Reattach | Step::Finish) {
        return tr(
            app.language,
            " Enter continue · F2 한국어 · Esc quit ",
            " Enter 계속 · F2 English · Esc 종료 ",
        )
        .to_string();
    }
    if app.current() == Step::Prefix {
        return format!(
            " {} {} · {} ",
            tr(app.language, "press only", "단독 입력"),
            app.prefix,
            tr(app.language, "wait for ✓ · Esc quit", "✓ 대기 · Esc 종료")
        );
    }
    format!(
        " prefix {} simulated · press {} · ← back · Esc quit ",
        app.prefix,
        expected_key(app)
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
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.current(), Step::Muxa);
        assert!(app.attached());
        press(&mut app, KeyCode::Char('s'));
        press(&mut app, KeyCode::Char('q'));
        press(&mut app, KeyCode::Char('D'));
        assert_eq!(app.current(), Step::Muxa);
        assert_eq!(app.muxa_stage, MuxaStage::Complete);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.current(), Step::Finish);
        press(&mut app, KeyCode::Enter);
        assert!(app.done);
    }

    #[test]
    fn korean_track_explains_virtual_prefix_and_tmux_hierarchy() {
        let app = TmuxApp::new(
            false,
            UiLanguage::Ko,
            detected("C-a"),
            PrefixCapture::TmuxClient,
        );
        let screen = rendered(&app, 130, 32).replace(' ', "");
        assert!(screen.contains("TMUX밖의가상기본SHELL에서시작합니다"));
        assert!(screen.contains("설정에서감지한prefix:Ctrl-a"));

        let mut model = app;
        model.step = 2;
        model.client_location = ClientLocation::Tmux;
        let screen = rendered(&model, 130, 32).replace(' ', "");
        assert!(screen.contains("SESSION=work/ticket"));
        assert!(screen.contains("WINDOW=layout/room"));
        assert!(screen.contains("PANE=process/agent"));
    }

    #[test]
    fn f2_switches_language_without_advancing() {
        let mut app = test_app(UiLanguage::En);
        press(&mut app, KeyCode::F(2));
        assert_eq!(app.language, UiLanguage::Ko);
        assert_eq!(app.current(), Step::Shell);
    }

    #[test]
    fn shell_window_tree_new_window_and_detach_are_persistent_scenes() {
        let mut app = test_app(UiLanguage::Ko);
        let shell = rendered(&app, 130, 32);
        assert!(shell.contains("shell · outside tmux"));
        assert!(shell.contains("tmux new-session -s CAL-7041"));

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
        assert!(detached.contains("[detached (from session CAL-7041)]"));
        assert!(detached.contains("tmux attach-session -t CAL-7041"));
        assert!(!detached.contains("1:review*"));

        press(&mut app, KeyCode::Backspace);
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
        assert!(split.contains("NOW SPLIT TOP AND BOTTOM"));

        app.step = 7;
        app.copy_stage = CopyStage::Exit;
        let copy = rendered(&app, 130, 32);
        assert!(copy.contains("copy mode · [0/120]"));
        assert!(copy.contains("q exit"));
    }

    #[test]
    fn muxa_shortcuts_render_watch_peek_and_dashboard_surfaces() {
        let mut app = test_app(UiLanguage::Ko);
        app.step = 10;
        app.client_location = ClientLocation::Tmux;

        press(&mut app, KeyCode::Char('s'));
        let watch = rendered(&app, 130, 32);
        assert!(watch.contains("muxa watch"));
        assert!(watch.contains("SESSION                 DUR    ACT    SUMMARY"));
        assert!(watch.contains("Inspector · CAL-7041:1.0 · WORK 8s"));
        assert!(watch.contains("j/k move"));
        assert!(!watch.contains("prefix Ctrl-b · 3 panes"));

        press(&mut app, KeyCode::Char('q'));
        let peek = rendered(&app, 130, 32);
        assert!(peek.contains("2 · codex · ● WORKING"));
        assert!(peek.contains("3 · reviewer · ▶ INPUT"));

        press(&mut app, KeyCode::Char('D'));
        let dashboard = rendered(&app, 130, 32);
        assert!(dashboard.contains("CAL-7041 · session card"));
        assert!(dashboard.contains("ACT 14m · WACT 9m"));
    }

    #[test]
    fn compact_terminal_requests_resize() {
        let app = test_app(UiLanguage::En);
        let screen = rendered(&app, 60, 16);
        assert!(screen.contains("needs a little more room"));
        assert!(screen.contains("68 × 20"));
    }
}
