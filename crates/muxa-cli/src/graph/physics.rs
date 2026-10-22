//! Grouped initial placement and a damped, local force simulation.
use super::model::{Graph, Kind, Point};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy)]
pub(super) struct Bounds {
    pub left: f64,
    pub right: f64,
    pub top: f64,
    pub bottom: f64,
}
#[derive(Debug, Clone)]
pub(super) struct Group {
    pub owner: usize,
    pub members: Vec<usize>,
    pub bounds: Bounds,
}
#[derive(Default)]
pub(super) struct Physics {
    pub groups: Vec<Group>,
    anchors: HashMap<String, Point>,
    velocity: HashMap<String, Point>,
    pub pinned: HashSet<String>,
    pub running: bool,
}
fn number(n: usize) -> f64 {
    f64::from(u32::try_from(n).unwrap_or(u32::MAX))
}
fn columns(n: usize) -> usize {
    let mut cols = 1;
    while cols * cols < n {
        cols += 1;
    }
    cols
}
fn descendants(root: usize, children: &[Vec<usize>]) -> Vec<usize> {
    let mut result = vec![root];
    let mut offset = 0;
    while offset < result.len() {
        result.extend(children[result[offset]].iter().copied());
        offset += 1;
    }
    result
}

struct Cluster {
    width: f64,
    height: f64,
    points: Vec<(usize, Point)>,
    groups: Vec<Group>,
}
fn window_cluster(owner: usize, children: &[Vec<usize>]) -> Cluster {
    let leaves: Vec<_> = descendants(owner, children)
        .into_iter()
        .filter(|&i| i != owner)
        .collect();
    let cols = columns(leaves.len());
    let rows = leaves.len().div_ceil(cols).max(1);
    let width = (number(cols) * 38. + 12.).max(64.);
    let height = number(rows) * 18. + 30.;
    let mut points = vec![(
        owner,
        Point {
            x: width / 2.,
            y: -8.,
        },
    )];
    for (i, &node) in leaves.iter().enumerate() {
        points.push((
            node,
            Point {
                x: 22. + number(i % cols) * 38.,
                y: -28. - number(i / cols) * 18.,
            },
        ));
    }
    Cluster {
        width,
        height,
        points,
        groups: vec![Group {
            owner,
            members: descendants(owner, children),
            bounds: Bounds {
                left: 0.,
                right: width,
                top: 0.,
                bottom: -height,
            },
        }],
    }
}
fn translate(cluster: &mut Cluster, dx: f64, dy: f64) {
    for (_, p) in &mut cluster.points {
        p.x += dx;
        p.y += dy;
    }
    for group in &mut cluster.groups {
        group.bounds.left += dx;
        group.bounds.right += dx;
        group.bounds.top += dy;
        group.bounds.bottom += dy;
    }
}
fn session_cluster(owner: usize, children: &[Vec<usize>]) -> Cluster {
    let mut windows: Vec<_> = children[owner]
        .iter()
        .map(|&i| window_cluster(i, children))
        .collect();
    if windows.is_empty() {
        return window_cluster(owner, children);
    }
    let cols = columns(windows.len());
    let cell_width = windows.iter().map(|w| w.width).fold(0., f64::max) + 14.;
    let cell_height = windows.iter().map(|w| w.height).fold(0., f64::max) + 12.;
    let width = number(cols) * cell_width + 8.;
    let height = number(windows.len().div_ceil(cols)) * cell_height + 24.;
    let mut cluster = Cluster {
        width,
        height,
        points: vec![(
            owner,
            Point {
                x: width / 2.,
                y: -8.,
            },
        )],
        groups: vec![Group {
            owner,
            members: descendants(owner, children),
            bounds: Bounds {
                left: 0.,
                right: width,
                top: 0.,
                bottom: -height,
            },
        }],
    };
    for (i, window) in windows.iter_mut().enumerate() {
        translate(
            window,
            10. + number(i % cols) * cell_width,
            -24. - number(i / cols) * cell_height,
        );
        cluster.points.append(&mut window.points);
        cluster.groups.append(&mut window.groups);
    }
    cluster
}

impl Physics {
    pub fn grouped(
        graph: &Graph,
        previous: &HashMap<String, Point>,
    ) -> (Self, HashMap<String, Point>) {
        let mut parent = vec![None; graph.nodes.len()];
        for &(a, b) in &graph.hierarchy {
            parent[b] = Some(a);
        }
        let mut children = vec![Vec::new(); graph.nodes.len()];
        for (i, p) in parent.iter().enumerate() {
            if let Some(p) = p {
                children[*p].push(i);
            }
        }
        let roots: Vec<_> = parent
            .iter()
            .enumerate()
            .filter(|(_, p)| p.is_none())
            .map(|(i, _)| i)
            .collect();
        let mut clusters: Vec<_> = roots
            .iter()
            .map(|&root| {
                if graph.nodes[root].kind == Kind::Session {
                    session_cluster(root, &children)
                } else {
                    window_cluster(root, &children)
                }
            })
            .collect();
        let cols = columns(clusters.len());
        let mut anchors = HashMap::new();
        let mut groups = Vec::new();
        let mut y = 0.;
        for row in clusters.chunks_mut(cols) {
            let mut x = 0.;
            let height = row.iter().map(|c| c.height).fold(0., f64::max);
            for cluster in row {
                translate(cluster, x, y);
                for (i, p) in &cluster.points {
                    anchors.insert(graph.nodes[*i].id.clone(), *p);
                }
                groups.append(&mut cluster.groups);
                x += cluster.width + 44.;
            }
            y -= height + 32.;
        }
        let points = graph
            .nodes
            .iter()
            .enumerate()
            .map(|(index, node)| {
                let anchor = anchors[&node.id];
                let point = previous.get(&node.id).copied().unwrap_or_else(|| {
                    if !matches!(node.kind, Kind::Pane | Kind::Agent) {
                        return anchor;
                    }
                    let group = groups.iter().find(|group| {
                        graph.nodes[group.owner].kind == Kind::Window
                            && group.members.contains(&index)
                    });
                    group.map_or(anchor, |group| Point {
                        x: f64::midpoint(group.bounds.left, group.bounds.right)
                            + (anchor.x - f64::midpoint(group.bounds.left, group.bounds.right))
                                * 0.5,
                        y: f64::midpoint(group.bounds.top - 20., group.bounds.bottom + 10.)
                            + (anchor.y
                                - f64::midpoint(group.bounds.top - 20., group.bounds.bottom + 10.))
                                * 0.5,
                    })
                });
                (node.id.clone(), point)
            })
            .collect();
        (
            Self {
                groups,
                anchors,
                velocity: HashMap::new(),
                pinned: HashSet::new(),
                running: true,
            },
            points,
        )
    }
    pub fn drag(
        &mut self,
        graph: &Graph,
        node: usize,
        to: Point,
        points: &mut HashMap<String, Point>,
    ) {
        let id = &graph.nodes[node].id;
        let from = points[id];
        let dx = to.x - from.x;
        let dy = to.y - from.y;
        if matches!(graph.nodes[node].kind, Kind::Session | Kind::Window) {
            let members = self
                .groups
                .iter()
                .find(|g| g.owner == node)
                .map_or_else(|| vec![node], |g| g.members.clone());
            for &i in &members {
                let id = &graph.nodes[i].id;
                if let Some(p) = points.get_mut(id) {
                    p.x += dx;
                    p.y += dy;
                }
                if let Some(p) = self.anchors.get_mut(id) {
                    p.x += dx;
                    p.y += dy;
                }
                self.velocity.insert(
                    id.clone(),
                    Point {
                        x: (dx * 0.4).clamp(-4., 4.),
                        y: (dy * 0.4).clamp(-4., 4.),
                    },
                );
            }
            for group in &mut self.groups {
                if members.contains(&group.owner) {
                    group.bounds.left += dx;
                    group.bounds.right += dx;
                    group.bounds.top += dy;
                    group.bounds.bottom += dy;
                }
            }
        } else {
            points.insert(id.clone(), to);
            self.velocity.insert(
                id.clone(),
                Point {
                    x: (dx * 0.4).clamp(-4., 4.),
                    y: (dy * 0.4).clamp(-4., 4.),
                },
            );
        }
    }
    pub fn toggle_pin(&mut self, graph: &Graph, node: usize) {
        if let Some(node) = graph.nodes.get(node) {
            if !self.pinned.remove(&node.id) {
                self.pinned.insert(node.id.clone());
            }
            self.velocity.remove(&node.id);
        }
    }
    /// An observable impulse, also useful for exploring spring response after settling.
    pub fn bounce(&mut self, graph: &Graph) {
        self.running = true;
        for (index, node) in graph.nodes.iter().enumerate() {
            if self.pinned.contains(&node.id) || matches!(node.kind, Kind::Session | Kind::Window) {
                continue;
            }
            let angle = number(index) * 2.399_963_229_728_653;
            self.velocity.insert(
                node.id.clone(),
                Point {
                    x: angle.cos() * 4.,
                    y: angle.sin() * 4.,
                },
            );
        }
    }
    // A local grid avoids all-pairs work on every animation frame.
    #[allow(clippy::cast_possible_truncation)] // Finite layout coordinates, clamped to i32 cell range.
    fn cell(p: Point) -> (i32, i32) {
        let coordinate = |v: f64| {
            (v / 64.)
                .floor()
                .clamp(f64::from(i32::MIN) + 2., f64::from(i32::MAX) - 2.) as i32
        };
        (coordinate(p.x), coordinate(p.y))
    }
    pub fn tick(
        &mut self,
        graph: &Graph,
        points: &mut HashMap<String, Point>,
        held: Option<usize>,
    ) -> bool {
        if !self.running {
            return false;
        }
        let positions: Vec<_> = graph.nodes.iter().map(|n| points[&n.id]).collect();
        let mut forces = vec![Point::default(); positions.len()];
        let mut grid = HashMap::<(i32, i32), Vec<usize>>::new();
        for (i, p) in positions.iter().enumerate() {
            grid.entry(Self::cell(*p)).or_default().push(i);
        }
        for (i, p) in positions.iter().enumerate() {
            let (cx, cy) = Self::cell(*p);
            for gx in cx - 1..=cx + 1 {
                for gy in cy - 1..=cy + 1 {
                    if let Some(neighbors) = grid.get(&(gx, gy)) {
                        for &j in neighbors {
                            if j <= i {
                                continue;
                            }
                            let dx = p.x - positions[j].x;
                            let dy = p.y - positions[j].y;
                            let distance = dx.hypot(dy).max(1.);
                            let mut force = (12. / distance.powi(2)).min(0.8);
                            // Labels are much wider than tall in a terminal.
                            let overlap = (1. - (dx.abs() / 30.).max(dy.abs() / 11.)).max(0.);
                            force += overlap * 0.8;
                            let direction = if dx.abs() + dy.abs() < 0.01 {
                                Point { x: 1., y: 0.3 }
                            } else {
                                Point {
                                    x: dx / distance,
                                    y: dy / distance,
                                }
                            };
                            forces[i].x += direction.x * force;
                            forces[i].y += direction.y * force;
                            forces[j].x -= direction.x * force;
                            forces[j].y -= direction.y * force;
                        }
                    }
                }
            }
        }
        // Normalize by degree so a busy hub cannot pull entire sessions together.
        let mut degree = vec![0usize; graph.nodes.len()];
        for edge in &graph.edges {
            degree[edge.from] += 1;
            degree[edge.to] += 1;
        }
        // Communication adds gentle tension; group gravity dominates cross-group edges.
        for edge in &graph.edges {
            let a = positions[edge.from];
            let b = positions[edge.to];
            let dx = b.x - a.x;
            let dy = b.y - a.y;
            let distance = dx.hypot(dy).max(1.);
            let force = ((distance - 38.) * 0.0008).clamp(-0.08, 0.12)
                / number(degree[edge.from].max(degree[edge.to]).max(1));
            forces[edge.from].x += dx / distance * force;
            forces[edge.from].y += dy / distance * force;
            forces[edge.to].x -= dx / distance * force;
            forces[edge.to].y -= dy / distance * force;
        }
        let mut held_nodes = vec![false; graph.nodes.len()];
        if let Some(index) = held {
            held_nodes[index] = true;
            if let Some(group) = self.groups.iter().find(|group| group.owner == index) {
                for &member in &group.members {
                    held_nodes[member] = true;
                }
            }
        }
        let mut moved = false;
        for (i, node) in graph.nodes.iter().enumerate() {
            if held_nodes[i] || self.pinned.contains(&node.id) {
                continue;
            }
            let p = points.get_mut(&node.id).expect("graph position");
            let anchor = self.anchors[&node.id];
            let velocity = self.velocity.entry(node.id.clone()).or_default();
            velocity.x = (velocity.x + forces[i].x + (anchor.x - p.x) * 0.035) * 0.90;
            velocity.y = (velocity.y + forces[i].y + (anchor.y - p.y) * 0.035) * 0.90;
            velocity.x = velocity.x.clamp(-4., 4.);
            velocity.y = velocity.y.clamp(-4., 4.);
            if velocity.x.hypot(velocity.y) > 0.008 {
                p.x += velocity.x;
                p.y += velocity.y;
                moved = true;
            }
        }
        moved
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::Node;
    use super::*;
    use std::collections::BTreeSet;
    fn graph() -> Graph {
        let nodes = (0..14)
            .map(|i| Node {
                id: i.to_string(),
                kind: match i {
                    0 | 7 => Kind::Session,
                    1 | 8 => Kind::Window,
                    _ => Kind::Pane,
                },
                label: i.to_string(),
                location: String::new(),
                members: BTreeSet::new(),
            })
            .collect();
        let hierarchy = vec![
            (0, 1),
            (1, 2),
            (1, 3),
            (1, 4),
            (1, 5),
            (1, 6),
            (7, 8),
            (8, 9),
            (8, 10),
            (8, 11),
            (8, 12),
            (8, 13),
        ];
        Graph {
            nodes,
            hierarchy,
            edges: Vec::new(),
        }
    }
    #[test]
    fn sessions_have_separate_regions_and_windows_contain_their_panes() {
        let graph = graph();
        let (physics, points) = Physics::grouped(&graph, &HashMap::new());
        let sessions: Vec<_> = physics
            .groups
            .iter()
            .filter(|g| graph.nodes[g.owner].kind == Kind::Session)
            .collect();
        assert_eq!(sessions.len(), 2);
        assert!(sessions[0].bounds.right < sessions[1].bounds.left);
        for group in &physics.groups {
            for i in &group.members {
                let p = points[&graph.nodes[*i].id];
                assert!(
                    p.x >= group.bounds.left
                        && p.x <= group.bounds.right
                        && p.y <= group.bounds.top
                        && p.y >= group.bounds.bottom
                );
            }
        }
    }
    #[test]
    fn disturbed_nodes_relax_smoothly_and_dragged_nodes_remain_pinned() {
        let graph = graph();
        let (mut physics, _) = Physics::grouped(&graph, &HashMap::new());
        let mut points = physics.anchors.clone();
        let anchor = physics.anchors["2"];
        points.get_mut("2").unwrap().x += 12.;
        let before = points["2"];
        assert!(physics.tick(&graph, &mut points, None));
        assert!((points["2"].x - before.x).abs() < 4.);
        for _ in 0..120 {
            physics.tick(&graph, &mut points, None);
        }
        assert!((points["2"].x - anchor.x).abs() < 4.);
        let pinned = Point { x: 17., y: 29. };
        physics.drag(&graph, 2, pinned, &mut points);
        physics.toggle_pin(&graph, 2);
        for _ in 0..20 {
            physics.tick(&graph, &mut points, None);
        }
        assert_eq!(points["2"], pinned);
        physics.running = false;
        let stopped = points.clone();
        physics.tick(&graph, &mut points, None);
        assert_eq!(points, stopped);
    }
    #[test]
    fn dragging_a_window_moves_its_cluster_together() {
        let graph = graph();
        let (mut physics, mut points) = Physics::grouped(&graph, &HashMap::new());
        let before = points.clone();
        let p = before["1"];
        physics.drag(
            &graph,
            1,
            Point {
                x: p.x + 30.,
                y: p.y - 10.,
            },
            &mut points,
        );
        for i in 1..7 {
            assert_eq!(
                points[&i.to_string()],
                Point {
                    x: before[&i.to_string()].x + 30.,
                    y: before[&i.to_string()].y - 10.
                }
            );
        }
        assert_eq!(points["0"], before["0"]);
        assert_eq!(points["8"], before["8"]);
    }
    #[test]
    fn sibling_windows_have_separate_boxes_inside_the_same_session() {
        let mut graph = graph();
        graph.nodes.push(Node {
            id: "14".into(),
            kind: Kind::Window,
            label: "second window".into(),
            location: String::new(),
            members: BTreeSet::new(),
        });
        graph
            .hierarchy
            .retain(|&(a, b)| !(a == 1 && matches!(b, 5 | 6)));
        graph.hierarchy.extend([(0, 14), (14, 5), (14, 6)]);
        let (physics, _) = Physics::grouped(&graph, &HashMap::new());
        let one = &physics.groups.iter().find(|g| g.owner == 1).unwrap().bounds;
        let two = &physics
            .groups
            .iter()
            .find(|g| g.owner == 14)
            .unwrap()
            .bounds;
        assert!(
            one.right < two.left
                || two.right < one.left
                || one.top < two.bottom
                || two.top < one.bottom
        );
        let session = &physics.groups.iter().find(|g| g.owner == 0).unwrap().bounds;
        assert!(one.left > session.left && two.right < session.right);
    }
    #[test]
    fn busy_cross_session_traffic_does_not_collapse_group_regions() {
        let mut graph = graph();
        for from in 2..7 {
            for to in 9..14 {
                graph.edges.push(super::super::model::Edge {
                    from,
                    to,
                    count: 100,
                    replies: 100,
                    pending: 0,
                });
            }
        }
        let (mut physics, mut points) = Physics::grouped(&graph, &HashMap::new());
        for _ in 0..300 {
            physics.tick(&graph, &mut points, None);
        }
        for group in &physics.groups {
            for &i in &group.members {
                let p = points[&graph.nodes[i].id];
                assert!(
                    p.x > group.bounds.left
                        && p.x < group.bounds.right
                        && p.y > group.bounds.bottom
                        && p.y < group.bounds.top
                );
            }
        }
    }
    #[test]
    fn released_drag_overshoots_and_settles_instead_of_becoming_fixed() {
        let graph = graph();
        let (mut physics, _) = Physics::grouped(&graph, &HashMap::new());
        let mut points = physics.anchors.clone();
        let anchor = points["2"];
        physics.drag(
            &graph,
            2,
            Point {
                x: anchor.x + 24.,
                y: anchor.y,
            },
            &mut points,
        );
        assert!(!physics.pinned.contains("2"));
        let held = points["2"];
        physics.tick(&graph, &mut points, Some(2));
        assert_eq!(points["2"], held);
        let mut crossed = false;
        for _ in 0..180 {
            physics.tick(&graph, &mut points, None);
            crossed |= points["2"].x < anchor.x - 0.5;
        }
        assert!(crossed, "the spring should overshoot its resting position");
        assert!((points["2"].x - anchor.x).abs() < 3.);
    }
    #[test]
    fn startup_and_bounce_have_visible_motion_without_dragging() {
        let graph = graph();
        let (mut physics, mut points) = Physics::grouped(&graph, &HashMap::new());
        let initial = points.clone();
        for _ in 0..20 {
            physics.tick(&graph, &mut points, None);
        }
        assert!((points["2"].x - initial["2"].x).abs() > 4.);
        let before = points.clone();
        physics.bounce(&graph);
        for _ in 0..6 {
            physics.tick(&graph, &mut points, None);
        }
        assert!(graph
            .nodes
            .iter()
            .any(|node| (points[&node.id].x - before[&node.id].x)
                .hypot(points[&node.id].y - before[&node.id].y)
                > 8.));
    }
}
