//! Fullscreen and printable Muxa onboarding.
//!
//! Interactive onboarding renders a stable mock of `muxa watch`, then places
//! short, location-aware dialogs beside the part being explained. This lets a
//! new user connect the domain model and shortcuts to the UI before touching
//! live sessions. `--print` remains available for scripts and accessibility.

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
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
    title: &'static str,
    body: &'static str,
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

const SECTIONS: &[Section] = &[
    Section {
        title: "1 · Mental model",
        body: POLICY,
    },
    Section {
        title: "2 · Default workflow",
        body: WORKFLOW,
    },
    Section {
        title: "3 · Agent-facing MCP pattern",
        body: MCP_PATTERN,
    },
    Section {
        title: "4 · Safety boundary",
        body: SAFETY,
    },
];

pub fn run(args: Args) -> Result<()> {
    let mode = Mode::detect(args.print);
    match mode {
        Mode::Print => print_guide(),
        Mode::Interactive => interactive_guide(args.no_quiz)?,
    }
    Ok(())
}

fn print_guide() {
    println!("Muxa onboarding");
    println!("===============");
    for section in SECTIONS {
        println!("\n{}\n{}\n", section.title, "-".repeat(section.title.len()));
        println!("{}", section.body);
    }
    println!("\n5 · muxa watch shortcuts\n------------------------");
    for line in crate::watch::help_overlay_text() {
        println!("{line}");
    }
    println!("\nNext: run muxa watch or press tmux prefix+s.");
}

fn interactive_guide(no_quiz: bool) -> Result<()> {
    let terminal = setup_terminal()?;
    let mut guard = TerminalGuard::new(terminal);
    guard.terminal_mut().hide_cursor()?;
    let mut app = TourApp::new(no_quiz);

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
    NewWork,
    Message,
    Mailbox,
}

#[derive(Debug)]
struct TourApp {
    step: usize,
    new_work_opened: bool,
    collaboration_overlay: MockOverlay,
    blocked_hint: bool,
    done: bool,
}

impl TourApp {
    fn new(no_quiz: bool) -> Self {
        Self {
            step: 0,
            new_work_opened: no_quiz,
            collaboration_overlay: MockOverlay::None,
            blocked_hint: false,
            done: false,
        }
    }

    fn current(&self) -> TourStep {
        TourStep::ALL[self.step]
    }

    fn next(&mut self) {
        if self.current() == TourStep::NewWork && !self.new_work_opened {
            self.blocked_hint = true;
            return;
        }
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
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => app.done = true,
        KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => app.previous(),
        KeyCode::Right | KeyCode::Char('l' | ' ') | KeyCode::Enter => app.next(),
        KeyCode::Home => app.step = 0,
        KeyCode::End => app.step = TourStep::ALL.len() - 1,
        KeyCode::Char('n') if app.current() == TourStep::NewWork => {
            app.new_work_opened = true;
            app.blocked_hint = false;
        }
        KeyCode::Char('m') if app.current() == TourStep::Collaboration => {
            app.collaboration_overlay = MockOverlay::Message;
        }
        KeyCode::Char('M') if app.current() == TourStep::Collaboration => {
            app.collaboration_overlay = MockOverlay::Mailbox;
        }
        _ => {}
    }
}

fn render_tour(frame: &mut Frame<'_>, app: &TourApp) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(Color::Rgb(8, 12, 20))),
        area,
    );
    if area.width < 68 || area.height < 20 {
        render_small_terminal(frame, area);
        return;
    }

    let shell = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Rgb(61, 78, 105)))
        .style(Style::default().bg(Color::Rgb(8, 12, 20)));
    let inner = shell.inner(area);
    frame.render_widget(shell, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(3),
        ])
        .margin(1)
        .split(inner);
    render_mock_header(frame, rows[0], app);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(rows[1]);
    render_mock_work(frame, columns[0], app.current());
    render_mock_inspector(frame, columns[1], app.current());
    render_mock_footer(frame, rows[2], app.current());

    let overlay = match app.current() {
        TourStep::NewWork if app.new_work_opened => MockOverlay::NewWork,
        TourStep::Collaboration => app.collaboration_overlay,
        _ => MockOverlay::None,
    };
    render_mock_overlay(frame, rows[1], overlay);
    render_callout(frame, area, app);
}

fn render_small_terminal(frame: &mut Frame<'_>, area: Rect) {
    let popup = centered_rect(
        area,
        area.width.saturating_sub(4).min(62),
        9.min(area.height),
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from(Span::styled(
                "Muxa onboarding needs a little more room",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from("Resize to at least 68 × 20."),
            Line::from("Esc or q closes the tour."),
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
    let progress = (0..TourStep::ALL.len())
        .map(|index| if index <= app.step { '●' } else { '○' })
        .collect::<String>();
    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            " MUXA WATCH ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  SESSION VIEW  ", Style::default().fg(Color::White)),
        Span::styled("2 work · 3 agents  ", Style::default().fg(Color::DarkGray)),
        Span::styled(progress, Style::default().fg(Color::Cyan)),
    ]))
    .block(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(Color::Rgb(43, 57, 78))),
    );
    frame.render_widget(header, area);
}

fn render_mock_work(frame: &mut Frame<'_>, area: Rect, step: TourStep) {
    let work_style = if step == TourStep::Work {
        selected_style()
    } else {
        Style::default().fg(Color::White)
    };
    let agent_style = if matches!(step, TourStep::Agents | TourStep::States) {
        Style::default().fg(Color::White).bg(Color::Rgb(25, 48, 64))
    } else {
        Style::default().fg(Color::Gray)
    };
    let state_bg = if matches!(step, TourStep::Agents | TourStep::States) {
        Color::Rgb(25, 48, 64)
    } else {
        Color::Reset
    };
    let lines = vec![
        Line::from(Span::styled(
            "WORK / SESSION                AGENTS  STATE",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(vec![
            Span::styled("▾ CAL-7041  checkout-hardening     2     ", work_style),
            Span::styled("ACTIVE", work_style.fg(Color::Cyan)),
        ]),
        Line::from(vec![
            Span::styled("  ├─ %42  codex   implementer       ", agent_style),
            Span::styled("WORKING", Style::default().fg(Color::Green).bg(state_bg)),
        ]),
        Line::from(vec![
            Span::styled("  └─ %43  claude  reviewer          ", agent_style),
            Span::styled("WAITING", Style::default().fg(Color::Yellow).bg(state_bg)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "▸ CAL-7088  dashboard-auth         1     ",
                Style::default().fg(Color::Gray),
            ),
            Span::styled("IDLE", Style::default().fg(Color::Blue)),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  session = work    child pane = agent",
            Style::default().fg(Color::Rgb(88, 110, 139)),
        )),
    ];
    let border = if matches!(step, TourStep::Work | TourStep::Agents | TourStep::States) {
        Color::Cyan
    } else {
        Color::Rgb(43, 57, 78)
    };
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(
            Block::default()
                .title(" WORK ")
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(border)),
        ),
        area,
    );
}

fn render_mock_inspector(frame: &mut Frame<'_>, area: Rect, step: TourStep) {
    let (title, lines) = if step == TourStep::Mcp {
        (
            " MCP ",
            vec![
                Line::from(Span::styled(
                    "muxa_start_agent",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from("  work: CAL-7041"),
                Line::from("  role: reviewer"),
                Line::from(""),
                Line::from(Span::styled(
                    "muxa_wait_for_change",
                    Style::default().fg(Color::Cyan),
                )),
                Line::from("  pane: %43"),
                Line::from("  until: settled"),
                Line::from("  include_capture: true"),
            ],
        )
    } else {
        (
            " INSPECTOR · %43 ",
            vec![
                Line::from(vec![
                    Span::styled("claude", Style::default().fg(Color::Magenta)),
                    Span::raw(" · reviewer · CAL-7041"),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    "$ review the current changes",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from("I found one behavior mismatch in"),
                Line::from("the public-read control boundary."),
                Line::from(""),
                Line::from(Span::styled(
                    "Waiting for your response...",
                    Style::default().fg(Color::Yellow),
                )),
            ],
        )
    };
    let border = if matches!(step, TourStep::Preview | TourStep::Mcp) {
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

fn render_mock_footer(frame: &mut Frame<'_>, area: Rect, step: TourStep) {
    let highlighted = matches!(
        step,
        TourStep::Shortcuts | TourStep::NewWork | TourStep::Collaboration
    );
    let style = if highlighted {
        Style::default()
            .fg(Color::White)
            .bg(Color::Rgb(19, 61, 80))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" Enter", style.fg(Color::Cyan)),
            Span::styled(" attach  ", style),
            Span::styled("n", style.fg(Color::Cyan)),
            Span::styled(" new work  ", style),
            Span::styled("m/M", style.fg(Color::Cyan)),
            Span::styled(" message/mailbox  ", style),
            Span::styled("a/A", style.fg(Color::Cyan)),
            Span::styled(" ask/history  ", style),
            Span::styled("?", style.fg(Color::Cyan)),
            Span::styled(" help", style),
        ]))
        .block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(Color::Rgb(43, 57, 78))),
        ),
        area,
    );
}

fn render_mock_overlay(frame: &mut Frame<'_>, area: Rect, overlay: MockOverlay) {
    match overlay {
        MockOverlay::None => {}
        MockOverlay::NewWork => {
            let popup = centered_rect(area, area.width.min(66), area.height.min(13));
            frame.render_widget(Clear, popup);
            frame.render_widget(
                Paragraph::new(Text::from(vec![
                    Line::from(vec![
                        Span::styled("cwd     ", Style::default().fg(Color::DarkGray)),
                        Span::raw("/home/june/personal/muxa"),
                    ]),
                    Line::from(vec![
                        Span::styled("ticket  ", Style::default().fg(Color::DarkGray)),
                        Span::styled("CAL-7041", Style::default().fg(Color::Cyan)),
                    ]),
                    Line::from(vec![
                        Span::styled("agent   ", Style::default().fg(Color::DarkGray)),
                        Span::raw("codex"),
                    ]),
                    Line::from(vec![
                        Span::styled("prompt  ", Style::default().fg(Color::DarkGray)),
                        Span::raw("Implement checkout hardening"),
                    ]),
                    Line::from(""),
                    Line::from(Span::styled(
                        "Tab fields · Enter start · Esc close",
                        Style::default().fg(Color::Yellow),
                    )),
                ]))
                .block(dialog_block(" new work + agent ", Color::Cyan)),
                popup,
            );
        }
        MockOverlay::Message => {
            render_compact_popup(
                frame,
                area,
                " message → claude@%43 ",
                "kind review   mode read_only\n\nReview the current changes and report findings.\n\nTab contract · Enter send · Backspace/Esc close",
            );
        }
        MockOverlay::Mailbox => {
            render_compact_popup(
                frame,
                area,
                " mailbox ",
                "incoming 0   sent 1\n\n▸ review · read_only · claude@%43\n  Review the current changes\n\nM/Esc close · Enter inspect",
            );
        }
    }
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
    let popup = callout_rect(area, step);
    frame.render_widget(Clear, popup);
    let title = format!(
        " {}/{} · {} ",
        app.step + 1,
        TourStep::ALL.len(),
        step_title(step)
    );
    let body = step_body(app);
    let footer = match step {
        TourStep::Finish => " Enter finish · ← back · Esc quit ",
        TourStep::NewWork if !app.new_work_opened => " press n here · ← back · Esc quit ",
        _ => " ←/Backspace back · Enter/→ next · Esc quit ",
    };
    frame.render_widget(
        Paragraph::new(body).wrap(Wrap { trim: false }).block(
            dialog_block(&title, Color::Cyan)
                .title_bottom(Line::from(footer).alignment(Alignment::Center)),
        ),
        popup,
    );
}

fn step_title(step: TourStep) -> &'static str {
    match step {
        TourStep::Welcome => "learn on a safe mock",
        TourStep::Work => "one session = one work",
        TourStep::Agents => "one pane = one agent",
        TourStep::States => "state tells you what to do",
        TourStep::Preview => "inspect without attaching",
        TourStep::Shortcuts => "actions stay at the bottom",
        TourStep::NewWork => "try n: new work + agent",
        TourStep::Collaboration => "message one exact peer",
        TourStep::Mcp => "agents use the same control plane",
        TourStep::Finish => "ready for the live dashboard",
    }
}

fn step_body(app: &TourApp) -> Text<'static> {
    let mut lines = step_lines(app);
    if app.blocked_hint {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Press n once to continue.",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
    }
    Text::from(lines)
}

fn step_lines(app: &TourApp) -> Vec<Line<'static>> {
    match app.current() {
        TourStep::Welcome => vec![
            callout_label("↙ THIS IS A MOCK OF MUXA WATCH"),
            Line::from(""),
            Line::from("Nothing here touches your real tmux sessions."),
            Line::from("Move with Enter/→; go back with ← or Backspace."),
            Line::from(""),
            policy_line("SESSION", "work / ticket"),
            policy_line("PANE", "agent"),
            policy_line("WINDOW", "layout only"),
        ],
        TourStep::Work => vec![
            callout_label("← LOOK AT THE HIGHLIGHTED ROW"),
            Line::from(""),
            Line::from("CAL-7041 is the work identity and the tmux session."),
            Line::from("Starting CAL-7041 again reuses this row; it does not"),
            Line::from("silently create another ticket session."),
        ],
        TourStep::Agents => vec![
            callout_label("← THESE CHILD ROWS ARE PANES"),
            Line::from(""),
            Line::from("%42 implements while %43 reviews the same work."),
            Line::from("Role and task metadata follow the exact pane."),
            Line::from("Windows only arrange these panes on screen."),
        ],
        TourStep::States => vec![
            callout_label("← READ STATE BEFORE SENDING"),
            Line::from(""),
            Line::from("WORKING: leave it alone.  WAITING: it needs input."),
            Line::from("IDLE: its turn settled.  ERROR: inspect the pane."),
            Line::from("Muxa can wait until settled instead of polling."),
        ],
        TourStep::Preview => vec![
            callout_label("→ THE INSPECTOR SHOWS THE LIVE PANE"),
            Line::from(""),
            Line::from("Use o or Alt-P to preview without attaching."),
            Line::from("Enter attaches when you need the real terminal."),
            Line::from("Alt-I keeps the inspector beside the work list."),
        ],
        TourStep::Shortcuts => vec![
            callout_label("↓ THE FOOTER TEACHES CONTEXTUAL ACTIONS"),
            Line::from(""),
            Line::from("n new/reused work  ·  m message  ·  M mailbox"),
            Line::from("a ask  ·  A history  ·  ? complete help"),
            Line::from("Alt-K terminates only after confirmation."),
        ],
        TourStep::NewWork if !app.new_work_opened => vec![
            callout_label("↓ YOUR TURN"),
            Line::from(""),
            Line::from(Span::styled(
                "Press n to open the mock work form.",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from("The exercise is local to this tutorial."),
        ],
        TourStep::NewWork => vec![
            callout_label("↑ THIS IS WHAT n OPENS"),
            Line::from(""),
            Line::from("Choose cwd, ticket, agent, and the first prompt."),
            Line::from("An existing ticket adds a pane to its session."),
            Line::from("Press Enter or → to continue the tour."),
        ],
        TourStep::Collaboration => vec![
            callout_label("↓ TRY m OR M"),
            Line::from(""),
            Line::from("m composes for the selected agent; M opens mailbox."),
            Line::from("kind and mode stay visible and are remembered."),
            Line::from("A single-peer work routes without another picker."),
        ],
        TourStep::Mcp => vec![
            callout_label("→ THE AGENT SEES COMPACT MUXA TOOLS"),
            Line::from(""),
            Line::from("muxa_start_agent creates/reuses tmux deterministically."),
            Line::from("settled + capture returns the useful final screen."),
            Line::from("No separate tmux MCP or model-written tmux script."),
        ],
        TourStep::Finish => vec![
            callout_label("THE MODEL TO REMEMBER"),
            Line::from(""),
            policy_line("SESSION", "work / ticket"),
            policy_line("PANE", "agent"),
            policy_line("WINDOW", "layout only"),
            Line::from(""),
            Line::from(Span::styled(
                "Next: muxa watch  (or tmux prefix+s)",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
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

fn callout_rect(area: Rect, step: TourStep) -> Rect {
    let compact = area.width < 100 || area.height < 28;
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
        TourStep::NewWork | TourStep::Collaboration => {
            Rect::new(area.x + 4, area.y + area.height - 10, area.width - 8, 8)
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
    fn onboarding_policy_and_workflow_pin_the_domain_model() {
        assert!(POLICY.contains("session = work/ticket"));
        assert!(POLICY.contains("pane    = agent"));
        assert!(POLICY.contains("window  = layout only"));
        assert!(WORKFLOW.contains("muxa work start CAL-7041"));
        assert!(WORKFLOW.contains("muxa agent start --work CAL-7041"));
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
        assert!(work.contains("MUXA WATCH"));
        assert!(work.contains("CAL-7041"));
        assert!(work.contains("one session = one work"));
        assert!(work.contains("LOOK AT THE HIGHLIGHTED ROW"));

        app.step = TourStep::ALL
            .iter()
            .position(|step| *step == TourStep::Preview)
            .unwrap();
        let preview = rendered(&app, 120, 34);
        assert!(preview.contains("INSPECTOR"));
        assert!(preview.contains("THE INSPECTOR SHOWS THE LIVE PANE"));
    }

    #[test]
    fn n_practice_opens_a_mock_work_form_before_advancing() {
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
        assert!(app.new_work_opened);
        let screen = rendered(&app, 120, 34);
        assert!(screen.contains("new work + agent"));
        assert!(screen.contains("ticket"));
        assert!(screen.contains("CAL-7041"));

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE),
        );
        assert_eq!(app.current(), TourStep::Collaboration);
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
            KeyEvent::new(KeyCode::Char('M'), crossterm::event::KeyModifiers::SHIFT),
        );
        assert!(rendered(&app, 120, 34).contains("sent 1"));
    }

    #[test]
    fn compact_terminals_render_a_resize_message_without_panicking() {
        let screen = rendered(&TourApp::new(false), 60, 16);
        assert!(screen.contains("needs a little more room"));
        assert!(screen.contains("68 × 20"));

        let compact = rendered(&TourApp::new(false), 80, 24);
        assert!(compact.contains("MUXA WATCH"));
        assert!(compact.contains("CAL-7041"));
        assert!(compact.contains("learn on a safe mock"));
    }
}
