use super::{
    collaboration, Agent, CollaborationClientKind, CollaborationOrigin, CollaborationOriginMatch,
    CollaborationPaneEvidence, CollaborationProvenance, CollaborationStore, CollaborationTopology,
    HostKind, PaneInfo, Participant, Response, SharedBackend, SharedStore, Stream,
};

/// A fresh Codex thread behind the shared app-server reports no pane, and the
/// reconciler binds it only every `reconciler.interval_secs`. Its first
/// collaboration call usually comes sooner, so bind now — only for a caller
/// that names an unbound Codex session, so ordinary requests never scan.
pub(super) async fn bind_unplaced_codex_origin(
    store: &SharedStore,
    backends: &[SharedBackend],
    agents: &[Agent],
    panes: &[PaneInfo],
    origin_session: Option<&str>,
) -> bool {
    let Some(session) = origin_session else {
        return false;
    };
    let unplaced = agents
        .iter()
        .any(|agent| agent.session_id == session && crate::codex_binding::is_unplaced_codex(agent));
    unplaced
        && crate::codex_binding::bind_paneless_codex(store, backends, Some(panes.to_vec())).await
            > 0
}

pub(super) async fn collaboration_participants(
    store: &SharedStore,
    backends: &[SharedBackend],
    collaboration: &CollaborationStore,
    origin_session: Option<&str>,
    scanned: Option<Vec<PaneInfo>>,
) -> CollaborationTopology {
    let mut agents = store.snapshot().await;
    let listed = backends.to_vec();
    let discovery_agents = agents.clone();
    let (panes, mut participants) = tokio::task::spawn_blocking(move || {
        // Reuse the inventory this request already took for caller
        // provenance; only a request that observed nothing scans here.
        let panes = scanned.unwrap_or_else(|| {
            listed
                .iter()
                .flat_map(|backend| backend.list_panes())
                .collect::<Vec<_>>()
        });
        let participants = collaboration::participants_from(&discovery_agents, &panes);
        (panes, participants)
    })
    .await
    .unwrap_or_default();
    if bind_unplaced_codex_origin(store, backends, &agents, &panes, origin_session).await {
        agents = store.snapshot().await;
        let refreshed = agents.clone();
        let observed = panes.clone();
        participants = tokio::task::spawn_blocking(move || {
            collaboration::participants_from(&refreshed, &observed)
        })
        .await
        .unwrap_or_default();
    }
    collaboration.enrich_participants(&mut participants).await;
    collaboration
        .enrich_launch_purposes(&mut participants, &panes)
        .await;
    collaboration.enrich_worker_drains(&mut participants).await;
    CollaborationTopology {
        participants,
        panes,
        agents,
    }
}

/// The live sessions a purpose change names, or why it is refused.
///
/// The operator, or an agent acting on the operator's instruction, names
/// exact sessions; there is no caller check, since same-user processes are
/// trusted and the change is audited with the caller's provenance. Each id
/// must name exactly one live agent session; a missing or ambiguous one
/// refuses the whole operation.
pub(super) fn promote_purpose_targets(
    topology: &CollaborationTopology,
    session_ids: &[String],
) -> Result<Vec<collaboration::Participant>, String> {
    if session_ids.is_empty() {
        return Err("name at least one agent session id".into());
    }
    session_ids
        .iter()
        .map(|id| {
            let matches: Vec<_> = topology
                .participants
                .iter()
                .filter(|participant| participant.agent_session_id == *id && !participant.console)
                .collect();
            match matches.as_slice() {
                [participant] => Ok((*participant).clone()),
                [] => Err(format!("no live agent session {id}")),
                _ => Err(format!("agent session {id} is ambiguous across panes")),
            }
        })
        .collect()
}

#[derive(Debug, Clone)]
pub(super) struct CollaborationConnectionActor {
    pub(super) client_kind: CollaborationClientKind,
    pub(super) caller_pid: Option<u32>,
    pub(super) caller_uid: Option<u32>,
    pub(super) caller_gid: Option<u32>,
    pub(super) executable: Option<String>,
    /// The peer's audit token at connect time (macOS), for
    /// [`crate::app_identity`].
    pub(super) audit_token: Option<[u32; 8]>,
    pub(super) observed_pane: Option<String>,
    pub(super) pane_evidence: Option<CollaborationPaneEvidence>,
    pub(super) pane_observed: bool,
    /// The pane inventory the current request took while observing the
    /// caller, handed to participant discovery so one request scans once.
    /// Cleared before every request; never reused across requests.
    pub(super) request_panes: Option<Vec<PaneInfo>>,
}

impl CollaborationConnectionActor {
    pub(super) async fn observe_pane(&mut self, backends: &[SharedBackend]) {
        if self.pane_observed {
            return;
        }
        self.pane_observed = true;
        let Some(pid) = self.caller_pid else {
            return;
        };
        let pane_backends = backends.to_vec();
        let Ok((observed, panes)) = tokio::task::spawn_blocking(move || {
            let scan = RequestPaneScan::take(&pane_backends);
            let observed = observed_process_pane(pid, &pane_backends, &scan);
            (observed, scan.into_panes())
        })
        .await
        else {
            return;
        };
        self.request_panes = Some(panes);
        if let Some((pane, evidence)) = observed {
            self.observed_pane = Some(pane);
            self.pane_evidence = Some(evidence);
        }
    }

    pub(super) fn take_request_panes(&mut self) -> Option<Vec<PaneInfo>> {
        self.request_panes.take()
    }

    pub(super) fn provenance(&self, origin: &CollaborationOrigin) -> CollaborationProvenance {
        let origin_match = match self.observed_pane.as_deref() {
            Some(pane) if pane == origin.pane => CollaborationOriginMatch::Matched,
            Some(_) => CollaborationOriginMatch::Mismatched,
            None => CollaborationOriginMatch::Unverifiable,
        };
        CollaborationProvenance {
            client_kind: self.client_kind,
            caller_pid: self.caller_pid,
            caller_uid: self.caller_uid,
            caller_gid: self.caller_gid,
            executable: self.executable.clone(),
            observed_pane: self.observed_pane.clone(),
            pane_evidence: self.pane_evidence,
            origin_match,
        }
    }
}

#[allow(clippy::similar_names)] // PID/UID/GID are the exact peer credential fields
pub(super) fn observe_collaboration_actor(stream: &Stream) -> CollaborationConnectionActor {
    let peer = stream.peer_identity();
    let caller_pid = peer.pid;
    let caller_uid = peer.uid;
    let caller_gid = peer.gid;
    let (executable, process_kind) =
        caller_pid.map_or((None, CollaborationClientKind::Unknown), process_identity);
    CollaborationConnectionActor {
        client_kind: process_kind,
        caller_pid,
        caller_uid,
        caller_gid,
        executable,
        audit_token: peer.audit_token,
        observed_pane: None,
        pane_evidence: None,
        pane_observed: false,
        request_panes: None,
    }
}

/// One request's pane inventory, per backend in `backends` order. Taken
/// fresh for every request that observes its caller; it is the same
/// inventory participant discovery would otherwise list again.
pub(super) struct RequestPaneScan {
    pub(super) per_backend: Vec<Vec<PaneInfo>>,
}

impl RequestPaneScan {
    pub(super) fn take(backends: &[SharedBackend]) -> Self {
        Self {
            per_backend: backends
                .iter()
                .map(|backend| backend.list_panes())
                .collect(),
        }
    }

    pub(super) fn into_panes(self) -> Vec<PaneInfo> {
        self.per_backend.into_iter().flatten().collect()
    }

    /// [`PaneBackend::resolve_pane`] answered from the scan for hosts whose
    /// resolver is itself a lookup in that inventory (tmux exact id, rmux
    /// namespaced id). Other hosts keep their native resolver.
    pub(super) fn resolve_unique(&self, backends: &[SharedBackend], pane: &str) -> Option<String> {
        let mut matches = std::collections::BTreeSet::new();
        for (backend, panes) in backends.iter().zip(&self.per_backend) {
            match backend.kind() {
                HostKind::Tmux | HostKind::Rmux => {
                    let id = if backend.kind() == HostKind::Rmux
                        && !pane.starts_with(crate::backend::rmux::PANE_ID_PREFIX)
                    {
                        format!("{}{pane}", crate::backend::rmux::PANE_ID_PREFIX)
                    } else {
                        pane.to_owned()
                    };
                    for found in panes.iter().filter(|found| found.pane_id == id) {
                        matches.insert((found.pane_id.clone(), found.socket.clone()));
                    }
                }
                HostKind::Cmux | HostKind::Zellij | HostKind::Herdr => {
                    if let Some(found) = backend.resolve_pane(pane) {
                        matches.insert((found.pane_id, found.socket));
                    }
                }
            }
        }
        if matches.len() == 1 {
            matches.into_iter().next().map(|(pane, _)| pane)
        } else {
            None
        }
    }

    /// [`PaneBackend::pane_pid_map`] derived from the scan for hosts whose map
    /// is their pane inventory keyed by shell pid; cmux keeps its own (empty)
    /// map because its single-surface listing is not pid evidence.
    pub(super) fn pid_map(
        &self,
        backends: &[SharedBackend],
    ) -> std::collections::HashMap<u32, String> {
        backends
            .iter()
            .zip(&self.per_backend)
            .flat_map(|(backend, panes)| match backend.kind() {
                HostKind::Cmux => backend.pane_pid_map().into_iter().collect::<Vec<_>>(),
                HostKind::Tmux | HostKind::Rmux | HostKind::Zellij | HostKind::Herdr => panes
                    .iter()
                    .filter(|pane| pane.pane_pid != 0)
                    .map(|pane| (pane.pane_pid, pane.pane_id.clone()))
                    .collect(),
            })
            .collect()
    }
}

pub(super) fn observed_process_pane(
    pid: u32,
    backends: &[SharedBackend],
    scan: &RequestPaneScan,
) -> Option<(String, CollaborationPaneEvidence)> {
    observed_unless_borrowed(
        crate::adapters::proc_ancestry::under_shared_codex_app_server(pid),
        || {
            observed_pane_in_scan(
                process_environment_pane(pid),
                pid,
                backends,
                scan,
                crate::adapters::proc_ancestry::parent_pid,
            )
        },
    )
}

/// A caller under a shared Codex app-server has no pane of its own: both its
/// inherited `TMUX_PANE` and its process ancestry lead to whichever pane
/// started that server. Report no evidence (the provenance then reads
/// `Unverifiable`) rather than another thread's pane.
pub(super) fn observed_unless_borrowed(
    shared_app_server: bool,
    observe: impl FnOnce() -> Option<(String, CollaborationPaneEvidence)>,
) -> Option<(String, CollaborationPaneEvidence)> {
    if shared_app_server {
        return None;
    }
    observe()
}

pub(super) fn observed_pane_in_scan(
    environment_pane: Option<String>,
    pid: u32,
    backends: &[SharedBackend],
    scan: &RequestPaneScan,
    parent_pid: impl Fn(u32) -> Option<u32>,
) -> Option<(String, CollaborationPaneEvidence)> {
    if let Some(pane) = environment_pane {
        if let Some(canonical) = scan.resolve_unique(backends, &pane) {
            return Some((canonical, CollaborationPaneEvidence::ProcessEnvironment));
        }
    }
    let pane_pids = scan.pid_map(backends);
    if pane_pids.is_empty() {
        return None;
    }
    if let Some(pane) = pane_pids.get(&pid) {
        return Some((pane.clone(), CollaborationPaneEvidence::ProcessAncestry));
    }
    let candidates = pane_pids
        .keys()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    let pane_pid = crate::adapters::proc_ancestry::ancestor_in_set(pid, &candidates, parent_pid)?;
    pane_pids
        .get(&pane_pid)
        .cloned()
        .map(|pane| (pane, CollaborationPaneEvidence::ProcessAncestry))
}

#[cfg(target_os = "linux")]
pub(super) fn process_environment_pane(pid: u32) -> Option<String> {
    let environment = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    environment
        .split(|byte| *byte == 0)
        .filter_map(|entry| std::str::from_utf8(entry).ok())
        .find_map(|entry| entry.strip_prefix("TMUX_PANE="))
        .filter(|pane| pane.starts_with('%'))
        .map(str::to_string)
}

#[cfg(not(target_os = "linux"))]
pub(super) fn process_environment_pane(_pid: u32) -> Option<String> {
    None
}

/// The caller's executable and client kind, read once when the connection
/// is accepted, so the provenance names the process that connected even if
/// it exits (and its pid is reused) during a long request.
pub(super) fn process_identity(pid: u32) -> (Option<String>, CollaborationClientKind) {
    #[cfg(target_os = "linux")]
    let kind = std::fs::read(format!("/proc/{pid}/cmdline")).ok().map_or(
        CollaborationClientKind::Unknown,
        |bytes| {
            bytes
                .split(|byte| *byte == 0)
                .filter_map(|arg| std::str::from_utf8(arg).ok())
                .find_map(client_kind_from_arg)
                .unwrap_or(CollaborationClientKind::Unknown)
        },
    );
    #[cfg(not(target_os = "linux"))]
    let kind = CollaborationClientKind::Unknown;
    (process_executable(pid), kind)
}

/// The executable of `pid`, without spawning a process: `/proc/<pid>/exe` on
/// Linux, a single-process query elsewhere. On macOS this used to be a
/// `ps -o comm=` spawn on every IPC connection.
pub(super) fn process_executable(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
    }
    #[cfg(not(target_os = "linux"))]
    {
        crate::process_snapshot::process_executable(pid)
    }
}

#[cfg(target_os = "linux")]
pub(super) fn client_kind_from_arg(arg: &str) -> Option<CollaborationClientKind> {
    match arg {
        "watch" => Some(CollaborationClientKind::Watch),
        "mcp" => Some(CollaborationClientKind::Mcp),
        "dashboard" => Some(CollaborationClientKind::Dashboard),
        "msg" | "peers" | "identity" => Some(CollaborationClientKind::Cli),
        _ => None,
    }
}

pub(super) fn represented_participant(
    response: &Response,
    origin: &CollaborationOrigin,
) -> Option<Participant> {
    if let Some(room) = response.room.as_ref() {
        return Some(room.current.clone());
    }
    let requests = response
        .collaboration_request
        .iter()
        .chain(response.collaboration_requests.iter().flatten())
        .chain(
            response
                .collaboration_page
                .iter()
                .flat_map(|page| &page.requests),
        );
    if origin.console {
        // The origin pane is provenance (where the operator opened the
        // console), not the represented identity. In particular, when the
        // console sends to that same pane, matching by `origin.pane` would
        // incorrectly record the recipient as the represented sender.
        return requests
            .flat_map(|request| [&request.from, &request.to])
            .find(|participant| participant.console)
            .cloned();
    }
    let requests: Vec<_> = requests.collect();
    if let Some(session) = origin.agent_session_id.as_deref() {
        if let Some(participant) = requests
            .iter()
            .flat_map(|request| [&request.from, &request.to])
            .find(|participant| participant.agent_session_id == session)
        {
            return Some(participant.clone());
        }
    }
    for request in requests {
        for participant in [&request.from, &request.to] {
            if participant.pane == origin.pane
                && origin
                    .socket
                    .as_deref()
                    .is_none_or(|socket| participant.socket.as_deref() == Some(socket))
            {
                return Some(participant.clone());
            }
        }
    }
    None
}
