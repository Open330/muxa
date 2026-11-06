//! CLI access to the same worker routing and permission contract as muxa_call_peer.
use anyhow::{bail, Context, Result};
use clap::Parser;
use muxa::{config::Config, ipc::Client};
use serde_json::{json, Value};

#[derive(Debug, Parser)]
#[allow(clippy::struct_excessive_bools)] // independent CLI switches, not internal state
pub(crate) struct Args {
    /// Provider (codex/claude), pane, alias, role, or auto for an opposite-provider review.
    pub target: String,
    /// Bounded instructions. Review and question requests are read-only.
    pub body: String,
    #[arg(long, default_value = "review", value_parser = ["review", "question", "task"])]
    pub intent: String,
    /// Authorize edits within the user's delegated scope; requires --intent task.
    #[arg(long)]
    pub execute: bool,
    /// Require this exact conversation; protected primaries then need explicit acceptance.
    #[arg(long)]
    pub context_bound: bool,
    /// Reuse workers only; do not create one when missing.
    #[arg(long)]
    pub no_spawn: bool,
    /// Wait for the structured reply instead of returning its durable request id.
    #[arg(long)]
    pub wait: bool,
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..=3600))]
    pub timeout_secs: u64,
    #[arg(long = "path")]
    pub paths: Vec<String>,
}

pub(crate) async fn run(client: &Client, config: &Config, args: Args) -> Result<()> {
    let mut request = json!({
        "target": args.target, "body": args.body, "intent": args.intent,
        "execute": args.execute, "context_bound": args.context_bound,
        "wait": args.wait, "timeout_secs": args.timeout_secs, "paths": args.paths,
    });
    // Omission preserves the user's auto_spawn_workers policy.
    if args.no_spawn {
        request["spawn_if_missing"] = json!(false);
    }
    let result = crate::mcp::call_peer(client, &request, config).await;
    print_result(&result)
}

fn print_result(result: &Value) -> Result<()> {
    let text = result["content"][0]["text"]
        .as_str()
        .context("peer call returned no receipt")?;
    if result["isError"].as_bool() == Some(true) {
        bail!("{text}");
    }
    // Keep correlation data even on timeout; never hide an already-sent request.
    let receipt: Value = serde_json::from_str(text).context("invalid peer receipt")?;
    println!("{text}");
    if receipt["reason"] == "timeout" {
        bail!("peer reply timed out; resume the same request with muxa msg wait REQUEST_ID");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn review_defaults_do_not_wait_or_escalate_permissions() {
        let args = Args::try_parse_from(["peer", "%16", "Read-only review"]).unwrap();
        assert_eq!(args.intent, "review");
        assert!(!args.execute && !args.context_bound && !args.wait && !args.no_spawn);
        assert!(Args::try_parse_from(["peer", "codex", "review", "--timeout-secs", "0"]).is_err());
    }

    #[test]
    fn peer_errors_are_not_successful_delivery() {
        assert!(
            print_result(&json!({"isError": true,"content":[{"text":"no worker available"}]}))
                .is_err()
        );
        assert!(print_result(
            &json!({"content":[{"text":"{\"request_id\":\"req_1\",\"reason\":\"timeout\"}"}]})
        )
        .is_err());
        assert!(print_result(&json!({"content":[{"text":"{\"request_id\":\"req_1\",\"reason\":\"sent_without_waiting\"}"}]})).is_ok());
    }
}
