//! Codex hook trust across account homes.
//!
//! Codex asks to review hooks that are new or changed ("Hooks need review")
//! and keeps the answer in `config.toml` as
//! `[hooks.state."<CODEX_HOME>/config.toml:<event>:<group>:<hook>"]
//! trusted_hash = "sha256:…"`. The key is the config path *as reached
//! through that `CODEX_HOME`*, not the file it resolves to, so the aas
//! profiles, whose `config.toml` is a symlink to `~/.codex/config.toml`,
//! share their hooks but each need their own trust entry. The hash depends
//! only on the hook's definition (checked against Codex 0.167.2's
//! `hooks/list`: the same `currentHash` under every path).
//!
//! A swap relaunches Codex under another account's home. If that home's
//! entry is missing, or still holds the hash of an older definition, the
//! relaunched agent stops on the review menu. tend never answers that menu.
//! Instead, before the relaunch, a hook the user already trusted as their
//! own under another account home on this machine — Codex itself reports
//! it `trusted`, a `user` hook from that home's own `config.toml`, same
//! event, same current hash — gets that trust under the target's key.
//! A raw `hooks.state` entry is never taken as proof: only what Codex
//! answers about a hook it actually loads.
//!
//! Everything goes through Codex's own app-server (`hooks/list` to read
//! each hook's key, current hash, source and trust status;
//! `config/value/write` to upsert `hooks.state`), so Codex writes its own
//! file, through the symlink, in its own format.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long one app-server request (written and answered) may take.
const RPC_TIMEOUT: Duration = Duration::from_secs(15);
/// How long `aas export` may take.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(15);
/// The whole of one swap's or one audit's trust work.
pub const BUDGET: Duration = Duration::from_secs(60);

/// How to run the Codex that is asked: a program, and for a Codex run by an
/// interpreter (`node …/codex.js`), the script it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Codex {
    pub program: PathBuf,
    pub args: Vec<PathBuf>,
}

impl Codex {
    /// A Codex program run as itself.
    #[must_use]
    pub fn program(program: PathBuf) -> Self {
        Self {
            program,
            args: Vec::new(),
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        command
    }

    /// The command line, for a report.
    #[must_use]
    pub fn display(&self) -> String {
        std::iter::once(&self.program)
            .chain(&self.args)
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// One hook as `hooks/list` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hook {
    /// `<config path>:<event>:<group>:<hook>`.
    pub key: String,
    pub event: String,
    pub command: Option<String>,
    pub current_hash: String,
    /// `trusted`, `untrusted`, `modified` or `managed`.
    pub trust: String,
    /// `user`, `project`, `system`, …
    pub source: String,
}

impl Hook {
    /// Waits on the user's review at startup.
    #[must_use]
    pub fn needs_review(&self) -> bool {
        matches!(self.trust.as_str(), "untrusted" | "modified")
    }
}

/// One hook the user trusted as their own under some home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trusted {
    pub key: String,
    pub event: String,
    pub hash: String,
}

/// The event of `key` when it is exactly
/// `<home>/config.toml:<event>:<group>:<hook>` for `home`'s own user config.
/// The directory is compared resolved (Codex may print `/private/var/…` for
/// `/var/…`); the `config.toml` link itself is not followed, so a profile
/// sharing another home's file is still its own home.
fn own_key_event<'a>(key: &'a str, home: &Path) -> Option<&'a str> {
    let mut parts = key.rsplitn(4, ':');
    let (hook, group, event, path) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    let valid = !event.is_empty()
        && event.chars().all(|c| c.is_ascii_lowercase() || c == '_')
        && group.parse::<u32>().is_ok()
        && hook.parse::<u32>().is_ok();
    let path = Path::new(path);
    let same_home = path.file_name().is_some_and(|name| name == "config.toml")
        && path.parent().is_some_and(|dir| same_dir(dir, home));
    (valid && same_home).then_some(event)
}

/// Whether two directories are one, by their resolved paths.
fn same_dir(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(a), Ok(b)) if a == b
        )
}

/// What `home`'s own hooks vouch for: those Codex reports `trusted`, from
/// the user's config of that very home.
#[must_use]
pub fn vouched_by(hooks: &[Hook], home: &Path) -> Vec<Trusted> {
    hooks
        .iter()
        .filter(|hook| hook.trust == "trusted" && hook.source == "user")
        .filter_map(|hook| {
            let event = own_key_event(&hook.key, home)?;
            (event == hook.event).then(|| Trusted {
                key: hook.key.clone(),
                event: hook.event.clone(),
                hash: hook.current_hash.clone(),
            })
        })
        .collect()
}

/// The trust entries to add under `hooks`' own keys: for each of the
/// target's own user hooks waiting on review, one trusted as the user's own
/// under another home — same event, same hash as the hook is now. Hooks
/// with no such twin are returned apart; they are left for the user.
#[must_use]
pub fn plan(hooks: &[Hook], home: &Path, trusted: &[Trusted]) -> (Vec<Trusted>, Vec<Hook>) {
    let mut copies = Vec::new();
    let mut left = Vec::new();
    // A project's hooks belong to its repository, not to the account: they
    // are neither trusted nor reported here. Everything else the home loads
    // (its user hooks, installed plugins) that waits on review is reported;
    // only its own user hooks can have trust carried to them.
    for hook in hooks
        .iter()
        .filter(|hook| hook.needs_review() && hook.source != "project")
    {
        let own =
            hook.source == "user" && own_key_event(&hook.key, home) == Some(hook.event.as_str());
        let vouched = own
            && trusted.iter().any(|entry| {
                entry.key != hook.key
                    && entry.hash == hook.current_hash
                    && entry.event == hook.event
            });
        if vouched {
            copies.push(Trusted {
                key: hook.key.clone(),
                event: hook.event.clone(),
                hash: hook.current_hash.clone(),
            });
        } else {
            left.push(hook.clone());
        }
    }
    (copies, left)
}

/// The `CODEX_HOME` a profile launches with (see [`profile_var`]), when it
/// is an absolute path.
fn profile_home(profile: &tend::accounts::AccountProfile) -> Option<PathBuf> {
    let value = profile_var(profile, "CODEX_HOME").ok()??;
    let path = expand_home(&value);
    path.is_absolute().then_some(path)
}

/// What `name` is when a profile launches, read without running anything:
/// its explicit `env` wins (the launch line sets it after the file is
/// sourced); otherwise the last plain assignment in its `env_file`.
/// `Ok(None)`: the launch leaves it as the launching process has it.
/// `Err`: the file is more than plain literal assignments, so it cannot be
/// told without running it.
fn profile_var(profile: &tend::accounts::AccountProfile, name: &str) -> Result<Option<String>, ()> {
    if let Some(value) = profile.env.get(name) {
        return Ok(Some(value.clone()));
    }
    let Some(file) = profile.env_file.as_deref() else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(expand_home(file)).map_err(|_| ())?;
    assigned(&text, name)
}

/// The `CODEX_HOME` an env file sets, when the whole file is nothing but
/// blank lines, comments and plain `[export ]NAME=value` assignments, and
/// that value is a literal absolute path. Anything else (control flow, a
/// command, a computed or `~` value) is not guessed at: `None`.
#[cfg(test)]
fn assigned_home(text: &str) -> Option<String> {
    assigned(text, "CODEX_HOME")
        .ok()?
        .filter(|value| value.starts_with('/'))
}

/// The last value an env file assigns to `name`, when the whole file is
/// nothing but blank lines, comments and plain `[export ]NAME=value`
/// assignments with literal values. `Err`: anything else (control flow, a
/// command, a computed or `~` value). `Ok(None)`: plain, but `name` is not
/// assigned.
fn assigned(text: &str, name: &str) -> Result<Option<String>, ()> {
    let mut found = None;
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").map_or(line, str::trim_start);
        let (key, value) = line.split_once('=').ok_or(())?;
        let plain_name = !key.is_empty()
            && key.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
            && !key.starts_with(|c: char| c.is_ascii_digit());
        let value = literal(value).ok_or(())?;
        if !plain_name {
            return Err(());
        }
        if key == name {
            found = Some(value);
        }
    }
    Ok(found.filter(|value| !value.is_empty()))
}

/// A shell word that means exactly its text: single-quoted, double-quoted
/// with nothing the shell expands, or bare with no special character.
fn literal(word: &str) -> Option<String> {
    if let Some(inner) = word.strip_prefix('\'').and_then(|w| w.strip_suffix('\'')) {
        return (!inner.contains('\'')).then(|| inner.to_owned());
    }
    if let Some(inner) = word.strip_prefix('"').and_then(|w| w.strip_suffix('"')) {
        return (!inner.contains(['$', '`', '\\', '"'])).then(|| inner.to_owned());
    }
    let special = |c: char| c.is_whitespace() || "$`\\\"'~;&|<>(){}*?[]!#".contains(c);
    (!word.contains(special)).then(|| word.to_owned())
}

fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}

/// The account homes on this machine whose own hooks can vouch for a hook:
/// `~/.codex`, `$CODEX_HOME`, the configured accounts' `CODEX_HOME`, and
/// the aas profiles (`asx/profiles/codex-*` under the data directory and
/// `~/.config`). Each listed once.
#[must_use]
pub fn known_homes(codex_profiles: &[tend::accounts::AccountProfile]) -> Vec<PathBuf> {
    let mut homes = Vec::new();
    let home = dirs::home_dir();
    homes.extend(home.as_ref().map(|home| home.join(".codex")));
    homes.extend(std::env::var_os("CODEX_HOME").map(PathBuf::from));
    homes.extend(codex_profiles.iter().filter_map(profile_home));
    let mut bases = Vec::new();
    bases.extend(dirs::data_dir().map(|data| data.join("asx/profiles")));
    bases.extend(home.as_ref().map(|home| home.join(".config/asx/profiles")));
    for base in bases {
        let Ok(entries) = std::fs::read_dir(base) else {
            continue;
        };
        let mut found: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("codex-") && !name.starts_with("codex-."))
            })
            .collect();
        found.sort();
        homes.extend(found);
    }
    let mut seen = Vec::new();
    homes.retain(|home| {
        if seen.contains(home) {
            false
        } else {
            seen.push(home.clone());
            true
        }
    });
    homes
}

/// The `CODEX_HOME` an account's relaunch runs under: the configured
/// profile's own (`env`, then `env_file`), or what `aas export <account>`
/// sets. `None` when it cannot be told.
#[must_use]
pub fn account_home(
    profiles: &[tend::accounts::AccountProfile],
    account: &str,
    aas: &str,
) -> Option<PathBuf> {
    account_home_within(profiles, account, aas, EXPORT_TIMEOUT)
}

/// [`account_home`], with `aas export` given at most `timeout`.
fn account_home_within(
    profiles: &[tend::accounts::AccountProfile],
    account: &str,
    aas: &str,
    timeout: Duration,
) -> Option<PathBuf> {
    if !profiles.is_empty() {
        return tend::accounts::find_profile(profiles, "codex", account).and_then(profile_home);
    }
    let program = crate::account_swap::find_aas(aas)?;
    let name = if account.contains('@') {
        account.to_owned()
    } else {
        format!("{account}@codex")
    };
    let output = run_bounded(Command::new(program).args(["export", &name]), timeout)?;
    export_home(&output)
}

/// The `CODEX_HOME` line of `aas export` output. Nothing else of it is
/// read or kept: the rest may carry credentials.
fn export_home(output: &str) -> Option<PathBuf> {
    output
        .lines()
        .find_map(|line| line.trim().strip_prefix("export CODEX_HOME="))
        .map(|value| PathBuf::from(value.trim().trim_matches('"').trim_matches('\'')))
        .filter(|path| path.is_absolute())
}

/// `command`'s standard output, if it succeeded within `timeout`. A child
/// whose descendant keeps the pipe open past its exit is not waited for.
fn run_bounded(command: &mut Command, timeout: Duration) -> Option<String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (send, read) = mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = send.send(std::io::Read::read_to_string(&mut stdout, &mut text).map(|_| text));
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let text = read
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()?
        .ok()?;
    status.success().then_some(text)
}

/// A short conversation with `codex app-server` over stdio under `home`.
/// Writing happens on its own thread, so a server that stops reading can
/// only make a request time out; the child is killed and reaped on drop.
struct AppServer {
    child: Child,
    writer: Option<mpsc::Sender<String>>,
    lines: mpsc::Receiver<String>,
    next: u64,
    /// No request outlives this.
    deadline: Instant,
}

impl AppServer {
    fn open(codex: &Codex, home: &Path, cwd: &Path, deadline: Instant) -> Result<Self, String> {
        let mut child = codex
            .command()
            .arg("app-server")
            .env("CODEX_HOME", home)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("cannot start {} app-server: {error}", codex.display()))?;
        let stdout = child.stdout.take().ok_or("no app-server output")?;
        let mut stdin = child.stdin.take().ok_or("no app-server input")?;
        let (send, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        let (writer, outgoing) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in outgoing {
                if writeln!(stdin, "{line}")
                    .and_then(|()| stdin.flush())
                    .is_err()
                {
                    break;
                }
            }
        });
        let mut server = Self {
            child,
            writer: Some(writer),
            lines,
            next: 0,
            deadline,
        };
        server.call(
            "initialize",
            serde_json::json!({"clientInfo": {"name": "tend", "version": env!("CARGO_PKG_VERSION")}}),
        )?;
        server.send(&serde_json::json!({"jsonrpc": "2.0", "method": "initialized"}))?;
        Ok(server)
    }

    fn send(&self, message: &serde_json::Value) -> Result<(), String> {
        self.writer
            .as_ref()
            .ok_or("app-server input closed")?
            .send(message.to_string())
            .map_err(|_| "app-server input closed".to_owned())
    }

    fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        self.next += 1;
        let id = self.next;
        let deadline = (Instant::now() + RPC_TIMEOUT).min(self.deadline);
        self.send(
            &serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        )?;
        loop {
            let line = self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .map_err(|_| format!("app-server did not answer {method}"))?;
            let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if message.get("id").and_then(serde_json::Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                return Err(format!("app-server {method}: {error}"));
            }
            return message
                .get("result")
                .cloned()
                .ok_or_else(|| format!("app-server {method}: no result"));
        }
    }

    /// The hooks Codex loads under this home for a session in `cwd`.
    fn hooks(&mut self, cwd: &Path) -> Result<Vec<Hook>, String> {
        let cwd = cwd
            .to_str()
            .ok_or("the directory to ask from is not UTF-8")?;
        let result = self.call("hooks/list", serde_json::json!({ "cwds": [cwd] }))?;
        Ok(parse_hooks(&result))
    }
}

impl Drop for AppServer {
    fn drop(&mut self) {
        self.writer = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The hooks `codex` loads under `home` for a session in `cwd`.
fn list_hooks(
    codex: &Codex,
    home: &Path,
    cwd: &Path,
    deadline: Instant,
) -> Result<Vec<Hook>, String> {
    AppServer::open(codex, home, cwd, deadline)?.hooks(cwd)
}

/// `hooks/list`'s result as hooks.
#[must_use]
fn parse_hooks(result: &serde_json::Value) -> Vec<Hook> {
    let text = |hook: &serde_json::Value, field: &str| {
        hook.get(field)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    result
        .get("data")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("hooks").and_then(serde_json::Value::as_array))
        .flatten()
        .filter_map(|hook| {
            Some(Hook {
                key: text(hook, "key")?,
                event: snake(&text(hook, "eventName")?),
                command: text(hook, "command"),
                current_hash: text(hook, "currentHash")?,
                trust: text(hook, "trustStatus")?,
                source: text(hook, "source").unwrap_or_default(),
            })
        })
        .collect()
}

/// `permissionRequest` → `permission_request`, the spelling of the keys.
fn snake(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_uppercase() {
            out.push('_');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// What [`prepare`] did for one relaunch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prepared {
    /// The home as Codex resolves it (its trust keys use this path).
    pub home: PathBuf,
    /// Trust entries copied to the target's keys.
    pub copied: usize,
    /// Hooks that will still ask for review: no trusted twin anywhere.
    pub left: Vec<String>,
}

impl Prepared {
    /// A note for the swap report, or `None` when there was nothing to do.
    #[must_use]
    pub fn note(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.copied > 0 {
            parts.push(format!(
                "carried over the trust of {} Codex hook(s) already trusted under another \
                 account to {}",
                self.copied,
                self.home.display()
            ));
        }
        if !self.left.is_empty() {
            parts.push(format!(
                "{} Codex hook(s) in {} are not trusted under any other account here and will \
                 ask for review: {}",
                self.left.len(),
                self.home.display(),
                self.left.join(", ")
            ));
        }
        (!parts.is_empty()).then(|| parts.join("; "))
    }
}

/// Before Codex is relaunched under `home` in `cwd`: copy the trust of
/// hooks the user trusted as their own under the other `homes` to `home`'s
/// own keys, so the relaunch does not stop on the review menu. Donor homes
/// are asked one at a time, only while something still waits, within
/// [`BUDGET`]. Errors leave the file as it was (the relaunch then shows the
/// menu, and says so).
pub fn prepare(
    codex: &Codex,
    home: &Path,
    homes: &[PathBuf],
    budget: Duration,
) -> Result<Prepared, String> {
    let neutral = Neutral::new()?;
    let cwd = neutral.path();
    let deadline = Instant::now() + budget;
    let mut target = AppServer::open(codex, home, cwd, deadline)?;
    let hooks = target.hooks(cwd)?;
    let resolved = resolved_home(home);
    if !hooks.iter().any(Hook::needs_review) {
        return Ok(Prepared {
            home: resolved,
            ..Prepared::default()
        });
    }
    let mut trusted = Vec::new();
    let (mut copies, mut left) = plan(&hooks, home, &trusted);
    for donor in homes.iter().filter(|donor| *donor != home) {
        if left.is_empty() || Instant::now() >= deadline {
            break;
        }
        if !donor.join("config.toml").exists() {
            continue;
        }
        // A home that cannot be read vouches for nothing.
        if let Ok(donor_hooks) = list_hooks(codex, donor, cwd, deadline) {
            trusted.extend(vouched_by(&donor_hooks, donor));
            (copies, left) = plan(&hooks, home, &trusted);
        }
    }
    if !copies.is_empty() {
        let value: serde_json::Map<String, serde_json::Value> = copies
            .iter()
            .map(|entry| {
                (
                    entry.key.clone(),
                    serde_json::json!({"trusted_hash": entry.hash}),
                )
            })
            .collect();
        target.call(
            "config/value/write",
            serde_json::json!({"keyPath": "hooks.state", "mergeStrategy": "upsert", "value": value}),
        )?;
    }
    Ok(Prepared {
        home: resolved,
        copied: copies.len(),
        left: left
            .iter()
            .map(|hook| hook.command.clone().unwrap_or_else(|| hook.event.clone()))
            .collect(),
    })
}

/// One home's hooks that wait on review, for `tend doctor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Audit {
    pub home: PathBuf,
    /// The home as Codex resolves it, the path its trust keys use.
    pub resolved: PathBuf,
    /// `(fixable, unvouched)`: hooks a swap would carry trust to, and ones
    /// it could not. `Err`: the home could not be checked, and why.
    pub result: Result<(usize, usize), String>,
}

/// [`Audit`] each home in `homes` that has hooks waiting on review or
/// cannot be checked, within [`BUDGET`] (homes past it are reported as not
/// checked). Read-only: nothing is written.
#[must_use]
pub fn audit(codex_for: impl Fn(&Path) -> Option<Codex>, homes: &[PathBuf]) -> Vec<Audit> {
    let neutral = match Neutral::new() {
        Ok(neutral) => neutral,
        Err(error) => {
            return homes
                .iter()
                .map(|home| Audit {
                    home: home.clone(),
                    resolved: resolved_home(home),
                    result: Err(error.clone()),
                })
                .collect()
        }
    };
    let cwd = neutral.path();
    let deadline = Instant::now() + BUDGET;
    let mut listed: Vec<(PathBuf, Result<Vec<Hook>, String>)> = Vec::new();
    for home in homes
        .iter()
        .filter(|home| home.join("config.toml").exists())
    {
        let hooks = if Instant::now() >= deadline {
            Err("not checked: the check ran out of time".to_owned())
        } else if let Some(codex) = codex_for(home) {
            list_hooks(&codex, home, cwd, deadline)
        } else {
            Err("no codex to ask for this home".to_owned())
        };
        listed.push((home.clone(), hooks));
    }
    let trusted: Vec<Trusted> = listed
        .iter()
        .filter_map(|(home, hooks)| hooks.as_ref().ok().map(|hooks| vouched_by(hooks, home)))
        .flatten()
        .collect();
    listed
        .into_iter()
        .filter_map(|(home, hooks)| {
            let result = match hooks {
                Err(error) => Err(error),
                Ok(hooks) => {
                    let (copies, left) = plan(&hooks, &home, &trusted);
                    if copies.is_empty() && left.is_empty() {
                        return None;
                    }
                    Ok((copies.len(), left.len()))
                }
            };
            Some(Audit {
                resolved: resolved_home(&home),
                home,
                result,
            })
        })
        .collect()
}

/// The `codex` an aas launch runs: aas's own shim, when there is one, else
/// the one on `PATH` or in the usual install places. Older releases hash
/// hooks differently, so the release asked matters.
#[must_use]
pub fn find_codex() -> Option<Codex> {
    let mut shims = Vec::new();
    shims.extend(dirs::data_dir().map(|data| data.join("aas/shims/codex")));
    shims.extend(dirs::home_dir().map(|home| home.join(".config/aas/shims/codex")));
    shims
        .into_iter()
        .find(|path| path.is_file())
        .map(Codex::program)
        .or_else(path_codex)
}

/// `codex` as a plain launch line's shell finds it: `PATH`, then the usual
/// install places. Not aas's shim.
#[must_use]
pub fn path_codex() -> Option<Codex> {
    crate::account_swap::find_aas("codex")
        .map(PathBuf::from)
        .map(Codex::program)
}

/// The `codex` a configured profile's launch runs: `program` itself when
/// absolute, else found on the `PATH` that launch has (the profile's own,
/// when it sets one). `None` when that `PATH` cannot be told without
/// running the profile's env file.
#[must_use]
pub fn profile_codex(profile: &tend::accounts::AccountProfile, program: &str) -> Option<Codex> {
    let program = Path::new(program);
    if program.is_absolute() {
        return program
            .is_file()
            .then(|| Codex::program(program.to_path_buf()));
    }
    match profile_var(profile, "PATH").ok()? {
        None => path_codex(),
        Some(path) => std::env::split_paths(&path)
            .map(|dir| expand_home(&dir.display().to_string()).join(program))
            .find(|candidate| candidate.is_file())
            .map(Codex::program),
    }
}

/// The `codex` a running agent `pid` runs, if that can be told: its own
/// executable when that is Codex, or its interpreter with the Codex script
/// it runs as the interpreter's first argument (`node /…/codex.js`). The
/// process must be the one that started at `start`, read before and after.
/// `None` otherwise (no start to check, a reused pid, anything else): no
/// other release is substituted.
#[must_use]
pub fn codex_of(pid: u32, start: Option<&str>) -> Option<Codex> {
    let start = start?;
    let same = || tend::agent_conversation::process_start(pid).as_deref() == Some(start);
    if !same() {
        return None;
    }
    let named_codex = |path: &Path| {
        path.is_absolute()
            && path.is_file()
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == "codex" || name == "codex.js")
    };
    let exe = executable_of(pid)?;
    let found = if named_codex(&exe) {
        Some(Codex::program(exe))
    } else {
        // An interpreter: its first argument is the script it runs.
        let script = tend::process_snapshot::process_argv(pid)?
            .get(1)
            .map(PathBuf::from)
            .filter(|path| named_codex(path))?;
        Some(Codex {
            program: exe,
            args: vec![script],
        })
    };
    found.filter(|_| same())
}

/// The executable image of `pid`.
fn executable_of(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/exe")).ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
        let pid = Pid::from_u32(pid);
        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing().with_exe(UpdateKind::Always),
        );
        system.process(pid)?.exe().map(Path::to_path_buf)
    }
}

/// `home` as Codex resolves it: the directory with its symlinks followed
/// (`…/asx/profiles/x` is `…/aas/profiles/x` once `asx` links to `aas`).
#[must_use]
pub fn resolved_home(home: &Path) -> PathBuf {
    std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf())
}

/// An empty directory to ask Codex from, so no repository's project hooks
/// are loaded with the home's own; removed on drop.
struct Neutral(PathBuf);

impl Neutral {
    fn new() -> Result<Self, String> {
        let dir = std::env::temp_dir().join(format!(
            "tend-hook-trust-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        Ok(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Neutral {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.0);
    }
}

/// How long a launch may spend on hook trust before it starts anyway.
pub const LAUNCH_BUDGET: Duration = Duration::from_secs(10);

/// Before Codex starts on `routed` (a launch, not a swap): carry trust to
/// its home the way a swap does, asking the `codex` that launch will run:
/// aas's for an aas launch, the launch line's own program on the profile's
/// `PATH` for a configured profile. All of it, `aas export` included, within
/// [`LAUNCH_BUDGET`]. `None` when there was nothing to say.
#[must_use]
pub fn prepare_launch(
    profiles: &[tend::accounts::AccountProfile],
    routed: &tend::swap_api::RoutedLaunch,
    aas: &str,
) -> Option<String> {
    let deadline = Instant::now() + LAUNCH_BUDGET;
    let left = || deadline.saturating_duration_since(Instant::now());
    let home = account_home_within(profiles, &routed.account, aas, left())?;
    let codex = if profiles.is_empty() {
        find_codex()
    } else {
        let profile = tend::accounts::find_profile(profiles, "codex", &routed.account)?;
        profile_codex(profile, routed.launcher.last()?)
    };
    let Some(codex) = codex else {
        return Some(format!(
            "could not tell which codex the launch runs, so hook trust in {} was not checked",
            resolved_home(&home).display()
        ));
    };
    let homes = known_homes(profiles);
    match prepare(&codex, &home, &homes, left()) {
        Ok(prepared) => prepared.note(),
        Err(error) => Some(format!("Codex hook trust was not checked ({error})")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &str = "sha256:270142e975acd1e3788822119c11b61b665c5db864cf75ba457d952e23e347ef";
    const OLD: &str = "sha256:51d95ede00000000000000000000000000000000000000000000000000000000";
    const TARGET: &str = "/p/codex-b_codex";

    fn hook(home: &str, event: &str, trust: &str) -> Hook {
        Hook {
            key: format!("{home}/config.toml:{event}:0:0"),
            event: event.into(),
            command: Some(format!("tend hook codex --event {event}")),
            current_hash: H.into(),
            trust: trust.into(),
            source: "user".into(),
        }
    }

    fn vouch(home: &str, event: &str, hash: &str) -> Trusted {
        Trusted {
            key: format!("{home}/config.toml:{event}:0:0"),
            event: event.into(),
            hash: hash.into(),
        }
    }

    #[test]
    fn trust_is_carried_only_from_an_identical_trusted_hook() {
        let target = Path::new(TARGET);
        let hooks = [hook(TARGET, "stop", "modified")];
        let (copies, left) = plan(&hooks, target, &[vouch("/h/.codex", "stop", H)]);
        assert_eq!(copies, [vouch(TARGET, "stop", H)]);
        assert!(left.is_empty());
        // Only an old definition was ever trusted: left for the user.
        let (copies, left) = plan(&hooks, target, &[vouch("/h/.codex", "stop", OLD)]);
        assert!(copies.is_empty());
        assert_eq!(left.len(), 1);
        // The same hash for another event does not vouch for this one.
        let (copies, _) = plan(&hooks, target, &[vouch("/h/.codex", "session_start", H)]);
        assert!(copies.is_empty());
        // A trusted hook needs nothing.
        assert_eq!(
            plan(&[hook(TARGET, "stop", "trusted")], target, &[]),
            (vec![], vec![])
        );
        // A project hook, or one from another file, is never vouched for,
        // nor counted as the home's: it belongs to its repository.
        let mut project = hook(TARGET, "stop", "untrusted");
        project.source = "project".into();
        let mut elsewhere = hook(TARGET, "stop", "untrusted");
        elsewhere.key = "/repo/.codex/hooks.json:stop:0:0".into();
        let (copies, left) = plan(&[project], target, &[vouch("/h/.codex", "stop", H)]);
        assert!(copies.is_empty() && left.is_empty());
        let (copies, left) = plan(&[elsewhere], target, &[vouch("/h/.codex", "stop", H)]);
        assert!(copies.is_empty());
        assert_eq!(left.len(), 1);
        // An installed plugin's hook waiting on review is reported, never
        // trusted for the user (the reviewer's case).
        let mut plugin = hook(TARGET, "stop", "untrusted");
        plugin.source = "plugin".into();
        plugin.key = "demo@test:hooks/hooks.json:stop:0:0".into();
        let (copies, left) = plan(&[plugin], target, &[vouch("/h/.codex", "stop", H)]);
        assert!(copies.is_empty());
        assert_eq!(left.len(), 1);
    }

    /// Only a hook Codex reports trusted, from a home's own user config,
    /// vouches. Malformed keys, other files and project or plugin hooks do
    /// not, whatever their hash (the reviewer's cases).
    #[test]
    fn only_a_homes_own_trusted_user_hook_vouches() {
        let home = Path::new("/h/.codex");
        let mut hooks = vec![hook("/h/.codex", "stop", "trusted")];
        assert_eq!(vouched_by(&hooks, home), [vouch("/h/.codex", "stop", H)]);
        for key in [
            "stop:0:0",
            "/untrusted/repository/.codex/hooks.json:stop:0:0",
            "plugin@unknown:hooks/hooks.json:stop:0:0",
            "/h/.codex/config.toml:stop:x:0",
            "/h/.codex/config.toml:stop:0:0:9",
            "/h/.codex/config.toml:Stop:0:0",
        ] {
            hooks[0].key = key.into();
            assert!(vouched_by(&hooks, home).is_empty(), "{key}");
        }
        let mut project = hook("/h/.codex", "stop", "trusted");
        project.source = "project".into();
        let untrusted = hook("/h/.codex", "stop", "untrusted");
        let mut mismatched = hook("/h/.codex", "stop", "trusted");
        mismatched.event = "session_start".into();
        assert!(vouched_by(&[project, untrusted, mismatched], home).is_empty());
    }

    /// The launch's own precedence, read without running anything: an
    /// explicit `env` wins over the file; a file is read as text, a plain
    /// assignment only.
    #[test]
    fn a_profiles_codex_home_follows_its_launch_environment() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("acct.env");
        let marker = dir.path().join("ran");
        std::fs::write(
            &file,
            format!(
                "touch {}\nexport CODEX_HOME=\"/tmp/from-env-file\"\n",
                marker.display()
            ),
        )
        .unwrap();
        let profile = tend::accounts::AccountProfile {
            name: "work".into(),
            env: std::collections::BTreeMap::from([(
                "CODEX_HOME".to_owned(),
                "/tmp/from-env".to_owned(),
            )]),
            env_file: Some(file.display().to_string()),
        };
        assert_eq!(profile_home(&profile), Some(PathBuf::from("/tmp/from-env")));
        let file_only = tend::accounts::AccountProfile {
            env: std::collections::BTreeMap::new(),
            ..profile
        };
        // A file with a command in it is not guessed at, and never run.
        assert_eq!(profile_home(&file_only), None);
        assert!(!marker.exists(), "the env file must not be run");
        std::fs::write(&file, "export CODEX_HOME=\"/tmp/from-env-file\"\n").unwrap();
        assert_eq!(
            profile_home(&file_only),
            Some(PathBuf::from("/tmp/from-env-file"))
        );
        assert_eq!(
            assigned_home("CODEX_HOME=/a\nCODEX_HOME='/b c'\n").as_deref(),
            Some("/b c")
        );
        assert_eq!(assigned_home("export CODEX_HOME=\"$HOME/x\"\n"), None);
        assert_eq!(assigned_home("CODEX_HOME=`pick`\n"), None);
        assert_eq!(assigned_home("OTHER=1\n"), None);
    }

    #[test]
    fn only_the_codex_home_line_of_aas_export_is_read() {
        let output = "export CLAUDE_CODE_OAUTH_TOKEN=\"secret\"\n\
                      export CODEX_HOME=\"/Users/u/Library/Application Support/asx/profiles/codex-e_codex\"\n";
        assert_eq!(
            export_home(output),
            Some(PathBuf::from(
                "/Users/u/Library/Application Support/asx/profiles/codex-e_codex"
            ))
        );
        assert_eq!(export_home("export CODEX_HOME=relative\n"), None);
        assert_eq!(export_home(""), None);
    }

    /// A child whose descendant keeps stdout open, and one that never
    /// exits, both end within the bound.
    #[cfg(unix)]
    #[test]
    fn a_bounded_run_never_outlives_its_deadline() {
        let started = Instant::now();
        assert_eq!(
            run_bounded(
                Command::new("sh").args(["-c", "sleep 30 & echo hi"]),
                Duration::from_secs(1)
            ),
            None
        );
        assert_eq!(
            run_bounded(Command::new("sleep").arg("30"), Duration::from_secs(1)),
            None
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(
            run_bounded(Command::new("echo").arg("hi"), Duration::from_secs(5)).as_deref(),
            Some("hi\n")
        );
    }

    /// A server that stops reading its input only times a request out.
    #[cfg(unix)]
    #[test]
    fn a_server_that_stops_reading_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("codex");
        // Answers `initialize`, then never reads again.
        std::fs::write(
            &fake,
            "#!/bin/sh\nread line\necho '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}'\nexec sleep 60\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut server = AppServer::open(
            &Codex::program(fake.clone()),
            dir.path(),
            dir.path(),
            Instant::now() + BUDGET,
        )
        .unwrap();
        let started = Instant::now();
        let big = "x".repeat(1024 * 1024);
        assert!(server
            .call("config/value/write", serde_json::json!({ "value": big }))
            .is_err());
        assert!(started.elapsed() < RPC_TIMEOUT + Duration::from_secs(5));
    }

    /// The codex an agent runs is its own executable, or its interpreter
    /// and the script that is the interpreter's first argument, even when
    /// the script is not executable. No start, a start that is not the
    /// planned one, or a process that runs no Codex, gives none.
    #[cfg(unix)]
    #[test]
    fn the_codex_an_agent_runs_is_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("codex.js");
        // Not executable and without a shebang: only `sh <script>` runs it.
        std::fs::write(&script, "sleep 30\n").unwrap();
        let mut child = Command::new("sh").arg(&script).spawn().unwrap();
        let pid = child.id();
        std::thread::sleep(Duration::from_millis(200));
        let start = tend::agent_conversation::process_start(pid);
        let found = codex_of(pid, start.as_deref());
        let other = codex_of(pid, Some("not-its-start"));
        let unplanned = codex_of(pid, None);
        let _ = child.kill();
        let _ = child.wait();
        let found = found.expect("an interpreter-run codex");
        assert_eq!(
            found
                .args
                .iter()
                .map(|path| std::fs::canonicalize(path).ok())
                .collect::<Vec<_>>(),
            [std::fs::canonicalize(&script).ok()]
        );
        assert!(
            found.program.ends_with("sh")
                || found.program.ends_with("bash")
                || found.program.ends_with("dash"),
            "{found:?}"
        );
        assert_eq!(other, None);
        assert_eq!(unplanned, None);
        let mut sleep = Command::new("sleep").arg("30").spawn().unwrap();
        let sleep_start = tend::agent_conversation::process_start(sleep.id());
        let none = codex_of(sleep.id(), sleep_start.as_deref());
        let _ = sleep.kill();
        let _ = sleep.wait();
        assert_eq!(none, None);
    }

    /// A configured profile's launch finds `codex` on its own `PATH`; a
    /// `PATH` its env file would compute cannot be told.
    #[cfg(unix)]
    #[test]
    fn a_profiles_codex_is_found_on_its_own_path() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("profile-bin");
        std::fs::create_dir(&bin).unwrap();
        std::fs::write(bin.join("codex"), "#!/bin/sh\n").unwrap();
        let mut profile = tend::accounts::AccountProfile {
            name: "work".into(),
            env: std::collections::BTreeMap::from([("PATH".to_owned(), bin.display().to_string())]),
            env_file: None,
        };
        assert_eq!(
            profile_codex(&profile, "codex"),
            Some(Codex::program(bin.join("codex")))
        );
        let file = dir.path().join("acct.env");
        std::fs::write(&file, "export PATH=\"$HOME/bin:$PATH\"\n").unwrap();
        profile.env.clear();
        profile.env_file = Some(file.display().to_string());
        assert_eq!(profile_codex(&profile, "codex"), None);
        std::fs::write(&file, format!("PATH={}\n", bin.display())).unwrap();
        assert_eq!(
            profile_codex(&profile, "codex"),
            Some(Codex::program(bin.join("codex")))
        );
    }

    #[test]
    fn hooks_list_results_are_read() {
        let result = serde_json::json!({"data": [{"cwd": "/w", "hooks": [{
            "key": "/p/b/config.toml:permission_request:0:0",
            "eventName": "permissionRequest",
            "command": "tend hook codex --event permission_request",
            "currentHash": H,
            "trustStatus": "untrusted",
            "source": "user"
        }], "warnings": [], "errors": []}]});
        let hooks = parse_hooks(&result);
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].event, "permission_request");
        assert!(hooks[0].needs_review());
        assert_eq!(snake("stop"), "stop");
    }

    #[test]
    fn the_report_says_what_was_carried_and_what_was_left() {
        assert_eq!(Prepared::default().note(), None);
        let note = Prepared {
            home: PathBuf::from("/p/aas/profiles/codex-b_codex"),
            copied: 6,
            left: vec!["my-hook".into()],
        }
        .note()
        .unwrap();
        assert!(note.contains("carried over the trust of 6"), "{note}");
        assert!(note.contains("to /p/aas/profiles/codex-b_codex"), "{note}");
        assert!(note.contains("will ask for review: my-hook"), "{note}");
    }

    /// Against the real Codex app-server (`TEND_TEST_CODEX=<codex>`): a
    /// home whose config is a symlink to a file trusted under another home
    /// is `untrusted` until [`prepare`] carries the trust over; the symlink
    /// stays a symlink. Run with `--ignored`.
    #[test]
    #[ignore = "needs a real codex binary in TEND_TEST_CODEX"]
    fn prepare_carries_trust_through_the_real_app_server() {
        let codex_path = PathBuf::from(std::env::var("TEND_TEST_CODEX").expect("TEND_TEST_CODEX"));
        let codex = Codex::program(codex_path.clone());
        let dir = tempfile::tempdir().unwrap();
        let (a, b, work) = (
            dir.path().join("a"),
            dir.path().join("b"),
            dir.path().join("w"),
        );
        for d in [&a, &b, &work] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(
            a.join("config.toml"),
            "[[hooks.Stop]]\nhooks = [{ type = \"command\", command = \"tend hook codex --event stop\" }]\n",
        )
        .unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(a.join("config.toml"), b.join("config.toml")).unwrap();
        // Trust it under `a`, the way a person answering the menu would.
        let deadline = Instant::now() + BUDGET;
        let mut server = AppServer::open(&codex, &a, &work, deadline).unwrap();
        let hook = server.hooks(&work).unwrap().remove(0);
        assert!(hook.needs_review());
        server
            .call(
                "config/value/write",
                serde_json::json!({"keyPath": "hooks.state", "mergeStrategy": "upsert",
                    "value": {hook.key.clone(): {"trusted_hash": hook.current_hash}}}),
            )
            .unwrap();
        drop(server);
        let homes = [a.clone(), b.clone()];
        // `b` is fixable only because `a`'s own hook is trusted.
        let audits = audit(|_| Some(Codex::program(codex_path.clone())), &homes);
        assert_eq!(audits.len(), 1, "{audits:?}");
        assert_eq!(audits[0].result, Ok((1, 0)));
        // `b` shares the file but not the key: carried over once, then trusted.
        let prepared = prepare(&codex, &b, &homes, BUDGET).unwrap();
        assert_eq!(prepared.copied, 1, "{prepared:?}");
        assert!(list_hooks(&codex, &b, &work, deadline)
            .unwrap()
            .iter()
            .all(|h| !h.needs_review()));
        assert!(std::fs::symlink_metadata(b.join("config.toml"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            prepare(&codex, &b, &homes, BUDGET).unwrap(),
            Prepared {
                home: resolved_home(&b),
                ..Prepared::default()
            }
        );
        assert!(audit(|_| Some(Codex::program(codex_path.clone())), &homes).is_empty());
        assert_eq!(audits[0].resolved, resolved_home(&b));
    }
}
