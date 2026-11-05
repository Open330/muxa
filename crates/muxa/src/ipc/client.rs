use super::*;

/// One list transport result. Legacy rows are exposed only so a multi-page
/// consumer can select locally instead of downloading the same mailbox again.
#[derive(Debug)]
pub struct CollaborationQueryResult {
    pub byte_budget_supported: bool,
    pub page: collaboration::CollaborationPage,
    pub scope: MailboxScope,
    pub legacy_requests: Option<Vec<CollaborationRequest>>,
}

/// Client-side helper. Single-shot request/response.
#[derive(Debug, Clone)]
pub struct Client {
    pub(super) socket_path: PathBuf,
    pub(super) collaboration_client_kind: CollaborationClientKind,
    pub(super) mailbox_cache: Option<Arc<tokio::sync::Mutex<mailbox_cache::MailboxCache>>>,
}

/// Identity and feature information returned by the daemon's `hello` method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub collaboration_error: Option<String>,
    pub capabilities: Vec<String>,
    pub generation: Option<u64>,
    /// The daemon's crate version. `None` from a daemon built before the
    /// field existed — which already means it is older than this client.
    pub version: Option<String>,
    /// The protocol the connection settled on, which is the daemon's ceiling
    /// when it is older than this build.
    pub protocol: u32,
}

impl Hello {
    /// The version skew between the daemon that answered this `hello` and the
    /// client asking, or `None` when the two builds agree.
    ///
    /// Separate from protocol negotiation on purpose. The protocol number only
    /// moves when the wire format changes, so a daemon left running across an
    /// upgrade can keep answering every request correctly-shaped while running
    /// months-old logic. That window is silent today: nothing fails, so nothing
    /// reports. Comparing crate versions closes it.
    #[must_use]
    pub fn version_skew(&self) -> Option<VersionSkew> {
        let client = env!("CARGO_PKG_VERSION");
        match self.version.as_deref() {
            Some(daemon) if daemon == client => None,
            daemon => Some(VersionSkew {
                daemon: daemon.map(str::to_string),
                client,
            }),
        }
    }
}

/// A daemon and a client that are not the same build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionSkew {
    /// The running daemon's version, or `None` when it is old enough not to
    /// report one at all.
    pub daemon: Option<String>,
    /// This binary's version.
    pub client: &'static str,
}

impl VersionSkew {
    /// How the daemon's side reads in a message: its version, or a phrase for
    /// the pre-`version` builds that cannot name themselves.
    #[must_use]
    pub fn daemon_label(&self) -> String {
        self.daemon
            .clone()
            .unwrap_or_else(|| "an older build".to_string())
    }
}

impl std::fmt::Display for VersionSkew {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "muxad is running {} while this CLI is {} — {}",
            self.daemon_label(),
            self.client,
            restart_remedy()
        )
    }
}

/// The result of a [`Client::send_prompt`]: the two non-atomic keystroke
/// injections (the text, then the optional submit CR) reported distinctly.
///
/// Only produced on `Ok`, i.e. when the text landed. `submitted` is `false`
/// either because `submit:false` was requested (nothing to submit) or because
/// a requested submit CR failed after the text landed — a **partial failure**
/// the caller distinguishes using its own `submit` intent. Either way, when
/// this value exists the text is already in the pane and MUST NOT be resent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendPromptOutcome {
    /// The text injection landed. Always `true` here (a failed text send is an
    /// `Err`); carried explicitly for symmetry and forward-compatibility.
    pub sent: bool,
    /// The host accepted the submit carriage return. This alone does not
    /// establish that the application committed the prompt.
    pub submitted: bool,
    pub submission_confirmed: Option<bool>,
}

/// Long-lived handle returned by [`Client::subscribe`]. Calls to
/// [`Self::recv`] yield successive `Transition`s as they happen on
/// the daemon. Returns `Ok(None)` when the daemon closes the
/// connection (shutdown) or `Err(_)` on a parse / IO failure that
/// the caller will probably want to handle by reconnecting.
pub struct TransitionStream {
    pub(super) reader: BufReader<ReadHalf>,
    pub(super) line: String,
}

/// Long-lived compact invalidation stream returned by
/// [`Client::fleet_subscribe`]. Callers fetch a coherent snapshot after
/// coalescing one or more updates.
pub struct FleetUpdateStream {
    pub(super) reader: BufReader<ReadHalf>,
    pub(super) line: String,
}

/// Content-free durable mailbox revision stream.
pub struct CollaborationUpdateStream {
    pub(super) reader: BufReader<ReadHalf>,
    pub(super) line: String,
}

/// Whether a subscribe-stream line is the daemon's `lagged` control marker
/// (`{"event":"lagged",…}`) rather than a `Transition`. Kept cheap: a real
/// `Transition` is tagged by `from`/`to`, never an `event` field, so a
/// single parse-and-check disambiguates without a speculative `Transition`
/// deserialize.
pub(super) fn is_lagged_marker(line: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|v| {
            v.get("event")
                .and_then(serde_json::Value::as_str)
                .map(|e| e == "lagged")
        })
        .unwrap_or(false)
}

/// The highest protocol a daemon named in its `unsupported protocol` refusal.
///
/// The refusal reads `unsupported protocol: server supports [1,4] client=6`.
/// Only the upper bound matters — it is the version the caller must step down
/// to. Parsing the daemon's own words beats guessing `PROTOCOL_VERSION - 1`,
/// which would need as many round trips as the skew is wide.
pub(super) fn server_protocol_ceiling(error: Option<&str>) -> Option<u32> {
    let error = error?;
    if !error.starts_with("unsupported protocol") {
        return None;
    }
    let range = error.split_once('[')?.1.split_once(']')?.0;
    range.split_once(',')?.1.trim().parse().ok()
}

/// Old daemons deserialize a request kind they do not know as a tagged-enum
/// `unknown variant` error. Limit compatibility fallback to that exact shape:
/// authorization, lookup, and persistence failures from a current daemon must
/// remain visible to the caller rather than being disguised as a poll loop.
pub(super) fn collaboration_wait_is_unsupported(error: &str) -> bool {
    error.contains("unknown variant") && error.contains("collaboration_wait")
}

/// Build the error for an `ok: false` response, from the daemon's own message.
///
/// A protocol mismatch gets a hint appended. It is the one refusal whose
/// message names a cause the reader cannot act on — two version numbers say
/// nothing about which half is stale or how to move it — and it is the
/// refusal a mixed install produces every single call.
pub(super) fn response_error(resp: &serde_json::Value) -> RuntimeError {
    let message = resp["error"].as_str().unwrap_or("request failed");
    if message.starts_with("protocol mismatch") {
        RuntimeError::Daemon(format!(
            "{message} — the running daemon is older than this CLI. {}",
            restart_remedy()
        ))
    } else {
        RuntimeError::Daemon(message.to_string())
    }
}

/// Whether this binary was installed by Homebrew.
///
/// Read off the resolved path of the running executable: Homebrew installs
/// into a versioned Cellar directory and links the result onto `PATH`, so a
/// canonicalized `current_exe()` lands inside `…/Cellar/…` no matter which
/// prefix (`/opt/homebrew`, `/usr/local`, or a custom `HOMEBREW_PREFIX`) the
/// machine uses. Checking the Cellar segment rather than a hardcoded prefix
/// keeps Linuxbrew and relocated installs working.
pub(super) fn installed_by_homebrew() -> bool {
    std::env::current_exe()
        .and_then(crate::paths::canonicalize)
        .is_ok_and(|exe| {
            exe.components()
                .any(|component| component.as_os_str() == "Cellar")
        })
}

/// The remedy sentence for a daemon that is out of step with this CLI.
///
/// `muxa daemon restart` is the answer in every case: the new binary is
/// already on disk after any install method, and only the process is stale.
/// It used to say `muxa upgrade` instead, which is actively wrong on a
/// Homebrew install — `muxa upgrade` is `git pull` + `cargo install`, writing
/// to `~/.cargo/bin`, which the Homebrew prefix shadows on `PATH`. That
/// builds a binary the user never executes and leaves the running daemon
/// exactly as stale as before.
pub(super) fn restart_remedy() -> String {
    if installed_by_homebrew() {
        "Restart it: `muxa daemon restart`. `brew upgrade` replaces the binary \
         on disk but never the process already running on it."
            .to_string()
    } else {
        "Restart it: `muxa daemon restart`. If the versions still disagree \
         afterwards, the installed binary itself is stale — `muxa upgrade` \
         rebuilds and reinstalls it (add `--no-pull` to build the current source)."
            .to_string()
    }
}

/// Decode the `agents` array out of a response, or surface the daemon's error.
///
/// The `ok` check is the point. A refusal and a genuinely empty registry are
/// indistinguishable to a lenient decoder — neither carries an `agents` array
/// — so returning `Vec::new()` for both turned every failure into "no active
/// agents": a confident, wrong, and perfectly stable answer that looks like
/// news about the user's agents rather than a broken connection.
///
/// Measured on a live host: a `muxad` built before the protocol 5 bump
/// answered `protocol mismatch: server=4 client=5` to every request, and
/// `muxa status` reported no agents for a full day while 58 were registered
/// and the daemon was writing all 58 to its state snapshot every 30 seconds.
pub(super) fn decode_agents(resp: &serde_json::Value) -> Result<Vec<Agent>, RuntimeError> {
    if !resp["ok"].as_bool().unwrap_or(false) {
        return Err(response_error(resp));
    }
    let Some(agents) = resp["agents"].as_array().cloned() else {
        return Ok(Vec::new());
    };
    serde_json::from_value(serde_json::Value::Array(agents)).map_err(RuntimeError::Json)
}

impl TransitionStream {
    /// Receive an invalidation from `subscribe_agent_changes`, skipping keepalives.
    pub async fn recv_change(&mut self) -> Result<Option<()>, RuntimeError> {
        loop {
            self.line.clear();
            if read_limited_line(&mut self.reader, &mut self.line).await? == 0 {
                return Ok(None);
            }
            if self.line.trim().is_empty() {
                continue;
            }
            let frame: serde_json::Value = serde_json::from_str(self.line.trim())?;
            if frame["event"] != "agents_changed" {
                return Err(RuntimeError::Json(serde::de::Error::custom(
                    "unexpected agent change frame",
                )));
            }
            return Ok(Some(()));
        }
    }

    /// Wait for and return the next streamed `Transition`.
    ///
    /// Blank lines are the daemon's keepalive frames (a bare newline it emits
    /// on an idle stream to detect dead clients); they carry no payload, so we
    /// skip them and keep waiting for the next real transition.
    pub async fn recv(&mut self) -> Result<Option<crate::state::Transition>, RuntimeError> {
        loop {
            self.line.clear();
            let n = read_limited_line(&mut self.reader, &mut self.line).await?;
            if n == 0 {
                return Ok(None);
            }
            let trimmed = self.line.trim();
            if trimmed.is_empty() {
                continue;
            }
            // The daemon interleaves non-transition control frames on this
            // stream: a bare newline keepalive (handled above) and a lagged
            // marker (`{"event":"lagged","dropped":N}`) after a broadcast
            // overflow. Skip the lagged marker — the caller reconciles holes
            // via its fallback snapshot poll — so it never reaches the
            // `Transition` deserializer below.
            if is_lagged_marker(trimmed) {
                tracing::debug!("subscribe stream lagged; skipping marker");
                continue;
            }
            let t: crate::state::Transition = serde_json::from_str(trimmed)?;
            return Ok(Some(t));
        }
    }
}

impl FleetUpdateStream {
    pub async fn recv(&mut self) -> Result<Option<FleetUpdate>, RuntimeError> {
        loop {
            self.line.clear();
            let n = read_limited_line(&mut self.reader, &mut self.line).await?;
            if n == 0 {
                return Ok(None);
            }
            let trimmed = self.line.trim();
            if trimmed.is_empty() {
                continue;
            }
            return serde_json::from_str(trimmed)
                .map(Some)
                .map_err(RuntimeError::Json);
        }
    }
}

impl CollaborationUpdateStream {
    pub async fn recv(&mut self) -> Result<Option<u64>, RuntimeError> {
        loop {
            self.line.clear();
            let n = read_limited_line(&mut self.reader, &mut self.line).await?;
            if n == 0 {
                return Ok(None);
            }
            let trimmed = self.line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let frame: serde_json::Value = serde_json::from_str(trimmed)?;
            return frame["revision"].as_u64().map(Some).ok_or_else(|| {
                RuntimeError::Json(serde::de::Error::custom(
                    "collaboration update is missing revision",
                ))
            });
        }
    }
}
mod agents;
mod ask;
#[path = "client/collaboration.rs"]
mod collaboration_requests;
mod config;
mod fleet;
mod prompts;
mod sessions;
mod swap;
mod transport;
mod work;
