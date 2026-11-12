use super::{Client, RuntimeError, PROTOCOL_VERSION};
use crate::work::{WorkEdit, WorkIdentity, WorkRecord, WorkStage};

/// One Work as `work_record_show` returns it: the record (absent for a Work
/// that only ever ran), its durable Run, and its result records.
#[derive(Debug, Clone)]
pub struct WorkShow {
    pub record: Option<WorkRecord>,
    pub run: Option<crate::pipeline_run::PipelineRun>,
    pub reviews: Vec<crate::review::ReviewItem>,
    pub reviews_more: bool,
    /// Set when the result records could not be read.
    pub reviews_error: Option<String>,
}

impl Client {
    /// Work records in backlog order. Requires `work_backlog_v1`.
    pub async fn work_backlog(
        &self,
        stage: Option<WorkStage>,
    ) -> Result<Vec<WorkRecord>, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "work_backlog",
            "stage": stage,
        });
        let response = self.call_checked(&req).await?;
        serde_json::from_value(response["work_records"].clone()).map_err(RuntimeError::Json)
    }

    /// Create or partially edit one Work record without launching it.
    pub async fn work_edit(
        &self,
        identity: &WorkIdentity,
        edit: &WorkEdit,
    ) -> Result<WorkRecord, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "work_edit",
            "identity": identity,
            "edit": edit,
        });
        let response = self.call_checked(&req).await?;
        serde_json::from_value(response["work_record"].clone()).map_err(RuntimeError::Json)
    }

    pub async fn work_record_show(
        &self,
        identity: &WorkIdentity,
    ) -> Result<WorkShow, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "work_record_show",
            "identity": identity,
        });
        let response = self.call_checked(&req).await?;
        Ok(WorkShow {
            record: optional(&response, "work_record")?,
            run: optional(&response, "pipeline_run")?,
            reviews: optional(&response, "review_items")?.unwrap_or_default(),
            reviews_more: response["review_has_more"].as_bool().unwrap_or(false),
            reviews_error: response["error"].as_str().map(str::to_owned),
        })
    }
}

fn optional<T: serde::de::DeserializeOwned>(
    response: &serde_json::Value,
    name: &str,
) -> Result<Option<T>, RuntimeError> {
    let value = response[name].clone();
    if value.is_null() {
        Ok(None)
    } else {
        serde_json::from_value(value)
            .map(Some)
            .map_err(RuntimeError::Json)
    }
}
