//! Bounded lookup over a reusable, normalized projection of the local topology.
//! Historical transcript evidence remains a separate search surface.

use crate::{AgentKind, AgentState, PaneKey, TopologySnapshot};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentLocation {
    pub location: PaneKey,
    pub workspace: String,
    pub window: String,
    pub agent_session_id: String,
    pub kind: AgentKind,
    pub state: AgentState,
    pub evidence: Vec<LocationEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocationEvidence {
    pub field: String,
    pub excerpt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocationResults {
    pub scope: String,
    pub total: usize,
    pub has_more: bool,
    pub unassigned_agents: usize,
    #[serde(default)]
    pub unlocated_matches: usize,
    pub warnings: Vec<String>,
    pub matches: Vec<AgentLocation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freshness: Option<LocationFreshness>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocationFreshness {
    #[serde(with = "time::serde::rfc3339")]
    pub inventory_observed_at: time::OffsetDateTime,
    pub inventory_age_ms: u64,
    pub max_inventory_age_ms: u64,
    pub reused_index: bool,
}

struct SearchField {
    name: &'static str,
    text: String,
    lower: String,
    weight: u32,
}

struct Document {
    location: AgentLocation,
    last_activity_at: time::OffsetDateTime,
    fields: Vec<SearchField>,
}

/// An immutable derived view. Construct once per source change, then share
/// across queries and clients. Query text and answers are never cached.
pub struct DirectoryIndex {
    documents: Vec<Document>,
    unassigned_agents: usize,
    unassigned: Vec<(String, Vec<String>)>,
}

impl DirectoryIndex {
    pub fn build(topology: &TopologySnapshot) -> Self {
        let mut documents = Vec::new();
        for session in &topology.sessions {
            for window in &session.windows {
                for pane in &window.panes {
                    let Some(agent) = &pane.agent else { continue };
                    if agent.state == AgentState::Stopped || agent.ask_origin.is_some() {
                        continue;
                    }
                    let fields = [
                        ("agent_session_id", agent.session_id.as_str(), 5),
                        ("pane", pane.key.pane_id.as_str(), 4),
                        ("workspace", session.name.as_str(), 4),
                        ("window", window.name.as_str(), 4),
                        ("cwd", pane.cwd.as_str(), 4),
                        ("alias", pane.agent_alias.as_deref().unwrap_or(""), 4),
                        ("title", pane.title.as_str(), 3),
                        ("ai_title", agent.ai_title.as_deref().unwrap_or(""), 3),
                        ("agent_cwd", agent.cwd.as_deref().unwrap_or(""), 3),
                        ("recap", agent.recap.as_deref().unwrap_or(""), 2),
                        ("last_prompt", agent.last_prompt.as_deref().unwrap_or(""), 1),
                        (
                            "last_response",
                            agent.last_response.as_deref().unwrap_or(""),
                            1,
                        ),
                    ]
                    .into_iter()
                    .filter(|(_, text, _)| !text.is_empty())
                    .map(|(name, text, weight)| {
                        let text = crate::agent_records::redact(text);
                        SearchField {
                            name,
                            lower: text.to_lowercase(),
                            text,
                            weight,
                        }
                    })
                    .collect();
                    documents.push(Document {
                        location: AgentLocation {
                            location: pane.key.clone(),
                            workspace: session.name.clone(),
                            window: window.name.clone(),
                            agent_session_id: agent.session_id.clone(),
                            kind: agent.kind,
                            state: agent.state,
                            evidence: Vec::new(),
                        },
                        last_activity_at: agent.last_activity_at,
                        fields,
                    });
                }
            }
        }
        let unassigned = topology
            .unassigned_agents
            .iter()
            .filter(|agent| agent.state != AgentState::Stopped && agent.ask_origin.is_none())
            .map(|agent| {
                (
                    agent.session_id.clone(),
                    [
                        agent.cwd.as_deref(),
                        agent.ai_title.as_deref(),
                        agent.recap.as_deref(),
                        agent.last_prompt.as_deref(),
                        agent.last_response.as_deref(),
                    ]
                    .into_iter()
                    .flatten()
                    .map(|text| crate::agent_records::redact(text).to_lowercase())
                    .collect(),
                )
            })
            .collect();
        Self {
            documents,
            unassigned_agents: topology.unassigned_agents.len(),
            unassigned,
        }
    }

    /// All whitespace-separated keywords must match, case-insensitively.
    /// Rank location/title evidence above incidental message mentions.
    pub fn find(
        &self,
        query: &str,
        limit: usize,
        exclude: &[String],
    ) -> Result<LocationResults, &'static str> {
        validate(query, limit)?;
        let terms: Vec<_> = query.split_whitespace().map(str::to_lowercase).collect();
        let identity = terms.first().filter(|_| terms.len() == 1).filter(|term| {
            uuid::Uuid::parse_str(term).is_ok()
                || self
                    .documents
                    .iter()
                    .any(|doc| matches_identity(&doc.location.agent_session_id, term))
                || self
                    .unassigned
                    .iter()
                    .any(|(id, _)| matches_identity(id, term))
        });
        let mut found = Vec::new();
        for doc in &self.documents {
            if exclude.contains(&doc.location.agent_session_id)
                || identity
                    .is_some_and(|term| !matches_identity(&doc.location.agent_session_id, term))
                || !terms
                    .iter()
                    .all(|term| doc.fields.iter().any(|f| matches_field(f, term)))
            {
                continue;
            }
            let score: u32 = doc
                .fields
                .iter()
                .filter(|f| terms.iter().any(|term| matches_field(f, term)))
                .map(|f| f.weight)
                .sum();
            found.push((score, doc));
        }
        found.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then(b.1.last_activity_at.cmp(&a.1.last_activity_at))
                .then(a.1.location.location.cmp(&b.1.location.location))
        });
        let total = found.len();
        let matches = found
            .into_iter()
            .take(limit)
            .map(|(_, doc)| {
                let mut location = doc.location.clone();
                location.evidence = doc
                    .fields
                    .iter()
                    .filter(|f| terms.iter().any(|term| matches_field(f, term)))
                    .take(3)
                    .map(|f| LocationEvidence {
                        field: f.name.into(),
                        excerpt: excerpt(&f.text, &terms),
                    })
                    .collect();
                location
            })
            .collect();
        Ok(LocationResults {
            scope: "local_observed_panes; keyword matches are candidates, not proof of task ownership or completion".into(),
            total, has_more: total > limit, unassigned_agents: self.unassigned_agents,
            unlocated_matches: self.unassigned.iter().filter(|(id, fields)| !exclude.contains(id)
                && identity.is_none_or(|term| matches_identity(id,term))
                && terms.iter().all(|term| matches_identity(id,term) || fields.iter().any(|f| f.contains(term)))).count(),
            warnings: Vec::new(), matches, freshness: None,
        })
    }
}

/// Uncached entry point for pure callers and topology fixtures.
pub fn find(
    topology: &TopologySnapshot,
    query: &str,
    limit: usize,
    exclude_sessions: &[String],
) -> Result<LocationResults, &'static str> {
    validate(query, limit)?;
    DirectoryIndex::build(topology).find(query, limit, exclude_sessions)
}

fn matches_identity(id: &str, term: &str) -> bool {
    let id = id.to_lowercase();
    id == term || (term.len() >= 8 && id.starts_with(term))
}

fn matches_field(field: &SearchField, term: &str) -> bool {
    match field.name {
        "agent_session_id" => {
            field.lower == term || (term.len() >= 8 && field.lower.starts_with(term))
        }
        "pane" => field.lower == term || field.lower.rsplit(':').next() == Some(term),
        _ => field.lower.contains(term),
    }
}

fn excerpt(text: &str, terms: &[String]) -> String {
    let lower = text.to_lowercase();
    let byte = terms
        .iter()
        .filter_map(|term| lower.find(term))
        .min()
        .unwrap_or(0);
    // Lowercasing may expand a character (İ -> i + combining dot). Map
    // back to the original character index before choosing the excerpt.
    let mut lower_bytes = 0;
    let center = text
        .chars()
        .position(|ch| {
            lower_bytes += ch.to_lowercase().map(char::len_utf8).sum::<usize>();
            lower_bytes > byte
        })
        .unwrap_or(0);
    let chars: Vec<_> = text.chars().collect();
    let start = center.saturating_sub(60).min(chars.len());
    let end = (start + 200).min(chars.len());
    format!(
        "{}{}{}",
        if start > 0 { "…" } else { "" },
        chars[start..end].iter().collect::<String>(),
        if end < chars.len() { "…" } else { "" }
    )
}

pub fn validate(query: &str, limit: usize) -> Result<(), &'static str> {
    if query.trim().is_empty() {
        return Err("provide at least one keyword or task identifier");
    }
    if query.len() > 1024 || query.split_whitespace().count() > 16 {
        return Err("use at most 16 keywords and 1024 bytes");
    }
    if !(1..=50).contains(&limit) {
        return Err("limit must be between 1 and 50");
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::process_tree::WorkloadSummary;
    use crate::tmux::PaneInfo;
    use crate::{Agent, HostKind, TopologyInput};
    use time::OffsetDateTime;
    pub(crate) fn pane(socket: &str, session_name: &str, window_name: &str) -> PaneInfo {
        PaneInfo {
            session_group: None,
            agent_program: None,
            worker_launch_id: None,
            agent_role: None,
            agent_alias: None,
            workspace_id: None,
            work_id: None,
            pane_id: "%1".into(),
            session_id: "$1".into(),
            session: session_name.into(),
            window_id: "@1".into(),
            window_name: window_name.into(),
            window_index: "0".into(),
            pane_index: "0".into(),
            tty: String::new(),
            current_command: "codex".into(),
            title: String::new(),
            current_path: "/repo".into(),
            pane_pid: 10,
            socket: Some(socket.into()),
        }
    }

    pub(crate) fn agent(agent_session_id: &str, socket: Option<&str>) -> Agent {
        let now = OffsetDateTime::UNIX_EPOCH;
        Agent {
            ask_origin: None,
            kind: AgentKind::Codex,
            session_id: agent_session_id.into(),
            surface: None,
            pane: Some("%1".into()),
            tmux_socket: socket.map(Into::into),
            tmux_session: None,
            cwd: Some("/repo".into()),
            pid: None,
            workload: WorkloadSummary::default(),
            subagents: Vec::new(),
            state: AgentState::Working,
            last_prompt: None,
            last_prompt_at: None,
            last_response: None,
            recap: None,
            ai_title: None,
            last_notification: None,
            model: None,
            context_used_pct: None,
            cost_usd: None,
            rate_limit_5h_pct: None,
            rate_limit_5h_resets_at: None,
            rate_limit_7d_pct: None,
            rate_limit_7d_resets_at: None,
            rate_limited_until: None,
            rate_limit_scope: None,
            rate_limit_source: None,
            started_at: now,
            last_activity_at: now,
            state_entered_at: now,
        }
    }

    fn fixture() -> TopologySnapshot {
        let mut direct = pane("a", "project", "CAL-8226");
        direct.session_group = Some("project".into());
        let mut view = direct.clone();
        view.session_id = "$9".into();
        view.session = "project~view~9".into();
        let mut first = agent("implementer", Some("a"));
        first.last_prompt = Some("unrelated follow-up".into());
        let other = pane("b", "other", "main");
        let mut second = agent("mention", Some("b"));
        second.last_response = Some(format!("{} CAL-8226 화자 분리", "앞".repeat(500)));
        TopologySnapshot::build(
            OffsetDateTime::UNIX_EPOCH,
            vec![TopologyInput::new(
                HostKind::Tmux,
                vec![direct, view, other],
            )],
            vec![first, second],
        )
    }

    #[test]
    fn ranks_location_over_mentions_and_folds_views_without_socket_collisions() {
        let result = find(&fixture(), "cal-8226", 10, &[]).unwrap();
        assert_eq!(result.total, 2);
        assert_eq!(result.matches[0].agent_session_id, "implementer");
        assert_eq!(result.matches[0].workspace, "project");
        assert_eq!(
            result.matches[0].location.window.session.endpoint.socket,
            "a"
        );
        assert_eq!(
            result.matches[1].location.window.session.endpoint.socket,
            "b"
        );
        assert!(result.matches[1].evidence[0].excerpt.contains("CAL-8226"));
        assert!(result.matches[1].evidence[0].excerpt.chars().count() <= 202);
    }

    #[test]
    fn supports_korean_keywords_across_fields_and_session_exclusion() {
        let result = find(&fixture(), "화자 분리 other", 10, &[]).unwrap();
        assert_eq!(result.total, 1);
        assert_eq!(result.matches[0].agent_session_id, "mention");
        assert_eq!(
            find(&fixture(), "CAL-8226", 10, &["implementer".into()])
                .unwrap()
                .total,
            1
        );
    }

    #[test]
    fn bounds_results_and_reports_no_match_without_inventing_a_location() {
        let result = find(&fixture(), "cal-8226", 1, &[]).unwrap();
        assert_eq!(result.matches.len(), 1);
        assert!(result.has_more);
        assert_eq!(result.total, 2);
        assert_eq!(
            find(&fixture(), "missing-identifier", 10, &[])
                .unwrap()
                .total,
            0
        );
        assert!(find(&fixture(), " ", 10, &[]).is_err());
        assert!(find(&fixture(), "x", 0, &[]).is_err());
        assert!(find(&fixture(), "x", 51, &[]).is_err());
    }

    #[test]
    fn stopped_and_unassigned_records_are_never_reported_as_current_locations() {
        let mut topology = fixture();
        let agent = topology.sessions[0].windows[0].panes[0]
            .agent
            .as_mut()
            .unwrap();
        agent.state = AgentState::Stopped;
        let mut orphan = agent.clone();
        orphan.state = AgentState::Working;
        orphan.session_id = "orphan".into();
        topology.unassigned_agents.push(orphan);
        let result = find(&topology, "cal-8226", 10, &[]).unwrap();
        assert_eq!(result.total, 1);
        assert_eq!(result.unassigned_agents, 1);
    }
    #[test]
    fn excerpts_redact_credentials_before_clipping_and_handle_unicode_case_expansion() {
        let text = format!("{} NEEDLE API_KEY=super-secret-token", "İ".repeat(400));
        let clipped = excerpt(&crate::agent_records::redact(&text), &["needle".into()]);
        assert!(clipped.contains("NEEDLE"));
        assert!(!clipped.contains("super-secret-token"));
        assert!(clipped.chars().count() <= 202);
    }
    #[test]
    fn secrets_are_not_searchable_and_short_numbers_do_not_match_identity_fields() {
        let mut topology = fixture();
        for session in &mut topology.sessions {
            for window in &mut session.windows {
                for pane in &mut window.panes {
                    if let Some(agent) = &mut pane.agent {
                        agent.last_response = Some("API_KEY=supersecretvalue".into());
                    }
                }
            }
        }
        let index = DirectoryIndex::build(&topology);
        assert_eq!(index.find("supersecretvalue", 10, &[]).unwrap().total, 0);
        assert_eq!(index.find("1", 10, &[]).unwrap().total, 0);
        assert_eq!(index.find("%1", 10, &[]).unwrap().total, 2);
    }
    #[test]
    fn session_identity_lookup_ignores_transcript_mentions_even_when_unlocated_or_unknown() {
        let mut topology = fixture();
        let sid = "01a1820e-a385-79d0-acfd-8ebda41e2d5b";
        topology.sessions[0].windows[0].panes[0]
            .agent
            .as_mut()
            .unwrap()
            .session_id = sid.into();
        topology.sessions[1].windows[0].panes[0]
            .agent
            .as_mut()
            .unwrap()
            .last_response = Some(format!("I located {sid}"));
        for query in [sid, &sid[..8]] {
            let result = find(&topology, query, 10, &[]).unwrap();
            assert_eq!(result.total, 1);
            assert_eq!(result.matches[0].agent_session_id, sid);
        }
        let mut target = topology.sessions[0].windows[0].panes[0]
            .agent
            .take()
            .unwrap();
        target.pane = None;
        topology.unassigned_agents.push(target);
        let result = find(&topology, sid, 10, &[]).unwrap();
        assert_eq!(result.total, 0);
        assert_eq!(result.unlocated_matches, 1);
        assert_eq!(
            find(&topology, &sid[..8], 10, &[])
                .unwrap()
                .unlocated_matches,
            1
        );
        topology.unassigned_agents.clear();
        assert_eq!(find(&topology, sid, 10, &[]).unwrap().total, 0);
    }
}
