use anyhow::{anyhow, Result};
use clap::Args;
use muxa::ipc::Client;

#[derive(Debug, Args)]
pub struct FindArgs {
    /// Task identifier or keywords (all must match). For prose, extract a few keywords.
    pub query: String,
    #[arg(long, default_value_t = 10)]
    pub limit: usize,
    /// Exclude an investigating agent session (repeatable).
    #[arg(long = "exclude-session")]
    pub exclude_sessions: Vec<String>,
    /// Re-observe pane inventory now instead of reusing the daemon's short cache.
    #[arg(long)]
    pub refresh: bool,
    #[arg(long)]
    pub json: bool,
}

pub async fn lookup(
    client: &Client,
    query: &str,
    limit: usize,
    exclude: &[String],
    refresh: bool,
) -> Result<muxa::agent_locator::LocationResults> {
    muxa::agent_locator::validate(query, limit).map_err(|error| anyhow!(error))?;
    client.agent_find(query, limit, exclude, refresh).await?
        .ok_or_else(|| anyhow!("Agent lookup requires agent_directory_v1 on muxad. Update the daemon together with this CLI; refusing an environment-dependent local fallback."))
}

pub async fn run(client: &Client, args: FindArgs) -> Result<()> {
    let result = lookup(
        client,
        &args.query,
        args.limit,
        &args.exclude_sessions,
        args.refresh,
    )
    .await?;
    if args.json {
        println!("{}", serde_json::to_string(&result)?);
    } else {
        println!("{} matching agents (local observed panes)", result.total);
        if let Some(freshness) = &result.freshness {
            println!(
                "Inventory age: {} ms (maximum {} ms); use --refresh to re-observe.",
                freshness.inventory_age_ms, freshness.max_inventory_age_ms
            );
        }
        if result.unlocated_matches > 0 {
            println!(
                "{} matching agent records have no confirmed location.",
                result.unlocated_matches
            );
        }
        for hit in result.matches {
            println!(
                "\n{} / {} / {}  {} {}\nsocket={} agent_session_id={}",
                hit.workspace,
                hit.window,
                hit.location.pane_id,
                hit.kind,
                hit.state,
                hit.location.window.session.endpoint.socket,
                hit.agent_session_id
            );
            for evidence in hit.evidence {
                println!(
                    "  {}: {}",
                    evidence.field,
                    evidence.excerpt.replace('\n', " ")
                );
            }
        }
        if result.has_more {
            println!("More matches available; narrow the keywords or increase --limit (max 50).");
        }
        for warning in result.warnings {
            eprintln!("Warning: {warning}");
        }
    }
    Ok(())
}
