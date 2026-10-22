//! Label placement in terminal cells, independent of world-space physics.
use super::model::{Graph, Kind, Point};
use std::collections::HashMap;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
#[derive(Debug)]
pub(super) struct Label {
    pub node: usize,
    pub text: String,
    pub column: i32,
    pub row: i32,
    pub width: i32,
}
fn clipped(value: &str, width: usize) -> String {
    let mut result = String::new();
    let mut used = 0;
    for ch in value.chars().filter(|c| !c.is_control()) {
        let cells = ch.width().unwrap_or(0);
        if used + cells > width {
            break;
        }
        result.push(ch);
        used += cells;
    }
    result
}
#[allow(clippy::cast_possible_truncation, clippy::many_single_char_names)] // World-to-cell coordinates are bounded by the viewport.
pub(super) fn place(
    graph: &Graph,
    points: &HashMap<String, Point>,
    selected: usize,
    x: [f64; 2],
    y: [f64; 2],
    width: u16,
    height: u16,
) -> Vec<Label> {
    if width < 4 || height < 2 {
        return Vec::new();
    }
    let project = |p: Point| {
        (
            ((p.x - x[0]) / (x[1] - x[0]) * f64::from(width - 1)).round() as i32,
            ((y[1] - p.y) / (y[1] - y[0]) * f64::from(height - 1)).round() as i32,
        )
    };
    let mut visible: Vec<_> = graph
        .nodes
        .iter()
        .enumerate()
        .filter_map(|(i, n)| {
            let p = points[&n.id];
            if p.x < x[0] || p.x > x[1] || p.y < y[0] || p.y > y[1] {
                None
            } else {
                let (c, r) = project(p);
                Some((i, c, r))
            }
        })
        .collect();
    let dense = visible.len() > usize::from(width) * usize::from(height) / 75;
    visible.sort_by_key(|&(i, _, _)| {
        (
            if i == selected {
                0
            } else {
                match graph.nodes[i].kind {
                    Kind::Session => 1,
                    Kind::Window => 2,
                    _ => 3,
                }
            },
            i,
        )
    });
    let mut labels = Vec::<Label>::new();
    for &(i, c, r) in &visible {
        let node = &graph.nodes[i];
        if dense && i != selected && !matches!(node.kind, Kind::Session | Kind::Window) {
            continue;
        }
        let prefix = node.kind.label().chars().next().unwrap_or('p');
        let text = clipped(
            &format!(
                "{}{}: {}{}",
                if i == selected { "[" } else { "" },
                prefix,
                node.label,
                if i == selected { "]" } else { "" }
            ),
            usize::from((width / 2).clamp(8, 24)),
        );
        let cells = i32::try_from(text.width()).unwrap_or(i32::MAX);
        let candidates = [
            (c - cells / 2, r + 1),
            (c - cells / 2, r - 1),
            (c + 2, r),
            (c - cells - 2, r),
            (c - cells / 2, r + 2),
        ];
        for (left, row) in candidates {
            let left = left.clamp(0, (i32::from(width) - cells).max(0));
            if row < 0 || row >= i32::from(height) {
                continue;
            }
            if labels.iter().any(|l| {
                l.row == row && left < l.column + l.width + 1 && left + cells + 1 > l.column
            }) {
                continue;
            }
            if visible.iter().any(|&(other, nc, nr)| {
                other != i && nr == row && nc >= left - 1 && nc <= left + cells
            }) {
                continue;
            }
            labels.push(Label {
                node: i,
                text,
                column: left,
                row,
                width: cells,
            });
            break;
        }
    }
    labels
}

#[cfg(test)]
mod tests {
    use super::super::model::Node;
    use super::*;
    use std::collections::BTreeSet;
    #[test]
    fn dense_unicode_labels_never_overwrite_each_other_and_selected_is_kept() {
        let nodes = (0..12)
            .map(|i| Node {
                id: i.to_string(),
                kind: Kind::Pane,
                label: format!("한글에이전트-{i}"),
                location: String::new(),
                members: BTreeSet::new(),
            })
            .collect();
        let graph = Graph {
            nodes,
            ..Default::default()
        };
        let points = graph
            .nodes
            .iter()
            .map(|n| (n.id.clone(), Point { x: 50., y: 50. }))
            .collect();
        let labels = place(&graph, &points, 3, [0., 100.], [0., 100.], 80, 24);
        assert!(labels.iter().any(|l| l.node == 3));
        for a in &labels {
            assert!(a.column >= 0 && a.column + a.width <= 80);
            for b in &labels {
                if a.node != b.node && a.row == b.row {
                    assert!(a.column + a.width < b.column || b.column + b.width < a.column);
                }
            }
        }
    }
    #[test]
    fn zooming_in_reveals_labels_that_are_suppressed_in_a_dense_overview() {
        let nodes = (0..100)
            .map(|i| Node {
                id: i.to_string(),
                kind: Kind::Pane,
                label: format!("agent-{i}"),
                location: String::new(),
                members: BTreeSet::new(),
            })
            .collect();
        let graph = Graph {
            nodes,
            ..Default::default()
        };
        let points = graph
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| {
                (
                    n.id.clone(),
                    Point {
                        x: f64::from(u32::try_from(i % 10).unwrap()) * 38.,
                        y: f64::from(u32::try_from(i / 10).unwrap()) * 18.,
                    },
                )
            })
            .collect();
        let overview = place(&graph, &points, 0, [0., 400.], [0., 200.], 80, 30);
        let detail = place(&graph, &points, 0, [0., 100.], [0., 60.], 80, 30);
        assert_eq!(overview.len(), 1);
        assert!(detail.len() > overview.len());
    }
}
