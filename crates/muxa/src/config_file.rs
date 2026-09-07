//! Reading and replacing `config.toml` as a whole document.
//!
//! Muxa.app's Advanced settings edits the daemon's configuration file
//! directly, so the daemon has to hand out its current text and take a
//! replacement back. Every write is checked before it lands: the document
//! must parse as a [`Config`] and pass [`Config::validate`], and the caller
//! may pin the text it edited so two editors cannot silently overwrite each
//! other. The file is replaced through a temporary file in the same
//! directory, keeping the mode of the file it replaces.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::Config;

/// The configuration file as a client sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigDocument {
    pub path: PathBuf,
    /// The file's contents; empty when it does not exist yet.
    pub text: String,
    pub exists: bool,
}

/// What went wrong, in the words a client should show verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigFileError {
    /// The file changed since the caller read it.
    Conflict {
        /// The text on disk now, so the caller can merge instead of asking
        /// for it again.
        current: String,
    },
    /// The replacement does not parse, or fails a semantic check.
    Invalid(String),
    /// The file could not be read or written.
    Io(String),
}

impl std::fmt::Display for ConfigFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict { .. } => write!(
                f,
                "config.toml changed on disk since it was read; reload it and apply the edit again"
            ),
            Self::Invalid(detail) | Self::Io(detail) => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for ConfigFileError {}

/// Reads the configuration file. A missing file is not an error: the daemon
/// runs on defaults, and the editor should offer to create one.
pub fn read(path: &Path) -> Result<ConfigDocument, ConfigFileError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(ConfigDocument {
            path: path.to_path_buf(),
            text,
            exists: true,
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(ConfigDocument {
            path: path.to_path_buf(),
            text: String::new(),
            exists: false,
        }),
        Err(error) => Err(ConfigFileError::Io(format!(
            "reading {}: {error}",
            path.display()
        ))),
    }
}

/// Replaces the configuration file with `text`.
///
/// `expected` is the text the caller started from; when it is given and no
/// longer matches what is on disk, the write is refused and the current text
/// comes back with the error. Nothing is written unless the replacement
/// parses as a [`Config`] and passes [`Config::validate`], so a daemon
/// restart can never find a file it cannot load.
pub fn write(
    path: &Path,
    text: &str,
    expected: Option<&str>,
) -> Result<ConfigDocument, ConfigFileError> {
    let current = read(path)?;
    if let Some(expected) = expected {
        if expected != current.text {
            return Err(ConfigFileError::Conflict {
                current: current.text,
            });
        }
    }

    let parsed: Config = toml::from_str(text).map_err(|error| {
        ConfigFileError::Invalid(format!(
            "the config would not parse, so it was not written: {error}"
        ))
    })?;
    parsed.validate().map_err(|error| {
        ConfigFileError::Invalid(format!(
            "the config is invalid, so it was not written: {error}"
        ))
    })?;

    atomic_write(path, text).map_err(ConfigFileError::Io)?;
    Ok(ConfigDocument {
        path: path.to_path_buf(),
        text: text.to_string(),
        exists: true,
    })
}

/// Write-then-rename in the target's directory, keeping the mode of the file
/// being replaced. A fresh file is owner-only, like the one `muxa init`
/// writes.
fn atomic_write(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write as _;

    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("creating {}: {error}", parent.display()))?;
    }
    let permissions = match std::fs::metadata(path) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(format!("reading mode of {}: {error}", path.display()));
        }
    };
    let tmp = path.with_extension(format!("toml.{}.config.tmp", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        match permissions {
            Some(permissions) => file.set_permissions(permissions)?,
            #[cfg(unix)]
            None => {
                use std::os::unix::fs::PermissionsExt as _;
                file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
            #[cfg(not(unix))]
            None => {}
        }
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("writing {}: {error}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of this test's own. Tests run on threads of one process,
    /// so a clock-based name can collide and one test's cleanup then deletes
    /// another's parent mid-write; the counter is what keeps them apart.
    fn temp_dir(label: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "muxa-config-file-{}-{label}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reading_a_missing_file_reports_it_without_failing() {
        let dir = temp_dir("missing");
        let path = dir.join("config.toml");

        let document = read(&path).expect("read");

        assert!(!document.exists);
        assert!(document.text.is_empty());
        assert_eq!(document.path, path);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_valid_document_replaces_the_file() {
        let dir = temp_dir("valid");
        let path = dir.join("config.toml");
        std::fs::write(&path, "[ui]\n").unwrap();

        let written = write(&path, "[ask]\nenabled = true\n", Some("[ui]\n")).expect("write");

        assert!(written.exists);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[ask]\nenabled = true\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_document_that_does_not_parse_leaves_the_file_alone() {
        let dir = temp_dir("unparsed");
        let path = dir.join("config.toml");
        std::fs::write(&path, "[ask]\nenabled = true\n").unwrap();

        let error = write(&path, "[ask\nenabled = true\n", None).expect_err("refused");

        assert!(matches!(error, ConfigFileError::Invalid(_)), "{error:?}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[ask]\nenabled = true\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unknown_key_is_refused_before_the_write() {
        let dir = temp_dir("unknown-key");
        let path = dir.join("config.toml");
        std::fs::write(&path, "[ask]\nenabled = true\n").unwrap();

        let error = write(&path, "[ask]\nnot_a_key = 1\n", None).expect_err("refused");

        assert!(matches!(error, ConfigFileError::Invalid(_)), "{error:?}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[ask]\nenabled = true\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_stale_editor_is_refused_and_handed_the_current_text() {
        let dir = temp_dir("stale");
        let path = dir.join("config.toml");
        std::fs::write(&path, "[ask]\nenabled = true\n").unwrap();

        let error = write(&path, "[ui]\n", Some("[something else]\n")).expect_err("conflict");

        match error {
            ConfigFileError::Conflict { current } => {
                assert_eq!(current, "[ask]\nenabled = true\n");
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[ask]\nenabled = true\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_file_can_be_created_by_pinning_the_empty_text() {
        let dir = temp_dir("create");
        let path = dir.join("nested").join("config.toml");

        let written = write(&path, "[ask]\nenabled = true\n", Some("")).expect("write");

        assert!(written.exists);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[ask]\nenabled = true\n"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn the_mode_of_the_replaced_file_survives() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = temp_dir("mode");
        let path = dir.join("config.toml");
        std::fs::write(&path, "[ui]\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();

        write(&path, "[ask]\nenabled = true\n", None).expect("write");

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
        std::fs::remove_dir_all(&dir).ok();
    }
}
