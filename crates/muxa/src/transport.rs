//! Local IPC transport: a Unix-domain socket, or a Windows named pipe.
//!
//! [`crate::ipc`] speaks line-delimited JSON (see `PROTOCOL.md`) and never
//! needed a *socket* specifically — it needs a local, owner-private,
//! full-duplex byte stream that a client can address by a path the daemon also
//! knows. Fleet already carries the same shape of protocol over an SSH stdio
//! channel, so the encoding was never the coupled part. The types were:
//! `UnixStream` and `OwnedWriteHalf` appeared directly in handler signatures.
//!
//! This module is that coupling, isolated. It deliberately exposes **one**
//! `Stream`/`ReadHalf`/`WriteHalf` family rather than a generic parameter, so
//! `ipc.rs` keeps concrete types in its signatures and gains no type
//! parameters — its dispatch table is unchanged apart from the names it
//! mentions.
//!
//! # Differences a caller can observe
//!
//! | | Unix | Windows |
//! | --- | --- | --- |
//! | Address | the socket path | a pipe name derived from that path |
//! | Exclusivity | bind fails on an existing path | `first_pipe_instance` |
//! | Protection | `chmod 0600` after bind | creation flags + the default DACL |
//! | Peer identity | `SO_PEERCRED` | none — see [`PeerIdentity`] |
//! | Teardown | unlink the socket file | the pipe dies with its last handle |
//! | Blocking client | a second synchronous socket | unsupported |
//!
//! Windows is not a supported host (see `docs/WINDOWS.md`). This exists so the
//! transport stops being the reason it cannot be, and so the differences above
//! live in one place instead of being rediscovered later.

use std::io;
use std::path::Path;
use std::time::Duration;

pub use imp::{BlockingStream, Listener, ReadHalf, Stream, WriteHalf};

/// Credentials of the process on the other end of a connection.
///
/// Populated from `SO_PEERCRED` on Unix. Every field is `None` on Windows:
/// recovering the client pid needs `GetNamedPipeClientProcessId`, and the
/// workspace sets `unsafe_code = "forbid"`, so calling it would take a new
/// dependency. Callers already treat missing credentials as an unknown actor —
/// `observe_collaboration_actor` maps `None` to
/// `CollaborationClientKind::Unknown` — so this is the existing degradation
/// rather than a new path. It does leave collaboration provenance weaker on
/// Windows, which is one more reason it is not a supported host.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PeerIdentity {
    pub pid: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}

/// Restrict an already-bound endpoint to its owner.
///
/// Kept as a standalone entry point because `muxad` re-applies it after the
/// listener appears. A no-op on Windows, where protection is fixed at creation
/// time — see [`Listener::bind`].
pub fn harden_permissions(endpoint: &Path) -> io::Result<()> {
    imp::harden(endpoint)
}

/// Whether a daemon is currently accepting on `endpoint`.
///
/// Used to tell a live daemon from a leftover socket file before binding. A
/// live endpoint means "already running"; anything else is clearable.
pub fn endpoint_is_live(endpoint: &Path) -> bool {
    imp::probe_live(endpoint)
}

/// Remove a dead endpoint so a bind can succeed.
///
/// Only Unix leaves a file behind; a named pipe vanishes with its last handle,
/// so there is nothing to clear.
pub fn clear_endpoint(endpoint: &Path) -> io::Result<()> {
    imp::clear(endpoint)
}

/// Best-effort teardown on daemon shutdown. Failures are the caller's to
/// ignore — the next startup clears a stale endpoint anyway.
pub fn remove_endpoint(endpoint: &Path) {
    let _ = imp::clear(endpoint);
}

/// Connect a synchronous client, with `deadline` applied to reads and writes.
///
/// For callers with no runtime to await on. Returns
/// [`io::ErrorKind::Unsupported`] on Windows: a named pipe opened as a file has
/// no way to bound a read without an unsafe `SetCommTimeouts`, and a transport
/// that can hang the caller forever is worse than one that declines. The single
/// caller ([`crate::ipc::blocking_call`]) already degrades every failure to
/// `None`.
pub fn blocking_connect(endpoint: &Path, deadline: Duration) -> io::Result<BlockingStream> {
    imp::blocking_connect(endpoint, deadline)
}

// --- Unix -------------------------------------------------------------------

#[cfg(unix)]
mod imp {
    use super::{io, Duration, Path, PeerIdentity};

    /// The daemon's listening socket.
    pub struct Listener(tokio::net::UnixListener);

    /// One accepted or connected byte stream.
    pub struct Stream(tokio::net::UnixStream);

    pub type ReadHalf = tokio::net::unix::OwnedReadHalf;
    pub type WriteHalf = tokio::net::unix::OwnedWriteHalf;
    pub type BlockingStream = std::os::unix::net::UnixStream;

    impl Listener {
        /// Bind, then immediately restrict the socket to its owner.
        ///
        /// The caller must have cleared a stale path first: `bind(2)` fails
        /// with `EADDRINUSE` on any existing file, live or not.
        pub fn bind(endpoint: &Path) -> io::Result<Self> {
            let listener = tokio::net::UnixListener::bind(endpoint)?;
            harden(endpoint)?;
            Ok(Self(listener))
        }

        /// Cancel-safe, as required by the daemon's `select!` accept loop.
        pub async fn accept(&self) -> io::Result<Stream> {
            let (stream, _addr) = self.0.accept().await?;
            Ok(Stream(stream))
        }
    }

    impl tokio::io::AsyncRead for Stream {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::pin::Pin::new(&mut self.0).poll_read(cx, buf)
        }
    }

    impl tokio::io::AsyncWrite for Stream {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<io::Result<usize>> {
            std::pin::Pin::new(&mut self.0).poll_write(cx, buf)
        }

        fn poll_flush(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::pin::Pin::new(&mut self.0).poll_flush(cx)
        }

        fn poll_shutdown(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::pin::Pin::new(&mut self.0).poll_shutdown(cx)
        }
    }

    impl Stream {
        pub async fn connect(endpoint: &Path) -> io::Result<Self> {
            tokio::net::UnixStream::connect(endpoint).await.map(Self)
        }

        /// A connected pair, for tests that drive a handler without a daemon.
        ///
        /// Nothing here needs to await; it is async only so the signature
        /// matches the Windows counterpart, which has a handshake to finish.
        #[cfg(test)]
        #[allow(clippy::unused_async)] // parity with the Windows counterpart
        pub async fn pair() -> io::Result<(Self, Self)> {
            let (a, b) = tokio::net::UnixStream::pair()?;
            Ok((Self(a), Self(b)))
        }

        /// Lock-free owned halves, exactly as before this module existed.
        pub fn into_split(self) -> (ReadHalf, WriteHalf) {
            self.0.into_split()
        }

        pub fn peer_identity(&self) -> PeerIdentity {
            let Ok(cred) = self.0.peer_cred() else {
                return PeerIdentity::default();
            };
            PeerIdentity {
                pid: cred.pid().and_then(|pid| u32::try_from(pid).ok()),
                uid: Some(cred.uid()),
                gid: Some(cred.gid()),
            }
        }
    }

    pub(super) fn harden(endpoint: &Path) -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(endpoint, std::fs::Permissions::from_mode(0o600))
    }

    pub(super) fn probe_live(endpoint: &Path) -> bool {
        // A connect refuses instantly on an orphaned socket file and succeeds
        // only when a server is accepting, so this costs one syscall.
        std::os::unix::net::UnixStream::connect(endpoint).is_ok()
    }

    pub(super) fn clear(endpoint: &Path) -> io::Result<()> {
        match std::fs::remove_file(endpoint) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }

    pub(super) fn blocking_connect(
        endpoint: &Path,
        deadline: Duration,
    ) -> io::Result<BlockingStream> {
        let stream = std::os::unix::net::UnixStream::connect(endpoint)?;
        stream.set_read_timeout(Some(deadline))?;
        stream.set_write_timeout(Some(deadline))?;
        Ok(stream)
    }
}

// --- Windows ----------------------------------------------------------------

#[cfg(not(unix))]
mod imp {
    use super::{io, Duration, Path, PeerIdentity};
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };

    /// A named pipe has no `accept(2)`. The server pre-creates one *instance*,
    /// waits for a client on it, and must create the next instance before it
    /// can take another connection — so the pending instance is state the
    /// listener carries, unlike a `UnixListener`.
    ///
    /// The mutex serialises that swap. A tokio mutex because the wait happens
    /// inside it; serialising costs nothing, as the daemon runs exactly one
    /// accept loop.
    pub struct Listener {
        name: std::ffi::OsString,
        pending: tokio::sync::Mutex<NamedPipeServer>,
    }

    /// Server- and client-side pipes are distinct types on Windows, unlike
    /// `UnixStream`. Erasing them behind one enum keeps `ipc.rs` to a single
    /// `Stream` type, matching the Unix shape it was written against.
    pub enum Stream {
        Server(NamedPipeServer),
        Client(NamedPipeClient),
    }

    pub type ReadHalf = tokio::io::ReadHalf<Stream>;
    pub type WriteHalf = tokio::io::WriteHalf<Stream>;

    /// A named pipe opened as a file would be a synchronous byte stream, which
    /// is the shape [`crate::ipc::blocking_call`] wants. The type is named so
    /// the signature stays honest; [`blocking_connect`] declines to build one.
    pub type BlockingStream = std::fs::File;

    impl Listener {
        /// `first_pipe_instance` is the exclusivity guarantee: a second process
        /// creating the same name fails rather than silently joining the pipe
        /// and racing for its clients. It doubles as the "already running"
        /// signal, standing in for a live socket file.
        ///
        /// `reject_remote_clients` keeps the pipe off SMB. That is tokio's
        /// default; it is set explicitly because a local-only transport quietly
        /// becoming network-reachable is exactly the mistake worth spelling
        /// out at the call site.
        ///
        /// There is no post-create hardening step. A pipe's DACL is fixed at
        /// creation and setting a custom one needs the raw unsafe entry point
        /// the workspace forbids, so it inherits the creating token's default
        /// DACL — owner plus SYSTEM/Administrators. That is weaker than `0600`
        /// (an administrator can connect) and is recorded in `docs/WINDOWS.md`.
        pub fn bind(endpoint: &Path) -> io::Result<Self> {
            let name = pipe_name(endpoint);
            let first = ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .create(&name)?;
            Ok(Self {
                name,
                pending: tokio::sync::Mutex::new(first),
            })
        }

        pub async fn accept(&self) -> io::Result<Stream> {
            let mut pending = self.pending.lock().await;
            pending.connect().await?;
            // Create the successor before handing this one out, so the name is
            // never momentarily unserved: a client connecting in that gap would
            // see FILE_NOT_FOUND instead of waiting for an instance.
            let next = ServerOptions::new()
                .reject_remote_clients(true)
                .create(&self.name)?;
            Ok(Stream::Server(std::mem::replace(&mut *pending, next)))
        }
    }

    impl Stream {
        /// Connect, absorbing the window in which no instance is free.
        ///
        /// A pipe instance serves exactly one client, and [`Listener::accept`]
        /// creates the successor only once a client has taken the pending one.
        /// A connect landing in that window fails with `ERROR_PIPE_BUSY`, which
        /// is transient and says nothing about whether the daemon is up — where
        /// a Unix socket's backlog absorbs the same burst invisibly. Two
        /// requests in quick succession are enough to hit it.
        ///
        /// So `ERROR_PIPE_BUSY` is retried briefly, to give callers the Unix
        /// behaviour they were written against. Every other error returns
        /// immediately, so an absent daemon is still reported at once.
        pub async fn connect(endpoint: &Path) -> io::Result<Self> {
            let name = pipe_name(endpoint);
            let deadline = std::time::Instant::now() + BUSY_RETRY_BUDGET;
            loop {
                match ClientOptions::new().open(&name) {
                    Ok(client) => return Ok(Stream::Client(client)),
                    Err(e)
                        if e.raw_os_error() == Some(ERROR_PIPE_BUSY)
                            && std::time::Instant::now() < deadline =>
                    {
                        tokio::time::sleep(BUSY_RETRY_BACKOFF).await;
                    }
                    Err(e) => return Err(e),
                }
            }
        }

        /// `tokio::io::split` rather than owned halves: `NamedPipeServer` has
        /// no `into_split`. Costs a `BiLock` per poll, which is noise beside
        /// the JSON parse on the same line.
        pub fn into_split(self) -> (ReadHalf, WriteHalf) {
            tokio::io::split(self)
        }

        /// Always empty — see [`PeerIdentity`].
        pub fn peer_identity(&self) -> PeerIdentity {
            PeerIdentity::default()
        }

        /// A connected pair, for tests that drive a handler without a daemon.
        ///
        /// Windows has no `socketpair`, so this brokers a real single-instance
        /// pipe under a name unique to this process and call, then connects to
        /// it. The name is never reused, so concurrent tests cannot collide.
        #[cfg(test)]
        pub async fn pair() -> io::Result<(Self, Self)> {
            use std::sync::atomic::{AtomicU64, Ordering};
            static SEQ: AtomicU64 = AtomicU64::new(0);

            let name = format!(
                r"\\.\pipe\muxa-test-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            );
            let server = ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .create(&name)?;
            let client = ClientOptions::new().open(&name)?;
            // A server instance holds no connection until `ConnectNamedPipe`
            // completes, and reads on it block until one arrives. The client
            // above is already attached, so this returns immediately — but
            // omitting it deadlocks the first read, which is the whole reason
            // this function is async.
            server.connect().await?;
            Ok((Stream::Server(server), Stream::Client(client)))
        }
    }

    // Both pipe types are `Unpin`, so `get_mut` projects without `unsafe`.
    impl AsyncRead for Stream {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            match self.get_mut() {
                Stream::Server(s) => Pin::new(s).poll_read(cx, buf),
                Stream::Client(c) => Pin::new(c).poll_read(cx, buf),
            }
        }
    }

    impl AsyncWrite for Stream {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            match self.get_mut() {
                Stream::Server(s) => Pin::new(s).poll_write(cx, buf),
                Stream::Client(c) => Pin::new(c).poll_write(cx, buf),
            }
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            match self.get_mut() {
                Stream::Server(s) => Pin::new(s).poll_flush(cx),
                Stream::Client(c) => Pin::new(c).poll_flush(cx),
            }
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            match self.get_mut() {
                Stream::Server(s) => Pin::new(s).poll_shutdown(cx),
                Stream::Client(c) => Pin::new(c).poll_shutdown(cx),
            }
        }
    }

    /// Protection is set at creation; there is nothing to re-apply after.
    pub(super) fn harden(_endpoint: &Path) -> io::Result<()> {
        Ok(())
    }

    pub(super) fn probe_live(endpoint: &Path) -> bool {
        // An instance exists only while the daemon holds it, so a successful
        // open is the liveness answer. `ERROR_PIPE_BUSY` means every instance
        // is momentarily taken — also live.
        match ClientOptions::new().open(pipe_name(endpoint)) {
            Ok(_) => true,
            Err(e) => e.raw_os_error() == Some(ERROR_PIPE_BUSY),
        }
    }

    /// Nothing to clear: the pipe is gone once the daemon drops its handles.
    pub(super) fn clear(_endpoint: &Path) -> io::Result<()> {
        Ok(())
    }

    pub(super) fn blocking_connect(
        _endpoint: &Path,
        _deadline: Duration,
    ) -> io::Result<BlockingStream> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "synchronous IPC needs a read timeout, which a named pipe cannot \
             set without an unsafe SetCommTimeouts call",
        ))
    }

    /// `ERROR_PIPE_BUSY` — all instances are in use, so the server is up.
    const ERROR_PIPE_BUSY: i32 = 231;

    /// How long [`Stream::connect`] keeps retrying a busy pipe. Generous
    /// against the microseconds the daemon needs to create a successor
    /// instance, and still far below any caller's own request deadline.
    const BUSY_RETRY_BUDGET: Duration = Duration::from_secs(1);

    /// Short enough that the common case — losing a race by a few
    /// microseconds — costs one sleep rather than a visible stall.
    const BUSY_RETRY_BACKOFF: Duration = Duration::from_millis(2);

    /// Map an endpoint path onto a pipe name.
    ///
    /// Pipe names live in a flat namespace and may not contain a backslash, so
    /// the path cannot be used as-is. The file stem keeps the name readable
    /// when someone lists pipes; the digest keeps two daemons on different
    /// paths (a test fixture and a login daemon, say) from colliding on it.
    ///
    /// FNV-1a rather than `DefaultHasher`: the client and the daemon derive
    /// this name independently, so it has to be stable across builds, and
    /// `DefaultHasher`'s algorithm is explicitly not guaranteed to be.
    fn pipe_name(endpoint: &Path) -> std::ffi::OsString {
        let full = endpoint.to_string_lossy();
        let stem: String = endpoint
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .take(32)
            .collect();
        format!(r"\\.\pipe\muxa-{stem}-{:016x}", fnv1a64(full.as_bytes())).into()
    }

    fn fnv1a64(bytes: &[u8]) -> u64 {
        const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x0000_0100_0000_01b3;
        bytes.iter().fold(OFFSET, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(PRIME)
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn pipe_name_is_stable_and_path_specific() {
            let a = pipe_name(Path::new(r"C:\Users\dev\AppData\Local\Temp\muxa.sock"));
            let b = pipe_name(Path::new(r"C:\Users\dev\AppData\Local\Temp\muxa.sock"));
            let c = pipe_name(Path::new(r"C:\other\muxa.sock"));

            assert_eq!(a, b, "same path must derive the same pipe");
            assert_ne!(a, c, "different paths must not collide");
            assert!(a.to_string_lossy().starts_with(r"\\.\pipe\muxa-muxa-"));
        }

        #[test]
        fn pipe_name_drops_characters_a_pipe_name_cannot_carry() {
            let name = pipe_name(Path::new(r"C:\tmp\od d\mu xa!.sock"));
            let text = name.to_string_lossy();

            assert!(text.starts_with(r"\\.\pipe\muxa-"));
            // Only the leading `\\.\pipe\` may contain backslashes.
            assert_eq!(text.matches('\\').count(), 4);
            assert!(!text.contains(' '));
        }
    }
}
