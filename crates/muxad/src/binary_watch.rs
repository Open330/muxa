//! Notice when the binary this process is running from has been replaced.
//!
//! Installing a new muxa does not restart the daemon. The package manager
//! writes the new build and repoints whatever is on `PATH`; the running
//! process holds its open inode and keeps serving the old logic. Service
//! managers do not close this: launchd's `KeepAlive` and systemd's
//! `Restart=always` restart a process that *exits*, and nothing exited.
//!
//! Measured on a live host: a daemon started six days before a Homebrew
//! upgrade kept answering `protocol mismatch: server=4 client=6` to every CLI
//! call until it was killed by hand.
//!
//! So the daemon watches the path it would re-exec through, and re-execs when
//! that path starts resolving to a different file.

use std::path::{Path, PathBuf};

/// What makes one installed binary distinguishable from another.
///
/// Inode alone is not enough. A package manager that writes in place reuses
/// it, and on macOS an inode number is only unique within its device — a
/// Homebrew prefix on a separate volume can hand back the same number for an
/// unrelated file. Size and mtime then separate two builds that a
/// write-in-place install would otherwise make look identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BinaryIdentity {
    device: u64,
    inode: u64,
    size: u64,
    modified_nanos: u128,
}

impl BinaryIdentity {
    /// Read the identity of whatever `path` resolves to right now.
    ///
    /// Deliberately follows symlinks: on a Homebrew install the watched path
    /// is the `bin/` link and the upgrade happens at its target, so watching
    /// the link itself would see nothing change.
    pub(crate) fn read(path: &Path) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;

        let metadata = std::fs::metadata(path).ok()?;
        if !metadata.is_file() || metadata.len() == 0 {
            // A zero-length or non-regular file is an install caught
            // mid-write. Reporting no identity holds the watch at its current
            // baseline rather than treating the half-written file as a build
            // to re-exec onto.
            return None;
        }
        Some(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.len(),
            modified_nanos: metadata
                .modified()
                .ok()
                .and_then(|time| {
                    time.duration_since(std::time::UNIX_EPOCH)
                        .ok()
                        .map(|since| since.as_nanos())
                })
                .unwrap_or(0),
        })
    }
}

/// The path a re-exec would actually load.
///
/// `reexec_self` runs `argv[0]`, so that is what must be watched — not
/// `current_exe()`, which on macOS resolves to the *old* build's Cellar path.
/// That path either stops existing when the old keg is cleaned up or keeps
/// pointing at the binary we are already running, so watching it would report
/// a deletion or nothing at all, never the upgrade.
///
/// `argv[0]` without a separator was found on `PATH` by the caller's shell and
/// cannot be re-resolved reliably from here (this process's `PATH` and cwd may
/// both differ), so those fall back to `current_exe()` — still useful, since a
/// `cargo install` overwrite of that exact path does change it.
pub(crate) fn reexec_target() -> Option<PathBuf> {
    let argv0 = std::env::args_os().next()?;
    let argv0 = PathBuf::from(argv0);
    if argv0.components().count() > 1 {
        return Some(argv0);
    }
    std::env::current_exe().ok()
}

/// Tracks the installed binary across polls and reports when a *settled*
/// change has happened.
#[derive(Debug)]
pub(crate) struct BinaryWatch {
    baseline: BinaryIdentity,
    /// A different identity seen once and waiting to be seen again.
    pending: Option<BinaryIdentity>,
}

impl BinaryWatch {
    pub(crate) fn new(baseline: BinaryIdentity) -> Self {
        Self {
            baseline,
            pending: None,
        }
    }

    /// Feed one observation. Returns `true` exactly once per upgrade, on the
    /// second consecutive poll that agrees on the new identity.
    ///
    /// The confirmation tick is what keeps a multi-step install from being
    /// mistaken for a finished one. An upgrade is not atomic: a `.tmp` file
    /// appears, gets renamed, and a symlink is repointed, and a poll landing
    /// between those steps sees a file that is real, non-empty, and about to
    /// be replaced again. Re-execing onto that one wastes a restart at best
    /// and lands on a partial binary at worst. Two agreeing polls mean the
    /// installer has stopped moving.
    ///
    /// `None` — an unreadable or half-written path — clears the pending
    /// candidate rather than being treated as a change, so a file that
    /// vanishes mid-install cannot arm a restart onto something that no longer
    /// exists.
    pub(crate) fn observe(&mut self, identity: Option<BinaryIdentity>) -> bool {
        let Some(identity) = identity else {
            self.pending = None;
            return false;
        };
        if identity == self.baseline {
            // Covers the rollback case too: an install reverted between polls
            // leaves us running the build that is installed, which is the
            // whole point — no restart is owed.
            self.pending = None;
            return false;
        }
        if self.pending == Some(identity) {
            // Adopt it as the new baseline before reporting. A re-exec that
            // fails leaves this process running, and re-reporting the same
            // change every poll would spend the rest of the process's life
            // retrying an exec that already failed.
            self.baseline = identity;
            self.pending = None;
            return true;
        }
        self.pending = Some(identity);
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(inode: u64, size: u64) -> BinaryIdentity {
        BinaryIdentity {
            device: 1,
            inode,
            size,
            modified_nanos: 0,
        }
    }

    #[test]
    fn an_unchanged_binary_never_fires() {
        let mut watch = BinaryWatch::new(identity(1, 100));
        for _ in 0..5 {
            assert!(!watch.observe(Some(identity(1, 100))));
        }
    }

    #[test]
    fn a_replaced_binary_fires_on_the_confirming_poll() {
        let mut watch = BinaryWatch::new(identity(1, 100));
        assert!(
            !watch.observe(Some(identity(2, 200))),
            "the first sighting is a candidate, not a decision"
        );
        assert!(watch.observe(Some(identity(2, 200))));
    }

    #[test]
    fn an_install_still_moving_does_not_fire() {
        // The shape of a real upgrade: a temporary file, then the final one.
        // Firing on the first would re-exec onto a build that is about to be
        // replaced again.
        let mut watch = BinaryWatch::new(identity(1, 100));
        assert!(!watch.observe(Some(identity(2, 50))));
        assert!(!watch.observe(Some(identity(3, 200))));
        assert!(watch.observe(Some(identity(3, 200))));
    }

    #[test]
    fn a_change_is_reported_once() {
        // A failed exec leaves this process alive. Re-reporting would retry it
        // every poll forever.
        let mut watch = BinaryWatch::new(identity(1, 100));
        watch.observe(Some(identity(2, 200)));
        assert!(watch.observe(Some(identity(2, 200))));
        assert!(!watch.observe(Some(identity(2, 200))));
    }

    #[test]
    fn a_binary_that_vanishes_mid_install_arms_nothing() {
        let mut watch = BinaryWatch::new(identity(1, 100));
        assert!(!watch.observe(Some(identity(2, 200))));
        assert!(
            !watch.observe(None),
            "an unreadable path is not a new build"
        );
        assert!(
            !watch.observe(Some(identity(2, 200))),
            "the candidate was dropped, so this sighting starts over"
        );
        assert!(watch.observe(Some(identity(2, 200))));
    }

    #[test]
    fn an_install_reverted_between_polls_owes_no_restart() {
        let mut watch = BinaryWatch::new(identity(1, 100));
        assert!(!watch.observe(Some(identity(2, 200))));
        assert!(!watch.observe(Some(identity(1, 100))));
        assert!(!watch.observe(Some(identity(1, 100))));
    }

    #[test]
    fn a_write_in_place_upgrade_is_seen_even_when_the_inode_is_reused() {
        // Same inode, different content. Inode-only identity would call this
        // binary unchanged and serve the old logic indefinitely.
        let mut watch = BinaryWatch::new(identity(1, 100));
        assert!(!watch.observe(Some(identity(1, 180))));
        assert!(watch.observe(Some(identity(1, 180))));
    }

    #[test]
    fn a_real_binary_reads_an_identity() {
        let exe = std::env::current_exe().expect("the test binary exists");
        let identity = BinaryIdentity::read(&exe).expect("a real file has an identity");
        assert_eq!(BinaryIdentity::read(&exe), Some(identity));
    }

    #[test]
    fn a_directory_has_no_binary_identity() {
        // `is_file` guards the case where the watched path is replaced by
        // something that is not a binary at all.
        assert_eq!(BinaryIdentity::read(Path::new("/tmp")), None);
    }
}
