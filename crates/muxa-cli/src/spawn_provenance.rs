//! Creation history is metadata, never inferred from messages or pane placement.
use anyhow::{Context, Result};
use muxa::backend::PaneBackend;
use muxa::collaboration::CollaborationEndpoint;
use muxa::tmux::layout::SpawnProvenance;
use std::path::Path;

/// Resolve through muxad so shared app-server environments cannot invent a parent.
pub(crate) fn resolve_creator(socket: &Path) -> Option<CollaborationEndpoint> {
    let origin = if let Ok(session) = std::env::var("CODEX_THREAD_ID") {
        if session.trim().is_empty() {
            return None;
        }
        muxa::collaboration::CollaborationOrigin {
            exact_session: false,
            pane: String::new(),
            socket: None,
            console: false,
            agent_session_id: Some(session),
        }
    } else {
        if crate::mcp::pane_env_is_borrowed() {
            return None;
        }
        crate::mcp::current_collaboration_origin().ok()?
    };
    let response = muxa::ipc::blocking_call(
        socket,
        &serde_json::json!({
            "protocol": muxa::event::PROTOCOL_VERSION,
            "kind": "collaboration_context", "origin": origin,
        }),
        std::time::Duration::from_secs(2),
    )?;
    let room: muxa::collaboration::RoomContext =
        serde_json::from_value(response["room"].clone()).ok()?;
    (!room.current.console).then(|| (&room.current).into())
}

pub(crate) fn record(request: &crate::agent_launch::StartRequest, pane: &str, launch_id: &str) {
    let Some(creator) = request.creator.clone() else {
        return;
    };
    let child = if let Some(rmux) = crate::mux_control::ambient_rmux() {
        rmux.resolve_pane(&format!("rmux:{pane}"))
    } else {
        muxa::TmuxBackend::new().resolve_pane(pane)
    };
    let Some(child) = child else {
        eprintln!(
            "warning: pane {pane} started, but creation provenance could not resolve its endpoint"
        );
        return;
    };
    let provenance = SpawnProvenance {
        launch_id: launch_id.to_owned(),
        created_at: time::OffsetDateTime::now_utc().to_string(),
        creator,
        child_pane: child.pane_id,
        child_socket: child.socket,
    };
    if let Err(error) = save(pane, &provenance) {
        // A bookkeeping failure must not kill a running worker or cause a respawn.
        eprintln!(
            "warning: pane {pane} started, but creation provenance was not fully saved: {error:#}"
        );
    }
}

fn save(pane: &str, provenance: &SpawnProvenance) -> Result<()> {
    let json = serde_json::to_vec(provenance)?;
    let directory = muxa::paths::data_dir()
        .context("no data directory")?
        .join("muxa")
        .join("spawns");
    archive(&directory, provenance, &json)?;
    let value = std::str::from_utf8(&json)?;
    let output = crate::mux_control::ambient_command_with_args(&[
        "set-option",
        "-p",
        "-t",
        pane,
        "@muxa_spawn",
        value,
    ])?
    .output()?;
    anyhow::ensure!(
        output.status.success(),
        "cannot stamp creation metadata: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn archive(directory: &Path, provenance: &SpawnProvenance, json: &[u8]) -> Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(directory)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join(format!("{}.json", provenance.launch_id)))?;
    file.write_all(json)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_keeps_distinct_launches_and_never_overwrites_a_creator() {
        let directory = tempfile::tempdir().unwrap();
        let mut provenance = SpawnProvenance {
            launch_id: uuid::Uuid::new_v4().to_string(),
            created_at: "2026-10-22".into(),
            creator: CollaborationEndpoint {
                location: None,
                agent_session_id: "original-session".into(),
                pane: "rmux:%37".into(),
                socket: Some("/tmp/server-a".into()),
                label: "builder".into(),
            },
            child_pane: "rmux:%52".into(),
            child_socket: Some("/tmp/server-b".into()),
        };
        let original_id = provenance.launch_id.clone();
        let json = serde_json::to_vec(&provenance).unwrap();
        archive(directory.path(), &provenance, &json).unwrap();
        assert!(archive(directory.path(), &provenance, b"replacement").is_err());
        provenance.launch_id = uuid::Uuid::new_v4().to_string();
        archive(
            directory.path(),
            &provenance,
            &serde_json::to_vec(&provenance).unwrap(),
        )
        .unwrap();
        let stored: SpawnProvenance = serde_json::from_slice(
            &std::fs::read(directory.path().join(format!("{original_id}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(stored.creator.agent_session_id, "original-session");
        assert_eq!(stored.child_socket.as_deref(), Some("/tmp/server-b"));
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
    }
}
