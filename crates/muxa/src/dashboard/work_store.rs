//! Durable operator annotations for dashboard work items.
//!
//! Session/window/pane identity remains authoritative in the active backend.
//! This store deliberately persists only information tmux cannot express well:
//! a human title, goal, next action, and explicit workflow stage.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

use crate::backend::HostKind;

const WORK_STORE_SCHEMA_VERSION: u8 = 1;
#[cfg(unix)]
const WORK_STORE_FILE_MODE: u32 = 0o600;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkKey {
    pub host: HostKind,
    pub socket: String,
    pub session_id: String,
    pub window_id: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkStage {
    #[default]
    Auto,
    Queued,
    InProgress,
    Review,
    Blocked,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_action: Option<String>,
    #[serde(default)]
    pub stage: WorkStage,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WorkMetadataPatch {
    pub title: Option<String>,
    pub goal: Option<String>,
    pub next_action: Option<String>,
    #[serde(default)]
    pub stage: WorkStage,
}

impl WorkMetadataPatch {
    pub fn validate(self) -> Result<Self, String> {
        Ok(Self {
            title: clean(self.title, 160, "title")?,
            goal: clean(self.goal, 4_000, "goal")?,
            next_action: clean(self.next_action, 1_000, "next action")?,
            stage: self.stage,
        })
    }

    fn into_metadata(self, updated_at: OffsetDateTime) -> WorkMetadata {
        WorkMetadata {
            title: self.title,
            goal: self.goal,
            next_action: self.next_action,
            stage: self.stage,
            updated_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkRecord {
    pub key: WorkKey,
    pub metadata: WorkMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkStoreFile {
    version: u8,
    works: Vec<WorkRecord>,
}

#[derive(Debug)]
pub struct WorkStore {
    path: Option<PathBuf>,
    works: HashMap<WorkKey, WorkMetadata>,
}

impl WorkStore {
    pub fn memory() -> Self {
        Self {
            path: None,
            works: HashMap::new(),
        }
    }

    pub fn load(path: Option<PathBuf>) -> Self {
        let Some(path) = path else {
            return Self::memory();
        };
        let works = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<WorkStoreFile>(&bytes).ok())
            .filter(|file| file.version == WORK_STORE_SCHEMA_VERSION)
            .map(|file| {
                file.works
                    .into_iter()
                    .map(|record| (record.key, record.metadata))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            path: Some(path),
            works,
        }
    }

    pub fn records(&self) -> Vec<WorkRecord> {
        let mut records = self
            .works
            .iter()
            .map(|(key, metadata)| WorkRecord {
                key: key.clone(),
                metadata: metadata.clone(),
            })
            .collect::<Vec<_>>();
        records.sort_by(|left, right| {
            left.key
                .socket
                .cmp(&right.key.socket)
                .then_with(|| left.key.session_id.cmp(&right.key.session_id))
                .then_with(|| left.key.window_id.cmp(&right.key.window_id))
        });
        records
    }

    pub async fn upsert(
        &mut self,
        key: WorkKey,
        patch: WorkMetadataPatch,
    ) -> std::io::Result<WorkRecord> {
        let metadata = patch.into_metadata(OffsetDateTime::now_utc());
        let previous = self.works.insert(key.clone(), metadata.clone());
        if let Some(path) = &self.path {
            if let Err(error) = save(path, self.records()).await {
                if let Some(previous) = previous {
                    self.works.insert(key, previous);
                } else {
                    self.works.remove(&key);
                }
                return Err(error);
            }
        }
        Ok(WorkRecord { key, metadata })
    }
}

fn clean(value: Option<String>, max: usize, label: &str) -> Result<Option<String>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > max {
        return Err(format!("{label} is too long (max {max} bytes)"));
    }
    if value.chars().any(|ch| ch == '\0') {
        return Err(format!("{label} cannot contain NUL"));
    }
    Ok(Some(value.to_string()))
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
        version: WORK_STORE_SCHEMA_VERSION,
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

    fn key() -> WorkKey {
        WorkKey {
            host: HostKind::Tmux,
            socket: "default".into(),
            session_id: "$1".into(),
            window_id: "@2".into(),
        }
    }

    #[tokio::test]
    async fn work_metadata_survives_a_store_reload() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("dashboard-work.json");
        let mut store = WorkStore::load(Some(path.clone()));
        store
            .upsert(
                key(),
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
        assert_eq!(records[0].metadata.stage, WorkStage::Review);
        assert_eq!(
            records[0].metadata.title.as_deref(),
            Some("Authentication cleanup")
        );
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
