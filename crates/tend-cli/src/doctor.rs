//! `tend doctor` — end-to-end diagnostic.
//!
//! Answers the question "is tend working correctly?" without making the
//! user know any paths or service-manager incantations. Each check
//! yields a `✔ / ✗ / ⚠` result with an actionable hint when it fails;
//! `doctor_report` groups them by section and provider, folding whatever
//! passes so the issues stand out (`-v` lists everything).
//!
//! Why this lives in its own module instead of reusing
//! `init::verify`: verify is post-apply (one shot, narrow scope),
//! whereas doctor is invocable any time and covers the whole install
//! surface — service manager, hooks, tmux, recent errors. Sharing the
//! IPC ping is the only meaningful overlap and that's a single
//! `Client::hello()` call.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use tend::ipc::Client;
use tend::tmux::layout::{self, ClientSurface};
use tokio::time::timeout;

use crate::doctor_report::{tilde, Level, Report};

use crate::init::files::{antigravity, claude, codex, gemini, launchd, systemd as systemd_files};

/// Marker block ids the `tend init` tmux installer writes — must match
/// the constants in `init::components::Component::id`. Duplicated as
/// string literals to keep `doctor` from depending on the components
/// module's enum surface (which is large and unrelated).
const TMUX_BLOCK_POPUP: &str = "tmux-popup";
const TMUX_BLOCK_STATUSLINE: &str = "tmux-statusline";

/// macOS launchd label used by the `TenddLaunchd` install component.
const LAUNCHD_LABEL: &str = "dev.open330.tendd";

/// systemd unit name installed by the `TenddSystemd` component.
const SYSTEMD_UNIT: &str = "tendd";

/// Run the diagnostic and print results to stdout. Returns `Ok(())`
/// regardless of how many checks fail — `tend doctor` is informational,
/// not a gate. The exit code stays 0 so users can run it from scripts
/// without conditionals.
///
/// `socket` is the resolved daemon socket path — typically what `main.rs`
/// computes from `--socket` / `TEND_SOCKET` / `cfg.socket` / the default.
/// Threading it in (rather than re-deriving from defaults here) keeps
/// `tend doctor` honest about which socket tend would actually talk to.
pub async fn run(socket: PathBuf, config: Option<&Path>, verbose: bool) -> Result<()> {
    let mut report = Report::default();

    // tendd: reachable, every live agent correlated, how it is kept running.
    let (daemon, responded) = probe_tendd(&socket).await;
    let reachable = !matches!(&daemon, CheckResult::Fail(_));
    add(&mut report, "tendd", None, daemon);
    // Skipped silently when tendd is unreachable: `check_tendd` already
    // owns and reported that condition.
    if reachable {
        for result in check_collaboration(&socket).await {
            add(&mut report, "tendd", None, result);
        }
    }
    // Its own check, on any daemon that answered: a failed verdict on
    // something else (a corrupt mailbox) never hides why swaps are down.
    if responded {
        add(&mut report, "tendd", None, check_swap(&socket).await);
    }
    add(&mut report, "tendd", None, check_service_manager());
    add(&mut report, "tendd", None, check_unit_file());
    // Recent tendd errors. These are warnings, not failures: a stale ERROR
    // line from yesterday shouldn't make the whole run look red.
    // tendd's stderr log: where the launchd plist and the systemd unit
    // redirect it, or the combined log a Windows start writes. Reading is
    // best-effort — a missing file just means nothing was logged to disk.
    let warnings = recent_tendd_errors(&crate::logs::default_err(), 20, 3);
    if warnings.is_empty() {
        add(
            &mut report,
            "tendd",
            None,
            CheckResult::Ok("recent log clean".into()),
        );
    }
    for line in &warnings {
        add(
            &mut report,
            "tendd",
            None,
            CheckResult::Warn(format_warning_line(line)),
        );
    }

    // One hook per agent CLI, so the provider is the group.
    for (label, result) in check_agent_hooks() {
        let provider = label.trim_start_matches("Hooks · ").to_string();
        add(&mut report, "Hooks", Some(&provider), result);
    }

    // Installed instructions, MCP entries and skills, one file per provider.
    match crate::init::integration::diagnostics(config) {
        Ok(checks) => {
            for (healthy, message) in checks {
                let (group, message) = match message.split_once(": ") {
                    Some((component, rest)) => (
                        Some(component.trim_start_matches("agent-").to_string()),
                        rest.to_string(),
                    ),
                    None => (None, message),
                };
                let result = if healthy {
                    CheckResult::Ok(message)
                } else {
                    CheckResult::Warn(message)
                };
                add(&mut report, "Agent integration", group.as_deref(), result);
            }
        }
        Err(error) => add(
            &mut report,
            "Agent integration",
            None,
            CheckResult::Warn(error.to_string()),
        ),
    }
    // Agent tool servers (`tend mcp`) still on a pre-upgrade binary: their
    // tools keep old behavior until the host refreshes them.
    add(
        &mut report,
        "Agent integration",
        None,
        check_stale_agent_servers(),
    );

    // What the tmux server and client this run talks to can actually do. Read
    // once and threaded into the checks below, because a config-isolated or
    // control-mode server changes what their remedies should be: "run `tend
    // init`" is wrong advice for a server that will never read `~/.tmux.conf`.
    let tmux_env = TmuxEnvironment::probe();
    for result in [
        tmux_env.describe(),
        check_tmux_blocks(),
        // A duplicate `bind-key` later in the file, or a server that never
        // re-read it, leaves the block present and the behaviour stale.
        check_tmux_watch_binding(&tmux_env),
        // The variable that lets every pane agree on the daemon socket.
        check_tend_socket_env(&socket, &tmux_env),
        // Reported, never acted on: the mark legitimately outranks the name.
        check_workspace_marks(),
    ] {
        add(
            &mut report,
            "tmux",
            None,
            result.map(|m| m.trim_start_matches("tmux: ").to_string()),
        );
    }

    add(
        &mut report,
        "Config",
        None,
        check_config_file().map(|m| m.trim_start_matches("config: ").to_string()),
    );

    println!("┌  tend doctor");
    print!("{}", report.render(verbose, &paint));
    // Point at the per-check guidance printed above rather than asserting a
    // blanket `tend init`/`upgrade` remedy — several warnings (e.g. "restart
    // the agents to enable hook events") are not fixed by either command.
    let summary = match report.issues() {
        0 => "All clean ✓".to_string(),
        1 => "1 issue — see the ⚠/✗ line above for its specific fix".to_string(),
        issues => format!("{issues} issues — see the ⚠/✗ lines above for their specific fixes"),
    };
    println!("└  {summary}");
    Ok(())
}

fn add(report: &mut Report, section: &'static str, group: Option<&str>, result: CheckResult) {
    let (level, message) = match result {
        CheckResult::Ok(m) => (Level::Ok, m),
        CheckResult::Warn(m) => (Level::Warn, m),
        CheckResult::Fail(m) => (Level::Fail, m),
    };
    report.push(section, group, level, tilde(&message));
}

fn paint(level: Level, text: &str) -> String {
    if !crate::use_colors() {
        return text.to_string();
    }
    let styled = match level {
        Level::Ok => console::style(text).green(),
        Level::Warn => console::style(text).yellow(),
        Level::Fail => console::style(text).red(),
    };
    styled.force_styling(true).to_string()
}

/// Workspace identity lives in a tmux session option, and the session name is
/// the fallback tend uses when nothing claims the workspace. When both exist
/// and point different ways, a launch goes somewhere the operator did not
/// name — visible in the result line, but only after the window has opened.
/// Saying so here is the cheap half: no refusal, no change to where anything
/// lands, just the one line that turns "why is my agent over there" into a
/// fact somebody can act on.
fn check_stale_agent_servers() -> CheckResult {
    let Some(binary) = crate::stale_agents::installed_tend() else {
        return CheckResult::Ok("agent tool servers — no installed tend found to compare".into());
    };
    match crate::stale_agents::scan(&binary).guidance() {
        None => CheckResult::Ok("agent tool servers (`tend mcp`) run the installed tend".into()),
        Some(guidance) => CheckResult::Warn(guidance),
    }
}

fn check_workspace_marks() -> CheckResult {
    match crate::tmux_work::workspace_mark_anomalies() {
        // No server, or a server with no marks, is the ordinary case.
        Ok(anomalies) if anomalies.is_empty() => {
            CheckResult::Ok("Workspace marks agree with session names".to_string())
        }
        Ok(anomalies) => CheckResult::Warn(format!(
            "{}. Rebind with `tend work start --workspace <id>` from the intended \
             session, or clear the stale mark with `tmux set-option -u -t <session> \
             @tend_workspace_id`",
            anomalies
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        )),
        Err(error) => CheckResult::Warn(format!("Could not read workspace marks: {error}")),
    }
}

/// Internal status — every check folds into one of these so the
/// `run()` body stays uniform.
enum CheckResult {
    Ok(String),
    Warn(String),
    Fail(String),
}

impl CheckResult {
    fn map(self, f: impl FnOnce(String) -> String) -> Self {
        match self {
            Self::Ok(m) => Self::Ok(f(m)),
            Self::Warn(m) => Self::Warn(f(m)),
            Self::Fail(m) => Self::Fail(f(m)),
        }
    }
}

/// IPC ping with a 1.5 s budget — same window `init::verify` uses.
/// Cold-started tendd answers a snapshot in <50 ms; anything past 1.5 s
/// is a real problem worth surfacing.
#[cfg(test)]
async fn check_tendd(socket: &Path) -> CheckResult {
    probe_tendd(socket).await.0
}

/// The daemon's verdict, and whether it answered at all (a failed verdict
/// can come from a daemon that did).
async fn probe_tendd(socket: &Path) -> (CheckResult, bool) {
    let client = Client::new(socket.to_path_buf());
    let verdict = match timeout(
        Duration::from_millis(1500),
        client.hello(Duration::from_millis(1500)),
    )
    .await
    {
        Ok(Ok(hello)) => return (tendd_hello_verdict(&hello, socket), true),
        // The inner error already carries the failure mode — including
        // socket path, connection refused vs. RPC mismatch, etc. The
        // previous wording ("reachable but errored") was misleading when
        // the inner error was actually "daemon not reachable", which is
        // the most common cause. Surface the underlying message and add
        // a single concrete next step.
        // A protocol mismatch already carries its own remedy from the IPC
        // layer, and it is not the one `tend init` performs — appending the
        // generic repair line would offer two fixes for a problem with one.
        Ok(Err(e)) => {
            let message = e.to_string();
            if message.starts_with("protocol mismatch") {
                CheckResult::Fail(message)
            } else {
                CheckResult::Fail(format!("{message} — run `tend init` to repair"))
            }
        }
        Err(_) => CheckResult::Fail(format!(
            "tendd not responding within 1.5 s at {} — start `tendd` or run `tend init`",
            socket.display()
        )),
    };
    (verdict, false)
}

fn tendd_hello_verdict(hello: &tend::ipc::Hello, socket: &Path) -> CheckResult {
    if let Some(error) = &hello.collaboration_error {
        return CheckResult::Fail(format!("tendd responding; collaboration unavailable: {error} — repair the mailbox and restart tendd"));
    }
    if let Some(skew) = hello.version_skew() {
        return CheckResult::Warn(skew.to_string());
    }
    if !hello
        .capabilities
        .iter()
        .any(|cap| cap == tend::collaboration::WORKER_SPAWN_CAPABILITY)
    {
        return CheckResult::Warn("tendd lacks collaboration_worker_spawn_v1; automatic worker creation is disabled — upgrade/restart tendd".into());
    }
    CheckResult::Ok(format!("tendd responding at {}", socket.display()))
}

/// Whether the daemon's swap service takes jobs, and if not why: read-only,
/// bounded, and never a plan.
async fn check_swap(socket: &Path) -> CheckResult {
    let client = Client::new(socket.to_path_buf());
    let probe = async {
        let hello = client.hello(Duration::from_millis(1500)).await?;
        let has = |name: &str| hello.capabilities.iter().any(|c| c == name);
        if !has(tend::ipc::SWAP_SERVICE_CAPABILITY) {
            return Ok(Probe::NotOffered);
        }
        if !has(tend::ipc::SWAP_HEALTH_CAPABILITY) {
            return Ok(Probe::NoHealth);
        }
        client.swap_health().await.map(Probe::Health)
    };
    match timeout(Duration::from_millis(3000), probe).await {
        Ok(Ok(probe)) => swap_verdict(&probe),
        Ok(Err(error)) => CheckResult::Warn(format!("swap jobs: health unknown ({error})")),
        Err(_) => CheckResult::Warn("swap jobs: health check did not answer within 3 s".into()),
    }
}

/// What asking the daemon about swap jobs found.
enum Probe {
    /// A daemon from before swap jobs.
    NotOffered,
    /// Swap jobs, but no health endpoint: an older daemon.
    NoHealth,
    Health(tend::swap_service::SwapHealth),
}

fn swap_verdict(probe: &Probe) -> CheckResult {
    use tend::swap_service::SwapReadiness;
    let health = match probe {
        Probe::NotOffered => {
            return CheckResult::Ok(
                "swap jobs: not offered by this daemon (swaps run in the CLI)".into(),
            );
        }
        Probe::NoHealth => {
            return CheckResult::Warn(
                "swap jobs: tendd predates swap health checks — upgrade/restart tendd".into(),
            );
        }
        Probe::Health(health) => health,
    };
    let reason = health.reason.as_deref().unwrap_or("no reason given");
    if let (SwapReadiness::Ready, Some(maintenance)) = (health.readiness, &health.maintenance) {
        return CheckResult::Warn(format!(
            "swap jobs: taking jobs, but {maintenance}; it is tried again after a day"
        ));
    }
    match health.readiness {
        SwapReadiness::Ready => CheckResult::Ok(format!(
            "swap jobs: taking jobs (store schema {}, {} running)",
            health
                .schema
                .map_or_else(|| "?".to_owned(), |schema| schema.to_string()),
            health.jobs_running
        )),
        SwapReadiness::Draining => {
            CheckResult::Warn(format!("swap jobs: shutting down ({reason})"))
        }
        SwapReadiness::Halted => CheckResult::Warn(format!(
            "swap jobs: halted by a fault — {reason}; restart tendd once it is understood"
        )),
        SwapReadiness::Unavailable => CheckResult::Warn(format!(
            "swap jobs: unavailable — {reason}{}",
            match health.schema {
                Some(schema) if schema != health.expected_schema => format!(
                    " (store schema {schema}, this build reads {})",
                    health.expected_schema
                ),
                _ => String::new(),
            }
        )),
    }
}

/// One-shot snapshot fetch shared by the post-IPC checks. Returns
/// `None` when tendd is unreachable so callers can skip rather than
/// double-reporting the daemon-down condition `check_tendd` owns.
/// Requests nobody has claimed for a day. Only the sender can cancel one, the
/// sender is often an agent that has since exited, and one-way notices never
/// wake their recipient, so these accumulate silently. `None` when the daemon
/// predates `msg expire` or does not answer.
async fn check_collaboration(socket: &Path) -> Vec<CheckResult> {
    let client = Client::new(socket.to_path_buf());
    let diagnostics = async {
        let hello = client.hello(Duration::from_millis(1500)).await?;
        if !hello
            .capabilities
            .iter()
            .any(|c| c == tend::collaboration::COLLABORATION_HEALTH_CAPABILITY)
        {
            return Ok(vec![CheckResult::Warn("request recovery diagnostics unknown: tendd lacks collaboration_health_v1; upgrade tendd".into())]);
        }
        client
            .collaboration_health()
            .await
            .map(|health| health_verdicts(&health))
    };
    let mut results = match timeout(Duration::from_millis(1500), diagnostics).await {
        Ok(Ok(results)) => results,
        Ok(Err(error)) => vec![CheckResult::Warn(format!(
            "collaboration diagnostics unavailable: {error}"
        ))],
        Err(_) => vec![CheckResult::Warn(
            "collaboration diagnostics timed out; request health is unknown".into(),
        )],
    };
    if let Some(workers) = check_workers(socket).await {
        results.push(workers);
    }
    results
}

fn health_verdicts(health: &tend::collaboration::CollaborationHealth) -> Vec<CheckResult> {
    let mut results = synthetic_verdicts(&health.synthetic_agents);
    results.push(if health.interrupted_requests == 0 && health.pending_recovery_decisions == 0 {
        CheckResult::Ok("no interrupted original collaboration requests".into())
    } else {
        CheckResult::Warn(format!("{} interrupted original request(s), {} with unavailable recipient bindings; {} pending recovery decision(s). Examples: {}. Inspect with `tend msg list --request-id REQUEST --full`; check existing work before a safe handoff. Missing bindings do not establish process exit; do not blindly resend.",health.interrupted_requests,health.binding_unavailable,health.pending_recovery_decisions,health.example_request_ids.join(", ")))
    });
    results.push(if health.stale_queued == 0 {
        CheckResult::Ok("no ordinary queued request has waited over 24h".into())
    } else if health.stale_quiet_notices == 0 {
        CheckResult::Warn(format!("{} ordinary queued request(s) older than 24h; none are quiet undelivered notices. Individual review is required; do not treat missing recipients or delivery evidence as task completion.",health.stale_queued))
    } else {
        CheckResult::Warn(format!("{} ordinary queued request(s) older than 24h; {} quiet undelivered notice(s) eligible for selective cleanup ({}). Preview with `tend msg expire --older-than 24h --quiet-only --request-id REQUEST --dry-run`; remove `--dry-run` only after reviewing. Other requests require individual review.",health.stale_queued,health.stale_quiet_notices,health.example_quiet_notice_ids.join(", ")))
    });
    results
}

fn synthetic_verdicts(
    diagnostics: &tend::agent_directory::SyntheticDiagnostics,
) -> Vec<CheckResult> {
    let mut results = Vec::new();
    for (bucket, explanation) in [
        (&diagnostics.unbound_hooks,"pane observed but hook session not correlated; trigger an agent event and review hook configuration"),
        (&diagnostics.unobserved_backend,"record(s) from a backend this daemon does not observe; review backend scope and orphan retention, not agent hooks"),
        (&diagnostics.unseen_panes,"record(s) absent from a recent complete pane inventory; verify topology and allow reconciliation, not an agent restart"),
        (&diagnostics.unknown,"record(s) with unavailable, stale or incomplete topology evidence; hook correlation is unknown"),
    ] {
        if bucket.count > 0 { results.push(CheckResult::Warn(format!("{} {explanation} ({})",bucket.count,bucket.examples.join(", ")))); }
    }
    if diagnostics.unknown.count > 0 {
        results.push(CheckResult::Warn(format!("cached topology age: {:?} ms; freshness budget: {} ms; invalidated: {}. Recheck with an explicit agent lookup refresh when fresh evidence is needed",diagnostics.inventory_age_ms,diagnostics.max_inventory_age_ms,diagnostics.inventory_invalidated)));
    }
    if results.is_empty() {
        results.push(CheckResult::Ok(
            "no unresolved synthetic agent records".into(),
        ));
    }
    results
}

/// Workers tend launched that are in error or long idle. `None` when the
/// daemon does not answer or predates the inventory.
async fn check_workers(socket: &Path) -> Option<CheckResult> {
    let client = Client::new(socket.to_path_buf());
    let inventory = timeout(
        Duration::from_millis(1500),
        client.collaboration_workers(tend::ipc::DEFAULT_WORKER_IDLE_SECS),
    )
    .await
    .ok()?
    .ok()?;
    Some(workers_verdict(&inventory))
}

fn workers_verdict(inventory: &tend::collaboration::WorkerInventory) -> CheckResult {
    use tend::collaboration::WorkerFlag;
    let panes = |flag: WorkerFlag| -> Vec<&str> {
        inventory
            .workers
            .iter()
            .filter(|worker| worker.flags.contains(&flag))
            .map(|worker| worker.pane.as_str())
            .collect()
    };
    let error = panes(WorkerFlag::Error);
    let idle = panes(WorkerFlag::LongIdle);
    let overlap = panes(WorkerFlag::PossibleOverlap);
    let total = inventory.workers.len();
    let overlap_note = if overlap.is_empty() {
        String::new()
    } else {
        format!(
            "; {} possibly overlapping ({}), which can be deliberate",
            overlap.len(),
            overlap.join(", ")
        )
    };
    if error.is_empty() && idle.is_empty() {
        return CheckResult::Ok(format!(
            "{total} live worker(s), none in error or idle over {}h{overlap_note}",
            inventory.idle_threshold_secs / 3600
        ));
    }
    let mut parts = Vec::new();
    if !error.is_empty() {
        parts.push(format!(
            "{} in error ({}) — never reused, so a new worker may be spawned beside it",
            error.len(),
            error.join(", ")
        ));
    }
    if !idle.is_empty() {
        parts.push(format!(
            "{} observed idle over {}h ({})",
            idle.len(),
            inventory.idle_threshold_secs / 3600,
            idle.join(", ")
        ));
    }
    CheckResult::Warn(format!(
        "{total} live worker(s): {}{overlap_note}. Review with `tend workers`",
        parts.join("; ")
    ))
}

/// Branch on the host OS: launchd on macOS, systemd on Linux. Other
/// targets get a neutral note rather than a red ✗ — tend runs on BSDs
/// and WSL1 too, where neither manager applies.
fn check_service_manager() -> CheckResult {
    match std::env::consts::OS {
        "macos" => check_launchctl(),
        "linux" => check_systemctl(),
        other => CheckResult::Warn(format!(
            "Service manager — no per-OS check for {other} (start tendd manually)"
        )),
    }
}

fn check_launchctl() -> CheckResult {
    let uid = uid_string();
    // Report a disable override ahead of the load probe. `launchctl print`
    // fails identically for "never installed" and "installed but disabled",
    // and only the first of those is fixed by re-running the installer — a
    // disabled label is skipped at every login no matter what the plist says.
    // `tend init` clears it now, so this is aimed at hosts that are still
    // carrying the override an older tend.app wrote.
    if launchd::service_disabled() == Some(true) {
        return CheckResult::Fail(format!(
            "launchd: tendd agent is disabled and will not start at login — run \
             `launchctl enable gui/{uid}/{LAUNCHD_LABEL}`, then `tend init --component tendd-launchd`"
        ));
    }
    let target = format!("gui/{uid}/{LAUNCHD_LABEL}");
    let out = Command::new("launchctl").args(["print", &target]).output();
    match out {
        Ok(o) if o.status.success() => CheckResult::Ok("launchd: tendd agent loaded".into()),
        Ok(_) => CheckResult::Fail(
            "launchd: tendd agent not loaded — run `tend init --component tendd-launchd`"
                .to_string(),
        ),
        Err(e) => CheckResult::Warn(format!("launchd: could not run launchctl ({e})")),
    }
}

fn check_systemctl() -> CheckResult {
    // Gate on actual bus reachability, not just binary presence: in
    // Docker without `--privileged` (and other non-systemd-init hosts)
    // systemctl exists but the user bus is gone, so any `is-active`
    // probe is meaningless. `tend init` already falls back to the
    // shellrc autostart there, and doctor should match that posture.
    if !systemd_files::systemd_available() {
        return shellrc_autostart_verdict(shellrc_autostart().as_deref());
    }
    let out = Command::new("systemctl")
        .args(["--user", "is-active", SYSTEMD_UNIT])
        .output();
    match out {
        Ok(o) => {
            // `is-active` writes "active" / "inactive" / "failed" /
            // "unknown" to stdout regardless of exit code; trim and
            // match for a clean message. Fall back to "not loaded"
            // when stdout is empty (no unit file at all).
            let raw = String::from_utf8_lossy(&o.stdout).trim().to_string();
            let state = if raw.is_empty() {
                "not loaded".to_string()
            } else {
                raw
            };
            if state == "active" {
                CheckResult::Ok("systemd: tendd.service active".into())
            } else {
                CheckResult::Fail(format!(
                    "systemd: tendd.service is {state} — run `systemctl --user enable --now tendd`"
                ))
            }
        }
        Err(e) => CheckResult::Warn(format!("systemd: could not run systemctl ({e})")),
    }
}

/// The shell rc file that starts tendd on a host with no usable service
/// manager, if any. `tend init` writes a managed block, but a hand-written
/// `tend daemon start` line works just as well, so any is accepted.
fn shellrc_autostart() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    [
        ".zshrc",
        ".zshenv",
        ".zprofile",
        ".bashrc",
        ".bash_profile",
        ".profile",
    ]
    .iter()
    .map(|name| home.join(name))
    .find(|path| std::fs::read_to_string(path).is_ok_and(|text| text.contains("tend daemon start")))
}

/// Without systemd, a shell rc autostart is the supported setup, not a
/// degraded one: only its absence is worth a warning.
fn shellrc_autostart_verdict(rc: Option<&Path>) -> CheckResult {
    match rc {
        Some(rc) => CheckResult::Ok(format!(
            "systemd: user manager not usable here — tendd autostarts from {}",
            rc.display()
        )),
        None => CheckResult::Warn(
            "systemd: user manager not usable here, and no shell rc starts tendd — run \
             `tend init --component tendd-shellrc`"
                .into(),
        ),
    }
}

/// Confirm the unit/plist file the install wizard would write actually
/// exists on disk. Catches the common "I deleted my dotfiles" footgun.
///
/// On Linux, the install location moved from `~/.config/systemd/user/`
/// to `~/.local/share/systemd/user/`. We surface the legacy path in
/// messages so upgraders aren't told their existing unit is "missing"
/// when systemd is happily using it.
fn check_unit_file() -> CheckResult {
    match std::env::consts::OS {
        "macos" => match launchd::default_unit_path() {
            Some(path) if path.exists() => {
                CheckResult::Ok(format!("Unit file present: {}", path.display()))
            }
            Some(path) => CheckResult::Fail(format!(
                "Unit file missing: {} — run `tend init`",
                path.display()
            )),
            None => {
                CheckResult::Warn("Service unit — could not resolve home directory".to_string())
            }
        },
        "linux" => {
            // Mirror `check_systemctl`: if the user manager isn't
            // usable, an absent unit file isn't an issue — it's
            // expected, because no manager will ever consume it.
            if !systemd_files::systemd_available() {
                return CheckResult::Ok(
                    "Service unit — not used without a systemd user manager".into(),
                );
            }
            check_systemd_unit_file()
        }
        _ => CheckResult::Warn("Service unit — no per-OS check for this platform".to_string()),
    }
}

fn check_systemd_unit_file() -> CheckResult {
    let primary = systemd_files::default_unit_path();
    let legacy = systemd_files::legacy_unit_path();
    let primary_exists = primary.as_deref().is_some_and(Path::exists);
    let legacy_exists = legacy.as_deref().is_some_and(Path::exists);

    match (primary, legacy, primary_exists, legacy_exists) {
        (Some(p), Some(l), true, true) => CheckResult::Warn(format!(
            "Unit file present at {} but legacy copy still at {} — remove the legacy file to avoid \
             override surprises",
            p.display(),
            l.display()
        )),
        (Some(p), _, true, _) => CheckResult::Ok(format!("Unit file present: {}", p.display())),
        (Some(p), Some(l), false, true) => CheckResult::Warn(format!(
            "Unit file at legacy location {} — move it to {} to match the current install layout",
            l.display(),
            p.display()
        )),
        (Some(p), _, false, false) => CheckResult::Fail(format!(
            "Unit file missing: {} — run `tend init`",
            p.display()
        )),
        _ => CheckResult::Warn("Service unit — could not resolve home directory".to_string()),
    }
}

/// Run the per-agent hook check for Claude, Codex, Gemini, and agy. Each
/// returns `(label, result)` so `run()` can log them with a consistent
/// `Hooks · <agent>` prefix.
fn check_agent_hooks() -> Vec<(String, CheckResult)> {
    let mut checks = vec![
        (
            "Hooks · Claude".to_string(),
            check_one_agent_hook("claude", claude::default_path(), claude_hook_configured),
        ),
        (
            "Hooks · Codex".to_string(),
            check_one_agent_hook("codex", codex::default_path(), codex_hook_configured),
        ),
        (
            "Hooks · Gemini".to_string(),
            check_one_agent_hook("gemini", gemini::default_path(), gemini_hook_configured),
        ),
    ];
    // agy only: reported when agy is actually installed. Unlike the other
    // three, a missing `hooks.json` is the *normal* state for a fresh agy
    // (it writes the file only once `/hooks` is used), so an unconditional
    // check would turn a red line — and a "run `tend init`" remedy that
    // wires an agent they don't have — on for every existing user.
    if crate::init::detect::antigravity_installed() {
        checks.push((
            "Hooks · Antigravity".to_string(),
            check_one_agent_hook(
                "agy",
                antigravity::default_path(),
                antigravity_hook_configured,
            ),
        ));
    }
    checks
}

fn check_one_agent_hook(
    agent: &str,
    path: Option<PathBuf>,
    is_configured: fn(&str) -> bool,
) -> CheckResult {
    let Some(path) = path else {
        return CheckResult::Warn(format!("could not resolve home path for {agent}"));
    };
    if !path.exists() {
        return CheckResult::Fail(format!(
            "{} missing — run `tend init --component {agent}-hooks`",
            path.display()
        ));
    }
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            if is_configured(&text) {
                CheckResult::Ok(format!("hook installed in {}", path.display()))
            } else {
                CheckResult::Fail(format!(
                    "no `tend hook {agent}` entry in {} — run `tend init`",
                    path.display()
                ))
            }
        }
        Err(e) => CheckResult::Fail(format!("could not read {}: {e}", path.display())),
    }
}

/// True iff Claude's settings.json contains at least one hook entry
/// whose command starts with `tend hook claude`. We do a structural
/// walk (not a substring search) because users do legitimately put the
/// literal "tend hook claude" inside other strings — e.g. comments in
/// adjacent fields — and we don't want false positives.
pub fn claude_hook_configured(settings_text: &str) -> bool {
    json_hooks_dict_has_command(settings_text, "tend hook claude")
}

/// True iff Gemini's settings.json contains at least one hook entry
/// whose command starts with `tend hook gemini`. Same JSON shape as
/// Claude.
pub fn gemini_hook_configured(settings_text: &str) -> bool {
    json_hooks_dict_has_command(settings_text, "tend hook gemini")
}

/// True iff agy's `hooks.json` contains at least one handler whose command
/// starts with `tend hook agy`.
///
/// A different shape from the Claude/Gemini walker, not a variant of it: agy
/// keys the file by hook NAME first (`{"<name>": {"<Event>": [...]}}`), and an
/// event's entries are either bare handlers (`Stop`, `PreInvocation`, …) or
/// `{matcher, hooks[]}` groups (`PreToolUse`, `PostToolUse`). Both forms are
/// accepted here so the check keeps passing if agy ever relaxes which events
/// take a matcher.
pub fn antigravity_hook_configured(hooks_text: &str) -> bool {
    const PREFIX: &str = "tend hook agy";
    /// Every event tend needs wired. Checking the whole set — rather than
    /// "any tend command anywhere" — is what makes a block left over from an
    /// older tend report as broken. A partial block is the worst state to
    /// call healthy: the row exists but never shows prompts or tools, and
    /// nothing tells the user to re-run `tend init`.
    const REQUIRED: &[&str] = &[
        "SessionStart",
        "PreInvocation",
        "PostInvocation",
        "PreToolUse",
        "PostToolUse",
        "Stop",
    ];

    let Ok(root) = serde_json::from_str::<serde_json::Value>(hooks_text) else {
        return false;
    };
    let Some(named) = root.as_object() else {
        return false;
    };
    // Union across named hooks rather than requiring one key to hold them
    // all: tend writes a single `tend` key, but a hand-split config that
    // still covers every event is genuinely configured.
    REQUIRED.iter().all(|event| {
        named.values().any(|spec| {
            spec.get(event)
                .and_then(serde_json::Value::as_array)
                .is_some_and(|arr| {
                    arr.iter()
                        .any(|entry| has_command(entry, PREFIX) || entry_has_command(entry, PREFIX))
                })
        })
    })
}

/// A bare handler object — the flat (matcher-less) event form.
fn has_command(entry: &serde_json::Value, prefix: &str) -> bool {
    entry
        .get("command")
        .and_then(|v| v.as_str())
        .is_some_and(|c| c.trim_start().starts_with(prefix))
}

/// True iff Codex's `config.toml` contains at least one hook entry
/// whose command starts with `tend hook codex`. The TOML shape is
/// `[[hooks.<Event>]]` arrays-of-tables containing `hooks = [...]`
/// inline tables — same shape as the upsert path writes.
pub fn codex_hook_configured(config_text: &str) -> bool {
    let Ok(doc) = config_text.parse::<toml_edit::DocumentMut>() else {
        return false;
    };
    // Bind to a local so the iterator's borrow on `doc` is dropped
    // before `doc` itself goes out of scope at end-of-function.
    let Some(hooks_tbl) = doc.get("hooks").and_then(toml_edit::Item::as_table) else {
        return false;
    };
    let found = hooks_tbl.iter().any(|(_, item)| {
        let Some(aot) = item.as_array_of_tables() else {
            return false;
        };
        aot.iter().any(toml_table_has_tend_command)
    });
    found
}

/// Walk a single `[[hooks.<Event>]]` table looking for any inner
/// `command` string starting with `tend hook codex`.
fn toml_table_has_tend_command(t: &toml_edit::Table) -> bool {
    let Some(inner) = t.get("hooks") else {
        return false;
    };
    match inner {
        toml_edit::Item::Value(toml_edit::Value::Array(arr)) => arr.iter().any(|v| match v {
            toml_edit::Value::InlineTable(it) => it
                .get("command")
                .and_then(toml_edit::Value::as_str)
                .is_some_and(|c| c.trim_start().starts_with("tend hook codex")),
            _ => false,
        }),
        toml_edit::Item::ArrayOfTables(aot) => aot.iter().any(|inner| {
            inner
                .get("command")
                .and_then(toml_edit::Item::as_str)
                .is_some_and(|c| c.trim_start().starts_with("tend hook codex"))
        }),
        _ => false,
    }
}

/// Helper for the JSON-hook agents (Claude + Gemini). Both store hooks
/// as `{ "hooks": { "<Event>": [{ "hooks": [{ "command": "..." }] }] } }`,
/// so the search shape is identical.
fn json_hooks_dict_has_command(settings_text: &str, prefix: &str) -> bool {
    let Ok(root) = serde_json::from_str::<serde_json::Value>(settings_text) else {
        return false;
    };
    let Some(hooks) = root.get("hooks").and_then(|v| v.as_object()) else {
        return false;
    };
    hooks.values().any(|event_arr| {
        event_arr
            .as_array()
            .is_some_and(|arr| arr.iter().any(|entry| entry_has_command(entry, prefix)))
    })
}

fn entry_has_command(entry: &serde_json::Value, prefix: &str) -> bool {
    let Some(arr) = entry.get("hooks").and_then(|v| v.as_array()) else {
        return false;
    };
    arr.iter().any(|h| {
        h.get("command")
            .and_then(|v| v.as_str())
            .is_some_and(|c| c.trim_start().starts_with(prefix))
    })
}

/// Report where tend reads `config.toml` from, and whether it is there.
///
/// A missing file is not a problem — every setting has a compiled default
/// and `tend init` only writes the file for the components that need it.
/// The problem is *finding* it: `dirs::config_dir()` is
/// `~/Library/Application Support` on macOS and `~/.config` on Linux, so
/// "add `[collaboration]` to `~/.config/tend/config.toml`" is wrong half
/// the time and the edit lands in a file nothing reads. Printing the
/// resolved path costs one line and removes the guesswork.
fn check_config_file() -> CheckResult {
    let Some(path) = tend::paths::default_config_file() else {
        return CheckResult::Warn("config: could not resolve the config directory".into());
    };
    let shown = path.display();
    match std::fs::read_to_string(&path) {
        Ok(text) => match text.parse::<toml_edit::DocumentMut>() {
            Ok(_) => CheckResult::Ok(format!("config: {shown}")),
            // A malformed config is silently ignored by some readers and
            // fatal to others; either way the user's edits are not taking
            // effect and they deserve to hear it here.
            Err(e) => CheckResult::Fail(format!("config: {shown} is not valid TOML — {e}")),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            CheckResult::Ok(format!("config: {shown} (absent — using defaults)"))
        }
        Err(e) => CheckResult::Warn(format!("config: {shown} unreadable — {e}")),
    }
}

/// What the tmux server and client behind this doctor run can actually do.
///
/// Read once and shared, because tend's two installation assumptions —
/// "the user's `~/.tmux.conf` is what the server reads" and "a popup
/// reaches the user" — are both false on a tmux server a *front-end* owns
/// rather than the user. amux/cmux starts its engine as
/// `tmux -f /dev/null -L amux -CC`: no config the user can edit, and a
/// control-mode client that tmux never sends popup content to. Checks that
/// don't know this print remedies that cannot work, and one of them
/// (`tmux source-file ~/.tmux.conf`) is worse than useless there — it is
/// the exact thing the front-end isolates its server against.
struct TmuxEnvironment {
    /// Short name of the server this shell is inside (`default`, `amux`),
    /// or `None` outside tmux — where every reading below describes the
    /// default server instead of the user's current context.
    socket: Option<String>,
    surface: ClientSurface,
    /// Config files the server loaded; empty when nothing could be read.
    config_files: Vec<String>,
}

impl TmuxEnvironment {
    fn probe() -> Self {
        let target = layout::WindowTarget::resolve();
        Self {
            socket: layout::current_socket_name(),
            surface: layout::client_surface(&target),
            config_files: layout::server_config_files(),
        }
    }

    /// The server reads no config the user could put tend's bindings in.
    fn isolated(&self) -> bool {
        layout::config_isolated(&self.config_files)
    }

    /// No server answered at all, so there is nothing to diagnose about
    /// one. Distinguished from a live server with nothing notable to say.
    fn no_server(&self) -> bool {
        self.config_files.is_empty() && self.surface == ClientSurface::Unknown
    }

    fn server_label(&self) -> String {
        self.socket
            .as_ref()
            .map_or_else(|| "the default server".into(), |s| format!("server `{s}`"))
    }

    /// One line on whether tend's overlays can be drawn here.
    ///
    /// Deliberately narrow: config isolation is reported by the checks it
    /// actually explains (bindings, `TEND_SOCKET`), so the two facts land
    /// on the lines a user would look for them on rather than repeating.
    fn describe(&self) -> CheckResult {
        if self.no_server() {
            return CheckResult::Ok("tmux: no running server to inspect".into());
        }
        let server = self.server_label();
        match self.surface {
            ClientSurface::Detached => CheckResult::Ok(format!(
                "tmux: nothing is attached to {server} — cmux drives panes this way, and so does \
                 any session nobody has attached. tmux resolves key bindings per client, so \
                 tend's prefix keys never fire and `display-popup` fails with \"no current \
                 client\"; run `tend peek`, `tend watch` and `tend dashboard` as commands \
                 instead — peek prints its per-pane report here"
            )),
            ClientSurface::ControlMode => CheckResult::Ok(format!(
                "tmux: {server} is shown through a control-mode client (`tmux -CC` — amux, \
                 iTerm2), which tmux sends no popup content, so the prefix popups cannot draw \
                 on it; run `tend watch` and `tend dashboard` as commands, and `tend peek` \
                 prints its per-pane report instead of the overlay"
            )),
            ClientSurface::Terminal => {
                CheckResult::Ok(format!("tmux: {server} — terminal client, overlays render"))
            }
            ClientSurface::Unknown => {
                CheckResult::Ok(format!("tmux: {server} — no client reading to inspect"))
            }
        }
    }
}

/// Confirm the `prefix+s` binding tmux will actually run is the managed
/// one.
///
/// `check_tmux_blocks` only proves the marker block exists in the file.
/// tmux resolves duplicate `bind-key` definitions last-one-wins, so a
/// hand-written binding further down `~/.tmux.conf` silently overrides
/// the block — and the server keeps whatever it read at startup until
/// someone sources the file again. Both cases leave the marker block
/// intact and doctor green while `prefix+s` opens an inset popup, whose
/// inner width cannot reach the 120 columns the watch inspector needs.
fn check_tmux_watch_binding(env: &TmuxEnvironment) -> CheckResult {
    // Scoped like the other tmux checks. Unscoped, it asked the default
    // socket, and rmux, unlike tmux, starts a server to answer: every
    // isolated doctor run (the test suite included) left one behind.
    // On Windows the host is rmux, asked only through the server this shell
    // is in for the same reason.
    let mut command = if cfg!(windows) {
        if tend::backend::rmux::endpoint_from_env().is_none() {
            return CheckResult::Ok("rmux: run from an rmux pane to check its bindings".into());
        }
        tend::RmuxBackend::new().command(None)
    } else {
        tend::tmux::tmux_command_scoped()
    };
    let out = command.args(["list-keys", "-T", "prefix"]).output();
    let text = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        // No server running yet is not a misconfiguration — there is
        // simply nothing to compare against.
        Ok(_) => {
            return CheckResult::Ok("tmux: no running server to check bindings against".into())
        }
        Err(e) => return CheckResult::Warn(format!("tmux: could not list keys ({e})")),
    };
    watch_binding_verdict(watch_popup_binding(&text), env)
}

/// Pure half of [`check_tmux_watch_binding`], over the live binding tmux
/// reported (`None` when no prefix key runs `tend watch`).
fn watch_binding_verdict(binding: Option<&str>, env: &TmuxEnvironment) -> CheckResult {
    match binding {
        // An isolated server never reads the file `tend init` writes, so
        // the missing binding is the design of whatever owns this server,
        // not a broken install. Saying otherwise would send the user to
        // `tmux source-file ~/.tmux.conf` — which on such a server pulls in
        // the very config (session-restoring plugins included) it was
        // started to keep out.
        None if env.isolated() => CheckResult::Ok(format!(
            "tmux: {} reads no config (`-f /dev/null`), so tend's `~/.tmux.conf` bindings never \
             reach it — expected on a front-end-owned server (amux/cmux); run `tend watch` and \
             `tend dashboard` directly instead of through prefix keys",
            env.server_label()
        )),
        None => CheckResult::Warn(
            "tmux: no prefix key runs `tend watch` — run `tend init --component tmux-popup`, then `tmux source-file ~/.tmux.conf`".into(),
        ),
        // Correct in the file, but "prefix+s opens tend watch" would be a
        // false claim on a server whose viewer cannot run it: tmux resolves
        // bindings per client, and these have none that draws.
        Some(binding) if popup_binding_is_full_client(binding) && !env.surface.draws_overlays() => {
            CheckResult::Ok(
                "tmux: the managed full-client `tend watch` binding is installed, but no viewer \
                 on this server can run it (see the line above) — run `tend watch` as a command"
                    .into(),
            )
        }
        Some(binding) if popup_binding_is_full_client(binding) => {
            CheckResult::Ok("tmux: prefix+s opens tend watch full-client".into())
        }
        Some(_) => CheckResult::Fail(
            "tmux: the live `tend watch` binding is an inset popup, so the inspector's 120-column minimum is unreachable — a later `bind-key` in ~/.tmux.conf is overriding the managed block, or the server has not re-read it. Remove the stray binding, then `tmux source-file ~/.tmux.conf`".into(),
        ),
    }
}

/// First `list-keys` line that launches `tend watch`, if any.
fn watch_popup_binding(list_keys_output: &str) -> Option<&str> {
    list_keys_output
        .lines()
        .find(|line| line.contains("tend watch"))
}

/// True when the binding gives the popup the whole client with no border.
/// Both matter: `-w 90%` of a 134-column terminal is 120, and the border
/// takes 2 more, landing at 118 — one column short of the inspector for
/// the entire reason the managed block uses `-B` and `100%`.
fn popup_binding_is_full_client(binding: &str) -> bool {
    // The current managed block runs `tend watch --popup`, which opens its
    // own borderless, full-width popup (`mux_control::watch_popup`).
    if binding.contains("tend watch --popup") {
        return true;
    }
    let width_full = binding.contains("-w \"100%\"") || binding.contains("-w 100%");
    // tmux prints merged short flags, so `-B -E` comes back as `-BE`.
    let borderless = binding
        .split_whitespace()
        .any(|tok| tok.starts_with('-') && !tok.starts_with("--") && tok.contains('B'));
    width_full && borderless
}

/// Confirm both tmux marker blocks (popup + statusline) exist in
/// `~/.tmux.conf`. We bundle the result so the user sees one tmux
/// line in the doctor output rather than two redundant ones.
fn check_tmux_blocks() -> CheckResult {
    let Some(path) = crate::init::files::tmux::default_path() else {
        return CheckResult::Warn("tmux: could not resolve $HOME".into());
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return CheckResult::Fail(format!(
            "tmux: {} not found — run `tend init`",
            path.display()
        ));
    };
    let popup = tmux_marker_block_detected(&text, TMUX_BLOCK_POPUP);
    let statusline = tmux_marker_block_detected(&text, TMUX_BLOCK_STATUSLINE);
    match (popup, statusline) {
        (true, true) => CheckResult::Ok("tmux: popup + statusline blocks present".into()),
        (true, false) => CheckResult::Fail(
            "tmux: statusline block missing — run `tend init --component tmux-statusline`".into(),
        ),
        (false, true) => CheckResult::Fail(
            "tmux: popup block missing — run `tend init --component tmux-popup`".into(),
        ),
        (false, false) => {
            CheckResult::Fail("tmux: tend managed blocks missing — run `tend init`".into())
        }
    }
}

/// Confirm `TEND_SOCKET` is pinned in the tmux server environment **and**
/// that the pinned path still points at a live socket. A pin alone isn't
/// enough: if it references a daemon socket that has since moved, or a
/// temp socket that was cleaned up (e.g. a dashboard socket left behind by
/// a short-lived nested session), every new pane inherits a dead path and
/// `tend watch` / status-right fail with "daemon not reachable". We probe
/// the path the same way the IPC client does — a plain connect — so a
/// stale pin downgrades to a warning instead of a misleading ✔.
fn check_tend_socket_env(daemon_socket: &Path, env: &TmuxEnvironment) -> CheckResult {
    // Ask the server this shell is in. Native tmux pointed at an rmux
    // socket (`$TMUX` inside rmux) cannot read it and reported the pin as
    // unset even when it was set.
    let value = match crate::mux_control::Control::Ambient.output(&[
        "show-environment",
        "-g",
        "TEND_SOCKET",
    ]) {
        Ok(stdout) => match parse_tend_socket_env(&stdout) {
            Some(value) => value,
            None => return tend_socket_unset(daemon_socket, env),
        },
        // A variable that was never set is a non-zero exit, as before.
        Err(_) => return tend_socket_unset(daemon_socket, env),
    };

    if socket_is_reachable(Path::new(&value)) {
        CheckResult::Ok(format!("tmux: TEND_SOCKET={value} (reachable)"))
    } else {
        CheckResult::Warn(format!(
            "tmux: TEND_SOCKET={value} is unreachable — new panes will fail to reach tendd; \
             re-pin it to {} with `tend init`",
            daemon_socket.display()
        ))
    }
}

/// The unset case, which means different things on different servers.
///
/// `tend init` writes the pin into `~/.tmux.conf`, so on a server that
/// reads no config that remedy is dead on arrival — and it may not be
/// needed at all: a pane with no pin falls back to the default socket
/// path, which is the right answer whenever tendd is listening there.
/// Only a daemon on a *non-default* socket makes the missing pin bite.
///
/// That holds on every server, not just config-isolated ones: `tend init`
/// deliberately writes no pin for the default socket (`needs_socket_pin`),
/// so telling a default-socket user to run it promised a fix that never
/// came, and the warning returned after every multiplexer restart.
fn tend_socket_unset(daemon_socket: &Path, env: &TmuxEnvironment) -> CheckResult {
    let shown = daemon_socket.display();
    if same_path(daemon_socket, &tend::paths::default_socket()) {
        return CheckResult::Ok(format!(
            "tmux: TEND_SOCKET unset on {} — harmless, since panes fall back to the default \
             socket tendd already listens on ({shown})",
            env.server_label()
        ));
    }
    if !env.isolated() {
        return CheckResult::Warn(format!(
            "tmux: TEND_SOCKET not set and tendd listens on the non-default {shown} — run \
             `tend init` to pin it in `~/.tmux.conf`"
        ));
    }
    CheckResult::Warn(format!(
        "tmux: TEND_SOCKET unset on {}, which reads no `~/.tmux.conf`, and tendd listens on \
         the non-default {shown} — panes there will fall back to the default path and fail \
         to reach it; pin it on that server with `tmux set-environment -g TEND_SOCKET \
         {shown}`",
        env.server_label()
    ))
}

/// Whether two socket paths name the same file, tolerating the `/tmp` →
/// `/private/tmp` symlink macOS resolves inconsistently between the
/// daemon's recorded path and `paths::default_socket()`. A path that
/// cannot be canonicalized (the socket is gone) compares literally, which
/// is the pre-existing behaviour.
fn same_path(a: &Path, b: &Path) -> bool {
    let resolve = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    resolve(a) == resolve(b)
}

/// Extract the value from a `tmux show-environment -g TEND_SOCKET` line.
/// Returns `None` when the variable is unset or empty — tmux prints a
/// `-TEND_SOCKET` removal marker (or nothing useful), neither of which
/// starts with `TEND_SOCKET=` followed by a value.
fn parse_tend_socket_env(stdout: &str) -> Option<String> {
    let value = stdout.trim().strip_prefix("TEND_SOCKET=")?;
    (!value.is_empty()).then(|| value.to_string())
}

/// Best-effort liveness probe for a unix socket path. A successful connect
/// means something is listening; `ENOENT` / `ECONNREFUSED` mean the pin is
/// stale. Mirrors the connect probe in `tend::ipc` — a local unix connect
/// resolves immediately, so no timeout is needed.
fn socket_is_reachable(path: &Path) -> bool {
    tend::transport::endpoint_is_live(path)
}

/// Pure detector for a `# >>> tend managed (<id>) >>>` ... `# <<< tend
/// managed (<id>) <<<` block in shell-comment-style config text.
///
/// Mirrors `init::marker::find_block` (private there) — copying keeps
/// us decoupled while preserving the same line-start anchoring that
/// stops false positives inside quoted strings.
pub fn tmux_marker_block_detected(haystack: &str, id: &str) -> bool {
    let open = format!("# >>> tend managed ({id}) >>>");
    let close = format!("# <<< tend managed ({id}) <<<");
    let Some(open_idx) = line_anchored_find(haystack, &open) else {
        return false;
    };
    let after_open = open_idx + open.len();
    line_anchored_find(&haystack[after_open..], &close).is_some()
}

/// Locate `needle` in `haystack` only when it begins at a line
/// boundary (start-of-string or just after `\n`).
fn line_anchored_find(haystack: &str, needle: &str) -> Option<usize> {
    let mut search_from = 0;
    while let Some(rel) = haystack[search_from..].find(needle) {
        let abs = search_from + rel;
        if abs == 0 || haystack.as_bytes()[abs - 1] == b'\n' {
            return Some(abs);
        }
        search_from = abs + 1;
    }
    None
}

/// Read the last `tail_lines` of `path` and return up to `max_warnings`
/// lines that mention "ERROR" or "panic". Best-effort: a missing file
/// or read error returns an empty vec, which the caller renders as
/// "log clean".
fn recent_tendd_errors(path: &Path, tail_lines: usize, max_warnings: usize) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    extract_recent_errors(&text, tail_lines, max_warnings)
}

/// Pure splitter — extracted so we can unit-test the tail/filter logic
/// without touching the filesystem.
fn extract_recent_errors(text: &str, tail_lines: usize, max_warnings: usize) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(tail_lines);
    lines[start..]
        .iter()
        .filter(|l| l.contains("ERROR") || l.contains("panic"))
        .take(max_warnings)
        .map(|s| (*s).to_string())
        .collect()
}

/// Render a single tendd-log warning line to fit under the
/// `⚠ tendd log — <line>` prefix the doctor box uses. Trims trailing
/// whitespace and collapses long lines so the box layout stays clean.
pub fn format_warning_line(raw: &str) -> String {
    let trimmed = raw.trim();
    // Cap at 200 chars — anything longer is almost certainly a stack
    // trace dump and isn't useful to surface inline.
    let capped: String = trimmed.chars().take(200).collect();
    format!("tendd log — {capped}")
}

/// POSIX uid as a decimal string. Falls back to `"501"` (typical
/// macOS user) only when `id -u` itself fails — same logic as
/// `init::util::uid_string`, duplicated so this module's checks don't
/// pull in the wider `init::util` surface area.
fn uid_string() -> String {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map_or_else(|| "501".into(), |s| s.trim().to_string())
}

#[cfg(test)]
mod tests {
    fn health(
        readiness: tend::swap_service::SwapReadiness,
        reason: Option<&str>,
        schema: Option<i64>,
    ) -> Probe {
        Probe::Health(tend::swap_service::SwapHealth {
            readiness,
            reason: reason.map(str::to_owned),
            schema,
            expected_schema: 2,
            fault: None,
            jobs_running: 1,
            maintenance: None,
        })
    }

    /// Every state the swap service can be in gets its own verdict; only a
    /// service taking jobs (or no service at all) is OK.
    #[test]
    fn the_swap_verdict_names_each_state() {
        use tend::swap_service::SwapReadiness::{Draining, Halted, Ready, Unavailable};
        let text = |result: &CheckResult| match result {
            CheckResult::Ok(t) | CheckResult::Warn(t) | CheckResult::Fail(t) => t.clone(),
        };
        let ok = swap_verdict(&health(Ready, None, Some(2)));
        assert!(
            matches!(&ok, CheckResult::Ok(t) if t.contains("taking jobs") && t.contains("1 running"))
        );
        assert!(matches!(
            swap_verdict(&Probe::NotOffered),
            CheckResult::Ok(_)
        ));
        assert!(text(&swap_verdict(&Probe::NoHealth)).contains("predates"));
        for (probe, word) in [
            (
                health(Draining, Some("shutting down"), Some(2)),
                "shutting down",
            ),
            (
                health(Halted, Some("journal full"), Some(2)),
                "journal full",
            ),
            (
                health(Unavailable, Some("owned by another live manager"), None),
                "another live manager",
            ),
        ] {
            let verdict = swap_verdict(&probe);
            assert!(
                matches!(verdict, CheckResult::Warn(_)),
                "{}",
                text(&verdict)
            );
            assert!(text(&verdict).contains(word), "{}", text(&verdict));
        }
        let skewed = swap_verdict(&health(Unavailable, Some("store schema too new"), Some(9)));
        assert!(text(&skewed).contains("store schema 9, this build reads 2"));
        // Upkeep that failed is a warning, not a halt.
        let Probe::Health(mut upkeep) = health(Ready, None, Some(3)) else {
            unreachable!()
        };
        upkeep.maintenance = Some("pruning old swap job evidence failed: disk I/O".into());
        let verdict = swap_verdict(&Probe::Health(upkeep));
        assert!(matches!(&verdict, CheckResult::Warn(t) if t.contains("disk I/O")));
    }

    use super::*;

    #[test]
    fn same_version_daemon_without_spawn_coordination_still_warns() {
        let mut hello = tend::ipc::Hello {
            collaboration_error: None,
            capabilities: Vec::new(),
            generation: None,
            protocol: tend::event::PROTOCOL_VERSION,
            version: Some(env!("CARGO_PKG_VERSION").into()),
        };
        let CheckResult::Warn(message) = tendd_hello_verdict(&hello, Path::new("/tmp/test-tendd"))
        else {
            panic!("missing capability must not look healthy")
        };
        assert!(message.contains("automatic worker creation is disabled"));
        assert!(message.contains("upgrade/restart tendd"));
        hello
            .capabilities
            .push(tend::collaboration::WORKER_SPAWN_CAPABILITY.into());
        assert!(matches!(
            tendd_hello_verdict(&hello, Path::new("/tmp/test-tendd")),
            CheckResult::Ok(_)
        ));
    }

    fn worker(
        pane: &str,
        flags: Vec<tend::collaboration::WorkerFlag>,
    ) -> tend::collaboration::WorkerEntry {
        tend::collaboration::WorkerEntry {
            pane: pane.into(),
            agent_kind: tend::event::AgentKind::Codex,
            agent_session_id: "s".into(),
            room: tend::collaboration::RoomId {
                host: "rmux".into(),
                socket: None,
                window_id: "@1".into(),
            },
            tmux_session_name: None,
            window_name: None,
            roles: Vec::new(),
            alias: None,
            state: tend::event::AgentState::Idle,
            pending: false,
            draining: false,
            observed_idle: None,
            queued_requests: 0,
            claimed_requests: 0,
            flags,
        }
    }

    #[test]
    fn workers_warn_on_error_and_long_idle_but_not_on_overlap_alone() {
        use tend::collaboration::{WorkerFlag, WorkerInventory};
        let inventory = |workers| WorkerInventory {
            workers,
            idle_threshold_secs: 24 * 3600,
            recorded_grants: 0,
        };
        let CheckResult::Warn(message) = workers_verdict(&inventory(vec![
            worker("rmux:%304", vec![WorkerFlag::Error]),
            worker("rmux:%11", vec![WorkerFlag::LongIdle]),
            worker("rmux:%9", vec![]),
        ])) else {
            panic!("error and idle workers warn");
        };
        assert!(message.contains("1 in error (rmux:%304)"), "{message}");
        assert!(
            message.contains("1 observed idle over 24h (rmux:%11)"),
            "{message}"
        );
        assert!(message.contains("tend workers"), "{message}");
        let CheckResult::Ok(message) = workers_verdict(&inventory(vec![
            worker("rmux:%1", vec![WorkerFlag::PossibleOverlap]),
            worker("rmux:%2", vec![WorkerFlag::PossibleOverlap]),
        ])) else {
            panic!("overlap alone is informational");
        };
        assert!(message.contains("can be deliberate"), "{message}");
    }

    /// A shell rc autostart is the supported setup without systemd.
    #[test]
    fn a_shellrc_autostart_is_healthy_without_systemd() {
        let rc = PathBuf::from("/home/u/.zshrc");
        let found = shellrc_autostart_verdict(Some(&rc));
        assert!(matches!(found, CheckResult::Ok(_)));
        let CheckResult::Warn(missing) = shellrc_autostart_verdict(None) else {
            panic!("no autostart at all is a warning");
        };
        assert!(
            missing.contains("tend init --component tendd-shellrc"),
            "{missing}"
        );
    }

    #[test]
    fn health_verdicts_separate_recovery_scope_and_selective_housekeeping() {
        let mut health = tend::collaboration::CollaborationHealth::default();
        assert!(health_verdicts(&health)
            .iter()
            .all(|r| matches!(r, CheckResult::Ok(_))));
        health.interrupted_requests = 15;
        health.binding_unavailable = 15;
        health.pending_recovery_decisions = 11;
        health.example_request_ids = vec!["original-request".into()];
        health.stale_queued = 2;
        health.stale_quiet_notices = 1;
        health.example_quiet_notice_ids = vec!["quiet-notice".into()];
        health.synthetic_agents.unobserved_backend.count = 2;
        health.synthetic_agents.unobserved_backend.examples =
            vec!["tmux.sock:%3".into(), "tmux.sock:%7".into()];
        let results = health_verdicts(&health);
        let lines = results.iter().map(message).collect::<Vec<_>>();
        assert!(lines[0].contains("daemon does not observe"));
        assert!(!lines[0].contains("trigger"));
        assert!(lines[1].contains("15 interrupted original"));
        assert!(lines[1].contains("11 pending recovery"));
        assert!(lines[2].contains("--quiet-only --request-id REQUEST --dry-run"));
        assert!(!lines[2].contains("close with"));
        health.stale_quiet_notices = 0;
        let next = health_verdicts(&health);
        assert!(!message(next.last().unwrap()).contains("--quiet-only"));
        let orphan = tend::collaboration::CollaborationHealth {
            pending_recovery_decisions: 1,
            ..Default::default()
        };
        assert!(health_verdicts(&orphan).iter().any(
            |r| matches!(r, CheckResult::Warn(_)) && message(r).contains("1 pending recovery")
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn legacy_health_is_unknown_not_green() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("legacy-health.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            for _ in 0..2 {
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&line).unwrap()["kind"],
                    "hello"
                );
                reader
                    .get_mut()
                    .write_all(b"{\"ok\":true,\"protocol\":6,\"capabilities\":[]}\n")
                    .await
                    .unwrap();
            }
        });
        let results = check_collaboration(&socket).await;
        assert!(results
            .iter()
            .any(|r| matches!(r, CheckResult::Warn(_)) && message(r).contains("health_v1")));
        assert!(!results
            .iter()
            .any(|r| message(r).contains("no interrupted")));
        server.await.unwrap();
    }

    /// A server the user owns: their `~/.tmux.conf` was read, and a
    /// terminal client is showing it.
    fn user_owned_server() -> TmuxEnvironment {
        TmuxEnvironment {
            socket: Some("default".into()),
            surface: ClientSurface::Terminal,
            config_files: vec!["/Users/x/.tmux.conf".into()],
        }
    }

    /// A front-end-owned server, as amux starts one:
    /// `tmux -f /dev/null -L amux -CC`.
    fn front_end_owned_server() -> TmuxEnvironment {
        TmuxEnvironment {
            socket: Some("amux".into()),
            surface: ClientSurface::ControlMode,
            config_files: vec!["/dev/null".into()],
        }
    }

    /// cmux: the user's own `~/.tmux.conf` server, driven by a front-end
    /// that attaches no client.
    fn viewer_driven_server() -> TmuxEnvironment {
        TmuxEnvironment {
            socket: Some("default".into()),
            surface: ClientSurface::Detached,
            config_files: vec!["/Users/x/.tmux.conf".into()],
        }
    }

    fn message(result: &CheckResult) -> &str {
        match result {
            CheckResult::Ok(m) | CheckResult::Warn(m) | CheckResult::Fail(m) => m,
        }
    }

    fn is_ok(result: &CheckResult) -> bool {
        matches!(result, CheckResult::Ok(_))
    }

    #[test]
    fn a_control_mode_client_is_reported_with_the_way_around_it() {
        let msg = message(&front_end_owned_server().describe()).to_string();
        assert!(msg.contains("-CC"), "{msg}");
        // What still works has to be in the line: the prefix popups do not,
        // the commands do, and peek answers in text rather than not at all.
        assert!(msg.contains("as commands"), "{msg}");
        assert!(msg.contains("per-pane report"), "{msg}");
    }

    #[test]
    fn a_terminal_client_is_reported_as_able_to_draw() {
        let result = user_owned_server().describe();
        assert!(is_ok(&result));
        assert!(message(&result).contains("overlays render"));
    }

    #[test]
    fn a_server_with_no_client_explains_why_prefix_keys_do_nothing() {
        // The cmux symptom the user actually hits: the binding is right
        // there in `list-keys` and pressing it does nothing, because tmux
        // resolves bindings per client and there is no client.
        let msg = message(&viewer_driven_server().describe()).to_string();
        assert!(msg.contains("nothing is attached"), "{msg}");
        assert!(msg.contains("key bindings per client"), "{msg}");
        assert!(msg.contains("no current client"), "{msg}");
    }

    #[test]
    fn an_installed_binding_no_viewer_can_run_is_not_reported_as_working() {
        // The file is correct, so this is not a failure — but claiming
        // "prefix+s opens tend watch" would contradict the line above it.
        let binding = Some("bind-key -T prefix s display-popup -BE -w \"100%\" \"tend watch\"");
        let result = watch_binding_verdict(binding, &viewer_driven_server());
        assert!(is_ok(&result));
        let msg = message(&result);
        assert!(!msg.contains("prefix+s opens"), "{msg}");
        assert!(msg.contains("no viewer"), "{msg}");
    }

    #[test]
    fn a_missing_binding_on_an_isolated_server_is_not_an_issue() {
        // `tend init` writes ~/.tmux.conf and this server never opens it,
        // so neither that nor `tmux source-file` is the fix — and sourcing
        // it there is what the front-end isolates against.
        let result = watch_binding_verdict(None, &front_end_owned_server());
        assert!(
            is_ok(&result),
            "a front-end-owned server is not a broken install: {}",
            message(&result)
        );
        let msg = message(&result);
        assert!(!msg.contains("tend init"), "{msg}");
        assert!(!msg.contains("source-file"), "{msg}");
    }

    #[test]
    fn a_missing_binding_on_the_users_own_server_still_points_at_init() {
        let result = watch_binding_verdict(None, &user_owned_server());
        assert!(matches!(result, CheckResult::Warn(_)));
        assert!(message(&result).contains("tend init"));
    }

    #[test]
    fn a_live_full_client_binding_is_green_on_any_server() {
        let binding = Some("bind-key -T prefix s display-popup -BE -w \"100%\" \"tend watch\"");
        assert!(is_ok(&watch_binding_verdict(binding, &user_owned_server())));
    }

    #[test]
    fn an_unset_socket_pin_is_harmless_when_tendd_is_on_the_default_path() {
        let default = tend::paths::default_socket();
        let result = tend_socket_unset(&default, &front_end_owned_server());
        assert!(
            is_ok(&result),
            "panes fall back to exactly this path: {}",
            message(&result)
        );
        assert!(!message(&result).contains("tend init"));
    }

    #[test]
    fn an_unset_socket_pin_bites_when_tendd_moved_off_the_default_path() {
        let moved = PathBuf::from("/tmp/tend-elsewhere.sock");
        let result = tend_socket_unset(&moved, &front_end_owned_server());
        assert!(matches!(result, CheckResult::Warn(_)));
        let msg = message(&result);
        // The pin cannot come from ~/.tmux.conf here, so the remedy is a
        // `set-environment` against the server that is actually running.
        assert!(msg.contains("set-environment"), "{msg}");
        assert!(!msg.contains("tend init"), "{msg}");
    }

    /// `tend init` writes no pin for the default socket, so on the user's
    /// own server too a missing pin is fine; only a moved socket needs init.
    #[test]
    fn an_unset_socket_pin_on_the_users_own_server_needs_init_only_off_default() {
        let result = tend_socket_unset(&tend::paths::default_socket(), &user_owned_server());
        assert!(is_ok(&result), "{}", message(&result));
        assert!(!message(&result).contains("tend init"));

        let moved = PathBuf::from("/tmp/tend-elsewhere.sock");
        let result = tend_socket_unset(&moved, &user_owned_server());
        assert!(matches!(result, CheckResult::Warn(_)));
        assert!(
            message(&result).contains("tend init"),
            "{}",
            message(&result)
        );
    }

    #[test]
    fn parse_tend_socket_env_extracts_value() {
        assert_eq!(
            parse_tend_socket_env("TEND_SOCKET=/tmp/tend-1044.sock\n").as_deref(),
            Some("/tmp/tend-1044.sock")
        );
    }

    #[test]
    fn parse_tend_socket_env_none_when_unset_or_empty() {
        // tmux prints `-TEND_SOCKET` when the global var is removed.
        assert_eq!(parse_tend_socket_env("-TEND_SOCKET\n"), None);
        assert_eq!(parse_tend_socket_env("TEND_SOCKET=\n"), None);
        assert_eq!(parse_tend_socket_env(""), None);
    }

    #[test]
    fn socket_is_reachable_true_when_listener_bound() {
        use std::os::unix::net::UnixListener;
        let path =
            std::env::temp_dir().join(format!("tend-doctor-probe-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind probe socket");
        assert!(socket_is_reachable(&path));
        drop(listener);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn socket_is_reachable_false_when_path_missing() {
        // A pinned-but-dead path is exactly the stale-TEND_SOCKET case.
        let path =
            std::env::temp_dir().join(format!("tend-doctor-missing-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        assert!(!socket_is_reachable(&path));
    }

    #[test]
    fn claude_hook_configured_detects_tend_entry() {
        // Settings shaped like the wizard would write — single
        // `SessionStart` event with a tend command embedded.
        let json = r#"{
          "hooks": {
            "SessionStart": [
              { "hooks": [{ "type": "command", "command": "tend hook claude --event session_start" }] }
            ]
          }
        }"#;
        assert!(claude_hook_configured(json));
    }

    #[test]
    fn claude_hook_configured_returns_false_when_only_user_hooks_present() {
        let json = r#"{
          "hooks": {
            "SessionStart": [
              { "hooks": [{ "type": "command", "command": "echo hi" }] }
            ]
          }
        }"#;
        assert!(!claude_hook_configured(json));
    }

    #[test]
    fn claude_hook_configured_handles_malformed_json() {
        // Garbage in must not panic — bare `false` is the right signal.
        assert!(!claude_hook_configured("not even json {"));
        assert!(!claude_hook_configured(""));
    }

    /// The block `files::antigravity::upsert` writes is what "configured"
    /// means, and both event shapes in it have to be recognized.
    #[test]
    fn antigravity_hook_configured_accepts_the_written_block() {
        let written = crate::init::files::antigravity::upsert("").unwrap().0;
        assert!(antigravity_hook_configured(&written));

        // Split across two named keys by hand, still complete.
        let split = r#"{
            "tend-lifecycle": {
                "SessionStart":   [{ "type": "command", "command": "tend hook agy --event session_start" }],
                "PreInvocation":  [{ "type": "command", "command": "tend hook agy --event pre_invocation" }],
                "PostInvocation": [{ "type": "command", "command": "tend hook agy --event post_invocation" }],
                "Stop":           [{ "type": "command", "command": "tend hook agy --event stop" }]
            },
            "tend-tools": {
                "PreToolUse":  [{ "matcher": "*", "hooks": [{ "type": "command", "command": "tend hook agy --event pre_tool_use" }] }],
                "PostToolUse": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "tend hook agy --event post_tool_use" }] }]
            }
        }"#;
        assert!(
            antigravity_hook_configured(split),
            "coverage is what matters, not which key holds it",
        );
    }

    /// A block left over from an older tend covers only some events. Calling
    /// that healthy is the worst outcome: the row exists but never shows a
    /// prompt or a tool, and nothing points at the fix.
    #[test]
    fn antigravity_hook_configured_rejects_a_partial_block() {
        let stale = r#"{"tend":{"Stop":[
            { "type": "command", "command": "tend hook agy --event stop" }
        ]}}"#;
        assert!(!antigravity_hook_configured(stale));

        // Everything but `PreToolUse` — one missing event is still broken.
        let mut v: serde_json::Value =
            serde_json::from_str(&crate::init::files::antigravity::upsert("").unwrap().0).unwrap();
        v["tend"].as_object_mut().unwrap().remove("PreToolUse");
        assert!(!antigravity_hook_configured(&v.to_string()));
    }

    /// Someone else's hooks in the same file must not read as ours — that is
    /// the whole point of walking the structure instead of substring-matching.
    #[test]
    fn antigravity_hook_configured_rejects_foreign_hooks() {
        let other = r#"{"lint-checker":{"PostToolUse":[
            { "matcher": "run_command", "hooks": [{ "type": "command", "command": "./lint.sh" }] }
        ]}}"#;
        assert!(!antigravity_hook_configured(other));
        // The Gemini CLI's file shape must not satisfy the agy check either:
        // that is the exact mix-up this whole adapter exists to prevent.
        let gemini_shaped = r#"{"hooks":{"BeforeAgent":[
            { "hooks": [{ "type": "command", "command": "tend hook gemini --event before_agent" }] }
        ]}}"#;
        assert!(!antigravity_hook_configured(gemini_shaped));
        assert!(!antigravity_hook_configured("{ not json"));
        assert!(!antigravity_hook_configured("[]"));
    }

    #[test]
    fn gemini_hook_configured_walks_event_dict() {
        let json = r#"{
          "hooks": {
            "BeforeAgent": [
              { "hooks": [{ "type": "command", "command": "tend hook gemini --event before_agent" }] }
            ]
          }
        }"#;
        assert!(gemini_hook_configured(json));
    }

    #[test]
    fn codex_hook_configured_reads_array_of_tables() {
        let toml = r#"
[[hooks.SessionStart]]
  hooks = [{ type = "command", command = "tend hook codex --event session_start" }]
"#;
        assert!(codex_hook_configured(toml));
    }

    #[test]
    fn codex_hook_configured_false_for_unrelated_command() {
        let toml = r#"
[[hooks.SessionStart]]
  hooks = [{ type = "command", command = "echo unrelated" }]
"#;
        assert!(!codex_hook_configured(toml));
    }

    /// What tmux actually prints for the managed binding: short flags
    /// arrive merged (`-BE`) and the percentages come back quoted.
    const LIVE_MANAGED: &str = r#"bind-key    -T prefix s       display-popup -BE -h "100%" -w "100%" -x 0 -y 0 "tend watch""#;
    /// What tmux prints for the run-shell-wrapped managed binding: the
    /// popup flags live inside the escaped inner string, but they are
    /// still whitespace-separated tokens of the one list-keys line.
    /// The managed block today: tend opens and sizes the popup itself.
    const LIVE_SELF_SIZED: &str = r#"bind-key    -T prefix       s                         run-shell -b "tend watch --popup --caller-client '#{client_name}' --caller-pane '#{pane_id}'""#;
    const LIVE_WRAPPED: &str = r#"bind-key    -T prefix s       run-shell -b "tmux display-popup -c '#{client_name}' -B -E -w 100% -h 100% -x 0 -y 0 "tend watch --caller-client '#{client_name}' --caller-pane '#{pane_id}'"""#;

    #[test]
    fn the_wrapped_binding_still_reads_as_full_client() {
        assert!(popup_binding_is_full_client(LIVE_WRAPPED));
        assert!(popup_binding_is_full_client(LIVE_SELF_SIZED));
        assert_eq!(watch_popup_binding(LIVE_WRAPPED), Some(LIVE_WRAPPED));
    }

    /// The pre-0.8.25 hand-written binding this check exists to catch.
    const LIVE_INSET: &str =
        r#"bind-key    -T prefix s       display-popup -E -h "85%" -w "90%" "tend watch""#;

    #[test]
    fn full_client_popup_binding_is_accepted() {
        assert!(popup_binding_is_full_client(LIVE_MANAGED));
    }

    #[test]
    fn inset_popup_binding_is_rejected() {
        // 90% of 134 columns is 120, and the border spends 2 more — one
        // short of the inspector, which is the whole bug.
        assert!(!popup_binding_is_full_client(LIVE_INSET));
    }

    #[test]
    fn full_width_without_borderless_is_still_rejected() {
        let bordered = r#"bind-key -T prefix s display-popup -E -h "100%" -w "100%" "tend watch""#;
        assert!(!popup_binding_is_full_client(bordered));
    }

    #[test]
    fn borderless_without_full_width_is_still_rejected() {
        let narrow = r#"bind-key -T prefix s display-popup -BE -h "100%" -w "90%" "tend watch""#;
        assert!(!popup_binding_is_full_client(narrow));
    }

    #[test]
    fn watch_binding_is_found_among_other_keys() {
        let listing = format!(
            "bind-key -T prefix D display-popup -E -w \"95%\" \"tend dashboard\"\n{LIVE_MANAGED}\nbind-key -T prefix q display-popup -BE \"tend peek\"\n"
        );
        assert_eq!(watch_popup_binding(&listing), Some(LIVE_MANAGED));
    }

    #[test]
    fn watch_binding_absent_when_nothing_runs_watch() {
        let listing = "bind-key -T prefix c new-window\nbind-key -T prefix D display-popup -E \"tend dashboard\"\n";
        assert_eq!(watch_popup_binding(listing), None);
    }

    #[test]
    fn tmux_marker_block_detected_finds_complete_block() {
        let conf = "set -g mouse on\n# >>> tend managed (tmux-popup) >>>\nbind-key s display-popup\n# <<< tend managed (tmux-popup) <<<\n";
        assert!(tmux_marker_block_detected(conf, "tmux-popup"));
    }

    #[test]
    fn tmux_marker_block_detected_requires_close_fence() {
        // Open fence without the corresponding close — must NOT match.
        let conf = "# >>> tend managed (tmux-popup) >>>\nbind-key s display-popup\n";
        assert!(!tmux_marker_block_detected(conf, "tmux-popup"));
    }

    #[test]
    fn tmux_marker_block_detected_ignores_quoted_substring() {
        // Defensive: a literal fence string inside a quoted status-right
        // value must not be confused for a real fence.
        let conf = "set -g status-right \"# >>> tend managed (tmux-popup) >>> not a fence\"\n";
        assert!(!tmux_marker_block_detected(conf, "tmux-popup"));
    }

    #[test]
    fn extract_recent_errors_filters_and_caps() {
        let log = [
            "2026-04-30T10:00 INFO  starting tendd",
            "2026-04-30T10:01 ERROR  socket bind failed",
            "2026-04-30T10:02 INFO  retrying",
            "2026-04-30T10:03 panic in worker thread",
            "2026-04-30T10:04 INFO  recovered",
            "2026-04-30T10:05 ERROR  another problem",
        ]
        .join("\n");
        let warnings = extract_recent_errors(&log, 20, 3);
        assert_eq!(warnings.len(), 3);
        assert!(warnings[0].contains("socket bind failed"));
        assert!(warnings[1].contains("panic"));
        assert!(warnings[2].contains("another problem"));
    }

    #[test]
    fn extract_recent_errors_only_scans_tail_window() {
        // Old ERROR is outside the tail window and should be skipped.
        let mut lines: Vec<String> = (0..30).map(|i| format!("INFO step {i}")).collect();
        lines[1] = "ERROR ancient".to_string();
        lines[28] = "ERROR fresh".to_string();
        let log = lines.join("\n");
        let warnings = extract_recent_errors(&log, 5, 3);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("fresh"));
    }

    #[test]
    fn extract_recent_errors_clean_log_returns_empty() {
        let log = "INFO ok\nINFO ok\n";
        assert!(extract_recent_errors(log, 20, 3).is_empty());
    }

    #[test]
    fn format_warning_line_renders_with_prefix() {
        // Trailing whitespace stripped, prefix added.
        let s = format_warning_line("  ERROR something went wrong  \n");
        assert_eq!(s, "tendd log — ERROR something went wrong");
    }

    #[test]
    fn format_warning_line_caps_long_lines() {
        // Use a marker char that's not present in the "tendd log — "
        // prefix so the count is unambiguous.
        let huge = "Z".repeat(500);
        let s = format_warning_line(&huge);
        // Body should be capped at 200 chars; prefix has zero Zs.
        assert_eq!(s.chars().filter(|c| *c == 'Z').count(), 200);
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn doctor_reports_a_disabled_mailbox_while_registry_is_available() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("disabled-mailbox.sock");
        let mailbox = tend::collaboration::CollaborationStore::unavailable(
            tend::collaboration::CollaborationOptions::default(),
            "corrupt durable mailbox".into(),
        );
        let server = tend::ipc::Server::new(socket.clone(), tend::Store::shared())
            .with_collaboration(mailbox);
        let (shutdown, rx) = tokio::sync::broadcast::channel(1);
        let task = tokio::spawn(async move { server.run(rx).await.unwrap() });
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::task::yield_now().await;
        }
        let result = check_tendd(&socket).await;
        shutdown.send(()).unwrap();
        task.await.unwrap();
        match result {
            CheckResult::Fail(message) => assert!(message.contains("corrupt durable mailbox")),
            _ => panic!("expected an explicit collaboration diagnostic"),
        }
    }

    /// A daemon that answers with two failures reports both: a corrupt
    /// mailbox does not hide why its swap service did not open.
    #[tokio::test]
    async fn a_mailbox_failure_does_not_hide_the_swap_reason() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("two-failures.sock");
        let mailbox = tend::collaboration::CollaborationStore::unavailable(
            tend::collaboration::CollaborationOptions::default(),
            "corrupt durable mailbox".into(),
        );
        let unopened: std::sync::Arc<dyn tend::swap_service::SwapJobs> =
            std::sync::Arc::new(tend::swap_service::UnopenedSwap::new(
                "swap jobs are owned by another live manager (pid 7)".into(),
            ));
        let server = tend::ipc::Server::new(socket.clone(), tend::Store::shared())
            .with_collaboration(mailbox)
            .with_swap(Some(unopened));
        let (shutdown, rx) = tokio::sync::broadcast::channel(1);
        let task = tokio::spawn(async move { server.run(rx).await.unwrap() });
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::task::yield_now().await;
        }
        let (daemon, responded) = probe_tendd(&socket).await;
        let swap = check_swap(&socket).await;
        shutdown.send(()).unwrap();
        task.await.unwrap();
        assert!(matches!(&daemon, CheckResult::Fail(m) if m.contains("corrupt durable mailbox")));
        assert!(responded, "the daemon answered");
        assert!(
            matches!(&swap, CheckResult::Warn(m) if m.contains("another live manager")),
            "{}",
            match &swap {
                CheckResult::Ok(t) | CheckResult::Warn(t) | CheckResult::Fail(t) => t,
            }
        );
        // Nobody listening: not answered, so no swap probe is made.
        let (_, responded) = probe_tendd(&dir.path().join("nobody.sock")).await;
        assert!(!responded);
    }
}
