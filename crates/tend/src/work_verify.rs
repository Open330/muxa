//! Acceptance criteria and declared checks on a Work.
//!
//! A person (`tend work plan`/`work up`) or the requesting agent (the MCP
//! start tool) declares what done means and which commands prove it. tend
//! runs those commands itself when the Work reports a result, so the person
//! is not the first verifier. Commands come only from this declaration,
//! never from repository content.

use serde::{Deserialize, Serialize};

/// Most criteria one Work carries.
pub const MAX_ACCEPTANCE: usize = 16;
/// Most declared checks one Work carries.
pub const MAX_CHECKS: usize = 8;
/// Default and largest per-check time limit.
pub const DEFAULT_CHECK_TIMEOUT_SECS: u32 = 300;
pub const MAX_CHECK_TIMEOUT_SECS: u32 = 3_600;

/// Who declared a Work's acceptance criteria and checks, as tendd saw the
/// caller: the person (any caller not running in a tracked agent's pane),
/// or the agent session whose pane the caller runs in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Declarer {
    Operator,
    Agent { session_id: String },
}

impl Declarer {
    /// Whether `self` may change a declaration `current` made. The person
    /// may always; an agent only its own, or one nobody has made yet. This
    /// keeps the agent whose work is checked from rewriting its own checks;
    /// it is not authentication, since every caller runs as the same user.
    #[must_use]
    pub fn may_replace(&self, current: Option<&Self>) -> bool {
        match (self, current) {
            (Self::Operator, _) | (_, None) => true,
            (Self::Agent { session_id }, Some(Self::Agent { session_id: owner })) => {
                session_id == owner
            }
            (Self::Agent { .. }, Some(Self::Operator)) => false,
        }
    }
}

/// One command tend runs to verify a result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkCheck {
    /// Shell command line, run in the Work's directory.
    pub command: String,
    /// Short label shown with the result; defaults to the command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Seconds before the check is stopped and counted as failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u32>,
}

impl WorkCheck {
    #[must_use]
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            name: None,
            timeout_secs: None,
        }
    }

    /// The label to show: the name, else the command.
    #[must_use]
    pub fn label(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.command)
    }

    #[must_use]
    pub fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(u64::from(
            self.timeout_secs.unwrap_or(DEFAULT_CHECK_TIMEOUT_SECS),
        ))
    }
}

fn clean_line(value: &str, max: usize, label: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("{label} cannot be empty"));
    }
    if value.len() > max {
        return Err(format!("{label} is too long (max {max} bytes)"));
    }
    if value.chars().any(|ch| ch.is_control() && ch != '\t') {
        return Err(format!(
            "{label} cannot contain control characters or newlines"
        ));
    }
    Ok(value.to_string())
}

/// Trimmed, bounded criteria; duplicates dropped, order kept.
pub fn validate_acceptance(criteria: Vec<String>) -> Result<Vec<String>, String> {
    if criteria.len() > MAX_ACCEPTANCE {
        return Err(format!("at most {MAX_ACCEPTANCE} acceptance criteria"));
    }
    let mut out: Vec<String> = Vec::with_capacity(criteria.len());
    for criterion in criteria {
        let criterion = clean_line(&criterion, 1_024, "acceptance criterion")?;
        if !out.contains(&criterion) {
            out.push(criterion);
        }
    }
    Ok(out)
}

/// Bounded checks with a sane timeout each.
pub fn validate_checks(checks: Vec<WorkCheck>) -> Result<Vec<WorkCheck>, String> {
    if checks.len() > MAX_CHECKS {
        return Err(format!("at most {MAX_CHECKS} checks"));
    }
    checks
        .into_iter()
        .map(|check| {
            let command = clean_line(&check.command, 2_048, "check command")?;
            let name = check
                .name
                .as_deref()
                .map(|name| clean_line(name, 120, "check name"))
                .transpose()?;
            if let Some(timeout) = check.timeout_secs {
                if timeout == 0 || timeout > MAX_CHECK_TIMEOUT_SECS {
                    return Err(format!(
                        "check timeout must be 1-{MAX_CHECK_TIMEOUT_SECS} seconds"
                    ));
                }
            }
            Ok(WorkCheck {
                command,
                name,
                timeout_secs: check.timeout_secs,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acceptance_is_trimmed_deduplicated_and_bounded() {
        assert_eq!(
            validate_acceptance(vec![
                " tests pass ".into(),
                "tests pass".into(),
                "docs".into()
            ])
            .unwrap(),
            ["tests pass", "docs"]
        );
        assert!(validate_acceptance(vec![String::new()]).is_err());
        assert!(validate_acceptance(vec!["a\nb".into()]).is_err());
        assert!(validate_acceptance((0..17).map(|i| i.to_string()).collect()).is_err());
    }

    #[test]
    fn checks_are_bounded_and_need_a_sane_timeout() {
        assert!(validate_checks(vec![WorkCheck::new("cargo test")]).is_ok());
        assert!(validate_checks(vec![WorkCheck::new("  ")]).is_err());
        let slow = WorkCheck {
            timeout_secs: Some(0),
            ..WorkCheck::new("x")
        };
        assert!(validate_checks(vec![slow]).is_err());
        assert!(validate_checks((0..9).map(|_| WorkCheck::new("x")).collect()).is_err());
    }
}
