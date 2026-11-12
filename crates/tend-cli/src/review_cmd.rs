//! `tend review` — explicit review items: claims that a result needs review,
//! kept visible until someone records a disposition.
//!
//! tend never creates an item from what it observes (a turn stopping, a
//! process exiting, a branch merging). Actors are declared, not
//! authenticated: `accept` records that someone said they accepted the
//! result, not that a person understood it.

use anyhow::{bail, Context, Result};
use std::fmt::Write as _;
use tend::decision::{Decision, Evidence, REVIEW_DECISION_CAPABILITY};
use tend::ipc::Client;
use tend::review::{
    ActorKind, DispositionKind, NewReview, ReviewActor, ReviewItem, ReviewQuery, ReviewTarget,
    REVIEW_CAPABILITY,
};
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
    /// Review item id (`rv_…`), as `tend review list` prints it.
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

/// Structured decision context for a review item (`review_decision_v1`).
#[derive(Debug, Default, clap::Args)]
pub(crate) struct DecisionArgs {
    /// The decision the person is asked to make.
    #[arg(long, value_name = "TEXT")]
    pub decision_needed: Option<String>,
    /// One choice on offer; repeat for each.
    #[arg(long = "option", value_name = "TEXT")]
    pub options: Vec<String>,
    /// What you would do, and why.
    #[arg(long, value_name = "TEXT")]
    pub recommend: Option<String>,
    /// Evidence: commit:SHA, pr:URL, diff:RANGE, test:CMD, test-pass:CMD,
    /// test-fail:CMD or path:PATH. Repeat for each.
    #[arg(long, value_name = "KIND:VALUE")]
    pub evidence: Vec<String>,
    /// Git checkout that commit: and diff: evidence lives in. Defaults to
    /// the current directory's checkout when it has one.
    #[arg(long, value_name = "DIR")]
    pub repo: Option<std::path::PathBuf>,
}

impl DecisionArgs {
    /// `None` when no decision flag was given.
    pub(crate) fn build(self) -> Result<Option<Decision>> {
        let needs_repo = self
            .evidence
            .iter()
            .any(|spec| spec.starts_with("commit:") || spec.starts_with("diff:"));
        let repo = match self.repo {
            Some(repo) => Some(
                std::path::absolute(&repo)
                    .with_context(|| format!("resolving --repo {}", repo.display()))?,
            ),
            None if needs_repo => current_checkout(),
            None => None,
        };
        let repo = repo.map(|repo| repo.to_string_lossy().into_owned());
        let evidence = self
            .evidence
            .iter()
            .map(|spec| Evidence::parse(spec, repo.as_deref()).map_err(anyhow::Error::msg))
            .collect::<Result<Vec<_>>>()?;
        let decision = Decision {
            needed: self.decision_needed,
            options: self.options,
            recommendation: self.recommend,
            evidence,
        };
        decision.validate().map_err(anyhow::Error::msg)?;
        Ok(decision.into_option())
    }
}

/// The result record `tend work done` files: what was done, and what the
/// person needs to decide about it.
#[derive(Debug, Default, clap::Args)]
pub(crate) struct ResultRecordArgs {
    /// One paragraph on what you finished. Files a review item for the
    /// person; every other record flag needs it.
    #[arg(long, value_name = "TEXT")]
    pub summary: Option<String>,
    /// A commit that holds the result (same as --evidence commit:SHA).
    #[arg(long = "commit", value_name = "SHA")]
    pub commits: Vec<String>,
    #[command(flatten)]
    pub decision: DecisionArgs,
}

impl ResultRecordArgs {
    fn is_empty(&self) -> bool {
        let d = &self.decision;
        self.summary.is_none()
            && self.commits.is_empty()
            && d.decision_needed.is_none()
            && d.options.is_empty()
            && d.recommend.is_none()
            && d.evidence.is_empty()
            && d.repo.is_none()
    }

    /// The review item for one completed pipeline alias, or `None` when no
    /// record flag was given. Keyed by alias and generation, so reporting
    /// the same completion again returns the same item.
    pub(crate) fn into_review(
        self,
        identity: &tend::work::WorkIdentity,
        alias: &str,
        generation: u64,
    ) -> Result<Option<NewReview>> {
        if self.is_empty() {
            return Ok(None);
        }
        let Some(summary) = self.summary else {
            bail!("a result record needs --summary");
        };
        let mut decision = self.decision;
        let commits = self.commits.into_iter().map(|sha| format!("commit:{sha}"));
        decision.evidence.splice(0..0, commits);
        Ok(Some(NewReview {
            target: ReviewTarget::Work {
                workspace: identity.workspace_id.clone(),
                work: identity.work_id.clone(),
                alias: Some(alias.to_owned()),
                pipeline_generation: Some(generation),
            },
            summary,
            anchor: None,
            idempotency_key: Some(result_key(
                &identity.workspace_id,
                &identity.work_id,
                alias,
                generation,
            )),
            requested_by: ReviewActor {
                kind: ActorKind::Agent,
                label: Some(alias.to_owned()),
            },
            decision: decision.build()?,
        }))
    }
}

/// A bounded idempotency key for a completion: `work-done:` and a digest
/// of the JSON tuple, so no component can shift into another (names may
/// contain `/`) and long Work names stay within the key limit.
fn result_key(workspace: &str, work: &str, alias: &str, generation: u64) -> String {
    use sha2::Digest as _;
    let tuple = serde_json::to_string(&(workspace, work, alias, generation)).unwrap_or_default();
    let digest = sha2::Sha256::digest(tuple.as_bytes());
    let mut key = String::from("work-done:");
    for byte in &digest[..16] {
        let _ = write!(key, "{byte:02x}");
    }
    key
}

/// File `review` after checking the daemon serves what it carries.
pub(crate) async fn file(client: &Client, review: &NewReview) -> Result<ReviewItem> {
    require_capability(client).await?;
    if review.decision.is_some() {
        require_decision_capability(client).await?;
    }
    Ok(client.review_create(review).await?)
}

/// Top of the git checkout holding the current directory, if any.
fn current_checkout() -> Option<std::path::PathBuf> {
    checkout_of(std::path::Path::new("."))
}

/// Top of the git checkout holding `dir`, if any.
pub(crate) fn checkout_of(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let top = String::from_utf8(output.stdout).ok()?;
    let top = top.trim();
    (output.status.success() && !top.is_empty()).then(|| top.into())
}

/// Refuse decision fields a daemon would reject as unknown, with the fix.
pub(crate) async fn require_decision_capability(client: &Client) -> Result<()> {
    let hello = client
        .hello(std::time::Duration::from_secs(3))
        .await
        .context("asking tendd what it supports")?;
    if !hello
        .capabilities
        .iter()
        .any(|c| c == REVIEW_DECISION_CAPABILITY)
    {
        bail!("this tendd does not store decision fields ({REVIEW_DECISION_CAPABILITY}); restart it with the same tend build");
    }
    Ok(())
}

#[derive(Debug, clap::Subcommand)]
#[allow(clippy::large_enum_variant)] // parsed once per invocation
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
        #[command(flatten)]
        decision: DecisionArgs,
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
            decision,
            actor,
            label,
            json,
        } => {
            let target = target(session, provider, work, alias, request)?;
            let decision = decision.build()?;
            if decision.is_some() {
                require_decision_capability(client).await?;
            }
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
                    decision,
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
        .context("asking tendd what it supports")?;
    if !hello.capabilities.iter().any(|c| c == REVIEW_CAPABILITY) {
        bail!("this tendd does not serve review items ({REVIEW_CAPABILITY}); restart it with the same tend build");
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
    if let Some(decision) = &item.decision {
        render_decision(&mut out, decision);
    }
    out
}

/// The decision block of `tend review show`: question, options,
/// recommendation, evidence, in that order.
pub(crate) fn render_decision(out: &mut String, decision: &Decision) {
    if let Some(needed) = &decision.needed {
        let _ = writeln!(out, "\ndecision needed:\n  {}", indent(needed));
    }
    if !decision.options.is_empty() {
        let _ = writeln!(out, "\noptions:");
        for (n, option) in decision.options.iter().enumerate() {
            let _ = writeln!(out, "  {}. {option}", n + 1);
        }
    }
    if let Some(recommendation) = &decision.recommendation {
        let _ = writeln!(out, "\nrecommendation:\n  {}", indent(recommendation));
    }
    if !decision.evidence.is_empty() {
        let _ = writeln!(out, "\nevidence:");
        for evidence in &decision.evidence {
            let _ = writeln!(out, "  - {}", evidence.describe());
            if let Evidence::Test {
                tail: Some(tail), ..
            } = evidence
            {
                for line in tail.lines() {
                    let _ = writeln!(out, "      {line}");
                }
            }
        }
    }
}

fn indent(text: &str) -> String {
    text.replace('\n', "\n  ")
}

/// tmux status segment for open review items: empty when there are none.
/// Uses a basic Geometric Shapes glyph (U+25C7) so terminals never resize it
/// through font fallback.
pub(crate) fn review_segment(summary: &tend::review::ReviewSummary, now: OffsetDateTime) -> String {
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
    use tend::review::ReviewSummary;
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
    fn decision_flags_build_a_validated_decision() {
        assert!(DecisionArgs::default().build().unwrap().is_none());
        let decision = DecisionArgs {
            decision_needed: Some("merge?".into()),
            options: vec!["merge".into(), "hold".into()],
            recommend: Some("merge".into()),
            evidence: vec!["commit:abc123".into(), "test-pass:cargo test".into()],
            repo: Some("/src/tend".into()),
        }
        .build()
        .unwrap()
        .unwrap();
        assert_eq!(
            decision.evidence[0],
            Evidence::Commit {
                sha: "abc123".into(),
                repo: Some("/src/tend".into())
            }
        );
        let mut out = String::new();
        render_decision(&mut out, &decision);
        assert!(out.contains("  1. merge\n  2. hold"), "{out}");
        assert!(out.contains("test passed: cargo test"), "{out}");
        let bad = DecisionArgs {
            evidence: vec!["commit:--exec".into()],
            repo: Some("/src/tend".into()),
            ..DecisionArgs::default()
        };
        assert!(bad.build().is_err());
    }

    #[test]
    fn work_done_files_a_keyed_record_only_with_a_summary() {
        let identity = tend::work::WorkIdentity::new("tend", "storage");
        assert!(ResultRecordArgs::default()
            .into_review(&identity, "impl", 3)
            .unwrap()
            .is_none());
        let without_summary = ResultRecordArgs {
            commits: vec!["abc123".into()],
            ..ResultRecordArgs::default()
        };
        assert!(without_summary.into_review(&identity, "impl", 3).is_err());
        let record = |generation| {
            ResultRecordArgs {
                summary: Some("done".into()),
                commits: vec!["abc123".into()],
                decision: DecisionArgs {
                    repo: Some("/src/tend".into()),
                    evidence: vec!["test-pass:cargo test".into()],
                    ..DecisionArgs::default()
                },
            }
            .into_review(&identity, "impl", generation)
            .unwrap()
            .unwrap()
        };
        let first = record(3);
        assert_eq!(first.idempotency_key, record(3).idempotency_key);
        assert_ne!(first.idempotency_key, record(4).idempotency_key);
        assert!(first.idempotency_key.as_ref().unwrap().len() <= 128);
        assert_eq!(
            describe_target(&first.target),
            "work tend/storage @impl gen 3"
        );
        assert_ne!(
            result_key("tend", "STORAGE/1", "impl", 3),
            result_key("tend", "STORAGE", "1/impl", 3),
            "components cannot shift into each other"
        );
        let decision = first.decision.unwrap();
        assert!(matches!(&decision.evidence[0], Evidence::Commit { sha, .. } if sha == "abc123"));
        assert_eq!(decision.evidence.len(), 2);
    }

    #[test]
    fn work_target_requires_workspace_and_work() {
        assert!(target(None, None, Some("nowork".into()), None, None).is_err());
        assert!(target(None, None, None, None, None).is_err());
        let t = target(
            None,
            None,
            Some("tend/review".into()),
            Some("impl".into()),
            None,
        )
        .unwrap();
        assert_eq!(describe_target(&t), "work tend/review @impl");
    }
}
