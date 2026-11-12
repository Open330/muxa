//! CLI adapter for durable, single-coordinator Fleet dispatch.
use crate::owner_only::OwnerOnly;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tend::ipc::Client;
use tend::orchestration::{DispatchPlan, DispatchRequest};
use tend::work_control::{execute_work_command, WorkCommandLimits, WorkUpRequest};
use tend::Config;

#[derive(Debug, Args)]
pub struct DispatchArgs {
    /// JSON request file, or - for stdin. A stable `dispatch_id` is required.
    #[arg(long, default_value = "-")]
    pub from_json: String,
    /// Resolve placement and paths without cloning or launching anything.
    #[arg(long)]
    pub plan: bool,
    #[arg(long, hide = true)]
    pub at_coordinator: bool,
}
#[derive(Debug, Args)]
pub struct StatusArgs {
    pub dispatch_id: String,
    #[arg(long, hide = true)]
    pub at_coordinator: bool,
}
#[derive(Debug, Args)]
pub struct ExecuteArgs {
    #[arg(long, default_value = "-")]
    pub from_json: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    plan: DispatchPlan,
    /// preparing / launched / failed / unknown. Launched does not mean Work completed.
    state: String,
    result: Option<Value>,
    error: Option<String>,
}
fn data_dir(role: &str) -> Result<PathBuf> {
    let dir = tend::paths::default_node_id_file()
        .context("no data directory")?
        .parent()
        .context("no data parent")?
        .join("dispatches")
        .join(role);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
fn read_json<T: serde::de::DeserializeOwned>(path: &str) -> Result<T> {
    let mut body = String::new();
    if path == "-" {
        std::io::stdin().take(65537).read_to_string(&mut body)?;
    } else {
        std::fs::File::open(path)?
            .take(65537)
            .read_to_string(&mut body)?;
    }
    if body.len() > 65536 {
        bail!("dispatch JSON exceeds 64 KiB");
    }
    Ok(serde_json::from_str(&body)?)
}
fn print(value: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string(value)?);
    Ok(())
}
fn load(path: &Path) -> Result<Option<Record>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).context(
            "incomplete dispatch journal; execution will not be repeated",
        )?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn save(path: &Path, record: &Record) -> Result<()> {
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .owner_only(0o600)
        .open(&temp)?;
    f.write_all(&serde_json::to_vec(record)?)?;
    f.sync_all()?;
    std::fs::rename(temp, path)?;
    std::fs::File::open(path.parent().context("journal parent missing")?)?.sync_all()?;
    Ok(())
}
/// Exclusive marker deliberately survives a crash: absence of an ack is not cancellation.
fn reserve(path: &Path, record: &Record) -> Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .owner_only(0o600)
        .open(path)
        .context("dispatch already reserved; read dispatch-status instead of launching again")?;
    f.write_all(&serde_json::to_vec(record)?)?;
    f.sync_all()?;
    std::fs::File::open(path.parent().context("journal parent missing")?)?.sync_all()?;
    Ok(())
}
fn coordinator(cfg: &Config, explicit: bool) -> Result<Option<&str>> {
    if !cfg.orchestration.enabled {
        bail!("enable [orchestration].enabled first");
    }
    let remote = cfg
        .orchestration
        .coordinator
        .as_deref()
        .filter(|v| *v != "local");
    if explicit && remote.is_some() {
        bail!("coordinator routing loop: the destination must own orchestration (coordinator omitted or local)");
    }
    Ok(remote)
}
fn output_json(output: tend::work_control::WorkCommandOutput) -> Result<Value> {
    if output.exit_code != 0 {
        bail!("{}", output.stderr);
    }
    serde_json::from_str(&output.stdout).context("invalid dispatch response")
}

#[allow(clippy::too_many_lines)] // Keep durable reservation and remote effect ordering together.
pub async fn dispatch(
    args: DispatchArgs,
    cfg: &Config,
    client: &Client,
    config: Option<&Path>,
) -> Result<()> {
    let request: DispatchRequest = read_json(&args.from_json)?;
    request.validate().map_err(anyhow::Error::msg)?;
    if let Some(host) = coordinator(cfg, args.at_coordinator)? {
        let mut command_args = vec!["work".into(), "dispatch".into(), "--at-coordinator".into()];
        if args.plan {
            command_args.push("--plan".into());
        }
        let result = client
            .work_command(
                Some(host),
                &command_args,
                Some(&serde_json::to_string(&request)?),
            )
            .await;
        return match result {
            Ok(output) => print(&output_json(output)?),
            Err(e) => print(
                &json!({"dispatch_id":request.dispatch_id,"state":"unknown","error":e.to_string(),"next_step":"read work dispatch-status using this ID; do not create a new dispatch"}),
            ),
        };
    }
    if args.plan {
        let snapshot = client.fleet_snapshot(None).await?;
        return print(
            &cfg.orchestration
                .plan(request, &snapshot)
                .map_err(anyhow::Error::msg)?,
        );
    }
    let dir = data_dir("coordinator")?;
    let path = dir.join(format!("{}.json", request.dispatch_id));
    if let Some(record) = load(&path)? {
        if record.plan.request != request {
            bail!("dispatch_id reused with a different request");
        }
        return print(&record);
    }
    let snapshot = client.fleet_snapshot(None).await?;
    let plan = cfg
        .orchestration
        .plan(request, &snapshot)
        .map_err(anyhow::Error::msg)?;
    let mut record = Record {
        plan,
        state: "preparing".into(),
        result: None,
        error: None,
    };
    reserve(&path, &record)?;
    // One assignment per logical Work. A new UUID is not permission to rerun it elsewhere.
    let work_dir = dir
        .join("work")
        .join(record.plan.request.workspace.to_ascii_lowercase());
    std::fs::create_dir_all(&work_dir)?;
    let work_path = work_dir.join(format!(
        "{}.json",
        record.plan.request.work.to_ascii_uppercase()
    ));
    if let Err(e) = reserve(&work_path, &record) {
        record.state = "failed".into();
        record.error = Some(e.to_string());
        save(&path, &record)?;
        return print(&record);
    }
    let mut command_args = vec!["work".into(), "dispatch-execute".into()];
    let input = serde_json::to_string(&record.plan)?;
    let outcome = if record.plan.host == "local" {
        if let Some(config) = config {
            command_args.splice(0..0, ["--config".into(), config.display().to_string()]);
        }
        execute_work_command(
            &std::env::current_exe()?,
            &command_args,
            Some(&input),
            Some(client.socket()),
            WorkCommandLimits::WORK_UP,
        )
        .await
        .map_err(|e| e.to_string())
    } else {
        client
            .work_command(Some(&record.plan.host), &command_args, Some(&input))
            .await
            .map_err(|e| e.to_string())
    };
    match outcome.and_then(|o| output_json(o).map_err(|e| e.to_string())) {
        Ok(value) => {
            record.state = value
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .into();
            record.result = Some(value);
        }
        Err(e) => {
            record.state = "unknown".into();
            record.error = Some(e);
        }
    }
    save(&path, &record)?;
    print(&record)
}

pub async fn status(args: StatusArgs, cfg: &Config, client: &Client) -> Result<()> {
    let id = uuid::Uuid::parse_str(&args.dispatch_id)?.to_string();
    if let Some(host) = coordinator(cfg, args.at_coordinator)? {
        let output = client
            .work_command(
                Some(host),
                &[
                    "work".into(),
                    "dispatch-status".into(),
                    id,
                    "--at-coordinator".into(),
                ],
                None,
            )
            .await?;
        return print(&output_json(output)?);
    }
    let path = data_dir("coordinator")?.join(format!("{id}.json"));
    let mut record = load(&path)?.context("dispatch not found on coordinator")?;
    // Recovery reads the worker journal, never resends a launch. A
    // finished dispatch is read too: its result records and their
    // dispositions keep changing after the Run completes.
    let finished = matches!(record.state.as_str(), "completed" | "failed");
    if finished
        || matches!(
            record.state.as_str(),
            "unknown" | "preparing" | "launched" | "blocked"
        )
    {
        let result = if record.plan.host == "local" {
            match refresh_worker(&id, client).await {
                Ok(worker) => Some(serde_json::to_value(worker)?),
                // A dispatch that failed before its worker journal existed
                // has nothing more to read; the record stands as it is.
                Err(_) if finished => None,
                Err(error) => return Err(error),
            }
        } else {
            client
                .work_command(
                    Some(&record.plan.host),
                    &["work".into(), "dispatch-worker-status".into(), id],
                    None,
                )
                .await
                .ok()
                .and_then(|o| output_json(o).ok())
        };
        if let Some(value) = result {
            if value.get("plan").and_then(|p| p.get("node_id")) == Some(&json!(record.plan.node_id))
            {
                record.state = value
                    .get("state")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .into();
                record.result = Some(value);
                save(&path, &record)?;
            }
        }
    }
    print(&record)
}

async fn refresh_worker(id: &str, client: &Client) -> Result<Record> {
    let id = uuid::Uuid::parse_str(id)?.to_string();
    let path = data_dir("worker")?.join(format!("{id}.json"));
    let mut record = load(&path)?.context("worker dispatch not found")?;
    let open = matches!(record.state.as_str(), "launched" | "blocked" | "unknown");
    // A finished journal written before Runs were linked by identity has
    // a result but no `work`: look its Run up once, without touching the
    // saved state, so its result records can be read.
    let unlinked = record
        .result
        .as_ref()
        .is_some_and(|result| result.get("work").is_none());
    if open || unlinked {
        let paths = record
            .plan
            .resolve_paths(&dirs::home_dir().context("home missing")?)
            .map_err(anyhow::Error::msg)?;
        // work up records a canonical cwd (notably /private/tmp on macOS).
        let run_path = tend::paths::canonicalize(&paths.run).unwrap_or_else(|_| paths.run.clone());
        let runs = client.pipeline_runs().await?;
        if let Some(run) = runs.iter().find(|r| {
            r.identity
                .workspace_id
                .eq_ignore_ascii_case(&record.plan.request.workspace)
                && r.identity
                    .work_id
                    .eq_ignore_ascii_case(&record.plan.request.work)
                && r.cwd == run_path
        }) {
            if open {
                use tend::pipeline_run::PipelineAliasStatus as S;
                if !run.aliases.is_empty() && run.aliases.values().all(|a| a.status == S::Done) {
                    record.state = "completed".into();
                } else if run.aliases.values().any(|a| a.status == S::Failed) {
                    record.state = "failed".into();
                } else if run.aliases.values().any(|a| a.status == S::Blocked) {
                    record.state = "blocked".into();
                } else {
                    record.state = "launched".into();
                }
                // Carry bounded machine-readable verification state and artifact location.
                record.result = Some(
                    json!({"generation":run.generation,"aliases":run.aliases,"artifacts":paths.artifacts,"cwd":paths.run,"commit":record.plan.request.commit,"work":run.identity}),
                );
            } else if let Some(result) = record.result.as_mut() {
                // The exact Run identity, never the dispatch's own spelling.
                result["work"] = json!(run.identity);
            }
            save(&path, &record)?;
        }
    }
    // Result records (`tend work done --summary`, `tend_file_result`) and
    // their dispositions keep changing after the Run completes, so they are
    // read on every status, not frozen with the Run state.
    if let Some(result) = record.result.as_mut() {
        if let Some(identity) = result
            .get("work")
            .cloned()
            .and_then(|work| serde_json::from_value::<tend::work::WorkIdentity>(work).ok())
        {
            let capabilities = client
                .hello(std::time::Duration::from_secs(2))
                .await
                .map(|hello| hello.capabilities)
                .unwrap_or_default();
            let has = |name: &str| capabilities.iter().any(|c| c == name);
            if has(tend::work::WORK_BACKLOG_CAPABILITY) {
                let recent = has(tend::work::WORK_RECENT_RESULTS_CAPABILITY);
                let newest = recent.then_some(MAX_STATUS_RESULTS);
                if let Ok(show) = client.work_record_show_newest(&identity, newest).await {
                    attach_results(result, &show, recent);
                }
            }
        }
    }
    Ok(record)
}

/// Put the result records on a status. An older daemon ignores `newest`
/// and answers with the oldest page; when that page is cut off, its tail is
/// not the newest records, so none are claimed and the status says why.
fn attach_results(result: &mut Value, show: &tend::ipc::WorkShow, recent: bool) {
    if let Some(object) = result.as_object_mut() {
        object.remove("results_more");
        object.remove("results_unavailable");
    }
    if !recent && show.reviews_more {
        result["results"] = json!([]);
        result["results_more"] = json!(true);
        result["results_unavailable"] =
            json!("tendd is too old to read a Work's newest result records; restart it with the same tend build");
        return;
    }
    let (results, dropped) = result_records(&show.reviews);
    result["results"] = results;
    if show.reviews_more || dropped {
        result["results_more"] = json!(true);
    }
}

/// Upper bound on result records carried in a dispatch status.
const MAX_STATUS_RESULTS: usize = 20;
/// Serialized budget for those records: a remote status travels through a
/// work command capped at 1 MiB, and the rest of the status must fit too.
const MAX_STATUS_RESULT_BYTES: usize = 256 * 1024;

/// The newest result records filed on a dispatched Work (oldest first),
/// compact: what was done, the decision asked for, and what the person
/// decided. Older records are dropped first to stay within
/// [`MAX_STATUS_RESULT_BYTES`]; the flag says some were.
fn result_records(items: &[tend::review::ReviewItem]) -> (Value, bool) {
    let skip = items.len().saturating_sub(MAX_STATUS_RESULTS);
    let mut entries: Vec<Value> = items
        .iter()
        .skip(skip)
        .map(|item| {
            let alias = match &item.target {
                tend::review::ReviewTarget::Work { alias, .. } => alias.clone(),
                _ => None,
            };
            let decision = item.decision.as_ref();
            json!({
                "id": item.id,
                "alias": alias,
                "generation": item.generation,
                "summary": item.summary,
                "decision_needed": decision.and_then(|d| d.needed.clone()),
                "options": decision.map(|d| d.options.clone()).unwrap_or_default(),
                "recommendation": decision.and_then(|d| d.recommendation.clone()),
                "evidence": decision.map(|d| d.evidence.clone()).unwrap_or_default(),
                "requested_at": item
                    .requested_at
                    .format(&time::format_description::well_known::Rfc3339)
                    .ok(),
                "disposition": item.disposition.as_ref().map(|d| d.kind),
                "note": item.disposition.as_ref().and_then(|d| d.note.clone()),
            })
        })
        .collect();
    let mut dropped = skip > 0;
    let size = |entries: &[Value]| serde_json::to_vec(entries).map_or(0, |bytes| bytes.len());
    while entries.len() > 1 && size(&entries) > MAX_STATUS_RESULT_BYTES {
        entries.remove(0);
        dropped = true;
    }
    (Value::Array(entries), dropped)
}
pub async fn worker_status(id: &str, client: &Client) -> Result<()> {
    print(&refresh_worker(id, client).await?)
}

async fn git(args: &[String], socket: &Path) -> Result<String> {
    let result = execute_work_command(
        Path::new("git"),
        args,
        None,
        Some(socket),
        WorkCommandLimits::WORK_UP,
    )
    .await?;
    if result.exit_code != 0 {
        bail!("git failed: {}", result.stderr);
    }
    Ok(result.stdout.trim().into())
}

/// Runs `git worktree add` for the dispatch's run directory with a receipt
/// around it (see `tend::worktree_receipt`). A receipt that cannot be
/// written only costs cleanup its proof.
async fn add_worktree_with_receipt(
    args: &[String],
    plan: &DispatchPlan,
    paths: &tend::orchestration::ResolvedPaths,
    socket: &Path,
) -> Result<()> {
    let receipt = crate::worktrees_cmd::begin_receipt(
        &paths.repo,
        &paths.run,
        None,
        Some(plan.request.commit.clone()),
        tend::worktree_receipt::ReceiptOrigin::Dispatch {
            dispatch_id: plan.request.dispatch_id.clone(),
            workspace: plan.request.workspace.clone(),
            work: plan.request.work.clone(),
        },
    );
    if let Err(error) = git(args, socket).await {
        if let Some(receipt) = receipt {
            let _ = receipt.fail();
        }
        return Err(error);
    }
    if let Some(receipt) = receipt {
        if let Err(error) = receipt.finish(Some(plan.request.commit.clone())) {
            tracing::warn!(%error, "worktree receipt not completed");
        }
    }
    Ok(())
}

async fn prepare_checkout(
    plan: &DispatchPlan,
    paths: &tend::orchestration::ResolvedPaths,
    socket: &Path,
) -> Result<()> {
    std::fs::create_dir_all(paths.repo.parent().context("repository parent missing")?)?;
    // Multiple Works can prepare the same registered clone concurrently. Lock
    // only Git preparation, not the agent lifetime; the OS releases it on crash.
    let repo = tend::paths::canonicalize(&paths.repo).unwrap_or_else(|_| paths.repo.clone());
    let parent = tend::paths::canonicalize(repo.parent().context("repository parent missing")?)?;
    let lock_path = parent.join(format!(
        ".{}.tend-prepare.lock",
        repo.file_name()
            .context("repository name missing")?
            .to_string_lossy()
    ));
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .owner_only(0o600)
        .open(lock_path)?;
    let _lock = tokio::task::spawn_blocking(move || -> std::io::Result<std::fs::File> {
        lock.lock()?;
        Ok(lock)
    })
    .await??;
    std::fs::create_dir_all(paths.run.parent().context("run parent missing")?)?;
    if !paths.repo.exists() {
        git(
            &[
                "clone".into(),
                "--no-checkout".into(),
                "--".into(),
                plan.url.clone(),
                paths.repo.display().to_string(),
            ],
            socket,
        )
        .await?;
    }
    let prefix = vec!["-C".into(), paths.repo.display().to_string()];
    let mut args = prefix.clone();
    args.extend(["remote".into(), "get-url".into(), "origin".into()]);
    if git(&args, socket).await? != plan.url {
        bail!("existing repository origin differs from registered URL");
    }
    let mut args = prefix.clone();
    args.extend([
        "cat-file".into(),
        "-e".into(),
        format!("{}^{{commit}}", plan.request.commit),
    ]);
    if git(&args, socket).await.is_err() {
        let mut args = prefix.clone();
        args.extend(["fetch".into(), "origin".into(), plan.request.commit.clone()]);
        git(&args, socket).await?;
    }
    if paths.run.exists() {
        bail!("run directory already exists without a matching completed preparation; refusing to reuse it");
    }
    let mut args = prefix;
    args.extend([
        "worktree".into(),
        "add".into(),
        "--detach".into(),
        paths.run.display().to_string(),
        plan.request.commit.clone(),
    ]);
    // Boxed: the dispatch future runs on small test-thread stacks.
    Box::pin(add_worktree_with_receipt(&args, plan, paths, socket)).await?;
    if paths.run.join(".gitmodules").exists() {
        git(
            &[
                "-C".into(),
                paths.run.display().to_string(),
                "submodule".into(),
                "update".into(),
                "--init".into(),
                "--recursive".into(),
            ],
            socket,
        )
        .await?;
    }
    std::fs::create_dir_all(&paths.artifacts)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .owner_only(0o600)
        .open(paths.artifacts.join("dispatch.json"))?;
    file.write_all(&serde_json::to_vec_pretty(
        &json!({"plan":plan,"paths":paths}),
    )?)?;
    file.sync_all()?;
    Ok(())
}

pub async fn execute(
    args: ExecuteArgs,
    cfg: &Config,
    client: &Client,
    config: Option<&Path>,
) -> Result<()> {
    let plan: DispatchPlan = read_json(&args.from_json)?;
    plan.request.validate().map_err(anyhow::Error::msg)?;
    if !cfg.orchestration.enabled {
        bail!("orchestration is disabled on worker");
    }
    let id = tend::fleet::load_or_create_node_id(
        &tend::paths::default_node_id_file().context("no node identity path")?,
    )?;
    if id != plan.node_id {
        bail!("worker NodeId does not match placement; refusing to execute on a changed SSH alias");
    }
    let paths = plan
        .resolve_paths(&dirs::home_dir().context("home directory unavailable")?)
        .map_err(anyhow::Error::msg)?;
    let path = data_dir("worker")?.join(format!("{}.json", plan.request.dispatch_id));
    if let Some(record) = load(&path)? {
        if record.plan != plan {
            bail!("dispatch payload conflict");
        }
        return print(&record);
    }
    if !cfg.pipeline.contains_key(&plan.pipeline) {
        bail!("pipeline is not configured on worker: {}", plan.pipeline);
    }
    let mut record = Record {
        plan: plan.clone(),
        state: "preparing".into(),
        result: None,
        error: None,
    };
    reserve(&path, &record)?;
    let outcome: Result<Value> = async {
        Box::pin(prepare_checkout(&plan, &paths, client.socket())).await?;
        let request = WorkUpRequest {
            work: plan.request.work.clone(), external: None,
            pipeline: Some(plan.pipeline.clone()), workspace: Some(plan.request.workspace.clone()),
            cwd: Some(paths.run.clone()), skill: None, body: Some(plan.request.body.clone()),
            context: Some(format!("Dispatch {}. Input commit {}. Write deliverables to {}. Report verification and resulting commit; launching is not completion.", plan.request.dispatch_id, plan.request.commit, paths.artifacts.display())),
            no_ticket: true, dry_run: false, host: None,
        };
        let mut command_args = request.arguments();
        if let Some(config) = config { command_args.splice(0..0, ["--config".into(), config.display().to_string()]); }
        // A crash after launch must never cause the launch to be repeated.
        record.state = "unknown".into();
        save(&path, &record)?;
        let output = tend::work_control::execute_tmux_work(&std::env::current_exe()?, &command_args, client.socket()).await?;
        output_json(output)
    }.await;
    match outcome {
        Ok(result) => {
            record.state = "launched".into();
            record.result = Some(result);
        }
        Err(e) => {
            if record.state != "unknown" {
                record.state = "failed".into();
            }
            record.error = Some(e.to_string());
        }
    }
    save(&path, &record)?;
    print(&record)
}

/// Called only through the control-authorized Work transport.
pub async fn shared_ask(cfg: &Config, client: &Client) -> Result<()> {
    coordinator(cfg, true)?;
    let request: Value = read_json("-")?;
    if !tend::orchestration::shared_ask_kind(request["kind"].as_str().unwrap_or("")) {
        bail!("unsupported shared Ask operation");
    }
    print(&client.call(&request).await?)
}

/// Read authoritative workspace choices without creating a dispatch.
pub async fn options(cfg: &Config, client: &Client, at_coordinator: bool) -> Result<()> {
    if let Some(host) = coordinator(cfg, at_coordinator)? {
        let output = client
            .work_command(
                Some(host),
                &[
                    "work".into(),
                    "dispatch-options".into(),
                    "--at-coordinator".into(),
                ],
                None,
            )
            .await?;
        let mut value = output_json(output)?;
        value["coordinator"] = json!(host);
        print(&value)
    } else {
        print(&cfg.orchestration)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_old_daemons_oldest_page_is_never_shown_as_the_newest() {
        let item = |n: usize| -> tend::review::ReviewItem {
            serde_json::from_value(json!({
                "id": format!("rv_{n}"), "generation": 1, "summary": "done",
                "target": {"kind": "work", "workspace": "web", "work": "ENG-1"},
                "requested_at": "2026-11-12T11:00:00Z", "requested_by": {"kind": "agent"}
            }))
            .unwrap()
        };
        // What an older daemon returns for 501 records: the oldest 500.
        let old_page = tend::ipc::WorkShow {
            record: None,
            run: None,
            reviews: (0..500).map(item).collect(),
            reviews_more: true,
            reviews_error: None,
        };
        let mut result = json!({});
        attach_results(&mut result, &old_page, false);
        assert_eq!(result["results"], json!([]));
        assert_eq!(result["results_more"], true);
        assert!(result["results_unavailable"].is_string());
        // A short page from an older daemon is complete, so its tail is right.
        let short = tend::ipc::WorkShow {
            reviews: (0..3).map(item).collect(),
            reviews_more: false,
            ..old_page.clone()
        };
        attach_results(&mut result, &short, false);
        assert_eq!(result["results"].as_array().unwrap().len(), 3);
        assert!(result.get("results_unavailable").is_none());
        assert!(result.get("results_more").is_none());
        // A current daemon's newest page is trusted as such.
        let newest = tend::ipc::WorkShow {
            reviews: (481..501).map(item).collect(),
            reviews_more: true,
            ..old_page
        };
        attach_results(&mut result, &newest, true);
        assert_eq!(result["results"][19]["id"], "rv_500");
        assert_eq!(result["results_more"], true);
    }

    #[test]
    fn a_dispatch_status_carries_the_works_result_records() {
        let item: tend::review::ReviewItem = serde_json::from_value(json!({
            "id": "rv_1", "generation": 2, "summary": "Login fixed",
            "target": {"kind": "work", "workspace": "web", "work": "ENG-1", "alias": "impl"},
            "requested_at": "2026-11-12T11:00:00Z", "requested_by": {"kind": "agent"},
            "decision": {"needed": "Ship it?", "recommendation": "yes",
                         "evidence": [{"kind": "commit", "sha": "abc123"}]},
            "disposition": {"kind": "accepted", "by": {"kind": "user"},
                            "at": "2026-11-12T11:30:00Z", "note": "fine"}
        }))
        .unwrap();
        let many: Vec<_> = (0..MAX_STATUS_RESULTS + 5)
            .map(|n| {
                let mut item = item.clone();
                item.id = format!("rv_{n}");
                item
            })
            .collect();
        let (records, dropped) = result_records(&many);
        assert!(dropped);
        let records = records.as_array().unwrap().clone();
        assert_eq!(records.len(), MAX_STATUS_RESULTS);
        assert_eq!(records[0]["id"], "rv_5", "the newest, oldest first");
        assert_eq!(records[MAX_STATUS_RESULTS - 1]["id"], "rv_24");
        assert_eq!(
            records[0],
            json!({
                "id": "rv_5", "alias": "impl", "generation": 2, "summary": "Login fixed",
                "decision_needed": "Ship it?", "options": [], "recommendation": "yes",
                "evidence": [{"kind": "commit", "sha": "abc123"}],
                "requested_at": "2026-11-12T11:00:00Z",
                "disposition": "accepted", "note": "fine"
            })
        );
        // Maximum-size legal evidence stays under the status byte budget.
        let mut heavy = item.clone();
        heavy.decision.as_mut().unwrap().evidence = (0..16)
            .map(|_| tend::decision::Evidence::Diff {
                range: "a".repeat(2048),
                repo: Some(format!("/{}", "r".repeat(2047))),
            })
            .collect();
        let heavy: Vec<_> = (0..MAX_STATUS_RESULTS).map(|_| heavy.clone()).collect();
        let (records, dropped) = result_records(&heavy);
        assert!(dropped);
        assert!(serde_json::to_vec(&records).unwrap().len() <= MAX_STATUS_RESULT_BYTES);
        assert!(!records.as_array().unwrap().is_empty());
    }
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tend::orchestration::ExecutionPaths;
    fn record() -> Record {
        Record {
            plan: DispatchPlan {
                request: DispatchRequest {
                    dispatch_id: uuid::Uuid::new_v4().to_string(),
                    workspace: "tend".into(),
                    work: "test".into(),
                    commit: "a".repeat(40),
                    body: "test".into(),
                    selector: None,
                    host: None,
                },
                node_id: tend::NodeId::generate(),
                host: "local".into(),
                repo: "tend".into(),
                url: "git@example:tend.git".into(),
                pipeline: "solo".into(),
                paths: ExecutionPaths::default(),
                reason: "test".into(),
            },
            state: "preparing".into(),
            result: None,
            error: None,
        }
    }
    #[tokio::test]
    async fn prepares_exact_commit_in_isolated_worktree_and_refuses_reuse() {
        let dir = tempfile::tempdir().unwrap();
        let origin = dir.path().join("origin");
        let socket = dir.path().join("unused.sock");
        git(&["init".into(), origin.display().to_string()], &socket)
            .await
            .unwrap();
        std::fs::write(origin.join("file"), "original").unwrap();
        git(
            &[
                "-C".into(),
                origin.display().to_string(),
                "add".into(),
                "file".into(),
            ],
            &socket,
        )
        .await
        .unwrap();
        git(
            &[
                "-C".into(),
                origin.display().to_string(),
                "-c".into(),
                "user.name=Test".into(),
                "-c".into(),
                "user.email=test@example.invalid".into(),
                "-c".into(),
                "commit.gpgsign=false".into(),
                "commit".into(),
                "-m".into(),
                "fixture".into(),
            ],
            &socket,
        )
        .await
        .unwrap();
        let head = git(
            &[
                "-C".into(),
                origin.display().to_string(),
                "rev-parse".into(),
                "HEAD".into(),
            ],
            &socket,
        )
        .await
        .unwrap();
        let mut plan = record().plan;
        plan.url = origin.display().to_string();
        plan.request.commit = head.clone();
        plan.paths.root = dir.path().join("managed").display().to_string();
        let paths = plan.resolve_paths(dir.path()).unwrap();
        let mut second_plan = plan.clone();
        second_plan.request.dispatch_id = uuid::Uuid::new_v4().to_string();
        second_plan.request.work = "parallel".into();
        let second_paths = second_plan.resolve_paths(dir.path()).unwrap();
        let (first, second) = tokio::join!(
            prepare_checkout(&plan, &paths, &socket),
            prepare_checkout(&second_plan, &second_paths, &socket),
        );
        first.unwrap();
        second.unwrap();
        assert!(second_paths.run.join("file").exists());
        assert_eq!(
            git(
                &[
                    "-C".into(),
                    paths.run.display().to_string(),
                    "rev-parse".into(),
                    "HEAD".into()
                ],
                &socket
            )
            .await
            .unwrap(),
            head
        );
        std::fs::write(paths.run.join("file"), "worker edit").unwrap();
        assert_eq!(
            std::fs::read_to_string(origin.join("file")).unwrap(),
            "original"
        );
        assert!(prepare_checkout(&plan, &paths, &socket).await.is_err());
        assert!(paths.artifacts.join("dispatch.json").exists());
    }

    #[test]
    fn exclusive_journal_survives_reopen_and_never_overwrites_a_reservation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dispatch.json");
        let first = record();
        reserve(&path, &first).unwrap();
        assert!(reserve(&path, &record()).is_err());
        assert_eq!(load(&path).unwrap().unwrap().plan, first.plan);
        let mut finished = first;
        finished.state = "launched".into();
        save(&path, &finished).unwrap();
        assert_eq!(load(&path).unwrap().unwrap().state, "launched");
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    #[test]
    fn torn_journal_blocks_reexecution_and_coordinator_loops_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dispatch.json");
        std::fs::write(&path, "{partial").unwrap();
        assert!(load(&path).is_err());
        assert!(reserve(&path, &record()).is_err());
        let mut cfg = Config::default();
        cfg.orchestration.enabled = true;
        cfg.orchestration.coordinator = Some("another".into());
        assert!(coordinator(&cfg, true).is_err());
        assert_eq!(coordinator(&cfg, false).unwrap(), Some("another"));
    }
}
