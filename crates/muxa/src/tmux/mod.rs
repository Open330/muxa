//! tmux CLI wrapper.
//!
//! Uses shell-outs for now. Control mode (`tmux -C`) will replace this once
//! we need real-time events (focus-changed, pane-close, etc.).
//!
//! The single-socket helpers in this module talk to whatever tmux server
//! `$TMUX_TMPDIR` / the `default` socket points to. For the global view —
//! every tmux server running for this user — see [`scanner`].

pub mod layout;
pub mod scanner;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::OnceLock;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::backend::{ObservationCompleteness, PaneObservation};

/// Bound every synchronous tmux shell-out used by watch/status paths.
///
/// The async dashboard scanner already has a per-socket timeout. These
/// wrappers are the older synchronous path used by `muxa watch` and the
/// daemon reconciler; without a guard, a wedged tmux server can hold the
/// first watch refresh for many seconds.
const TMUX_COMMAND_TIMEOUT: Duration = Duration::from_secs(1);

/// Absolute path to the `tmux` binary, resolved once per process.
///
/// `tmux_command()` relies on `$PATH`, which is fine for an
/// interactive shell but fails inside `launchd`-spawned `muxad` whose
/// inherited `PATH` is `/usr/bin:/bin:/usr/sbin:/sbin` — neither
/// Homebrew prefix is on that list. A failed shell-out collapsed to an
/// empty pane inventory drove the reconciler to reap every paned agent
/// each tick, wiping `last_prompt` on every row.
///
/// Resolution order:
/// 1. `$PATH` lookup via `tmux -V`. Cheap (~5 ms once) and the steady-state
///    path everywhere except launchd.
/// 2. Known Homebrew install prefixes — Apple Silicon then Intel.
/// 3. Bare `tmux` as a last-resort sentinel so the eventual error message
///    matches the prior behavior.
///
/// Build a `Command` for shelling out to tmux with the resolved binary
/// path and a UTF-8 locale pre-applied.
///
/// launchd's gui-domain user-agents inherit no `LANG`/`LC_*` env, leaving
/// child processes in the POSIX (C) locale. tmux in that locale silently
/// transliterates non-ASCII bytes — including the literal TAB characters
/// our format strings rely on as field separators — to `_`. The result
/// is a `list-panes` payload that exits 0 with 1.9 KB of stdout but parses
/// to zero rows, so the reconciler then reaps every paned agent.
///
/// Set `LC_ALL` (overrides every `LC_*`) to a UTF-8 locale on every tmux
/// invocation so the daemon and the user's interactive shell see byte-
/// identical output. `en_US.UTF-8` is universally available on macOS;
/// `C.UTF-8` would work on recent macOS too but is less portable.
pub fn tmux_command() -> Command {
    let mut cmd = Command::new(tmux_binary());
    cmd.env("LC_ALL", "en_US.UTF-8");
    cmd
}

pub fn tmux_binary() -> &'static Path {
    static RESOLVED: OnceLock<PathBuf> = OnceLock::new();
    RESOLVED.get_or_init(|| {
        if Command::new("tmux")
            .arg("-V")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            return PathBuf::from("tmux");
        }
        ["/opt/homebrew/bin/tmux", "/usr/local/bin/tmux"]
            .iter()
            .find(|p| Path::new(p).exists())
            .map_or_else(|| PathBuf::from("tmux"), PathBuf::from)
    })
}

#[derive(Debug, thiserror::Error)]
pub enum TmuxError {
    #[error("running tmux: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("{command} timed out after {timeout:?}")]
    Timeout { command: String, timeout: Duration },
    #[error("tmux returned non-zero exit: {0}")]
    NonZero(String),
    #[error("unexpected tmux output: {0}")]
    BadOutput(String),
}

/// Reader threads draining a child's stdout/stderr for the whole wait.
///
/// A pipe holds only its capacity before the writer blocks in `write`, and
/// that capacity is not the 64 KB one might assume: once a user's total pipe
/// pages cross `fs.pipe-user-pages-soft`, Linux hands out minimum-size pipes
/// (one page). A box running dozens of agents crosses that line easily, and
/// an 8 KB pipe is smaller than one `list-panes -a` payload on a busy tmux
/// server.
///
/// So the parent must never block while a child still has output to write.
/// Waiting on the child without reading deadlocks: tmux blocks writing, so it
/// never exits, so we kill it at the timeout and [`list_panes`] collapses to
/// an empty inventory — which surfaces to the user as every NAME column
/// falling back to a raw `%42` pane id. Draining on separate threads keeps
/// the child unblocked regardless of payload size.
struct Drains {
    stdout: Option<JoinHandle<Vec<u8>>>,
    stderr: Option<JoinHandle<Vec<u8>>>,
}

impl Drains {
    /// Start draining both pipes. Call this before any blocking parent-side
    /// work — a stdin write, the wait loop — so a full pipe can never stall
    /// the child.
    fn start(child: &mut Child) -> Self {
        Self {
            stdout: child.stdout.take().map(spawn_drain),
            stderr: child.stderr.take().map(spawn_drain),
        }
    }

    /// Join both readers and hand back what they read. Only valid once the
    /// child has exited, which closes the write ends and lets `read_to_end`
    /// see EOF.
    fn collect(&mut self) -> (Vec<u8>, Vec<u8>) {
        (
            join_drain(self.stdout.take()),
            join_drain(self.stderr.take()),
        )
    }
}

/// Read one child pipe to EOF on a dedicated thread. A read error collapses
/// to the bytes seen so far: callers parse the payload and already treat a
/// short read the same as malformed output, so there is nothing extra to
/// recover.
fn spawn_drain<R: Read + Send + 'static>(mut pipe: R) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    })
}

fn join_drain(handle: Option<JoinHandle<Vec<u8>>>) -> Vec<u8> {
    handle.map_or_else(Vec::new, |h| h.join().unwrap_or_default())
}

/// Wait for `child` under `timeout` while `drains` empties its pipes.
///
/// On timeout the drain threads are dropped rather than joined — killing the
/// child closes the write ends, so they finish on their own, and we must not
/// block the caller on a process we just gave up on.
fn wait_drained(
    mut child: Child,
    mut drains: Drains,
    timeout: Duration,
    command: String,
) -> Result<Output, TmuxError> {
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
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(TmuxError::Timeout { command, timeout });
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn command_output_with_timeout(
    mut cmd: Command,
    timeout: Duration,
    command: String,
) -> Result<Output, TmuxError> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let drains = Drains::start(&mut child);
    wait_drained(child, drains, timeout, command)
}

fn tmux_output(args: &[&str]) -> Result<Output, TmuxError> {
    let mut cmd = tmux_command();
    cmd.args(args);
    command_output_with_timeout(
        cmd,
        TMUX_COMMAND_TIMEOUT,
        format!("tmux {}", args.join(" ")),
    )
}

/// Build a tmux `Command` scoped to `MUXA_TMUX_SOCKET` when that env var
/// names a specific server socket.
///
/// The other single-socket helpers target whatever the default socket /
/// `$TMUX_TMPDIR` resolves to. When the daemon is scoped to one server
/// (`MUXA_TMUX_SOCKET`, as in an isolated or test context) we pass
/// `-S <socket>` so the command lands on that server rather than the default
/// one. Unset ⇒ no `-S`, byte-identical to `tmux_command()`. This is the
/// *env-scoped* fallback; a control op that knows the pane's server prefers
/// [`tmux_command_targeting`], which pins the exact server the pane lives on.
pub fn tmux_command_scoped() -> Command {
    let mut cmd = tmux_command();
    if let Ok(sock) = std::env::var("MUXA_TMUX_SOCKET") {
        let trimmed = sock.trim();
        if !trimmed.is_empty() {
            cmd.arg("-S").arg(trimmed);
        }
    }
    cmd
}

/// Resolve a short tmux socket name (a pane row's recorded `tmux_socket`, e.g.
/// `default` / `amux` — the socket file's basename) to the full socket *path*
/// of a live server, by matching the basename against the scanner's socket
/// enumeration. `None` when no enumerated socket matches (server gone, or the
/// name came from a non-standard socket dir the scanner doesn't walk).
///
/// This is what lets a control op target the *specific* server a pane lives
/// on: pane id `%5` exists on every tmux server, so `send-keys` / `capture-pane`
/// must be pinned to the right one via `-S <full-path>`. Matching against
/// [`scanner::enumerate_sockets`] (which already honors `MUXA_TMUX_SOCKET`
/// scoping and the macOS `/tmp`↔`/private/tmp` split) keeps the targeting
/// byte-identical to how the pane was discovered in the first place.
fn resolve_socket_path(short_name: &str) -> Option<PathBuf> {
    let short_name = short_name.trim();
    if short_name.is_empty() {
        return None;
    }
    scanner::enumerate_sockets()
        .into_iter()
        .find(|p| p.file_name().and_then(|n| n.to_str()) == Some(short_name))
}

/// Build a tmux `Command` pinned to the specific server named by `socket` (a
/// pane row's recorded short socket name). Resolves the name to the live
/// server's full socket path and passes `-S <path>` so the command can't leak
/// onto a different server that happens to share the pane id.
///
/// Falls back to [`tmux_command_scoped`] (the `MUXA_TMUX_SOCKET`-or-default
/// behavior) when `socket` is `None`/empty or doesn't resolve to a live
/// server — i.e. when the agent row has no recorded socket — preserving the
/// pre-control-plane single-server behavior.
fn tmux_command_targeting(socket: Option<&str>) -> Command {
    if let Some(path) = socket.and_then(resolve_socket_path) {
        let mut cmd = tmux_command();
        cmd.arg("-S").arg(path);
        return cmd;
    }
    tmux_command_scoped()
}

/// The `send-keys` argv (after any server-scope flags) for a literal text
/// injection. Split out from [`send_text_on`] so the argument construction is
/// unit-testable without a running tmux server.
///
/// `-l` sends the text literally — no key-name lookup — so arbitrary prompt
/// text can't be misread as a tmux key (`Enter`, `C-c`, …). The `--` marks the
/// end of options so text that *starts* with `-` (e.g. `-rf`) is taken as the
/// literal argument, not a flag — MCP forwards arbitrary model text, so this
/// path must survive hostile leading characters.
///
/// The `--` does NOT rescue a *trailing* `;`: tmux consumes a trailing
/// unescaped `;` as a command separator at its command-parse layer, before this
/// command's own option scanner runs. Text ending in `;` (and multi-line text)
/// is routed through the paste path instead — see [`needs_paste`].
fn send_keys_argv<'a>(pane_id: &'a str, text: &'a str) -> [&'a str; 6] {
    ["send-keys", "-t", pane_id, "-l", "--", text]
}

/// Whether literal `text` must be injected via the paste-buffer path rather
/// than a single `send-keys -l -- …` call.
///
/// Two hazards can't be sent verbatim through `send-keys` and are NOT fixed by
/// the `--` terminator (both are resolved by tmux *before* send-keys' own
/// option scanner runs):
///   - **embedded newline** — `send-keys -l` replays each `\n` as an Enter, so
///     a multi-line prompt is submitted line-by-line even with `submit:false`.
///   - **trailing `;`** — tmux eats a trailing unescaped `;` as a command
///     separator, silently dropping it (verified on tmux 3.x: `-l -- ';'` types
///     nothing, while `-l -- 'a;b'` is fine — only a *trailing* `;` is lost).
///
/// Feeding the text on stdin via `load-buffer` and replaying it with a
/// bracketed `paste-buffer` sidesteps argv parsing entirely, so both land
/// literally. The lone submit CR (`"\r"`) has neither hazard, so it stays on
/// the fast `send-keys` path.
fn needs_paste(text: &str) -> bool {
    text.contains('\n') || text.ends_with(';')
}

/// A process-unique scratch tmux buffer name for one paste injection. Unique
/// (pid + monotonic counter) so concurrent control ops can't clobber each
/// other's buffer between `load-buffer` and `paste-buffer`.
fn paste_buffer_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("muxa-send-{}-{n}", std::process::id())
}

/// Like [`command_output_with_timeout`] but writes `input` to the child's
/// stdin (then closes it) before waiting — for `load-buffer -b <buf> -`, which
/// reads the paste payload from stdin so it never passes through argv.
fn feed_stdin_with_timeout(
    mut cmd: Command,
    timeout: Duration,
    command: String,
    input: &[u8],
) -> Result<Output, TmuxError> {
    use std::io::Write;
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    // Drain before writing: `write_all` on a paste payload larger than the
    // stdin pipe blocks until tmux consumes it, and tmux can only do that if
    // its own output side is not already wedged. See [`Drains`].
    let drains = Drains::start(&mut child);
    // Write the payload and drop the handle so tmux sees EOF. Best-effort: a
    // failed write (child already died) falls through to the wait/timeout
    // below, which surfaces the real failure via the exit status.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input);
    }
    wait_drained(child, drains, timeout, command)
}

/// Inject `text` into `pane_id` via tmux's paste buffer instead of
/// `send-keys`, so multi-line text lands as one bracketed paste (not an
/// Enter-per-newline submit) and a trailing `;` survives. Used by
/// [`send_text_on`] for text that [`needs_paste`] flags.
///
/// Sequence: `load-buffer -b <buf> -` (payload on stdin, never in argv) →
/// `paste-buffer -p -b <buf> -t <pane>` (`-p` = bracketed paste) →
/// `delete-buffer -b <buf>` (best-effort scratch cleanup).
///
/// Bracketed paste is best-effort by nature: a paste-aware target (Claude
/// Code's input, a modern readline shell with bracketed-paste on) inserts the
/// whole block without executing intermediate newlines; a target that doesn't
/// honor bracketed paste still sees the raw newlines and may run them
/// line-by-line. This is strictly better than `send-keys -l` (which ALWAYS
/// submits per newline), so it's the least-surprising default for multi-line
/// submit semantics. The trailing submit CR (`submit:true`) is sent separately
/// as a `send-keys` Enter and is unaffected.
fn paste_text_on(socket: Option<&str>, pane_id: &str, text: &str) -> bool {
    let buf = paste_buffer_name();

    let mut load = tmux_command_targeting(socket);
    load.args(["load-buffer", "-b", &buf, "-"]);
    let loaded = feed_stdin_with_timeout(
        load,
        TMUX_COMMAND_TIMEOUT,
        format!("tmux load-buffer -b {buf}"),
        text.as_bytes(),
    )
    .is_ok_and(|o| o.status.success());
    if !loaded {
        return false;
    }

    let mut paste = tmux_command_targeting(socket);
    paste.args(["paste-buffer", "-p", "-b", &buf, "-t", pane_id]);
    let pasted = command_output_with_timeout(
        paste,
        TMUX_COMMAND_TIMEOUT,
        format!("tmux paste-buffer -t {pane_id}"),
    )
    .is_ok_and(|o| o.status.success());

    // `paste-buffer` without `-d` leaves the named buffer behind; delete it so
    // scratch buffers don't accumulate. A leaked buffer is harmless, so the
    // result is ignored — the paste's success is what we report.
    let mut del = tmux_command_targeting(socket);
    del.args(["delete-buffer", "-b", &buf]);
    let _ = command_output_with_timeout(
        del,
        TMUX_COMMAND_TIMEOUT,
        format!("tmux delete-buffer -b {buf}"),
    );

    pasted
}

/// Inject `text` into `pane_id` as literal keystrokes on the env-scoped
/// default server. Thin wrapper over [`send_text_on`] with no pinned socket —
/// kept for callers/tests that don't carry a recorded socket. The control
/// plane uses [`send_text_on`] with the agent row's recorded socket.
pub fn send_text(pane_id: &str, text: &str) -> bool {
    send_text_on(None, pane_id, text)
}

/// Inject `text` into `pane_id` on the specific tmux server named by `socket`
/// (a pane row's recorded short socket name; `None` ⇒ env-scoped default).
/// Backs the tmux [`crate::backend::PaneBackend::send_text_on`] capability
/// and, through it, the daemon's `send_prompt` IPC.
///
/// Simple single-line text goes through a fast `send-keys -l -- <text>`; text
/// with an embedded newline or a trailing `;` is routed through the paste
/// buffer instead (see [`needs_paste`] / [`paste_text_on`]) so it lands
/// verbatim. Returns `true` on success; `false` when the pane is gone, tmux
/// errors, or the shell-out times out (best-effort, matching this module).
pub fn send_text_on(socket: Option<&str>, pane_id: &str, text: &str) -> bool {
    if needs_paste(text) {
        return paste_text_on(socket, pane_id, text);
    }
    let mut cmd = tmux_command_targeting(socket);
    cmd.args(send_keys_argv(pane_id, text));
    command_output_with_timeout(
        cmd,
        TMUX_COMMAND_TIMEOUT,
        format!("tmux send-keys -t {pane_id}"),
    )
    .is_ok_and(|o| o.status.success())
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PaneInfo {
    pub pane_id: String,
    /// tmux's stable session id (for example `$3`). Empty for backends that
    /// do not expose a tmux session identity.
    #[serde(default)]
    pub session_id: String,
    pub session: String,
    /// tmux's stable window id (for example `@7`). Collaboration rooms use
    /// `(socket, window_id)` rather than the mutable name/index.
    #[serde(default)]
    pub window_id: String,
    /// Human-readable tmux window name. Informational; never used as the
    /// durable room identity.
    #[serde(default)]
    pub window_name: String,
    pub window_index: String,
    pub pane_index: String,
    pub tty: String,
    pub current_command: String,
    pub title: String,
    /// Working directory of the pane's active process, from tmux
    /// `#{pane_current_path}`. Empty when the backend can't supply it
    /// (zellij CLI, older `PANE_FMT` output). Used to correlate a paneless
    /// agent hook — a `code_mode_host` codex fires hooks from a detached
    /// app-server with no `TMUX_PANE`, so its row carries a `cwd` but no
    /// pane — back to the tmux pane it is actually running in.
    #[serde(default)]
    pub current_path: String,
    /// PID of the pane's initial process (typically the shell tmux spawned).
    /// `0` means "unknown" — backends that can't supply it (zellij CLI today,
    /// truncated lines from older tmux) leave it zeroed out, and downstream
    /// discovery treats `0` as "no process tree to walk."
    pub pane_pid: u32,
    /// Server endpoint identity for this pane. tmux rows use a short socket
    /// name (for example `default`); rmux rows use the full native endpoint.
    /// Pane ids are only unique per server, so consumers matching by pane id
    /// use this to disambiguate. `None` when the backend cannot name one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<String>,
}

/// The short display name for a tmux socket path: its file basename
/// (`/private/tmp/tmux-501/amux` → `amux`). Falls back to the input when
/// there is no basename. Used to compare `$TMUX`-derived paths against
/// scanner socket names.
pub fn socket_short_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .to_string()
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// tmux's stable session id (e.g. `$3`). Prefer this over the mutable
    /// session name for persisted counters.
    pub session_id: String,
    pub name: String,
    /// Number of attached clients. A session is "active" for muxa's
    /// cumulative timer when this is greater than zero.
    pub attached_clients: u32,
}

#[derive(Debug, Clone)]
pub struct ClientInfo {
    /// tmux `#{client_name}`, typically the controlling tty (e.g. `/dev/pts/3`).
    /// Reused across attaches of the same terminal, so it alone can't tell a
    /// reattach from a keypress — pair it with [`Self::created`].
    pub name: String,
    /// Name of the session currently displayed by this tmux client.
    pub session: String,
    /// tmux control-mode clients are automation, not an interactive user
    /// looking at a foreground session, so duration tracking ignores them.
    pub control_mode: bool,
    /// Unix epoch (seconds) of this client's last activity — a keypress,
    /// scroll, or other input — from tmux `#{client_activity}`. It advances
    /// only when the human interacts, which is what distinguishes active
    /// reading from an idle attach. `0` when tmux did not report it.
    pub last_activity: i64,
    /// Unix epoch (seconds) this client attached, from tmux `#{client_created}`.
    /// Changes on every (re)attach even when the tty is reused, so
    /// `(name, created)` uniquely identifies one attach session — letting input
    /// detection treat an idle reattach as a fresh client rather than input.
    /// `0` when tmux did not report it.
    pub created: i64,
    /// Whether the client's active pane is in copy/view mode, from tmux
    /// `#{pane_in_mode}`. When `true`, an activity advance is almost always
    /// scrollback navigation (reading) rather than typing input to the program,
    /// so input detection tags the tick as scroll. `false` when tmux did not
    /// report it (older tmux / other backends) — defaults to "not scrolling".
    /// Caveat: a TUI that handles its own scroll without entering tmux copy-mode
    /// keeps this `false`, so it catches tmux scrollback, not in-app scroll.
    pub in_copy_mode: bool,
}

/// `tmux -F` format string for `list-panes`. Tab-separated columns parsed
/// in `parse_pane_lines`. Kept `pub(crate)` so [`scanner`] can reuse it.
pub(crate) const PANE_FMT: &str =
    "#{pane_id}\t#{session_name}\t#{window_index}\t#{pane_index}\t#{pane_tty}\t#{pane_current_command}\t#{pane_title}\t#{pane_pid}\t#{pane_current_path}\t#{session_id}\t#{window_id}\t#{window_name}";

pub(crate) const SESSION_FMT: &str = "#{session_id}\t#{session_name}\t#{session_attached}";
pub(crate) const CLIENT_FMT: &str =
    "#{client_name}\t#{client_session}\t#{client_control_mode}\t#{client_activity}\t#{client_created}\t#{pane_in_mode}";

/// Parse the `\t`-separated stdout of `tmux list-panes -F PANE_FMT` into
/// `PaneInfo` rows. Lines with too few columns are silently skipped — the
/// caller only sees well-formed rows. The `pane_pid` column was added in
/// 0.5.x; rows from older `PANE_FMT` outputs (or other backends that
/// don't emit it) get `pane_pid = 0`.
#[cfg(test)]
pub(crate) fn parse_pane_lines(stdout: &str) -> Vec<PaneInfo> {
    parse_pane_lines_for_socket(stdout, None)
}

/// `parse_pane_lines` with the originating server's socket short name
/// stamped on every row (see [`PaneInfo::socket`]).
pub(crate) fn parse_pane_lines_for_socket(stdout: &str, socket: Option<&str>) -> Vec<PaneInfo> {
    let mut panes = Vec::new();
    for line in stdout.lines() {
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 7 {
            continue;
        }
        let pane_pid = cols.get(7).and_then(|s| s.parse().ok()).unwrap_or(0);
        panes.push(PaneInfo {
            pane_id: cols[0].into(),
            session_id: cols.get(9).map(|s| (*s).to_string()).unwrap_or_default(),
            session: cols[1].into(),
            window_id: cols.get(10).map(|s| (*s).to_string()).unwrap_or_default(),
            window_name: cols.get(11).map(|s| (*s).to_string()).unwrap_or_default(),
            window_index: cols[2].into(),
            pane_index: cols[3].into(),
            tty: cols[4].into(),
            current_command: cols[5].into(),
            title: cols[6].into(),
            pane_pid,
            current_path: cols.get(8).map(|s| (*s).to_string()).unwrap_or_default(),
            socket: socket.map(Into::into),
        });
    }
    panes
}

/// Parse pane output while retaining whether every non-empty row had the
/// minimum shape required by [`PANE_FMT`]. The ordinary parser intentionally
/// stays best-effort; reconciliation uses this stricter signal so a locale- or
/// version-mangled response cannot masquerade as an authoritative empty set.
fn observe_pane_lines_for_socket(stdout: &str, socket: Option<&str>) -> PaneObservation {
    let complete = stdout
        .lines()
        .filter(|line| !line.is_empty())
        .all(|line| line.split('\t').count() >= 7);
    let panes = parse_pane_lines_for_socket(stdout, socket);
    if complete {
        PaneObservation::complete(panes)
    } else {
        PaneObservation::incomplete(panes)
    }
}

pub(crate) fn parse_session_lines(stdout: &str) -> Vec<SessionInfo> {
    let mut sessions = Vec::new();
    for line in stdout.lines() {
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 3 {
            continue;
        }
        let attached_clients = cols[2].trim().parse::<u32>().unwrap_or(0);
        sessions.push(SessionInfo {
            session_id: cols[0].into(),
            name: cols[1].into(),
            attached_clients,
        });
    }
    sessions
}

pub(crate) fn parse_client_lines(stdout: &str) -> Vec<ClientInfo> {
    let mut clients = Vec::new();
    for line in stdout.lines() {
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 3 || cols[1].is_empty() {
            continue;
        }
        clients.push(ClientInfo {
            name: cols[0].into(),
            session: cols[1].into(),
            control_mode: matches!(cols[2].trim(), "1" | "true"),
            last_activity: cols.get(3).and_then(|s| s.trim().parse().ok()).unwrap_or(0),
            created: cols.get(4).and_then(|s| s.trim().parse().ok()).unwrap_or(0),
            in_copy_mode: matches!(cols.get(5).map(|s| s.trim()), Some("1" | "true")),
        });
    }
    clients
}

struct PaneScan {
    observation: PaneObservation,
    last_error: Option<TmuxError>,
}

impl PaneScan {
    fn empty() -> Self {
        Self {
            observation: PaneObservation::complete(Vec::new()),
            last_error: None,
        }
    }

    fn add_observation(&mut self, observation: PaneObservation) {
        if !observation.is_complete() {
            self.observation.completeness = ObservationCompleteness::Incomplete;
        }
        self.observation.panes.extend(observation.panes);
    }

    fn add_failure(&mut self, error: TmuxError) {
        self.observation.completeness = ObservationCompleteness::Incomplete;
        self.last_error = Some(error);
    }
}

fn is_stale_socket_error(stderr: &str) -> bool {
    stderr.starts_with("no server running on")
}

fn scan_panes() -> PaneScan {
    // Under `launchd` (gui-domain user agent) tmux's default socket
    // lookup resolves to a different temp dir than the user's
    // interactive shell, so a bare `tmux list-panes -a` finds no server
    // and returns nothing. Enumerate every known socket (the same
    // dedup'd /tmp/tmux-<uid> + /private/tmp/tmux-<uid> set the scanner
    // uses) and aggregate; fall back to the bare call only when no
    // enumerable socket exists (e.g. CI sandboxes).
    let sockets = scanner::enumerate_sockets();
    if sockets.is_empty() {
        return scan_panes_bare();
    }
    let mut scan = PaneScan::empty();
    for sock in &sockets {
        let Some(sock_str) = sock.to_str() else {
            scan.add_failure(TmuxError::BadOutput("non-UTF-8 tmux socket path".into()));
            continue;
        };
        match tmux_output(&["-S", sock_str, "list-panes", "-a", "-F", PANE_FMT]) {
            Ok(o) if o.status.success() => {
                let socket = socket_short_name(sock_str);
                match String::from_utf8(o.stdout) {
                    Ok(stdout) => {
                        let observed = observe_pane_lines_for_socket(&stdout, Some(&socket));
                        if !observed.is_complete() {
                            scan.add_failure(TmuxError::BadOutput(format!(
                                "malformed list-panes output from socket {socket}",
                            )));
                        }
                        scan.add_observation(observed);
                    }
                    Err(error) => {
                        scan.add_failure(TmuxError::BadOutput(error.to_string()));
                    }
                }
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
                // "no server running" is the steady-state for stale socket
                // files left behind by crashed servers; treat as empty
                // rather than a hard error.
                if !is_stale_socket_error(&stderr) {
                    scan.add_failure(TmuxError::NonZero(stderr));
                }
            }
            Err(e) => {
                scan.add_failure(e);
            }
        }
    }
    scan
}

/// Observe all tmux servers with an explicit indication of whether every
/// enumerable server was read successfully. Stale socket files reporting
/// "no server running" count as successful empty sources; hard failures on
/// any socket make the aggregate incomplete even when other sockets yielded
/// useful panes.
pub fn observe_panes() -> PaneObservation {
    scan_panes().observation
}

/// Best-effort pane listing retained for discovery, previews, and legacy
/// callers. Partial multi-socket scans return their successful rows. A scan
/// with no rows and at least one hard failure retains the historical `Err`.
pub fn list_panes() -> Result<Vec<PaneInfo>, TmuxError> {
    let PaneScan {
        observation,
        last_error,
    } = scan_panes();
    if observation.panes.is_empty() {
        if let Some(error) = last_error {
            return Err(error);
        }
    }
    Ok(observation.panes)
}

fn scan_panes_bare() -> PaneScan {
    let out = match tmux_output(&["list-panes", "-a", "-F", PANE_FMT]) {
        Ok(out) => out,
        Err(error) => {
            return PaneScan {
                observation: PaneObservation::incomplete(Vec::new()),
                last_error: Some(error),
            };
        }
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if is_stale_socket_error(&stderr) {
            return PaneScan::empty();
        }
        return PaneScan {
            observation: PaneObservation::incomplete(Vec::new()),
            last_error: Some(TmuxError::NonZero(stderr)),
        };
    }
    let stdout = match String::from_utf8(out.stdout) {
        Ok(stdout) => stdout,
        Err(error) => {
            return PaneScan {
                observation: PaneObservation::incomplete(Vec::new()),
                last_error: Some(TmuxError::BadOutput(error.to_string())),
            };
        }
    };
    let observation = observe_pane_lines_for_socket(&stdout, None);
    let last_error = (!observation.is_complete())
        .then(|| TmuxError::BadOutput("malformed list-panes output".into()));
    PaneScan {
        observation,
        last_error,
    }
}

pub fn list_sessions() -> Result<Vec<SessionInfo>, TmuxError> {
    let out = tmux_output(&["list-sessions", "-F", SESSION_FMT])?;
    if !out.status.success() {
        return Err(TmuxError::NonZero(
            String::from_utf8_lossy(&out.stderr).into(),
        ));
    }
    let stdout = String::from_utf8(out.stdout).map_err(|e| TmuxError::BadOutput(e.to_string()))?;
    Ok(parse_session_lines(&stdout))
}

pub fn list_clients() -> Result<Vec<ClientInfo>, TmuxError> {
    let out = tmux_output(&["list-clients", "-F", CLIENT_FMT])?;
    if !out.status.success() {
        return Err(TmuxError::NonZero(
            String::from_utf8_lossy(&out.stderr).into(),
        ));
    }
    let stdout = String::from_utf8(out.stdout).map_err(|e| TmuxError::BadOutput(e.to_string()))?;
    Ok(parse_client_lines(&stdout))
}

pub fn inside_tmux() -> bool {
    std::env::var_os("TMUX").is_some()
}

/// Capture the visible contents of a tmux pane, preserving ANSI escape
/// sequences so the caller can render colors / attributes faithfully.
///
/// Wraps `tmux capture-pane -ep -t <pane_id>`:
///   - `-p` writes to stdout instead of a paste buffer.
///   - `-e` keeps escape sequences for color and attribute codes — without
///     this the output is plain text and we lose all color information.
///
/// Errors map onto `TmuxError`: spawn failure (no tmux at all), non-zero
/// exit (pane no longer exists, or no tmux server), or non-UTF-8 stdout.
/// The "pane gone" case is the most common in practice — tmux's exit
/// status is non-zero, the stderr is short — so callers should treat
/// `NonZero` as "ephemeral, retry next tick" rather than fatal.
pub fn capture_pane(pane_id: &str) -> Result<String, TmuxError> {
    // DEFAULT-SERVER capture. This is the `muxa watch` preview + web dashboard
    // path, which must always hit the default tmux server (whatever the user's
    // interactive `$TMUX_TMPDIR` / `default` socket resolves to) — NOT a
    // pane-row-recorded socket. Kept on `tmux_output` (no `-S`): a PR that
    // scoped this to `MUXA_TMUX_SOCKET` regressed the preview/dashboard when a
    // stale env socket was set in the daemon.
    //
    // The control-plane `capture` IPC uses [`capture_pane_on`] instead, which
    // pins the specific server a pane lives on. The two paths are deliberately
    // distinct — see that function.
    let out = tmux_output(&["capture-pane", "-ep", "-t", pane_id])?;
    if !out.status.success() {
        return Err(TmuxError::NonZero(
            String::from_utf8_lossy(&out.stderr).into(),
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| TmuxError::BadOutput(e.to_string()))
}

/// Capture the visible contents of a pane on the specific tmux server named by
/// `socket` (a pane row's recorded short socket name; `None` ⇒ env-scoped
/// default). Backs the tmux [`crate::backend::PaneBackend::capture_pane_on`]
/// capability and the control-plane `capture` IPC, whose whole point is to read
/// the RIGHT `%5` when the pane id exists on several servers. Same `-ep`
/// flags / error mapping as [`capture_pane`]; only the server targeting differs.
pub fn capture_pane_on(socket: Option<&str>, pane_id: &str) -> Result<String, TmuxError> {
    let mut cmd = tmux_command_targeting(socket);
    cmd.args(["capture-pane", "-ep", "-t", pane_id]);
    let out = command_output_with_timeout(
        cmd,
        TMUX_COMMAND_TIMEOUT,
        format!("tmux capture-pane -t {pane_id}"),
    )?;
    if !out.status.success() {
        return Err(TmuxError::NonZero(
            String::from_utf8_lossy(&out.stderr).into(),
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| TmuxError::BadOutput(e.to_string()))
}

/// Best-effort resolution of the user's active pane.
///
/// `$TMUX_PANE` covers the common case (a shell running inside that pane).
/// But tmux does NOT propagate it to processes spawned by `run-shell`, key
/// bindings, or `display-popup` — including the
/// `bind-key s display-popup -E "muxa watch"` recipe shipped in
/// `examples/muxa.tmux.conf`. In those contexts we ask tmux directly,
/// scoping the query to the session id parsed out of `$TMUX` so that with
/// multiple attached clients we still return the active pane of the
/// session that triggered the binding (rather than tmux's most-recently-
/// active client, which `display-message` defaults to).
/// The client tmux considers "current", e.g. `/dev/pts/87`.
///
/// This is a guess, not an identity: tmux resolves the current client
/// from recent activity, so with two terminals attached the answer is
/// routinely the *other* one — especially from inside a `display-popup`,
/// which runs detached from the client that opened it. The only reliable
/// identity is `#{client_name}` expanded by the key binding at the
/// keypress and passed in (see `muxa watch --caller-client`); use this
/// fallback only when no such value exists.
pub fn current_client() -> Option<String> {
    let out = tmux_output(&["display-message", "-p", "#{client_name}"]).ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8(out.stdout).ok()?;
    let client = stdout.lines().next().unwrap_or("").trim();
    (!client.is_empty()).then(|| client.to_string())
}

pub fn current_pane() -> Option<String> {
    if let Some(p) = std::env::var("TMUX_PANE").ok().filter(|s| !s.is_empty()) {
        return Some(p);
    }
    // `display-popup` runs its command with an empty `TMUX_PANE`, so watch
    // launched from `prefix+s` always lands here — and `$TMUX` is the
    // answer, not a fallback. tmux rewrites its session field to the
    // popup's own session, so it names where the popup actually is.
    //
    // The tempting alternative — asking tmux for the current client's pane
    // — is wrong precisely when it matters. An unpinned `display-message`
    // resolves against whichever client tmux last saw activity on, so with
    // two terminals attached it answers for the other one. Measured inside
    // a single popup on one session, `$TMUX` gave that session's pane while
    // the unpinned query gave a pane from an unrelated session the other
    // terminal happened to be showing. This pane seeds the collaboration
    // room and the opening cursor, so the wrong answer moves the user's
    // entire frame of reference to a window they are not in.
    let target = parse_tmux_session_target(&std::env::var("TMUX").ok()?)?;
    let out = tmux_output(&["display-message", "-p", "-t", &target, "#{pane_id}"]).ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8(out.stdout).ok()?;
    let pane = stdout.lines().next().unwrap_or("").trim();
    if pane.is_empty() {
        None
    } else {
        Some(pane.to_string())
    }
}

/// Parse a tmux target spec for the session this client is attached to,
/// out of the `$TMUX` env var. `$TMUX` is `socket_path,server_pid,session_id`
/// where `session_id` is numeric; tmux accepts `$<id>` as a session target.
///
/// Returns `None` when the env var is malformed or the trailing field isn't
/// a plain decimal id, so callers fall back to "no target known".
fn parse_tmux_session_target(tmux_env: &str) -> Option<String> {
    let sid = tmux_env.rsplit(',').next()?;
    if sid.is_empty() || !sid.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(format!("${sid}"))
}

/// Resolve a raw tmux `pane_id` (e.g. `%42`) to its full `PaneInfo`.
///
/// Returns `None` if tmux is unavailable, if the pane no longer exists, or
/// if `list-panes` fails for any other reason. Callers should be ready to
/// fall back to showing the raw id.
pub fn resolve_pane(pane_id: &str) -> Option<PaneInfo> {
    list_panes()
        .ok()?
        .into_iter()
        .find(|p| p.pane_id == pane_id)
}

/// Map of tmux pane shell PIDs → tmux pane id (e.g. `12345 -> "%42"`).
///
/// Used by the hook adapter's ancestry-walk fallback in
/// [`crate::adapters::hook::run_hook`]: when `TMUX_PANE` isn't in the
/// hook process's environment, walk parent PIDs and look them up here
/// to recover the pane the SDK-spawned agent actually belongs to.
///
/// Empty on any failure (tmux unavailable, server down, etc.). Callers
/// treat the empty map as "no fallback available" and proceed with
/// `pane: None`.
pub fn pane_pid_map() -> std::collections::HashMap<u32, String> {
    let out = match tmux_output(&["list-panes", "-a", "-F", "#{pane_pid}\t#{pane_id}"]) {
        Ok(o) if o.status.success() => o.stdout,
        _ => return std::collections::HashMap::new(),
    };
    let Ok(stdout) = String::from_utf8(out) else {
        return std::collections::HashMap::new();
    };
    parse_pane_pid_map(&stdout)
}

/// Pulled out for direct unit testing — `pane_pid_map` itself shells
/// out to `tmux`.
pub(crate) fn parse_pane_pid_map(stdout: &str) -> std::collections::HashMap<u32, String> {
    let mut out = std::collections::HashMap::new();
    for line in stdout.lines() {
        let mut cols = line.split('\t');
        let Some(pid_str) = cols.next() else { continue };
        let Some(pane_id) = cols.next() else { continue };
        let Ok(pid) = pid_str.trim().parse::<u32>() else {
            continue;
        };
        out.insert(pid, pane_id.into());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locks the `send-keys` argv shape: literal (`-l`) injection targeting
    /// the pane id, with `--` terminating options and the text passed verbatim
    /// as the final arg. `-l` keeps prompt text from being reinterpreted as a
    /// tmux key name; `--` keeps a leading `-` from being parsed as a flag.
    #[test]
    fn send_keys_argv_is_literal_and_targeted() {
        assert_eq!(
            send_keys_argv("%12", "fix the bug"),
            ["send-keys", "-t", "%12", "-l", "--", "fix the bug"],
        );
        // A bare carriage return is the submit form (send-keys Enter equiv).
        assert_eq!(
            send_keys_argv("%3", "\r"),
            ["send-keys", "-t", "%3", "-l", "--", "\r"],
        );
    }

    /// Hostile argv shapes the `--` terminator must neutralize: text that
    /// *begins* with a dash (would otherwise be read as a send-keys flag),
    /// a bare semicolon, and the empty string. MCP forwards arbitrary model
    /// text, so these are real inputs — the argv always keeps `--` in front
    /// of the payload so the text is positional, never optional.
    #[test]
    fn send_keys_argv_terminates_options_for_hostile_text() {
        // Leading dash: `--` makes `-rf x` the literal text, not flags.
        assert_eq!(
            send_keys_argv("%1", "-rf x"),
            ["send-keys", "-t", "%1", "-l", "--", "-rf x"],
        );
        // Bare semicolon: still placed after `--` (runtime routing sends it
        // via the paste path — see `needs_paste` — because a *trailing* `;`
        // is eaten by tmux's command splitter regardless of `--`).
        assert_eq!(
            send_keys_argv("%1", ";"),
            ["send-keys", "-t", "%1", "-l", "--", ";"],
        );
        // Empty string: a harmless no-op injection, shape unchanged.
        assert_eq!(
            send_keys_argv("%1", ""),
            ["send-keys", "-t", "%1", "-l", "--", ""],
        );
    }

    /// The paste-vs-send-keys routing predicate: multi-line text and text with
    /// a trailing `;` must take the paste path (both are corrupted by
    /// `send-keys -l`); everything else — including an embedded `;`, a leading
    /// dash, the empty string, and the lone submit CR — stays on the fast path.
    #[test]
    fn needs_paste_flags_newline_and_trailing_semicolon() {
        assert!(needs_paste("line one\nline two"));
        assert!(needs_paste("run this;"));
        assert!(needs_paste(";")); // lone `;` ends with `;`
        assert!(!needs_paste("a;b")); // embedded `;` is safe
        assert!(!needs_paste("-rf x")); // leading dash handled by `--`
        assert!(!needs_paste("")); // empty is a no-op, not hazardous
        assert!(!needs_paste("\r")); // lone submit CR stays on send-keys
        assert!(!needs_paste("plain text"));
    }

    #[test]
    fn bounded_command_returns_fast_output() {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "printf ok"]);
        let out = command_output_with_timeout(
            cmd,
            Duration::from_secs(1),
            "test fast command".to_string(),
        )
        .expect("fast command should complete");

        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout), "ok");
    }

    /// Regression: the wait loop used to poll `try_wait` without reading the
    /// child's pipes, so any output past the pipe capacity blocked tmux in
    /// `write` forever and the helper killed it at the timeout. `list_panes`
    /// then returned nothing and every status row fell back to a raw `%42`.
    /// 1 MiB clears both the 64 KB default capacity and the one-page minimum
    /// a host over `fs.pipe-user-pages-soft` hands out.
    #[test]
    fn bounded_command_drains_output_larger_than_a_pipe_buffer() {
        const MIB: usize = 1024 * 1024;
        let mut cmd = std::process::Command::new("sh");
        cmd.args([
            "-c",
            "dd if=/dev/zero bs=1024 count=1024 2>/dev/null | tr '\\0' 'x'",
        ]);
        let out = command_output_with_timeout(
            cmd,
            Duration::from_secs(30),
            "test large output".to_string(),
        )
        .expect("output larger than a pipe buffer must not time out");

        assert!(out.status.success());
        assert_eq!(out.stdout.len(), MIB);
    }

    /// Same deadlock, stderr side: a child that only writes to stderr must
    /// not stall either, and its bytes must survive into `Output`.
    #[test]
    fn bounded_command_drains_large_stderr() {
        const MIB: usize = 1024 * 1024;
        let mut cmd = std::process::Command::new("sh");
        cmd.args([
            "-c",
            "dd if=/dev/zero bs=1024 count=1024 2>/dev/null | tr '\\0' 'x' >&2",
        ]);
        let out = command_output_with_timeout(
            cmd,
            Duration::from_secs(30),
            "test large stderr".to_string(),
        )
        .expect("stderr larger than a pipe buffer must not time out");

        assert!(out.status.success());
        assert_eq!(out.stderr.len(), MIB);
        assert!(out.stdout.is_empty());
    }

    #[test]
    fn bounded_command_times_out_slow_process() {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "sleep 2"]);
        let started = Instant::now();
        let err = command_output_with_timeout(
            cmd,
            Duration::from_millis(50),
            "test slow command".to_string(),
        )
        .expect_err("slow command should time out");

        assert!(matches!(err, TmuxError::Timeout { .. }));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn parses_pane_pid_map_well_formed() {
        let stdout = "12345\t%10\n67890\t%11\n";
        let map = parse_pane_pid_map(stdout);
        assert_eq!(map.get(&12345).map(String::as_str), Some("%10"));
        assert_eq!(map.get(&67890).map(String::as_str), Some("%11"));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn parse_pane_pid_map_skips_garbage_lines() {
        let stdout = "12345\t%10\nnot-a-pid\t%99\n67890\t%11\n\n";
        let map = parse_pane_pid_map(stdout);
        // The malformed pid line is dropped without aborting the parse.
        assert_eq!(map.len(), 2);
        assert!(map.contains_key(&12345));
        assert!(map.contains_key(&67890));
    }

    #[test]
    fn parses_existing_pane_lines_format() {
        // Sanity-check that the older `parse_pane_lines` still handles
        // the existing 7-column format — protects against accidental
        // breakage when adding the new pid map alongside it.
        let stdout = "%10\tmain\t0\t0\t/dev/pts/0\tclaude\tclaude session\n";
        let panes = parse_pane_lines(stdout);
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].pane_id, "%10");
        assert_eq!(panes[0].current_command, "claude");
    }

    #[test]
    fn parses_stable_session_and_window_identity() {
        let stdout = "%10\tmain\t2\t0\t/dev/pts/0\tclaude\tclaude session\t42\t/repo\t$3\t@7\tauth-refactor\n";
        let panes = parse_pane_lines(stdout);
        assert_eq!(panes[0].session_id, "$3");
        assert_eq!(panes[0].window_id, "@7");
        assert_eq!(panes[0].window_name, "auth-refactor");
    }

    #[test]
    fn pane_observation_retains_valid_rows_but_marks_malformed_mix_incomplete() {
        let stdout = "%10\tmain\t0\t0\t/dev/pts/0\tclaude\tclaude session\n\
                      locale_mangled_row_without_tabs\n";
        let observed = observe_pane_lines_for_socket(stdout, Some("default"));

        assert_eq!(observed.panes.len(), 1);
        assert_eq!(observed.panes[0].socket.as_deref(), Some("default"));
        assert!(!observed.is_complete());
    }

    #[test]
    fn multi_socket_scan_retains_partial_rows_and_marks_hard_failure_incomplete() {
        let mut scan = PaneScan::empty();
        scan.add_observation(observe_pane_lines_for_socket(
            "%10\tmain\t0\t0\t/dev/pts/0\tclaude\tclaude session\n",
            Some("default"),
        ));
        scan.add_failure(TmuxError::Timeout {
            command: "tmux -S amux list-panes".into(),
            timeout: Duration::from_secs(1),
        });

        assert_eq!(scan.observation.panes.len(), 1);
        assert!(!scan.observation.is_complete());
        assert!(scan.last_error.is_some());
    }

    #[test]
    fn stale_socket_empty_result_keeps_scan_complete() {
        // The production stale-socket branch deliberately adds neither an
        // observation nor a failure. Starting from an empty aggregate thus
        // represents its authoritative empty-success result.
        assert!(is_stale_socket_error(
            "no server running on /tmp/tmux-501/stale",
        ));
        assert!(!is_stale_socket_error("permission denied"));
        let scan = PaneScan::empty();
        assert!(scan.observation.panes.is_empty());
        assert!(scan.observation.is_complete());
        assert!(scan.last_error.is_none());
    }

    #[test]
    fn parses_session_lines() {
        let stdout = "$1\tmain\t1\n$2\twork\t0\n";
        let sessions = parse_session_lines(stdout);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_id, "$1");
        assert_eq!(sessions[0].name, "main");
        assert_eq!(sessions[0].attached_clients, 1);
        assert_eq!(sessions[1].attached_clients, 0);
    }

    #[test]
    fn parses_client_lines() {
        // name \t session \t control_mode \t client_activity \t client_created
        let stdout = "/dev/pts/0\tmain\t0\t1780900000\t1780899000\n\
                      /dev/pts/1\twork\t1\t1780900050\t1780899100\n\
                      /dev/pts/2\t\t0\t0\t0\n";
        let clients = parse_client_lines(stdout);
        // The third row has an empty session and is skipped.
        assert_eq!(clients.len(), 2);
        assert_eq!(clients[0].name, "/dev/pts/0");
        assert_eq!(clients[0].session, "main");
        assert!(!clients[0].control_mode);
        assert_eq!(clients[0].last_activity, 1_780_900_000);
        assert_eq!(clients[0].created, 1_780_899_000);
        assert_eq!(clients[1].session, "work");
        assert!(clients[1].control_mode);
    }

    #[test]
    fn parse_client_lines_tolerates_missing_trailing_columns() {
        // Older tmux (no activity/created) still yields a valid row, epochs 0.
        let clients = parse_client_lines("/dev/pts/0\tmain\t0\n");
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].last_activity, 0);
        assert_eq!(clients[0].created, 0);
    }

    #[test]
    fn parses_session_id_from_tmux_env() {
        assert_eq!(
            parse_tmux_session_target("/tmp/tmux-1044/default,82477,475"),
            Some("$475".into())
        );
    }

    #[test]
    fn rejects_malformed_tmux_env() {
        // Missing fields, non-numeric session id, empty string — all fall
        // through to None so the caller knows it can't scope the query.
        assert_eq!(parse_tmux_session_target(""), None);
        assert_eq!(parse_tmux_session_target("/tmp/sock,82477,abc"), None);
        assert_eq!(parse_tmux_session_target("/tmp/sock,82477,"), None);
    }
}
