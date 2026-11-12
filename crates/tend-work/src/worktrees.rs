//! Worktree receipts for the worktrees `work up` creates.

use std::path::Path;
use tend::worktree_receipt::WorktreeReceipt;

/// Writes the receipt for a worktree tend is about to create. A receipt
/// that cannot be written only costs cleanup its proof, so failure is
/// logged, not raised. Unit tests make worktrees in temporary directories;
/// their receipts would land in the user's tend data, so tests write none.
pub fn begin_receipt(
    repo: &Path,
    path: &Path,
    branch: Option<String>,
    base: Option<String>,
    origin: tend::worktree_receipt::ReceiptOrigin,
) -> Option<WorktreeReceipt> {
    if cfg!(test) {
        return None;
    }
    WorktreeReceipt::begin(repo, path, branch, base, origin)
        .map_err(|error| tracing::warn!(%error, "worktree receipt not written"))
        .ok()
}
