//! Filing a result record and completing its pipeline alias as one daemon
//! operation (`work_done_record_v1`).
//!
//! The record lives in the review store (SQLite) and the completion in the
//! pipeline Run store (JSON), so no single transaction covers both. Instead
//! the completion is written down first, record included
//! ([`PipelineRunStore::begin_completion`]), then the record is filed under
//! its idempotency key, then the alias completes. A crash anywhere in
//! between leaves the written-down completion, and [`recover`] finishes it
//! at the next start: filing the record again returns the item already
//! filed, or files it if it never was. So a filed result is never left
//! with its alias still open.

use crate::pipeline_run::{Began, PendingCompletion, PipelineRun, PipelineRunStore};
use crate::review::{NewReview, ReviewItem, ReviewStore, ReviewTarget};
use crate::work::WorkIdentity;

/// Advertised by a daemon whose `pipeline_done` takes a `record`.
pub const CAPABILITY: &str = "work_done_record_v1";

/// Refuse a record that is not this alias's result for this generation, or
/// that has no key to make a retry (or a recovery) return the same item.
///
/// # Errors
/// Says what does not match.
pub fn check_record(
    record: &NewReview,
    identity: &WorkIdentity,
    alias: &str,
    generation: u64,
) -> Result<(), String> {
    let matches = matches!(
        &record.target,
        ReviewTarget::Work {
            workspace,
            work,
            alias: Some(record_alias),
            pipeline_generation: Some(record_generation),
        } if *workspace == identity.workspace_id
            && *work == identity.work_id
            && record_alias == alias
            && *record_generation == generation
    );
    if !matches {
        return Err(format!(
            "the result record must target {}/{} alias {alias:?} generation {generation}",
            identity.workspace_id, identity.work_id
        ));
    }
    if record
        .idempotency_key
        .as_deref()
        .is_none_or(|key| key.trim().is_empty())
    {
        return Err("a result record filed with a completion needs an idempotency key".into());
    }
    Ok(())
}

/// Write the completion down, file its record, complete the alias.
///
/// # Errors
/// A record that could not be filed leaves the alias open and nothing
/// written down, so the caller can retry. A filed record whose completion
/// failed says so with the item id; the completion stays written down
/// unless an invalidation since has replaced it.
pub async fn complete_with_record(
    runs: &PipelineRunStore,
    reviews: &ReviewStore,
    identity: &WorkIdentity,
    alias: &str,
    pending: PendingCompletion,
) -> Result<(PipelineRun, ReviewItem), String> {
    let generation = pending.generation;
    let key = pending.record.idempotency_key.clone();
    let began = runs
        .begin_completion(identity, alias, pending.clone())
        .await
        .map_err(|error| error.to_string())?;
    let item = match reviews
        .create_verified(pending.record, pending.verification)
        .await
    {
        Ok(item) => item,
        Err(error) => {
            // Only what this call wrote down: an earlier one with the same
            // record may already have filed it and must still roll forward.
            if began == Began::Created {
                let _ = runs
                    .abandon_completion(identity, alias, generation, key.as_deref())
                    .await;
            }
            return Err(format!(
                "filing the result record failed ({error}); {alias:?} is not reported done"
            ));
        }
    };
    match runs.done(identity, alias, generation).await {
        Ok(run) => Ok((run, item)),
        Err(error) => Err(format!(
            "result record {} was filed, but completing {alias:?} failed: {error}",
            item.id
        )),
    }
}

/// What [`recover`] did with one written-down completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovered {
    /// Record filed (or found) and the alias completed.
    Completed { alias: String, item: String },
    /// The alias was restarted since; dropped without filing.
    Stale { alias: String },
    /// Could not finish now; kept for the next start.
    Kept { alias: String, reason: String },
    /// The record was filed but the alias could no longer complete.
    Dropped { alias: String, reason: String },
}

/// Finish every completion a crash interrupted. Run once at startup, before
/// anything else can complete or restart an alias.
pub async fn recover(runs: &PipelineRunStore, reviews: &ReviewStore) -> Vec<Recovered> {
    let mut outcomes = Vec::new();
    for (identity, alias, pending, current) in runs.pending_completions().await {
        let generation = pending.generation;
        let key = pending.record.idempotency_key.clone();
        if !current {
            let _ = runs
                .abandon_completion(&identity, &alias, generation, key.as_deref())
                .await;
            outcomes.push(Recovered::Stale { alias });
            continue;
        }
        let item = match reviews
            .create_verified(pending.record, pending.verification)
            .await
        {
            Ok(item) => item,
            Err(error) => {
                outcomes.push(Recovered::Kept {
                    alias,
                    reason: error.to_string(),
                });
                continue;
            }
        };
        match runs.done(&identity, &alias, generation).await {
            Ok(_) => outcomes.push(Recovered::Completed {
                alias,
                item: item.id,
            }),
            Err(error @ crate::pipeline_run::PipelineRunError::Io(_)) => {
                outcomes.push(Recovered::Kept {
                    alias,
                    reason: error.to_string(),
                });
            }
            Err(error) => {
                let _ = runs
                    .abandon_completion(&identity, &alias, generation, key.as_deref())
                    .await;
                outcomes.push(Recovered::Dropped {
                    alias,
                    reason: error.to_string(),
                });
            }
        }
    }
    outcomes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::DesiredAgent;
    use crate::pipeline_run::{PipelineAliasStatus, PipelineRunRegistration};
    use crate::review::{ActorKind, ReviewActor};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn identity() -> WorkIdentity {
        WorkIdentity::new("ws", "WORK-1")
    }

    fn registration() -> PipelineRunRegistration {
        PipelineRunRegistration {
            identity: identity(),
            pipeline: "solo".to_string(),
            desired: vec![DesiredAgent {
                alias: "impl".to_string(),
                program: "codex".to_string(),
                role: None,
                task: None,
                prompt: Some("do it".to_string()),
                options: Vec::new(),
                direction: None,
                after: Vec::new(),
                purpose: crate::collaboration::LaunchPurpose::Primary,
            }],
            cwd: PathBuf::from("/tmp/work"),
            window_id: None,
            observed: Vec::new(),
            invalidate: Vec::new(),
        }
    }

    fn record(generation: u64) -> NewReview {
        NewReview {
            target: ReviewTarget::Work {
                workspace: "ws".to_string(),
                work: "WORK-1".to_string(),
                alias: Some("impl".to_string()),
                pipeline_generation: Some(generation),
            },
            summary: "implemented it".to_string(),
            anchor: None,
            idempotency_key: Some(format!("work-done:test-{generation}")),
            requested_by: ReviewActor {
                kind: ActorKind::Agent,
                label: Some("impl".to_string()),
            },
            decision: None,
        }
    }

    fn pending(generation: u64) -> PendingCompletion {
        PendingCompletion {
            generation,
            record: record(generation),
            verification: None,
        }
    }

    /// A file-backed Run store with `impl` started, and its generation.
    async fn started(path: &std::path::Path) -> (Arc<PipelineRunStore>, u64) {
        let runs = PipelineRunStore::load(Some(path.to_path_buf())).unwrap();
        let run = runs.register(registration()).await.unwrap();
        runs.claim_ready(&run.identity, run.generation)
            .await
            .unwrap();
        (runs, run.generation)
    }

    async fn status(runs: &PipelineRunStore) -> (PipelineAliasStatus, bool) {
        let run = runs
            .list()
            .await
            .into_iter()
            .find(|run| run.identity == identity())
            .unwrap();
        let state = &run.aliases["impl"];
        (state.status, state.completing.is_some())
    }

    async fn items(reviews: &ReviewStore) -> Vec<ReviewItem> {
        reviews
            .list(crate::review::ReviewQuery {
                open_only: false,
                oldest_first: false,
                limit: 50,
                offset: 0,
            })
            .await
            .unwrap()
            .items
    }

    #[tokio::test]
    async fn the_record_and_the_completion_land_together() {
        let dir = tempfile::tempdir().unwrap();
        let (runs, generation) = started(&dir.path().join("runs.json")).await;
        let reviews = ReviewStore::in_memory();
        let (run, item) =
            complete_with_record(&runs, &reviews, &identity(), "impl", pending(generation))
                .await
                .unwrap();
        assert_eq!(run.aliases["impl"].status, PipelineAliasStatus::Done);
        assert!(run.aliases["impl"].completing.is_none());
        // A retry returns the same item and completes again harmlessly.
        let (_, again) =
            complete_with_record(&runs, &reviews, &identity(), "impl", pending(generation))
                .await
                .unwrap();
        assert_eq!(again.id, item.id);
        assert_eq!(items(&reviews).await.len(), 1);
    }

    #[tokio::test]
    async fn a_crash_before_filing_is_filed_and_completed_at_the_next_start() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runs.json");
        let (runs, generation) = started(&path).await;
        runs.begin_completion(&identity(), "impl", pending(generation))
            .await
            .unwrap();
        drop(runs);
        // The next daemon: the completion was written down.
        let runs = PipelineRunStore::load(Some(path)).unwrap();
        assert_eq!(status(&runs).await, (PipelineAliasStatus::Running, true));
        let reviews = ReviewStore::in_memory();
        let outcomes = recover(&runs, &reviews).await;
        let filed = items(&reviews).await;
        assert_eq!(filed.len(), 1);
        assert_eq!(
            outcomes,
            vec![Recovered::Completed {
                alias: "impl".to_string(),
                item: filed[0].id.clone()
            }]
        );
        assert_eq!(status(&runs).await, (PipelineAliasStatus::Done, false));
    }

    #[tokio::test]
    async fn a_crash_after_filing_completes_without_a_second_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runs.json");
        let reviews_path = dir.path().join("reviews.sqlite");
        let (runs, generation) = started(&path).await;
        runs.begin_completion(&identity(), "impl", pending(generation))
            .await
            .unwrap();
        let reviews = ReviewStore::open(&reviews_path).unwrap();
        let first = reviews.create(record(generation)).await.unwrap();
        drop((runs, reviews));

        let runs = PipelineRunStore::load(Some(path)).unwrap();
        let reviews = ReviewStore::open(&reviews_path).unwrap();
        let outcomes = recover(&runs, &reviews).await;
        assert_eq!(
            outcomes,
            vec![Recovered::Completed {
                alias: "impl".to_string(),
                item: first.id.clone()
            }]
        );
        assert_eq!(items(&reviews).await.len(), 1);
        assert_eq!(status(&runs).await, (PipelineAliasStatus::Done, false));
        // Nothing left for a later start.
        assert!(recover(&runs, &reviews).await.is_empty());
    }

    #[tokio::test]
    async fn a_record_that_cannot_be_filed_leaves_the_alias_open_and_nothing_pending() {
        let dir = tempfile::tempdir().unwrap();
        let (runs, generation) = started(&dir.path().join("runs.json")).await;
        let reviews = ReviewStore::unavailable("test: no store");
        let error = complete_with_record(&runs, &reviews, &identity(), "impl", pending(generation))
            .await
            .unwrap_err();
        assert!(error.contains("not reported done"), "{error}");
        assert_eq!(status(&runs).await, (PipelineAliasStatus::Running, false));
    }

    #[tokio::test]
    async fn recovery_keeps_a_completion_it_cannot_file_yet() {
        let dir = tempfile::tempdir().unwrap();
        let (runs, generation) = started(&dir.path().join("runs.json")).await;
        runs.begin_completion(&identity(), "impl", pending(generation))
            .await
            .unwrap();
        let outcomes = recover(&runs, &ReviewStore::unavailable("test: down")).await;
        assert!(
            matches!(&outcomes[..], [Recovered::Kept { .. }]),
            "{outcomes:?}"
        );
        assert_eq!(status(&runs).await, (PipelineAliasStatus::Running, true));
    }

    #[tokio::test]
    async fn a_failed_retry_keeps_an_earlier_completion_that_may_be_filed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runs.json");
        let reviews_path = dir.path().join("reviews.sqlite");
        let (runs, generation) = started(&path).await;
        // Written down and filed, then the daemon died before `done`.
        runs.begin_completion(&identity(), "impl", pending(generation))
            .await
            .unwrap();
        let first = ReviewStore::open(&reviews_path)
            .unwrap()
            .create(record(generation))
            .await
            .unwrap();
        drop(runs);
        // Back up with the review store down: recovery keeps it, and a
        // retry of the same record fails to file but must not drop it.
        let runs = PipelineRunStore::load(Some(path.clone())).unwrap();
        let down = ReviewStore::unavailable("test: down");
        assert!(matches!(
            &recover(&runs, &down).await[..],
            [Recovered::Kept { .. }]
        ));
        assert!(
            complete_with_record(&runs, &down, &identity(), "impl", pending(generation))
                .await
                .is_err()
        );
        assert_eq!(status(&runs).await, (PipelineAliasStatus::Running, true));
        drop(runs);
        // With the store back, the next start finishes it with that item.
        let runs = PipelineRunStore::load(Some(path)).unwrap();
        let reviews = ReviewStore::open(&reviews_path).unwrap();
        assert_eq!(
            recover(&runs, &reviews).await,
            vec![Recovered::Completed {
                alias: "impl".to_string(),
                item: first.id
            }]
        );
        assert_eq!(status(&runs).await, (PipelineAliasStatus::Done, false));
    }

    #[tokio::test]
    async fn another_record_cannot_replace_a_completion_in_progress() {
        let dir = tempfile::tempdir().unwrap();
        let (runs, generation) = started(&dir.path().join("runs.json")).await;
        runs.begin_completion(&identity(), "impl", pending(generation))
            .await
            .unwrap();
        let mut other = pending(generation);
        other.record.idempotency_key = Some("work-done:someone-else".to_string());
        let error = runs
            .begin_completion(&identity(), "impl", other.clone())
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("still being completed"),
            "{error}"
        );
        // Nor can it abandon the one in progress.
        runs.abandon_completion(
            &identity(),
            "impl",
            generation,
            other.record.idempotency_key.as_deref(),
        )
        .await
        .unwrap();
        assert_eq!(status(&runs).await, (PipelineAliasStatus::Running, true));
        assert_eq!(
            runs.begin_completion(&identity(), "impl", pending(generation))
                .await
                .unwrap(),
            Began::Existing
        );
    }

    #[tokio::test]
    async fn a_restart_of_the_alias_drops_its_unfinished_completion() {
        let dir = tempfile::tempdir().unwrap();
        let (runs, generation) = started(&dir.path().join("runs.json")).await;
        runs.begin_completion(&identity(), "impl", pending(generation))
            .await
            .unwrap();
        runs.invalidate(&identity(), "impl", generation)
            .await
            .unwrap();
        let reviews = ReviewStore::in_memory();
        assert!(recover(&runs, &reviews).await.is_empty());
        assert!(items(&reviews).await.is_empty());
        let (status, completing) = status(&runs).await;
        assert_ne!(status, PipelineAliasStatus::Done);
        assert!(!completing);
    }

    #[tokio::test]
    async fn a_completion_is_written_down_only_for_an_alias_that_may_complete() {
        let runs = PipelineRunStore::in_memory();
        let run = runs.register(registration()).await.unwrap();
        // Not started yet: the same refusal `done` gives.
        let error = runs
            .begin_completion(&identity(), "impl", pending(run.generation))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("has not started"), "{error}");
        let error = runs
            .begin_completion(&identity(), "impl", pending(run.generation + 7))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("stale completion"), "{error}");
    }

    #[test]
    fn the_record_must_be_this_aliases_result_with_a_key() {
        let id = identity();
        assert!(check_record(&record(3), &id, "impl", 3).is_ok());
        assert!(check_record(&record(3), &id, "impl", 4).is_err());
        assert!(check_record(&record(3), &id, "review", 3).is_err());
        assert!(check_record(&record(3), &WorkIdentity::new("ws", "OTHER"), "impl", 3).is_err());
        let mut keyless = record(3);
        keyless.idempotency_key = None;
        assert!(check_record(&keyless, &id, "impl", 3)
            .unwrap_err()
            .contains("idempotency key"));
        let mut session = record(3);
        session.target = ReviewTarget::Session {
            provider: None,
            session_id: "s".to_string(),
        };
        assert!(check_record(&session, &id, "impl", 3).is_err());
    }
}
