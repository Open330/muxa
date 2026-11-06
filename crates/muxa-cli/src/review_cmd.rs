//! `muxa review` — explicit review items: claims that a result needs review,
//! kept visible until someone records a disposition.
//!
//! muxa never creates an item from what it observes (a turn stopping, a
//! process exiting, a branch merging). Actors are declared, not
//! authenticated: `accept` records that someone said they accepted the
//! result, not that a person understood it.

use anyhow::{bail, Context, Result};
use muxa::ipc::Client;
use muxa::review::{
    ActorKind, DispositionKind, NewReview, ReviewActor, ReviewItem, ReviewQuery, ReviewTarget,
    REVIEW_CAPABILITY,
};
use std::fmt::Write as _;
use time::OffsetDateTime;

#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    action: Action,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum Actor {
    User,
    Agent,
}

impl From<Actor> for ActorKind {
    fn from(actor: Actor) -> Self {
        match actor {
            Actor::User => Self::User,
            Actor::Agent => Self::Agent,
        }
    }
}

#[derive(Debug, clap::Args)]
struct DisposeArgs {
    /// Review item id (`rv_…`), as `muxa review list` prints it.
    id: String,
    /// One line on why, kept with the disposition.
    #[arg(long)]
    note: Option<String>,
    /// Who is declaring this disposition.
    #[arg(long = "as", value_enum, default_value_t = Actor::User)]
    actor: Actor,
    /// Display label for the declaring actor.
    #[arg(long)]
    label: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, clap::Subcommand)]
enum Action {
    /// Ask for review of one result. Exactly one target: --session, --work
    /// or --request.
    Request {
        /// What to inspect, in your words.
        #[arg(long)]
        summary: String,
        /// An agent session id (for agents without Work).
        #[arg(long, conflicts_with_all = ["work", "request"])]
        session: Option<String>,
        /// Provider of --session (claude, codex, …).
        #[arg(long, requires = "session")]
        provider: Option<String>,
        /// A Work, as WORKSPACE/WORK.
        #[arg(long, conflicts_with_all = ["session", "request"])]
        work: Option<String>,
        /// Pipeline alias within --work.
        #[arg(long, requires = "work")]
        alias: Option<String>,
        /// A collaboration request id whose result needs review.
        #[arg(long, conflicts_with_all = ["session", "work"])]
        request: Option<String>,
        /// Pointer to the evidence (commit, log path, locator). Recorded,
        /// never opened.
        #[arg(long)]
        anchor: Option<String>,
        /// Retry key: the same key returns the same item instead of a new one.
        #[arg(long)]
        key: Option<String>,
        /// Who is asking.
        #[arg(long = "as", value_enum, default_value_t = Actor::User)]
        actor: Actor,
        /// Display label for the asking actor.
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Open review items, newest first.
    List {
        /// Include accepted, returned and withdrawn items.
        #[arg(long)]
        all: bool,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Skip this many items (continue a listing that had more).
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Oldest first, which is where the status-line age comes from.
        #[arg(long)]
        oldest: bool,
        #[arg(long)]
        json: bool,
    },
    /// One review item in full.
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Record that the result was looked at and accepted.
    Accept(DisposeArgs),
    /// Record that the result was sent back for more work.
    Return(DisposeArgs),
    /// Record that the item no longer needs review.
    Withdraw(DisposeArgs),
}

pub(crate) async fn run(args: Args, client: &Client) -> Result<()> {
    require_capability(client).await?;
    match args.action {
        Action::Request {
            summary,
            session,
            provider,
            work,
            alias,
            request,
            anchor,
            key,
            actor,
            label,
            json,
        } => {
            let target = target(session, provider, work, alias, request)?;
            let item = client
                .review_create(&NewReview {
                    target,
                    summary,
                    anchor,
                    idempotency_key: key,
                    requested_by: ReviewActor {
                        kind: actor.into(),
                        label,
                    },
                })
                .await?;
            print_item(&item, json)
        }
        Action::List {
            all,
            limit,
            offset,
            oldest,
            json,
        } => {
            let page = client
                .review_list(&ReviewQuery {
                    open_only: !all,
                    oldest_first: oldest,
                    limit,
                    offset,
                })
                .await?;
            if json {
                println!("{}", serde_json::to_string(&page)?);
            } else {
                print!("{}", render_list(&page.items, OffsetDateTime::now_utc()));
                if page.has_more {
                    println!(
                        "… more: --offset {}{}",
                        offset + page.items.len(),
                        if oldest { " --oldest" } else { "" }
                    );
                }
            }
            Ok(())
        }
        Action::Show { id, json } => print_item(&client.review_get(&id).await?, json),
        Action::Accept(a) => dispose(client, DispositionKind::Accepted, a).await,
        Action::Return(a) => dispose(client, DispositionKind::Returned, a).await,
        Action::Withdraw(a) => dispose(client, DispositionKind::Withdrawn, a).await,
    }
}

async fn require_capability(client: &Client) -> Result<()> {
    let hello = client
        .hello(std::time::Duration::from_secs(3))
        .await
        .context("asking muxad what it supports")?;
    if !hello.capabilities.iter().any(|c| c == REVIEW_CAPABILITY) {
        bail!("this muxad does not serve review items ({REVIEW_CAPABILITY}); restart it with the same muxa build");
    }
    Ok(())
}

fn target(
    session: Option<String>,
    provider: Option<String>,
    work: Option<String>,
    alias: Option<String>,
    request: Option<String>,
) -> Result<ReviewTarget> {
    if let Some(session_id) = session {
        return Ok(ReviewTarget::Session {
            provider,
            session_id,
        });
    }
    if let Some(work) = work {
        let Some((workspace, work)) = work.split_once('/') else {
            bail!("--work takes WORKSPACE/WORK");
        };
        return Ok(ReviewTarget::Work {
            workspace: workspace.into(),
            work: work.into(),
            alias,
            pipeline_generation: None,
        });
    }
    if let Some(request_id) = request {
        return Ok(ReviewTarget::Request {
            request_id,
            attempt_id: None,
        });
    }
    bail!("name what to review: --session, --work or --request")
}

async fn dispose(client: &Client, kind: DispositionKind, args: DisposeArgs) -> Result<()> {
    let by = ReviewActor {
        kind: args.actor.into(),
        label: args.label,
    };
    let item = client
        .review_dispose(&args.id, kind, &by, args.note.as_deref())
        .await?;
    print_item(&item, args.json)
}

fn print_item(item: &ReviewItem, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(item)?);
    } else {
        print!("{}", render_item(item, OffsetDateTime::now_utc()));
    }
    Ok(())
}

fn describe_target(target: &ReviewTarget) -> String {
    match target {
        ReviewTarget::Session {
            provider,
            session_id,
        } => match provider {
            Some(p) => format!("session {p}:{session_id}"),
            None => format!("session {session_id}"),
        },
        ReviewTarget::Work {
            workspace,
            work,
            alias,
            pipeline_generation,
        } => {
            let mut out = format!("work {workspace}/{work}");
            if let Some(alias) = alias {
                let _ = write!(out, " @{alias}");
            }
            if let Some(generation) = pipeline_generation {
                let _ = write!(out, " gen {generation}");
            }
            out
        }
        ReviewTarget::Request {
            request_id,
            attempt_id,
        } => match attempt_id {
            Some(a) => format!("request {request_id} attempt {a}"),
            None => format!("request {request_id}"),
        },
    }
}

fn actor_label(actor: &ReviewActor) -> String {
    let kind = match actor.kind {
        ActorKind::User => "user",
        ActorKind::Agent => "agent",
        ActorKind::Policy => "policy",
    };
    match &actor.label {
        Some(label) => format!("{kind} ({label})"),
        None => kind.to_owned(),
    }
}

fn disposition_label(kind: DispositionKind) -> &'static str {
    match kind {
        DispositionKind::Accepted => "accepted",
        DispositionKind::Returned => "returned",
        DispositionKind::Withdrawn => "withdrawn",
    }
}

fn render_list(items: &[ReviewItem], now: OffsetDateTime) -> String {
    if items.is_empty() {
        return "no review items\n".into();
    }
    let mut out = String::new();
    for item in items {
        let state = item
            .disposition
            .as_ref()
            .map_or("open", |d| disposition_label(d.kind));
        let summary = item.summary.lines().next().unwrap_or("");
        let _ = writeln!(
            out,
            "{}  {:<9} {:>4}  {}  {summary}",
            item.id,
            state,
            crate::attend::humanize_since(item.requested_at, now),
            describe_target(&item.target),
        );
    }
    out
}

fn render_item(item: &ReviewItem, now: OffsetDateTime) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}  (generation {})", item.id, item.generation);
    let _ = writeln!(out, "target:     {}", describe_target(&item.target));
    let _ = writeln!(
        out,
        "requested:  {} ago by {}",
        crate::attend::humanize_since(item.requested_at, now),
        actor_label(&item.requested_by)
    );
    if let Some(anchor) = &item.anchor {
        let _ = writeln!(out, "anchor:     {anchor}");
    }
    match &item.disposition {
        None => {
            let _ = writeln!(out, "state:      open");
        }
        Some(d) => {
            let _ = writeln!(
                out,
                "state:      {} {} ago by {} (declared)",
                disposition_label(d.kind),
                crate::attend::humanize_since(d.at, now),
                actor_label(&d.by)
            );
            if let Some(note) = &d.note {
                let _ = writeln!(out, "note:       {note}");
            }
        }
    }
    let _ = writeln!(out, "\n{}", item.summary);
    out
}

/// tmux status segment for open review items: empty when there are none.
/// Uses a basic Geometric Shapes glyph (U+25C7) so terminals never resize it
/// through font fallback.
pub(crate) fn review_segment(summary: &muxa::review::ReviewSummary, now: OffsetDateTime) -> String {
    if summary.open == 0 {
        return String::new();
    }
    let age = summary
        .oldest_requested_at
        .map(|at| format!(" · {}", crate::attend::humanize_since(at, now)))
        .unwrap_or_default();
    format!("#[fg=yellow]◇ {} to review{age}#[default]", summary.open)
}

#[cfg(test)]
mod tests {
    use super::*;
    use muxa::review::ReviewSummary;
    use time::macros::datetime;

    #[test]
    fn segment_is_empty_with_nothing_open() {
        let now = datetime!(2026-11-06 12:00 UTC);
        assert_eq!(review_segment(&ReviewSummary::default(), now), "");
    }

    #[test]
    fn segment_shows_count_and_oldest_age() {
        let now = datetime!(2026-11-06 12:00 UTC);
        let summary = ReviewSummary {
            open: 4,
            oldest_requested_at: Some(datetime!(2026-11-06 09:00 UTC)),
        };
        assert_eq!(
            review_segment(&summary, now),
            "#[fg=yellow]◇ 4 to review · 3h#[default]"
        );
    }

    #[test]
    fn work_target_requires_workspace_and_work() {
        assert!(target(None, None, Some("nowork".into()), None, None).is_err());
        assert!(target(None, None, None, None, None).is_err());
        let t = target(
            None,
            None,
            Some("muxa/review".into()),
            Some("impl".into()),
            None,
        )
        .unwrap();
        assert_eq!(describe_target(&t), "work muxa/review @impl");
    }
}
