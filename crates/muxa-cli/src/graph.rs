//! In-terminal collaboration graph. Reads the durable mailbox without writes.
mod labels;
mod model;
mod physics;
use anyhow::{Context, Result};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use model::{Graph, Kind, Point};
use muxa::{
    collaboration::{read_history_page, CollaborationPage, CollaborationRequest},
    Config,
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    symbols::Marker,
    text::{Line, Span},
    widgets::{
        canvas::{Canvas, Circle, Line as CanvasLine},
        Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap,
    },
    Frame, Terminal,
};
use std::{
    collections::HashMap,
    io::{self, IsTerminal},
    path::PathBuf,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// Number of recent messages to load initially. Press n to load older ones.
    #[arg(long,default_value_t=200,value_parser=clap::value_parser!(u16).range(1..=5000))]
    limit: u16,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Graph,
    Messages,
    Detail,
}
#[derive(Clone, Copy)]
struct Drag {
    node: Option<usize>,
    start: Point,
}
#[allow(clippy::struct_excessive_bools)] // Independent UI toggles, not lifecycle states.
struct App {
    path: PathBuf,
    requests: Vec<CollaborationRequest>,
    matching: Vec<usize>,
    total: usize,
    limit: usize,
    graph: Graph,
    physics: physics::Physics,
    show_groups: bool,
    edge_labels: bool,
    inspector: bool,
    fit_span: f64,
    positions: HashMap<String, Point>,
    selected: usize,
    filter: Option<String>,
    focus: Focus,
    message: usize,
    list: ListState,
    detail_scroll: u16,
    center: Point,
    span: f64,
    needs_fit: bool,
    hierarchy: bool,
    graph_area: Rect,
    messages_area: Rect,
    detail_area: Rect,
    drag: Option<Drag>,
    search: String,
    searching: bool,
    error: Option<String>,
    help: bool,
}
fn clean(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .collect()
}
fn short(value: &str, max: usize) -> String {
    clean(value)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max)
        .collect()
}
fn wrap_detail(text: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    let mut result = Vec::new();
    for source in text.split('\n') {
        let mut line = String::new();
        let mut used = 0;
        for ch in source.chars() {
            let cells = ch.width().unwrap_or(0);
            if used + cells > width && !line.is_empty() {
                result.push(std::mem::take(&mut line));
                used = 0;
            }
            line.push(ch);
            used += cells;
        }
        result.push(line);
    }
    result
}
fn message_detail(request: &CollaborationRequest) -> String {
    use std::fmt::Write as _;
    let mut detail = format!(
        "{}\n{} -> {}\n{:?} | {:?}\n{}\n\nREQUEST\n{}",
        request.id,
        request.from.label(),
        request.to.label(),
        request.kind,
        request.status,
        request.created_at,
        clean(&request.body)
    );
    for update in &request.updates {
        let _ = write!(detail, "\n\nUPDATE\n{}", clean(&update.body));
    }
    if let Some(reply) = &request.reply {
        let _ = write!(
            detail,
            "\n\nREPLY ({:?})\n{}",
            reply.status,
            clean(&reply.body)
        );
    }
    detail
}
fn color(kind: Kind) -> Color {
    match kind {
        Kind::Session => Color::Cyan,
        Kind::Window => Color::Magenta,
        Kind::Console => Color::Blue,
        _ => Color::Green,
    }
}
impl App {
    fn new(path: PathBuf, limit: usize, page: CollaborationPage) -> Self {
        let mut app = Self {
            path,
            requests: page.requests,
            matching: Vec::new(),
            total: page.total,
            limit,
            graph: Graph::default(),
            physics: physics::Physics::default(),
            show_groups: true,
            edge_labels: false,
            inspector: true,
            fit_span: 100.,
            positions: HashMap::new(),
            selected: 0,
            filter: None,
            focus: Focus::Graph,
            message: 0,
            list: ListState::default(),
            detail_scroll: 0,
            center: Point::default(),
            span: 100.,
            needs_fit: true,
            hierarchy: true,
            graph_area: Rect::default(),
            messages_area: Rect::default(),
            detail_area: Rect::default(),
            drag: None,
            search: String::new(),
            searching: false,
            error: None,
            help: false,
        };
        app.rebuild();
        app
    }
    fn matches(&self, r: &CollaborationRequest) -> bool {
        self.search.is_empty()
            || format!(
                "{} {} {} {} {} {} {} {} {}",
                r.from.tmux_session_name.as_deref().unwrap_or(""),
                r.to.tmux_session_name.as_deref().unwrap_or(""),
                r.from.window_name.as_deref().unwrap_or(""),
                r.to.window_name.as_deref().unwrap_or(""),
                r.from.label(),
                r.to.label(),
                r.body,
                r.reply.as_ref().map_or("", |p| p.body.as_str()),
                r.id
            )
            .to_lowercase()
            .contains(&self.search.to_lowercase())
    }
    fn rebuild(&mut self) {
        let selected = self.graph.nodes.get(self.selected).map(|n| n.id.clone());
        let old_nodes: Vec<_> = self.graph.nodes.iter().map(|n| n.id.clone()).collect();
        self.matching = self
            .requests
            .iter()
            .enumerate()
            .filter(|(_, r)| self.matches(r))
            .map(|(i, _)| i)
            .collect();
        self.graph = Graph::build(
            &self
                .matching
                .iter()
                .map(|&i| &self.requests[i])
                .collect::<Vec<_>>(),
        );
        let changed = old_nodes
            != self
                .graph
                .nodes
                .iter()
                .map(|n| n.id.clone())
                .collect::<Vec<_>>();
        self.needs_fit |= changed;
        if changed {
            let running = self.physics.running;
            let pinned = self.physics.pinned.clone();
            (self.physics, self.positions) =
                physics::Physics::grouped(&self.graph, &self.positions);
            self.physics.pinned = pinned;
            if !old_nodes.is_empty() {
                self.physics.running = running;
            }
        }
        self.selected = selected
            .and_then(|id| self.graph.nodes.iter().position(|n| n.id == id))
            .unwrap_or(0);
        if self
            .filter
            .as_ref()
            .is_some_and(|id| !self.graph.nodes.iter().any(|n| &n.id == id))
        {
            self.filter = None;
        }
        self.clamp_message();
    }
    fn visible(&self) -> Vec<usize> {
        let node = self
            .filter
            .as_ref()
            .and_then(|id| self.graph.nodes.iter().position(|n| &n.id == id));
        self.matching
            .iter()
            .copied()
            .filter(|&i| node.is_none_or(|n| self.graph.touches(n, &self.requests[i])))
            .collect()
    }
    fn clamp_message(&mut self) {
        self.message = self.message.min(self.visible().len().saturating_sub(1));
        self.list.select(if self.visible().is_empty() {
            None
        } else {
            Some(self.message)
        });
    }
    fn bounds(&self) -> ([f64; 2], [f64; 2]) {
        let aspect =
            f64::from(self.graph_area.width.max(1)) / f64::from(self.graph_area.height.max(1)) / 2.;
        (
            [
                self.center.x - self.span * aspect / 2.,
                self.center.x + self.span * aspect / 2.,
            ],
            [
                self.center.y - self.span / 2.,
                self.center.y + self.span / 2.,
            ],
        )
    }
    fn fit(&mut self) {
        if self.positions.is_empty() {
            self.center = Point::default();
            self.span = 100.;
            return;
        }
        let (mut x0, mut x1, mut y0, mut y1) = (
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
        );
        for p in self.positions.values() {
            x0 = x0.min(p.x);
            x1 = x1.max(p.x);
            y0 = y0.min(p.y);
            y1 = y1.max(p.y);
        }
        if self.show_groups {
            for group in &self.physics.groups {
                x0 = x0.min(group.bounds.left);
                x1 = x1.max(group.bounds.right);
                y0 = y0.min(group.bounds.bottom);
                y1 = y1.max(group.bounds.top);
            }
        }
        self.center = Point {
            x: f64::midpoint(x0, x1),
            y: f64::midpoint(y0, y1),
        };
        let aspect =
            f64::from(self.graph_area.width.max(1)) / f64::from(self.graph_area.height.max(1)) / 2.;
        self.span = (y1 - y0 + 22.).max((x1 - x0 + 40.) / aspect).max(30.);
        self.fit_span = self.span;
        self.needs_fit = false;
    }
    fn navigate(&mut self, dx: f64, dy: f64) {
        let Some(node) = self.graph.nodes.get(self.selected) else {
            return;
        };
        let current = self.positions[&node.id];
        let next = self
            .graph
            .nodes
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != self.selected)
            .filter_map(|(i, n)| {
                let p = self.positions[&n.id];
                let x = p.x - current.x;
                let y = p.y - current.y;
                let forward = x * dx + y * dy;
                (forward > 0.01).then_some((
                    i,
                    x.hypot(y) * (1. + (x * dy - y * dx).abs() / forward.max(0.01)),
                ))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1));
        if let Some((i, _)) = next {
            self.selected = i;
            let p = self.positions[&self.graph.nodes[i].id];
            let (x, y) = self.bounds();
            if p.x < x[0] || p.x > x[1] || p.y < y[0] || p.y > y[1] {
                self.center = p;
            }
        }
    }
    fn select(&mut self) {
        self.inspector = true;
        if let Some(n) = self.graph.nodes.get(self.selected) {
            self.filter = Some(n.id.clone());
            self.message = 0;
            self.detail_scroll = 0;
            self.clamp_message();
            self.focus = Focus::Messages;
        }
    }
    fn world(&self, column: u16, row: u16) -> Point {
        let (x, y) = self.bounds();
        Point {
            x: x[0]
                + f64::from(column.saturating_sub(self.graph_area.x))
                    / f64::from(self.graph_area.width.max(1))
                    * (x[1] - x[0]),
            y: y[1]
                - f64::from(row.saturating_sub(self.graph_area.y))
                    / f64::from(self.graph_area.height.max(1))
                    * (y[1] - y[0]),
        }
    }
    fn hit(&self, column: u16, row: u16) -> Option<usize> {
        let p = self.world(column, row);
        let (x, y) = self.bounds();
        self.graph
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let a = self.positions[&n.id];
                let dx = (p.x - a.x) / (x[1] - x[0]) * f64::from(self.graph_area.width);
                let dy = (p.y - a.y) / (y[1] - y[0]) * f64::from(self.graph_area.height);
                (i, dx.hypot(dy))
            })
            .filter(|(_, d)| *d < 3.)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i)
    }
    fn zoom(&mut self, factor: f64) {
        self.span = (self.span * factor).clamp(8., 5000.);
    }
    fn zoom_at(&mut self, factor: f64, column: u16, row: u16) {
        let before = self.world(column, row);
        self.zoom(factor);
        let after = self.world(column, row);
        self.center.x += before.x - after.x;
        self.center.y += before.y - after.y;
    }
    #[allow(clippy::too_many_lines)] // Keep the key map together.
    fn key(&mut self, key: KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return true;
        }
        if self.searching {
            match key.code {
                KeyCode::Enter => self.searching = false,
                KeyCode::Esc => {
                    self.searching = false;
                    self.search.clear();
                    self.rebuild();
                }
                KeyCode::Backspace => {
                    self.search.pop();
                    self.rebuild();
                }
                KeyCode::Char(c) => {
                    self.search.push(c);
                    self.rebuild();
                }
                _ => {}
            }
            return false;
        }
        if self.help {
            match key.code {
                KeyCode::Char('q' | '?') | KeyCode::Esc => self.help = false,
                _ => {}
            }
            return false;
        }
        if !self.inspector && matches!(key.code, KeyCode::Tab | KeyCode::BackTab | KeyCode::Enter) {
            self.inspector = true;
            self.needs_fit = true;
        }
        match key.code {
            KeyCode::Char('q') => return true,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('/') => self.searching = true,
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Graph => Focus::Messages,
                    Focus::Messages => Focus::Detail,
                    Focus::Detail => Focus::Graph,
                }
            }
            KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Graph => Focus::Detail,
                    Focus::Messages => Focus::Graph,
                    Focus::Detail => Focus::Messages,
                }
            }
            KeyCode::Esc => {
                self.search.clear();
                self.filter = None;
                self.rebuild();
                self.focus = Focus::Graph;
                self.detail_scroll = 0;
                self.clamp_message();
            }
            KeyCode::Char('f') => self.fit(),
            KeyCode::Char('0') => {
                self.positions.clear();
                (self.physics, self.positions) =
                    physics::Physics::grouped(&self.graph, &HashMap::new());
                self.fit();
            }
            KeyCode::Char('+' | '=') => self.zoom(0.8),
            KeyCode::Char('-') => self.zoom(1.25),
            KeyCode::Char('H') => self.hierarchy = !self.hierarchy,
            KeyCode::Char(' ') => self.physics.running = !self.physics.running,
            KeyCode::Char('b') => self.physics.bounce(&self.graph),
            KeyCode::Char('p') => self.physics.toggle_pin(&self.graph, self.selected),
            KeyCode::Char('g') => self.show_groups = !self.show_groups,
            KeyCode::Char('e') => self.edge_labels = !self.edge_labels,
            KeyCode::Char('v') => {
                self.inspector = !self.inspector;
                self.focus = Focus::Graph;
            }
            KeyCode::Enter => match self.focus {
                Focus::Graph => self.select(),
                Focus::Messages => {
                    self.focus = Focus::Detail;
                    self.detail_scroll = 0;
                }
                Focus::Detail => self.focus = Focus::Graph,
            },
            KeyCode::Left if self.focus == Focus::Graph => self.navigate(-1., 0.),
            KeyCode::Right if self.focus == Focus::Graph => self.navigate(1., 0.),
            KeyCode::Up if self.focus == Focus::Graph => self.navigate(0., 1.),
            KeyCode::Down if self.focus == Focus::Graph => self.navigate(0., -1.),
            KeyCode::Up | KeyCode::Char('k') if self.focus == Focus::Messages => {
                self.message = self.message.saturating_sub(1);
                self.detail_scroll = 0;
                self.clamp_message();
            }
            KeyCode::Down | KeyCode::Char('j') if self.focus == Focus::Messages => {
                self.message = self.message.saturating_add(1);
                self.detail_scroll = 0;
                self.clamp_message();
            }
            KeyCode::Up | KeyCode::Char('k') if self.focus == Focus::Detail => {
                self.detail_scroll = self.detail_scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') if self.focus == Focus::Detail => {
                self.detail_scroll = self.detail_scroll.saturating_add(1);
            }
            KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(10),
            KeyCode::PageDown => {
                self.detail_scroll = self.detail_scroll.saturating_add(10);
                self.focus = Focus::Detail;
            }
            KeyCode::Char('h') => self.center.x -= self.span * 0.1,
            KeyCode::Char('l') => self.center.x += self.span * 0.1,
            KeyCode::Char('k') => self.center.y += self.span * 0.1,
            KeyCode::Char('j') => self.center.y -= self.span * 0.1,
            _ => {}
        }
        false
    }
    fn mouse(&mut self, event: MouseEvent) {
        let location = ratatui::layout::Position::new(event.column, event.row);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) if self.graph_area.contains(location) => {
                self.focus = Focus::Graph;
                let node = self.hit(event.column, event.row);
                if let Some(i) = node {
                    self.selected = i;
                    self.filter = Some(self.graph.nodes[i].id.clone());
                    self.message = 0;
                    self.clamp_message();
                }
                self.drag = Some(Drag {
                    node,
                    start: self.world(event.column, event.row),
                });
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(drag) = self.drag {
                    let p = self.world(event.column, event.row);
                    if let Some(i) = drag.node {
                        self.physics.drag(&self.graph, i, p, &mut self.positions);
                    } else {
                        self.center.x += drag.start.x - p.x;
                        self.center.y += drag.start.y - p.y;
                    }
                }
            }
            MouseEventKind::Up(_) => self.drag = None,
            MouseEventKind::ScrollUp if self.graph_area.contains(location) => {
                self.zoom_at(0.8, event.column, event.row);
            }
            MouseEventKind::ScrollDown if self.graph_area.contains(location) => {
                self.zoom_at(1.25, event.column, event.row);
            }
            MouseEventKind::ScrollUp if self.detail_area.contains(location) => {
                self.detail_scroll = self.detail_scroll.saturating_sub(3);
            }
            MouseEventKind::ScrollDown if self.detail_area.contains(location) => {
                self.detail_scroll = self.detail_scroll.saturating_add(3);
            }
            MouseEventKind::Down(MouseButton::Left) if self.messages_area.contains(location) => {
                self.focus = Focus::Messages;
                self.message = self.list.offset()
                    + usize::from(event.row.saturating_sub(self.messages_area.y)) / 2;
                self.detail_scroll = 0;
                self.clamp_message();
            }
            MouseEventKind::ScrollUp if self.messages_area.contains(location) => {
                self.message = self.message.saturating_sub(1);
                self.clamp_message();
            }
            MouseEventKind::ScrollDown if self.messages_area.contains(location) => {
                self.message += 1;
                self.clamp_message();
            }
            _ => {}
        }
    }
    #[allow(clippy::too_many_lines, clippy::many_single_char_names)] // Canvas geometry and panel layout.
    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        if area.width < 50 || area.height < 14 {
            frame.render_widget(
                Paragraph::new("Muxa graph: resize terminal to at least 50 x 14. q quits."),
                area,
            );
            return;
        }
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(6),
                Constraint::Length(2),
            ])
            .split(area);
        frame.render_widget(Paragraph::new(vec![
            Line::from(vec![Span::styled(" Muxa collaboration graph ",Style::default().fg(Color::Cyan)),Span::raw(format!("{} nodes | {} connections | {}/{} messages",self.graph.nodes.len(),self.graph.edges.len(),self.requests.len(),self.total))]),
            Line::from(format!(" zoom {:.0}% [+/- wheel] | physics {} [Space/b] | session/window groups {} [g] | inspector [v]",self.fit_span/self.span*100.,if self.physics.running{"live"}else{"paused"},if self.show_groups{"on"}else{"off"})),
        ]),rows[0]);
        let wide = area.width >= 95;
        let columns = if self.inspector {
            Layout::default()
                .direction(if wide {
                    Direction::Horizontal
                } else {
                    Direction::Vertical
                })
                .constraints(if wide {
                    vec![Constraint::Percentage(63), Constraint::Percentage(37)]
                } else {
                    vec![Constraint::Percentage(55), Constraint::Percentage(45)]
                })
                .split(rows[1])
                .to_vec()
        } else {
            vec![rows[1], Rect::default()]
        };
        let graph_block = Block::default()
            .borders(Borders::ALL)
            .title(if self.focus == Focus::Graph {
                " Graph [focused] "
            } else {
                " Graph "
            })
            .border_style(Style::default().fg(if self.focus == Focus::Graph {
                Color::Cyan
            } else {
                Color::DarkGray
            }));
        let graph_area = graph_block.inner(columns[0]);
        if self.graph_area != graph_area {
            self.graph_area = graph_area;
            self.needs_fit = true;
        }
        if self.needs_fit {
            self.fit();
        }
        let (x_bounds, y_bounds) = self.bounds();
        let nodes = &self.graph.nodes;
        let positions = &self.positions;
        let selected = self.selected;
        let span = self.span;
        let hierarchy = self.hierarchy;
        let graph = &self.graph;
        let placed = labels::place(
            &self.graph,
            &self.positions,
            self.selected,
            x_bounds,
            y_bounds,
            graph_area.width,
            graph_area.height,
        );
        let canvas = Canvas::default()
            .block(graph_block)
            .marker(Marker::Braille)
            .x_bounds(x_bounds)
            .y_bounds(y_bounds)
            .paint(|ctx| {
                if self.show_groups {
                    for group in &self.physics.groups {
                        if !matches!(nodes[group.owner].kind, Kind::Session | Kind::Window) {
                            continue;
                        }
                        let b = group.bounds;
                        let tint = if nodes[group.owner].kind == Kind::Session {
                            Color::DarkGray
                        } else {
                            Color::Blue
                        };
                        for (a, z) in [
                            (
                                Point {
                                    x: b.left,
                                    y: b.top,
                                },
                                Point {
                                    x: b.right,
                                    y: b.top,
                                },
                            ),
                            (
                                Point {
                                    x: b.right,
                                    y: b.top,
                                },
                                Point {
                                    x: b.right,
                                    y: b.bottom,
                                },
                            ),
                            (
                                Point {
                                    x: b.right,
                                    y: b.bottom,
                                },
                                Point {
                                    x: b.left,
                                    y: b.bottom,
                                },
                            ),
                            (
                                Point {
                                    x: b.left,
                                    y: b.bottom,
                                },
                                Point {
                                    x: b.left,
                                    y: b.top,
                                },
                            ),
                        ] {
                            ctx.draw(&CanvasLine {
                                x1: a.x,
                                y1: a.y,
                                x2: z.x,
                                y2: z.y,
                                color: tint,
                            });
                        }
                    }
                }
                if hierarchy {
                    for &(a, b) in &graph.hierarchy {
                        let a = positions[&nodes[a].id];
                        let b = positions[&nodes[b].id];
                        ctx.draw(&CanvasLine {
                            x1: a.x,
                            y1: a.y,
                            x2: b.x,
                            y2: b.y,
                            color: Color::DarkGray,
                        });
                    }
                }
                for edge in &graph.edges {
                    let a = positions[&nodes[edge.from].id];
                    let b = positions[&nodes[edge.to].id];
                    let dx = b.x - a.x;
                    let dy = b.y - a.y;
                    let d = dx.hypot(dy).max(0.1);
                    let related = nodes.get(selected).is_some_and(|node| {
                        node.members.contains(&nodes[edge.from].id)
                            || node.members.contains(&nodes[edge.to].id)
                    });
                    let edge_color = if related {
                        if edge.pending > 0 {
                            Color::Yellow
                        } else {
                            Color::Gray
                        }
                    } else {
                        Color::DarkGray
                    };
                    ctx.draw(&CanvasLine {
                        x1: a.x,
                        y1: a.y,
                        x2: b.x,
                        y2: b.y,
                        color: edge_color,
                    });
                    let t = Point {
                        x: b.x - dx / d * 3.,
                        y: b.y - dy / d * 3.,
                    };
                    let r = span * 0.025;
                    for sign in [-1., 1.] {
                        ctx.draw(&CanvasLine {
                            x1: t.x,
                            y1: t.y,
                            x2: t.x - dx / d * r + sign * dy / d * r * 0.5,
                            y2: t.y - dy / d * r - sign * dx / d * r * 0.5,
                            color: edge_color,
                        });
                    }
                    if edge.replies > 0 {
                        let offset = span * 0.015;
                        let from = Point {
                            x: b.x - dy / d * offset,
                            y: b.y + dx / d * offset,
                        };
                        let to = Point {
                            x: a.x - dy / d * offset + dx / d * 3.,
                            y: a.y + dx / d * offset + dy / d * 3.,
                        };
                        ctx.draw(&CanvasLine {
                            x1: from.x,
                            y1: from.y,
                            x2: to.x,
                            y2: to.y,
                            color: if related {
                                Color::Cyan
                            } else {
                                Color::DarkGray
                            },
                        });
                        for sign in [-1., 1.] {
                            ctx.draw(&CanvasLine {
                                x1: to.x,
                                y1: to.y,
                                x2: to.x + dx / d * r + sign * dy / d * r * 0.5,
                                y2: to.y + dy / d * r - sign * dx / d * r * 0.5,
                                color: if related {
                                    Color::Cyan
                                } else {
                                    Color::DarkGray
                                },
                            });
                        }
                    }
                    if self.edge_labels && related {
                        ctx.print(
                            f64::midpoint(a.x, b.x),
                            f64::midpoint(a.y, b.y),
                            Span::styled(
                                format!(
                                    "{} req{}",
                                    edge.count,
                                    if edge.replies > 0 {
                                        format!(" / {} rep", edge.replies)
                                    } else {
                                        String::new()
                                    }
                                ),
                                Style::default().fg(edge_color),
                            ),
                        );
                    }
                }
                ctx.layer();
                for (i, n) in nodes.iter().enumerate() {
                    let p = positions[&n.id];
                    let tint = if i == selected {
                        Color::Yellow
                    } else {
                        color(n.kind)
                    };
                    ctx.draw(&Circle {
                        x: p.x,
                        y: p.y,
                        radius: span * 0.012,
                        color: tint,
                    });
                }
                for label in &placed {
                    let x = x_bounds[0]
                        + f64::from(label.column)
                            / f64::from(graph_area.width.saturating_sub(1).max(1))
                            * (x_bounds[1] - x_bounds[0]);
                    let y = y_bounds[1]
                        - f64::from(label.row)
                            / f64::from(graph_area.height.saturating_sub(1).max(1))
                            * (y_bounds[1] - y_bounds[0]);
                    let tint = if label.node == selected {
                        Color::Yellow
                    } else {
                        color(nodes[label.node].kind)
                    };
                    ctx.print(
                        x,
                        y,
                        Span::styled(label.text.clone(), Style::default().fg(tint)),
                    );
                }
            });
        frame.render_widget(canvas, columns[0]);
        if self.graph.nodes.is_empty() {
            frame.render_widget(Paragraph::new("No collaboration messages match.\nPress / to search, Esc to clear selection.\nRecords appear here as agents exchange messages."),graph_area);
        }
        let right = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(columns[1]);
        let (message_area, detail_area) = if wide {
            (right[0], right[1])
        } else if self.focus == Focus::Detail {
            (Rect::default(), columns[1])
        } else {
            (columns[1], Rect::default())
        };
        let visible = self.visible();
        let title = self
            .filter
            .as_ref()
            .and_then(|id| self.graph.nodes.iter().find(|n| &n.id == id))
            .map_or_else(
                || " Messages: all ".into(),
                |n| format!(" {}: {} ", n.kind.label(), short(&n.label, 24)),
            );
        let message_block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(if self.focus == Focus::Messages {
                Color::Cyan
            } else {
                Color::DarkGray
            }));
        self.messages_area = message_block.inner(message_area);
        let items: Vec<_> = visible
            .iter()
            .map(|&i| {
                let r = &self.requests[i];
                ListItem::new(vec![
                    Line::from(format!(
                        "{} -> {}",
                        short(&r.from.label(), 20),
                        short(&r.to.label(), 20)
                    )),
                    Line::from(Span::styled(
                        format!("{:?} | {:?} | {}", r.kind, r.status, short(&r.body, 50)),
                        Style::default().fg(Color::Gray),
                    )),
                ])
            })
            .collect();
        self.clamp_message();
        if message_area.height > 0 {
            frame.render_stateful_widget(
                List::new(items)
                    .block(message_block)
                    .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White))
                    .highlight_symbol("> "),
                message_area,
                &mut self.list,
            );
        }
        let detail_block = Block::default()
            .borders(Borders::ALL)
            .title(" Message detail ")
            .border_style(Style::default().fg(if self.focus == Focus::Detail {
                Color::Cyan
            } else {
                Color::DarkGray
            }));
        self.detail_area = detail_block.inner(detail_area);
        let detail = if detail_area.height == 0 {
            String::new()
        } else {
            visible.get(self.message).map_or_else(
                || "Select a node and press Enter to inspect its messages.".into(),
                |&i| message_detail(&self.requests[i]),
            )
        };
        let lines = wrap_detail(&clean(&detail), usize::from(self.detail_area.width.max(1)));
        let count = lines.len();
        let paragraph = Paragraph::new(lines.into_iter().map(Line::from).collect::<Vec<_>>());
        let max_scroll = u16::try_from(count.saturating_sub(usize::from(self.detail_area.height)))
            .unwrap_or(u16::MAX);
        self.detail_scroll = self.detail_scroll.min(max_scroll);
        if detail_area.height > 0 {
            frame.render_widget(
                paragraph
                    .block(detail_block)
                    .scroll((self.detail_scroll, 0)),
                detail_area,
            );
        }
        let node = self
            .graph
            .nodes
            .get(self.selected)
            .map(|n| format!("{}: {} | {}", n.kind.label(), n.label, n.location))
            .unwrap_or_default();
        let node = if self
            .graph
            .nodes
            .get(self.selected)
            .is_some_and(|n| self.physics.pinned.contains(&n.id))
        {
            format!("[pinned; p releases] {node}")
        } else {
            node
        };
        let node = if self.search.is_empty() {
            node
        } else {
            format!("/{} | {}", short(&self.search, 30), node)
        };
        let status = if self.searching {
            format!(" /{}█  Enter: apply  Esc: clear", self.search)
        } else if let Some(error) = &self.error {
            format!(" Refresh failed: {}", short(error, 150))
        } else {
            format!(
                " {}{}",
                short(&node, 140),
                if self.requests.len() >= 5000 && self.requests.len() < self.total {
                    " | display limit: 5000"
                } else if self.requests.len() < self.total {
                    " | n: load older"
                } else {
                    ""
                }
            )
        };
        frame.render_widget(Paragraph::new(vec![Line::from(status),Line::from(" arrows select | Enter inspect | Tab panel | hjkl pan | +/- zoom | f fit | b bounce | Space pause | v expand | ? help | q quit")]),rows[2]);
        if self.help {
            let popup = Rect {
                x: area.x + area.width / 10,
                y: area.y + area.height / 8,
                width: area.width * 8 / 10,
                height: area.height * 3 / 4,
            };
            frame.render_widget(Clear, popup);
            frame.render_widget(Paragraph::new("Graph: arrow keys select the next node in that direction.\nEnter: filter messages to selected node and descendants.\nTab / Shift+Tab: cycle graph, messages, detail.\nMessages: Up/Down or j/k select; Enter focuses full detail.\nDetail: Up/Down, PageUp/PageDown scroll.\nh/j/k/l: pan graph; +/- or wheel: zoom; f: fit; 0: reset layout.\nMouse: click a node to filter; drag a node to move it;\ndrag empty canvas to pan; click messages to read.\nH: hierarchy; g: group outlines; e: edge counts; v: expand graph.\nSpace: pause/resume; b: bounce; p: pin/unpin selected node.\n0: reset grouped layout.\n/: search loaded history.\nr: refresh; n: load older messages (up to 5000 loaded).\nEsc: clear node selection; q or Ctrl+C: quit.\n\nRead-only saved collaboration history. No browser or server.\nClose help with ?, Esc, or q.").block(Block::default().borders(Borders::ALL).title(" Graph controls ")).wrap(Wrap{trim:false}),popup);
        }
    }
}
struct RestoreTerminal;
impl Drop for RestoreTerminal {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableMouseCapture,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
    }
}
#[allow(clippy::too_many_lines)] // Event loop and terminal lifetime stay together.
pub async fn run(cfg: &Config, args: Args) -> Result<()> {
    anyhow::ensure!(
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        "muxa graph needs an interactive terminal"
    );
    let path = cfg
        .collaboration
        .path
        .clone()
        .or_else(muxa::paths::default_collaboration_file)
        .context("cannot resolve collaboration history path")?;
    let limit = usize::from(args.limit);
    let initial_path = path.clone();
    let page = tokio::task::spawn_blocking(move || read_history_page(&initial_path, 0, limit))
        .await?
        .context("cannot read collaboration history")?;
    let mut app = App::new(path, limit, page);
    enable_raw_mode()?;
    let _restore = RestoreTerminal;
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;
    let mut last = Instant::now();
    let mut animation = Instant::now();
    let mut refresh: Option<
        tokio::task::JoinHandle<Result<CollaborationPage, muxa::collaboration::CollaborationError>>,
    > = None;
    loop {
        if app.drag.is_none()
            && refresh
                .as_ref()
                .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            match refresh.take().unwrap().await {
                Ok(Ok(page)) => {
                    let selected = app
                        .visible()
                        .get(app.message)
                        .map(|&i| app.requests[i].id.clone());
                    app.requests = page.requests;
                    app.total = page.total;
                    app.error = None;
                    app.rebuild();
                    if let Some(id) = selected {
                        app.message = app
                            .visible()
                            .iter()
                            .position(|&i| app.requests[i].id == id)
                            .unwrap_or(0);
                        app.clamp_message();
                    }
                }
                Ok(Err(error)) => app.error = Some(error.to_string()),
                Err(error) => app.error = Some(error.to_string()),
            }
            last = Instant::now();
        }
        if refresh.is_none() && last.elapsed() >= Duration::from_secs(5) && app.drag.is_none() {
            let path = app.path.clone();
            let limit = app.limit;
            refresh = Some(tokio::task::spawn_blocking(move || {
                read_history_page(&path, 0, limit)
            }));
        }
        if animation.elapsed() >= Duration::from_millis(32) {
            let held = app.drag.and_then(|d| d.node);
            app.physics.tick(&app.graph, &mut app.positions, held);
            animation = Instant::now();
        }
        terminal.draw(|frame| app.draw(frame))?;
        if event::poll(Duration::from_millis(32))? {
            match event::read()? {
                Event::Key(key)
                    if !app.searching
                        && matches!(key.code, KeyCode::Char('r' | 'n'))
                        && key.kind != KeyEventKind::Release =>
                {
                    if refresh.is_none() {
                        if key.code == KeyCode::Char('n') {
                            app.limit = (app.limit + 200).min(5000);
                        }
                        let path = app.path.clone();
                        let limit = app.limit;
                        refresh = Some(tokio::task::spawn_blocking(move || {
                            read_history_page(&path, 0, limit)
                        }));
                    }
                }
                Event::Key(key) => {
                    if app.key(key) {
                        break;
                    }
                }
                Event::Mouse(mouse) => app.mouse(mouse),
                Event::Resize(_, _) => app.needs_fit = true,
                _ => {}
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use muxa::collaboration::{
        CollaborationOptions, CollaborationStore, NewRequest, Participant, RoomId,
    };
    use ratatui::backend::TestBackend;
    async fn fixture() -> App {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let peer = |id: &str| {
            let mut p = Participant::console(RoomId {
                host: "tmux".into(),
                socket: Some("/test".into()),
                window_id: "@1".into(),
            });
            p.console = false;
            p.agent_session_id = id.into();
            p.pane = format!("%{id}");
            p.alias = Some(id.into());
            p.tmux_session_id = Some("$1".into());
            p.tmux_session_name = Some("test-session".into());
            p
        };
        let a = store
            .create(
                peer("a"),
                peer("b"),
                NewRequest {
                    body: format!("first request\n{}", "long detail line\n".repeat(80)),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let a = store
            .reply(
                &a.to,
                &a.id,
                muxa::collaboration::RequestStatus::Completed,
                "response detail".into(),
                Vec::new(),
                Vec::new(),
            )
            .await
            .unwrap();
        let b = store
            .create(
                peer("c"),
                peer("d"),
                NewRequest {
                    body: "second request".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        App::new(
            "/unused".into(),
            200,
            CollaborationPage {
                requests: vec![b, a],
                total: 2,
                has_more: false,
                next_cursor: None,
            },
        )
    }
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<Vec<_>>()
            .join("")
    }
    #[tokio::test]
    async fn node_selection_filters_messages_and_keyboard_opens_scrollable_details() {
        let mut app = fixture().await;
        app.selected = app
            .graph
            .nodes
            .iter()
            .position(|n| n.label.starts_with("a@"))
            .unwrap();
        assert!(!app.key(key(KeyCode::Enter)));
        assert_eq!(app.focus, Focus::Messages);
        assert_eq!(app.visible().len(), 1);
        app.key(key(KeyCode::Enter));
        assert_eq!(app.focus, Focus::Detail);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(text(&terminal).contains("REQUEST"));
        app.key(key(KeyCode::PageDown));
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(app.detail_scroll > 0);
        app.detail_scroll = u16::MAX;
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(text(&terminal).contains("REPLY"));
        app.key(key(KeyCode::Esc));
        assert_eq!(app.focus, Focus::Graph);
        assert_eq!(app.visible().len(), 2);
        app.key(key(KeyCode::Char('/')));
        assert!(!app.key(key(KeyCode::Char('q'))));
        assert!(app.searching);
        app.key(key(KeyCode::Esc));
        assert!(app.key(key(KeyCode::Char('q'))));
    }
    #[tokio::test]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // In-bounds test viewport coordinates.
    async fn pointer_selects_and_drags_nodes_while_fit_includes_retained_positions() {
        let mut app = fixture().await;
        let mut terminal = Terminal::new(TestBackend::new(140, 45)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let node = app
            .graph
            .nodes
            .iter()
            .position(|n| n.label.starts_with("a@"))
            .unwrap();
        let point = app.positions[&app.graph.nodes[node].id];
        let (x, y) = app.bounds();
        let column = app.graph_area.x
            + (((point.x - x[0]) / (x[1] - x[0])) * f64::from(app.graph_area.width)) as u16;
        let row = app.graph_area.y
            + (((y[1] - point.y) / (y[1] - y[0])) * f64::from(app.graph_area.height)) as u16;
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.selected, node);
        assert_eq!(app.visible().len(), 1);
        app.mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: column + 3,
            row,
            modifiers: KeyModifiers::NONE,
        });
        assert_ne!(app.positions[&app.graph.nodes[node].id], point);
        app.mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: column + 3,
            row,
            modifiers: KeyModifiers::NONE,
        });
        let dragged = app.positions[&app.graph.nodes[node].id];
        app.rebuild();
        assert_eq!(app.positions[&app.graph.nodes[node].id], dragged);
        app.fit();
        let (x, y) = app.bounds();
        assert!(dragged.x > x[0] && dragged.x < x[1] && dragged.y > y[0] && dragged.y < y[1]);
    }
    #[tokio::test]
    async fn directional_navigation_zoom_pan_and_hierarchy_controls() {
        let mut app = fixture().await;
        app.graph_area = Rect::new(1, 3, 80, 30);
        for (i, node) in app.graph.nodes.iter().enumerate() {
            app.positions.insert(
                node.id.clone(),
                Point {
                    x: -1000.,
                    y: -1000. - f64::from(u32::try_from(i).unwrap()) * 20.,
                },
            );
        }
        for (i, point) in [
            (0, Point { x: 0., y: 0. }),
            (1, Point { x: 20., y: 0. }),
            (2, Point { x: 20., y: 20. }),
        ] {
            app.positions.insert(app.graph.nodes[i].id.clone(), point);
        }
        app.selected = 0;
        app.key(key(KeyCode::Right));
        assert_eq!(app.selected, 1);
        app.key(key(KeyCode::Up));
        assert_eq!(app.selected, 2);
        let before = app.span;
        app.key(key(KeyCode::Char('+')));
        assert!(app.span < before);
        let center = app.center;
        app.key(key(KeyCode::Char('h')));
        assert!(app.center.x < center.x);
        app.key(key(KeyCode::Char('H')));
        assert!(!app.hierarchy);
        app.key(key(KeyCode::Char(' ')));
        assert!(!app.physics.running);
        app.key(key(KeyCode::Char(' ')));
        assert!(app.physics.running);
        app.key(key(KeyCode::Char('g')));
        assert!(!app.show_groups);
        app.key(key(KeyCode::Char('e')));
        assert!(app.edge_labels);
        app.key(key(KeyCode::Char('v')));
        assert!(!app.inspector);
        let before = app.world(25, 15);
        app.zoom_at(0.8, 25, 15);
        let after = app.world(25, 15);
        assert!((before.x - after.x).abs() < 1e-8 && (before.y - after.y).abs() < 1e-8);

        app.key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Messages);
        app.key(key(KeyCode::BackTab));
        assert_eq!(app.focus, Focus::Graph);
        let lines = wrap_detail("한글텍스트abc", 4);
        assert_eq!(lines.concat(), "한글텍스트abc");
        assert!(lines
            .iter()
            .all(|line| unicode_width::UnicodeWidthStr::width(line.as_str()) <= 4));
    }

    #[tokio::test]
    async fn spring_motion_changes_terminal_cells_and_pause_really_stops_it() {
        let mut app = fixture().await;
        let mut terminal = Terminal::new(TestBackend::new(140, 45)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let initial = text(&terminal);
        for _ in 0..20 {
            app.physics.tick(&app.graph, &mut app.positions, None);
        }
        terminal.draw(|f| app.draw(f)).unwrap();
        assert_ne!(
            text(&terminal),
            initial,
            "startup movement must be visible in actual terminal cells"
        );
        for _ in 0..150 {
            app.physics.tick(&app.graph, &mut app.positions, None);
        }
        terminal.draw(|f| app.draw(f)).unwrap();
        let settled = text(&terminal);
        app.key(key(KeyCode::Char('b')));
        for _ in 0..6 {
            app.physics.tick(&app.graph, &mut app.positions, None);
        }
        terminal.draw(|f| app.draw(f)).unwrap();
        assert_ne!(
            text(&terminal),
            settled,
            "b should visibly bounce a settled graph"
        );
        app.key(key(KeyCode::Char(' ')));
        let paused = app.positions.clone();
        for _ in 0..20 {
            app.physics.tick(&app.graph, &mut app.positions, None);
        }
        assert_eq!(app.positions, paused);
    }

    #[tokio::test]
    async fn resize_empty_history_and_search_render_without_a_browser_or_server() {
        let mut app = fixture().await;
        for (width, height) in [(140, 45), (80, 24), (50, 14), (30, 10)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
            assert!(text(&terminal).contains("graph"));
        }
        app.search = "no-such-record".into();
        app.rebuild();
        assert!(app.graph.nodes.is_empty());
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(text(&terminal).contains("No collaboration messages match"));
        assert_eq!(clean("safe\u{1b}[31m\u{7}\ntext"), "safe[31m\ntext");
    }
}
