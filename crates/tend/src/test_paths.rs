//! Absolute paths for test fixtures on every platform.
//!
//! A fixture path like `/src/tend` is absolute on Unix but not on Windows,
//! where tend (correctly) refuses it. Tests that only need *an* absolute
//! path write it Unix-style and pass it through [`abs`].

/// `unix` as an absolute path on this platform: unchanged on Unix, and on
/// the `C:` drive with backslashes on Windows (`/src/tend` -> `C:\src\tend`).
#[must_use]
pub fn abs(unix: &str) -> String {
    debug_assert!(unix.starts_with('/'), "{unix} is not Unix-absolute");
    if cfg!(windows) {
        format!("C:{}", unix.replace('/', "\\"))
    } else {
        unix.to_owned()
    }
}

/// [`abs`] as a path.
#[must_use]
pub fn abs_path(unix: &str) -> std::path::PathBuf {
    abs(unix).into()
}
