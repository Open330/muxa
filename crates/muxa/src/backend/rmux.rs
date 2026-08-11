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

    fn command(&self, endpoint: Option<&str>) -> Command {
        rmux_command(endpoint.or(self.endpoint.as_deref()))
    }

    fn scan_panes(&self, target: Option<&str>) -> PaneObservation {
        let mut command = self.command(None);
        command.arg("list-panes");
        if let Some(target) = target {
            command.args(["-t", strip_prefix(target)]);
        } else {
            command.arg("-a");
        }
        // `RmuxBackend::new()` is endpoint-less in a long-running daemon.
        // Ask rmux to stamp its resolved socket on every row so the resulting
        // pane identity still routes later control calls to the right server.
        let format = format!("{PANE_FMT}\t#{{socket_path}}");
        command.args(["-F", &format]);

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
        let mut command = self.command(endpoint);
        command.args(["capture-pane", "-ep", "-t", strip_prefix(pane_id)]);
        successful_stdout(command)
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

        let mut paste = self.command(endpoint);
        paste.args([
            "paste-buffer",
            "-p",
            "-b",
            &buffer,
            "-t",
            strip_prefix(pane_id),
        ]);
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
        let mut command = self.command(None);
        command.args(["select-pane", "-t", strip_prefix(pane_id)]);
        command_output(command, None).is_ok_and(|o| o.status.success())
    }

    fn send_text(&self, pane_id: &str, text: &str) -> bool {
        self.send_text_on(None, pane_id, text)
    }

    fn send_text_on(&self, endpoint: Option<&str>, pane_id: &str, text: &str) -> bool {
        if text.contains('\n') || text.ends_with(';') {
            return self.paste_text_on(endpoint, pane_id, text);
        }
        let mut command = self.command(endpoint);
        command.args(["send-keys", "-t", strip_prefix(pane_id), "-l", "--", text]);
        command_output(command, None).is_ok_and(|o| o.status.success())
    }

    fn capture_pane_on(&self, endpoint: Option<&str>, pane_id: &str) -> Option<String> {
        self.capture_on(endpoint, pane_id)
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

fn parse_pane_observation(stdout: &str, endpoint: Option<&str>) -> PaneObservation {
    let complete = stdout
        .lines()
        .filter(|line| !line.is_empty())
        .all(|line| line.split('\t').count() >= 13);
    let mut panes = Vec::new();
    for line in stdout.lines().filter(|line| !line.is_empty()) {
        let columns = line.split('\t').collect::<Vec<_>>();
        let row_endpoint = endpoint.or_else(|| {
            columns
                .get(12)
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
        command.arg("-S").arg(endpoint);
    }
    command
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
    if let Some(home) = home {
        // rmux's official installer defaults to ~/.local/bin. Cargo installs
        // into ~/.cargo/bin. Neither is guaranteed to be in systemd/launchd's
        // PATH, so both must be resolved explicitly for muxad.
        candidates.push(home.join(".local/bin/rmux"));
        candidates.push(home.join(".cargo/bin/rmux"));
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
        let input = "%7\talpha\t2\t1\t/dev/pts/3\tcodex\twork\t4242\t/tmp/project\t$3\t@9\teditor\t/tmp/row.sock\n";
        let observed = parse_pane_observation(input, Some("/tmp/rmux.sock"));
        assert_eq!(observed.completeness, ObservationCompleteness::Complete);
        assert_eq!(observed.panes.len(), 1);
        let pane = &observed.panes[0];
        assert_eq!(pane.pane_id, "rmux:%7");
        assert_eq!(pane.session_id, "$3");
        assert_eq!(pane.window_id, "@9");
        assert_eq!(pane.current_path, "/tmp/project");
        assert_eq!(pane.pane_pid, 4242);
        assert_eq!(pane.socket.as_deref(), Some("/tmp/rmux.sock"));
    }

    #[test]
    fn parser_uses_formatted_socket_when_backend_is_endpointless() {
        let input = "%7\talpha\t2\t1\t/dev/pts/3\tcodex\twork\t4242\t/tmp/project\t$3\t@9\teditor\t/tmp/resolved.sock\n";
        let observed = parse_pane_observation(input, None);
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
    fn backend_reports_rmux_kind_and_is_object_safe() {
        let backend = RmuxBackend::with_endpoint("/tmp/rmux.sock");
        assert_eq!(backend.kind(), HostKind::Rmux);
        let _backend: Box<dyn PaneBackend> = Box::new(backend);
    }

    #[test]
    fn daemon_fallbacks_cover_official_and_cargo_user_installs() {
        let candidates = rmux_fallback_candidates(Some(Path::new("/home/tester")));
        assert_eq!(candidates[0], PathBuf::from("/home/tester/.local/bin/rmux"));
        assert_eq!(candidates[1], PathBuf::from("/home/tester/.cargo/bin/rmux"));
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
