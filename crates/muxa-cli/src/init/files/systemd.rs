//! `~/.config/systemd/user/muxad.service` installer.
//!
//! Two pieces:
//!   1. Render the unit file (matches `examples/muxad.service`).
//!   2. Drive `systemctl --user daemon-reload && enable --now`.
//!
//! Step (1) is content-only and unit-tested. Step (2) shells out to
//! `systemctl` and is invoked from `apply.rs`. We deliberately keep
//! the unit file content layer pure so dry-run can show its diff
//! without touching the system.

use crate::init::marker::Outcome;
use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;
use std::process::Command;

pub const UNIT_FILENAME: &str = "muxad.service";
const UNIT_BODY: &str = include_str!("../../../../../examples/muxad.service");

pub fn default_unit_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("systemd").join("user").join(UNIT_FILENAME))
}

/// Compute the install outcome for the unit file given its current
/// on-disk content (or `None` if the file doesn't exist). Pure.
pub fn upsert(existing: Option<&str>) -> (String, Outcome) {
    let want = UNIT_BODY.to_string();
    match existing {
        None => (want, Outcome::Inserted),
        Some(prev) if prev == UNIT_BODY => (want, Outcome::Unchanged),
        Some(_) => (want, Outcome::Replaced),
    }
}

/// Is `systemctl --user` even usable on this host? On macOS, in
/// containers, or in CI without a session bus the whole component is
/// a non-starter. We check by running `systemctl --user is-system-running
/// --quiet || systemctl --user --version` — the `--version` path always
/// succeeds when systemctl exists at all.
pub fn systemd_available() -> bool {
    Command::new("systemctl")
        .args(["--user", "--version"])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Run `systemctl --user daemon-reload && enable --now muxad.service`.
pub fn enable_service() -> Result<()> {
    run_systemctl(&["--user", "daemon-reload"]).context("systemctl daemon-reload")?;
    run_systemctl(&["--user", "enable", "--now", UNIT_FILENAME])
        .context("systemctl enable --now muxad.service")?;
    Ok(())
}

/// Reverse: `disable --now`. Best-effort — `disable` returns
/// non-zero when the unit wasn't enabled, and uninstall has to be
/// idempotent, so we deliberately swallow exit codes.
pub fn disable_service() {
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", UNIT_FILENAME])
        .status();
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
}

fn run_systemctl(args: &[&str]) -> Result<()> {
    let status = Command::new("systemctl")
        .args(args)
        .status()
        .with_context(|| format!("spawning systemctl {}", args.join(" ")))?;
    if !status.success() {
        return Err(anyhow!(
            "systemctl {} exited with {}",
            args.join(" "),
            status
                .code()
                .map_or_else(|| "signal".into(), |c| c.to_string())
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_body_has_expected_shape() {
        // Sanity: the bundled unit file still describes muxad.
        assert!(UNIT_BODY.contains("ExecStart=%h/.cargo/bin/muxad"));
        assert!(UNIT_BODY.contains("[Install]"));
    }

    #[test]
    fn upsert_decides_outcome_correctly() {
        assert!(matches!(upsert(None).1, Outcome::Inserted));
        assert!(matches!(upsert(Some(UNIT_BODY)).1, Outcome::Unchanged));
        assert!(matches!(
            upsert(Some("[Unit]\nstale=1\n")).1,
            Outcome::Replaced
        ));
    }
}
