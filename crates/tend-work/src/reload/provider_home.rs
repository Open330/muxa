//! Preserve an agent's account home without persisting its environment.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tend::shell::{program_name, Dialect};

use super::PaneShape;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum Variable {
    #[serde(rename = "CLAUDE_CONFIG_DIR")]
    Claude,
    #[serde(rename = "CODEX_HOME")]
    Codex,
}

impl Variable {
    fn name(self) -> &'static str {
        match self {
            Self::Claude => "CLAUDE_CONFIG_DIR",
            Self::Codex => "CODEX_HOME",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ProviderHome {
    variable: Variable,
    value: String,
}

/// Only executable positions count: a shell or wrapper mentioning `claude`
/// in its arguments is not the provider process. Node/Bun launchers put the
/// provider script immediately after their executable.
pub(super) fn variable_for_argv(argv: &[String]) -> Option<Variable> {
    let first = program_name(argv.first()?);
    let program = if matches!(first, "node" | "bun" | "deno") {
        program_name(argv.get(1)?)
    } else {
        first
    };
    match program {
        "claude" | "claude.js" => Some(Variable::Claude),
        "codex" | "codex.js" => Some(Variable::Codex),
        // Claude's npm package launches node with its cli.js entry point.
        "cli.js"
            if argv.get(1).is_some_and(|path| {
                path.replace('\\', "/")
                    .contains("/@anthropic-ai/claude-code/")
            }) =>
        {
            Some(Variable::Claude)
        }
        _ => None,
    }
}

/// Read all selected agent environments together, then retain just the one
/// allowlisted variable for each provider. Shell environments never enter it.
pub(super) fn capture(
    panes: &mut [PaneShape],
    pane_pids: &[Option<u32>],
    find_agent: impl Fn(u32) -> Option<(u32, Variable)>,
    read: impl FnOnce(&[u32]) -> HashMap<u32, Option<Vec<u8>>>,
) {
    let agents: Vec<_> = pane_pids
        .iter()
        .map(|pid| pid.and_then(&find_agent))
        .collect();
    let pids: Vec<_> = agents.iter().flatten().map(|(pid, _)| *pid).collect();
    let environs = read(&pids);
    for (pane, agent) in panes.iter_mut().zip(agents) {
        let Some((pid, variable)) = agent else {
            continue;
        };
        pane.provider_home = environs
            .get(&pid)
            .and_then(Option::as_deref)
            .and_then(|raw| {
                raw.split(|byte| *byte == 0).find_map(|entry| {
                    let (name, value) = std::str::from_utf8(entry).ok()?.split_once('=')?;
                    (name == variable.name() && !value.is_empty()).then(|| ProviderHome {
                        variable,
                        value: value.to_owned(),
                    })
                })
            });
    }
}

/// Restore only an implicit account selection; explicit wrappers retain
/// control of the account they will select when they execute.
pub(super) fn relaunch(pane: &PaneShape, command: String) -> String {
    let Some(home) = pane.provider_home.as_ref() else {
        return command;
    };
    let argv = pane.argv.as_deref();
    let program = argv
        .and_then(|argv| argv.first())
        .map_or_else(|| tend::shell::program_token(&command), String::as_str);
    if matches!(program_name(program), "aas" | "env")
        || command.contains(&format!("{}=", home.variable.name()))
        || home.value.contains('\0')
    {
        return command;
    }
    let variable = home.variable.name();
    match Dialect::native() {
        Dialect::Posix => format!(
            "env {} {command}",
            Dialect::Posix.word(&format!("{variable}={}", home.value))
        ),
        Dialect::PowerShell => format!(
            "$env:{variable}={}; {command}",
            Dialect::PowerShell.quote(&home.value)
        ),
    }
}

#[cfg(target_os = "linux")]
pub(super) fn provider_process(pane_pid: u32) -> Option<(u32, Variable)> {
    let mut level = vec![pane_pid];
    for _ in 0..8 {
        for pid in &level {
            if let Some(variable) = std::fs::read(format!("/proc/{pid}/cmdline"))
                .ok()
                .and_then(|raw| variable_for_argv(&super::split_cmdline(&raw)))
            {
                return Some((*pid, variable));
            }
        }
        level = level
            .iter()
            .flat_map(|pid| {
                std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
                    .unwrap_or_default()
                    .split_whitespace()
                    .filter_map(|child| child.parse().ok())
                    .collect::<Vec<u32>>()
            })
            .collect();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(command: &str) -> PaneShape {
        let mut pane = super::super::tests::pane("work", "0", "0", Some(command));
        pane.argv = Some(command.split_whitespace().map(str::to_owned).collect());
        pane
    }

    fn home(variable: Variable, value: &str) -> ProviderHome {
        ProviderHome {
            variable,
            value: value.into(),
        }
    }

    #[test]
    fn capture_reads_agent_pids_in_one_batch_and_serializes_only_provider_homes() {
        let mut panes = vec![pane("claude"), pane("codex"), pane("zsh")];
        capture(
            &mut panes,
            &[Some(10), Some(20), Some(30)],
            |pid| match pid {
                10 => Some((11, Variable::Claude)),
                20 => Some((22, Variable::Codex)),
                _ => None,
            },
            |pids| {
                assert_eq!(pids, &[11, 22], "read the agents, never their shells");
                HashMap::from([
                (11, Some(b"TOKEN=secret\0CLAUDE_CONFIG_DIR=/accounts/claude\0CODEX_HOME=/wrong\0".to_vec())),
                (22, Some(b"API_KEY=secret\0CODEX_HOME=/accounts/codex\0CLAUDE_CONFIG_DIR=/wrong\0".to_vec())),
            ])
            },
        );
        assert_eq!(
            panes[0].provider_home,
            Some(home(Variable::Claude, "/accounts/claude"))
        );
        assert_eq!(
            panes[1].provider_home,
            Some(home(Variable::Codex, "/accounts/codex"))
        );
        assert_eq!(panes[2].provider_home, None);
        let json = serde_json::to_string(&panes).unwrap();
        assert!(!json.contains("secret"));
        assert!(!json.contains("TOKEN"));
        assert!(!json.contains("API_KEY"));
        assert!(!json.contains("/wrong"));
        let loaded: Vec<PaneShape> = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded, panes);
    }

    #[test]
    fn absent_empty_and_unreadable_environments_keep_old_snapshot_behavior() {
        for raw in [
            None,
            Some(Vec::new()),
            Some(b"CLAUDE_CONFIG_DIR=\0TOKEN=secret\0".to_vec()),
        ] {
            let mut panes = vec![pane("claude")];
            capture(
                &mut panes,
                &[Some(1)],
                |_| Some((2, Variable::Claude)),
                |_| HashMap::from([(2, raw)]),
            );
            assert_eq!(panes[0].provider_home, None);
            assert_eq!(
                super::super::relaunch_command(&panes[0]).as_deref(),
                Some("claude")
            );
        }
        let old = r#"{"session":"work","window_index":"0","pane_index":"0","path":"/tmp","command":"claude","replayable":true}"#;
        let loaded: PaneShape = serde_json::from_str(old).unwrap();
        assert_eq!(loaded.provider_home, None);
        assert_eq!(
            super::super::relaunch_command(&loaded).as_deref(),
            Some("claude")
        );
    }

    #[test]
    fn relaunch_keeps_home_and_resume_for_both_providers() {
        for (program, variable, kind, resume) in [
            ("claude", Variable::Claude, "claude_code", "--resume"),
            ("codex", Variable::Codex, "codex", "resume"),
        ] {
            let mut pane = pane(program);
            pane.provider_home = Some(home(variable, "/accounts/work"));
            pane.agent = Some(super::super::AgentShape {
                kind: kind.into(),
                session_id: "thread-id".into(),
                working: false,
            });
            let prefix = if cfg!(windows) {
                format!("$env:{}='/accounts/work';", variable.name())
            } else {
                format!("env {}=/accounts/work", variable.name())
            };
            assert_eq!(
                super::super::relaunch_command(&pane).unwrap(),
                if program == "codex" {
                    format!(
                        "{prefix} codex -c check_for_update_on_startup=false \
                         -c 'tui.resume_cwd=\"session\"' resume thread-id"
                    )
                } else {
                    format!("{prefix} {program} {resume} thread-id")
                }
            );
        }
    }

    #[test]
    fn explicit_account_wrappers_and_assignments_are_unchanged() {
        for command in [
            "aas e work@claude",
            "/usr/local/bin/aas e work@codex",
            "env CLAUDE_CONFIG_DIR=/explicit claude",
            "env OTHER=value claude",
            "CLAUDE_CONFIG_DIR=/explicit claude",
        ] {
            let mut pane = pane(command);
            pane.provider_home = Some(home(Variable::Claude, "/captured"));
            let baseline = super::super::relaunch_command(&{
                let mut old = pane.clone();
                old.provider_home = None;
                old
            })
            .unwrap();
            assert_eq!(super::super::relaunch_command(&pane).unwrap(), baseline);
            // Joined command fallback in snapshots without argv.
            pane.argv = None;
            assert_eq!(super::super::relaunch_command(&pane).unwrap(), command);
        }
    }

    #[test]
    fn executable_positions_select_the_provider_not_shell_or_wrapper_arguments() {
        for (command, expected) in [
            (vec!["/bin/claude"], Some(Variable::Claude)),
            (vec!["C:\\bin\\codex.exe"], Some(Variable::Codex)),
            (
                vec!["node", "/npm/@anthropic-ai/claude-code/cli.js"],
                Some(Variable::Claude),
            ),
            (
                vec!["node", "/npm/@openai/codex/bin/codex.js"],
                Some(Variable::Codex),
            ),
            (vec!["bash", "-c", "claude"], None),
            (vec!["aas", "e", "work@claude"], None),
            (vec!["vim", "claude"], None),
        ] {
            assert_eq!(
                variable_for_argv(
                    &command
                        .iter()
                        .map(|word| (*word).to_owned())
                        .collect::<Vec<_>>()
                ),
                expected
            );
        }
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "subprocess fixture for provider environment capture"]
    fn agent_probe_child() {
        std::thread::sleep(std::time::Duration::from_secs(30));
    }

    #[cfg(unix)]
    #[test]
    fn a_direct_agent_process_supplies_its_environment() {
        use std::os::unix::process::CommandExt;
        let value = "/tmp/tend-test-account with spaces";
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg0("claude")
            .args([
                "--ignored",
                "--exact",
                "reload::provider_home::tests::agent_probe_child",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .env("CLAUDE_CONFIG_DIR", value)
            .env("TEND_TEST_SECRET", "must not persist")
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let selected = loop {
            #[cfg(target_os = "linux")]
            let found = provider_process(child.id());
            #[cfg(not(target_os = "linux"))]
            let found = super::super::process::ProcessTable::read().provider_process(child.id());
            if found.is_some() || std::time::Instant::now() >= deadline {
                break found;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let mut panes = vec![pane("claude")];
        capture(
            &mut panes,
            &[Some(child.id())],
            |_| selected,
            tend::accounts::processes_environ_raw,
        );
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(selected, Some((child.id(), Variable::Claude)));
        assert_eq!(panes[0].provider_home, Some(home(Variable::Claude, value)));
        assert!(!serde_json::to_string(&panes)
            .unwrap()
            .contains("must not persist"));
    }

    #[cfg(unix)]
    #[test]
    fn a_shells_home_does_not_replace_its_agent_childs_home() {
        let mut shell = std::process::Command::new("bash")
            .args([
                "-c",
                "CLAUDE_CONFIG_DIR='/tmp/child account' bash -c 'exec -a claude \"$@\"' _ \"$@\" & wait",
                "_",
            ])
            .arg(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "reload::provider_home::tests::agent_probe_child"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .env("CLAUDE_CONFIG_DIR", "/tmp/shell account")
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let selected = loop {
            #[cfg(target_os = "linux")]
            let found = provider_process(shell.id());
            #[cfg(not(target_os = "linux"))]
            let found = super::super::process::ProcessTable::read().provider_process(shell.id());
            if found.is_some() || std::time::Instant::now() >= deadline {
                break found;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let mut panes = vec![pane("claude")];
        capture(
            &mut panes,
            &[Some(shell.id())],
            |_| selected,
            tend::accounts::processes_environ_raw,
        );
        if let Some((pid, _)) = selected {
            let _ = std::process::Command::new("kill")
                .arg(pid.to_string())
                .status();
        }
        let _ = shell.kill();
        let _ = shell.wait();
        assert!(selected.is_some_and(|(pid, _)| pid != shell.id()));
        assert_eq!(
            panes[0].provider_home,
            Some(home(Variable::Claude, "/tmp/child account"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn quoted_home_is_one_literal_environment_assignment() {
        let value = "/Users/test/Library/Application Support/O'Brien/$(exit 9)`exit 8`";
        let mut pane = pane("claude");
        pane.provider_home = Some(home(Variable::Claude, value));
        let command = relaunch(&pane, "sh -c 'printf %s \"$CLAUDE_CONFIG_DIR\"'".into());
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap(), value);
    }
}
