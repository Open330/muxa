//! cmux implementation of [`crate::backend::PaneBackend`].
//!
//! This first slice uses only cmux's documented environment and Unix-socket
//! API. It can identify the current surface, focus an exact surface, and send
//! targeted text. Full workspace/surface enumeration waits for a versioned
//! JSON fixture; until then observations are deliberately partial so the
//! reconciler never treats an absent cmux row as proof that an agent exited.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use serde_json::json;

use super::{BackendCaps, HostKind, PaneBackend, PaneObservation};
use crate::tmux::PaneInfo;

/// Namespace prefix for cmux surface UUIDs inside muxa.
pub const PANE_ID_PREFIX: &str = "cmux:";
pub const DEFAULT_SOCKET_PATH: &str = "/tmp/cmux.sock";

const SOCKET_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_RESPONSE_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone)]
pub struct CmuxBackend {
    endpoint: String,
}

impl CmuxBackend {
    #[must_use]
    pub fn new() -> Self {
        Self {
            endpoint: endpoint_from_env(),
        }
    }

    #[must_use]
    pub fn with_endpoint(endpoint: impl Into<String>) -> Self {
        let endpoint = endpoint.into();
        Self {
            endpoint: if endpoint.trim().is_empty() {
                DEFAULT_SOCKET_PATH.into()
            } else {
                endpoint
            },
        }
    }

    fn current_pane_info(&self) -> Option<PaneInfo> {
        current_pane_info_from(|name| std::env::var(name).ok(), Some(self.endpoint.clone()))
    }

    fn rpc(&self, endpoint: Option<&str>, method: &str, params: serde_json::Value) -> bool {
        let endpoint = endpoint.unwrap_or(&self.endpoint);
        let Ok(mut stream) = UnixStream::connect(endpoint) else {
            return false;
        };
        if stream.set_read_timeout(Some(SOCKET_TIMEOUT)).is_err()
            || stream.set_write_timeout(Some(SOCKET_TIMEOUT)).is_err()
        {
            return false;
        }
        let request = json!({
            "id": "muxa-pane-control",
            "method": method,
            "params": params,
        });
        let Ok(mut bytes) = serde_json::to_vec(&request) else {
            return false;
        };
        bytes.push(b'\n');
        if stream.write_all(&bytes).is_err() || stream.flush().is_err() {
            return false;
        }
        let mut response = String::new();
        let mut reader = BufReader::new(stream).take(MAX_RESPONSE_BYTES + 1);
        let Ok(read) = reader.read_line(&mut response) else {
            return false;
        };
        if read == 0 || u64::try_from(response.len()).unwrap_or(u64::MAX) > MAX_RESPONSE_BYTES {
            return false;
        }
        serde_json::from_str::<serde_json::Value>(&response)
            .ok()
            .and_then(|value| value.get("ok").and_then(serde_json::Value::as_bool))
            .unwrap_or(false)
    }
}

impl Default for CmuxBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl PaneBackend for CmuxBackend {
    fn kind(&self) -> HostKind {
        HostKind::Cmux
    }

    fn list_panes(&self) -> Vec<PaneInfo> {
        self.current_pane_info().into_iter().collect()
    }

    fn observe_panes(&self) -> PaneObservation {
        // The env identifies only this process's surface, not every surface
        // owned by cmux. Absence is therefore never authoritative yet.
        PaneObservation::partial(self.list_panes())
    }

    fn resolve_pane(&self, pane_id: &str) -> Option<PaneInfo> {
        self.current_pane_info()
            .filter(|pane| pane.pane_id == namespace_pane_id(pane_id))
    }

    fn capture_pane(&self, _pane_id: &str) -> Option<String> {
        None
    }

    fn pane_pid_map(&self) -> HashMap<u32, String> {
        HashMap::new()
    }

    fn current_pane(&self) -> Option<String> {
        std::env::var("CMUX_SURFACE_ID")
            .ok()
            .filter(|id| !id.trim().is_empty())
            .map(|id| namespace_pane_id(&id))
    }

    fn focus_pane(&self, pane_id: &str) -> bool {
        self.rpc(
            None,
            "surface.focus",
            json!({ "surface_id": strip_prefix(pane_id) }),
        )
    }

    fn send_text(&self, pane_id: &str, text: &str) -> bool {
        self.send_text_on(None, pane_id, text)
    }

    fn send_text_on(&self, endpoint: Option<&str>, pane_id: &str, text: &str) -> bool {
        self.rpc(
            endpoint,
            "surface.send_text",
            json!({ "surface_id": strip_prefix(pane_id), "text": text }),
        )
    }

    fn caps(&self) -> BackendCaps {
        BackendCaps {
            current_command: false,
            pane_pid_map: false,
            capture_pane: false,
            focus_pane: true,
            send_text: true,
        }
    }
}

#[must_use]
pub fn endpoint_from_env() -> String {
    std::env::var("CMUX_SOCKET_PATH")
        .ok()
        .filter(|path| !path.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_SOCKET_PATH.into())
}

#[must_use]
pub fn namespace_pane_id(pane_id: &str) -> String {
    if pane_id.starts_with(PANE_ID_PREFIX) {
        pane_id.to_string()
    } else {
        format!("{PANE_ID_PREFIX}{pane_id}")
    }
}

#[must_use]
pub fn strip_prefix(pane_id: &str) -> &str {
    pane_id.strip_prefix(PANE_ID_PREFIX).unwrap_or(pane_id)
}

fn current_pane_info_from(
    read: impl Fn(&str) -> Option<String>,
    endpoint: Option<String>,
) -> Option<PaneInfo> {
    let surface = read("CMUX_SURFACE_ID").filter(|id| !id.trim().is_empty())?;
    let workspace = read("CMUX_WORKSPACE_ID")
        .filter(|id| !id.trim().is_empty())
        .unwrap_or_else(|| "cmux".into());
    Some(PaneInfo {
        session_group: None,
        agent_role: None,
        agent_alias: None,
        socket: endpoint,
        pane_id: namespace_pane_id(&surface),
        session_id: workspace.clone(),
        session: workspace.clone(),
        window_id: workspace,
        window_name: String::new(),
        window_index: "0".into(),
        pane_index: "0".into(),
        tty: String::new(),
        current_command: String::new(),
        title: String::new(),
        pane_pid: 0,
        // cmux's environment contract does not expose another surface's cwd.
        // The daemon cwd is not a safe substitute for terminal metadata.
        current_path: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_reader(
        pairs: &'static [(&'static str, &'static str)],
    ) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        }
    }

    #[test]
    fn namespaces_surface_ids_once() {
        assert_eq!(namespace_pane_id("surface-7"), "cmux:surface-7");
        assert_eq!(namespace_pane_id("cmux:surface-7"), "cmux:surface-7");
        assert_eq!(strip_prefix("cmux:surface-7"), "surface-7");
    }

    #[test]
    fn current_env_becomes_a_partial_namespaced_observation() {
        let pane = current_pane_info_from(
            env_reader(&[
                ("CMUX_SURFACE_ID", "surface-7"),
                ("CMUX_WORKSPACE_ID", "workspace-2"),
            ]),
            Some("/tmp/cmux-debug.sock".into()),
        )
        .unwrap();
        assert_eq!(pane.pane_id, "cmux:surface-7");
        assert_eq!(pane.session_id, "workspace-2");
        assert_eq!(pane.socket.as_deref(), Some("/tmp/cmux-debug.sock"));
    }

    #[test]
    fn capabilities_match_the_environment_only_first_slice() {
        let caps = CmuxBackend::new().caps();
        assert!(!caps.current_command);
        assert!(!caps.pane_pid_map);
        assert!(!caps.capture_pane);
        assert!(caps.focus_pane);
        assert!(caps.send_text);
    }

    #[test]
    fn send_text_uses_the_exact_surface_and_socket_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("cmux.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            let request: serde_json::Value = serde_json::from_str(&request).unwrap();
            assert_eq!(request["method"], "surface.send_text");
            assert_eq!(request["params"]["surface_id"], "surface-7");
            assert_eq!(request["params"]["text"], "hello\n");
            stream
                .write_all(b"{\"id\":\"muxa-pane-control\",\"ok\":true}\n")
                .unwrap();
        });

        let backend = CmuxBackend::with_endpoint(socket.display().to_string());
        assert!(backend.send_text("cmux:surface-7", "hello\n"));
        server.join().unwrap();
    }

    #[test]
    fn oversized_socket_response_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("cmux.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            let oversized = vec![b' '; usize::try_from(MAX_RESPONSE_BYTES + 1).unwrap()];
            let _ = stream.write_all(&oversized);
        });

        let backend = CmuxBackend::with_endpoint(socket.display().to_string());
        assert!(!backend.focus_pane("cmux:surface-7"));
        server.join().unwrap();
    }
}
