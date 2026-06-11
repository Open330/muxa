//! muxa-owned terminal sessions.
//!
//! `PaneBackend` models panes owned by an external host such as tmux or
//! zellij. This module models sessions muxa itself owns: a child process
//! running inside a PTY, a bounded output buffer, and small control
//! operations used by `muxa run`, `muxa attach`, and read-only dashboard
//! capture.

use crate::event::{SurfaceKind, SurfaceRef};
use portable_pty::{
    native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize as PortablePtySize,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use time::OffsetDateTime;

const DEFAULT_COLS: u16 = 120;
const DEFAULT_ROWS: u16 = 40;
const MAX_BUFFER_BYTES: usize = 512 * 1024;
const READ_CHUNK_BYTES: usize = 8192;
const EXITED_SESSION_TTL: Duration = Duration::from_secs(30 * 60);
static SESSION_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum SessionBackendKind {
    Tmux,
    Zellij,
    Pty,
}

impl From<SessionBackendKind> for SurfaceKind {
    fn from(value: SessionBackendKind) -> Self {
        match value {
            SessionBackendKind::Tmux => Self::Tmux,
            SessionBackendKind::Zellij => Self::Zellij,
            SessionBackendKind::Pty => Self::Pty,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionRef {
    pub id: String,
    pub backend: SessionBackendKind,
    pub display_name: Option<String>,
    pub cwd: Option<String>,
    pub attached_clients: usize,
    pub exited: bool,
    pub exit_status: Option<i32>,
    /// OS pid of the PTY child, when known. Lets the daemon register the
    /// session as a pid-tracked `Task` row so `muxa run` processes show up
    /// in `muxa status`.
    #[serde(default)]
    pub pid: Option<u32>,
}

impl SessionRef {
    pub fn surface(&self) -> SurfaceRef {
        SurfaceRef {
            kind: self.backend.into(),
            id: self.id.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalSnapshot {
    pub session: SessionRef,
    pub cols: u16,
    pub rows: u16,
    pub lines: Vec<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub fetched_at: OffsetDateTime,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionOutput {
    pub session_id: String,
    pub offset: u64,
    pub next_offset: u64,
    pub data: String,
    pub truncated: bool,
    pub exited: bool,
    pub exit_status: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnSession {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
    pub name: Option<String>,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSignal {
    Interrupt,
    Kill,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("session not found: {0}")]
    NotFound(String),
    #[error("session already exited: {0}")]
    Exited(String),
    #[error("pty error: {0}")]
    Pty(String),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionBackendCaps {
    pub capture: bool,
    pub write_input: bool,
    pub resize: bool,
    pub terminate: bool,
    pub attach: bool,
}

impl Default for SessionBackendCaps {
    fn default() -> Self {
        Self {
            capture: true,
            write_input: true,
            resize: true,
            terminate: true,
            attach: true,
        }
    }
}

pub trait SessionBackend: Send + Sync + 'static {
    fn kind(&self) -> SessionBackendKind;
    fn list_sessions(&self) -> Vec<SessionRef>;
    fn resolve_session(&self, id_or_name: &str) -> Option<SessionRef>;
    fn capture(&self, session_id: &str) -> Result<TerminalSnapshot, SessionError>;
    fn read_output(&self, session_id: &str, offset: u64) -> Result<SessionOutput, SessionError>;
    fn send_input(&self, session_id: &str, bytes: &[u8]) -> Result<(), SessionError>;
    fn resize(&self, session_id: &str, cols: u16, rows: u16) -> Result<(), SessionError>;
    fn set_attached(&self, session_id: &str, attached: bool) -> Result<(), SessionError>;
    fn terminate(&self, session_id: &str) -> Result<(), SessionError>;
    fn caps(&self) -> SessionBackendCaps {
        SessionBackendCaps::default()
    }
}

pub type SharedSessionBackend = Arc<PtySessionBackend>;

#[derive(Default)]
pub struct PtySessionBackend {
    sessions: Mutex<HashMap<String, Arc<PtySession>>>,
}

impl PtySessionBackend {
    pub fn shared() -> SharedSessionBackend {
        Arc::new(Self::default())
    }

    pub fn spawn_session(&self, input: SpawnSession) -> Result<SessionRef, SessionError> {
        if input.command.trim().is_empty() {
            return Err(SessionError::Pty("command is empty".into()));
        }
        let cols = input.cols.unwrap_or(DEFAULT_COLS).max(1);
        let rows = input.rows.unwrap_or(DEFAULT_ROWS).max(1);
        let size = PortablePtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = native_pty_system()
            .openpty(size)
            .map_err(|e| SessionError::Pty(e.to_string()))?;

        let mut cmd = CommandBuilder::new(&input.command);
        cmd.args(&input.args);
        if !input.env.is_empty() {
            cmd.env_clear();
            for (key, value) in &input.env {
                validate_env_pair(key, value)?;
                cmd.env(key, value);
            }
        }
        if let Some(cwd) = &input.cwd {
            cmd.cwd(cwd);
        }

        let id = next_session_id();
        cmd.env("MUXA_SESSION_ID", &id);
        cmd.env("MUXA_SESSION_BACKEND", "pty");

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| SessionError::Pty(e.to_string()))?;
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| SessionError::Pty(e.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| SessionError::Pty(e.to_string()))?;
        let killer = child.clone_killer();
        let pid = child.process_id();
        let session = Arc::new(PtySession {
            meta: Mutex::new(SessionMeta {
                id: id.clone(),
                display_name: input.name.or_else(|| Some(input.command.clone())),
                cwd: input.cwd.map(|p| p.display().to_string()),
                pid,
                cols,
                rows,
                attached_clients: 0,
                exited: false,
                exit_status: None,
                exited_at: None,
            }),
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            killer: Mutex::new(killer),
            buffer: Mutex::new(OutputBuffer::new(MAX_BUFFER_BYTES)),
        });

        let session_for_reader = session.clone();
        thread::spawn(move || read_pty_loop(reader, session_for_reader));

        let session_for_wait = session.clone();
        thread::spawn(move || {
            let status = child.wait();
            let exit_status = status.ok().map(|s| {
                if s.success() {
                    0
                } else {
                    i32::try_from(s.exit_code()).unwrap_or(1)
                }
            });
            if let Ok(mut meta) = session_for_wait.meta.lock() {
                meta.exited = true;
                meta.exit_status = exit_status;
                meta.exited_at = Some(SystemTime::now());
            }
        });

        let reference = session.reference();
        self.sessions
            .lock()
            .map_err(|_| SessionError::Pty("session registry lock poisoned".into()))?
            .insert(id, session);
        Ok(reference)
    }

    fn prune_exited(&self) {
        let Ok(mut sessions) = self.sessions.lock() else {
            return;
        };
        sessions.retain(|_, session| {
            let Ok(meta) = session.meta.lock() else {
                return false;
            };
            if !meta.exited || meta.attached_clients > 0 {
                return true;
            }
            let Some(exited_at) = meta.exited_at else {
                return true;
            };
            exited_at.elapsed().unwrap_or(Duration::ZERO) < EXITED_SESSION_TTL
        });
    }

    fn get(&self, session_id: &str) -> Result<Arc<PtySession>, SessionError> {
        self.prune_exited();
        self.sessions
            .lock()
            .map_err(|_| SessionError::Pty("session registry lock poisoned".into()))?
            .get(session_id)
            .cloned()
            .ok_or_else(|| SessionError::NotFound(session_id.to_string()))
    }
}

impl SessionBackend for PtySessionBackend {
    fn kind(&self) -> SessionBackendKind {
        SessionBackendKind::Pty
    }

    fn list_sessions(&self) -> Vec<SessionRef> {
        self.prune_exited();
        self.sessions
            .lock()
            .map(|sessions| sessions.values().map(|s| s.reference()).collect())
            .unwrap_or_default()
    }

    fn resolve_session(&self, id_or_name: &str) -> Option<SessionRef> {
        self.prune_exited();
        self.sessions.lock().ok().and_then(|sessions| {
            sessions.values().find_map(|s| {
                let r = s.reference();
                (r.id == id_or_name || r.display_name.as_deref() == Some(id_or_name)).then_some(r)
            })
        })
    }

    fn capture(&self, session_id: &str) -> Result<TerminalSnapshot, SessionError> {
        let session = self.get(session_id)?;
        let reference = session.reference();
        let (cols, rows, text, truncated) = session.snapshot_text()?;
        Ok(TerminalSnapshot {
            session: reference,
            cols,
            rows,
            lines: text.lines().map(str::to_string).collect(),
            fetched_at: OffsetDateTime::now_utc(),
            truncated,
        })
    }

    fn read_output(&self, session_id: &str, offset: u64) -> Result<SessionOutput, SessionError> {
        let session = self.get(session_id)?;
        session.read_output(offset)
    }

    fn send_input(&self, session_id: &str, bytes: &[u8]) -> Result<(), SessionError> {
        let session = self.get(session_id)?;
        if session.reference().exited {
            return Err(SessionError::Exited(session_id.to_string()));
        }
        let mut writer = session
            .writer
            .lock()
            .map_err(|_| SessionError::Pty("session writer lock poisoned".into()))?;
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }

    fn resize(&self, session_id: &str, cols: u16, rows: u16) -> Result<(), SessionError> {
        let session = self.get(session_id)?;
        let cols = cols.max(1);
        let rows = rows.max(1);
        {
            let mut meta = session
                .meta
                .lock()
                .map_err(|_| SessionError::Pty("session meta lock poisoned".into()))?;
            meta.cols = cols;
            meta.rows = rows;
        }
        let result = session
            .master
            .lock()
            .map_err(|_| SessionError::Pty("session master lock poisoned".into()))?
            .resize(PortablePtySize {
                cols,
                rows,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| SessionError::Pty(e.to_string()));
        result
    }

    fn set_attached(&self, session_id: &str, attached: bool) -> Result<(), SessionError> {
        let session = self.get(session_id)?;
        let mut meta = session
            .meta
            .lock()
            .map_err(|_| SessionError::Pty("session meta lock poisoned".into()))?;
        if attached {
            meta.attached_clients = meta.attached_clients.saturating_add(1);
        } else {
            meta.attached_clients = meta.attached_clients.saturating_sub(1);
        }
        Ok(())
    }

    fn terminate(&self, session_id: &str) -> Result<(), SessionError> {
        let session = self.get(session_id)?;
        let result = session
            .killer
            .lock()
            .map_err(|_| SessionError::Pty("session killer lock poisoned".into()))?
            .kill()
            .map_err(SessionError::Io);
        result
    }
}

struct PtySession {
    meta: Mutex<SessionMeta>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    buffer: Mutex<OutputBuffer>,
}

impl PtySession {
    fn reference(&self) -> SessionRef {
        let meta = self.meta.lock().expect("session meta lock poisoned");
        SessionRef {
            id: meta.id.clone(),
            backend: SessionBackendKind::Pty,
            display_name: meta.display_name.clone(),
            cwd: meta.cwd.clone(),
            attached_clients: meta.attached_clients,
            exited: meta.exited,
            exit_status: meta.exit_status,
            pid: meta.pid,
        }
    }

    fn snapshot_text(&self) -> Result<(u16, u16, String, bool), SessionError> {
        let meta = self
            .meta
            .lock()
            .map_err(|_| SessionError::Pty("session meta lock poisoned".into()))?;
        let (bytes, truncated) = self
            .buffer
            .lock()
            .map_err(|_| SessionError::Pty("session buffer lock poisoned".into()))?
            .snapshot();
        Ok((
            meta.cols,
            meta.rows,
            String::from_utf8_lossy(&bytes).into_owned(),
            truncated,
        ))
    }

    fn read_output(&self, offset: u64) -> Result<SessionOutput, SessionError> {
        let reference = self.reference();
        let (base, next, bytes, truncated) = self
            .buffer
            .lock()
            .map_err(|_| SessionError::Pty("session buffer lock poisoned".into()))?
            .read_from(offset);
        Ok(SessionOutput {
            session_id: reference.id,
            offset: base,
            next_offset: next,
            data: String::from_utf8_lossy(&bytes).into_owned(),
            truncated,
            exited: reference.exited,
            exit_status: reference.exit_status,
        })
    }
}

struct SessionMeta {
    id: String,
    display_name: Option<String>,
    cwd: Option<String>,
    pid: Option<u32>,
    cols: u16,
    rows: u16,
    attached_clients: usize,
    exited: bool,
    exit_status: Option<i32>,
    exited_at: Option<SystemTime>,
}

struct OutputBuffer {
    max: usize,
    base_offset: u64,
    data: Vec<u8>,
    ever_truncated: bool,
}

impl OutputBuffer {
    fn new(max: usize) -> Self {
        Self {
            max,
            base_offset: 0,
            data: Vec::new(),
            ever_truncated: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.data.extend_from_slice(bytes);
        if self.data.len() > self.max {
            let drain = self.data.len() - self.max;
            self.data.drain(0..drain);
            self.base_offset = self
                .base_offset
                .saturating_add(u64::try_from(drain).unwrap_or(u64::MAX));
            self.ever_truncated = true;
        }
    }

    fn snapshot(&self) -> (Vec<u8>, bool) {
        (self.data.clone(), self.ever_truncated)
    }

    fn read_from(&self, offset: u64) -> (u64, u64, Vec<u8>, bool) {
        let start_offset = offset.max(self.base_offset);
        let start = usize::try_from(start_offset.saturating_sub(self.base_offset)).unwrap_or(0);
        let bytes = self.data.get(start..).unwrap_or_default().to_vec();
        let next = self
            .base_offset
            .saturating_add(u64::try_from(self.data.len()).unwrap_or(u64::MAX));
        (start_offset, next, bytes, offset < self.base_offset)
    }
}

fn read_pty_loop(mut reader: Box<dyn Read + Send>, session: Arc<PtySession>) {
    let mut buf = vec![0_u8; READ_CHUNK_BYTES];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if let Ok(mut out) = session.buffer.lock() {
                    out.push(&buf[..n]);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
}

fn next_session_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0));
    let seq = SESSION_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "pty:{:x}{:08x}{:08x}",
        now.as_nanos(),
        std::process::id(),
        seq
    )
}

fn validate_env_pair(key: &str, value: &str) -> Result<(), SessionError> {
    if key.is_empty() || key.contains('=') || key.contains('\0') || value.contains('\0') {
        return Err(SessionError::Pty(format!(
            "invalid environment variable key: {key:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_session_ids_do_not_collide_in_same_process() {
        let a = next_session_id();
        let b = next_session_id();
        assert_ne!(a, b);
        assert!(a.starts_with("pty:"));
        assert!(b.starts_with("pty:"));
    }

    #[test]
    fn rejects_invalid_env_keys() {
        assert!(validate_env_pair("OK", "value").is_ok());
        assert!(validate_env_pair("", "value").is_err());
        assert!(validate_env_pair("A=B", "value").is_err());
        assert!(validate_env_pair("A", "bad\0value").is_err());
    }

    #[test]
    fn pty_spawn_uses_supplied_environment() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        let backend = PtySessionBackend::default();
        let session = backend
            .spawn_session(SpawnSession {
                command: "/bin/sh".into(),
                args: vec![
                    "-lc".into(),
                    "printf '%s %s' \"$MUXA_TEST_ENV\" \"$MUXA_SESSION_BACKEND\"".into(),
                ],
                env: vec![("MUXA_TEST_ENV".into(), "from-cli".into())],
                cwd: None,
                name: None,
                cols: Some(80),
                rows: Some(24),
            })
            .unwrap();

        for _ in 0..50 {
            let out = backend.read_output(&session.id, 0).unwrap();
            if out.data.contains("from-cli pty") {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("PTY output never reflected supplied env");
    }
}
