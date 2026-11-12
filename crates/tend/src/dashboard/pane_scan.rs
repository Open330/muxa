//! The multi-host pane inventory behind `/api/panes` and the Work
//! projection. Compiled without the `dashboard` feature too: `tend watch`
//! builds the same projection from it without linking the HTTP server.

use std::path::PathBuf;
use std::sync::Arc;

use time::OffsetDateTime;

use crate::backend::{HostKind, SharedBackend};
use crate::tmux::scanner::{self, PaneSummary, TendPaneMetadata};

/// Pull one multi-host inventory: the tmux scanner plus every other active
/// backend (rmux, zellij, …), which a tmux scan alone never sees. The two run
/// concurrently; each backend is sync-blocking, so those calls stay on
/// Tokio's blocking pool. Work projections must be built from this, or a
/// host whose panes live on rmux shows no Work at all.
pub async fn multi_host_pane_scan(backends: Arc<Vec<SharedBackend>>) -> scanner::ScanResult {
    let non_tmux = backends
        .iter()
        .filter(|backend| backend.kind() != HostKind::Tmux)
        .cloned()
        .collect::<Vec<_>>();
    let backend_scan = tokio::task::spawn_blocking(move || {
        non_tmux
            .into_iter()
            .map(|backend| {
                let panes = backend.work_panes().unwrap_or_else(|| {
                    backend
                        .list_panes()
                        .into_iter()
                        .map(|pane| (pane, TendPaneMetadata::default()))
                        .collect()
                });
                (backend.kind(), panes)
            })
            .collect::<Vec<_>>()
    });
    let (mut scan, backend_rows) = tokio::join!(scanner::scan(), backend_scan);
    if let Ok(backend_rows) = backend_rows {
        for (kind, panes) in backend_rows {
            let additional = if kind == HostKind::Herdr {
                scanner::herdr_scan_result(panes.into_iter().map(|(pane, _)| pane).collect())
            } else {
                backend_scan_result(kind, panes)
            };
            scan = merge_pane_scans(scan, additional);
        }
    }
    scan
}

/// Convert a non-tmux backend's common `PaneInfo` rows to the dashboard wire
/// shape. rmux retains its full socket path; hosts without endpoint paths use
/// a stable host label. Bare-terminal attach commands remain tmux-only.
pub(super) fn backend_scan_result(
    kind: HostKind,
    panes: Vec<(crate::tmux::PaneInfo, TendPaneMetadata)>,
) -> scanner::ScanResult {
    let panes = panes
        .into_iter()
        .map(|(pane, tend)| {
            let socket = match kind {
                HostKind::Rmux | HostKind::Cmux => pane
                    .socket
                    .as_deref()
                    .map_or_else(|| PathBuf::from(kind.to_string()), PathBuf::from),
                HostKind::Herdr => PathBuf::from("herdr"),
                HostKind::Zellij => PathBuf::from("zellij"),
                HostKind::Tmux => pane
                    .socket
                    .as_deref()
                    .map_or_else(|| PathBuf::from("tmux"), PathBuf::from),
            };
            PaneSummary {
                host: kind,
                pane_id: pane.pane_id,
                session_id: pane.session_id,
                session: pane.session,
                window_id: pane.window_id,
                window_name: pane.window_name,
                window_index: pane.window_index,
                pane_index: pane.pane_index,
                tty: pane.tty,
                current_command: pane.current_command,
                title: pane.title,
                current_path: pane.current_path,
                socket,
                tend,
                attach_command: String::new(),
            }
        })
        .collect();
    scanner::ScanResult {
        panes,
        errors: Vec::new(),
        fetched_at: OffsetDateTime::now_utc(),
    }
}

/// Append one backend inventory while preserving earlier scan errors and the
/// original fetch timestamp.
pub(super) fn merge_pane_scans(
    mut base: scanner::ScanResult,
    additional: scanner::ScanResult,
) -> scanner::ScanResult {
    base.panes.extend(additional.panes);
    base.errors.extend(additional.errors);
    base
}
