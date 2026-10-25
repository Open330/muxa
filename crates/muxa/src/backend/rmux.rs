//! rmux implementation of [`crate::backend::PaneBackend`].
//!
//! rmux intentionally exposes a tmux-compatible command surface, but it also
//! sets `TMUX` / `TMUX_PANE` for compatibility. Muxa must therefore keep rmux
//! as a distinct backend: pane ids are namespaced as `rmux:%N` internally and
//! the native `$RMUX` endpoint is threaded through control operations so a
//! pane can never be confused with a tmux pane carrying the same `%N` id.
//!
//! This first integration uses rmux's public CLI. The backend seam remains
//! independent of that transport, so it can move to `rmux-sdk` later without
//! changing daemon, hook, or UI callers.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::{HostKind, PaneBackend, PaneObservation};
use crate::tmux::{PaneInfo, PANE_FMT};

/// Namespace prefix for rmux pane ids inside muxa.
pub const PANE_ID_PREFIX: &str = "rmux:";

/// Keep synchronous CLI calls within the same budget as the tmux backend.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(1);

static BUFFER_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, thiserror::Error)]
enum RmuxError {
    #[error("running rmux: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("rmux command timed out after {0:?}")]
    Timeout(Duration),
}

/// rmux backend bound to the endpoint advertised by `$RMUX`, if any.
///
/// `$RMUX` has the native `"<socket>,<server-pid>,<session-id>"` shape. An
/// endpoint-less backend talks to rmux's default socket, which is how a
/// launchd/systemd daemon can observe a default rmux server without inheriting
/// pane environment variables.
#[derive(Debug, Clone)]
pub struct RmuxBackend {
    endpoint: Option<String>,
}

impl RmuxBackend {
    #[must_use]
    pub fn new() -> Self {
        Self {
            endpoint: endpoint_from_env(),
        }
    }

    /// Construct a backend for a known rmux socket path.
    #[must_use]
    pub fn with_endpoint(endpoint: impl Into<String>) -> Self {
        let endpoint = endpoint.into();
        Self {
            endpoint: (!endpoint.trim().is_empty()).then_some(endpoint),
        }
    }

    /// Build a native command pinned to this backend or an explicit endpoint.
    pub fn command(&self, endpoint: Option<&str>) -> Command {
        rmux_command(endpoint.or(self.endpoint.as_deref()))
    }

    /// Qualify native ids with a session before passing them to rmux. Linked
    /// windows repeat in a session group, and rmux 0.10 rejects their bare
    /// @N targets as ambiguous for several mutation commands.
    pub fn control_command(&self, args: &[&str]) -> Result<Command, String> {
        let mut args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
        if let Some(index) = args.iter().position(|arg| arg == "-t") {
            if let Some(raw) = args.get(index + 1) {
                let target = strip_prefix(raw);
                let pane_target = target.starts_with('%');
                if pane_target || target.starts_with('@') {
                    let mut query = self.command(None);
                    query.args([
                        if pane_target {
                            "list-panes"
                        } else {
                            "list-windows"
                        },
                        "-a",
                        "-F",
                        "#{session_id}\t#{window_id}\t#{pane_id}",
                    ]);
                    let output = successful_stdout(query).ok_or("cannot resolve rmux target")?;
                    let mut matches: Vec<_> = output
                        .lines()
                        .filter_map(|line| {
                            let fields: Vec<_> = line.split('\t').collect();
                            if fields.len() != 3
                                || fields[if pane_target { 2 } else { 1 }] != target
                            {
                                return None;
                            }
                            let sequence = fields[0].trim_start_matches('$').parse::<u64>().ok()?;
                            let qualified = if pane_target {
                                format!("{}:{}.{}", fields[0], fields[1], fields[2])
                            } else {
                                format!("{}:{}", fields[0], fields[1])
                            };
                            Some((sequence, qualified))
                        })
                        .collect();
                    matches.sort_by_key(|(sequence, _)| *sequence);
                    if let Some((_, qualified)) = matches.into_iter().next() {
                        args[index + 1] = qualified;
                    }
                }
            }
        }
        let mut command = self.command(None);
        command.args(args);
        Ok(command)
    }

    /// Execute a bounded control command, preserving the server error.
    pub fn run_control(&self, args: &[&str]) -> Result<(), String> {
        let command = self.control_command(args)?;
        let output = command_output(command, None).map_err(|error| error.to_string())?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
        }
    }

    /// Read a bounded control response from this exact rmux endpoint.
    pub fn capture_control(&self, args: &[&str]) -> Result<String, String> {
        let command = self.control_command(args)?;
        let output = command_output(command, None).map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
        }
        String::from_utf8(output.stdout).map_err(|error| error.to_string())
    }

    fn scan_panes(&self, target: Option<&str>) -> PaneObservation {
        let format = format!("{PANE_FMT}\t#{{socket_path}}");
        let mut args = vec!["list-panes"];
        if let Some(target) = target {
            args.extend(["-t", strip_prefix(target)]);
        } else {
            args.push("-a");
        }
        args.extend(["-F", &format]);
        let Ok(command) = self.control_command(&args) else {
            return PaneObservation::incomplete(Vec::new());
        };

        let output = match command_output(command, None) {
            Ok(output) => output,
            Err(error) => {
                tracing::debug!(%error, "rmux pane observation failed");
                return PaneObservation::incomplete(Vec::new());
            }
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            // The message is authoritative for this one endpoint, but an
            // endpoint-less daemon may also track hook rows from named rmux
            // servers it cannot enumerate. Without endpoint scope on an empty
            // response, treating this as globally complete would reap them.
            if stderr.starts_with("no server running on") {
                return PaneObservation::incomplete(Vec::new());
            }
            tracing::debug!(%stderr, "rmux list-panes returned non-zero");
            return PaneObservation::incomplete(Vec::new());
        }

        let stdout = match String::from_utf8(output.stdout) {
            Ok(stdout) => stdout,
            Err(error) => {
                tracing::debug!(%error, "rmux list-panes returned non-UTF-8 output");
                return PaneObservation::incomplete(Vec::new());
            }
        };
        parse_pane_observation(&stdout, self.endpoint.as_deref())
    }

    fn capture_on(&self, endpoint: Option<&str>, pane_id: &str) -> Option<String> {
        let scoped = Self {
            endpoint: endpoint
                .map(str::to_string)
                .or_else(|| self.endpoint.clone()),
        };
        let command = scoped
            .control_command(&["capture-pane", "-ep", "-t", strip_prefix(pane_id)])
            .ok()?;
        if let Some(output) = successful_stdout(command) {
            return Some(output);
        }
        // The pane may have moved since its observation. Only this read-only
        // operation retries with a freshly resolved identity; mutations never
        // enter this path or get replayed.
        let target = strip_prefix(pane_id);
        let (scope, pane) = target.rsplit_once('.')?;
        let (session, window) = scope.split_once(':')?;
        if qualified_pane_target(session, window, pane).as_deref() != Some(target) {
            return None;
        }
        successful_stdout(
            scoped
                .control_command(&["capture-pane", "-ep", "-t", pane])
                .ok()?,
        )
    }

    fn paste_text_on(&self, endpoint: Option<&str>, pane_id: &str, text: &str) -> bool {
        let buffer = format!(
            "muxa-rmux-send-{}-{}",
            std::process::id(),
            BUFFER_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );

        let mut load = self.command(endpoint);
        load.args(["load-buffer", "-b", &buffer, "-"]);
        if !command_output(load, Some(text.as_bytes())).is_ok_and(|o| o.status.success()) {
            return false;
        }

        let scoped = Self {
            endpoint: endpoint
                .map(str::to_string)
                .or_else(|| self.endpoint.clone()),
        };
        let Ok(paste) = scoped.control_command(&[
            "paste-buffer",
            "-p",
            "-r",
            "-b",
            &buffer,
            "-t",
            strip_prefix(pane_id),
        ]) else {
            let mut delete = self.command(endpoint);
            delete.args(["delete-buffer", "-b", &buffer]);
            let _ = command_output(delete, None);
            return false;
        };
        let pasted = command_output(paste, None).is_ok_and(|o| o.status.success());

        let mut delete = self.command(endpoint);
        delete.args(["delete-buffer", "-b", &buffer]);
        let _ = command_output(delete, None);
        pasted
    }
}

impl Default for RmuxBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl PaneBackend for RmuxBackend {
    fn kind(&self) -> HostKind {
        HostKind::Rmux
    }

    fn list_panes(&self) -> Vec<PaneInfo> {
        self.scan_panes(None).panes
    }

    fn observe_panes(&self) -> PaneObservation {
        self.scan_panes(None)
    }

    fn resolve_pane(&self, pane_id: &str) -> Option<PaneInfo> {
        self.scan_panes(Some(pane_id))
            .panes
            .into_iter()
            .find(|pane| pane.pane_id == namespace_pane_id(pane_id))
    }

    fn capture_pane(&self, pane_id: &str) -> Option<String> {
        self.capture_on(None, pane_id)
    }

    fn pane_pid_map(&self) -> HashMap<u32, String> {
        self.list_panes()
            .into_iter()
            .filter(|pane| pane.pane_pid != 0)
            .map(|pane| (pane.pane_pid, pane.pane_id))
            .collect()
    }

    fn current_pane(&self) -> Option<String> {
        std::env::var("RMUX_PANE")
            .ok()
            .filter(|pane| !pane.is_empty())
            .map(|pane| namespace_pane_id(&pane))
    }

    fn focus_pane(&self, pane_id: &str) -> bool {
        self.run_control(&["select-pane", "-t", strip_prefix(pane_id)])
            .is_ok()
    }

    fn send_text(&self, pane_id: &str, text: &str) -> bool {
        self.send_text_on(None, pane_id, text)
    }

    fn set_remain_on_exit_on(&self, endpoint: Option<&str>, pane_id: &str, on: bool) -> bool {
        let scoped = Self {
            endpoint: endpoint
                .map(str::to_string)
                .or_else(|| self.endpoint.clone()),
        };
        let value = if on { "on" } else { "off" };
        scoped
            .run_control(&[
                "set-option",
                "-p",
                "-t",
                strip_prefix(pane_id),
                "remain-on-exit",
                value,
            ])
            .is_ok()
    }

    fn respawn_pane_on(
        &self,
        endpoint: Option<&str>,
        pane_id: &str,
        cwd: &str,
        command: &str,
    ) -> bool {
        let scoped = Self {
            endpoint: endpoint
                .map(str::to_string)
                .or_else(|| self.endpoint.clone()),
        };
        scoped
            .run_control(&[
                "respawn-pane",
                "-k",
                "-c",
                cwd,
                "-t",
                strip_prefix(pane_id),
                command,
            ])
            .is_ok()
    }

    fn send_text_on(&self, endpoint: Option<&str>, pane_id: &str, text: &str) -> bool {
        if text.contains('\n') || text.ends_with(';') {
            return self.paste_text_on(endpoint, pane_id, text);
        }
        let scoped = Self {
            endpoint: endpoint
                .map(str::to_string)
                .or_else(|| self.endpoint.clone()),
        };
        scoped
            .run_control(&["send-keys", "-t", strip_prefix(pane_id), "-l", "--", text])
            .is_ok()
    }

    fn send_prompt_text_on(&self, socket: Option<&str>, pane_id: &str, text: &str) -> bool {
        text.is_empty() || self.paste_text_on(socket, pane_id, text)
    }

    fn capture_pane_on(&self, endpoint: Option<&str>, pane_id: &str) -> Option<String> {
        self.capture_on(endpoint, pane_id)
    }

    fn capture_pane_history_on(
        &self,
        endpoint: Option<&str>,
        pane_id: &str,
        history: usize,
    ) -> Option<String> {
        let scoped = Self {
            endpoint: endpoint
                .map(str::to_string)
                .or_else(|| self.endpoint.clone()),
        };
        let start = format!("-{history}");
        let command = scoped
            .control_command(&[
                "capture-pane",
                "-ep",
                "-S",
                &start,
                "-t",
                strip_prefix(pane_id),
            ])
            .ok()?;
        successful_stdout(command)
    }
}

/// Whether the rmux CLI is installed and runnable.
///
/// The daemon keeps an endpoint-less rmux backend active whenever the binary
/// exists, even before a server starts. Otherwise a daemon launched at login
/// would never notice an rmux server created later, and hook-registered named
/// endpoints could not route control calls through their recorded socket.
#[must_use]
pub fn binary_available() -> bool {
    let mut command = Command::new(rmux_binary());
    command.arg("-V");
    command_output(command, None).is_ok_and(|output| output.status.success())
}

/// Parse the socket path from rmux's native environment tuple.
#[must_use]
pub fn endpoint_from_value(value: &str) -> Option<String> {
    let endpoint = value.split(',').next()?.trim();
    (!endpoint.is_empty()).then(|| endpoint.to_string())
}

/// Socket path advertised to the current rmux pane.
#[must_use]
pub fn endpoint_from_env() -> Option<String> {
    endpoint_from_value(&std::env::var("RMUX").ok()?)
}

/// Whether `socket` is an rmux server's control endpoint: a socket under an
/// `rmux-<uid>` directory on Unix, or a `\\.\pipe\rmux-…` named pipe on
/// Windows.
#[must_use]
pub fn is_endpoint(socket: &str) -> bool {
    socket.contains("/rmux-") || socket.contains("\\rmux-")
}

fn namespace_pane_id(pane_id: &str) -> String {
    if pane_id.starts_with(PANE_ID_PREFIX) {
        pane_id.to_string()
    } else {
        format!("{PANE_ID_PREFIX}{pane_id}")
    }
}

fn strip_prefix(pane_id: &str) -> &str {
    pane_id.strip_prefix(PANE_ID_PREFIX).unwrap_or(pane_id)
}

/// Reuse identities from an authoritative pane observation, avoiding a second
/// whole-server inventory for a read-only capture. Unknown/legacy ids retain
/// the normal resolver and its old-rmux compatibility behavior.
pub fn qualified_pane_target(session: &str, window: &str, pane: &str) -> Option<String> {
    let pane = strip_prefix(pane);
    for (value, prefix) in [(session, '$'), (window, '@'), (pane, '%')] {
        let id = value.strip_prefix(prefix)?;
        if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
    }
    Some(format!("{session}:{window}.{pane}"))
}

fn parse_pane_observation(stdout: &str, endpoint: Option<&str>) -> PaneObservation {
    let complete = stdout
        .lines()
        .filter(|line| !line.is_empty())
        .all(|line| line.split('\t').count() > PANE_FMT.split('\t').count());
    let mut panes = Vec::new();
    for line in stdout.lines().filter(|line| !line.is_empty()) {
        let columns = line.split('\t').collect::<Vec<_>>();
        let row_endpoint = endpoint.or_else(|| {
            columns
                .get(PANE_FMT.split('\t').count())
                .copied()
                .filter(|value| !value.trim().is_empty())
        });
        for mut pane in crate::tmux::parse_pane_lines_for_socket(line, row_endpoint) {
            pane.pane_id = namespace_pane_id(&pane.pane_id);
            panes.push(pane);
        }
    }
    if complete {
        PaneObservation::complete(panes)
    } else {
        PaneObservation::incomplete(panes)
    }
}

fn rmux_command(endpoint: Option<&str>) -> Command {
    let mut command = Command::new(rmux_binary());
    command.env("LC_ALL", "en_US.UTF-8");
    if let Some(endpoint) = endpoint.filter(|endpoint| !endpoint.trim().is_empty()) {
        // rmux 0.10 can let an inherited pane override an explicit grouped
        // window target in display-message, returning empty identity fields.
        // Commands on a recorded endpoint use their own targets; retain RMUX
        // for session context, but never borrow the calling pane's identity.
        command.env_remove("RMUX_PANE").env_remove("TMUX_PANE");
        command.arg("-S").arg(live_endpoint(endpoint));
    }
    command
}

/// The pipe the server behind `endpoint` listens on now.
///
/// On Windows rmux names its pipe `\\.\pipe\rmux-<user>-…-<label hash>-<nonce>`
/// and draws a fresh nonce every time the server starts, so an endpoint
/// recorded before a restart — in a snapshot, or by `muxa reload` across its
/// own `kill-server` — names a pipe that no longer exists. The rest of the
/// name is the same user and socket label, which is the server meant. The
/// pipe directory is only listed, never opened, so no server sees a client.
#[must_use]
pub fn live_endpoint(endpoint: &str) -> String {
    if !cfg!(windows) {
        return endpoint.to_owned();
    }
    let Some(name) = endpoint.strip_prefix(r"\\.\pipe\") else {
        return endpoint.to_owned();
    };
    let Ok(entries) = std::fs::read_dir(r"\\.\pipe\") else {
        return endpoint.to_owned();
    };
    let names: Vec<String> = entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    restarted_pipe(name, &names)
        .map_or_else(|| endpoint.to_owned(), |live| format!(r"\\.\pipe\{live}"))
}

/// Pure half of [`live_endpoint`]: `None` while `name` itself is listed (or
/// it has no nonce to replace), else the one listed pipe that differs from it
/// only in its final `-<nonce>`.
fn restarted_pipe<'a>(name: &str, listed: &'a [String]) -> Option<&'a str> {
    if listed.iter().any(|pipe| pipe == name) {
        return None;
    }
    let (stem, nonce) = name.rsplit_once('-')?;
    if nonce.is_empty() || !nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut candidates = listed.iter().filter(|pipe| {
        pipe.strip_prefix(stem)
            .and_then(|rest| rest.strip_prefix('-'))
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_hexdigit()))
    });
    let live = candidates.next()?;
    candidates.next().is_none().then_some(live.as_str())
}

/// `list-sessions` and `list-clients` stdout from the default rmux server, for
/// session-activity sampling. `Ok(None)` when no server is running (the tmux
/// "no server running" analog); `Err` for any other failure, so the caller
/// leaves its open intervals alone.
pub(crate) fn activity_listing(
    session_format: &str,
    client_format: &str,
) -> Result<Option<(String, String)>, String> {
    let run = |args: &[&str]| -> Result<Option<String>, String> {
        let mut command = rmux_command(None);
        command.args(args);
        let output = command_output(command, None).map_err(|error| error.to_string())?;
        if output.status.success() {
            return Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()));
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("Error connecting") || stderr.contains("no server running") {
            return Ok(None);
        }
        Err(stderr.trim().to_owned())
    };
    let Some(sessions) = run(&["list-sessions", "-F", session_format])? else {
        return Ok(None);
    };
    let Some(clients) = run(&["list-clients", "-F", client_format])? else {
        return Ok(None);
    };
    Ok(Some((sessions, clients)))
}

fn rmux_binary() -> &'static Path {
    static RESOLVED: OnceLock<PathBuf> = OnceLock::new();
    RESOLVED.get_or_init(|| {
        if Command::new("rmux")
            .arg("-V")
            .output()
            .is_ok_and(|output| output.status.success())
        {
            return PathBuf::from("rmux");
        }
        rmux_fallback_candidates(dirs::home_dir().as_deref())
            .into_iter()
            .find(|path| path.is_file())
            .unwrap_or_else(|| PathBuf::from("rmux"))
    })
}

fn rmux_fallback_candidates(home: Option<&Path>) -> Vec<PathBuf> {
    let mut candidates = Vec::with_capacity(4);
    let program = if cfg!(windows) { "rmux.exe" } else { "rmux" };
    if let Some(home) = home {
        // rmux's official installer defaults to ~/.local/bin. Cargo installs
        // into ~/.cargo/bin. Neither is guaranteed to be in systemd/launchd's
        // PATH, so both must be resolved explicitly for muxad.
        candidates.push(home.join(".local").join("bin").join(program));
        candidates.push(home.join(".cargo").join("bin").join(program));
    }
    candidates.push(PathBuf::from("/opt/homebrew/bin/rmux"));
    candidates.push(PathBuf::from("/usr/local/bin/rmux"));
    candidates
}

fn successful_stdout(command: Command) -> Option<String> {
    let output = command_output(command, None).ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

struct Drains {
    stdout: Option<JoinHandle<Vec<u8>>>,
    stderr: Option<JoinHandle<Vec<u8>>>,
}

impl Drains {
    fn start(child: &mut Child) -> Self {
        Self {
            stdout: child.stdout.take().map(spawn_drain),
            stderr: child.stderr.take().map(spawn_drain),
        }
    }

    fn collect(&mut self) -> (Vec<u8>, Vec<u8>) {
        (
            join_drain(self.stdout.take()),
            join_drain(self.stderr.take()),
        )
    }
}

fn spawn_drain<R: Read + Send + 'static>(mut pipe: R) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = pipe.read_to_end(&mut bytes);
        bytes
    })
}

fn join_drain(handle: Option<JoinHandle<Vec<u8>>>) -> Vec<u8> {
    handle.map_or_else(Vec::new, |handle| handle.join().unwrap_or_default())
}

fn command_output(mut command: Command, input: Option<&[u8]>) -> Result<Output, RmuxError> {
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let mut drains = Drains::start(&mut child);
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        let _ = stdin.write_all(input);
    }

    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            let (stdout, stderr) = drains.collect();
            return Ok(Output {
                status,
                stdout,
                stderr,
            });
        }
        if started.elapsed() >= COMMAND_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(RmuxError::Timeout(COMMAND_TIMEOUT));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::ObservationCompleteness;

    fn full_row(socket: &str) -> String {
        let mut cols = vec![""; PANE_FMT.split('\t').count()];
        cols[..12].copy_from_slice(&[
            "%7",
            "alpha",
            "2",
            "1",
            "/dev/pts/3",
            "codex",
            "work",
            "4242",
            "/tmp/project",
            "$3",
            "@9",
            "editor",
        ]);
        cols[12] = "workspace-is-not-a-socket";
        cols[18] = "codex";
        cols[32] = "alpha-group";
        cols.push(socket);
        cols.join("\t")
    }

    #[test]
    fn endpoint_parser_reads_native_rmux_tuple() {
        assert_eq!(
            endpoint_from_value("/tmp/rmux-501/default,1234,$2").as_deref(),
            Some("/tmp/rmux-501/default")
        );
        assert_eq!(endpoint_from_value("  "), None);
    }

    #[test]
    fn parser_namespaces_ids_and_retains_endpoint() {
        let input = full_row("/tmp/row.sock");
        let observed = parse_pane_observation(&input, Some("/tmp/rmux.sock"));
        assert_eq!(observed.completeness, ObservationCompleteness::Complete);
        assert_eq!(observed.panes.len(), 1);
        let pane = &observed.panes[0];
        assert_eq!(pane.pane_id, "rmux:%7");
        assert_eq!(pane.session_id, "$3");
        assert_eq!(pane.window_id, "@9");
        assert_eq!(pane.session_group.as_deref(), Some("alpha-group"));
        assert_eq!(pane.current_path, "/tmp/project");
        assert_eq!(pane.pane_pid, 4242);
        assert_eq!(pane.agent_program.as_deref(), Some("codex"));
        assert_eq!(pane.socket.as_deref(), Some("/tmp/rmux.sock"));
    }

    #[test]
    fn parser_uses_formatted_socket_when_backend_is_endpointless() {
        let input = full_row("/tmp/resolved.sock");
        let observed = parse_pane_observation(&input, None);
        assert_eq!(observed.completeness, ObservationCompleteness::Complete);
        assert_eq!(
            observed.panes[0].socket.as_deref(),
            Some("/tmp/resolved.sock")
        );
    }

    #[test]
    fn malformed_rows_make_observation_incomplete() {
        let observed = parse_pane_observation("not-a-pane\n", None);
        assert_eq!(observed.completeness, ObservationCompleteness::Incomplete);
        assert!(observed.panes.is_empty());
    }

    #[test]
    fn command_targets_endpoint_before_subcommand() {
        let backend = RmuxBackend::with_endpoint("/tmp/rmux.sock");
        let command = backend.command(None);
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, ["-S", "/tmp/rmux.sock"]);
    }

    #[test]
    fn observed_identity_qualifies_capture_without_resolving_again() {
        let target = qualified_pane_target("$3", "@7", "rmux:%9").unwrap();
        let command = RmuxBackend::with_endpoint("/tmp/absent.sock")
            .control_command(&["capture-pane", "-ep", "-t", &target])
            .unwrap();
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "-S",
                "/tmp/absent.sock",
                "capture-pane",
                "-ep",
                "-t",
                "$3:@7.%9"
            ]
        );
        assert!(qualified_pane_target("", "@7", "%9").is_none());
        assert!(qualified_pane_target("$3", "@7", "%9junk").is_none());
    }

    #[test]
    fn backend_reports_rmux_kind_and_is_object_safe() {
        let backend = RmuxBackend::with_endpoint("/tmp/rmux.sock");
        assert_eq!(backend.kind(), HostKind::Rmux);
        let _backend: Box<dyn PaneBackend> = Box::new(backend);
    }

    #[test]
    fn daemon_fallbacks_cover_official_and_cargo_user_installs() {
        let home = Path::new("/home/tester");
        let program = if cfg!(windows) { "rmux.exe" } else { "rmux" };
        let candidates = rmux_fallback_candidates(Some(home));
        assert_eq!(candidates[0], home.join(".local").join("bin").join(program));
        assert_eq!(candidates[1], home.join(".cargo").join("bin").join(program));
    }

    #[test]
    fn a_restarted_windows_server_is_found_by_its_label() {
        let old = "rmux-S-1-5-21-1001-il-high-g-a2e5d6-e896b048";
        let listed = |names: &[&str]| names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
        // The recorded pipe is still there: nothing to re-resolve.
        assert_eq!(restarted_pipe(old, &listed(&[old, "other"])), None);
        // Restarted: same user and label, new nonce.
        let pipes = listed(&[
            "rmux-S-1-5-21-1001-il-high-g-a2e5d6-ad57aac8",
            "rmux-S-1-5-21-1001-il-high-g-ffff00-0001",
        ]);
        assert_eq!(
            restarted_pipe(old, &pipes),
            Some("rmux-S-1-5-21-1001-il-high-g-a2e5d6-ad57aac8")
        );
        // Another label, or two candidates, is not a guess worth making.
        assert_eq!(
            restarted_pipe(old, &listed(&["rmux-S-1-5-21-1001-il-high-g-ffff00-0001"])),
            None
        );
        let twins = listed(&[
            "rmux-S-1-5-21-1001-il-high-g-a2e5d6-01",
            "rmux-S-1-5-21-1001-il-high-g-a2e5d6-02",
        ]);
        assert_eq!(restarted_pipe(old, &twins), None);
    }

    /// Exercise the public CLI transport against an explicitly isolated rmux
    /// server. The caller owns the server lifecycle so ordinary test runs stay
    /// hermetic; see `docs/RMUX.md` for the invocation.
    #[test]
    #[ignore = "requires MUXA_RMUX_TEST_ENDPOINT and MUXA_RMUX_TEST_PANE"]
    fn live_backend_smoke_against_explicit_endpoint() {
        let endpoint = std::env::var("MUXA_RMUX_TEST_ENDPOINT")
            .expect("MUXA_RMUX_TEST_ENDPOINT must name a running rmux socket");
        let pane_id = std::env::var("MUXA_RMUX_TEST_PANE")
            .expect("MUXA_RMUX_TEST_PANE must name a pane on that socket");
        let pane_id = namespace_pane_id(&pane_id);
        let backend = RmuxBackend::with_endpoint(&endpoint);

        let observed = backend.observe_panes();
        assert_eq!(observed.completeness, ObservationCompleteness::Complete);
        assert!(
            observed.panes.iter().any(|pane| {
                pane.pane_id == pane_id && pane.socket.as_deref() == Some(endpoint.as_str())
            }),
            "target pane was absent from observation: {observed:?}"
        );

        let resolved = backend
            .resolve_pane(&pane_id)
            .expect("target pane should resolve through its namespaced id");
        assert_eq!(resolved.pane_id, pane_id);
        assert_eq!(resolved.socket.as_deref(), Some(endpoint.as_str()));
        assert!(backend.focus_pane(&pane_id));

        // The expected output must not occur literally in the typed command;
        // otherwise capture could pass merely because the shell echoed the
        // pending input even if the submit CR never executed it.
        let marker_value = u64::from(std::process::id()) + 104_729;
        let marker = format!("__MUXA_RMUX_SMOKE_{marker_value}__");
        let command = format!(
            "printf '__MUXA_RMUX_SMOKE_'$(({}+104729))'__'",
            std::process::id()
        );
        assert!(!command.contains(&marker));
        assert!(backend.send_text(&pane_id, &command));
        assert!(backend.send_text(&pane_id, "\r"));

        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let captured = backend
                .capture_pane(&pane_id)
                .expect("target pane should remain capturable");
            if captured.contains(&marker) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "marker did not appear in captured pane before timeout: {captured:?}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }

        // A trailing semicolon takes the load/paste/delete-buffer path rather
        // than send-keys, covering the transport used for multiline prompts.
        let paste_value = marker_value + 1;
        let paste_marker = format!("__MUXA_RMUX_PASTE_{paste_value}__");
        let paste_command = format!(
            "printf '__MUXA_RMUX_PASTE_'$(({}+104730))'__';",
            std::process::id()
        );
        assert!(!paste_command.contains(&paste_marker));
        assert!(backend.send_text(&pane_id, &paste_command));
        assert!(backend.send_text(&pane_id, "\r"));
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let captured = backend
                .capture_pane(&pane_id)
                .expect("target pane should remain capturable");
            if captured.contains(&paste_marker) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "paste marker did not appear before timeout: {captured:?}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}
