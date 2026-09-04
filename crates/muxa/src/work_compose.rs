//! `work_compose` — draft one pipeline from a sentence.
//!
//! The visual editor and `muxa work compose` share this: the operator
//! says "implementer in claude, reviewer in codex after it", one headless
//! turn turns that into the pipeline JSON `muxa work pipeline set` reads,
//! and muxa's job is the part that does *not* trust the answer — pull the
//! JSON out, parse it, run every launch-time check, and ask once more with
//! the error text when the first draft would not launch. Nothing here
//! writes config; the caller shows the draft and decides.
//!
//! The drafting turn runs read-only (`AskPermissionMode::Plan`) whatever
//! `[ask]` says, because a model asked to *describe* a pipeline has no
//! business editing files while it thinks.
//!
//! `muxa work init` borrows the schema text and prompt builder from here
//! too, so the two ways of asking an agent for pipeline config cannot
//! drift on what the config looks like.

use serde::{Deserialize, Serialize};
use std::future::Future;

use crate::ask::AskAnswer;
use crate::pipeline::ALLOWLISTED_PROGRAMS;
use crate::work_pipeline_spec::{validate_spec, PipelineSpec, PipelineSpecError};

/// What the model is told about the `[ticket]`/`[[route]]`/`[pipeline.*]`
/// TOML `muxa work init` writes. Kept in code rather than the docs
/// directory so the two cannot drift: if the schema changes, this is in
/// the same diff.
pub const CONFIG_SCHEMA: &str = r#"
muxa work pipeline configuration. Three top-level keys, all optional except
as noted:

[ticket]                      # how a work id becomes ticket context
agent = "claude"              # or "codex" — the resolver CLI
cwd = "~"                     # where the resolver runs
timeout_secs = 300
cache_secs = 900              # 0 disables the cache
additional_dirs = ["/path"]   # extra roots the resolver may read

[ticket.source.<name>]        # tried in sorted-key order, first match wins
match = '^cal-\d+$'           # regex against the work id, case-insensitive
prompt = '''...'''            # asks an agent to answer with ticket JSON.
                              # {{id}} is the lowercased work id. muxa reads
                              # id/identifier/key, title/name/summary,
                              # body/description, url, state, branch.

[[route]]                     # REQUIRED: ordered, first match wins
match     = '^cal-'           # regex against the work id
workspace = 'callabo'         # the tmux session; defaults to the cwd name
pipeline  = 'triad'           # must name a [pipeline.*] below
cwd       = '~/src/{{id}}'    # optional; omit to use the current directory
prepare   = 'mk-ws {{id}} {{ticket.branch}}'
                              # optional: command that provisions this work's
                              # environment, run once when the work window does
                              # not exist yet. Pair it with `cwd`, since the
                              # directory usually does not exist until it has
                              # run. Cannot be combined with [route.worktree].
[route.worktree]              # optional: a git worktree per work item.
repo   = '~/src/repo'         # Use this OR prepare, never both.
branch = '{{id}}'

[pipeline.<name>]             # REQUIRED: at least one
layout = 'main-vertical'      # tmux layout, applied once every pane exists
prompt = '''...'''            # context every agent in this pipeline gets

[[pipeline.<name>.agent]]     # one per pane, at least one
alias   = 'impl'              # unique within the pipeline; keys the pane diff
program = 'codex'             # ONLY claude, codex, gemini, agy, or opencode
role    = 'implementer'       # optional; peers address it as role:<role>
task    = 'fix the reaper'    # optional; short label in `muxa work show`/`watch`
prompt  = '...'               # optional; this agent's own instructions
direction = 'auto'            # optional: auto (default), right, or down
after   = ['impl']            # optional: aliases that must report finishing
                              # before this one starts. Omit for work that is
                              # genuinely parallel; use it when one agent must
                              # not see a tree the other is still changing —
                              # a reviewer after its implementer, say. The
                              # upstream agent opens the edge by running
                              # `muxa work done` from its own pane, so tell it
                              # to in that agent's prompt.

Placeholders, usable in any prompt/path/workspace string:
{{id}} lowercased work id, {{work}} as muxa stores it, {{workspace}},
{{cwd}}, {{alias}}, {{role}}, {{program}}, {{request}} (the caller's
--body/--skill/--context), and {{ticket.title|body|url|state|id|branch}}.

Unknown keys are a hard error, so do not invent any.
"#;

/// The JSON shape of one pipeline, as the model is shown it. The program
/// list is rendered from the launcher's allowlist so it cannot drift.
#[must_use]
pub fn pipeline_json_schema() -> String {
    let programs = ALLOWLISTED_PROGRAMS.join(", ");
    format!(
        r#"One muxa work pipeline as JSON — the shape `muxa work options --json`
emits and `muxa work pipeline set --from-json` reads:

{{
  "name": "triad",                    // REQUIRED: letters, digits, - and _ only (a TOML bare key)
  "description": "plan → implement → review",   // or null
  "layout": "main-vertical",          // tmux layout applied once every pane exists, or null
  "prompt": "context every agent in this pipeline gets",   // or null
  "agents": [                         // REQUIRED: one per pane, at least one
    {{
      "alias": "impl",                // REQUIRED, unique within the pipeline
      "program": "codex",             // REQUIRED: ONLY {programs}
      "role": "implementer",          // or null; peers address it as role:<role>
      "task": "fix the reaper",       // or null; short label in `muxa work show`/`watch`
      "prompt": "this agent's own instructions",   // or null
      "direction": "auto",            // "auto" (default), "right", "down", or null
      "after": ["plan"]               // aliases that must report finishing first; [] for parallel work
    }}
  ]
}}

Use `after` when one agent must not see a tree another is still changing —
a reviewer after its implementer, say. The upstream agent opens the edge by
running `muxa work done` from its own pane, so tell it to in that agent's
prompt. Placeholders usable in any prompt: {{{{id}}}} lowercased work id,
{{{{work}}}} as muxa stores it, {{{{workspace}}}}, {{{{cwd}}}}, {{{{alias}}}}, {{{{role}}}},
{{{{program}}}}, {{{{request}}}} (the caller's body/skill/context), and
{{{{ticket.title|body|url|state|id|branch}}}}.

No other keys exist; an unknown key is a hard error, so do not invent any.
"#
    )
}

/// The prompt `muxa work init` sends: the TOML schema, the current file,
/// which agent programs are installed, and what the operator asked for.
#[must_use]
pub fn config_prompt(describe: &str, existing: &str, installed: &[String]) -> String {
    // The current file goes in so the model extends what is there rather
    // than proposing a config that contradicts it — and so it can see which
    // of the three keys already exist.
    let current = if existing.trim().is_empty() {
        "(the config file is empty or absent)".to_string()
    } else {
        format!("Current config.toml:\n```toml\n{}\n```", existing.trim())
    };
    format!(
        "You are writing muxa work pipeline configuration.\n\n\
         SCHEMA\n{CONFIG_SCHEMA}\n\n\
         {}\n\n\
         {current}\n\n\
         WHAT THE OPERATOR WANTS\n{describe}\n\n\
         Answer with ONE ```toml block containing only the [ticket], [[route]], and\n\
         [pipeline.*] sections. Do not repeat other sections of the current config.\n\
         Include at least one [[route]] and one [pipeline.*]. End routes with a\n\
         catch-all `match = '.*'` unless the operator said otherwise. Prefer omitting\n\
         `cwd` so the work runs where the operator invoked it. No prose outside the\n\
         block.",
        installed_section(installed)
    )
}

fn installed_section(installed: &[String]) -> String {
    if installed.is_empty() {
        "INSTALLED AGENT PROGRAMS\n(none of the allowlisted agent CLIs was found on PATH; \
         prefer claude and say so)"
            .to_string()
    } else {
        format!(
            "INSTALLED AGENT PROGRAMS\n{} — prefer these for `program`; any other \
             allowlisted program would fail to launch on this machine.",
            installed.join(", ")
        )
    }
}

/// A request to draft, or refine, one pipeline.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkComposeRequest {
    /// What the operator wants, in their own words. With `current` set it
    /// is the change request against that draft.
    pub description: String,
    /// Provider instance id to draft with — a built-in or one the
    /// operator added; `None` means the ask store's selected one.
    #[serde(default)]
    pub agent: Option<String>,
    /// A previous draft to refine.
    #[serde(default)]
    pub current: Option<PipelineSpec>,
}

/// The draft, validated, plus what the model said around it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkComposeOutput {
    pub pipeline: PipelineSpec,
    /// The answer text outside the JSON block, trimmed; may be empty.
    pub notes: String,
    /// The whole answer, for a client that wants to show it.
    pub raw: String,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkComposeError {
    #[error("describe the pipeline you want; the description is empty")]
    EmptyDescription,
    #[error("the draft carried no JSON object; the reply began: {0}")]
    NoJson(String),
    #[error("the draft is not pipeline JSON: {0}")]
    BadJson(String),
    #[error("the draft has no pipeline name")]
    MissingName,
    #[error("the draft would not launch: {0}")]
    Invalid(#[from] PipelineSpecError),
    /// The provider turn itself failed; there is nothing to retry with.
    #[error("{0}")]
    Turn(String),
}

/// Draft the pipeline `request` describes. `ask` runs one headless turn
/// for a prompt and answers with the text (the caller picks the provider,
/// its credentials, and the read-only permission mode); it is called a
/// second time, with the rejection appended, when the first draft does
/// not survive validation.
///
/// # Errors
/// [`WorkComposeError::Turn`] when a provider turn fails, otherwise the
/// reason the *second* draft was rejected.
pub async fn compose<F, Fut>(
    request: &WorkComposeRequest,
    installed: &[String],
    ask: F,
) -> Result<WorkComposeOutput, WorkComposeError>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Result<AskAnswer, String>>,
{
    let description = request.description.trim();
    if description.is_empty() {
        return Err(WorkComposeError::EmptyDescription);
    }
    let prompt = compose_prompt(description, request.current.as_ref(), installed, None);
    let answer = ask(prompt).await.map_err(WorkComposeError::Turn)?;
    let rejected = match interpret(&answer.text) {
        Ok(output) => return Ok(output),
        Err(error) => error,
    };
    let retry = compose_prompt(
        description,
        request.current.as_ref(),
        installed,
        Some(&Rejection {
            draft: &answer.text,
            error: &rejected.to_string(),
        }),
    );
    let answer = ask(retry).await.map_err(WorkComposeError::Turn)?;
    interpret(&answer.text)
}

/// What the previous attempt answered and why it was refused.
pub struct Rejection<'a> {
    pub draft: &'a str,
    pub error: &'a str,
}

/// The drafting prompt: the JSON schema, the installed programs, the
/// current draft when refining, the description, and — on the retry — the
/// previous answer with the reason it was refused.
#[must_use]
pub fn compose_prompt(
    description: &str,
    current: Option<&PipelineSpec>,
    installed: &[String],
    rejection: Option<&Rejection<'_>>,
) -> String {
    let mut prompt = format!(
        "You are drafting ONE muxa work pipeline as JSON. Do not run commands and do not \
         edit files; answer from the description alone.\n\n\
         SCHEMA\n{}\n\n{}\n\n",
        pipeline_json_schema(),
        installed_section(installed)
    );
    if let Some(current) = current {
        let json = serde_json::to_string_pretty(current).unwrap_or_default();
        prompt.push_str(
            "THE CURRENT DRAFT\nRefine this rather than starting over; the request below is \
             the change the operator wants. Keep its name unless asked to change it.\n",
        );
        prompt.push_str("```json\n");
        prompt.push_str(&json);
        prompt.push_str("\n```\n\n");
    }
    prompt.push_str("WHAT THE OPERATOR WANTS\n");
    prompt.push_str(description);
    prompt.push_str("\n\n");
    if let Some(rejection) = rejection {
        prompt.push_str("YOUR PREVIOUS ANSWER WAS REJECTED\n");
        prompt.push_str(rejection.draft.trim());
        prompt.push_str("\n\nBECAUSE\n");
        prompt.push_str(rejection.error);
        prompt.push_str("\n\nFix exactly that and answer again.\n\n");
    }
    prompt.push_str(
        "Answer with ONE ```json block holding the pipeline object and nothing else inside \
         the block. Choose a short `name` if none was given. Keep any prose outside the \
         block to a sentence or two.",
    );
    prompt
}

/// Turn one answer into a validated draft: find the JSON, parse it, name
/// it, and run the launch-time checks.
pub fn interpret(reply: &str) -> Result<WorkComposeOutput, WorkComposeError> {
    let (json, notes) = extract_json_object(reply).ok_or_else(|| {
        WorkComposeError::NoJson(reply.trim().chars().take(80).collect::<String>())
    })?;
    let mut spec: PipelineSpec =
        serde_json::from_str(json).map_err(|error| WorkComposeError::BadJson(error.to_string()))?;
    let name = spec
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or(WorkComposeError::MissingName)?
        .to_string();
    validate_spec(&name, &spec)?;
    spec.name = Some(name);
    Ok(WorkComposeOutput {
        pipeline: spec,
        notes,
        raw: reply.to_string(),
    })
}

/// The JSON object in a reply and the text around it.
///
/// Prefers a fenced `json` (or untagged) code block whose body is an
/// object, because that is what the prompt asks for and what models
/// reliably produce. Falls back to the first balanced `{…}` in the reply
/// so a model that answered with bare JSON still works. The second value
/// is the reply with the block (fences included) removed and trimmed.
#[must_use]
pub fn extract_json_object(reply: &str) -> Option<(&str, String)> {
    let mut rest = reply;
    let mut offset = 0usize;
    while let Some(open) = rest.find("```") {
        let after = &rest[open + 3..];
        let Some((tag, body)) = after.split_once('\n') else {
            break;
        };
        let body_start = offset + open + 3 + tag.len() + 1;
        let tag = tag.trim();
        let tagged_json = tag.is_empty() || tag.eq_ignore_ascii_case("json");
        if let Some(close) = body.find("```") {
            let block = &body[..close];
            if tagged_json && block.trim_start().starts_with('{') {
                let before = &reply[..offset + open];
                let after_close = &reply[body_start + close + 3..];
                return Some((block.trim(), notes(before, after_close)));
            }
            offset = body_start + close + 3;
            rest = &reply[offset..];
        } else {
            // Unterminated fence: take what is there rather than nothing.
            if tagged_json && body.trim_start().starts_with('{') {
                return Some((body.trim(), notes(&reply[..offset + open], "")));
            }
            break;
        }
    }
    let start = reply.find('{')?;
    let end = balanced_object_end(&reply[start..])?;
    let json = &reply[start..start + end];
    Some((json, notes(&reply[..start], &reply[start + end..])))
}

fn notes(before: &str, after: &str) -> String {
    let before = before.trim();
    let after = after.trim();
    match (before.is_empty(), after.is_empty()) {
        (true, true) => String::new(),
        (false, true) => before.to_string(),
        (true, false) => after.to_string(),
        (false, false) => format!("{before}\n\n{after}"),
    }
}

/// Byte length of the balanced `{…}` at the start of `text`, honouring
/// string literals and escapes so a brace inside a prompt does not close
/// the object early.
fn balanced_object_end(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (index, ch) in text.char_indices() {
        if in_string {
            match ch {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index + ch.len_utf8());
                }
            }
            _ => {}
        }
    }
    None
}

/// Which allowlisted agent programs are on `PATH`, in allowlist order.
#[must_use]
pub fn installed_programs() -> Vec<String> {
    ALLOWLISTED_PROGRAMS
        .iter()
        .filter(|program| is_installed(program))
        .map(|program| (*program).to_string())
        .collect()
}

/// Whether `program` resolves to an executable on `PATH`.
#[must_use]
pub fn is_installed(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| is_executable(&dir.join(program)))
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &std::path::Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file())
        .unwrap_or(false)
}

#[cfg(test)]
#[allow(clippy::unnecessary_wraps)] // `answer` mirrors the `ask` closure's return type
mod tests {
    use super::*;
    use std::cell::RefCell;

    const DRAFT: &str = r#"{"name":"pair","description":"implementer then reviewer","layout":null,"prompt":null,
        "agents":[{"alias":"impl","program":"claude","role":"implementer","task":null,"prompt":"Run `muxa work done` when finished.","direction":null,"after":[]},
                  {"alias":"review","program":"codex","role":"reviewer","task":null,"prompt":null,"direction":"down","after":["impl"]}]}"#;

    fn answer(text: &str) -> Result<AskAnswer, String> {
        Ok(reply(text))
    }

    fn reply(text: &str) -> AskAnswer {
        AskAnswer {
            text: text.to_string(),
            session_id: None,
            cost_usd: None,
        }
    }

    #[test]
    fn the_schemas_document_every_key_a_pipeline_agent_accepts() {
        let agent = serde_json::to_value(crate::config::PipelineAgentConfig::default())
            .expect("PipelineAgentConfig serializes");
        let json_schema = pipeline_json_schema();
        for key in agent.as_object().expect("an object").keys() {
            assert!(
                CONFIG_SCHEMA.contains(key.as_str()),
                "pipeline agent key `{key}` is not in the TOML schema"
            );
            assert!(
                json_schema.contains(&format!("\"{key}\"")),
                "pipeline agent key `{key}` is not in the JSON schema"
            );
        }
        let route = serde_json::to_value(crate::config::RouteConfig::default())
            .expect("RouteConfig serializes");
        for key in route.as_object().expect("an object").keys() {
            assert!(
                CONFIG_SCHEMA.contains(key.as_str()),
                "route key `{key}` is not in the TOML schema"
            );
        }
        for program in ALLOWLISTED_PROGRAMS {
            assert!(json_schema.contains(program), "{program} missing");
        }
        for key in ["name", "description", "layout", "prompt", "agents"] {
            assert!(json_schema.contains(&format!("\"{key}\"")), "{key} missing");
        }
    }

    #[test]
    fn the_config_prompt_carries_the_schema_the_file_and_the_installed_programs() {
        let prompt = config_prompt(
            "cal tickets get three agents",
            "[watch]\ntheme = \"ops\"\n",
            &["claude".to_string(), "codex".to_string()],
        );
        assert!(
            prompt.contains("[[pipeline.<name>.agent]]"),
            "schema missing"
        );
        assert!(prompt.contains("theme = \"ops\""), "current config missing");
        assert!(prompt.contains("cal tickets get three agents"));
        assert!(prompt.contains("claude, codex — prefer these"), "{prompt}");
        // An empty file says so rather than sending an empty fence.
        let empty = config_prompt("x", "  ", &[]);
        assert!(empty.contains("empty or absent"));
        assert!(
            empty.contains("none of the allowlisted agent CLIs"),
            "{empty}"
        );
    }

    #[test]
    fn the_compose_prompt_carries_the_draft_the_request_and_the_rejection() {
        let current: PipelineSpec = serde_json::from_str(DRAFT).unwrap();
        let prompt = compose_prompt(
            "add a gemini tester after review",
            Some(&current),
            &["gemini".to_string()],
            None,
        );
        assert!(prompt.contains("\"agents\": ["), "schema missing");
        assert!(prompt.contains("THE CURRENT DRAFT"), "{prompt}");
        assert!(
            prompt.contains("\"alias\": \"review\""),
            "current draft missing"
        );
        assert!(prompt.contains("add a gemini tester after review"));
        assert!(prompt.contains("gemini — prefer these"), "{prompt}");
        assert!(!prompt.contains("REJECTED"));
        assert!(prompt.contains("Do not run commands and do not edit files"));

        let retry = compose_prompt(
            "x",
            None,
            &[],
            Some(&Rejection {
                draft: "{\"name\": \"bad\"}",
                error: "declares no agents",
            }),
        );
        assert!(
            retry.contains("YOUR PREVIOUS ANSWER WAS REJECTED"),
            "{retry}"
        );
        assert!(retry.contains("{\"name\": \"bad\"}"));
        assert!(retry.contains("declares no agents"));
        assert!(!retry.contains("THE CURRENT DRAFT"));
    }

    #[test]
    fn json_is_taken_from_a_fence_first_and_a_bare_object_second() {
        let fenced =
            format!("Here is a draft.\n```json\n{DRAFT}\n```\nTwo agents, reviewer after.");
        let (json, notes) = extract_json_object(&fenced).unwrap();
        assert_eq!(json, DRAFT);
        assert_eq!(notes, "Here is a draft.\n\nTwo agents, reviewer after.");

        // An untagged fence works; a non-JSON fence before it is skipped.
        let mixed = format!("```toml\n[x]\n```\nthen\n```\n{DRAFT}\n```");
        let (json, notes) = extract_json_object(&mixed).unwrap();
        assert_eq!(json, DRAFT);
        assert_eq!(notes, "```toml\n[x]\n```\nthen");

        // A brace inside a prompt string does not end the object early.
        let bare = r#"Draft: {"name":"solo","agents":[{"alias":"a","program":"claude","prompt":"use {{id}} and a } brace"}]} done"#;
        let (json, notes) = extract_json_object(bare).unwrap();
        assert!(json.starts_with("{\"name\":\"solo\""), "{json}");
        assert!(json.ends_with("brace\"}]}"), "{json}");
        assert_eq!(notes, "Draft:\n\ndone");

        // Unterminated fence: take what is there.
        let open = format!("```json\n{DRAFT}\n");
        assert_eq!(extract_json_object(&open).unwrap().0, DRAFT);

        assert!(extract_json_object("no json here").is_none());
        assert!(extract_json_object("{ unbalanced").is_none());
    }

    #[test]
    fn interpreting_a_draft_names_validates_and_keeps_the_notes() {
        let output = interpret(&format!("note\n```json\n{DRAFT}\n```")).unwrap();
        assert_eq!(output.pipeline.name.as_deref(), Some("pair"));
        assert_eq!(output.pipeline.agents.len(), 2);
        assert_eq!(output.notes, "note");
        assert!(output.raw.contains(DRAFT));
        // The output round-trips as the `pipeline set` input shape.
        let value = serde_json::to_value(&output.pipeline).unwrap();
        assert_eq!(value["agents"][1]["after"], serde_json::json!(["impl"]));
        let back: PipelineSpec = serde_json::from_value(value).unwrap();
        assert_eq!(back, output.pipeline);

        let unnamed = DRAFT.replacen("\"name\":\"pair\"", "\"name\":\" \"", 1);
        assert!(matches!(
            interpret(&unnamed),
            Err(WorkComposeError::MissingName)
        ));
        let bad_name = DRAFT.replacen("\"name\":\"pair\"", "\"name\":\"a b\"", 1);
        assert!(interpret(&bad_name)
            .unwrap_err()
            .to_string()
            .contains("cannot be a [pipeline.<name>] key"));
        let vim = DRAFT.replacen("\"program\":\"codex\"", "\"program\":\"vim\"", 1);
        assert!(interpret(&vim)
            .unwrap_err()
            .to_string()
            .contains("not an allowlisted agent CLI"));
        let typo = DRAFT.replacen("\"agents\"", "\"agent\"", 1);
        assert!(matches!(
            interpret(&typo),
            Err(WorkComposeError::BadJson(_))
        ));
        assert!(matches!(
            interpret("nothing"),
            Err(WorkComposeError::NoJson(_))
        ));
    }

    #[tokio::test]
    async fn a_valid_first_draft_costs_one_turn() {
        let prompts = RefCell::new(Vec::new());
        let request = WorkComposeRequest {
            description: "implementer in claude, reviewer in codex after it".into(),
            agent: None,
            current: None,
        };
        let output = compose(&request, &["claude".to_string()], |prompt| {
            prompts.borrow_mut().push(prompt);
            async { answer(&format!("```json\n{DRAFT}\n```")) }
        })
        .await
        .unwrap();
        assert_eq!(output.pipeline.name.as_deref(), Some("pair"));
        assert_eq!(output.notes, "");
        let prompts = prompts.into_inner();
        assert_eq!(prompts.len(), 1);
        assert!(prompts[0].contains("implementer in claude, reviewer in codex after it"));
        assert!(!prompts[0].contains("REJECTED"));
    }

    #[tokio::test]
    async fn an_invalid_draft_is_retried_once_with_the_rejection() {
        let prompts = RefCell::new(Vec::new());
        let request = WorkComposeRequest {
            description: "two agents".into(),
            agent: Some("codex".into()),
            current: None,
        };
        let bad = DRAFT.replacen("\"after\":[\"impl\"]", "\"after\":[\"plan\"]", 1);
        let output = compose(&request, &[], |prompt| {
            let attempt = {
                let mut prompts = prompts.borrow_mut();
                prompts.push(prompt);
                prompts.len()
            };
            let bad = bad.clone();
            async move {
                if attempt == 1 {
                    answer(&format!("first try\n```json\n{bad}\n```"))
                } else {
                    answer(&format!("fixed\n```json\n{DRAFT}\n```"))
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(output.notes, "fixed");
        let prompts = prompts.into_inner();
        assert_eq!(prompts.len(), 2);
        assert!(
            prompts[1].contains("YOUR PREVIOUS ANSWER WAS REJECTED"),
            "{}",
            prompts[1]
        );
        assert!(
            prompts[1].contains("waits on \"plan\", which is not an alias"),
            "{}",
            prompts[1]
        );
        assert!(
            prompts[1].contains("first try"),
            "the rejected draft is shown back"
        );
    }

    #[tokio::test]
    async fn a_second_bad_draft_is_the_error_and_a_failed_turn_is_not_retried() {
        let calls = RefCell::new(0);
        let request = WorkComposeRequest {
            description: "two agents".into(),
            ..WorkComposeRequest::default()
        };
        let error = compose(&request, &[], |_| {
            *calls.borrow_mut() += 1;
            async { answer("I cannot help with that.") }
        })
        .await
        .unwrap_err();
        assert!(matches!(error, WorkComposeError::NoJson(_)), "{error}");
        assert_eq!(*calls.borrow(), 2);

        let calls = RefCell::new(0);
        let error = compose(&request, &[], |_| {
            *calls.borrow_mut() += 1;
            async { Err("claude exited non-zero: not logged in".to_string()) }
        })
        .await
        .unwrap_err();
        assert!(matches!(error, WorkComposeError::Turn(_)), "{error}");
        assert_eq!(error.to_string(), "claude exited non-zero: not logged in");
        assert_eq!(*calls.borrow(), 1);

        let blank = WorkComposeRequest::default();
        let error = compose(&blank, &[], |_| async { answer("") })
            .await
            .unwrap_err();
        assert!(matches!(error, WorkComposeError::EmptyDescription));
    }

    #[test]
    fn installed_programs_only_reports_allowlisted_binaries_on_path() {
        let installed = installed_programs();
        for program in &installed {
            assert!(
                ALLOWLISTED_PROGRAMS.contains(&program.as_str()),
                "{program}"
            );
        }
        assert!(!is_installed("definitely-not-an-agent-cli-xyz"));
    }
}
