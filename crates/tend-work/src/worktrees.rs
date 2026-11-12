//! Worktree receipts for the worktrees `work up` creates.

use std::path::Path;
use tend::worktree_receipt::WorktreeReceipt;

/// Writes the receipt for a worktree tend is about to create. A receipt
/// that cannot be written only costs cleanup its proof, so failure is
/// logged, not raised. Unit tests make worktrees in temporary directories;
/// their receipts would land in the user's tend data, so tests write none:
/// this crate's own (`cfg(test)`) and its dependents', which turn on the
/// `test-no-receipts` feature from their dev-dependencies because a
/// library cannot see its caller's `cfg(test)`.
pub fn begin_receipt(
    repo: &Path,
    path: &Path,
    branch: Option<String>,
    base: Option<String>,
    origin: tend::worktree_receipt::ReceiptOrigin,
) -> Option<WorktreeReceipt> {
    if cfg!(any(test, feature = "test-no-receipts")) {
        return None;
    }
    WorktreeReceipt::begin(repo, path, branch, base, origin)
        .map_err(|error| tracing::warn!(%error, "worktree receipt not written"))
        .ok()
}
