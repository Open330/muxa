//! Receipts for the git worktrees tend creates.
//!
//! A path that looks like one of tend's worktrees proves nothing: a person
//! can make or reuse a directory there. So tend writes a receipt when it is
//! about to run `git worktree add`, and marks it created only after git
//! succeeds and the receipt's id is written into the worktree's own git
//! administrative directory. A crash in between leaves a `creating`
//! receipt, which never counts as proof. Git deletes that directory with the
//! worktree, so a tree later made at the same path carries no mark and is
//! not taken for tend's: the path alone proves nothing.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptState {
    /// Written before `git worktree add`; not proof of anything.
    Creating,
    /// `git worktree add` succeeded for this path.
    Created,
    /// `git worktree add` failed; nothing was created.
    Failed,
}

/// What asked for a worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReceiptOrigin {
    /// A Work route's worktree, from `tend work up`.
    Work {
        workspace: Option<String>,
        work: String,
    },
    /// A dispatch's detached run checkout.
    Dispatch {
        dispatch_id: String,
        workspace: String,
        work: String,
    },
}

/// The file in a worktree's git administrative directory that holds the id
/// of the receipt that created it.
pub const MARK_FILE: &str = "tend-receipt";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeReceipt {
    pub id: String,
    /// The repository the worktree belongs to, canonical.
    pub repo: PathBuf,
    /// The worktree directory, canonical once it exists.
    pub path: PathBuf,
    /// The branch the worktree was made on; None for a detached checkout.
    pub branch: Option<String>,
    /// What it started from: a branch, ref or commit.
    pub base: Option<String>,
    /// The commit checked out when it was created.
    pub start_head: Option<String>,
    /// What asked for it.
    pub origin: ReceiptOrigin,
    /// The repository's common git directory, canonical, once created.
    #[serde(default)]
    pub common_dir: Option<PathBuf>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub state: ReceiptState,
}

/// Where receipts live: `<tend data>/worktrees/<id>.json`.
#[must_use]
pub fn dir() -> Option<PathBuf> {
    Some(
        crate::paths::default_node_id_file()?
            .parent()?
            .join("worktrees"),
    )
}

impl WorktreeReceipt {
    /// Records that tend is about to create a worktree at `path`.
    pub fn begin(
        repo: &Path,
        path: &Path,
        branch: Option<String>,
        base: Option<String>,
        origin: ReceiptOrigin,
    ) -> std::io::Result<Self> {
        let receipt = Self {
            id: uuid::Uuid::new_v4().to_string(),
            repo: crate::paths::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf()),
            path: path.to_path_buf(),
            branch,
            base,
            start_head: None,
            origin,
            common_dir: None,
            created_at: OffsetDateTime::now_utc(),
            state: ReceiptState::Creating,
        };
        receipt.save()?;
        Ok(receipt)
    }

    /// Marks the worktree created, with the commit it checked out, after
    /// writing the receipt's id into the worktree's git administrative
    /// directory. Without that mark the receipt stays `creating`.
    pub fn finish(mut self, start_head: Option<String>) -> std::io::Result<Self> {
        self.mark_created(start_head)?;
        self.save()?;
        Ok(self)
    }

    /// Writes the mark and records the created state, without saving.
    pub fn mark_created(&mut self, start_head: Option<String>) -> std::io::Result<()> {
        self.path = crate::paths::canonicalize(&self.path).unwrap_or_else(|_| self.path.clone());
        let admin = admin_dir(&self.path).ok_or_else(|| {
            std::io::Error::other("git did not name the worktree's administrative directory")
        })?;
        std::fs::write(admin.join(MARK_FILE), &self.id)?;
        self.common_dir = common_dir(&self.path);
        self.start_head = start_head;
        self.state = ReceiptState::Created;
        Ok(())
    }

    /// Whether the worktree now at `path` is the one this receipt created:
    /// its git administrative directory holds this receipt's id and it
    /// shares the recorded common git directory. None when git cannot say.
    #[must_use]
    pub fn still_owns(&self) -> Option<bool> {
        let admin = admin_dir(&self.path)?;
        let mark = std::fs::read_to_string(admin.join(MARK_FILE)).unwrap_or_default();
        let same_repo = match (&self.common_dir, common_dir(&self.path)) {
            (Some(recorded), Some(current)) => *recorded == current,
            _ => false,
        };
        Some(mark.trim() == self.id && same_repo)
    }

    /// Marks the attempt failed; nothing was created.
    pub fn fail(mut self) -> std::io::Result<()> {
        self.state = ReceiptState::Failed;
        self.save()
    }

    fn save(&self) -> std::io::Result<()> {
        let dir = dir().ok_or_else(|| std::io::Error::other("no tend data directory"))?;
        self.save_in(&dir)
    }

    fn save_in(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let body = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        // Write then rename, so a reader never sees half a receipt.
        let tmp = dir.join(format!(".{}.tmp", self.id));
        {
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(&body)?;
            file.sync_all()?;
        }
        std::fs::rename(tmp, dir.join(format!("{}.json", self.id)))
    }
}

/// The worktree's own administrative directory (`.git/worktrees/<name>`).
fn admin_dir(path: &Path) -> Option<PathBuf> {
    git_path(path, "--absolute-git-dir")
}

fn common_dir(path: &Path) -> Option<PathBuf> {
    git_path(path, "--git-common-dir")
}

fn git_path(path: &Path, flag: &str) -> Option<PathBuf> {
    let args: [&std::ffi::OsStr; 5] = [
        "-C".as_ref(),
        path.as_os_str(),
        "rev-parse".as_ref(),
        "--path-format=absolute".as_ref(),
        flag.as_ref(),
    ];
    let (ok, stdout) = crate::worktree_inventory::bounded("git", &args)?;
    if !ok {
        return None;
    }
    let text = stdout.trim().to_string();
    if text.is_empty() {
        return None;
    }
    Some(crate::paths::canonicalize(&text).unwrap_or_else(|_| PathBuf::from(text)))
}

/// Every readable receipt, oldest first. Unreadable files are skipped.
#[must_use]
pub fn list() -> Vec<WorktreeReceipt> {
    dir().map(|dir| list_in(&dir)).unwrap_or_default()
}

fn list_in(dir: &Path) -> Vec<WorktreeReceipt> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut receipts: Vec<WorktreeReceipt> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|path| std::fs::read(path).ok())
        .filter_map(|body| serde_json::from_slice(&body).ok())
        .collect();
    receipts.sort_by_key(|receipt| receipt.created_at);
    receipts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipts_round_trip_and_only_created_ones_are_created() {
        let dir = tempfile::tempdir().unwrap();
        let mut receipt = WorktreeReceipt {
            id: "r1".into(),
            repo: "/repo".into(),
            path: "/repo-wt".into(),
            branch: Some("feat".into()),
            base: Some("origin/main".into()),
            start_head: None,
            origin: ReceiptOrigin::Work {
                workspace: Some("ws".into()),
                work: "feat".into(),
            },
            common_dir: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            state: ReceiptState::Creating,
        };
        receipt.save_in(dir.path()).unwrap();
        assert_eq!(list_in(dir.path())[0].state, ReceiptState::Creating);
        receipt.state = ReceiptState::Created;
        receipt.start_head = Some("abc".into());
        receipt.save_in(dir.path()).unwrap();
        let listed = list_in(dir.path());
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0], receipt);
        std::fs::write(dir.path().join("junk.json"), b"{").unwrap();
        assert_eq!(list_in(dir.path()).len(), 1);
    }
}
