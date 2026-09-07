//! The pipeline JSON shape editors and the daemon exchange, and the checks
//! a pipeline has to pass before it is written or launched.
//!
//! `muxa work options --json` emits one of these per `[pipeline.<name>]`,
//! `muxa work pipeline set --from-json` reads one back, and `work_compose`
//! drafts one from a description. All three run the same validation from
//! here — allowlisted programs, unique aliases, `after` edges that resolve
//! and do not cycle, a name TOML can use as a bare key — so the CLI and the
//! daemon cannot drift on what counts as a launchable pipeline.

use serde::{Deserialize, Serialize};

use crate::config::{PipelineAgentConfig, PipelineConfig};
use crate::pipeline::{self, PipelineError, Vars, ALLOWLISTED_PROGRAMS};

/// A pipeline as the editor sends it: one `pipelines[]` entry of
/// `muxa work options --json`. `name` is tolerated so an entry can be
/// handed back verbatim, but it has to agree with the command line when a
/// command line names the pipeline.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PipelineSpec {
    pub name: Option<String>,
    pub description: Option<String>,
    pub layout: Option<String>,
    pub prompt: Option<String>,
    pub agents: Vec<AgentSpec>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSpec {
    pub alias: String,
    pub program: String,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub task: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub direction: Option<String>,
    #[serde(default)]
    pub after: Vec<String>,
}

impl PipelineSpec {
    /// Project a configured pipeline onto the wire shape, key for key.
    #[must_use]
    pub fn from_config(name: &str, pipeline: &PipelineConfig) -> Self {
        Self {
            name: Some(name.to_string()),
            description: pipeline.description.clone(),
            layout: pipeline.layout.clone(),
            prompt: pipeline.prompt.clone(),
            agents: pipeline
                .agent
                .iter()
                .map(|agent| AgentSpec {
                    alias: agent.alias.clone(),
                    program: agent.program.clone(),
                    role: agent.role.clone(),
                    task: agent.task.clone(),
                    prompt: agent.prompt.clone(),
                    direction: agent.direction.clone(),
                    after: agent.after.clone(),
                })
                .collect(),
        }
    }
}

impl From<PipelineSpec> for PipelineConfig {
    fn from(spec: PipelineSpec) -> Self {
        PipelineConfig {
            description: spec.description,
            layout: spec.layout,
            prompt: spec.prompt,
            agent: spec
                .agents
                .into_iter()
                .map(|agent| PipelineAgentConfig {
                    alias: agent.alias,
                    program: agent.program,
                    role: agent.role,
                    task: agent.task,
                    prompt: agent.prompt,
                    direction: agent.direction,
                    after: agent.after,
                })
                .collect(),
        }
    }
}

/// Why a pipeline was refused. The launcher's own rules come through
/// [`PipelineError`] unchanged so the text matches what a hand-written
/// config would produce at `muxa work up`.
#[derive(Debug, thiserror::Error)]
pub enum PipelineSpecError {
    #[error(
        "pipeline name {0:?} cannot be a [pipeline.<name>] key; use letters, digits, `-`, and `_`"
    )]
    BadName(String),
    #[error("the JSON names pipeline {given:?} but the command line says {expected:?}; drop `name` from the JSON or make them agree")]
    NameMismatch { given: String, expected: String },
    #[error("pipeline {pipeline:?} agent #{index} has no alias")]
    MissingAlias { pipeline: String, index: usize },
    #[error("pipeline {pipeline:?} agent {alias:?}: {message}")]
    BadDirection {
        pipeline: String,
        alias: String,
        message: String,
    },
    #[error(transparent)]
    Pipeline(#[from] PipelineError),
}

/// A pipeline name has to work as a TOML bare key: `[pipeline.<name>]`.
pub fn check_name(name: &str) -> Result<(), PipelineSpecError> {
    let bare = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if bare {
        Ok(())
    } else {
        Err(PipelineSpecError::BadName(name.to_string()))
    }
}

/// The split direction a pane joins with: `auto` (the default, which splits
/// along the target pane's longer side), `right`, or `down`, with tmux's
/// `horizontal`/`vertical` accepted as spellings. Returns the canonical name.
pub fn parse_direction(value: Option<&str>) -> Result<&'static str, String> {
    match value.unwrap_or("auto").trim().to_ascii_lowercase().as_str() {
        "auto" | "" => Ok("auto"),
        "right" | "horizontal" => Ok("right"),
        "down" | "vertical" => Ok("down"),
        other => Err(format!(
            "unknown direction {other:?}; expected auto, right, or down"
        )),
    }
}

/// The checks `muxa work up` would fail at launch, run while the pipeline
/// is still just data: an agent with no alias, a program that is not an
/// agent CLI, a split direction tmux has no name for, and — through the
/// launcher's own renderer — an empty line-up, a duplicate alias, an edge
/// to nobody, or a cycle.
pub fn check_pipeline(name: &str, pipeline: &PipelineConfig) -> Result<(), PipelineSpecError> {
    for (index, agent) in pipeline.agent.iter().enumerate() {
        if agent.alias.trim().is_empty() {
            return Err(PipelineSpecError::MissingAlias {
                pipeline: name.to_string(),
                index: index + 1,
            });
        }
        let program = agent.program.trim().to_ascii_lowercase();
        if !ALLOWLISTED_PROGRAMS.contains(&program.as_str()) {
            return Err(PipelineError::UnknownProgram {
                pipeline: name.to_string(),
                alias: agent.alias.clone(),
                program: agent.program.clone(),
            }
            .into());
        }
        if let Some(direction) = agent.direction.as_deref() {
            parse_direction(Some(direction)).map_err(|message| {
                PipelineSpecError::BadDirection {
                    pipeline: name.to_string(),
                    alias: agent.alias.clone(),
                    message,
                }
            })?;
        }
    }
    pipeline::desired_agents(name, pipeline, &Vars::new())?;
    Ok(())
}

/// Validate `spec` as `[pipeline.<name>]`: the name, an optional JSON
/// `name` that has to agree, and every launch-time rule. Returns the
/// config the pipeline would be written as.
pub fn validate_spec(name: &str, spec: &PipelineSpec) -> Result<PipelineConfig, PipelineSpecError> {
    check_name(name)?;
    if let Some(given) = spec.name.as_deref() {
        if given != name {
            return Err(PipelineSpecError::NameMismatch {
                given: given.to_string(),
                expected: name.to_string(),
            });
        }
    }
    let pipeline = PipelineConfig::from(spec.clone());
    check_pipeline(name, &pipeline)?;
    Ok(pipeline)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn spec(value: Value) -> PipelineSpec {
        serde_json::from_value(value).unwrap()
    }

    fn pair() -> Value {
        json!({
            "description": "implementer → reviewer",
            "layout": null,
            "prompt": "You are working on {{work}}.",
            "agents": [
                {"alias": "impl", "program": "claude", "role": "implementer",
                 "task": "Implement", "prompt": "Own the change.", "direction": null, "after": []},
                {"alias": "review", "program": "codex", "role": "reviewer",
                 "task": null, "prompt": null, "direction": "down", "after": ["impl"]}
            ]
        })
    }

    #[test]
    fn names_have_to_be_bare_toml_keys() {
        for good in ["triad", "solo-1_a", "A"] {
            assert!(check_name(good).is_ok(), "{good}");
        }
        for bad in ["", "pair.two", "pa ir", "é"] {
            let error = check_name(bad).unwrap_err().to_string();
            assert!(
                error.contains("cannot be a [pipeline.<name>] key"),
                "{bad}: {error}"
            );
        }
    }

    #[test]
    fn directions_default_to_auto_and_accept_either_spelling() {
        assert_eq!(parse_direction(None).unwrap(), "auto");
        assert_eq!(parse_direction(Some("auto")).unwrap(), "auto");
        assert_eq!(parse_direction(Some(" Horizontal ")).unwrap(), "right");
        assert_eq!(parse_direction(Some("vertical")).unwrap(), "down");
        assert_eq!(parse_direction(Some("down")).unwrap(), "down");
        assert_eq!(
            parse_direction(Some("left")).unwrap_err(),
            "unknown direction \"left\"; expected auto, right, or down"
        );
    }

    #[test]
    fn a_valid_spec_becomes_the_pipeline_it_would_write() {
        let pipeline = validate_spec("pair", &spec(pair())).unwrap();
        assert_eq!(pipeline.agent.len(), 2);
        assert_eq!(pipeline.agent[1].after, ["impl"]);
        assert_eq!(
            pipeline.description.as_deref(),
            Some("implementer → reviewer")
        );
        // A JSON name that agrees is fine; so is a matching config round trip.
        let mut named = pair();
        named["name"] = json!("pair");
        assert!(validate_spec("pair", &spec(named)).is_ok());
        let back = PipelineSpec::from_config("pair", &pipeline);
        assert_eq!(back.name.as_deref(), Some("pair"));
        assert_eq!(back.agents.len(), 2);
        assert_eq!(PipelineConfig::from(back), pipeline);
    }

    #[test]
    fn every_launch_rule_is_enforced_with_the_launchers_words() {
        let refused = |name: &str, value: Value, expected: &str| {
            let error = validate_spec(name, &spec(value)).unwrap_err().to_string();
            assert!(error.contains(expected), "{name}: {error}");
        };

        let mut bad_program = pair();
        bad_program["agents"][0]["program"] = json!("vim");
        refused("pair", bad_program, "not an allowlisted agent CLI");

        let mut duplicate = pair();
        duplicate["agents"][1]["alias"] = json!("IMPL");
        refused("pair", duplicate, "uses alias \"impl\" twice");

        let mut dangling = pair();
        dangling["agents"][1]["after"] = json!(["plan"]);
        refused("pair", dangling, "waits on \"plan\", which is not an alias");

        let mut cycle = pair();
        cycle["agents"][0]["after"] = json!(["review"]);
        refused("pair", cycle, "has a cycle");

        let mut empty = pair();
        empty["agents"] = json!([]);
        refused("pair", empty, "declares no agents");

        let mut blank_alias = pair();
        blank_alias["agents"][0]["alias"] = json!("  ");
        refused("pair", blank_alias, "agent #1 has no alias");

        let mut sideways = pair();
        sideways["agents"][1]["direction"] = json!("left");
        refused("pair", sideways, "unknown direction \"left\"");

        let mut renamed = pair();
        renamed["name"] = json!("other");
        refused("pair", renamed, "names pipeline \"other\"");

        refused("pair.two", pair(), "cannot be a [pipeline.<name>] key");

        let typo: Result<PipelineSpec, _> =
            serde_json::from_value(json!({"agent": [{"alias": "x", "program": "claude"}]}));
        assert!(typo
            .unwrap_err()
            .to_string()
            .contains("unknown field `agent`"));
    }

    #[test]
    fn the_wire_shape_serializes_every_key_with_nulls_for_absent_ones() {
        let value = serde_json::to_value(spec(json!({
            "name": "solo",
            "agents": [{"alias": "x", "program": "claude"}]
        })))
        .unwrap();
        assert_eq!(
            value,
            json!({
                "name": "solo", "description": null, "layout": null, "prompt": null,
                "agents": [{"alias": "x", "program": "claude", "role": null, "task": null,
                            "prompt": null, "direction": null, "after": []}]
            })
        );
    }
}
