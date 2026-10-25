//! Provider accounts muxa can start and move agents on by itself.
//!
//! An account is the environment a provider CLI needs to run as someone
//! else: Claude reads `CLAUDE_CONFIG_DIR` / `CLAUDE_CODE_OAUTH_TOKEN`, Codex
//! reads `CODEX_HOME`. `[[accounts.<provider>]]` names those environments,
//! in the order `auto` tries them:
//!
//! ```toml
//! [[accounts.claude]]
//! name = "work"
//! env = { CLAUDE_CONFIG_DIR = "~/.claude-work" }
//!
//! [[accounts.claude]]
//! name = "personal"
//! env_file = "~/.secrets/claude-personal.env"   # CLAUDE_CODE_OAUTH_TOKEN=…
//! ```
//!
//! With a provider's accounts listed, swaps and account-routed launches use
//! them and need no external tool; without, they keep using `aas`. A running
//! agent's account is read from its own environment, never from how it was
//! started, so an agent launched bare, by hand or by another tool is still
//! recognised.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// `[accounts]` in `config.toml`. Host-local: paths and secrets differ per
/// machine.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccountsConfig {
    pub claude: Vec<AccountProfile>,
    pub codex: Vec<AccountProfile>,
}

/// One account: a name and the environment that selects it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountProfile {
    pub name: String,
    /// Variables set on launch. A leading `~/` is the home directory.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// A `KEY=VALUE` file sourced on launch, for values that must not be
    /// typed into a pane or kept in shell history (tokens).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_file: Option<String>,
}

impl AccountsConfig {
    /// `provider`'s accounts, in `auto` order. Empty when none are listed.
    #[must_use]
    pub fn for_provider(&self, provider: &str) -> &[AccountProfile] {
        match provider {
            "claude" => &self.claude,
            "codex" => &self.codex,
            _ => &[],
        }
    }

    /// The account named `name` (`work`, or `work@claude` as `aas` spells
    /// it) for `provider`.
    #[must_use]
    pub fn find(&self, provider: &str, name: &str) -> Option<&AccountProfile> {
        find_profile(self.for_provider(provider), provider, name)
    }

    /// Problems a config load should reject: a blank or repeated name, an
    /// account that sets nothing (it would match every agent).
    pub fn validate(&self) -> Result<(), String> {
        for (provider, profiles) in [("claude", &self.claude), ("codex", &self.codex)] {
            let mut seen = std::collections::BTreeSet::new();
            for profile in profiles {
                let path = format!("accounts.{provider}.{}", profile.name);
                if profile.name.trim().is_empty() {
                    return Err(format!("accounts.{provider}: an account has no name"));
                }
                if !seen.insert(profile.name.as_str()) {
                    return Err(format!("{path}: listed twice"));
                }
                if profile.env.is_empty() && profile.env_file.is_none() {
                    return Err(format!("{path}: sets neither `env` nor `env_file`"));
                }
                // Sourced from the pane's shell, whose directory is not
                // muxa's: a relative path would name two different files.
                if profile
                    .env_file
                    .as_deref()
                    .is_some_and(|file| !(file.starts_with('/') || file.starts_with("~/")))
                {
                    return Err(format!("{path}: env_file must be an absolute or ~/ path"));
                }
            }
        }
        Ok(())
    }
}

/// The account named `name` (with or without `@provider`) in `profiles`.
#[must_use]
pub fn find_profile<'a>(
    profiles: &'a [AccountProfile],
    provider: &str,
    name: &str,
) -> Option<&'a AccountProfile> {
    let name = bare_name(name, provider);
    profiles.iter().find(|p| p.name == name)
}

/// `name` without a trailing `@provider`.
#[must_use]
pub fn bare_name<'a>(name: &'a str, provider: &str) -> &'a str {
    name.strip_suffix(provider)
        .and_then(|rest| rest.strip_suffix('@'))
        .unwrap_or(name)
}

impl AccountProfile {
    /// The words that start `program` on this account, ahead of its own
    /// arguments: `env K=V … program`, behind a small `sh` that sources
    /// [`Self::env_file`] when there is one, so its values stay off the
    /// command line.
    #[must_use]
    pub fn launch_prefix(&self, program: &str) -> Vec<String> {
        let mut words = Vec::new();
        if let Some(file) = &self.env_file {
            words.extend([
                "sh".to_owned(),
                "-c".to_owned(),
                r#"set -a; . "$0"; set +a; exec "$@""#.to_owned(),
                expand_home(file),
            ]);
        }
        if !self.env.is_empty() {
            words.push("env".to_owned());
            words.extend(
                self.env
                    .iter()
                    .map(|(key, value)| format!("{key}={}", expand_home(value))),
            );
        }
        words.push(program.to_owned());
        words
    }

    /// Every variable this account sets, `env_file` included. A file that
    /// cannot be read contributes nothing.
    #[must_use]
    pub fn resolved_env(&self) -> Vec<(String, String)> {
        let mut vars: Vec<(String, String)> = self
            .env
            .iter()
            .map(|(key, value)| (key.clone(), expand_home(value)))
            .collect();
        if let Some(file) = &self.env_file {
            vars.extend(source_env_file(&expand_home(file)));
        }
        vars
    }

    /// Does a process with `environ` run on this account? Every variable the
    /// account sets must be present with the same value; paths compare
    /// without a trailing slash.
    #[must_use]
    pub fn matches(&self, environ: &[(String, String)]) -> bool {
        env_matches(&self.resolved_env(), environ)
    }
}

fn env_matches(wanted: &[(String, String)], environ: &[(String, String)]) -> bool {
    !wanted.is_empty()
        && wanted.iter().all(|(key, value)| {
            environ
                .iter()
                .any(|(k, v)| k == key && same_value(v, value))
        })
}

/// A provider's accounts with their environments resolved once, so
/// identifying many agents reads each `env_file` a single time.
#[derive(Debug, Clone)]
pub struct ResolvedAccounts {
    entries: Vec<(String, Vec<(String, String)>)>,
}

impl ResolvedAccounts {
    #[must_use]
    pub fn new(profiles: &[AccountProfile]) -> Self {
        Self {
            entries: profiles
                .iter()
                .map(|profile| (profile.name.clone(), profile.resolved_env()))
                .collect(),
        }
    }

    /// The account `environ` belongs to, by name.
    #[must_use]
    pub fn identify(&self, environ: &[(String, String)]) -> Option<&str> {
        self.entries
            .iter()
            .find(|(_, wanted)| env_matches(wanted, environ))
            .map(|(name, _)| name.as_str())
    }
}

/// The account `environ` belongs to, by name.
#[must_use]
pub fn identify<'a>(
    profiles: &'a [AccountProfile],
    environ: &[(String, String)],
) -> Option<&'a str> {
    profiles
        .iter()
        .find(|profile| profile.matches(environ))
        .map(|profile| profile.name.as_str())
}

/// The values an env file gives the variables it declares, as the launch's
/// `sh` sees them (`$HOME`, quotes and comments resolved). Falls back to a
/// literal read when `sh` cannot be run.
#[must_use]
pub fn source_env_file(path: &str) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let declared = parse_env_file(&text);
    if cfg!(windows) {
        return declared;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let sourced = std::process::Command::new("sh")
        .args(["-c", r#"set -a; . "$0" >/dev/null 2>&1; env"#, path])
        .env_clear()
        .env("HOME", &home)
        .env("PATH", "/usr/bin:/bin")
        .stdin(std::process::Stdio::null())
        .output();
    let Ok(output) = sourced else {
        return declared;
    };
    if !output.status.success() {
        return declared;
    }
    let environ: Vec<(String, String)> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    declared
        .into_iter()
        .map(|(key, literal)| {
            let value = environ
                .iter()
                .find(|(k, _)| *k == key)
                .map_or(literal, |(_, v)| v.clone());
            (key, value)
        })
        .collect()
}

fn same_value(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

/// `~/…` against `$HOME`; anything else unchanged.
#[must_use]
pub fn expand_home(value: &str) -> String {
    match (value.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{}/{rest}", home.trim_end_matches('/')),
        _ => value.to_owned(),
    }
}

/// `KEY=VALUE` lines; `#` comments, blank lines, `export ` and one level of
/// quotes are allowed.
#[must_use]
pub fn parse_env_file(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let line = line.strip_prefix("export ").unwrap_or(line);
            let (key, value) = line.split_once('=')?;
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(value);
            Some((key.trim().to_owned(), value.to_owned()))
        })
        .collect()
}

/// A process's environment as `NUL`-separated `KEY=VALUE` entries.
#[cfg(target_os = "linux")]
#[must_use]
pub fn process_environ_raw(pid: u32) -> Option<Vec<u8>> {
    std::fs::read(format!("/proc/{pid}/environ")).ok()
}

/// A process's environment as `NUL`-separated `KEY=VALUE` entries. There is
/// no `/proc` here. sysinfo (macOS `KERN_PROCARGS2`) returns each entry
/// whole where the OS lets it; recent macOS hands it an empty environment,
/// so `ps eww` is the fallback, its space-joined output split back into
/// entries after the process's own argv.
#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn process_environ_raw(pid: u32) -> Option<Vec<u8>> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let sys_pid = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[sys_pid]),
        true,
        // sysinfo fills `environ` only when `cmd` is refreshed too.
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .with_environ(UpdateKind::Always),
    );
    let process = system.process(sys_pid)?;
    if !process.environ().is_empty() {
        return Some(
            process
                .environ()
                .iter()
                .flat_map(|entry| entry.as_encoded_bytes().iter().copied().chain([0]))
                .collect(),
        );
    }
    if cfg!(windows) {
        return None;
    }
    let argv: Vec<String> = process
        .cmd()
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    // Without the argv to strip, its words could pass for variables.
    if argv.is_empty() {
        return None;
    }
    let output = std::process::Command::new("ps")
        .args(["eww", "-o", "command=", "-p", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    let line = String::from_utf8_lossy(&output.stdout);
    Some(environ_from_ps_line(
        line.trim_end_matches('\n'),
        &argv.join(" "),
    ))
}

/// The environment half of a `ps eww` line, as `NUL`-separated entries.
/// `ps` prints argv, then every `KEY=VALUE`, all joined by spaces: an entry
/// starts at a word shaped like `KEY=`, and other words belong to the
/// value before them.
#[cfg(not(target_os = "linux"))]
fn environ_from_ps_line(line: &str, argv: &str) -> Vec<u8> {
    let rest = line.strip_prefix(argv).unwrap_or(line);
    let starts_entry = |word: &str| {
        word.split_once('=').is_some_and(|(key, _)| {
            !key.is_empty()
                && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !key.starts_with(|c: char| c.is_ascii_digit())
        })
    };
    let mut entries: Vec<String> = Vec::new();
    for word in rest.split(' ') {
        if starts_entry(word) {
            entries.push(word.to_owned());
        } else if let Some(last) = entries.last_mut() {
            last.push(' ');
            last.push_str(word);
        }
    }
    entries
        .into_iter()
        .flat_map(|entry| entry.into_bytes().into_iter().chain([0]))
        .collect()
}

/// `KEY=VALUE` pairs from a `NUL`-separated environment block.
#[must_use]
pub fn parse_environ(raw: &[u8]) -> Vec<(String, String)> {
    raw.split(|byte| *byte == 0)
        .filter_map(|entry| std::str::from_utf8(entry).ok())
        .filter_map(|entry| entry.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

/// A process's environment; empty when it cannot be read.
#[must_use]
pub fn process_environ(pid: u32) -> Vec<(String, String)> {
    process_environ_raw(pid)
        .map(|raw| parse_environ(&raw))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(name: &str, env: &[(&str, &str)]) -> AccountProfile {
        AccountProfile {
            name: name.into(),
            env: env
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            env_file: None,
        }
    }

    fn environ(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn an_agent_is_identified_by_its_environment_including_paths_with_spaces() {
        let dir = "/Users/june/Library/Application Support/asx/profiles/claude-work_claude";
        let profiles = vec![
            profile(
                "personal",
                &[("CLAUDE_CONFIG_DIR", "/Users/june/.claude-personal")],
            ),
            profile("work", &[("CLAUDE_CONFIG_DIR", dir)]),
        ];
        let env = environ(&[
            ("HOME", "/Users/june"),
            ("CLAUDE_CONFIG_DIR", &format!("{dir}/")),
        ]);
        assert_eq!(identify(&profiles, &env), Some("work"));
        // A bare agent on the default credentials is nobody's account.
        assert_eq!(
            identify(&profiles, &environ(&[("HOME", "/Users/june")])),
            None
        );
    }

    #[test]
    fn a_token_from_an_env_file_identifies_and_launches_without_being_typed() {
        let dir = std::env::temp_dir().join(format!("muxa-accounts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("work.env");
        std::fs::write(
            &file,
            "# work\nexport CLAUDE_CODE_OAUTH_TOKEN=\"sk-ant-oat-1\"\nCLAUDE_CONFIG_DIR=$HOME/.claude-work  # dir\n",
        )
        .unwrap();
        let account = AccountProfile {
            name: "work".into(),
            env: BTreeMap::new(),
            env_file: Some(file.display().to_string()),
        };
        // Values as the launch's `sh` sees them: `$HOME` expanded, comment gone.
        let home = std::env::var("HOME").unwrap();
        let dir_value = format!("{home}/.claude-work");
        let on = |token: &str| {
            environ(&[
                ("CLAUDE_CODE_OAUTH_TOKEN", token),
                ("CLAUDE_CONFIG_DIR", &dir_value),
            ])
        };
        assert!(account.matches(&on("sk-ant-oat-1")));
        assert!(!account.matches(&on("sk-ant-oat-2")));
        let resolved = ResolvedAccounts::new(std::slice::from_ref(&account));
        assert_eq!(resolved.identify(&on("sk-ant-oat-1")), Some("work"));
        let words = account.launch_prefix("claude");
        assert!(words.iter().all(|w| !w.contains("sk-ant")), "{words:?}");
        assert_eq!(words.first().map(String::as_str), Some("sh"));
        assert_eq!(words.last().map(String::as_str), Some("claude"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn launch_prefix_expands_home_and_sets_every_variable() {
        let home = std::env::var("HOME").unwrap();
        let account = profile("work", &[("CODEX_HOME", "~/.codex-work")]);
        assert_eq!(
            account.launch_prefix("codex"),
            vec![
                "env".to_owned(),
                format!("CODEX_HOME={home}/.codex-work"),
                "codex".to_owned()
            ]
        );
    }

    #[test]
    fn names_resolve_with_or_without_the_provider_suffix() {
        let accounts = AccountsConfig {
            claude: vec![profile("work", &[("CLAUDE_CONFIG_DIR", "/w")])],
            codex: Vec::new(),
        };
        assert!(accounts.find("claude", "work").is_some());
        assert!(accounts.find("claude", "work@claude").is_some());
        assert!(accounts.find("codex", "work").is_none());
        assert_eq!(bare_name("work@codex", "claude"), "work@codex");
    }

    #[test]
    fn validation_rejects_empty_repeated_and_matchless_accounts() {
        let ok = profile("work", &[("CLAUDE_CONFIG_DIR", "/w")]);
        let config = |claude: Vec<AccountProfile>| AccountsConfig {
            claude,
            codex: Vec::new(),
        };
        assert!(config(vec![ok.clone()]).validate().is_ok());
        assert!(config(vec![ok.clone(), ok.clone()]).validate().is_err());
        assert!(config(vec![profile(" ", &[("A", "b")])])
            .validate()
            .is_err());
        assert!(config(vec![profile("empty", &[])]).validate().is_err());
        let relative = AccountProfile {
            name: "rel".into(),
            env: BTreeMap::new(),
            env_file: Some("secrets/x.env".into()),
        };
        assert!(config(vec![relative])
            .validate()
            .unwrap_err()
            .contains("absolute"));
    }

    /// Parks while the probe test reads this process's environment. Run
    /// only as that test's child: macOS hides the environment of its own
    /// signed binaries (`/bin/sleep`), not of this test binary.
    #[test]
    #[ignore = "a child process of a_running_process_environment_is_read_whole"]
    fn environ_probe_child() {
        if std::env::var_os("MUXA_ACCOUNT_PROBE").is_some() {
            std::thread::sleep(std::time::Duration::from_secs(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_running_process_environment_is_read_whole() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "accounts::tests::environ_probe_child",
            ])
            .env("MUXA_ACCOUNT_PROBE", "/x/Application Support/y")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut found = None;
        for _ in 0..100 {
            found = process_environ(child.id())
                .into_iter()
                .find(|(k, _)| k == "MUXA_ACCOUNT_PROBE")
                .map(|(_, v)| v);
            if found.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(found.as_deref(), Some("/x/Application Support/y"));
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn a_ps_line_splits_into_entries_after_the_argv() {
        let line = "claude --resume x HOME=/Users/j CLAUDE_CONFIG_DIR=/Users/j/Library/Application Support/p TERM=xterm";
        assert_eq!(
            parse_environ(&environ_from_ps_line(line, "claude --resume x")),
            vec![
                ("HOME".into(), "/Users/j".into()),
                (
                    "CLAUDE_CONFIG_DIR".into(),
                    "/Users/j/Library/Application Support/p".into()
                ),
                ("TERM".into(), "xterm".into()),
            ]
        );
    }

    #[test]
    fn env_files_and_environ_blocks_parse() {
        assert_eq!(
            parse_env_file("A=1\n\n# c\nexport B='two words'\nbad line\n"),
            vec![("A".into(), "1".into()), ("B".into(), "two words".into())]
        );
        assert_eq!(
            parse_environ(b"A=1\0B=x=y\0\0"),
            vec![("A".into(), "1".into()), ("B".into(), "x=y".into())]
        );
    }
}
