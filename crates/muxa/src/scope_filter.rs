//! Scope exclusion helpers for pane/session-based reports.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScopeExclusions {
    pane_patterns: Vec<String>,
    session_patterns: Vec<String>,
}

impl ScopeExclusions {
    #[must_use]
    pub fn new(pane_patterns: Vec<String>, session_patterns: Vec<String>) -> Self {
        Self {
            pane_patterns: clean_patterns(pane_patterns),
            session_patterns: clean_patterns(session_patterns),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pane_patterns.is_empty() && self.session_patterns.is_empty()
    }

    #[must_use]
    pub fn excludes(
        &self,
        pane: Option<&str>,
        session_id: Option<&str>,
        session_name: Option<&str>,
    ) -> bool {
        pane.is_some_and(|pane| matches_any(&self.pane_patterns, pane))
            || session_name.is_some_and(|name| matches_any(&self.session_patterns, name))
            || session_id.is_some_and(|id| matches_any(&self.session_patterns, id))
    }
}

fn clean_patterns(patterns: Vec<String>) -> Vec<String> {
    patterns
        .into_iter()
        .map(|pattern| pattern.trim().to_string())
        .filter(|pattern| !pattern.is_empty())
        .collect()
}

fn matches_any(patterns: &[String], value: &str) -> bool {
    patterns
        .iter()
        .any(|pattern| wildcard_match(pattern, value))
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    let pattern = pattern.chars().collect::<Vec<_>>();
    let value = value.chars().collect::<Vec<_>>();
    let mut previous = vec![false; value.len() + 1];
    previous[0] = true;

    for ch in pattern {
        let mut current = vec![false; value.len() + 1];
        match ch {
            '*' => {
                current[0] = previous[0];
                for idx in 1..=value.len() {
                    current[idx] = previous[idx] || current[idx - 1];
                }
            }
            '?' => {
                current[1..=value.len()].copy_from_slice(&previous[..value.len()]);
            }
            literal => {
                for idx in 1..=value.len() {
                    current[idx] = previous[idx - 1] && value[idx - 1] == literal;
                }
            }
        }
        previous = current;
    }

    previous[value.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_supports_star_and_question_mark() {
        assert!(wildcard_match("monitor*", "monitoring"));
        assert!(wildcard_match("%?", "%1"));
        assert!(wildcard_match("muxa-*-prod", "muxa-agent-prod"));
        assert!(!wildcard_match("monitor?", "monitoring"));
        assert!(!wildcard_match("main", "main-2"));
    }

    #[test]
    fn exclusions_match_pane_session_name_or_session_id() {
        let exclusions = ScopeExclusions::new(vec!["%monitor*".into()], vec!["watch-*".into()]);

        assert!(exclusions.excludes(Some("%monitor1"), None, None));
        assert!(exclusions.excludes(None, Some("agent-1"), Some("watch-main")));
        assert!(exclusions.excludes(None, Some("watch-agent"), None));
        assert!(!exclusions.excludes(Some("%1"), Some("agent-1"), Some("main")));
    }
}
