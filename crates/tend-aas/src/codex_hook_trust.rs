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
//! Instead, before the relaunch, a hook the user already trusted under
//! another home on this machine — same event, a `trusted_hash` equal to the
//! hook's current hash — gets that same entry under the target's key.
//! Nothing is trusted that the user has not trusted somewhere.
//!
//! Everything goes through Codex's own app-server (`hooks/list` to read
//! each hook's key, current hash and trust status; `config/value/write` to
//! upsert `hooks.state`), so Codex writes its own file, through the
//! symlink, in its own format.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// How long one app-server conversation may take.
const APP_SERVER_TIMEOUT: Duration = Duration::from_secs(15);

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

/// One `trusted_hash` the user gave under some key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trusted {
    pub key: String,
    pub hash: String,
}

/// The event segment of a hooks.state key (`…:stop:0:0` → `stop`).
fn key_event(key: &str) -> Option<&str> {
    let mut parts = key.rsplitn(4, ':');
    let _hook = parts.next()?;
    let _group = parts.next()?;
    parts.next()
}

/// The trust entries to add under `hooks`' own keys: for each hook waiting
/// on review, one the user already trusted under another key — same event,
/// same hash as the hook is now. Hooks with no such entry are returned
/// apart; they are left for the user.
#[must_use]
pub fn plan(hooks: &[Hook], trusted: &[Trusted]) -> (Vec<Trusted>, Vec<Hook>) {
    let mut copies = Vec::new();
    let mut left = Vec::new();
    for hook in hooks.iter().filter(|hook| hook.needs_review()) {
        let vouched = hook.source == "user"
            && trusted.iter().any(|entry| {
                entry.key != hook.key
                    && entry.hash == hook.current_hash
                    && key_event(&entry.key) == Some(hook.event.as_str())
            });
        if vouched {
            copies.push(Trusted {
                key: hook.key.clone(),
                hash: hook.current_hash.clone(),
            });
        } else {
            left.push(hook.clone());
        }
    }
    (copies, left)
}

/// Every `hooks.state` entry with a `trusted_hash` in `config` (a TOML
/// document). Malformed documents give none.
#[must_use]
pub fn trusted_in(config: &str) -> Vec<Trusted> {
    let Ok(value) = config.parse::<toml::Table>() else {
        return Vec::new();
    };
    let Some(state) = value
        .get("hooks")
        .and_then(|hooks| hooks.get("state"))
        .and_then(toml::Value::as_table)
    else {
        return Vec::new();
    };
    state
        .iter()
        .filter_map(|(key, entry)| {
            let hash = entry.get("trusted_hash")?.as_str()?;
            Some(Trusted {
                key: key.clone(),
                hash: hash.to_owned(),
            })
        })
        .collect()
}

/// The Codex homes on this machine whose trust can vouch for a hook:
/// `~/.codex`, `$CODEX_HOME`, the configured accounts' `CODEX_HOME`, and
/// the aas profiles (`asx/profiles/codex-*` under the data directory and
/// `~/.config`). Each listed once.
#[must_use]
pub fn known_homes(codex_profiles: &[tend::accounts::AccountProfile]) -> Vec<PathBuf> {
    let mut homes = Vec::new();
    let home = dirs::home_dir();
    homes.extend(home.as_ref().map(|home| home.join(".codex")));
    homes.extend(std::env::var_os("CODEX_HOME").map(PathBuf::from));
    for profile in codex_profiles {
        if let Some(path) = profile.env.get("CODEX_HOME") {
            homes.push(expand_home(path));
        }
    }
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

fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}

/// Every trust entry in the config files of `homes`, each file read once.
#[must_use]
pub fn trusted_across(homes: &[PathBuf]) -> Vec<Trusted> {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut trusted = Vec::new();
    for home in homes {
        let config = home.join("config.toml");
        let Ok(real) = std::fs::canonicalize(&config) else {
            continue;
        };
        if files.contains(&real) {
            continue;
        }
        files.push(real);
        if let Ok(text) = std::fs::read_to_string(&config) {
            trusted.extend(trusted_in(&text));
        }
    }
    trusted.sort_by(|a, b| a.key.cmp(&b.key).then_with(|| a.hash.cmp(&b.hash)));
    trusted.dedup();
    trusted
}

/// The `CODEX_HOME` an account's relaunch runs under: the configured
/// profile's own, or what `aas export <account>` sets. `None` when it
/// cannot be told.
#[must_use]
pub fn account_home(
    profiles: &[tend::accounts::AccountProfile],
    account: &str,
    aas: &str,
) -> Option<PathBuf> {
    if !profiles.is_empty() {
        return tend::accounts::find_profile(profiles, "codex", account)
            .and_then(|profile| profile.env.get("CODEX_HOME"))
            .map(|path| expand_home(path));
    }
    let program = crate::account_swap::find_aas(aas)?;
    let name = if account.contains('@') {
        account.to_owned()
    } else {
        format!("{account}@codex")
    };
    let output = run_bounded(Command::new(program).args(["export", &name]))?;
    // Only the one line is read; anything else it prints stays unread.
    output
        .lines()
        .find_map(|line| line.strip_prefix("export CODEX_HOME="))
        .map(|value| PathBuf::from(value.trim().trim_matches('"').trim_matches('\'')))
        .filter(|path| path.is_absolute())
}

/// `command`'s standard output, if it succeeded within the timeout.
fn run_bounded(command: &mut Command) -> Option<String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut stdout, &mut text).map(|_| text)
    });
    let deadline = std::time::Instant::now() + APP_SERVER_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let text = reader.join().ok()?.ok()?;
    status.success().then_some(text)
}

/// A short conversation with `codex app-server` over stdio under `home`.
struct AppServer {
    child: Child,
    lines: std::sync::mpsc::Receiver<String>,
    next: u64,
}

impl AppServer {
    fn open(codex: &Path, home: &Path) -> Result<Self, String> {
        let mut child = Command::new(codex)
            .arg("app-server")
            .env("CODEX_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("cannot start {} app-server: {error}", codex.display()))?;
        let stdout = child.stdout.take().ok_or("no app-server output")?;
        let (send, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        let mut server = Self {
            child,
            lines,
            next: 0,
        };
        server.call(
            "initialize",
            serde_json::json!({"clientInfo": {"name": "tend", "version": env!("CARGO_PKG_VERSION")}}),
        )?;
        server.notify("initialized")?;
        Ok(server)
    }

    fn write(&mut self, message: &serde_json::Value) -> Result<(), String> {
        let stdin = self.child.stdin.as_mut().ok_or("app-server input closed")?;
        writeln!(stdin, "{message}").map_err(|error| format!("app-server: {error}"))?;
        stdin
            .flush()
            .map_err(|error| format!("app-server: {error}"))
    }

    fn notify(&mut self, method: &str) -> Result<(), String> {
        self.write(&serde_json::json!({"jsonrpc": "2.0", "method": method}))
    }

    fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        self.next += 1;
        let id = self.next;
        self.write(
            &serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        )?;
        let deadline = std::time::Instant::now() + APP_SERVER_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let line = self
                .lines
                .recv_timeout(left)
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
}

impl Drop for AppServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The hooks Codex loads under `home` for a session in `cwd`.
fn list_hooks(server: &mut AppServer, cwd: &str) -> Result<Vec<Hook>, String> {
    let result = server.call("hooks/list", serde_json::json!({"cwds": [cwd]}))?;
    Ok(parse_hooks(&result))
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
                "carried over the trust of {} Codex hook(s) already trusted under another account",
                self.copied
            ));
        }
        if !self.left.is_empty() {
            parts.push(format!(
                "{} Codex hook(s) were never trusted on this machine and will ask for review: {}",
                self.left.len(),
                self.left.join(", ")
            ));
        }
        (!parts.is_empty()).then(|| parts.join("; "))
    }
}

/// Before Codex is relaunched under `home` in `cwd`: copy the trust of
/// hooks the user already trusted under `homes` to `home`'s own keys, so
/// the relaunch does not stop on the review menu. Errors leave the file
/// as it was (the relaunch then shows the menu, and says so).
pub fn prepare(
    codex: &Path,
    home: &Path,
    cwd: &str,
    homes: &[PathBuf],
) -> Result<Prepared, String> {
    let mut server = AppServer::open(codex, home)?;
    let hooks = list_hooks(&mut server, cwd)?;
    if !hooks.iter().any(Hook::needs_review) {
        return Ok(Prepared::default());
    }
    let (copies, left) = plan(&hooks, &trusted_across(homes));
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
        server.call(
            "config/value/write",
            serde_json::json!({"keyPath": "hooks.state", "mergeStrategy": "upsert", "value": value}),
        )?;
    }
    Ok(Prepared {
        copied: copies.len(),
        left: left
            .iter()
            .map(|hook| hook.command.clone().unwrap_or_else(|| hook.event.clone()))
            .collect(),
    })
}

/// One home's hooks that wait on review, for `tend doctor`: the ones a
/// swap would carry trust to (`fixable`), and the ones it could not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Audit {
    pub home: PathBuf,
    pub fixable: usize,
    pub unvouched: usize,
}

/// [`Audit`] every home in `homes` that has hooks waiting on review.
/// Read-only: nothing is written.
pub fn audit(codex: &Path, homes: &[PathBuf], cwd: &str) -> Result<Vec<Audit>, String> {
    let trusted = trusted_across(homes);
    let mut out = Vec::new();
    for home in homes
        .iter()
        .filter(|home| home.join("config.toml").exists())
    {
        let mut server = AppServer::open(codex, home)?;
        let hooks = list_hooks(&mut server, cwd)?;
        let (copies, left) = plan(&hooks, &trusted);
        if !copies.is_empty() || !left.is_empty() {
            out.push(Audit {
                home: home.clone(),
                fixable: copies.len(),
                unvouched: left.len(),
            });
        }
    }
    Ok(out)
}

/// The `codex` program to ask: on `PATH` or in the usual install places.
#[must_use]
pub fn find_codex() -> Option<PathBuf> {
    crate::account_swap::find_aas("codex").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &str = "sha256:270142e975acd1e3788822119c11b61b665c5db864cf75ba457d952e23e347ef";
    const OLD: &str = "sha256:51d95ede00000000000000000000000000000000000000000000000000000000";

    fn hook(key: &str, event: &str, trust: &str) -> Hook {
        Hook {
            key: key.into(),
            event: event.into(),
            command: Some(format!("tend hook codex --event {event}")),
            current_hash: H.into(),
            trust: trust.into(),
            source: "user".into(),
        }
    }

    fn trusted(key: &str, hash: &str) -> Trusted {
        Trusted {
            key: key.into(),
            hash: hash.into(),
        }
    }

    /// Against the real Codex app-server (`TEND_TEST_CODEX=<codex>`): a
    /// home whose config is a symlink to a file trusted under another home
    /// is `untrusted` until [`prepare`] carries the trust over; the symlink
    /// stays a symlink. Run with `--ignored`.
    #[test]
    #[ignore = "needs a real codex binary in TEND_TEST_CODEX"]
    fn prepare_carries_trust_through_the_real_app_server() {
        let codex = PathBuf::from(std::env::var("TEND_TEST_CODEX").expect("TEND_TEST_CODEX"));
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
        let cwd = work.to_str().unwrap();
        // Trust it under `a`, the way a person answering the menu would.
        let mut server = AppServer::open(&codex, &a).unwrap();
        let hook = list_hooks(&mut server, cwd).unwrap().remove(0);
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
        // `b` shares the file but not the key: carried over once, then trusted.
        let prepared = prepare(&codex, &b, cwd, &homes).unwrap();
        assert_eq!(prepared.copied, 1, "{prepared:?}");
        let mut server = AppServer::open(&codex, &b).unwrap();
        assert!(list_hooks(&mut server, cwd)
            .unwrap()
            .iter()
            .all(|h| !h.needs_review()));
        drop(server);
        assert!(std::fs::symlink_metadata(b.join("config.toml"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            prepare(&codex, &b, cwd, &homes).unwrap(),
            Prepared::default()
        );
    }

    #[test]
    fn trust_is_carried_only_from_an_identical_trusted_hook() {
        let target = "/p/codex-b_codex/config.toml:stop:0:0";
        let hooks = [hook(target, "stop", "modified")];
        // Trusted under ~/.codex with the current hash: carried over.
        let (copies, left) = plan(&hooks, &[trusted("/h/.codex/config.toml:stop:0:0", H)]);
        assert_eq!(copies, [trusted(target, H)]);
        assert!(left.is_empty());
        // Only an old definition was ever trusted: left for the user.
        let (copies, left) = plan(&hooks, &[trusted("/h/.codex/config.toml:stop:0:0", OLD)]);
        assert!(copies.is_empty());
        assert_eq!(left.len(), 1);
        // The same hash for another event does not vouch for this one.
        let (copies, _) = plan(
            &hooks,
            &[trusted("/h/.codex/config.toml:session_start:0:0", H)],
        );
        assert!(copies.is_empty());
        // Its own stale entry is not a vouch either.
        let (copies, _) = plan(&hooks, &[trusted(target, OLD)]);
        assert!(copies.is_empty());
        // A trusted hook needs nothing; a project hook is never vouched for.
        assert_eq!(
            plan(&[hook(target, "stop", "trusted")], &[]),
            (vec![], vec![])
        );
        let mut project = hook(target, "stop", "untrusted");
        project.source = "project".into();
        let (copies, left) = plan(&[project], &[trusted("/h/.codex/config.toml:stop:0:0", H)]);
        assert!(copies.is_empty());
        assert_eq!(left.len(), 1);
    }

    #[test]
    fn a_keys_event_is_its_third_segment_from_the_end() {
        assert_eq!(
            key_event("/a b/c:d/config.toml:permission_request:0:0"),
            Some("permission_request")
        );
        assert_eq!(key_event("stop:0"), None);
        assert_eq!(snake("permissionRequest"), "permission_request");
        assert_eq!(snake("stop"), "stop");
    }

    #[test]
    fn trust_entries_are_read_from_hooks_state() {
        let config = format!(
            r#"
model = "x"
[[hooks.Stop]]
hooks = [{{ type = "command", command = "tend hook codex --event stop" }}]

[hooks.state."/h/.codex/config.toml:stop:0:0"]
trusted_hash = "{H}"

[hooks.state."/p/x/config.toml:stop:0:0"]
enabled = false
"#
        );
        assert_eq!(
            trusted_in(&config),
            [trusted("/h/.codex/config.toml:stop:0:0", H)]
        );
        assert!(trusted_in("not = [toml").is_empty());
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
    }

    #[test]
    fn the_report_says_what_was_carried_and_what_was_left() {
        assert_eq!(Prepared::default().note(), None);
        let note = Prepared {
            copied: 6,
            left: vec!["my-hook".into()],
        }
        .note()
        .unwrap();
        assert!(note.contains("carried over the trust of 6"), "{note}");
        assert!(note.contains("will ask for review: my-hook"), "{note}");
    }
}
