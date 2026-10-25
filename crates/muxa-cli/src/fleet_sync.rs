//! Explicit, automation-only configuration synchronization over trusted Fleet SSH.
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Args as ClapArgs, ValueEnum};
use muxa::automation::AutomationConfig;
use muxa::config::FleetHostConfig;
use muxa::config_file::ConfigDocument;
use muxa::fleet::{sanitize_terminal_text, HostAccessMode};
use muxa::ipc::Client;
use muxa::work_control::shell_quote;
use muxa::Config;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;

const LIMIT: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Section {
    Automation,
}

#[derive(Debug, ClapArgs)]
pub(crate) struct Args {
    /// Source Fleet alias; local and localhost refer to this daemon.
    #[arg(long = "from", default_value = "local")]
    source: String,
    /// Destination Fleet aliases. Explicit targets prevent accidental fan-out.
    #[arg(long = "to", value_delimiter = ',', required = true)]
    targets: Vec<String>,
    #[arg(long, value_enum, default_value = "automation")]
    section: Section,
    /// Back up, conditionally write, and restart each changed target daemon.
    #[arg(long)]
    apply: bool,
    /// Explicit alias for the default preview mode.
    #[arg(long, conflicts_with = "apply")]
    diff: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Transfer {
    expected_text: String,
    automation: AutomationConfig,
}

fn local(alias: &str) -> bool {
    matches!(alias, "local" | "localhost")
}

fn host<'a>(cfg: &'a Config, alias: &str, write: bool) -> Result<&'a FleetHostConfig> {
    let h = cfg
        .fleet
        .hosts
        .get(alias)
        .with_context(|| format!("unknown Fleet host {alias:?}"))?;
    if !h.enabled {
        bail!("Fleet host {alias:?} is disabled");
    }
    if write && h.mode != HostAccessMode::Control {
        bail!("Fleet host {alias:?} is observe-only");
    }
    Ok(h)
}

async fn remote(h: &FleetHostConfig, args: &[&str], input: Option<Vec<u8>>) -> Result<Vec<u8>> {
    let mut words = vec![shell_quote(&h.muxa_path)];
    if let Some(socket) = &h.remote_socket {
        words.extend(["--socket".into(), shell_quote(&socket.to_string_lossy())]);
    }
    words.extend(args.iter().map(|s| shell_quote(s)));
    let mut command = Command::new("ssh");
    command.args([
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        "ClearAllForwardings=yes",
        "-o",
        "ConnectTimeout=8",
        "--",
        &h.ssh,
        &words.join(" "),
    ]);
    command
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().context("starting Fleet sync SSH")?;
    let stdin = child.stdin.take().context("SSH stdin")?;
    let mut stdout = child.stdout.take().context("SSH stdout")?;
    let mut stderr = child.stderr.take().context("SSH stderr")?;
    let operation = async {
        let writer = async move {
            let mut stdin = stdin;
            if let Some(input) = input {
                stdin.write_all(&input).await?;
            }
            stdin.shutdown().await
        };
        let ((), out, err, status) = tokio::try_join!(
            writer,
            muxa::fleet::drain_bounded(&mut stdout, LIMIT),
            muxa::fleet::drain_bounded(&mut stderr, 8192),
            child.wait()
        )?;
        if !status.success() {
            bail!(
                "Fleet sync SSH failed: {}",
                sanitize_terminal_text(&String::from_utf8_lossy(&err))
            );
        }
        if out.len() >= LIMIT {
            bail!("Fleet sync response exceeds limit");
        }
        Ok(out)
    };
    tokio::time::timeout(Duration::from_secs(60), operation)
        .await
        .context("Fleet sync SSH timed out; inspect target before retrying an apply")?
}

async fn read(alias: &str, client: &Client, cfg: &Config) -> Result<ConfigDocument> {
    if local(alias) {
        return Ok(client.config_read().await?);
    }
    Ok(serde_json::from_slice(
        &remote(
            host(cfg, alias, false)?,
            &["config", "show", "--json"],
            None,
        )
        .await?,
    )?)
}

/// Merge by name, retaining target-only rules, pause and all unrelated config.
fn merge(text: &str, source: &AutomationConfig) -> Result<String> {
    source.validate().map_err(anyhow::Error::msg)?;
    let mut document = text.parse::<toml_edit::DocumentMut>()?;
    let current: Config = toml::from_str(text)?;
    let mut merged = current.automation.clone();
    merged.enabled = source.enabled;
    for rule in &source.rule {
        if let Some(old) = merged.rule.iter_mut().find(|old| old.name == rule.name) {
            *old = rule.clone();
        } else {
            merged.rule.push(rule.clone());
        }
    }
    if merged == current.automation {
        return Ok(text.to_owned());
    }
    let encoded = toml::to_string(&merged)?.parse::<toml_edit::DocumentMut>()?;
    document["automation"] = toml_edit::Item::Table(encoded.as_table().clone());
    let result = document.to_string();
    let config: Config = toml::from_str(&result)?;
    config.validate()?;
    Ok(result)
}

fn diff(before: &str, after: &str) -> Result<String> {
    let before: Config = toml::from_str(before)?;
    let after: Config = toml::from_str(after)?;
    let mut lines = String::new();
    if before.automation.enabled != after.automation.enabled {
        write!(
            lines,
            "@@ automation.enabled @@\n-{}\n+{}\n",
            before.automation.enabled, after.automation.enabled
        )?;
    }
    for rule in &after.automation.rule {
        let old = before.automation.rule_named(&rule.name);
        if old == Some(rule) {
            continue;
        }
        writeln!(lines, "@@ automation.rule {:?} @@", rule.name)?;
        if let Some(old) = old {
            for line in toml::to_string(old)?.lines() {
                writeln!(lines, "-{line}")?;
            }
        }
        for line in toml::to_string(rule)?.lines() {
            writeln!(lines, "+{line}")?;
        }
    }
    Ok(sanitize_terminal_text(&lines))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    before: String,
    after: String,
    backup: std::path::PathBuf,
}

pub(crate) fn pending_path(path: &std::path::Path) -> std::path::PathBuf {
    path.with_extension("toml.fleet-sync-pending.json")
}

/// The one lock every sync writer of a config file takes, so Fleet sync and
/// `muxa config apply` never interleave their journals.
pub(crate) async fn lock_config(path: &std::path::Path) -> Result<std::fs::File> {
    lock_config_within(path, LOCK_WAIT).await
}

/// How long a sync waits for the config lock before reporting contention.
/// An flock lives as long as any copy of its descriptor, and a process
/// spawned at the wrong moment holds a copy until it execs; failing on the
/// first refusal turned that brief overlap into "another sync is applying".
const LOCK_WAIT: Duration = Duration::from_secs(2);

async fn lock_config_within(path: &std::path::Path, wait: Duration) -> Result<std::fs::File> {
    let lock = private_options()
        .read(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("toml.fleet-sync.lock"))?;
    let deadline = std::time::Instant::now() + wait;
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(lock),
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => {
                return Err(anyhow::Error::from(error))
                    .context("another sync is applying this config");
            }
        }
    }
}

/// Open options for a file only this user may read: backups and journals
/// hold the whole private config.
pub(crate) fn private_options() -> std::fs::OpenOptions {
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options
}

/// Durably create a journal at `path`, refusing to replace an unfinished one.
pub(crate) fn write_journal<T: Serialize>(path: &std::path::Path, pending: &T) -> Result<()> {
    if path.exists() {
        bail!(
            "an unfinished sync journal already exists: {}",
            path.display()
        );
    }
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = private_options().create_new(true).open(&temp)?;
        file.write_all(&serde_json::to_vec(pending)?)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)?;
        #[cfg(unix)]
        if let Some(parent) = path.parent() {
            std::fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

pub(crate) fn ensure_parent(path: &std::path::Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("daemon returned a relative config path; upgrade muxad or start it with an absolute --config path");
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

async fn persist(
    client: &Client,
    transfer: &Transfer,
) -> Result<Option<(std::path::PathBuf, String)>> {
    let current = client.config_read().await?;
    if current.text != transfer.expected_text {
        bail!("config changed since preview; run sync again");
    }
    let next = merge(&current.text, &transfer.automation)?;
    if next == current.text {
        return Ok(None);
    }
    let backup = current
        .path
        .with_extension(format!("toml.fleet-sync-{}.bak", uuid::Uuid::new_v4()));
    ensure_parent(&current.path)?;
    let mut file = private_options().create_new(true).open(&backup)?;
    file.write_all(current.text.as_bytes())?;
    file.sync_all()?;
    let pending = Pending {
        before: current.text.clone(),
        after: next.clone(),
        backup: backup.clone(),
    };
    write_journal(&pending_path(&current.path), &pending)?;
    // Keep the journal on IPC failure: the write may have landed despite a
    // lost response. A retry resolves it under the same per-config file lock.
    client.config_write(&next, Some(&current.text)).await?;
    Ok(Some((backup, next)))
}

async fn apply(client: &Client, transfer: &Transfer) -> Result<String> {
    apply_with_reload(client, transfer, || async {
        crate::daemon::run(
            crate::daemon::Action::Restart,
            client,
            client.socket(),
            None,
        )
        .await
    })
    .await
}

async fn finish_pending<F, Fut>(
    client: &Client,
    path: &std::path::Path,
    pending: &Pending,
    reload: &F,
) -> Result<()>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    reload().await.with_context(|| {
        format!(
            "config saved but reload incomplete; retry --apply; backup {}",
            pending.backup.display()
        )
    })?;
    if client.config_read().await?.text != pending.after {
        bail!(
            "config changed during reload; inspect target and backup {}",
            pending.backup.display()
        );
    }
    std::fs::remove_file(pending_path(path))?;
    Ok(())
}

async fn apply_with_reload<F, Fut>(
    client: &Client,
    transfer: &Transfer,
    reload: F,
) -> Result<String>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let initial = client.config_read().await?;
    if merge(&initial.text, &transfer.automation)? == initial.text
        && !pending_path(&initial.path).exists()
    {
        return Ok("unchanged".into());
    }
    ensure_parent(&initial.path)?;
    let _lock = lock_config(&initial.path).await?;
    let other = crate::config_sync_cmd::journal_path(&initial.path);
    if other.exists() {
        bail!(
            "an unfinished `muxa config apply` holds this config; rerun it to recover ({})",
            other.display()
        );
    }
    let current = client.config_read().await?;
    let journal = pending_path(&current.path);
    let mut recovered = false;
    if journal.exists() {
        let pending: Pending = serde_json::from_slice(&std::fs::read(&journal)?)?;
        if current.text == pending.after {
            finish_pending(client, &current.path, &pending, &reload).await?;
            recovered = true;
        } else if current.text == pending.before {
            // Previous process stopped before the conditional write landed.
            std::fs::remove_file(&journal)?;
        } else {
            bail!(
                "unfinished sync conflicts with a later config edit; inspect {} and backup {}",
                journal.display(),
                pending.backup.display()
            );
        }
    }
    let Some((backup, next)) = persist(client, transfer).await? else {
        return Ok(if recovered {
            "recovered unfinished reload"
        } else {
            "unchanged"
        }
        .into());
    };
    let pending = Pending {
        before: current.text,
        after: next,
        backup: backup.clone(),
    };
    finish_pending(client, &current.path, &pending, &reload).await?;
    Ok(format!("applied; backup {}", backup.display()))
}

pub(crate) async fn receive(client: &Client) -> Result<()> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take((LIMIT + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > LIMIT {
        bail!("sync payload exceeds limit");
    }
    let transfer: Transfer = serde_json::from_slice(&bytes)?;
    println!("{}", apply(client, &transfer).await?);
    Ok(())
}

pub(crate) async fn run(args: Args, client: &Client, cfg: &Config) -> Result<()> {
    let mut aliases = BTreeSet::new();
    for target in &args.targets {
        let key = if local(target) { "local" } else { target };
        if !aliases.insert(key) {
            bail!("duplicate target {target:?}");
        }
        if !local(target) {
            host(cfg, target, args.apply)?;
        }
    }
    let source = read(&args.source, client, cfg).await?;
    let config: Config = toml::from_str(&source.text)?;
    config.automation.validate().map_err(anyhow::Error::msg)?;
    let mut plans = Vec::new();
    // Read and validate every target before any mutation.
    for target in &args.targets {
        let current = read(target, client, cfg)
            .await
            .with_context(|| format!("reading {target}"))?;
        if args.apply && !current.path.is_absolute() {
            bail!("{target}: daemon returned a relative config path; upgrade muxad or start it with an absolute --config path");
        }
        let next = merge(&current.text, &config.automation)?;
        println!("--- {target}/automation\n+++ {}/automation", args.source);
        if next == current.text {
            println!("unchanged (target-only rules and pause retained)");
        } else {
            print!("{}", diff(&current.text, &next)?);
        }
        plans.push((target, current, next));
    }
    if !args.apply {
        println!("Preview only. Repeat with --apply to back up, write and reload changed hosts.");
        return Ok(());
    }
    // Fail before mutating any host when an old remote CLI cannot apply.
    for (target, current, _next) in &plans {
        let transfer = Transfer {
            expected_text: current.text.clone(),
            automation: config.automation.clone(),
        };
        if serde_json::to_vec(&transfer)?.len() > LIMIT {
            bail!("{target}: sync payload exceeds limit; no hosts have been applied");
        }
        if !local(target) {
            remote(
                host(cfg, target, true)?,
                &["fleet", "sync-receive", "--help"],
                None,
            )
            .await
            .with_context(|| format!("{target} needs a CLI with Fleet sync support"))?;
        }
    }
    for (target, current, _next) in plans {
        let transfer = Transfer {
            expected_text: current.text,
            automation: config.automation.clone(),
        };
        let result = if local(target) {
            apply(client, &transfer).await?
        } else {
            let output = remote(
                host(cfg, target, true)?,
                &["fleet", "sync-receive"],
                Some(serde_json::to_vec(&transfer)?),
            )
            .await
            .with_context(|| {
                format!("applying {target}; earlier targets may already be applied")
            })?;
            sanitize_terminal_text(&String::from_utf8_lossy(&output))
        };
        println!("{target}: {result}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn merge_preserves_private_config_local_rules_pause_and_is_idempotent() {
        let before = "# private comment\n[dashboard]\ntoken = 'secret'\n[automation]\npaused_until = '2030-01-01T00:00:00Z'\n[[automation.rule]]\nname = 'local'\non = 'error'\naction = 'notify'\nmessage = 'local'\n[[automation.rule]]\nname = 'shared'\non = 'error'\naction = 'notify'\nmessage = 'old'\n";
        let source: AutomationConfig = toml::from_str("enabled = false\n[[rule]]\nname = 'shared'\non = 'error'\naction = 'notify'\nmessage = 'new'\n").unwrap();
        let next = merge(before, &source).unwrap();
        let parsed: Config = toml::from_str(&next).unwrap();
        assert_eq!(parsed.automation.rule.len(), 2);
        assert!(parsed.automation.paused_until.is_some());
        assert!(next.contains("# private comment"));
        assert!(next.contains("token = 'secret'"));
        assert_eq!(merge(&next, &source).unwrap(), next);
        let delta = diff(before, &next).unwrap();
        assert!(delta.contains("-message = \"old\""));
        assert!(delta.contains("+message = \"new\""));
        assert!(!delta.contains("secret"));
        assert!(!delta.contains("name = \"local\""));
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::*;

    #[tokio::test]
    async fn conflicts_do_not_write_and_success_keeps_private_backup() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("sync.sock");
        let path = dir.path().join("config.toml");
        let before = "[ask]\nenabled = false\n";
        std::fs::write(&path, before).unwrap();
        let server = muxa::ipc::Server::new(socket.clone(), muxa::Store::shared())
            .with_config_path(Some(path.clone()));
        let (shutdown, rx) = tokio::sync::broadcast::channel(1);
        let task = tokio::spawn(async move { server.run(rx).await.unwrap() });
        let client = Client::new(socket);
        for _ in 0..100 {
            if client.config_read().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut transfer = Transfer {
            expected_text: "stale".into(),
            automation: AutomationConfig {
                enabled: false,
                ..Default::default()
            },
        };
        assert!(persist(&client, &transfer).await.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
        transfer.expected_text = before.into();
        let (backup, next) = persist(&client, &transfer).await.unwrap().unwrap();
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), before);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(backup).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(std::fs::read_to_string(path).unwrap(), next);
        transfer.expected_text = next;
        assert!(persist(&client, &transfer).await.unwrap().is_none());
        shutdown.send(()).unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn first_sync_creates_a_missing_config_directory_and_backup() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("fresh-sync.sock");
        let path = dir.path().join("not-created-yet/muxa/config.toml");
        let server = muxa::ipc::Server::new(socket.clone(), muxa::Store::shared())
            .with_config_path(Some(path.clone()));
        let (shutdown, rx) = tokio::sync::broadcast::channel(1);
        let task = tokio::spawn(async move { server.run(rx).await.unwrap() });
        let client = Client::new(socket);
        for _ in 0..100 {
            if client.config_read().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let transfer = Transfer {
            expected_text: String::new(),
            automation: AutomationConfig {
                enabled: false,
                ..Default::default()
            },
        };
        let result = persist(&client, &transfer).await;
        shutdown.send(()).unwrap();
        task.await.unwrap();
        let (backup, next) = result.unwrap().unwrap();
        assert_eq!(std::fs::read_to_string(backup).unwrap(), "");
        assert_eq!(std::fs::read_to_string(path).unwrap(), next);
    }

    #[test]
    fn observe_targets_are_readable_but_cannot_be_written() {
        let mut config = Config::default();
        config.fleet.hosts.insert(
            "dev".into(),
            FleetHostConfig {
                ssh: "dev".into(),
                enabled: true,
                mode: HostAccessMode::Observe,
                ..Default::default()
            },
        );
        assert!(host(&config, "dev", false).is_ok());
        assert!(host(&config, "dev", true).is_err());
        assert!(host(&config, "missing", false).is_err());
        config.fleet.hosts.get_mut("dev").unwrap().enabled = false;
        assert!(host(&config, "dev", false).is_err());
    }
}

#[cfg(test)]
mod reload_recovery_tests {
    use super::*;

    #[tokio::test]
    async fn retry_recovers_a_failed_reload_even_when_config_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let socket = dir.path().join("recovery.sock");
        let before = "[automation]\nenabled = true\n";
        std::fs::write(&path, before).unwrap();
        let server = muxa::ipc::Server::new(socket.clone(), muxa::Store::shared())
            .with_config_path(Some(path.clone()));
        let (shutdown, rx) = tokio::sync::broadcast::channel(1);
        let task = tokio::spawn(async move { server.run(rx).await.unwrap() });
        let client = Client::new(socket);
        for _ in 0..100 {
            if client.config_read().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut transfer = Transfer {
            expected_text: before.into(),
            automation: AutomationConfig {
                enabled: false,
                ..Default::default()
            },
        };
        let failed = apply_with_reload(&client, &transfer, || async {
            bail!("fixture reload failed")
        })
        .await;
        assert!(failed
            .unwrap_err()
            .to_string()
            .contains("reload incomplete"));
        assert!(pending_path(&path).exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(pending_path(&path))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        transfer.expected_text = std::fs::read_to_string(&path).unwrap();
        let calls = std::cell::Cell::new(0);
        let result = apply_with_reload(&client, &transfer, || async {
            calls.set(calls.get() + 1);
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(result, "recovered unfinished reload");
        assert_eq!(calls.get(), 1);
        assert!(!pending_path(&path).exists());
        let count = std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(
            apply_with_reload(&client, &transfer, || async {
                panic!("unchanged config must not reload")
            })
            .await
            .unwrap(),
            "unchanged"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), count);
        shutdown.send(()).unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn the_config_lock_waits_out_a_brief_holder_but_reports_real_contention() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let held = lock_config(&config).await.unwrap();
        // Released while the second writer is still waiting: it gets the lock.
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(held);
        });
        let started = std::time::Instant::now();
        let second = lock_config_within(&config, Duration::from_secs(2))
            .await
            .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(150));
        release.join().unwrap();
        // Held for longer than the wait: contention is reported.
        let error = lock_config_within(&config, Duration::from_millis(200))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("another sync"), "{error}");
        drop(second);
    }

    #[test]
    fn apply_lock_excludes_another_process_handle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sync.lock");
        let first = private_options()
            .read(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let second = private_options().read(true).open(&path).unwrap();
        first.try_lock().unwrap();
        assert!(second.try_lock().is_err());
        drop(first);
        second.try_lock().unwrap();
    }
}
