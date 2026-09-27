use super::routes::Failure;
use super::{Grant, Target};
use crate::{dashboard::server::AppState, tmux::PaneInfo, HostKind, PaneKey};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
#[derive(Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum Scope {
    #[default]
    Pane,
    Window,
}

pub(super) async fn pin(
    kind: HostKind,
    pane: PaneInfo,
    requested_socket: Option<&str>,
) -> Result<Target, Failure> {
    if pane.socket.as_ref().is_none_or(String::is_empty)
        || pane.session_id.is_empty()
        || pane.window_id.is_empty()
    {
        return Err((
            StatusCode::CONFLICT,
            "pane must expose an exact socket, session and window identity",
        ));
    }
    // Dashboard tmux rows carry full paths; backend rows use short names.
    // Resolve once and retain the absolute path for every later operation.
    let observed_socket = pane.socket.as_deref().expect("checked socket");
    let socket = if kind == HostKind::Tmux {
        let name = crate::tmux::socket_short_name(observed_socket);
        if crate::tmux::scanner::enumerate_sockets()
            .iter()
            .filter(|path| path.file_name().and_then(|name| name.to_str()) == Some(name.as_str()))
            .count()
            > 1
        {
            return Err((StatusCode::CONFLICT, "multiple tmux servers use this socket name; assign unique socket names before sharing"));
        }
        crate::tmux::socket_path_or_default(observed_socket)
            .to_string_lossy()
            .into_owned()
    } else {
        observed_socket.to_owned()
    };
    if requested_socket.is_some_and(|requested| {
        requested.contains('/')
            && std::fs::canonicalize(requested).unwrap_or_else(|_| requested.into())
                != std::fs::canonicalize(&socket).unwrap_or_else(|_| (&socket).into())
    }) {
        return Err((
            StatusCode::CONFLICT,
            "pane socket does not match the selected server",
        ));
    }
    let Ok(stamp) = super::routes::process_stamp(pane.pane_pid).await else {
        return Err((
            StatusCode::CONFLICT,
            "cannot verify this pane's process identity",
        ));
    };
    Ok(Target {
        socket,
        key: PaneKey::from_pane(kind, &pane),
        pid: pane.pane_pid,
        tty: pane.tty,
        process_stamp: stamp,
    })
}

impl Grant {
    pub(super) fn targets(&self) -> impl Iterator<Item = &Target> {
        std::iter::once(&self.target).chain(self.peers.iter())
    }
    pub(super) fn select(&self, pane: Option<&str>) -> Result<&Target, Failure> {
        let pane = pane.unwrap_or(&self.target.key.pane_id);
        self.targets()
            .find(|target| target.key.pane_id == pane)
            .ok_or((StatusCode::FORBIDDEN, "pane is outside this invitation"))
    }
}

pub(super) async fn window_members(
    state: &AppState,
    target: &Target,
) -> Result<Vec<Target>, Failure> {
    let kind = target.key.window.session.endpoint.host;
    let backend = state
        .backends
        .iter()
        .find(|backend| backend.kind() == kind)
        .cloned()
        .ok_or((StatusCode::GONE, "pane backend unavailable"))?;
    let window = target.key.window.clone();
    let permit = state
        .sharing
        .pane_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| (StatusCode::TOO_MANY_REQUESTS, "pane service busy"))?;
    let panes = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let observed = backend.observe_panes();
        if !observed.is_complete() {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "window inventory temporarily unavailable",
            ));
        }
        Ok(observed
            .panes
            .into_iter()
            .filter(|pane| PaneKey::from_pane(kind, pane).window == window)
            .collect::<Vec<_>>())
    })
    .await
    .map_err(|_| (StatusCode::BAD_GATEWAY, "window lookup failed"))??;
    if panes.is_empty() || panes.len() > 16 {
        return Err((
            StatusCode::BAD_REQUEST,
            "window sharing requires between 1 and 16 panes",
        ));
    }
    let mut peers = Vec::new();
    let mut original_found = false;
    for pane in panes {
        let pinned = pin(kind, pane, Some(&target.socket)).await?;
        if pinned.key.pane_id == target.key.pane_id {
            if &pinned != target {
                return Err((
                    StatusCode::CONFLICT,
                    "window changed during sharing; try again",
                ));
            }
            original_found = true;
        } else {
            peers.push(pinned);
        }
    }
    if !original_found {
        return Err((
            StatusCode::CONFLICT,
            "window changed during sharing; try again",
        ));
    }
    peers.sort_by(|a, b| a.key.pane_id.cmp(&b.key.pane_id));
    Ok(peers)
}
