//! External issues as a read-only view on a Work: link a GitHub issue or
//! pull request or a Linear issue to a Work record, and refresh its title
//! and state from the tracker without spending an agent turn.
//!
//! GitHub is read with `gh api` (its own login); Linear with its GraphQL
//! API and `LINEAR_API_KEY`. Nothing is written to the tracker, and the
//! tracker's state never moves the Work's own stage.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::time::Duration;
use tend::ipc::Client;
use tend::work::{ExternalItemRef, WorkIdentity, WorkRecord, WORK_EXTERNAL_CAPABILITY};
use time::OffsetDateTime;

const FETCH_DEADLINE: Duration = Duration::from_secs(15);

#[derive(Debug, clap::Args)]
pub struct LinkArgs {
    /// Work id, or WORKSPACE/WORK.
    pub work: String,
    #[arg(long)]
    pub workspace: Option<String>,
    /// A GitHub issue or pull request: OWNER/REPO#N or its URL.
    #[arg(long, conflicts_with = "linear", required_unless_present = "linear")]
    pub github: Option<String>,
    /// A Linear issue key, for example ENG-123.
    #[arg(long)]
    pub linear: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct RefreshArgs {
    /// Work id, or WORKSPACE/WORK. Omit with --all.
    #[arg(required_unless_present = "all")]
    pub work: Option<String>,
    #[arg(long)]
    pub workspace: Option<String>,
    /// Refresh every Work record that has an external reference.
    #[arg(long, conflicts_with = "work")]
    pub all: bool,
    #[arg(long)]
    pub json: bool,
}

/// A reference the CLI knows how to read.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    GitHub { repo: String, number: u64 },
    Linear { key: String },
}

impl Source {
    fn parse_github(spec: &str) -> Result<Self> {
        let spec = spec.trim();
        let (repo, number) = if let Some(rest) = spec.strip_prefix("https://github.com/") {
            let parts: Vec<&str> = rest.trim_end_matches('/').split('/').collect();
            match parts.as_slice() {
                [owner, repo, "issues" | "pull", number, ..] => {
                    (format!("{owner}/{repo}"), (*number).to_owned())
                }
                _ => bail!("not a GitHub issue or pull request URL: {spec}"),
            }
        } else {
            let Some((repo, number)) = spec.split_once('#') else {
                bail!("--github takes OWNER/REPO#N or an issue/PR URL");
            };
            (repo.to_owned(), number.to_owned())
        };
        let valid = |part: &str| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
        };
        match repo.split_once('/') {
            Some((owner, name)) if valid(owner) && valid(name) => {}
            _ => bail!("GitHub repository {repo:?} must be OWNER/REPO"),
        }
        let number = number
            .parse::<u64>()
            .ok()
            .filter(|n| *n > 0)
            .with_context(|| format!("GitHub number {number:?} must be a positive integer"))?;
        Ok(Self::GitHub { repo, number })
    }

    fn parse_linear(key: &str) -> Result<Self> {
        let key = key.trim().to_ascii_uppercase();
        let valid = key.split_once('-').is_some_and(|(team, n)| {
            !team.is_empty()
                && team.chars().all(|c| c.is_ascii_alphanumeric())
                && !n.is_empty()
                && n.chars().all(|c| c.is_ascii_digit())
        });
        if !valid {
            bail!("Linear key {key:?} must look like ENG-123");
        }
        Ok(Self::Linear { key })
    }

    /// The source a stored reference names, when the CLI can read it.
    fn of(item: &ExternalItemRef) -> Option<Self> {
        match item.source.as_str() {
            "github" => {
                let number = item.display_key.trim_start_matches('#');
                Self::parse_github(&format!("{}#{number}", item.scope.as_deref()?)).ok()
            }
            "linear" => Self::parse_linear(&item.display_key).ok(),
            _ => None,
        }
    }
}

/// Keep tracker text printable before it is stored.
fn printable(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_owned()
}

async fn fetch(source: &Source) -> Result<ExternalItemRef> {
    match source {
        Source::GitHub { repo, number } => fetch_github(repo, *number).await,
        Source::Linear { key } => fetch_linear(key).await,
    }
}

async fn fetch_github(repo: &str, number: u64) -> Result<ExternalItemRef> {
    // The issues endpoint answers for pull requests too.
    let mut command = tokio::process::Command::new("gh");
    command
        // Pinned to github.com: the reference names it, and GH_HOST would
        // otherwise read the same number from another host.
        .args([
            "api",
            "--hostname",
            "github.com",
            &format!("repos/{repo}/issues/{number}"),
        ])
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(FETCH_DEADLINE, command.output())
        .await
        .context("gh api took too long")?
        .context("running gh (install the GitHub CLI and `gh auth login`)")?;
    if !output.status.success() {
        bail!(
            "gh api repos/{repo}/issues/{number}: {}",
            printable(
                String::from_utf8_lossy(&output.stderr)
                    .lines()
                    .next()
                    .unwrap_or("")
            )
        );
    }
    let value: Value = serde_json::from_slice(&output.stdout).context("reading gh api output")?;
    Ok(github_item(repo, number, &value, OffsetDateTime::now_utc()))
}

fn github_item(repo: &str, number: u64, value: &Value, now: OffsetDateTime) -> ExternalItemRef {
    let pull = value.get("pull_request").filter(|p| !p.is_null());
    let state = value["state"].as_str().map(printable);
    let merged = pull.is_some_and(|p| p["merged_at"].is_string());
    // GitHub's own spelling of owner/repo, so case variants of one link
    // are one item.
    let scope = value["repository_url"]
        .as_str()
        .and_then(|url| url.rsplit_once("/repos/").map(|(_, repo)| printable(repo)))
        .filter(|canonical| canonical.eq_ignore_ascii_case(repo))
        .unwrap_or_else(|| repo.to_owned());
    ExternalItemRef {
        fetched_at: Some(now),
        source: "github".into(),
        scope: Some(scope),
        stable_id: value["node_id"].as_str().map(printable),
        display_key: format!("#{number}"),
        title: value["title"].as_str().map(printable),
        url: value["html_url"].as_str().map(printable),
        status: if merged { Some("merged".into()) } else { state },
        item_type: Some(
            if pull.is_some() {
                "pull_request"
            } else {
                "issue"
            }
            .into(),
        ),
        synced_at: now,
    }
}

async fn fetch_linear(key: &str) -> Result<ExternalItemRef> {
    let Ok(token) = std::env::var("LINEAR_API_KEY") else {
        bail!("set LINEAR_API_KEY to refresh Linear issues");
    };
    let body = serde_json::json!({
        "query": "query($id: String!) { issue(id: $id) { id identifier title url state { name } } }",
        "variables": { "id": key },
    });
    let response = reqwest::Client::builder()
        .timeout(FETCH_DEADLINE)
        .build()?
        .post("https://api.linear.app/graphql")
        .header("Authorization", token)
        .json(&body)
        .send()
        .await
        .context("reaching the Linear API")?;
    let status = response.status();
    let value: Value = response
        .json()
        .await
        .context("reading the Linear API reply")?;
    if !status.is_success() || value.get("errors").is_some_and(|e| !e.is_null()) {
        bail!(
            "Linear API refused {key}: {}",
            printable(&value["errors"][0]["message"].to_string())
        );
    }
    linear_item(key, &value["data"]["issue"], OffsetDateTime::now_utc())
}

fn linear_item(key: &str, issue: &Value, now: OffsetDateTime) -> Result<ExternalItemRef> {
    if issue.is_null() {
        bail!("Linear has no issue {key}");
    }
    // The identifier Linear returns, which follows an issue that moved
    // teams, rather than the key it was asked for.
    let display_key = issue["identifier"]
        .as_str()
        .map_or_else(|| key.to_owned(), printable);
    Ok(ExternalItemRef {
        fetched_at: Some(now),
        source: "linear".into(),
        scope: display_key.split_once('-').map(|(team, _)| team.to_owned()),
        stable_id: issue["id"].as_str().map(printable),
        display_key,
        title: issue["title"].as_str().map(printable),
        url: issue["url"].as_str().map(printable),
        status: issue["state"]["name"].as_str().map(printable),
        item_type: Some("issue".into()),
        synced_at: now,
    })
}

async fn require_external(client: &Client) -> Result<()> {
    let hello = client
        .hello(Duration::from_secs(3))
        .await
        .context("asking tendd what it supports")?;
    if !hello
        .capabilities
        .iter()
        .any(|c| c == WORK_EXTERNAL_CAPABILITY)
    {
        bail!("this tendd does not store external references ({WORK_EXTERNAL_CAPABILITY}); restart it with the same tend build");
    }
    Ok(())
}

/// Split `WORKSPACE/WORK` and `--workspace` into the Work id and the
/// workspace to look in, refusing two that disagree.
fn target(work: &str, workspace: Option<&str>) -> Result<(String, Option<String>)> {
    match work.split_once('/') {
        Some((prefix, id)) => {
            if workspace.is_some_and(|w| !w.eq_ignore_ascii_case(prefix)) {
                bail!("--workspace disagrees with {prefix:?}/");
            }
            Ok((id.to_owned(), Some(prefix.to_owned())))
        }
        None => Ok((work.to_owned(), workspace.map(str::to_owned))),
    }
}

async fn resolve(client: &Client, work: &str, workspace: Option<&str>) -> Result<WorkIdentity> {
    let records = client.work_backlog(None).await?;
    let (id, workspace) = target(work, workspace)?;
    if let Some(record) =
        crate::work_backlog::find_record(&records, &id, workspace.as_deref(), false)?
    {
        return Ok(record.identity.clone());
    }
    crate::work_backlog::identity(&id, workspace.as_deref(), &std::env::current_dir()?)
}

pub async fn run_link(args: LinkArgs, client: &Client) -> Result<()> {
    require_external(client).await?;
    let source = match (&args.github, &args.linear) {
        (Some(spec), _) => Source::parse_github(spec)?,
        (None, Some(key)) => Source::parse_linear(key)?,
        (None, None) => bail!("name the issue: --github or --linear"),
    };
    let identity = resolve(client, &args.work, args.workspace.as_deref()).await?;
    let item = fetch(&source).await?;
    let record = client.work_external_upsert(&identity, &item).await?;
    print_items(&record, args.json)
}

pub async fn run_refresh(args: RefreshArgs, client: &Client) -> Result<()> {
    require_external(client).await?;
    let records = client.work_backlog(None).await?;
    let targets = select_targets(records, &args)?;
    let mut report = Vec::new();
    for record in targets {
        for item in &record.external_items {
            let line = match Source::of(item) {
                None => format!(
                    "skip  {} {}: not refreshable",
                    item.source, item.display_key
                ),
                Some(source) => match fetch(&source).await {
                    Ok(fresh) => {
                        let line = describe(&record.identity, &fresh);
                        client
                            .work_external_upsert(&record.identity, &fresh)
                            .await?;
                        line
                    }
                    Err(error) => format!(
                        "fail  {} {} {}: {error:#}",
                        record.identity.key(),
                        item.source,
                        item.display_key
                    ),
                },
            };
            report.push(line);
        }
    }
    if args.json {
        println!("{}", serde_json::to_string(&report)?);
    } else if report.is_empty() {
        println!("no external references to refresh");
    } else {
        for line in report {
            println!("{line}");
        }
    }
    Ok(())
}

/// The records `tend work refresh` reads: one named Work (either id form),
/// or with `--all` every record with a reference, in `--workspace` only
/// when one is given.
fn select_targets(records: Vec<WorkRecord>, args: &RefreshArgs) -> Result<Vec<WorkRecord>> {
    if args.all {
        let workspace = args
            .workspace
            .as_deref()
            .map(crate::tmux_work::normalize_workspace_id)
            .transpose()?;
        return Ok(records
            .into_iter()
            .filter(|record| {
                !record.external_items.is_empty()
                    && workspace
                        .as_ref()
                        .is_none_or(|w| &record.identity.workspace_id == w)
            })
            .collect());
    }
    let work = args.work.as_deref().unwrap_or_default();
    let (id, workspace) = target(work, args.workspace.as_deref())?;
    let Some(record) =
        crate::work_backlog::find_record(&records, &id, workspace.as_deref(), false)?
    else {
        bail!("no Work record for {work}; link an issue with `tend work link`");
    };
    Ok(vec![record.clone()])
}

fn describe(identity: &WorkIdentity, item: &ExternalItemRef) -> String {
    format!(
        "ok    {}  {} {} [{}] {}",
        identity.key(),
        item.source,
        item.display_key,
        item.status.as_deref().unwrap_or("?"),
        item.title.as_deref().unwrap_or("")
    )
}

fn print_items(record: &WorkRecord, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(record)?);
    } else {
        for item in &record.external_items {
            println!("{}", describe(&record.identity, item));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn references_parse_strictly() {
        assert_eq!(
            Source::parse_github("Open330/tend#483").unwrap(),
            Source::GitHub {
                repo: "Open330/tend".into(),
                number: 483
            }
        );
        assert_eq!(
            Source::parse_github("https://github.com/Open330/tend/pull/483/files").unwrap(),
            Source::GitHub {
                repo: "Open330/tend".into(),
                number: 483
            }
        );
        assert!(Source::parse_github("Open330/tend#0").is_err());
        assert!(Source::parse_github("../etc#1").is_err());
        assert!(Source::parse_github("a/b/c#1").is_err());
        assert!(Source::parse_github("https://gitlab.com/a/b/issues/1").is_err());
        assert_eq!(
            Source::parse_linear("eng-12").unwrap(),
            Source::Linear {
                key: "ENG-12".into()
            }
        );
        assert!(Source::parse_linear("ENG-").is_err());
        assert!(Source::parse_linear("EN G-1").is_err());
    }

    #[test]
    fn refresh_targets_take_both_id_forms_and_the_workspace_filter() {
        let record = |workspace: &str, work: &str, linked: bool| WorkRecord {
            identity: WorkIdentity::new(workspace, work),
            metadata: tend::work::WorkMetadata {
                title: None,
                goal: None,
                next_action: None,
                stage: tend::work::WorkStage::Queued,
                updated_at: datetime!(2026-11-12 10:00 UTC),
            },
            external_items: if linked {
                vec![linear_item(
                    "ENG-1",
                    &serde_json::json!({"identifier": "ENG-1"}),
                    datetime!(2026-11-12 10:00 UTC),
                )
                .unwrap()]
            } else {
                Vec::new()
            },
            legacy_binding: None,
            plan: None,
            created_at: datetime!(2026-11-12 10:00 UTC),
        };
        let records = vec![
            record("web", "ENG-1", true),
            record("api", "ENG-1", true),
            record("api", "ENG-2", false),
        ];
        let args = |work: Option<&str>, workspace: Option<&str>, all: bool| RefreshArgs {
            work: work.map(Into::into),
            workspace: workspace.map(Into::into),
            all,
            json: false,
        };
        let one = select_targets(records.clone(), &args(Some("web/ENG-1"), None, false)).unwrap();
        assert_eq!(one[0].identity, WorkIdentity::new("web", "ENG-1"));
        assert!(select_targets(records.clone(), &args(Some("ENG-1"), None, false)).is_err());
        assert!(select_targets(
            records.clone(),
            &args(Some("web/ENG-1"), Some("api"), false)
        )
        .is_err());
        let api = select_targets(records.clone(), &args(None, Some("api"), true)).unwrap();
        assert_eq!(api.len(), 1, "only api, and only linked records");
        assert_eq!(
            select_targets(records, &args(None, None, true))
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn tracker_replies_become_validated_references() {
        let now = datetime!(2026-11-12 12:00 UTC);
        let pr = github_item(
            "Open330/tend",
            483,
            &serde_json::json!({
                "title": "feat(work): a backlog\u{1b}[2J", "state": "closed", "node_id": "PR_1",
                "repository_url": "https://api.github.com/repos/Open330/tend",
                "html_url": "https://github.com/Open330/tend/pull/483",
                "pull_request": {"merged_at": "2026-11-12T10:30:00Z"}
            }),
            now,
        );
        assert_eq!(pr.status.as_deref(), Some("merged"));
        assert_eq!(pr.item_type.as_deref(), Some("pull_request"));
        assert_eq!(pr.title.as_deref(), Some("feat(work): a backlog[2J"));
        assert!(pr.fetched_at.is_some());
        let lower = github_item(
            "open330/tend",
            483,
            &serde_json::json!({"repository_url": "https://api.github.com/repos/Open330/tend"}),
            now,
        );
        assert_eq!(
            lower.scope.as_deref(),
            Some("Open330/tend"),
            "GitHub's spelling"
        );
        pr.validate().unwrap();
        assert_eq!(
            Source::of(&pr),
            Some(Source::GitHub {
                repo: "Open330/tend".into(),
                number: 483
            })
        );
        let issue = linear_item(
            "ENG-12",
            &serde_json::json!({"id": "uuid", "identifier": "ENG-12", "title": "Fix login",
                                "url": "https://linear.app/x/issue/ENG-12", "state": {"name": "In Progress"}}),
            now,
        )
        .unwrap();
        assert_eq!(issue.status.as_deref(), Some("In Progress"));
        issue.validate().unwrap();
        assert!(linear_item("ENG-13", &Value::Null, now).is_err());
        let mut odd = issue.clone();
        odd.source = "jira".into();
        assert!(Source::of(&odd).is_none());
    }
}
