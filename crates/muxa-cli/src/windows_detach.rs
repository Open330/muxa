//! Starting a process on Windows so that it is truly on its own.
//!
//! `CreateProcess` from here would hand the child every inheritable handle
//! this process holds — including the pipe a caller reads `muxa`'s output
//! from, so `muxa daemon start *> $null` in a `PowerShell` profile would wait
//! for the daemon to exit before the shell could start. It would also leave
//! the child in this process's job object: rmux closes a pane's job along
//! with its server, and a Task Scheduler task's job forbids the breakaway
//! rmux needs to start its own daemon. Neither is reachable without unsafe
//! Win32 calls, so the process is created by the WMI service instead
//! (`Win32_Process.Create`): it inherits no handle and belongs to no job.

use anyhow::{bail, Context, Result};
use std::process::{Command, Stdio};

/// Create the process described by `command` (program, arguments, and
/// environment changes) on its own, without waiting for it.
pub fn spawn(command: &Command) -> Result<()> {
    // `Win32_Process.Create` does not search `PATH`: a bare `rmux` fails
    // with "path not found", so the program is resolved here.
    let program = command.get_program().to_string_lossy().into_owned();
    let program = if std::path::Path::new(&program).is_absolute() {
        program
    } else {
        muxa::shell::find_program(&program)
            .with_context(|| format!("finding {program} on PATH"))?
            .to_string_lossy()
            .into_owned()
    };
    let mut words = vec![quote(&program)];
    words.extend(command.get_args().map(|arg| quote(&arg.to_string_lossy())));
    let command_line = words.join(" ");
    let mut environment: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    for (name, value) in command.get_envs() {
        let name = name.to_string_lossy().into_owned();
        match value {
            Some(value) => {
                environment.insert(name, value.to_string_lossy().into_owned());
            }
            None => {
                environment.remove(&name);
            }
        }
    }
    let environment: Vec<String> = environment
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());

    let mut powershell = Command::new(muxa::shell::powershell());
    powershell
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            SCRIPT,
        ])
        .env("MUXA_DETACH_COMMAND_LINE", &command_line)
        .env("MUXA_DETACH_CWD", &cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = powershell.spawn().context("starting PowerShell")?;
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().context("PowerShell stdin")?;
        stdin.write_all(serde_json::to_string(&environment)?.as_bytes())?;
    }
    let output = child.wait_with_output().context("waiting for PowerShell")?;
    if !output.status.success() {
        bail!(
            "Win32_Process.Create failed: {}{}",
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// The environment arrives as JSON on stdin — it can exceed what a
/// command line holds — and replaces WMI's default block entirely.
const SCRIPT: &str = r"
$ErrorActionPreference = 'Stop'
$environment = [string[]]([Console]::In.ReadToEnd() | ConvertFrom-Json)
$startup = New-CimInstance -ClassName Win32_ProcessStartup -ClientOnly -Property @{
ShowWindow = [uint16]0
EnvironmentVariables = $environment
}
$created = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{
CommandLine = $env:MUXA_DETACH_COMMAND_LINE
CurrentDirectory = $env:MUXA_DETACH_CWD
ProcessStartupInformation = $startup
}
if ($created.ReturnValue -ne 0) { Write-Output ('error ' + $created.ReturnValue); exit 1 }
";

fn quote(word: &str) -> String {
    format!("\"{word}\"")
}
