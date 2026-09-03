//! Built-in Work pipeline presets.
//!
//! `[pipeline.*]` is the most structured configuration muxa asks for, and a
//! launcher on a fresh machine has nothing to offer until one exists.
//! `muxa work init` fills that gap by spending an agent turn; these presets
//! fill it without one. They are ordinary [`PipelineConfig`] values —
//! `muxa work preset apply` writes one into `config.toml`, and from then on
//! it is the operator's to edit — so nothing downstream has to tell a preset
//! from a hand-written pipeline.
//!
//! Prompts here use only the `{{work}}` placeholder. `{{request}}` is left
//! to the launcher on purpose: when no template places it, `muxa work up`
//! prepends the request whenever one is given and adds nothing when it is
//! not, whereas a template that spells it out would show a literal
//! `{{request}}` to an agent launched without a body.

use crate::config::{PipelineAgentConfig, PipelineConfig};

/// Names of the built-in presets, in the order they are offered.
pub const PRESET_NAMES: [&str; 3] = ["solo", "pair", "triad"];

/// One built-in preset: a stable name plus the pipeline it stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkPreset {
    /// The `[pipeline.<name>]` key `preset apply` writes.
    pub name: &'static str,
    pub pipeline: PipelineConfig,
}

impl WorkPreset {
    /// The pipeline's description, which every preset carries.
    #[must_use]
    pub fn description(&self) -> &str {
        self.pipeline.description.as_deref().unwrap_or_default()
    }
}

/// Every built-in preset, in [`PRESET_NAMES`] order.
#[must_use]
pub fn builtin() -> Vec<WorkPreset> {
    vec![solo(), pair(), triad()]
}

/// Look a preset up by name, case-insensitively.
#[must_use]
pub fn find(name: &str) -> Option<WorkPreset> {
    let wanted = name.trim().to_ascii_lowercase();
    builtin().into_iter().find(|preset| preset.name == wanted)
}

/// One implementer, on its own.
fn solo() -> WorkPreset {
    WorkPreset {
        name: "solo",
        pipeline: PipelineConfig {
            description: Some("one implementer".into()),
            layout: None,
            prompt: Some("You are working on {{work}}.".into()),
            agent: vec![agent(
                "claude",
                "claude",
                "implementer",
                "Implement the request",
                "You own the implementation end to end: understand the request, make the \
                 change, verify it, and summarize what you did.",
            )],
        },
    }
}

/// An implementer whose finished tree is handed to a reviewer.
fn pair() -> WorkPreset {
    let mut review = agent(
        "review",
        "codex",
        "reviewer",
        "Review the implementation",
        "You own review. The implementer has finished; critique the change for \
         correctness and regressions, and do not edit code yourself.",
    );
    review.after = vec!["impl".into()];
    review.direction = Some("down".into());
    WorkPreset {
        name: "pair",
        pipeline: PipelineConfig {
            description: Some("implementer → reviewer".into()),
            layout: None,
            prompt: Some(
                "You are working on {{work}}. The other pane in this tmux window is your \
                 peer on the same work."
                    .into(),
            ),
            agent: vec![
                agent(
                    "impl",
                    "claude",
                    "implementer",
                    "Implement the request",
                    "You own the implementation. Make the change and verify it, then run \
                     `muxa work done` so the reviewer starts on a finished tree.",
                ),
                review,
            ],
        },
    }
}

/// Planner, then implementer, then reviewer, each waiting on the last.
fn triad() -> WorkPreset {
    let mut implementer = agent(
        "impl",
        "codex",
        "implementer",
        "Implement the plan",
        "You own the implementation. Follow the plan and ask before changing scope. \
         Verify the change, then run `muxa work done` so the reviewer starts on a \
         finished tree.",
    );
    implementer.after = vec!["plan".into()];
    implementer.direction = Some("down".into());
    let mut review = agent(
        "review",
        "claude",
        "reviewer",
        "Review the implementation",
        "You own review. Critique the implementation against the plan for correctness \
         and regressions; do not edit code yourself.",
    );
    review.after = vec!["impl".into()];
    WorkPreset {
        name: "triad",
        pipeline: PipelineConfig {
            description: Some("planner → implementer → reviewer".into()),
            layout: Some("main-vertical".into()),
            prompt: Some(
                "You are working on {{work}}. The other panes in this tmux window are your \
                 peers on the same work."
                    .into(),
            ),
            agent: vec![
                agent(
                    "plan",
                    "codex",
                    "planner",
                    "Plan the approach",
                    "You own the approach. Write the plan first and do not edit code; run \
                     `muxa work done` when the plan is ready so the implementer starts from it.",
                ),
                implementer,
                review,
            ],
        },
    }
}

fn agent(alias: &str, program: &str, role: &str, task: &str, prompt: &str) -> PipelineAgentConfig {
    PipelineAgentConfig {
        alias: alias.into(),
        program: program.into(),
        role: Some(role.into()),
        task: Some(task.into()),
        prompt: Some(prompt.into()),
        direction: None,
        after: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RouteConfig;
    use crate::pipeline::{self, Vars, ALLOWLISTED_PROGRAMS};
    use std::collections::BTreeMap;

    #[test]
    fn presets_are_offered_in_a_stable_order() {
        let names: Vec<&str> = builtin().iter().map(|preset| preset.name).collect();
        assert_eq!(names, PRESET_NAMES);
        assert_eq!(find("TRIAD ").map(|preset| preset.name), Some("triad"));
        assert!(find("quartet").is_none());
    }

    #[test]
    fn every_preset_renders_with_empty_vars() {
        for preset in builtin() {
            let agents = pipeline::desired_agents(preset.name, &preset.pipeline, &Vars::new())
                .unwrap_or_else(|error| panic!("{} does not render: {error}", preset.name));
            assert_eq!(agents.len(), preset.pipeline.agent.len(), "{}", preset.name);
            assert!(!preset.description().is_empty(), "{}", preset.name);
        }
    }

    #[test]
    fn presets_use_only_allowlisted_programs() {
        for preset in builtin() {
            for agent in &preset.pipeline.agent {
                assert!(
                    ALLOWLISTED_PROGRAMS.contains(&agent.program.as_str()),
                    "{}/{} names {:?}",
                    preset.name,
                    agent.alias,
                    agent.program
                );
            }
        }
    }

    #[test]
    fn presets_place_only_work_and_request_placeholders() {
        for preset in builtin() {
            let templates = std::iter::once(preset.pipeline.prompt.as_deref())
                .chain(
                    preset
                        .pipeline
                        .agent
                        .iter()
                        .map(|agent| agent.prompt.as_deref()),
                )
                .chain(
                    preset
                        .pipeline
                        .agent
                        .iter()
                        .map(|agent| agent.task.as_deref()),
                )
                .flatten();
            for template in templates {
                for key in placeholders(template) {
                    assert!(
                        key == "work" || key == "request",
                        "{} places {{{{{key}}}}}",
                        preset.name
                    );
                }
            }
        }
    }

    #[test]
    fn solo_is_one_claude_implementer() {
        let solo = find("solo").unwrap();
        assert_eq!(solo.pipeline.agent.len(), 1);
        let agent = &solo.pipeline.agent[0];
        assert_eq!(agent.alias, "claude");
        assert_eq!(agent.program, "claude");
        assert_eq!(agent.role.as_deref(), Some("implementer"));
        assert_eq!(agent.task.as_deref(), Some("Implement the request"));
        assert!(agent.after.is_empty());
    }

    #[test]
    fn pair_sequences_a_codex_reviewer_after_the_implementer() {
        let pair = find("pair").unwrap();
        assert_eq!(pair.description(), "implementer → reviewer");
        let aliases: Vec<&str> = pair
            .pipeline
            .agent
            .iter()
            .map(|agent| agent.alias.as_str())
            .collect();
        assert_eq!(aliases, ["impl", "review"]);
        let review = &pair.pipeline.agent[1];
        assert_eq!(review.program, "codex");
        assert_eq!(review.role.as_deref(), Some("reviewer"));
        assert_eq!(review.after, ["impl"]);
        assert_eq!(review.direction.as_deref(), Some("down"));
    }

    #[test]
    fn triad_chains_plan_impl_review() {
        let triad = find("triad").unwrap();
        assert_eq!(triad.description(), "planner → implementer → reviewer");
        assert_eq!(triad.pipeline.layout.as_deref(), Some("main-vertical"));
        let agents = pipeline::desired_agents("triad", &triad.pipeline, &Vars::new()).unwrap();
        assert_eq!(agents[0].alias, "plan");
        assert_eq!(agents[0].program, "codex");
        assert!(agents[0].after.is_empty());
        assert_eq!(agents[1].alias, "impl");
        assert_eq!(agents[1].program, "codex");
        assert_eq!(agents[1].after, ["plan"]);
        assert_eq!(agents[1].direction.as_deref(), Some("down"));
        assert_eq!(agents[2].alias, "review");
        assert_eq!(agents[2].program, "claude");
        assert_eq!(agents[2].after, ["impl"]);
    }

    #[test]
    fn every_preset_survives_a_config_round_trip() {
        #[derive(serde::Serialize)]
        struct Snippet<'a> {
            route: Vec<RouteConfig>,
            pipeline: BTreeMap<&'a str, &'a PipelineConfig>,
        }
        for preset in builtin() {
            let snippet = Snippet {
                route: vec![RouteConfig {
                    pattern: ".*".into(),
                    pipeline: Some(preset.name.into()),
                    ..RouteConfig::default()
                }],
                pipeline: BTreeMap::from([(preset.name, &preset.pipeline)]),
            };
            let text = toml::to_string(&snippet).unwrap();
            let (config, summary) = pipeline::validate_proposal(&text)
                .unwrap_or_else(|error| panic!("{} is refused: {error}\n{text}", preset.name));
            assert_eq!(config.pipeline[preset.name], preset.pipeline);
            assert_eq!(
                summary.pipelines,
                vec![(preset.name.to_string(), preset.pipeline.agent.len())]
            );
        }
    }

    fn placeholders(template: &str) -> Vec<String> {
        let mut keys = Vec::new();
        let mut rest = template;
        while let Some(open) = rest.find("{{") {
            let after = &rest[open..];
            let Some(close) = after.find("}}") else { break };
            keys.push(after[2..close].trim().to_string());
            rest = &after[close + 2..];
        }
        keys
    }
}
