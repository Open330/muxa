use super::*;

// WS-F: snapshot
/// A finished snapshot request as a response: the CLI's document, or its
/// error line.
pub(super) fn mux_snapshot_response(
    outcome: Result<serde_json::Value, crate::mux_snapshot_control::MuxSnapshotError>,
) -> Response {
    match outcome {
        Ok(value) => Response::with_mux_snapshot(value),
        Err(error) => Response::err(error.to_string()),
    }
}

pub(super) fn collaboration_audit_entry(
    actor: &CollaborationConnectionActor,
    context: CollaborationAuditContext,
    response: &Response,
) -> crate::collaboration_audit::CollaborationAuditEntry {
    let represented = represented_participant(response, &context.represented_origin);
    let response_request_id = response
        .collaboration_request
        .as_ref()
        .map(|request| request.id.as_str());
    let result_count = response
        .collaboration_requests
        .as_ref()
        .map(Vec::len)
        .or_else(|| {
            response
                .collaboration_page
                .as_ref()
                .map(|page| page.requests.len())
        })
        .or_else(|| response.collaboration_request.as_ref().map(|_| 1))
        .or_else(|| {
            response
                .collaboration_mailbox_snapshot
                .as_ref()
                .map(|s| s.incoming.len() + s.sent.len())
        });
    let provenance = actor.provenance(&context.represented_origin);
    context.finish(
        provenance,
        represented.as_ref(),
        response_request_id,
        result_count,
        response.error.as_deref(),
    )
}

pub(super) fn orchestration_config_response(
    path: Option<&Path>,
    update: Option<(&str, &crate::orchestration::OrchestrationConfig)>,
) -> Response {
    let Some(path) = path else {
        return Response::err(NO_CONFIG_PATH);
    };
    let result = match update {
        Some((expected, settings)) => {
            crate::config_file::write_orchestration(path, expected, settings)
        }
        None => crate::config_file::read(path),
    };
    match result {
        Ok(document) => match toml::from_str::<crate::Config>(&document.text) {
            Ok(config) => {
                let mut response = Response::with_config(document);
                response.orchestration = Some(config.orchestration);
                response
            }
            Err(e) => Response::err(e.to_string()),
        },
        Err(crate::config_file::ConfigFileError::Conflict { current }) => {
            let mut response =
                Response::err("config.toml changed; reload before saving Fleet policy");
            response.config = Some(crate::config_file::ConfigDocument {
                path: path.into(),
                exists: true,
                text: current,
            });
            response
        }
        Err(e) => Response::err(e.to_string()),
    }
}

/// A swap request returns once its job is accepted, not when it ends; the
/// bound covers planning (process scans, `aas candidates`).
pub(super) const SWAP_CLIENT_TIMEOUT: Duration = Duration::from_secs(120);
