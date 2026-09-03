//! `muxa work compose` — draft one pipeline from a sentence and print it
//! as the JSON `muxa work pipeline set --from-json` reads. Writes nothing.
//!
//! The daemon's `work_compose` request and this command share
//! [`muxa::work_compose`]: the same prompt, the same JSON extraction, the
//! same launch-time validation, the same single retry. This one runs the
//! turn in-process with `[ask]`'s provider settings, so it works without a
//! running daemon and without the `[ask].enabled` grant — typing the
//! command is the consent, as with `muxa work init`.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

use muxa::config::{AskPermissionMode, Config};
use muxa::work_compose::{self, WorkComposeRequest};
use muxa::work_pipeline_spec::PipelineSpec;

use crate::work_up::expand_tilde;

#[derive(Debug, clap::Args)]
pub struct ComposeArgs {
    /// What you want, in your own words. With `--current` it is the change
    /// to make to that draft.
    pub description: String,
    /// Provider to draft with: claude, codex, gemini, anthropic, or openai.
    /// Defaults to `[ask].agent`.
    #[arg(long)]
    pub agent: Option<String>,
    /// A previous draft to refine: a JSON file in the shape `muxa work
    /// options --json` prints for one pipeline, or `-` to read it from
    /// stdin.
    #[arg(long, value_name = "PATH")]
    pub current: Option<PathBuf>,
    /// Print the whole response — `pipeline`, `notes`, `raw` — as JSON,
    /// exactly as the daemon's `work_compose` request answers.
    #[arg(long)]
    pub json: bool,
}

pub async fn run(args: ComposeArgs, config: &Config) -> Result<()> {
    let current = args.current.as_deref().map(read_current).transpose()?;
    let agent = args
        .agent
        .clone()
        .unwrap_or_else(|| config.ask.agent.clone());
    let cwd = config
        .ask
        .cwd
        .clone()
        .map(|cwd| PathBuf::from(expand_tilde(&cwd.to_string_lossy())))
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    let request = WorkComposeRequest {
        description: args.description.clone(),
        agent: Some(agent.clone()),
        current,
    };
    let installed = work_compose::installed_programs();
    // Say what is about to be spent before spending it — on stderr, so
    // `--json` stdout stays parseable.
    eprintln!("drafting with {agent} (one read-only turn, billed to that provider's account)…");

    let provider_config = config.ask.providers.get(&agent).cloned();
    let additional_dirs = config.ask.additional_dirs.clone();
    let timeout = Duration::from_secs(config.ask.timeout_secs.max(60));
    let output = work_compose::compose(&request, &installed, |prompt| {
        let agent = agent.clone();
        let cwd = cwd.clone();
        let additional_dirs = additional_dirs.clone();
        let provider_config = provider_config.clone();
        async move {
            muxa::ask::one_shot_configured(
                muxa::ask::OneShot {
                    agent: &agent,
                    prompt: &prompt,
                    cwd: &cwd,
                    // Drafting never edits files, whatever `[ask]` allows.
                    permission_mode: AskPermissionMode::Plan,
                    additional_dirs: &additional_dirs,
                    timeout,
                },
                provider_config.as_ref(),
            )
            .await
            .map_err(|error| error.to_string())
        }
    })
    .await
    .context("drafting the pipeline")?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        if !output.notes.is_empty() {
            println!("{}\n", output.notes);
        }
        println!("{}", serde_json::to_string_pretty(&output.pipeline)?);
        eprintln!(
            "write it with:  muxa work pipeline set {} --from-json <file>",
            output.pipeline.name.as_deref().unwrap_or("<name>")
        );
    }
    Ok(())
}

/// The draft to refine, from a file or stdin (`-`).
fn read_current(source: &Path) -> Result<PipelineSpec> {
    let text = if source == Path::new("-") {
        std::io::read_to_string(std::io::stdin()).context("reading the current draft from stdin")?
    } else {
        std::fs::read_to_string(source).with_context(|| format!("reading {}", source.display()))?
    };
    serde_json::from_str(&text).context("parsing the current draft as pipeline JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_current_draft_is_read_in_the_pipeline_set_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pair.json");
        std::fs::write(
            &path,
            r#"{"name":"pair","description":null,"layout":null,"prompt":null,
                "agents":[{"alias":"impl","program":"claude","role":null,"task":null,
                           "prompt":null,"direction":null,"after":[]}]}"#,
        )
        .unwrap();
        let spec = read_current(&path).unwrap();
        assert_eq!(spec.name.as_deref(), Some("pair"));
        assert_eq!(spec.agents[0].alias, "impl");

        std::fs::write(&path, r#"{"agent": []}"#).unwrap();
        let error = format!("{:#}", read_current(&path).unwrap_err());
        assert!(error.contains("unknown field `agent`"), "{error}");
        assert!(read_current(Path::new("/nonexistent/draft.json")).is_err());
    }
}
