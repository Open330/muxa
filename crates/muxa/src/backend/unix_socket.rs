//! `UnixStream` for the cmux and herdr clients, with a non-Unix stand-in.
//!
//! Both backends speak a line-delimited JSON protocol to a daemon that only
//! ships for Unix — cmux and herdr have no Windows build. muxa still has to
//! *compile* on Windows (see `docs/WINDOWS.md`), and `HostKind` matches over
//! all five hosts exhaustively, so removing the modules there would ripple
//! through `default_backend`, `backend_of`, and `active_kinds_from`.
//!
//! Re-exporting the real type on Unix and a stand-in elsewhere keeps both
//! clients' bodies byte-for-byte identical across platforms: the shipping
//! platform's behavior cannot regress, because its code did not change.
//!
//! The stand-in is deliberately **uninhabited** — it wraps [`Infallible`], so
//! no value of it can ever exist and the compiler proves every method body
//! below is unreachable. `connect` is the only constructor and it always
//! fails, which lands both clients on paths they already handle:
//!
//! - herdr's `request` stats the socket path first, gets "absent", and
//!   returns `SocketMissing` — authoritatively no panes, which is correct and
//!   reap-safe on a host where herdr cannot run.
//! - herdr's `server_reachable` and cmux's `rpc` return `false`.
//!
//! So the Windows answer is "this host is not present", reached through the
//! backends' own logic rather than a parallel set of stubbed return values.

#[cfg(unix)]
pub(crate) use std::os::unix::net::UnixStream;

#[cfg(not(unix))]
pub(crate) use stub::UnixStream;

#[cfg(not(unix))]
mod stub {
    use std::convert::Infallible;
    use std::io::{self, Read, Write};
    use std::path::Path;
    use std::time::Duration;

    /// See the module docs. Uninhabited: `Infallible` has no values, so this
    /// struct has none either, and `match self.0 {}` discharges every method.
    pub(crate) struct UnixStream(Infallible);

    impl UnixStream {
        /// Always fails. `NotFound` is the same error a Unix host reports for
        /// a socket path with no daemon behind it, so callers need no
        /// platform-specific branch.
        pub(crate) fn connect<P: AsRef<Path>>(_path: P) -> io::Result<Self> {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "unix-socket backends (cmux, herdr) are not available on this platform",
            ))
        }

        pub(crate) fn set_read_timeout(&self, _timeout: Option<Duration>) -> io::Result<()> {
            match self.0 {}
        }

        pub(crate) fn set_write_timeout(&self, _timeout: Option<Duration>) -> io::Result<()> {
            match self.0 {}
        }
    }

    impl Read for UnixStream {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            match self.0 {}
        }
    }

    // herdr wraps a borrow (`BufReader::new(&stream)`); std's `UnixStream`
    // implements `Read` for both forms, so the stand-in must too.
    impl Read for &UnixStream {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            match self.0 {}
        }
    }

    impl Write for UnixStream {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            match self.0 {}
        }

        fn flush(&mut self) -> io::Result<()> {
            match self.0 {}
        }
    }
}
