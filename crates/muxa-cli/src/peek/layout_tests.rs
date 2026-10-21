//! Geometry regression coverage for the real overlay renderer.
use muxa::collaboration::CollaborationEndpoint;
use ratatui::backend::TestBackend;
use ratatui::layout::{Position, Rect};
use ratatui::style::Color;
use ratatui::Terminal;

use super::{cell_rect, draw, info_rect, render_cell, tests::linked_layout, PeekCell, Placement};

fn layouts() -> Vec<(&'static str, Vec<Rect>)> {
    vec![
        (
            "grid",
            vec![
                Rect::new(0, 0, 59, 23),
                Rect::new(60, 0, 60, 23),
                Rect::new(0, 24, 59, 23),
                Rect::new(60, 24, 60, 23),
            ],
        ),
        (
            "main-left",
            vec![
                Rect::new(0, 0, 79, 47),
                Rect::new(80, 0, 40, 23),
                Rect::new(80, 24, 40, 23),
            ],
        ),
        (
            "main-top",
            vec![
                Rect::new(0, 0, 120, 23),
                Rect::new(0, 24, 39, 23),
                Rect::new(40, 24, 39, 23),
                Rect::new(80, 24, 40, 23),
            ],
        ),
        (
            "unequal-T",
            vec![
                Rect::new(0, 0, 38, 47),
                Rect::new(39, 0, 81, 16),
                Rect::new(39, 17, 40, 30),
                Rect::new(80, 17, 40, 30),
            ],
        ),
        (
            "nine-panes",
            (0..9)
                .map(|i| Rect::new((i % 3) * 40, (i / 3) * 16, 39, 15))
                .collect(),
        ),
        (
            "six-columns",
            (0..6).map(|i| Rect::new(i * 20, 0, 19, 47)).collect(),
        ),
    ]
}

fn cells_for(rects: &[Rect]) -> Vec<PeekCell> {
    let template = linked_layout(false)[0].clone();
    let mut cells: Vec<_> = rects
        .iter()
        .enumerate()
        .map(|(i, rect)| {
            let mut cell = template.clone();
            let pane = format!("rmux:%{}", 1000 + i);
            cell.geo.pane_id = format!("%{}", 1000 + i);
            cell.geo.pane_index = i.to_string();
            cell.geo.alias = Some(format!("검토담당-길이가긴별칭-{i}"));
            cell.geo.left = rect.x;
            cell.geo.top = rect.y;
            cell.geo.width = rect.width;
            cell.geo.height = rect.height;
            cell.geo.active = i == 0;
            let agent = cell.agent.as_mut().unwrap();
            agent.pane = Some(pane);
            agent.session_id = format!("layout-session-{i}");
            agent.last_prompt = Some(
                "다국어 요청과 매우긴경로/가나다라마바사아자차카타파하를 함께 확인합니다".into(),
            );
            cell.collaboration.connections.clear();
            cell.collaboration.links = vec!["to reviewer · Review · claimed".into()];
            cell
        })
        .collect();
    let endpoint = |cell: &PeekCell| {
        let agent = cell.agent.as_ref().unwrap();
        CollaborationEndpoint {
            agent_session_id: agent.session_id.clone(),
            pane: agent.pane.clone().unwrap(),
            socket: agent.tmux_socket.clone(),
            label: cell.geo.alias.clone().unwrap(),
        }
    };
    let endpoints: Vec<_> = cells.iter().map(endpoint).collect();
    for (i, cell) in cells.iter_mut().enumerate() {
        let mut link = template.collaboration.connections[0].clone();
        link.request_id = format!("layout-link-{i}");
        link.from = endpoints[i].clone();
        link.to = endpoints[(i + 1) % endpoints.len()].clone();
        link.reply_received = i % 2 == 0;
        cell.collaboration.connections.push(link);
    }
    cells
}

fn verify_frame(cells: &[PeekCell], size: (u16, u16), placement: Placement, zoomed: bool) -> usize {
    let area = Rect::new(0, 0, size.0, size.1);
    let visible: Vec<_> = cells
        .iter()
        .filter(|cell| !zoomed || cell.geo.active)
        .collect();
    let mut baseline = Terminal::new(TestBackend::new(size.0, size.1)).unwrap();
    baseline
        .draw(|frame| {
            for cell in &visible {
                if let Some(rect) = cell_rect(&cell.geo, placement.origin_y, area) {
                    render_cell(frame, cell, rect, false);
                }
            }
        })
        .unwrap();
    let mut terminal = Terminal::new(TestBackend::new(size.0, size.1)).unwrap();
    terminal
        .draw(|frame| draw(frame, cells, placement, zoomed, ""))
        .unwrap();
    let hint_y = if placement.hint_at_top {
        0
    } else {
        size.1.saturating_sub(1)
    };
    // Compare every prompt pixel/style, including wide characters, with the
    // same cards rendered without any collaboration panels or connecting lines.
    for cell in visible {
        let Some(rect) = cell_rect(&cell.geo, placement.origin_y, area) else {
            continue;
        };
        let rect = info_rect(cell, rect);
        for y in rect.y..rect.bottom() {
            if y == hint_y {
                continue;
            }
            for x in rect.x..rect.right() {
                let p = Position::new(x, y);
                assert_eq!(terminal.backend().buffer().cell(p),baseline.backend().buffer().cell(p),
                    "prompt overwritten at {p:?}, size={size:?}, placement={placement:?}, zoomed={zoomed}");
            }
        }
    }
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .filter(|cell| cell.fg == Color::Cyan && ["→", "←", "↑", "↓"].contains(&cell.symbol()))
        .count()
}

#[test]
fn mixed_layouts_preserve_prompts_across_viewports_status_positions_and_pane_order() {
    let mut frames = 0;
    for (name, rects) in layouts() {
        for size in [(120, 48), (87, 31), (40, 12), (12, 4), (1, 1)] {
            for top in [false, true] {
                for reverse in [false, true] {
                    let mut cells = cells_for(&rects);
                    if reverse {
                        cells.reverse();
                    }
                    let placement = Placement {
                        origin_y: u16::from(top),
                        hint_at_top: top,
                    };
                    let arrows = verify_frame(&cells, size, placement, false);
                    if size == (120, 48) {
                        assert!(
                            arrows > 0,
                            "no connections for {name}, reverse={reverse}, top={top}"
                        );
                    }
                    if size.0 < 12 || size.1 < 5 {
                        assert_eq!(arrows, 0, "crowded viewport should omit connections");
                    }
                    frames += 1;
                }
            }
        }
    }
    assert_eq!(frames, 120);
}

#[test]
fn mixed_layouts_hide_connections_to_covered_panes_when_zoomed() {
    for (_, rects) in layouts() {
        for top in [false, true] {
            let mut cells = cells_for(&rects);
            let active = cells.iter_mut().find(|cell| cell.geo.active).unwrap();
            active.geo.left = 0;
            active.geo.top = 0;
            active.geo.width = 120;
            active.geo.height = 47;
            assert_eq!(
                verify_frame(
                    &cells,
                    (120, 48),
                    Placement {
                        origin_y: u16::from(top),
                        hint_at_top: top
                    },
                    true
                ),
                0
            );
        }
    }
}
