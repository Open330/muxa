use super::{transport, Duration, Path, PROTOCOL_VERSION};

/// True for the "too many open files" family of `accept()` errors:
/// `EMFILE` (this process hit its fd limit) or `ENFILE` (system-wide table
/// full). Both are transient — shedding load frees descriptors — so the
/// accept loop backs off and retries rather than treating them as fatal.
pub(super) fn is_fd_exhaustion(e: &std::io::Error) -> bool {
    // `ErrorKind` has no stable variant for either, so match the raw errno.
    // EMFILE = 24, ENFILE = 23 on both Linux and macOS.
    matches!(e.raw_os_error(), Some(24 | 23))
}

/// After `UnixListener::bind`, chmod the path so only the owner can connect.
/// One blocking request/response against the daemon, for callers that have no
/// runtime to await on.
///
/// `agent_launch::start` and the `work up` pipeline stamp a pane's explicit
/// alias from synchronous code that is reached from both async and blocking
/// contexts, and threading a runtime through every one of them to register a
/// name would be a large change in service of one small call. The wire is
/// line-delimited JSON over a Unix socket, so a synchronous client is a
/// connect, a write, and a read.
///
/// Every failure is `None`: the caller's job is to name a pane, and losing
/// the daemon means it names it without the arbiter's blessing rather than
/// not at all.
#[cfg(not(unix))]
pub fn blocking_call(
    socket_path: &Path,
    req: &serde_json::Value,
    deadline: Duration,
) -> Option<serde_json::Value> {
    // A named pipe opened as a file cannot bound a read, so the call runs on
    // the async transport instead, on a thread of its own: the caller may
    // itself be inside a runtime, which cannot be blocked on from within.
    let socket_path = socket_path.to_path_buf();
    let mut bytes = serde_json::to_vec(req).ok()?;
    bytes.push(b'\n');
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        runtime.block_on(async move {
            tokio::time::timeout(deadline, async {
                let (read, mut write) = Stream::connect(&socket_path).await.ok()?.into_split();
                write.write_all(&bytes).await.ok()?;
                write.flush().await.ok()?;
                let mut line = String::new();
                let read = BufReader::new(read).read_line(&mut line).await.ok()?;
                (read != 0)
                    .then(|| serde_json::from_str(&line).ok())
                    .flatten()
            })
            .await
            .ok()
            .flatten()
        })
    })
    .join()
    .ok()
    .flatten()
}

#[cfg(unix)]
pub fn blocking_call(
    socket_path: &Path,
    req: &serde_json::Value,
    deadline: Duration,
) -> Option<serde_json::Value> {
    use std::io::{BufRead, BufReader as SyncBufReader, Write};
    let mut stream = transport::blocking_connect(socket_path, deadline).ok()?;
    let mut bytes = serde_json::to_vec(req).ok()?;
    bytes.push(b'\n');
    stream.write_all(&bytes).ok()?;
    stream.flush().ok()?;
    let mut line = String::new();
    let read = SyncBufReader::new(&stream).read_line(&mut line).ok()?;
    if read == 0 {
        return None;
    }
    serde_json::from_str(&line).ok()
}

/// Register an explicit alias with the room's namespace arbiter.
///
/// `Ok(())` means the room accepted it, `Err` that another pane already
/// answers to that name. `Ok(())` is also what a missing or older daemon
/// yields — an explicit alias comes from the user's own configuration, so a
/// daemon that cannot referee is no reason to refuse to honour it.
pub fn blocking_reserve_handle(
    socket_path: &Path,
    pane: &str,
    mux_socket: Option<&str>,
    handle: &str,
    deadline: Duration,
) -> Result<(), String> {
    let req = serde_json::json!({
        "protocol": PROTOCOL_VERSION,
        "kind": "collaboration_issue_handle",
        "pane": pane,
        "socket": mux_socket,
        "request": { "reserve": { "handle": handle } },
    });
    let Some(resp) = blocking_call(socket_path, &req, deadline) else {
        return Ok(());
    };
    if resp["ok"].as_bool() == Some(true) {
        return Ok(());
    }
    match resp["error"].as_str() {
        Some(message) if message.contains("already used by a live peer") => {
            Err(message.to_string())
        }
        // Anything else is the daemon failing to referee rather than
        // refusing: an unknown request kind on an older build, a room it
        // cannot resolve. Honour the caller's own configuration.
        _ => Ok(()),
    }
}

pub fn harden_permissions(socket_path: &Path) -> std::io::Result<()> {
    transport::harden_permissions(socket_path)
}
