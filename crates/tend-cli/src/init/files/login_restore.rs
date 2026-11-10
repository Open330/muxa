//! Restore the workspace at login: `tend restore --boot`, run once per login
//! by the platform's own per-user scheduler.
//!
//! - Linux: a systemd user unit, `tend-restore.service`, enabled for
//!   `default.target`.
//! - macOS: a `LaunchAgent`, `dev.open330.tend-restore.plist`, which launchd
//!   loads at the next login.
//! - Windows: a Task Scheduler task, `tend-restore`, triggered at logon.
//!
//! `restore --boot` changes nothing when the multiplexer already has
//! sessions, so the hook firing on a login that kept its server — or twice —
//! is harmless. The multiplexer server it starts must outlive the hook: the
//! unit keeps only its main process (`KillMode=process`), the agent abandons
//! its process group, and on Windows `restore --boot` starts rmux outside the
//! task's job.
//!
//! Like `systemd.rs` and `launchd.rs`, the content layer is pure so a dry run
//! can show it; enabling and disabling shell out and run from `apply.rs`.

use crate::init::marker::Outcome;
use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;
use std::process::Command;

pub const SYSTEMD_UNIT: &str = "tend-restore.service";
pub const LAUNCHD_LABEL: &str = "dev.open330.tend-restore";
pub const WINDOWS_TASK: &str = "tend-restore";

/// The file this host's scheduler reads, or `None` where the hook is not a
/// file (Windows keeps tasks in its own store).
pub fn unit_path() -> Option<PathBuf> {
    match std::env::consts::OS {
        "linux" => dirs::data_dir().map(|d| d.join("systemd").join("user").join(SYSTEMD_UNIT)),
        "macos" => dirs::home_dir().map(|h| {
            h.join("Library/LaunchAgents")
                .join(format!("{LAUNCHD_LABEL}.plist"))
        }),
        _ => None,
    }
}

/// Where the hook's output goes on hosts without a journal.
pub fn log_path() -> PathBuf {
    if cfg!(windows) {
        std::env::temp_dir().join("tend-restore.log")
    } else {
        PathBuf::from("/tmp/tend-restore.log")
    }
}

/// The `tend` this hook runs: the binary running now, so the hook keeps
/// working under a scheduler whose `PATH` is not the shell's.
pub fn locate_tend() -> String {
    std::env::current_exe()
        .ok()
        .map(|path| tend::paths::canonicalize(&path).unwrap_or(path))
        .or_else(|| which::which("tend").ok())
        .map_or_else(|| "tend".into(), |path| path.to_string_lossy().into_owned())
}

/// The systemd unit. `PATH` is the installing shell's: a user manager's own
/// is minimal, and the multiplexer has to be found by name.
pub fn render_unit(tend: &str, path: &str) -> String {
    // `%` starts a systemd specifier.
    let tend = tend.replace('%', "%%");
    let path = path.replace('%', "%%");
    format!(
        "[Unit]\n\
         Description=Restore the tend workspace after login\n\
         Documentation=https://github.com/Open330/tend/blob/main/docs/INSTALL.md\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         Environment=\"PATH={path}\"\n\
         ExecStart=\"{tend}\" restore --boot\n\
         # The multiplexer server this starts outlives the one-shot run.\n\
         KillMode=process\n\
         RemainAfterExit=yes\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

/// The `LaunchAgent`. `AbandonProcessGroup` keeps launchd from killing the
/// multiplexer server once `tend` exits; `PATH` is the installing shell's,
/// since launchd's own does not reach Homebrew.
pub fn render_plist(tend: &str, path: &str) -> String {
    let log = log_path();
    let log = log.display();
    let tend = xml_escape(tend);
    let path = xml_escape(path);
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \x20 <key>Label</key>\n\
         \x20 <string>{LAUNCHD_LABEL}</string>\n\
         \x20 <key>ProgramArguments</key>\n\
         \x20 <array>\n\
         \x20   <string>{tend}</string>\n\
         \x20   <string>restore</string>\n\
         \x20   <string>--boot</string>\n\
         \x20 </array>\n\
         \x20 <key>EnvironmentVariables</key>\n\
         \x20 <dict>\n\
         \x20   <key>PATH</key>\n\
         \x20   <string>{path}</string>\n\
         \x20 </dict>\n\
         \x20 <key>RunAtLoad</key>\n\
         \x20 <true/>\n\
         \x20 <key>AbandonProcessGroup</key>\n\
         \x20 <true/>\n\
         \x20 <key>StandardOutPath</key>\n\
         \x20 <string>{log}</string>\n\
         \x20 <key>StandardErrorPath</key>\n\
         \x20 <string>{log}</string>\n\
         </dict>\n\
         </plist>\n"
    )
}

/// This host's unit file content, when it has one.
pub fn render() -> Option<String> {
    let tend = locate_tend();
    let path = std::env::var("PATH").unwrap_or_default();
    match std::env::consts::OS {
        "linux" => Some(render_unit(&tend, &path)),
        "macos" => Some(render_plist(&tend, &path)),
        _ => None,
    }
}

pub fn upsert(existing: Option<&str>, want: &str) -> (String, Outcome) {
    match existing {
        None => (want.to_string(), Outcome::Inserted),
        Some(prev) if prev == want => (want.to_string(), Outcome::Unchanged),
        Some(_) => (want.to_string(), Outcome::Replaced),
    }
}

/// Register the hook with the scheduler. A unit file is already in place;
/// systemd needs it enabled, launchd reads it at the next login, and Windows
/// gets its task.
pub fn enable() -> Result<()> {
    match std::env::consts::OS {
        "linux" => {
            run("systemctl", &["--user", "daemon-reload"])?;
            run("systemctl", &["--user", "enable", SYSTEMD_UNIT])
        }
        "windows" => register_windows_task(),
        _ => Ok(()),
    }
}

/// Undo [`enable`]. Best-effort, like the daemon services: uninstall has to
/// be idempotent, and "not registered" is the state it wants.
pub fn disable() {
    match std::env::consts::OS {
        "linux" => {
            let _ = run("systemctl", &["--user", "disable", SYSTEMD_UNIT]);
            let _ = run("systemctl", &["--user", "daemon-reload"]);
        }
        "macos" => {
            let target = format!("gui/{}/{LAUNCHD_LABEL}", super::super::util::uid_string());
            let _ = Command::new("launchctl")
                .args(["bootout", &target])
                .output();
        }
        "windows" => {
            let _ = powershell(UNREGISTER_SCRIPT, &[("TEND_TASK_NAME", WINDOWS_TASK)]);
        }
        _ => {}
    }
}

/// The task runs `tend` through a headless console host, so logging in does
/// not flash a console window, and through `PowerShell` for the redirect.
/// It runs at the installing shell's level: an elevated terminal attaches to
/// the rmux server of an elevated token, which is a different server from a
/// limited token's.
fn register_windows_task() -> Result<()> {
    let tend = locate_tend();
    let shell = tend::shell::powershell().to_string_lossy().into_owned();
    let quote = |word: &str| tend::shell::Dialect::PowerShell.quote(word);
    let script = format!(
        "& {} restore --boot *> {}",
        quote(&tend),
        quote(&log_path().to_string_lossy())
    );
    let arguments =
        format!("--headless \"{shell}\" -NoLogo -NoProfile -NonInteractive -Command \"{script}\"");
    let conhost = std::env::var("SystemRoot")
        .map_or_else(|_| PathBuf::from(r"C:\Windows"), PathBuf::from)
        .join("System32")
        .join("conhost.exe");
    powershell(
        REGISTER_SCRIPT,
        &[
            ("TEND_TASK_PROGRAM", conhost.to_string_lossy().as_ref()),
            ("TEND_TASK_ARGUMENTS", &arguments),
            ("TEND_TASK_NAME", WINDOWS_TASK),
        ],
    )
}

const REGISTER_SCRIPT: &str = r"
$ErrorActionPreference = 'Stop'
$user = [Security.Principal.WindowsIdentity]::GetCurrent()
$elevated = ([Security.Principal.WindowsPrincipal]$user).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
$level = if ($elevated) { 'Highest' } else { 'Limited' }
$action = New-ScheduledTaskAction -Execute $env:TEND_TASK_PROGRAM -Argument $env:TEND_TASK_ARGUMENTS
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $user.Name
$principal = New-ScheduledTaskPrincipal -UserId $user.Name -LogonType Interactive -RunLevel $level
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit (New-TimeSpan -Minutes 30) -MultipleInstances IgnoreNew
Register-ScheduledTask -TaskName $env:TEND_TASK_NAME -Description 'Restore the tend workspace after login (tend restore --boot)' -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Force | Out-Null
";

const UNREGISTER_SCRIPT: &str = r"
Unregister-ScheduledTask -TaskName $env:TEND_TASK_NAME -Confirm:$false -ErrorAction SilentlyContinue
";

fn powershell(script: &str, env: &[(&str, &str)]) -> Result<()> {
    let mut command = Command::new(tend::shell::powershell());
    command.args([
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        script,
    ]);
    for (name, value) in env {
        command.env(name, value);
    }
    let output = command.output().context("starting PowerShell")?;
    if !output.status.success() {
        return Err(anyhow!(
            "{}",
            String::from_utf8_lossy(&output.stderr).trim().to_owned()
                + String::from_utf8_lossy(&output.stdout).trim()
        ));
    }
    Ok(())
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("spawning {program} {}", args.join(" ")))?;
    if !status.success() {
        return Err(anyhow!("{program} {} exited with {status}", args.join(" ")));
    }
    Ok(())
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_restores_once_and_keeps_the_server_it_starts() {
        let unit = render_unit("/home/u/.cargo/bin/tend", "/usr/bin:/bin");
        assert!(unit.contains("ExecStart=\"/home/u/.cargo/bin/tend\" restore --boot\n"));
        assert!(unit.contains("Type=oneshot\n"));
        assert!(unit.contains("KillMode=process\n"));
        assert!(unit.contains("Environment=\"PATH=/usr/bin:/bin\"\n"));
        assert!(unit.contains("WantedBy=default.target\n"));
    }

    #[test]
    fn the_agent_abandons_its_process_group() {
        let plist = render_plist("/opt/homebrew/bin/tend", "/opt/homebrew/bin:/usr/bin");
        assert!(plist.contains("<string>dev.open330.tend-restore</string>"));
        assert!(plist.contains("<string>/opt/homebrew/bin/tend</string>\n    <string>restore</string>\n    <string>--boot</string>"));
        assert!(plist.contains("<key>AbandonProcessGroup</key>\n  <true/>"));
        assert!(plist.contains("<string>/opt/homebrew/bin:/usr/bin</string>"));
    }

    #[test]
    fn upsert_decides_outcome() {
        assert!(matches!(upsert(None, "a").1, Outcome::Inserted));
        assert!(matches!(upsert(Some("a"), "a").1, Outcome::Unchanged));
        assert!(matches!(upsert(Some("b"), "a").1, Outcome::Replaced));
    }
}
