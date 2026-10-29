//! Validate a replacement with its own parser before giving up a live process.
use sha2::{Digest, Sha256};
use std::ffi::OsString;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchStamp {
    target: PathBuf,
    size: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    inode: (u64, u64),
    config: [u8; 32],
}

#[derive(Debug, Clone)]
pub struct UpgradeCheck {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    config: Option<PathBuf>,
}
impl UpgradeCheck {
    pub fn new(program: PathBuf, args: Vec<OsString>) -> Self {
        let mut config = None;
        for (i, arg) in args.iter().enumerate() {
            if arg == "--config" {
                config = args.get(i + 1).map(PathBuf::from);
            } else if let Some(path) = arg.to_str().and_then(|a| a.strip_prefix("--config=")) {
                config = Some(path.into());
            }
        }
        let config = config
            .or_else(|| std::env::var_os("MUXA_CONFIG").map(PathBuf::from))
            .or_else(crate::paths::default_config_file);
        Self {
            program,
            args,
            config,
        }
    }
    pub fn current() -> Option<Self> {
        let mut args = std::env::args_os();
        let program = PathBuf::from(args.next()?);
        let program = if program.components().count() > 1 {
            program
        } else {
            std::env::current_exe().ok()?
        };
        Some(Self::new(program, args.collect()))
    }
    pub fn stamp(&self) -> Result<LaunchStamp, String> {
        let target =
            std::fs::canonicalize(&self.program).map_err(|_| "upgrade target is unavailable")?;
        let metadata = std::fs::metadata(&target).map_err(|_| "upgrade target cannot be read")?;
        if !metadata.is_file() || metadata.len() == 0 {
            return Err("upgrade target is not a complete file".into());
        }
        let bytes = match &self.config {
            Some(path) => match std::fs::read(path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(_) => return Err("upgrade configuration cannot be read".into()),
            },
            None => Vec::new(),
        };
        Ok(LaunchStamp {
            target,
            size: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            inode: (metadata.dev(), metadata.ino()),
            config: Sha256::digest(&bytes).into(),
        })
    }
    /// No stdout/stderr or input inheritance: config diagnostics can contain secrets,
    /// and a probe must never consume an MCP frame or start an agent/daemon.
    pub async fn validate(&self) -> Result<ValidatedLaunch, String> {
        let before = self.stamp()?;
        #[cfg(target_os = "linux")]
        let file = std::sync::Arc::new(
            std::fs::File::open(&before.target).map_err(|_| "cannot pin replacement binary")?,
        );
        #[cfg(target_os = "linux")]
        let probe_path = {
            use std::os::fd::AsRawFd;
            PathBuf::from(format!(
                "/proc/{}/fd/{}",
                std::process::id(),
                file.as_raw_fd()
            ))
        };
        #[cfg(not(target_os = "linux"))]
        let probe_path = before.target.clone();
        let status = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            tokio::process::Command::new(&probe_path)
                .args(&self.args)
                .arg("--check-config")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .status(),
        )
        .await;
        match status {
            Ok(Ok(status)) if status.success()=>{},
            Ok(Ok(_))=>return Err("replacement rejected the effective configuration or lacks --check-config; keeping the live process".into()),
            Ok(Err(_))=>return Err("replacement configuration probe could not start; keeping the live process".into()),
            Err(_)=>return Err("replacement configuration probe timed out; keeping the live process".into()),
        }
        if self.stamp()? != before {
            return Err("replacement binary or configuration changed during validation".into());
        }
        let checked = ValidatedLaunch {
            target: before.target.clone(),
            #[cfg(target_os = "linux")]
            file,
            check: self.clone(),
            stamp: before,
        };
        checked
            .execution_path()
            .map_err(|_| "pinned binary changed during validation")?;
        Ok(checked)
    }
}
#[derive(Debug, Clone)]
pub struct ValidatedLaunch {
    pub target: PathBuf,
    #[cfg(target_os = "linux")]
    file: std::sync::Arc<std::fs::File>,
    check: UpgradeCheck,
    stamp: LaunchStamp,
}
impl ValidatedLaunch {
    pub fn unchanged(&self) -> bool {
        self.check.stamp().is_ok_and(|now| now == self.stamp)
    }
    /// Linux executes the held inode across an atomic path replacement.
    /// CLOEXEC closes it after the kernel has opened the ELF image.
    /// A destructive in-place write is refused rather than executed.
    pub fn execution_path(&self) -> std::io::Result<PathBuf> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            use std::os::unix::fs::FileExt;
            let meta = self.file.metadata()?;
            if (meta.dev(), meta.ino()) != self.stamp.inode
                || meta.len() != self.stamp.size
                || meta.modified().ok() != self.stamp.modified
            {
                return Err(std::io::Error::other(
                    "validated binary was overwritten in place; refusing unvalidated execution",
                ));
            }
            let mut magic = [0; 4];
            self.file.read_exact_at(&mut magic, 0)?;
            if &magic != b"\x7fELF" {
                return if self.unchanged() {
                    Ok(self.target.clone())
                } else {
                    Err(std::io::Error::other(
                        "validated script changed; refusing execution",
                    ))
                };
            }
            Ok(PathBuf::from(format!(
                "/proc/self/fd/{}",
                self.file.as_raw_fd()
            )))
        }
        #[cfg(not(target_os = "linux"))]
        {
            if self.unchanged() {
                Ok(self.target.clone())
            } else {
                Err(std::io::Error::other(
                    "validated replacement changed; refusing execution",
                ))
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::path::Path;
    fn script(path: &Path, body: &str) {
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    #[tokio::test]
    async fn validation_pins_binary_and_rejects_failure_config_changes_and_timeouts() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("candidate");
        let link = dir.path().join("muxa");
        let config = dir.path().join("config.toml");
        std::fs::write(&config, "[collaboration]\nwake_payload='adaptive'\n").unwrap();
        script(&binary, "test \"$3\" = --check-config");
        symlink(&binary, &link).unwrap();
        let check = UpgradeCheck::new(
            link.clone(),
            vec!["--config".into(), config.clone().into_os_string()],
        );
        let valid = check.validate().await.unwrap();
        assert_eq!(valid.target, binary);
        assert!(valid.unchanged());
        std::fs::write(&config, "changed").unwrap();
        assert!(!valid.unchanged());
        script(&binary, "exit 1");
        assert!(check.validate().await.is_err());
        script(&binary, "printf modified > \"$2\"");
        assert!(check.validate().await.unwrap_err().contains("changed"));
        script(&binary, "exec sleep 10");
        assert!(check.validate().await.unwrap_err().contains("timed out"));
        script(&binary, "exit 0");
        let valid = check.validate().await.unwrap();
        let other = dir.path().join("other");
        script(&other, "exit 1");
        std::fs::remove_file(&link).unwrap();
        symlink(other, &link).unwrap();
        assert!(!valid.unchanged());
    }
}

#[cfg(all(test, target_os = "linux"))]
mod pinned_tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    #[tokio::test]
    async fn atomic_replacement_executes_validated_inode_and_in_place_writes_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("binary");
        std::fs::copy("/bin/true", &binary).unwrap();
        let config = dir.path().join("config");
        std::fs::write(&config, "").unwrap();
        let check = UpgradeCheck::new(
            binary.clone(),
            vec!["--config".into(), config.into_os_string()],
        );
        let checked = check.validate().await.unwrap();
        let replacement = dir.path().join("next");
        std::fs::copy("/bin/false", &replacement).unwrap();
        std::fs::rename(&replacement, &binary).unwrap();
        assert!(!checked.unchanged());
        assert!(
            std::process::Command::new(checked.execution_path().unwrap())
                .status()
                .unwrap()
                .success()
        );
        assert!(!std::process::Command::new(&binary)
            .arg0("false")
            .status()
            .unwrap()
            .success());
        std::fs::copy("/bin/true", &binary).unwrap();
        let checked = check.validate().await.unwrap();
        std::fs::write(&binary, "overwritten").unwrap();
        assert!(checked.execution_path().is_err());
    }
}
