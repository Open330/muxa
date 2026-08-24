//! Durable muxa work records.
//!
//! Schema v1 keyed annotations by host/socket/session/window. Schema v2 keys
//! them by logical workspace/work identity and retains the physical v1 key
//! only as a migration binding. This lets closing a window end a run without
//! deleting or renaming the work item.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

use crate::backend::HostKind;
use crate::work::{
    ExecutionIdentity, ExternalItemRef, WorkIdentity, WorkMetadata, WorkMetadataPatch, WorkRecord,
    WORK_SCHEMA_VERSION,
};

#[cfg(unix)]
const WORK_STORE_FILE_MODE: u32 = 0o600;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkStoreFile {
    version: u8,
    works: Vec<WorkRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
struct LegacyWorkKey {
    host: HostKind,
    socket: String,
    session_id: String,
    window_id: String,
}

#[derive(Debug, Clone, Deserialize)]
struct LegacyWorkRecord {
    key: LegacyWorkKey,
    metadata: WorkMetadata,
}

#[derive(Debug, Clone, Deserialize)]
struct LegacyWorkStoreFile {
    version: u8,
    works: Vec<LegacyWorkRecord>,
}

#[derive(Debug)]
pub struct WorkStore {
    path: Option<PathBuf>,
    works: BTreeMap<WorkIdentity, WorkRecord>,
}

impl WorkStore {
    pub fn memory() -> Self {
        Self {
            path: None,
            works: BTreeMap::new(),
        }
    }

    pub fn load(path: Option<PathBuf>) -> Self {
        let Some(path) = path else {
            return Self::memory();
        };
        let works = std::fs::read(&path)
            .ok()
            .and_then(|bytes| load_records(&bytes))
            .unwrap_or_default()
            .into_iter()
            .map(|record| (record.identity.clone(), record))
            .collect();
        Self {
            path: Some(path),
            works,
        }
    }

    pub fn records(&self) -> Vec<WorkRecord> {
        self.works.values().cloned().collect()
    }

    pub async fn upsert_metadata(
        &mut self,
        identity: WorkIdentity,
        patch: WorkMetadataPatch,
    ) -> std::io::Result<WorkRecord> {
        let now = OffsetDateTime::now_utc();
        let previous = self.works.get(&identity).cloned();
        let record = WorkRecord {
            identity: identity.clone(),
            metadata: patch.into_metadata(now),
            external_items: previous
                .as_ref()
                .map(|record| record.external_items.clone())
                .unwrap_or_default(),
            legacy_binding: previous
                .as_ref()
                .and_then(|record| record.legacy_binding.clone()),
            created_at: previous.as_ref().map_or(now, |record| record.created_at),
        };
        self.persist_change(identity, record, previous).await
    }

    pub async fn upsert_legacy_metadata(
        &mut self,
        binding: ExecutionIdentity,
        patch: WorkMetadataPatch,
    ) -> std::io::Result<WorkRecord> {
        let existing = self
            .works
            .values()
            .find(|record| record.legacy_binding.as_ref() == Some(&binding))
            .cloned();
        let identity = existing.as_ref().map_or_else(
            || legacy_identity(&binding),
            |record| record.identity.clone(),
        );
        let now = OffsetDateTime::now_utc();
        let record = WorkRecord {
            identity: identity.clone(),
            metadata: patch.into_metadata(now),
            external_items: existing
                .as_ref()
                .map(|record| record.external_items.clone())
                .unwrap_or_default(),
            legacy_binding: Some(binding),
            created_at: existing.as_ref().map_or(now, |record| record.created_at),
        };
        self.persist_change(identity, record, existing).await
    }

    pub async fn upsert_external_item(
        &mut self,
        identity: WorkIdentity,
        item: ExternalItemRef,
    ) -> std::io::Result<WorkRecord> {
        let now = OffsetDateTime::now_utc();
        let previous = self.works.get(&identity).cloned();
        let mut record = previous.clone().unwrap_or_else(|| WorkRecord {
            identity: identity.clone(),
            metadata: WorkMetadata {
                // External title is a reference field. Keeping local title
                // unset lets the snapshot use the latest external title as a
                // fallback without turning provider data into operator-owned
                // Work metadata.
                title: None,
                goal: None,
                next_action: None,
                stage: crate::work::WorkStage::default(),
                updated_at: now,
            },
            external_items: Vec::new(),
            legacy_binding: None,
            created_at: now,
        });
        let same_identity = |candidate: &ExternalItemRef| {
            candidate.source == item.source
                && candidate.scope == item.scope
                && match (&candidate.stable_id, &item.stable_id) {
                    (Some(left), Some(right)) => left == right,
                    _ => candidate.display_key == item.display_key,
                }
        };
        if let Some(existing) = record
            .external_items
            .iter_mut()
            .find(|row| same_identity(row))
        {
            if same_external_snapshot(existing, &item) {
                return Ok(record);
            }
            *existing = item;
        } else {
            record.external_items.push(item);
        }
        self.persist_change(identity, record, previous).await
    }

    async fn persist_change(
        &mut self,
        identity: WorkIdentity,
        record: WorkRecord,
        previous: Option<WorkRecord>,
    ) -> std::io::Result<WorkRecord> {
        self.works.insert(identity.clone(), record.clone());
        if let Some(path) = &self.path {
            if let Err(error) = save(path, self.records()).await {
                if let Some(previous) = previous {
                    self.works.insert(identity, previous);
                } else {
                    self.works.remove(&identity);
                }
                return Err(error);
            }
        }
        Ok(record)
    }
}

fn same_external_snapshot(left: &ExternalItemRef, right: &ExternalItemRef) -> bool {
    left.source == right.source
        && left.scope == right.scope
        && left.stable_id == right.stable_id
        && left.display_key == right.display_key
        && left.title == right.title
        && left.url == right.url
        && left.status == right.status
        && left.item_type == right.item_type
}

fn load_records(bytes: &[u8]) -> Option<Vec<WorkRecord>> {
    let value = serde_json::from_slice::<serde_json::Value>(bytes).ok()?;
    match value.get("version").and_then(serde_json::Value::as_u64) {
        Some(version) if version == u64::from(WORK_SCHEMA_VERSION) => {
            serde_json::from_value::<WorkStoreFile>(value)
                .ok()
                .map(|file| file.works)
        }
        Some(1) => serde_json::from_value::<LegacyWorkStoreFile>(value)
            .ok()
            .map(migrate_v1),
        _ => None,
    }
}

fn migrate_v1(file: LegacyWorkStoreFile) -> Vec<WorkRecord> {
    debug_assert_eq!(file.version, 1);
    file.works
        .into_iter()
        .map(|legacy| {
            let binding = ExecutionIdentity {
                host: legacy.key.host,
                socket: legacy.key.socket,
                session_id: legacy.key.session_id,
                window_id: legacy.key.window_id,
            };
            let identity = legacy_identity(&binding);
            WorkRecord {
                identity,
                created_at: legacy.metadata.updated_at,
                metadata: legacy.metadata,
                external_items: Vec::new(),
                legacy_binding: Some(binding),
            }
        })
        .collect()
}

fn legacy_identity(binding: &ExecutionIdentity) -> WorkIdentity {
    let suffix = format!(
        "{}-{}-{}-{}",
        binding.host, binding.socket, binding.session_id, binding.window_id
    );
    WorkIdentity::new("migrated", normalize_legacy_id(&suffix))
}

fn normalize_legacy_id(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

async fn save(path: &Path, works: Vec<WorkRecord>) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "work store path has no parent",
        )
    })?;
    tokio::fs::create_dir_all(parent).await?;
    let basename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("dashboard-work.json");
    let temporary = parent.join(format!(".{basename}.{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec_pretty(&WorkStoreFile {
        version: WORK_SCHEMA_VERSION,
        works,
    })
    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    {
        let mut options = OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        options.mode(WORK_STORE_FILE_MODE);
        let mut file = options.open(&temporary).await?;
        file.write_all(&bytes).await?;
        file.flush().await?;
        file.sync_all().await?;
    }
    tokio::fs::rename(&temporary, path).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::WorkStage;

    fn identity() -> WorkIdentity {
        WorkIdentity::new("muxa", "CAL-1")
    }

    #[tokio::test]
    async fn work_metadata_survives_a_store_reload() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("dashboard-work.json");
        let mut store = WorkStore::load(Some(path.clone()));
        store
            .upsert_metadata(
                identity(),
                WorkMetadataPatch {
                    title: Some("Authentication cleanup".into()),
                    goal: Some("Remove the legacy token flow".into()),
                    next_action: Some("Run integration tests".into()),
                    stage: WorkStage::Review,
                },
            )
            .await
            .unwrap();

        let loaded = WorkStore::load(Some(path));
        let records = loaded.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].identity, identity());
        assert_eq!(records[0].metadata.stage, WorkStage::Review);
    }

    #[tokio::test]
    async fn external_issue_does_not_overwrite_local_work_metadata() {
        let mut store = WorkStore::memory();
        let now = OffsetDateTime::now_utc();
        let item = ExternalItemRef {
            source: "linear".into(),
            scope: Some("CAL".into()),
            stable_id: Some("uuid-1".into()),
            display_key: "CAL-1".into(),
            title: Some("Provider title".into()),
            url: None,
            status: Some("started".into()),
            item_type: Some("issue".into()),
            synced_at: now,
        };
        let created = store
            .upsert_external_item(identity(), item.clone())
            .await
            .unwrap();
        assert_eq!(created.metadata.title, None);
        assert_eq!(created.external_items, vec![item]);

        store
            .upsert_metadata(
                identity(),
                WorkMetadataPatch {
                    title: Some("Local outcome".into()),
                    stage: WorkStage::InProgress,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let mut refreshed = created.external_items[0].clone();
        refreshed.title = Some("Provider title changed".into());
        refreshed.status = Some("done".into());
        let updated = store
            .upsert_external_item(identity(), refreshed)
            .await
            .unwrap();
        assert_eq!(updated.metadata.title.as_deref(), Some("Local outcome"));
        assert_eq!(updated.metadata.stage, WorkStage::InProgress);
        assert_eq!(updated.external_items[0].status.as_deref(), Some("done"));
    }

    #[test]
    fn v1_physical_key_migrates_to_a_logical_record_with_a_binding() {
        let raw = br#"{
          "version": 1,
          "works": [{
            "key": {"host":"tmux","socket":"default","session_id":"$1","window_id":"@2"},
            "metadata": {"title":"Legacy","stage":"in_progress","updated_at":"2026-01-01T00:00:00Z"}
          }]
        }"#;
        let records = load_records(raw).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].identity.workspace_id, "migrated");
        assert_eq!(records[0].legacy_binding.as_ref().unwrap().window_id, "@2");
    }

    #[test]
    fn metadata_validation_trims_blanks_and_rejects_oversized_values() {
        let clean = WorkMetadataPatch {
            title: Some("  ".into()),
            goal: Some(" ship it ".into()),
            ..Default::default()
        }
        .validate()
        .unwrap();
        assert_eq!(clean.title, None);
        assert_eq!(clean.goal.as_deref(), Some("ship it"));

        let error = WorkMetadataPatch {
            title: Some("x".repeat(161)),
            ..Default::default()
        }
        .validate()
        .unwrap_err();
        assert!(error.contains("title is too long"));
    }
}
