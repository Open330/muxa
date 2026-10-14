//! Invalidate Codex's unchanged-client cache when the registered Muxa binary changes.
use anyhow::{bail, Context, Result};
use clap::Parser;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub(crate) const ENV_KEY: &str = "MUXA_MCP_BINARY_REVISION";

pub(crate) fn binary_revision(binary: &Path) -> Result<String> {
    let mut file = std::fs::File::open(binary).context("read installed Muxa binary revision")?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let size = file.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        hash.update(&buffer[..size]);
    }
    Ok(format!("sha256:{:x}", hash.finalize()))
}

pub(crate) fn revision_tag(revision: &str) -> Option<&str> {
    let hash = revision.strip_prefix("sha256:")?;
    (hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())).then(|| &hash[..16])
}

pub(crate) fn server_version() -> String {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION
        .get_or_init(|| {
            let declared = std::env::var(ENV_KEY).ok();
            if declared.as_deref().and_then(revision_tag).is_none() {
                return env!("CARGO_PKG_VERSION").into();
            }
            // Linux keeps the executing image pinned here even if its installed pathname changes.
            #[cfg(target_os = "linux")]
            let actual = binary_revision(Path::new("/proc/self/exe")).ok();
            #[cfg(not(target_os = "linux"))]
            let actual = std::env::current_exe()
                .ok()
                .and_then(|path| binary_revision(&path).ok());
            actual.as_deref().and_then(revision_tag).map_or_else(
                || env!("CARGO_PKG_VERSION").into(),
                |tag| format!("{}+{tag}", env!("CARGO_PKG_VERSION")),
            )
        })
        .clone()
}

pub(crate) struct RevisionPlan {
    pub path: PathBuf,
    pub revision: String,
    before: String,
}

impl RevisionPlan {
    pub fn prepare(home: &Path, binary: &Path, revision: &str) -> Result<Option<Self>> {
        let path = home.join("config.toml");
        let before = match std::fs::read_to_string(&path) {
            Ok(before) => before,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("read Codex profile configuration"),
        };
        let document: toml::Value = before.parse().map_err(|_| {
            anyhow::anyhow!("Codex configuration could not be parsed; repair it before reload")
        })?;
        let Some(server) = document
            .get("mcp_servers")
            .and_then(|servers| servers.get("muxa"))
        else {
            return Ok(None);
        };
        if server.get("enabled").and_then(toml::Value::as_bool) == Some(false) {
            return Ok(None);
        }
        let command = server
            .get("command")
            .and_then(toml::Value::as_str)
            .unwrap_or_default();
        let managed_command = command == "muxa"
            || (!command.is_empty()
                && Path::new(command)
                    .canonicalize()
                    .ok()
                    .zip(binary.canonicalize().ok())
                    .is_some_and(|(a, b)| a == b));
        let args = server
            .get("args")
            .and_then(toml::Value::as_array)
            .and_then(|args| {
                args.iter()
                    .map(toml::Value::as_str)
                    .collect::<Option<Vec<_>>>()
            });
        let stdio = server.get("url").is_none()
            && args.is_some_and(|args| {
                crate::Args::try_parse_from(std::iter::once("muxa").chain(args))
                    .is_ok_and(|args| matches!(args.cmd, crate::Cmd::Mcp))
            });
        if !managed_command || !stdio {
            return Ok(None);
        }
        if server
            .get("env")
            .and_then(|env| env.get(ENV_KEY))
            .and_then(toml::Value::as_str)
            == Some(revision)
        {
            return Ok(None);
        }
        Ok(Some(Self {
            path: path.canonicalize()?,
            revision: revision.into(),
            before,
        }))
    }

    /// Use the exact user-layer version with Codex's atomic config writer.
    pub fn expected_version<'a>(&self, config: &'a Value) -> Result<&'a str> {
        config.get("layers").and_then(Value::as_array).into_iter().flatten().find_map(|layer| {
            let name = layer.get("name")?;
            let path = Path::new(name.get("file")?.as_str()?).canonicalize().ok()?;
            (name.get("type")?.as_str()? == "user" && path == self.path).then(|| layer.get("version")?.as_str()).flatten()
        }).context("Codex did not provide the selected user configuration version; no revision was written")
    }

    pub fn prune_backups(&self) -> Result<()> {
        let parent = self
            .path
            .parent()
            .context("Codex configuration has no parent")?;
        let mut backups = Vec::new();
        for entry in std::fs::read_dir(parent)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name();
            let Some(stamp) = name
                .to_str()
                .and_then(|name| name.strip_prefix("config.toml.muxa-mcp-reload-"))
                .and_then(|name| name.strip_suffix(".bak"))
                .and_then(|stamp| stamp.parse::<u128>().ok())
            else {
                continue;
            };
            backups.push((stamp, entry.path()));
        }
        backups.sort_by_key(|(stamp, _)| *stamp);
        let excess = backups.len().saturating_sub(3);
        for (_, path) in backups.into_iter().take(excess) {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }

    pub fn backup(&self) -> Result<PathBuf> {
        if std::fs::read_to_string(&self.path)? != self.before {
            bail!("Codex configuration changed while planning reload; no revision was written");
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = self
            .path
            .with_file_name(format!("config.toml.muxa-mcp-reload-{stamp}.bak"));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(&path)?.write_all(self.before.as_bytes())?;
        Ok(path)
    }
}

/// The reserved revision is Muxa-owned cache metadata, not a user edit.
pub(crate) fn entries_equal(a: Option<&Value>, b: Option<&Value>) -> bool {
    let clean = |value: &Value| {
        let mut value = value.clone();
        if let Some(env) = value.get_mut("env").and_then(Value::as_object_mut) {
            env.remove(ENV_KEY);
            if env.is_empty() {
                value
                    .as_object_mut()
                    .expect("server entry is an object")
                    .remove("env");
            }
        }
        value
    };
    a.map(clean) == b.map(clean)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn revision_is_stable_and_only_registered_local_muxa_is_changed() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("muxa");
        std::fs::write(&binary, "same binary").unwrap();
        let revision = binary_revision(&binary).unwrap();
        assert_eq!(revision, binary_revision(&binary).unwrap());
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "# preserve me\n[mcp_servers.muxa]\ncommand='muxa'\nargs=['mcp']\n",
        )
        .unwrap();
        let plan = RevisionPlan::prepare(dir.path(), &binary, &revision)
            .unwrap()
            .unwrap();
        let backup = plan.backup().unwrap();
        assert_eq!(
            std::fs::read_to_string(backup).unwrap(),
            std::fs::read_to_string(&path).unwrap()
        );
        std::fs::write(&path, format!("[mcp_servers.muxa]\ncommand='muxa'\nargs=['mcp']\n[mcp_servers.muxa.env]\n{ENV_KEY}='{revision}'\n")).unwrap();
        assert!(RevisionPlan::prepare(dir.path(), &binary, &revision)
            .unwrap()
            .is_none());
        std::fs::write(
            &path,
            "[mcp_servers.muxa]\nurl='https://example.invalid/mcp'\n",
        )
        .unwrap();
        assert!(RevisionPlan::prepare(dir.path(), &binary, &revision)
            .unwrap()
            .is_none());
    }

    #[test]
    fn disabled_registration_is_never_selected_for_revision_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        for (setting, selected) in [
            ("enabled=false\n", false),
            ("enabled=true\n", true),
            ("", true),
        ] {
            let config = format!("[mcp_servers.muxa]\ncommand='muxa'\nargs=['mcp']\n{setting}");
            std::fs::write(&path, &config).unwrap();
            assert_eq!(
                RevisionPlan::prepare(dir.path(), Path::new("/none"), "sha256:test")
                    .unwrap()
                    .is_some(),
                selected
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), config);
        }
    }

    #[test]
    fn ownership_ignores_only_the_reserved_revision() {
        let original = json!({"command":"muxa","args":["mcp"],"env":{"USER_VAR":"kept"}});
        let mut revised = original.clone();
        revised["env"][ENV_KEY] = json!("sha256:new");
        assert!(entries_equal(Some(&original), Some(&revised)));
        revised["env"]["USER_VAR"] = json!("user changed");
        assert!(!entries_equal(Some(&original), Some(&revised)));
        assert!(!entries_equal(None, Some(&revised)));
    }

    #[test]
    fn backup_retention_preserves_unrelated_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[mcp_servers.muxa]\ncommand='muxa'\nargs=['mcp']\n",
        )
        .unwrap();
        let plan = RevisionPlan::prepare(dir.path(), Path::new("/none"), "sha256:test")
            .unwrap()
            .unwrap();
        for stamp in 1..=5 {
            std::fs::write(
                dir.path()
                    .join(format!("config.toml.muxa-mcp-reload-{stamp}.bak")),
                "backup",
            )
            .unwrap();
        }
        let unrelated = dir.path().join("config.toml.user-backup.bak");
        std::fs::write(&unrelated, "user backup").unwrap();
        plan.prune_backups().unwrap();
        assert!(!dir
            .path()
            .join("config.toml.muxa-mcp-reload-1.bak")
            .exists());
        assert!(!dir
            .path()
            .join("config.toml.muxa-mcp-reload-2.bak")
            .exists());
        for stamp in 3..=5 {
            assert!(dir
                .path()
                .join(format!("config.toml.muxa-mcp-reload-{stamp}.bak"))
                .exists());
        }
        assert!(unrelated.exists());
    }

    #[test]
    fn concurrent_config_changes_are_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[mcp_servers.muxa]\ncommand='muxa'\nargs=['mcp']\n").unwrap();
        let plan = RevisionPlan::prepare(dir.path(), Path::new("/none"), "sha256:test")
            .unwrap()
            .unwrap();
        std::fs::write(path, "user changed it").unwrap();
        assert!(plan
            .backup()
            .unwrap_err()
            .to_string()
            .contains("changed while planning"));
    }
}
