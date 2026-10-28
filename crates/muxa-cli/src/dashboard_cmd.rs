//! `muxa dashboard` — `muxa watch` opened on its Work board screen.
//!
//! The dashboard's flags are kept so existing invocations and the `prefix+D`
//! binding keep working; each one maps onto the board for that run only.

use anyhow::{Context, Result};
use clap::ValueEnum;
use muxa::config::WatchBoardSort;

use crate::ThemeArg;

#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// Include agents that are not attached to a tmux/zellij pane or muxa PTY.
    #[arg(long)]
    include_paneless: bool,

    /// ACT/WACT time window. Accepts the same values as `muxa stats --since`.
    #[arg(long, default_value = "today")]
    since: String,

    /// Work order: attention (default), activity or name.
    #[arg(long, value_enum, default_value_t = DashboardSort::Attention)]
    sort: DashboardSort,

    /// One-shot visual theme override.
    #[arg(long, value_enum)]
    theme: Option<ThemeArg>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum DashboardSort {
    Attention,
    Activity,
    /// Kept so the old spelling gets an explanation rather than a parse
    /// error; see [`Args::board_sort`].
    Active,
    Name,
}

impl Args {
    /// The Work board's order for this invocation. `active` ordered cards by
    /// active time, which the board cannot attribute per Work yet; it is
    /// refused rather than quietly mapped to something else.
    pub(crate) fn board_sort(&self) -> Result<WatchBoardSort> {
        Ok(match self.sort {
            DashboardSort::Attention => WatchBoardSort::Attention,
            DashboardSort::Activity => WatchBoardSort::Activity,
            DashboardSort::Name => WatchBoardSort::Name,
            DashboardSort::Active => anyhow::bail!(
                "`--sort active` ordered Works by active time, which the Work board cannot \
                 attribute per Work yet; use --sort attention, activity or name (or `busy` \
                 in the board for agents at work)"
            ),
        })
    }

    /// The ACT/WACT window, checked the way `muxa stats --since` checks it.
    pub(crate) fn board_since(&self) -> Result<String> {
        crate::stats::parse_since(&self.since, time::OffsetDateTime::now_utc())
            .with_context(|| format!("invalid --since {:?}", self.since))?;
        Ok(self.since.clone())
    }

    pub(crate) fn include_paneless(&self) -> bool {
        self.include_paneless
    }

    pub(crate) fn theme(&self) -> Option<ThemeArg> {
        self.theme
    }
}
