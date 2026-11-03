//! What a relaunch needs that can be checked before the agent is stopped,
//! read-only: nothing is typed into the pane and no shell is started.
//!
//! A check passes or fails only when it reads the very thing the relaunch
//! will use: the `env_file` it sources, the shell and directory a respawn is
//! given, a program named by an absolute path (and its script interpreter).
//! A program found through the pane's own shell (`aas`, a bare `claude`) is
//! resolved there, in a context this cannot reproduce, and is recorded as
//! `unknown` rather than certified. Passing is not a promise the launch
//! works: the filesystem can still change after the last check.

use super::io::{PathFacts, SwapIo};
use serde::{Deserialize, Serialize};

/// What one relaunch depends on, built with its command line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Prerequisites {
    /// The programs the line runs, in order, as written in it.
    pub(crate) programs: Vec<(Stage, String)>,
    /// The file the line sources, and its digest when the line was built.
    pub(crate) env_file: Option<(String, Option<String>)>,
    /// The shell a respawn runs the line through, for a pane with none.
    pub(crate) respawn_shell: Option<String>,
}

/// Which part of the launch a program is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub(crate) enum Stage {
    /// `aas`, which starts the provider on an account.
    Wrapper,
    /// The `sh` that sources an account's `env_file`.
    Shell,
    /// `env`, which sets an account's variables.
    Env,
    /// The provider's own program.
    Provider,
    /// The login shell a directly-run pane is respawned through.
    RespawnShell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub(crate) enum Verdict {
    Pass,
    Fail,
    /// Not checked here: it is resolved where the line runs.
    Unknown,
}

/// One check, as journalled: its name, the path it read (never a value
/// from the environment or a file's contents), and a digest for a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Check {
    pub(crate) name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) path: Option<String>,
    pub(crate) verdict: Verdict,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) why: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) digest: Option<String>,
}

impl Check {
    fn new(name: impl Into<String>, path: Option<&str>, verdict: Verdict) -> Self {
        Self {
            name: name.into(),
            path: path.map(str::to_owned),
            verdict,
            why: None,
            digest: None,
        }
    }

    fn because(mut self, why: impl Into<String>) -> Self {
        self.why = Some(why.into());
        self
    }
}

/// Every check for one target, right before anything is done to it.
pub(super) fn run(
    io: &dyn SwapIo,
    prerequisites: &Prerequisites,
    direct: bool,
    cwd: &str,
    pane_pid: u32,
) -> Vec<Check> {
    let mut checks = Vec::new();
    // Where the line runs: a respawn's directory is the one it is given; a
    // typed line runs wherever the pane's shell is now.
    checks.push(if direct {
        directory("cwd", io, cwd)
    } else {
        match io.process_cwd(pane_pid) {
            Some(path) => directory("cwd", io, &path),
            None => Check::new("cwd", None, Verdict::Unknown)
                .because("the pane shell's directory could not be read"),
        }
    });
    if direct {
        checks.push(match &prerequisites.respawn_shell {
            Some(shell) => program(Stage::RespawnShell, io, shell),
            None => Check::new(Stage::RespawnShell.to_string(), None, Verdict::Fail)
                .because("no shell to respawn the pane through"),
        });
    }
    if let Some((file, planned)) = &prerequisites.env_file {
        checks.push(env_file(io, file, planned.as_deref()));
    }
    for (stage, word) in &prerequisites.programs {
        checks.push(program(*stage, io, word));
    }
    checks
}

/// The first check that failed, said for the report.
pub(super) fn failure(checks: &[Check]) -> Option<String> {
    checks
        .iter()
        .find(|check| check.verdict == Verdict::Fail)
        .map(|check| {
            let what = check.path.as_deref().map_or_else(
                || check.name.clone(),
                |path| format!("{} {path}", check.name),
            );
            match &check.why {
                Some(why) => format!("{what}: {why}"),
                None => what,
            }
        })
}

fn directory(name: &str, io: &dyn SwapIo, path: &str) -> Check {
    match io.path_facts(path) {
        PathFacts::Directory => Check::new(name, Some(path), Verdict::Pass),
        PathFacts::Missing => Check::new(name, Some(path), Verdict::Fail).because("it is gone"),
        PathFacts::File { .. } => {
            Check::new(name, Some(path), Verdict::Fail).because("it is not a directory")
        }
        PathFacts::Unreadable => {
            Check::new(name, Some(path), Verdict::Fail).because("it cannot be read")
        }
    }
}

fn env_file(io: &dyn SwapIo, path: &str, planned: Option<&str>) -> Check {
    match io.path_facts(path) {
        PathFacts::File { .. } => match io.file_digest(path) {
            Some(digest) => {
                let changed = planned.is_some_and(|planned| planned != digest);
                let mut check = if changed {
                    Check::new("env_file", Some(path), Verdict::Fail)
                        .because("it changed since the swap was planned")
                } else {
                    Check::new("env_file", Some(path), Verdict::Pass)
                };
                check.digest = Some(digest);
                check
            }
            None => Check::new("env_file", Some(path), Verdict::Fail).because("it cannot be read"),
        },
        PathFacts::Missing => {
            Check::new("env_file", Some(path), Verdict::Fail).because("it is gone")
        }
        PathFacts::Directory => {
            Check::new("env_file", Some(path), Verdict::Fail).because("it is a directory")
        }
        PathFacts::Unreadable => {
            Check::new("env_file", Some(path), Verdict::Fail).because("it cannot be read")
        }
    }
}

/// A program the line runs: certain only when named by an absolute path.
fn program(stage: Stage, io: &dyn SwapIo, word: &str) -> Check {
    let name = stage.to_string();
    if !word.starts_with('/') {
        return Check::new(name, Some(word), Verdict::Unknown)
            .because("found through the pane's own PATH, which is not read here");
    }
    match io.path_facts(word) {
        PathFacts::File {
            executable: true,
            interpreter,
        } => match interpreter {
            // A script runs only if its interpreter does.
            Some(interpreter) if interpreter.starts_with('/') => {
                match io.path_facts(&interpreter) {
                    PathFacts::File {
                        executable: true, ..
                    } => Check::new(name, Some(word), Verdict::Pass),
                    _ => Check::new(name, Some(word), Verdict::Fail)
                        .because(format!("its interpreter {interpreter} cannot run")),
                }
            }
            _ => Check::new(name, Some(word), Verdict::Pass),
        },
        PathFacts::File { .. } => {
            Check::new(name, Some(word), Verdict::Fail).because("it is not executable")
        }
        PathFacts::Missing => Check::new(name, Some(word), Verdict::Fail).because("it is gone"),
        PathFacts::Directory => {
            Check::new(name, Some(word), Verdict::Fail).because("it is a directory")
        }
        PathFacts::Unreadable => {
            Check::new(name, Some(word), Verdict::Fail).because("it cannot be read")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::io;
    use super::super::io::PathFacts;

    /// The live reads, on a real filesystem: a script's interpreter, the
    /// execute bit, a digest, and a deleted directory read as gone.
    #[cfg(unix)]
    #[test]
    fn the_live_reads_see_what_a_launch_would() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("codex");
        std::fs::write(&script, "#!/usr/bin/env node\nconsole.log(1)\n").unwrap();
        let path = script.to_str().unwrap();
        assert_eq!(
            io::path_facts(path),
            PathFacts::File {
                executable: false,
                interpreter: Some("/usr/bin/env".into())
            }
        );
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(
            io::path_facts(path),
            PathFacts::File {
                executable: true,
                ..
            }
        ));
        assert_eq!(
            io::path_facts(dir.path().to_str().unwrap()),
            PathFacts::Directory
        );
        assert_eq!(
            io::path_facts(dir.path().join("nothing").to_str().unwrap()),
            PathFacts::Missing
        );
        let digest = io::file_digest(path).unwrap();
        assert_eq!(digest.len(), 64);
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        assert_ne!(io::file_digest(path).unwrap(), digest);
        // A process in a removed directory: its cwd reads as that path,
        // which is gone.
        #[cfg(target_os = "linux")]
        {
            let gone = dir.path().join("gone");
            std::fs::create_dir(&gone).unwrap();
            let mut child = std::process::Command::new("sleep")
                .arg("30")
                .current_dir(&gone)
                .spawn()
                .unwrap();
            std::fs::remove_dir(&gone).unwrap();
            let cwd = io::process_cwd(child.id()).unwrap();
            let _ = child.kill();
            let _ = child.wait();
            assert_eq!(cwd, gone.to_str().unwrap());
            assert_eq!(io::path_facts(&cwd), PathFacts::Missing);
        }
    }
}
