//! Two agents must not be told to edit the same files at once.
//!
//! An execute request's `paths` used to be advisory: nothing stopped a
//! second agent from being told to edit the same files while the first was
//! still at it. The store now reserves the files an execute request claims
//! and refuses a new one, or a move of one to another agent, that overlaps
//! a reservation held by another agent, unless the sender says the overlap
//! is intended (`allow_path_overlap`). Requests queued for the same agent
//! are not compared: it takes them in turn.
//!
//! Deterministic and local, and no git is run:
//! - each path resolves against the recipient's working directory the way
//!   the filesystem resolves it (each existing component's symlink followed
//!   before a later `..`; the part that does not exist yet as written), so
//!   `../shared` from two sibling directories, or a symlink and its target,
//!   are one path;
//! - a glob counts from its literal prefix (`src/**/*.rs` holds `src`);
//! - no paths means the whole checkout: the nearest directory holding a
//!   `.git` entry, else the working directory itself.
//!
//! The resolved paths are stored on the request (`execute_scope`) when it is
//! accepted, so the reservation does not drift when its recipient session is
//! later pinned or rebound. A reservation lasts while the request is open,
//! which is why an execute request must expect a reply: a no-reply request
//! completes when it is claimed, before any edit is made. An execute request
//! whose recipient has no known working directory cannot be checked and is
//! refused the same way.

use super::{CollaborationError, CollaborationRequest, NewRequest, Participant, WorkMode};
use std::path::{Component, Path, PathBuf};

/// The reservation `input` would take for `to`, or why it is refused.
/// `None` when no reservation applies (read-only, console, opted out).
pub(super) fn reserve<'a>(
    open: impl IntoIterator<Item = &'a CollaborationRequest>,
    to: &Participant,
    input: &NewRequest,
) -> Result<Option<Vec<String>>, CollaborationError> {
    if input.work_mode != WorkMode::Execute || input.allow_path_overlap || to.console {
        return Ok(None);
    }
    if !input.expects_reply {
        return Err(refused(
            "an execute request needs a reply: its reservation lasts until the reply, and a no-reply request ends when it is claimed",
        ));
    }
    let scope = scope_for(to, &input.paths)?;
    check(open, &to.agent_session_id, &scope, None)?;
    Ok(Some(scope))
}

/// Re-check a held request about to move to `to`, excluding itself, and
/// return its new reservation.
pub(super) fn rebind<'a>(
    open: impl IntoIterator<Item = &'a CollaborationRequest>,
    request: &CollaborationRequest,
    to: &Participant,
) -> Result<Option<Vec<String>>, CollaborationError> {
    if request.work_mode != WorkMode::Execute || request.allow_path_overlap || to.console {
        return Ok(None);
    }
    let scope = scope_for(to, &request.paths)?;
    check(open, &to.agent_session_id, &scope, Some(&request.id))?;
    Ok(Some(scope))
}

/// A concrete session about to adopt a request addressed to its pane's
/// pending placeholder: the reservation must hold in the directory that
/// session really works in. `Ok(Some(scope))` is the reservation to store,
/// `Ok(None)` leaves it unchanged, and an error means the adopting session
/// may not take it (it stays queued).
pub(super) fn adopt<'a>(
    open: impl IntoIterator<Item = &'a CollaborationRequest>,
    request: &CollaborationRequest,
    caller: &Participant,
) -> Result<Option<Vec<String>>, CollaborationError> {
    if request.work_mode != WorkMode::Execute || request.allow_path_overlap || caller.console {
        return Ok(None);
    }
    let scope = scope_for(caller, &request.paths)?;
    if request.execute_scope.as_ref() == Some(&scope) {
        return Ok(None);
    }
    check(open, &caller.agent_session_id, &scope, Some(&request.id))?;
    Ok(Some(scope))
}

fn refused(reason: &str) -> CollaborationError {
    CollaborationError::ExecuteUnreserved(reason.to_owned())
}

fn scope_for(to: &Participant, paths: &[String]) -> Result<Vec<String>, CollaborationError> {
    let Some(cwd) = to.cwd.as_deref().filter(|cwd| Path::new(cwd).is_absolute()) else {
        return Err(refused(
            "the recipient has no known working directory, so its paths cannot be reserved; retry once it has registered, or set allow_path_overlap",
        ));
    };
    Ok(scope(Path::new(cwd), paths)
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect())
}

fn check<'a>(
    open: impl IntoIterator<Item = &'a CollaborationRequest>,
    recipient: &str,
    wanted: &[String],
    except: Option<&str>,
) -> Result<(), CollaborationError> {
    for request in open {
        if request.work_mode != WorkMode::Execute
            || request.status.is_terminal()
            || request.allow_path_overlap
            || request.to.agent_session_id == recipient
            || Some(request.id.as_str()) == except
        {
            continue;
        }
        let held = match &request.execute_scope {
            Some(scope) => scope.clone(),
            // Accepted before reservations existed: derive it the same way
            // when its recipient's directory is known.
            None => match scope_for(&request.to, &request.paths) {
                Ok(scope) => scope,
                Err(_) => continue,
            },
        };
        for mine in wanted {
            if let Some(other) = held.iter().find(|other| overlaps(mine, other)) {
                return Err(CollaborationError::PathOverlap {
                    request_id: request.id.clone(),
                    path: if mine.len() <= other.len() {
                        mine
                    } else {
                        other
                    }
                    .clone(),
                });
            }
        }
    }
    Ok(())
}

/// Absolute paths a request claims. Empty means the whole checkout.
fn scope(cwd: &Path, paths: &[String]) -> Vec<PathBuf> {
    let base = resolve(cwd);
    let claimed: Vec<PathBuf> = paths
        .iter()
        .map(|path| literal_prefix(path.trim()))
        .filter(|path| !path.is_empty())
        .map(|path| resolve(&base.join(path)))
        .collect();
    if claimed.is_empty() {
        vec![checkout_root(&base).unwrap_or(base)]
    } else {
        claimed
    }
}

/// Resolve `path` the way the filesystem would: each existing component is
/// resolved (symlinks followed) before the next one, so `link/..` is the
/// parent of the link's target, not of the link. From the first component
/// that does not exist, the rest is applied as written.
fn resolve(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    let mut exists = true;
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
                // `missing/..` can lead back into a real directory.
                if !exists {
                    if let Ok(real) = std::fs::canonicalize(&out) {
                        out = real;
                        exists = true;
                    }
                }
            }
            other => {
                out.push(other.as_os_str());
                if exists {
                    match std::fs::canonicalize(&out) {
                        Ok(real) => out = real,
                        Err(_) => exists = false,
                    }
                }
            }
        }
    }
    out
}

/// The nearest directory at or above `dir` with a `.git` entry (a
/// directory, or the file a linked worktree has).
fn checkout_root(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|ancestor| std::fs::symlink_metadata(ancestor.join(".git")).is_ok())
        .map(Path::to_path_buf)
}

/// The part of a path before its first glob character, cut back to a
/// whole component (`src/**/x.rs` → `src`, `src/a*.rs` → `src`).
fn literal_prefix(path: &str) -> &str {
    let Some(glob) = path.find(['*', '?', '[', '{']) else {
        return path;
    };
    match path[..glob].rfind('/') {
        Some(slash) => &path[..slash],
        // A glob in the first component: the whole directory it sits in.
        None => ".",
    }
}

/// One path is the other or inside it, by whole components.
fn nested(a: &str, b: &str) -> bool {
    let (a, b) = (Path::new(a), Path::new(b));
    a.starts_with(b) || b.starts_with(a)
}

/// Two claims touch the same files: one is the other or inside it, unless
/// the inner one sits in a separate checkout below the outer one (a linked
/// worktree kept inside the repository, such as `.claude/worktrees/…`).
fn overlaps(a: &str, b: &str) -> bool {
    if !nested(a, b) {
        return false;
    }
    let (outer, inner) = if Path::new(b).starts_with(a) {
        (a, b)
    } else {
        (b, a)
    };
    let (outer, inner) = (Path::new(outer), Path::new(inner));
    let existing = inner
        .ancestors()
        .find(|ancestor| ancestor.exists())
        .unwrap_or(inner);
    // Only positive evidence of a distinct checkout (its own `.git`
    // strictly inside the outer claim) separates them.
    !checkout_root(existing).is_some_and(|root| root != outer && root.starts_with(outer))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkout_scope(cwd: &Path, paths: &[&str]) -> Vec<PathBuf> {
        scope(
            cwd,
            &paths.iter().map(|p| (*p).to_owned()).collect::<Vec<_>>(),
        )
    }

    #[test]
    fn paths_resolve_against_the_directory_and_globs_count_from_their_prefix() {
        let cwd = Path::new("/nonexistent-tend-test/tend");
        assert_eq!(
            checkout_scope(
                cwd,
                &["./crates/tend/../tend-cli/src/", "docs/**/*.md", "*.toml"]
            ),
            [
                PathBuf::from("/nonexistent-tend-test/tend/crates/tend-cli/src"),
                PathBuf::from("/nonexistent-tend-test/tend/docs"),
                PathBuf::from("/nonexistent-tend-test/tend"),
            ]
        );
        assert!(overlaps("/a/b", "/a/b/c"));
        assert!(!overlaps("/a/b", "/a/bc"));
    }

    #[test]
    fn real_directories_without_git_still_overlap() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        // No `.git` anywhere: nesting alone decides.
        std::fs::create_dir_all(root.join("nongit/src/a")).unwrap();
        let outer = root.join("nongit/src").to_string_lossy().into_owned();
        assert!(overlaps(
            &outer,
            &root.join("nongit/src/a").to_string_lossy()
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_names_resolve_to_the_files_they_reach() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        std::fs::create_dir_all(root.join("repo/real/a")).unwrap();
        std::fs::create_dir_all(root.join("other/sub")).unwrap();
        std::os::unix::fs::symlink(root.join("repo/real"), root.join("repo/alias")).unwrap();
        std::os::unix::fs::symlink(root.join("other/sub"), root.join("repo/link")).unwrap();
        let repo = root.join("repo");
        assert_eq!(
            checkout_scope(&repo, &["alias/a/new.rs"]),
            [root.join("repo/real/a/new.rs")]
        );
        assert_eq!(
            checkout_scope(&repo, &["real/a"]),
            [root.join("repo/real/a")]
        );
        // `link/..` is the parent of the link's target, as the filesystem
        // resolves it, not `repo` itself.
        assert_eq!(
            checkout_scope(&repo, &["link/../shared"]),
            [root.join("other/shared")]
        );
        assert_eq!(
            checkout_scope(&repo, &["missing/../real/a"]),
            [root.join("repo/real/a")]
        );
    }

    #[test]
    fn no_paths_claims_the_checkout_root_found_by_its_git_entry() {
        let dir = tempfile::tempdir().unwrap();
        let repo = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("crates/a")).unwrap();
        assert_eq!(
            checkout_scope(&repo.join("crates/a"), &[]),
            std::slice::from_ref(&repo)
        );
        // A linked worktree marks itself with a `.git` file.
        let linked = repo.join("wt");
        std::fs::create_dir_all(&linked).unwrap();
        std::fs::write(linked.join(".git"), "gitdir: elsewhere").unwrap();
        assert_eq!(checkout_scope(&linked, &[]), std::slice::from_ref(&linked));
        // The repository's whole-checkout claim does not reach into it.
        let outer = repo.to_string_lossy().into_owned();
        assert!(!overlaps(&outer, &linked.join("src").to_string_lossy()));
        assert!(overlaps(&outer, &repo.join("crates/a").to_string_lossy()));
    }
}
