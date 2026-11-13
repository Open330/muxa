use super::{RequestBody, Response};
use crate::dashboard::work_store::SharedWorkStore;
use crate::pipeline_run::PipelineRunStore;
use crate::review::ReviewStore;

/// Serve `work_backlog`, `work_edit` and `work_record_show`.
pub(super) async fn response(
    work_store: &SharedWorkStore,
    reviews: &ReviewStore,
    pipeline_runs: &PipelineRunStore,
    body: RequestBody,
    declarer: Option<crate::work_verify::Declarer>,
) -> (&'static str, Response) {
    match body {
        RequestBody::WorkBacklog { stage } => {
            let mut records = work_store.lock().await.records();
            if let Some(stage) = stage {
                records.retain(|record| record.metadata.stage == stage);
            }
            crate::work::sort_backlog(&mut records);
            (
                "work_backlog",
                Response {
                    work_records: Some(records),
                    ..Response::ok()
                },
            )
        }
        RequestBody::WorkEdit { identity, edit } => {
            let result = work_store
                .lock()
                .await
                .edit_as(identity, edit, declarer.as_ref())
                .await;
            (
                "work_edit",
                match result {
                    Ok(record) => Response {
                        work_record: Some(Box::new(record)),
                        ..Response::ok()
                    },
                    Err(error) => Response::err(error),
                },
            )
        }
        RequestBody::WorkRecordShow { identity, newest } => {
            let record = work_store.lock().await.get(&identity);
            let run = pipeline_runs.get(&identity).await;
            let items = reviews
                .for_work(
                    identity.workspace_id.clone(),
                    identity.work_id.clone(),
                    newest.unwrap_or(crate::review::MAX_LIST_LIMIT),
                    newest.is_some(),
                )
                .await;
            if record.is_none() && run.is_none() && items.as_ref().is_ok_and(|p| p.items.is_empty())
            {
                return (
                    "work_record_show",
                    Response::err(format!("no Work record for {}", identity.key())),
                );
            }
            let mut response = Response::ok();
            response.work_record = record.map(Box::new);
            response.pipeline_run = run;
            match items {
                Ok(page) => {
                    response.review_items = Some(page.items);
                    response.review_has_more = Some(page.has_more);
                }
                // The record still reads without its result records.
                Err(error) => response.error = Some(format!("result records: {error}")),
            }
            ("work_record_show", response)
        }
        RequestBody::WorkExternalUpsert { identity, item } => {
            if let Err(error) =
                crate::work::validate_identity(&identity).and_then(|()| item.validate())
            {
                return ("work_external_upsert", Response::err(error));
            }
            let result = work_store
                .lock()
                .await
                .upsert_external_item(identity, item)
                .await;
            (
                "work_external_upsert",
                match result {
                    Ok(record) => Response {
                        work_record: Some(Box::new(record)),
                        ..Response::ok()
                    },
                    Err(error) => Response::err(format!("saving the Work record: {error}")),
                },
            )
        }
        _ => ("work", Response::err("not a Work backlog request")),
    }
}
