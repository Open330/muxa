//! Separate peer panels and spatial links; no prompt content or additional IPC.
use std::collections::{BTreeMap, VecDeque};

use muxa::collaboration::CollaborationEndpoint;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use super::{
    cell_rect, collaboration_lines, display_width, identity, info_rect, pane_address, PeekCell,
    Placement,
};

struct Panel {
    index: usize,
    rect: Rect,
}

fn panel(cell: &PeekCell, pane: Rect, bottom: u16) -> Option<Rect> {
    if cell.agent.is_none() || cell.collaboration.purpose.is_none() || pane.width < 12 {
        return None;
    }
    let prompt = info_rect(cell, pane);
    let y = prompt.bottom().saturating_add(1);
    let available = pane.bottom().min(bottom).saturating_sub(y);
    if available < 3 {
        return None;
    }
    let rows = u16::try_from(collaboration_lines(cell).len()).unwrap_or(4);
    Some(Rect {
        y,
        height: available.min(rows + 2),
        ..prompt
    })
}

pub(super) fn draw(f: &mut Frame, cells: &[PeekCell], placement: Placement, zoomed: bool) {
    let mut canvas = f.area();
    if canvas.height == 0 {
        return;
    }
    canvas.height -= 1;
    if placement.hint_at_top {
        canvas.y += 1;
    }
    let panels: Vec<_> = cells
        .iter()
        .enumerate()
        .filter_map(|(index, cell)| {
            if zoomed && !cell.geo.active {
                return None;
            }
            let pane = cell_rect(&cell.geo, placement.origin_y, f.area())?;
            let rect = panel(cell, pane, canvas.bottom())?;
            Some(Panel { index, rect })
        })
        .collect();
    for panel in &panels {
        let cell = &cells[panel.index];
        let title = format!(" Links · {} ", pane_address(cell));
        let title = if display_width(&title) <= usize::from(panel.rect.width.saturating_sub(2)) {
            title
        } else {
            " Links ".into()
        };
        let block = Block::bordered()
            .title(identity::address_line(
                title,
                pane_address(cell),
                cell.agent
                    .as_ref()
                    .and_then(|agent| agent.tmux_socket.as_deref()),
            ))
            .border_style(Style::default().fg(Color::Yellow));
        let inner = block.inner(panel.rect);
        f.render_widget(Clear, panel.rect);
        f.render_widget(block, panel.rect);
        let lines: Vec<_> = styled_lines(cell)
            .into_iter()
            .take(usize::from(inner.height))
            .collect();
        f.render_widget(
            Paragraph::new(lines).style(Style::default().fg(Color::Yellow)),
            inner,
        );
    }
    draw_links(f, cells, &panels, canvas, placement, zoomed);
}

fn styled_lines(cell: &PeekCell) -> Vec<Line<'static>> {
    let mut lines = collaboration_lines(cell).into_iter();
    let mut styled = Vec::new();
    if cell.collaboration.purpose.is_some() {
        styled.extend(lines.next().map(Line::raw));
    }
    if let Some(spawn) = &cell.geo.spawn {
        styled.extend(lines.next().map(|text| {
            identity::address_line(text, &spawn.creator.pane, spawn.creator.socket.as_deref())
        }));
    }
    for (index, text) in lines.enumerate() {
        let endpoint = cell.collaboration.connections.get(index).map(|link| {
            if cell.agent.as_ref().is_some_and(|agent| {
                agent.session_id == link.from.agent_session_id
                    && agent.tmux_socket == link.from.socket
            }) {
                &link.to
            } else {
                &link.from
            }
        });
        styled.push(match endpoint {
            Some(endpoint) => {
                identity::address_line(text, &endpoint.pane, endpoint.socket.as_deref())
            }
            None => Line::raw(text),
        });
    }
    styled
}

fn matches(cell: &PeekCell, endpoint: &CollaborationEndpoint) -> bool {
    let Some(agent) = &cell.agent else {
        return false;
    };
    let same_socket = match (agent.tmux_socket.as_deref(), endpoint.socket.as_deref()) {
        (Some(left), Some(right)) => {
            muxa::backend::pane_endpoints_match(agent.pane.as_deref(), left, right)
        }
        (None, _) => true,
        (Some(_), None) => false,
    };
    same_socket
        && agent.session_id == endpoint.agent_session_id
        && agent
            .pane
            .as_deref()
            .and_then(muxa::backend::pane_id_host_kind)
            == muxa::backend::pane_id_host_kind(&endpoint.pane)
}

fn endpoint_panel(
    cells: &[PeekCell],
    panels: &[Panel],
    endpoint: &CollaborationEndpoint,
) -> Option<usize> {
    let mut matches = panels
        .iter()
        .enumerate()
        .filter(|(_, panel)| matches(&cells[panel.index], endpoint));
    let first = matches.next()?.0;
    matches.next().is_none().then_some(first)
}

fn edges(cells: &[PeekCell], panels: &[Panel]) -> BTreeMap<(usize, usize), (bool, bool)> {
    let mut edges = BTreeMap::new();
    for cell in cells {
        for link in &cell.collaboration.connections {
            let Some(from) = endpoint_panel(cells, panels, &link.from) else {
                continue;
            };
            let Some(to) = endpoint_panel(cells, panels, &link.to) else {
                continue;
            };
            if from == to {
                continue;
            }
            let key = (from.min(to), from.max(to));
            let directions = edges.entry(key).or_insert((false, false));
            directions.0 |= from < to || link.reply_received;
            directions.1 |= from > to || link.reply_received;
        }
    }
    edges
}

/// Keep the connection node on the pane's horizontal centerline. Prefer its
/// geometric center, then the nearest free row, without covering any text.
fn node_position(pane: Rect, area: Rect, blocked: &[bool]) -> Option<Position> {
    let visible = pane.intersection(area);
    let x = pane.x.checked_add(pane.width / 2)?;
    let center_y = pane.y.checked_add(pane.height / 2)?;
    (visible.y..visible.bottom())
        .map(|y| Position::new(x, y))
        .filter(|point| offset(area, *point).is_some_and(|index| !blocked[index]))
        .min_by_key(|point| point.y.abs_diff(center_y))
}

fn offset(area: Rect, p: Position) -> Option<usize> {
    area.contains(p)
        .then(|| usize::from(p.y - area.y) * usize::from(area.width) + usize::from(p.x - area.x))
}

fn position(area: Rect, offset: usize) -> Position {
    Position::new(
        area.x + u16::try_from(offset % usize::from(area.width)).unwrap_or(0),
        area.y + u16::try_from(offset / usize::from(area.width)).unwrap_or(0),
    )
}

/// A bounded viewport search avoids card interiors, other titles and the hint bar.
fn route(area: Rect, blocked: &[bool], from: Position, to: Position) -> Option<Vec<Position>> {
    let (start, goal) = (offset(area, from)?, offset(area, to)?);
    let mut previous = vec![usize::MAX; blocked.len()];
    previous[start] = start;
    let mut queue = VecDeque::from([start]);
    while let Some(current) = queue.pop_front() {
        if current == goal {
            break;
        }
        let p = position(area, current);
        for (dx, dy) in [(1, 0), (0, 1), (-1, 0), (0, -1)] {
            let (Some(x), Some(y)) = (p.x.checked_add_signed(dx), p.y.checked_add_signed(dy))
            else {
                continue;
            };
            let Some(next) = offset(area, Position::new(x, y)) else {
                continue;
            };
            if previous[next] != usize::MAX || (blocked[next] && next != goal) {
                continue;
            }
            previous[next] = current;
            queue.push_back(next);
        }
    }
    if previous[goal] == usize::MAX {
        return None;
    }
    let mut path = vec![to];
    let mut current = goal;
    while current != start {
        current = previous[current];
        path.push(position(area, current));
    }
    path.reverse();
    Some(path)
}

fn arrow(from: Position, to: Position) -> &'static str {
    if to.x > from.x {
        "→"
    } else if to.x < from.x {
        "←"
    } else if to.y > from.y {
        "↓"
    } else {
        "↑"
    }
}

fn direction(from: Position, to: Position) -> u8 {
    if to.x < from.x {
        1
    } else if to.x > from.x {
        2
    } else if to.y < from.y {
        4
    } else {
        8
    }
}

fn stroke(mask: u8) -> &'static str {
    match mask {
        1..=3 => "─",
        4 | 8 | 12 => "│",
        10 => "┌",
        9 => "┐",
        6 => "└",
        5 => "┘",
        11 => "┬",
        7 => "┴",
        14 => "├",
        13 => "┤",
        15 => "┼",
        _ => "·",
    }
}

fn record_path(
    path: &[Position],
    forward: bool,
    backward: bool,
    strokes: &mut BTreeMap<(u16, u16), u8>,
    heads: &mut BTreeMap<(u16, u16), &'static str>,
) {
    for pair in path.windows(2) {
        *strokes.entry((pair[0].x, pair[0].y)).or_default() |= direction(pair[0], pair[1]);
        *strokes.entry((pair[1].x, pair[1].y)).or_default() |= direction(pair[1], pair[0]);
    }
    let last = path.len() - 1;
    for (point, glyph) in [
        (
            path[0],
            if backward {
                arrow(path[1], path[0])
            } else {
                "○"
            },
        ),
        (
            path[last],
            if forward {
                arrow(path[last - 1], path[last])
            } else {
                "○"
            },
        ),
    ] {
        heads
            .entry((point.x, point.y))
            .and_modify(|old| {
                if glyph != "○" {
                    *old = glyph;
                }
            })
            .or_insert(glyph);
    }
}

fn paint(f: &mut Frame, point: (u16, u16), glyph: &str) {
    if let Some(cell) = f.buffer_mut().cell_mut(point) {
        cell.set_symbol(glyph)
            .set_fg(Color::Cyan)
            .set_style(Style::default().add_modifier(Modifier::BOLD));
    }
}

fn draw_links(
    f: &mut Frame,
    cells: &[PeekCell],
    panels: &[Panel],
    area: Rect,
    placement: Placement,
    zoomed: bool,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let mut blocked = vec![false; usize::from(area.width) * usize::from(area.height)];
    let mut obstacles: Vec<_> = panels.iter().map(|panel| panel.rect).collect();
    obstacles.extend(cells.iter().filter_map(|cell| {
        if zoomed && !cell.geo.active {
            return None;
        }
        let pane = cell_rect(&cell.geo, placement.origin_y, f.area())?;
        Some(if cell.agent.is_some() {
            info_rect(cell, pane)
        } else {
            Rect { height: 1, ..pane }
        })
    }));
    for rect in obstacles {
        let rect = rect.intersection(area);
        for y in rect.y..rect.bottom() {
            for x in rect.x..rect.right() {
                if let Some(index) = offset(area, Position::new(x, y)) {
                    blocked[index] = true;
                }
            }
        }
    }
    let nodes: Vec<_> = panels
        .iter()
        .map(|panel| {
            let pane = cell_rect(&cells[panel.index].geo, placement.origin_y, f.area())?;
            node_position(pane, area, &blocked)
        })
        .collect();
    // Routes may end on their own nodes, but must not pass through a third
    // pane's node and imply a relationship that does not exist.
    for node in nodes.iter().flatten() {
        if let Some(index) = offset(area, *node) {
            blocked[index] = true;
        }
    }
    // Creation is pane history; only show edges touching the active pane and
    // only while the original creator session is still visible on its server.
    // Message strokes are painted afterwards and take precedence at crossings.
    for child in panels {
        let Some(spawn) = &cells[child.index].geo.spawn else {
            continue;
        };
        let Some(parent) = endpoint_panel(cells, panels, &spawn.creator) else {
            continue;
        };
        let Some(child_panel) = panels.iter().position(|panel| panel.index == child.index) else {
            continue;
        };
        let (Some(start), Some(end)) = (nodes[parent], nodes[child_panel]) else {
            continue;
        };
        let parent = &panels[parent];
        if parent.index == child.index
            || !(cells[parent.index].geo.active || cells[child.index].geo.active)
        {
            continue;
        }
        let Some(path) = route(area, &blocked, start, end) else {
            continue;
        };
        for (i, point) in path.iter().enumerate() {
            let glyph = if i + 1 == path.len() && i > 0 {
                arrow(path[i - 1], *point)
            } else if i == 0 {
                "◇"
            } else if path[i - 1].x == point.x {
                "┆"
            } else {
                "┄"
            };
            if let Some(cell) = f.buffer_mut().cell_mut((point.x, point.y)) {
                cell.set_symbol(glyph).set_fg(Color::DarkGray);
            }
        }
    }
    let mut strokes = BTreeMap::new();
    let mut heads = BTreeMap::new();
    for ((from, to), (forward, backward)) in edges(cells, panels).into_iter().take(8) {
        let (Some(start), Some(end)) = (nodes[from], nodes[to]) else {
            continue;
        };
        let Some(path) = route(area, &blocked, start, end) else {
            continue;
        };
        if path.len() >= 2 {
            record_path(&path, forward, backward, &mut strokes, &mut heads);
        }
    }
    for (point, mask) in strokes {
        paint(f, point, stroke(mask));
    }
    // Paint arrowheads last, including at ports shared by several requests.
    for (point, glyph) in heads {
        paint(f, point, glyph);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_color_matches_headers_spawn_history_and_both_message_directions() {
        let mut cells = crate::peek::tests::linked_layout(false);
        let creator = cells[0].collaboration.connections[0].from.clone();
        let child = cells[0].collaboration.connections[0].to.clone();
        cells[1].geo.spawn = Some(muxa::tmux::layout::SpawnProvenance {
            launch_id: "test-colors".into(),
            created_at: "2026-10-22".into(),
            creator: creator.clone(),
            child_pane: child.pane.clone(),
            child_socket: child.socket.clone(),
        });
        for cell in &cells {
            let address = pane_address(cell);
            let header = crate::peek::header_spans(cell, 120);
            let id = header
                .iter()
                .find(|span| span.content.trim() == address)
                .unwrap();
            assert_eq!(id.style.fg, Some(identity::cell_color(cell)));
        }
        // Parent outbound row names the child; child history and inbound row
        // name the parent. Every occurrence must use the endpoint's own color.
        for (cell, endpoint, expected_count) in [(&cells[0], &child, 1), (&cells[1], &creator, 3)] {
            let count = styled_lines(cell)
                .iter()
                .flat_map(|line| line.spans.iter())
                .filter(|span| span.content == endpoint.pane)
                .inspect(|span| {
                    assert_eq!(
                        span.style.fg,
                        Some(identity::pane_color(
                            &endpoint.pane,
                            endpoint.socket.as_deref()
                        ))
                    );
                })
                .count();
            assert_eq!(count, expected_count);
        }
    }

    #[test]
    fn routing_avoids_every_card_cell_and_fails_closed_without_a_path() {
        let area = Rect::new(3, 4, 12, 8);
        let from = Position::new(3, 8);
        let to = Position::new(14, 8);
        let mut blocked = vec![false; 96];
        for y in 5..10 {
            for x in 7..11 {
                blocked[offset(area, Position::new(x, y)).unwrap()] = true;
            }
        }
        let path = route(area, &blocked, from, to).unwrap();
        assert_eq!(path.first(), Some(&from));
        assert_eq!(path.last(), Some(&to));
        assert!(path
            .iter()
            .all(|point| !blocked[offset(area, *point).unwrap()]));
        for pair in path.windows(2) {
            assert_eq!(
                pair[0].x.abs_diff(pair[1].x) + pair[0].y.abs_diff(pair[1].y),
                1
            );
        }
        for y in area.y..area.bottom() {
            blocked[offset(area, Position::new(8, y)).unwrap()] = true;
        }
        assert!(route(area, &blocked, from, to).is_none());
        assert!(route(area, &blocked, Position::new(0, 0), to).is_none());
    }

    #[test]
    fn nodes_prefer_pane_centers_and_move_only_vertically_to_avoid_text() {
        let area = Rect::new(0, 0, 120, 48);
        let pane = Rect::new(60, 0, 60, 48);
        let mut blocked = vec![false; 120 * 48];
        assert_eq!(
            node_position(pane, area, &blocked),
            Some(Position::new(90, 24))
        );
        for y in 22..27 {
            blocked[offset(area, Position::new(90, y)).unwrap()] = true;
        }
        assert_eq!(
            node_position(pane, area, &blocked),
            Some(Position::new(90, 21))
        );
        for y in 0..48 {
            blocked[offset(area, Position::new(90, y)).unwrap()] = true;
        }
        assert_eq!(node_position(pane, area, &blocked), None);
        assert_eq!(node_position(pane, Rect::new(0, 0, 1, 1), &blocked), None);
    }
}
