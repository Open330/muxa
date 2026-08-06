//! `muxa doctor` — end-to-end diagnostic.
//!
//! Answers the question "is muxa working correctly?" without making the
//! user know any paths or service-manager incantations. Each check
//! prints exactly one line with a `✔ / ✗ / ⚠` glyph and an actionable
//! hint when it fails. The output is cliclack-styled so it matches
//! `muxa init`'s look.
//!
//! Why this lives in its own module instead of reusing
//! `init::verify`: verify is post-apply (one shot, narrow scope),
//! whereas doctor is invocable any time and covers the whole install
//! surface — service manager, hooks, tmux, recent errors. Sharing the
//! IPC ping is the only meaningful overlap and that's a single
//! `Client::snapshot()` call.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use muxa::ipc::Client;
use muxa::state::SYNTHETIC_SESSION_PREFIX;
use muxa::Agent;
use tokio::time::timeout;

use crate::init::files::{claude, codex, gemini, launchd, systemd as systemd_files};

/// Marker block ids the `muxa init` tmux installer writes — must match
/// the constants in `init::components::Component::id`. Duplicated as
/// string literals to keep `doctor` from depending on the components
/// module's enum surface (which is large and unrelated).
const TMUX_BLOCK_POPUP: &str = "tmux-popup";
const TMUX_BLOCK_STATUSLINE: &str = "tmux-statusline";

/// macOS launchd label used by the `MuxadLaunchd` install component.
const LAUNCHD_LABEL: &str = "dev.open330.muxad";

/// systemd unit name installed by the `MuxadSystemd` component.
const SYSTEMD_UNIT: &str = "muxad";

/// Path to muxad's stderr log on macOS+Linux (the launchd plist and the
/// systemd unit both redirect there). Reading is best-effort — a
/// missing file just means the daemon hasn't logged anything to disk.
const MUXAD_ERR_LOG: &str = "/tmp/muxad.err";

/// Run the diagnostic and print results to stdout. Returns `Ok(())`
/// regardless of how many checks fail — `muxa doctor` is informational,
/// not a gate. The exit code stays 0 so users can run it from scripts
/// without conditionals.
///
/// `socket` is the resolved daemon socket path — typically what `main.rs`
/// computes from `--socket` / `MUXA_SOCKET` / `cfg.socket` / the default.
/// Threading it in (rather than re-deriving from defaults here) keeps
/// `muxa doctor` honest about which socket muxa would actually talk to.
pub async fn run(socket: PathBuf) -> Result<()> {
    let _ = cliclack::intro("muxa doctor");

    let mut issues = 0u32;

    // 1. muxad responsive over IPC.
    tally(check_muxad(&socket).await, &mut issues);

    // 1½. Agents seen via discovery but not yet correlated with a hook.
    //      Skipped silently when muxad is unreachable — `check_muxad`
    //      already owns and reported that condition.
    if let Some(snapshot) = fetch_snapshot(&socket).await {
        tally(check_synthetic_agents(&snapshot), &mut issues);
    }

    // 2. Service manager status (per-OS).
    tally(check_service_manager(), &mut issues);

    // 3. Service manager unit file on disk.
    tally(check_unit_file(), &mut issues);

    // 4. Per-agent hook configuration. Label-prefixed, so it folds
    //    its own message before handing off to `tally`.
    for (label, result) in check_agent_hooks() {
        let labelled = match result {
            CheckResult::Ok(m) => CheckResult::Ok(format!("{label} — {m}")),
            CheckResult::Warn(m) => CheckResult::Warn(format!("{label} — {m}")),
            CheckResult::Fail(m) => CheckResult::Fail(format!("{label} — {m}")),
        };
        tally(labelled, &mut issues);
    }

    // 5. tmux marker blocks.
    tally(check_tmux_blocks(), &mut issues);

    // 5¼. …and whether the binding tmux would actually run is the one
    // those blocks describe. A duplicate `bind-key` later in the file,
    // or a server that never re-read the file, leaves the block present
    // and the behaviour stale.
    tally(check_tmux_watch_binding(), &mut issues);

    // 5½. MUXA_SOCKET env — the variable that lets every pane agree
    // on the daemon socket path after a restart.
    tally(check_muxa_socket_env(&socket), &mut issues);

    // 5¾. Where config.toml is expected to live. Informational, but the
    // path differs per OS and nothing else prints it.
    tally(check_config_file(), &mut issues);

    // 6. Recent muxad errors. These are surfaced as warnings (not
    //    failures) — a stale ERROR line from yesterday shouldn't make
    //    the whole doctor run look red.
    let warnings = recent_muxad_errors(MUXAD_ERR_LOG, 20, 3);
    if warnings.is_empty() {
        log_ok("Recent muxad log clean");
    } else {
        for line in &warnings {
            log_warn(&format_warning_line(line));
        }
        issues += u32::try_from(warnings.len()).unwrap_or(u32::MAX);
    }

    // Summary footer matches the cliclack box style; outro renders
    // the closing └ glyph.
    // Point at the per-check guidance printed above rather than asserting a
    // blanket `muxa init`/`upgrade` remedy — several warnings (e.g. "restart
    // the agents to enable hook events") are not fixed by either command.
    let summary = if issues == 0 {
        "All clean ✓".to_string()
    } else if issues == 1 {
        "1 issue — see the ⚠/✗ line above for its specific fix".to_string()
    } else {
        format!("{issues} issues — see the ⚠/✗ lines above for their specific fixes")
    };
    let _ = cliclack::outro(summary);
    Ok(())
}

/// Internal status — every check folds into one of these so the
/// `run()` body stays uniform.
enum CheckResult {
    Ok(String),
    Warn(String),
    Fail(String),
}

fn log_ok(msg: &str) {
    let _ = cliclack::log::success(format!("✔ {msg}"));
}

fn log_warn(msg: &str) {
    let _ = cliclack::log::warning(format!("⚠ {msg}"));
}

fn log_fail(msg: &str) {
    let _ = cliclack::log::error(format!("✗ {msg}"));
}

/// Fold a uniform check into the running issue count + log line.
/// One call site per check keeps `run()` readable and removes the
/// Ok/Warn/Fail boilerplate that used to repeat for every check.
fn tally(result: CheckResult, issues: &mut u32) {
    match result {
        CheckResult::Ok(msg) => log_ok(&msg),
        CheckResult::Warn(msg) => {
            log_warn(&msg);
            *issues += 1;
        }
        CheckResult::Fail(msg) => {
            log_fail(&msg);
            *issues += 1;
        }
    }
}

/// IPC ping with a 1.5 s budget — same window `init::verify` uses.
/// Cold-started muxad answers a snapshot in <50 ms; anything past 1.5 s
/// is a real problem worth surfacing.
async fn check_muxad(socket: &Path) -> CheckResult {
    let client = Client::new(socket.to_path_buf());
    match timeout(Duration::from_millis(1500), client.snapshot()).await {
        Ok(Ok(_)) => CheckResult::Ok(format!("muxad responding at {}", socket.display())),
        // The inner error already carries the failure mode — including
        // socket path, connection refused vs. RPC mismatch, etc. The
        // previous wording ("reachable but errored") was misleading when
        // the inner error was actually "daemon not reachable", which is
        // the most common cause. Surface the underlying message and add
        // a single concrete next step.
        Ok(Err(e)) => CheckResult::Fail(format!("{e} — run `muxa init` to repair")),
        Err(_) => CheckResult::Fail(format!(
            "muxad not responding within 1.5 s at {} — start `muxad` or run `muxa init`",
            socket.display()
        )),
    }
}

/// One-shot snapshot fetch shared by the post-IPC checks. Returns
/// `None` when muxad is unreachable so callers can skip rather than
/// double-reporting the daemon-down condition `check_muxad` owns.
async fn fetch_snapshot(socket: &Path) -> Option<Vec<Agent>> {
    let client = Client::new(socket.to_path_buf());
    match timeout(Duration::from_millis(1500), client.snapshot()).await {
        Ok(Ok(s)) => Some(s),
        _ => None,
    }
}

/// Flag agents that still exist as discovery placeholders. A `synthetic-*`
/// session id means muxa saw an agent process in a pane but has not correlated
/// a real hook session with that pane yet. The hook can be completely absent,
/// or it can be arriving as a separate paneless row; the snapshot alone cannot
/// safely distinguish those cases, so the guidance covers both without
/// claiming that no hook events were received.
fn check_synthetic_agents(snapshot: &[Agent]) -> CheckResult {
    let panes = synthetic_panes(
        snapshot
            .iter()
            .map(|a| (a.session_id.as_str(), a.pane.as_deref())),
    );
    if panes.is_empty() {
        return CheckResult::Ok("Agent hooks — all tracked agents reporting".into());
    }
    CheckResult::Warn(format!(
        "{} pane agent(s) not yet correlated with hook events ({}) — trigger a \
         new agent event. If they still remain, use `muxa watch --include-paneless` \
         to check for pane-correlation failure; otherwise restart the agent or \
         review `/hooks` inside Codex.",
        panes.len(),
        panes.join(", ")
    ))
}

/// Pure core of [`check_synthetic_agents`], split out so the
/// classification is unit-testable without constructing the heavy
/// `Agent` struct. Returns the sorted, de-duplicated panes whose only
/// record is a synthetic discovery placeholder.
fn synthetic_panes<'a>(agents: impl Iterator<Item = (&'a str, Option<&'a str>)>) -> Vec<&'a str> {
    let mut panes: Vec<&str> = agents
        .filter(|(sid, _)| sid.starts_with(SYNTHETIC_SESSION_PREFIX))
        .map(|(_, pane)| pane.unwrap_or("?"))
        .collect();
    panes.sort_unstable();
    panes.dedup();
    panes
}

/// Branch on the host OS: launchd on macOS, systemd on Linux. Other
/// targets get a neutral note rather than a red ✗ — muxa runs on BSDs
/// and WSL1 too, where neither manager applies.
fn check_service_manager() -> CheckResult {
    match std::env::consts::OS {
        "macos" => check_launchctl(),
        "linux" => check_systemctl(),
        other => CheckResult::Warn(format!(
            "Service manager — no per-OS check for {other} (start muxad manually)"
        )),
    }
}

fn check_launchctl() -> CheckResult {
    let uid = uid_string();
    let target = format!("gui/{uid}/{LAUNCHD_LABEL}");
    let out = Command::new("launchctl").args(["print", &target]).output();
    match out {
        Ok(o) if o.status.success() => CheckResult::Ok("launchd: muxad agent loaded".into()),
        Ok(_) => CheckResult::Fail(
            "launchd: muxad agent not loaded — run `muxa init --component muxad-launchd`"
                .to_string(),
        ),
        Err(e) => CheckResult::Warn(format!("launchd: could not run launchctl ({e})")),
    }
}

fn check_systemctl() -> CheckResult {
    // Gate on actual bus reachability, not just binary presence: in
    // Docker without `--privileged` (and other non-systemd-init hosts)
    // systemctl exists but the user bus is gone, so any `is-active`
    // probe is meaningless. `muxa init` already falls back to the
    // shellrc autostart there, and doctor should match that posture.
    if !systemd_files::systemd_available() {
        return CheckResult::Warn(
            "systemd: user manager not usable on this host — relying on shellrc autostart".into(),
        );
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
                CheckResult::Ok("systemd: muxad.service active".into())
            } else {
                CheckResult::Fail(format!(
                    "systemd: muxad.service is {state} — run `systemctl --user enable --now muxad`"
                ))
            }
        }
        Err(e) => CheckResult::Warn(format!("systemd: could not run systemctl ({e})")),
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
                "Unit file missing: {} — run `muxa init`",
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
                return CheckResult::Warn(
                    "Service unit — systemd user manager not usable, no unit needed here".into(),
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
            "Unit file missing: {} — run `muxa init`",
            p.display()
        )),
        _ => CheckResult::Warn("Service unit — could not resolve home directory".to_string()),
    }
}

/// Run the per-agent hook check for Claude, Codex, and Gemini. Each
/// returns `(label, result)` so `run()` can log them with a consistent
/// `Hooks · <agent>` prefix.
fn check_agent_hooks() -> Vec<(String, CheckResult)> {
    vec![
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
    ]
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
            "{} missing — run `muxa init --component {agent}-hooks`",
            path.display()
        ));
    }
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            if is_configured(&text) {
                CheckResult::Ok(format!("hook installed in {}", path.display()))
            } else {
                CheckResult::Fail(format!(
                    "no `muxa hook {agent}` entry in {} — run `muxa init`",
                    path.display()
                ))
            }
        }
        Err(e) => CheckResult::Fail(format!("could not read {}: {e}", path.display())),
    }
}

/// True iff Claude's settings.json contains at least one hook entry
/// whose command starts with `muxa hook claude`. We do a structural
/// walk (not a substring search) because users do legitimately put the
/// literal "muxa hook claude" inside other strings — e.g. comments in
/// adjacent fields — and we don't want false positives.
pub fn claude_hook_configured(settings_text: &str) -> bool {
    json_hooks_dict_has_command(settings_text, "muxa hook claude")
}

/// True iff Gemini's settings.json contains at least one hook entry
/// whose command starts with `muxa hook gemini`. Same JSON shape as
/// Claude.
pub fn gemini_hook_configured(settings_text: &str) -> bool {
    json_hooks_dict_has_command(settings_text, "muxa hook gemini")
}

/// True iff Codex's `config.toml` contains at least one hook entry
/// whose command starts with `muxa hook codex`. The TOML shape is
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
        aot.iter().any(toml_table_has_muxa_command)
    });
    found
}

/// Walk a single `[[hooks.<Event>]]` table looking for any inner
/// `command` string starting with `muxa hook codex`.
fn toml_table_has_muxa_command(t: &toml_edit::Table) -> bool {
    let Some(inner) = t.get("hooks") else {
        return false;
    };
    match inner {
        toml_edit::Item::Value(toml_edit::Value::Array(arr)) => arr.iter().any(|v| match v {
            toml_edit::Value::InlineTable(it) => it
                .get("command")
                .and_then(toml_edit::Value::as_str)
                .is_some_and(|c| c.trim_start().starts_with("muxa hook codex")),
            _ => false,
        }),
        toml_edit::Item::ArrayOfTables(aot) => aot.iter().any(|inner| {
            inner
                .get("command")
                .and_then(toml_edit::Item::as_str)
                .is_some_and(|c| c.trim_start().starts_with("muxa hook codex"))
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

/// Report where muxa reads `config.toml` from, and whether it is there.
///
/// A missing file is not a problem — every setting has a compiled default
/// and `muxa init` only writes the file for the components that need it.
/// The problem is *finding* it: `dirs::config_dir()` is
/// `~/Library/Application Support` on macOS and `~/.config` on Linux, so
/// "add `[collaboration]` to `~/.config/muxa/config.toml`" is wrong half
/// the time and the edit lands in a file nothing reads. Printing the
/// resolved path costs one line and removes the guesswork.
fn check_config_file() -> CheckResult {
    let Some(path) = muxa::paths::default_config_file() else {
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
fn check_tmux_watch_binding() -> CheckResult {
    let out = muxa::tmux::tmux_command()
        .args(["list-keys", "-T", "prefix"])
        .output();
    let text = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        // No server running yet is not a misconfiguration — there is
        // simply nothing to compare against.
        Ok(_) => {
            return CheckResult::Ok("tmux: no running server to check bindings against".into())
        }
        Err(e) => return CheckResult::Warn(format!("tmux: could not list keys ({e})")),
    };
    match watch_popup_binding(&text) {
        None => CheckResult::Warn(
            "tmux: no prefix key runs `muxa watch` — run `muxa init --component tmux-popup`, then `tmux source-file ~/.tmux.conf`".into(),
        ),
        Some(binding) if popup_binding_is_full_client(binding) => {
            CheckResult::Ok("tmux: prefix+s opens muxa watch full-client".into())
        }
        Some(_) => CheckResult::Fail(
            "tmux: the live `muxa watch` binding is an inset popup, so the inspector's 120-column minimum is unreachable — a later `bind-key` in ~/.tmux.conf is overriding the managed block, or the server has not re-read it. Remove the stray binding, then `tmux source-file ~/.tmux.conf`".into(),
        ),
    }
}

/// First `list-keys` line that launches `muxa watch`, if any.
fn watch_popup_binding(list_keys_output: &str) -> Option<&str> {
    list_keys_output
        .lines()
        .find(|line| line.contains("muxa watch"))
}

/// True when the binding gives the popup the whole client with no border.
/// Both matter: `-w 90%` of a 134-column terminal is 120, and the border
/// takes 2 more, landing at 118 — one column short of the inspector for
/// the entire reason the managed block uses `-B` and `100%`.
fn popup_binding_is_full_client(binding: &str) -> bool {
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
    let Some(path) = dirs::home_dir().map(|h| h.join(".tmux.conf")) else {
        return CheckResult::Warn("tmux: could not resolve $HOME".into());
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return CheckResult::Fail(format!(
            "tmux: {} not found — run `muxa init`",
            path.display()
        ));
    };
    let popup = tmux_marker_block_detected(&text, TMUX_BLOCK_POPUP);
    let statusline = tmux_marker_block_detected(&text, TMUX_BLOCK_STATUSLINE);
    match (popup, statusline) {
        (true, true) => CheckResult::Ok("tmux: popup + statusline blocks present".into()),
        (true, false) => CheckResult::Fail(
            "tmux: statusline block missing — run `muxa init --component tmux-statusline`".into(),
        ),
        (false, true) => CheckResult::Fail(
            "tmux: popup block missing — run `muxa init --component tmux-popup`".into(),
        ),
        (false, false) => {
            CheckResult::Fail("tmux: muxa managed blocks missing — run `muxa init`".into())
        }
    }
}

/// Confirm `MUXA_SOCKET` is pinned in the tmux server environment **and**
/// that the pinned path still points at a live socket. A pin alone isn't
/// enough: if it references a daemon socket that has since moved, or a
/// temp socket that was cleaned up (e.g. a dashboard socket left behind by
/// a short-lived nested session), every new pane inherits a dead path and
/// `muxa watch` / status-right fail with "daemon not reachable". We probe
/// the path the same way the IPC client does — a plain connect — so a
/// stale pin downgrades to a warning instead of a misleading ✔.
fn check_muxa_socket_env(daemon_socket: &Path) -> CheckResult {
    let out = muxa::tmux::tmux_command()
        .args(["show-environment", "-g", "MUXA_SOCKET"])
        .output();
    let value = match out {
        Ok(o) if o.status.success() => {
            match parse_muxa_socket_env(&String::from_utf8_lossy(&o.stdout)) {
                Some(value) => value,
                None => return muxa_socket_unset(),
            }
        }
        Ok(_) => return muxa_socket_unset(),
        Err(e) => return CheckResult::Warn(format!("tmux: could not query environment ({e})")),
    };

    if socket_is_reachable(Path::new(&value)) {
        CheckResult::Ok(format!("tmux: MUXA_SOCKET={value} (reachable)"))
    } else {
        CheckResult::Warn(format!(
            "tmux: MUXA_SOCKET={value} is unreachable — new panes will fail to reach muxad; \
             re-pin it to {} with `muxa init`",
            daemon_socket.display()
        ))
    }
}

fn muxa_socket_unset() -> CheckResult {
    CheckResult::Warn(
        "tmux: MUXA_SOCKET not set — run `muxa init` to repair socket propagation".into(),
    )
}

/// Extract the value from a `tmux show-environment -g MUXA_SOCKET` line.
/// Returns `None` when the variable is unset or empty — tmux prints a
/// `-MUXA_SOCKET` removal marker (or nothing useful), neither of which
/// starts with `MUXA_SOCKET=` followed by a value.
fn parse_muxa_socket_env(stdout: &str) -> Option<String> {
    let value = stdout.trim().strip_prefix("MUXA_SOCKET=")?;
    (!value.is_empty()).then(|| value.to_string())
}

/// Best-effort liveness probe for a unix socket path. A successful connect
/// means something is listening; `ENOENT` / `ECONNREFUSED` mean the pin is
/// stale. Mirrors the connect probe in `muxa::ipc` — a local unix connect
/// resolves immediately, so no timeout is needed.
fn socket_is_reachable(path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

/// Pure detector for a `# >>> muxa managed (<id>) >>>` ... `# <<< muxa
/// managed (<id>) <<<` block in shell-comment-style config text.
///
/// Mirrors `init::marker::find_block` (private there) — copying keeps
/// us decoupled while preserving the same line-start anchoring that
/// stops false positives inside quoted strings.
pub fn tmux_marker_block_detected(haystack: &str, id: &str) -> bool {
    let open = format!("# >>> muxa managed ({id}) >>>");
    let close = format!("# <<< muxa managed ({id}) <<<");
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
fn recent_muxad_errors(path: &str, tail_lines: usize, max_warnings: usize) -> Vec<String> {
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

/// Render a single muxad-log warning line to fit under the
/// `⚠ muxad log — <line>` prefix the doctor box uses. Trims trailing
/// whitespace and collapses long lines so the box layout stays clean.
pub fn format_warning_line(raw: &str) -> String {
    let trimmed = raw.trim();
    // Cap at 200 chars — anything longer is almost certainly a stack
    // trace dump and isn't useful to surface inline.
    let capped: String = trimmed.chars().take(200).collect();
    format!("muxad log — {capped}")
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
    use super::*;

    #[test]
    fn synthetic_panes_picks_only_synthetic_sorted_deduped() {
        let rows = [
            ("real-abc", Some("%1")),
            ("synthetic-%4", Some("%4")),
            ("synthetic-%2", Some("%2")),
            ("real-def", Some("%3")),
            // Duplicate synthetic for the same pane (re-sync) collapses.
            ("synthetic-%2", Some("%2")),
        ];
        assert_eq!(
            synthetic_panes(rows.iter().map(|&(s, p)| (s, p))),
            vec!["%2", "%4"]
        );
    }

    #[test]
    fn synthetic_panes_empty_when_all_real() {
        let rows = [("real-a", Some("%1")), ("real-b", Some("%2"))];
        assert!(synthetic_panes(rows.iter().map(|&(s, p)| (s, p))).is_empty());
    }

    #[test]
    fn synthetic_panes_handles_missing_pane() {
        let rows = [("synthetic-x", None)];
        assert_eq!(
            synthetic_panes(rows.iter().map(|&(s, p)| (s, p))),
            vec!["?"]
        );
    }

    #[test]
    fn check_synthetic_agents_ok_when_none() {
        assert!(matches!(check_synthetic_agents(&[]), CheckResult::Ok(_)));
    }

    #[test]
    fn parse_muxa_socket_env_extracts_value() {
        assert_eq!(
            parse_muxa_socket_env("MUXA_SOCKET=/tmp/muxa-1044.sock\n").as_deref(),
            Some("/tmp/muxa-1044.sock")
        );
    }

    #[test]
    fn parse_muxa_socket_env_none_when_unset_or_empty() {
        // tmux prints `-MUXA_SOCKET` when the global var is removed.
        assert_eq!(parse_muxa_socket_env("-MUXA_SOCKET\n"), None);
        assert_eq!(parse_muxa_socket_env("MUXA_SOCKET=\n"), None);
        assert_eq!(parse_muxa_socket_env(""), None);
    }

    #[test]
    fn socket_is_reachable_true_when_listener_bound() {
        use std::os::unix::net::UnixListener;
        let path =
            std::env::temp_dir().join(format!("muxa-doctor-probe-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind probe socket");
        assert!(socket_is_reachable(&path));
        drop(listener);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn socket_is_reachable_false_when_path_missing() {
        // A pinned-but-dead path is exactly the stale-MUXA_SOCKET case.
        let path =
            std::env::temp_dir().join(format!("muxa-doctor-missing-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        assert!(!socket_is_reachable(&path));
    }

    #[test]
    fn claude_hook_configured_detects_muxa_entry() {
        // Settings shaped like the wizard would write — single
        // `SessionStart` event with a muxa command embedded.
        let json = r#"{
          "hooks": {
            "SessionStart": [
              { "hooks": [{ "type": "command", "command": "muxa hook claude --event session_start" }] }
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

    #[test]
    fn gemini_hook_configured_walks_event_dict() {
        let json = r#"{
          "hooks": {
            "BeforeAgent": [
              { "hooks": [{ "type": "command", "command": "muxa hook gemini --event before_agent" }] }
            ]
          }
        }"#;
        assert!(gemini_hook_configured(json));
    }

    #[test]
    fn codex_hook_configured_reads_array_of_tables() {
        let toml = r#"
[[hooks.SessionStart]]
  hooks = [{ type = "command", command = "muxa hook codex --event session_start" }]
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
    const LIVE_MANAGED: &str = r#"bind-key    -T prefix s       display-popup -BE -h "100%" -w "100%" -x 0 -y 0 "muxa watch""#;
    /// What tmux prints for the run-shell-wrapped managed binding: the
    /// popup flags live inside the escaped inner string, but they are
    /// still whitespace-separated tokens of the one list-keys line.
    const LIVE_WRAPPED: &str = r#"bind-key    -T prefix s       run-shell -b "tmux display-popup -c '#{client_name}' -B -E -w 100% -h 100% -x 0 -y 0 "muxa watch --caller-client '#{client_name}' --caller-pane '#{pane_id}'"""#;

    #[test]
    fn the_wrapped_binding_still_reads_as_full_client() {
        assert!(popup_binding_is_full_client(LIVE_WRAPPED));
        assert_eq!(watch_popup_binding(LIVE_WRAPPED), Some(LIVE_WRAPPED));
    }

    /// The pre-0.8.25 hand-written binding this check exists to catch.
    const LIVE_INSET: &str =
        r#"bind-key    -T prefix s       display-popup -E -h "85%" -w "90%" "muxa watch""#;

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
        let bordered = r#"bind-key -T prefix s display-popup -E -h "100%" -w "100%" "muxa watch""#;
        assert!(!popup_binding_is_full_client(bordered));
    }

    #[test]
    fn borderless_without_full_width_is_still_rejected() {
        let narrow = r#"bind-key -T prefix s display-popup -BE -h "100%" -w "90%" "muxa watch""#;
        assert!(!popup_binding_is_full_client(narrow));
    }

    #[test]
    fn watch_binding_is_found_among_other_keys() {
        let listing = format!(
            "bind-key -T prefix D display-popup -E -w \"95%\" \"muxa dashboard\"\n{LIVE_MANAGED}\nbind-key -T prefix q display-popup -BE \"muxa peek\"\n"
        );
        assert_eq!(watch_popup_binding(&listing), Some(LIVE_MANAGED));
    }

    #[test]
    fn watch_binding_absent_when_nothing_runs_watch() {
        let listing = "bind-key -T prefix c new-window\nbind-key -T prefix D display-popup -E \"muxa dashboard\"\n";
        assert_eq!(watch_popup_binding(listing), None);
    }

    #[test]
    fn tmux_marker_block_detected_finds_complete_block() {
        let conf = "set -g mouse on\n# >>> muxa managed (tmux-popup) >>>\nbind-key s display-popup\n# <<< muxa managed (tmux-popup) <<<\n";
        assert!(tmux_marker_block_detected(conf, "tmux-popup"));
    }

    #[test]
    fn tmux_marker_block_detected_requires_close_fence() {
        // Open fence without the corresponding close — must NOT match.
        let conf = "# >>> muxa managed (tmux-popup) >>>\nbind-key s display-popup\n";
        assert!(!tmux_marker_block_detected(conf, "tmux-popup"));
    }

    #[test]
    fn tmux_marker_block_detected_ignores_quoted_substring() {
        // Defensive: a literal fence string inside a quoted status-right
        // value must not be confused for a real fence.
        let conf = "set -g status-right \"# >>> muxa managed (tmux-popup) >>> not a fence\"\n";
        assert!(!tmux_marker_block_detected(conf, "tmux-popup"));
    }

    #[test]
    fn extract_recent_errors_filters_and_caps() {
        let log = [
            "2026-04-30T10:00 INFO  starting muxad",
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
        assert_eq!(s, "muxad log — ERROR something went wrong");
    }

    #[test]
    fn format_warning_line_caps_long_lines() {
        // Use a marker char that's not present in the "muxad log — "
        // prefix so the count is unambiguous.
        let huge = "Z".repeat(500);
        let s = format_warning_line(&huge);
        // Body should be capped at 200 chars; prefix has zero Zs.
        assert_eq!(s.chars().filter(|c| *c == 'Z').count(), 200);
    }
}
