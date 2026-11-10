//! Owner-only creation modes that read the same on every platform.
//!
//! On Unix `owner_only(0o600)` is `mode(0o600)`. Windows has no mode bits: a
//! file or directory created under the per-user profile inherits that
//! profile's ACL, which already excludes other non-administrator users, so
//! the call is a no-op there rather than a cfg at every call site.

pub(crate) trait OwnerOnly {
    fn owner_only(&mut self, mode: u32) -> &mut Self;
}

impl OwnerOnly for std::fs::OpenOptions {
    fn owner_only(&mut self, mode: u32) -> &mut Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            self.mode(mode)
        }
        #[cfg(not(unix))]
        {
            let _ = mode;
            self
        }
    }
}

impl OwnerOnly for std::fs::DirBuilder {
    fn owner_only(&mut self, mode: u32) -> &mut Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            self.mode(mode)
        }
        #[cfg(not(unix))]
        {
            let _ = mode;
            self
        }
    }
}
