//! Composing one agent-facing request out of a registered skill, a body,
//! and bounded context.
//!
//! Two surfaces hand an instruction to an agent: `muxa_call_peer` asks a
//! peer that already exists, and `muxa work up` staffs a work window with
//! agents that may not exist yet. They differ in every other respect —
//! one carries a permission contract, the other carries a pipeline — but
//! the way a caller *phrases* the instruction should not be one of those
//! differences. So the phrasing lives here once:
//!
//! ```text
//! <skill expansion>            registered [message.skills] entry
//! <body>                       the request-specific instruction
//! Invocation context:          bounded extra context, labelled so the
//! <context>                    agent can tell it from the instruction
//! ```
//!
//! joined by blank lines, any part optional. Nothing else is attached:
//! muxa never folds in a transcript or a diff on the caller's behalf,
//! because a request the caller cannot see the whole of is one they
//! cannot be responsible for.

use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
pub enum RequestError {
    #[error("message skill /{name} is not registered{}", available_suffix(.available))]
    UnknownSkill {
        name: String,
        available: Vec<String>,
    },
}

fn available_suffix(available: &[String]) -> String {
    if available.is_empty() {
        return String::new();
    }
    format!(
        "; available: {}",
        available
            .iter()
            .map(|name| format!("/{name}"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// The three parts a caller may supply, before lookup or joining.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestParts<'a> {
    /// Registered `[message.skills]` name, with or without a leading `/`.
    pub skill: Option<&'a str>,
    /// The request-specific instruction.
    pub body: Option<&'a str>,
    /// Bounded extra context, labelled in the output.
    pub context: Option<&'a str>,
}

/// One composed request, plus the skill name that was expanded into it so
/// callers can echo which template ran.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ComposedRequest {
    pub text: String,
    /// `/name` when a skill was expanded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
}

/// Expand the skill, join the parts, and return `None` when the caller
/// supplied nothing at all.
///
/// `None` rather than an error because "nothing to say" is legitimate for
/// some callers (`muxa work up` on a ticket-driven pipeline) and a hard
/// error for others (`muxa_call_peer`). The caller that needs it to be an
/// error owns that message.
///
/// # Errors
/// [`RequestError::UnknownSkill`] when `skill` names no registered entry.
/// The error lists what is registered, because a caller that guessed a
/// skill name needs the real ones more than it needs a restatement of the
/// wrong one.
pub fn compose(
    parts: RequestParts<'_>,
    skills: &BTreeMap<String, String>,
) -> Result<Option<ComposedRequest>, RequestError> {
    let name = parts
        .skill
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| name.trim_start_matches('/'));
    let skill = match name {
        Some(name) => {
            let prompt = skills
                .get(name)
                .or_else(|| {
                    skills
                        .iter()
                        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
                        .map(|(_, prompt)| prompt)
                })
                .ok_or_else(|| RequestError::UnknownSkill {
                    name: name.to_string(),
                    available: skills.keys().cloned().collect(),
                })?;
            Some((name.to_string(), prompt.trim().to_string()))
        }
        None => None,
    };
    let body = parts.body.map(str::trim).filter(|body| !body.is_empty());
    let context = parts
        .context
        .map(str::trim)
        .filter(|context| !context.is_empty());

    let mut sections = Vec::new();
    if let Some((_, prompt)) = skill.as_ref() {
        sections.push(prompt.clone());
    }
    if let Some(body) = body {
        sections.push(body.to_string());
    }
    if let Some(context) = context {
        sections.push(format!("Invocation context:\n{context}"));
    }
    if sections.is_empty() {
        return Ok(None);
    }
    Ok(Some(ComposedRequest {
        text: sections.join("\n\n"),
        skill: skill.map(|(name, _)| format!("/{name}")),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skills() -> BTreeMap<String, String> {
        BTreeMap::from([
            (
                "review-plan-feedback".to_string(),
                "Run the iterative peer review workflow.".to_string(),
            ),
            ("review".to_string(), "review it".to_string()),
        ])
    }

    #[test]
    fn all_three_parts_join_in_a_fixed_order() {
        let composed = compose(
            RequestParts {
                skill: Some("/review-plan-feedback"),
                body: Some("Review commit abc123."),
                context: Some("Tests already passed."),
            },
            &skills(),
        )
        .unwrap()
        .expect("composed");
        assert_eq!(composed.skill.as_deref(), Some("/review-plan-feedback"));
        assert_eq!(
            composed.text,
            "Run the iterative peer review workflow.\n\nReview commit abc123.\n\nInvocation context:\nTests already passed."
        );
    }

    #[test]
    fn a_skill_name_resolves_with_or_without_the_slash_and_ignores_case() {
        for name in ["review", "/review", "REVIEW"] {
            let composed = compose(
                RequestParts {
                    skill: Some(name),
                    ..RequestParts::default()
                },
                &skills(),
            )
            .unwrap()
            .expect("composed");
            assert_eq!(composed.text, "review it");
        }
    }

    #[test]
    fn an_unknown_skill_lists_the_registered_names() {
        let error = compose(
            RequestParts {
                skill: Some("missing"),
                ..RequestParts::default()
            },
            &skills(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("/missing"), "{error}");
        assert!(error.contains("/review"), "{error}");
    }

    #[test]
    fn nothing_supplied_is_nothing_to_say_not_an_error() {
        assert!(compose(RequestParts::default(), &skills())
            .unwrap()
            .is_none());
        // Whitespace-only parts are the same as absent.
        assert!(compose(
            RequestParts {
                skill: Some("  "),
                body: Some("\n"),
                context: Some(" ")
            },
            &skills()
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn a_body_alone_is_carried_verbatim() {
        let composed = compose(
            RequestParts {
                body: Some("  fix the reaper  "),
                ..RequestParts::default()
            },
            &skills(),
        )
        .unwrap()
        .expect("composed");
        assert_eq!(composed.text, "fix the reaper");
        assert!(composed.skill.is_none());
    }

    #[test]
    fn context_is_labelled_so_it_cannot_be_read_as_the_instruction() {
        let composed = compose(
            RequestParts {
                body: Some("do the thing"),
                context: Some("branch is feat/x"),
                ..RequestParts::default()
            },
            &skills(),
        )
        .unwrap()
        .expect("composed");
        assert_eq!(
            composed.text,
            "do the thing\n\nInvocation context:\nbranch is feat/x"
        );
    }
}
