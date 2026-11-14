//! `tend snapshot` / `tend restore` / `tend reload`. The snapshot and restore
//! code lives in `tend_work::reload`, where tendd runs it too; `tend reload
//! mcp` stays here with the rest of the MCP code.

use anyhow::Result;
use clap::Parser;
use tend::ipc::Client;
pub use tend_work::reload::{
    restore, snapshot, snapshot_before_upgrade, MultiplexerReloadArgs, RestoreArgs, SnapshotArgs,
};

#[derive(Debug, Parser)]
#[command(args_conflicts_with_subcommands = true)]
pub struct ReloadArgs {
    #[command(subcommand)]
    target: Option<ReloadTarget>,
    #[command(flatten)]
    multiplexer: MultiplexerReloadArgs,
}

#[derive(Debug, clap::Subcommand)]
enum ReloadTarget {
    /// Refresh shared Codex app-server MCP connections; standalone clients need their own reconnect
    Mcp(crate::mcp_reload::McpReloadArgs),
    /// Snapshot, restart the multiplexer, and restore the workspace (same as bare reload)
    Mux(MultiplexerReloadArgs),
}

pub async fn reload(client: &Client, args: ReloadArgs) -> Result<()> {
    match args.target {
        Some(ReloadTarget::Mcp(args)) => crate::mcp_reload::run(client, args).await,
        Some(ReloadTarget::Mux(args)) => tend_work::reload::reload_multiplexer(client, args).await,
        None => tend_work::reload::reload_multiplexer(client, args.multiplexer).await,
    }
}
