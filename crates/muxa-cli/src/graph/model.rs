use muxa::collaboration::{CollaborationRequest, Participant};
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Session,
    Window,
    Pane,
    Agent,
    Console,
}
impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Window => "window",
            Self::Pane => "pane",
            Self::Agent => "agent",
            Self::Console => "console",
        }
    }
}
#[derive(Debug, Clone)]
pub(super) struct Node {
    pub id: String,
    pub kind: Kind,
    pub label: String,
    pub location: String,
    pub members: BTreeSet<String>,
}
#[derive(Debug, Clone)]
pub(super) struct Edge {
    pub from: usize,
    pub to: usize,
    pub count: usize,
    pub replies: usize,
    pub pending: usize,
}
#[derive(Debug, Default)]
pub(super) struct Graph {
    pub nodes: Vec<Node>,
    pub hierarchy: Vec<(usize, usize)>,
    pub edges: Vec<Edge>,
}
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub(super) struct Point {
    pub x: f64,
    pub y: f64,
}

pub(super) fn identity(p: &Participant) -> String {
    if p.console {
        return "console".into();
    }
    serde_json::to_string(&(
        &p.room.host,
        p.socket.as_ref().or(p.room.socket.as_ref()),
        &p.agent_session_id,
        &p.pane,
    ))
    .expect("serializable identity")
}

impl Graph {
    #[allow(clippy::too_many_lines)] // Build hierarchy and directed counts in one pass.
    pub fn build(requests: &[&CollaborationRequest]) -> Self {
        let mut nodes = BTreeMap::<String, Node>::new();
        let mut links = BTreeSet::new();
        let mut pairs = BTreeMap::<(String, String), (usize, usize, usize)>::new();
        // Newest records determine labels and locations for a participant.
        for request in requests.iter().rev() {
            for p in [&request.from, &request.to] {
                let leaf = identity(p);
                let socket = p.socket.as_ref().or(p.room.socket.as_ref());
                let mut parent: Option<String> = None;
                if !p.console {
                    for (kind, value, label) in [
                        (
                            Kind::Session,
                            p.tmux_session_id.as_ref().or(p.tmux_session_name.as_ref()),
                            p.tmux_session_name.as_ref().or(p.tmux_session_id.as_ref()),
                        ),
                        (
                            Kind::Window,
                            Some(&p.room.window_id),
                            p.window_name.as_ref().or(Some(&p.room.window_id)),
                        ),
                    ] {
                        let Some(value) = value.filter(|v| !v.is_empty()) else {
                            continue;
                        };
                        let id =
                            serde_json::to_string(&(kind.label(), &p.room.host, socket, value))
                                .unwrap();
                        let node = nodes.entry(id.clone()).or_insert_with(|| Node {
                            id: id.clone(),
                            kind,
                            label: label.cloned().unwrap_or_default(),
                            location: format!(
                                "{} {} {}",
                                p.room.host,
                                socket.map_or("default", String::as_str),
                                value
                            ),
                            members: BTreeSet::new(),
                        });
                        node.members.insert(leaf.clone());
                        if let Some(parent) = parent {
                            links.insert((parent, id.clone()));
                        }
                        parent = Some(id);
                    }
                }
                let kind = if p.console {
                    Kind::Console
                } else if p.pane.is_empty() {
                    Kind::Agent
                } else {
                    Kind::Pane
                };
                nodes.insert(
                    leaf.clone(),
                    Node {
                        id: leaf.clone(),
                        kind,
                        label: if p.console {
                            "console".into()
                        } else {
                            p.label()
                        },
                        location: format!(
                            "{} {} {}",
                            p.room.host,
                            socket.map_or("default", String::as_str),
                            p.agent_session_id
                        ),
                        members: BTreeSet::from([leaf.clone()]),
                    },
                );
                if let Some(parent) = parent {
                    links.insert((parent, leaf));
                }
            }
            let pair = pairs
                .entry((identity(&request.from), identity(&request.to)))
                .or_default();
            pair.0 += 1;
            pair.1 += usize::from(request.reply.is_some());
            pair.2 += usize::from(!request.status.is_terminal());
        }
        let nodes: Vec<_> = nodes.into_values().collect();
        let index: HashMap<_, _> = nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id.clone(), i))
            .collect();
        Self {
            hierarchy: links
                .into_iter()
                .map(|(a, b)| (index[&a], index[&b]))
                .collect(),
            edges: pairs
                .into_iter()
                .map(|((a, b), (count, replies, pending))| Edge {
                    from: index[&a],
                    to: index[&b],
                    count,
                    replies,
                    pending,
                })
                .collect(),
            nodes,
        }
    }
    pub fn touches(&self, node: usize, request: &CollaborationRequest) -> bool {
        self.nodes.get(node).is_some_and(|n| {
            n.members.contains(&identity(&request.from))
                || n.members.contains(&identity(&request.to))
        })
    }
    #[cfg(test)]
    pub fn layout(&self, previous: &HashMap<String, Point>) -> HashMap<String, Point> {
        super::physics::Physics::grouped(self, previous).1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn participant(socket: &str, id: &str) -> Participant {
        let mut p = Participant::console(muxa::collaboration::RoomId {
            host: "tmux".into(),
            socket: Some(socket.into()),
            window_id: "@1".into(),
        });
        p.console = false;
        p.pane = "%1".into();
        p.agent_session_id = id.into();
        p.socket = Some(socket.into());
        p.tmux_session_id = Some("$1".into());
        p.tmux_session_name = Some("work".into());
        p
    }
    #[tokio::test]
    async fn identity_hierarchy_direction_and_retained_positions() {
        let store = muxa::collaboration::CollaborationStore::in_memory(
            muxa::collaboration::CollaborationOptions::default(),
        );
        let a = participant("/a/default", "a");
        let b = participant("/b/default", "b");
        let request = store
            .create(
                a,
                b,
                muxa::collaboration::NewRequest {
                    body: "test".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let graph = Graph::build(&[&request]);
        assert_eq!(graph.nodes.len(), 6);
        assert_eq!(graph.hierarchy.len(), 4);
        assert_eq!(graph.edges[0].count, 1);
        assert_eq!(graph.edges[0].pending, 1);
        assert!(graph.touches(graph.edges[0].from, &request));
        assert_ne!(
            graph.nodes[graph.edges[0].from].id,
            graph.nodes[graph.edges[0].to].id
        );
        let replied = store
            .reply(
                &request.to,
                &request.id,
                muxa::collaboration::RequestStatus::Completed,
                "done".into(),
                Vec::new(),
                Vec::new(),
            )
            .await
            .unwrap();
        let replied_graph = Graph::build(&[&replied]);
        assert_eq!(replied_graph.edges[0].replies, 1);
        assert_eq!(replied_graph.edges[0].pending, 0);
        let mut points = graph.layout(&HashMap::new());
        points.insert(graph.nodes[0].id.clone(), Point { x: 123., y: -42. });
        assert_eq!(
            graph.layout(&points)[&graph.nodes[0].id],
            Point { x: 123., y: -42. }
        );
    }
}
