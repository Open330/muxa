//! Requests that take over the connection as a stream.

#![allow(clippy::wildcard_imports)] // the arms name the IPC module's items as `dispatch.rs` does

use super::*;

/// Whether `body` hands the connection to a streaming pump.
pub(super) fn takes_over(body: &RequestBody) -> bool {
    matches!(
        body,
        RequestBody::PipelineSubscribe
            | RequestBody::FleetSubscribe { .. }
            | RequestBody::AskSubscribe
            | RequestBody::CollaborationSubscribe { .. }
            | RequestBody::SubscribeAgentChanges
            | RequestBody::Subscribe { .. }
    )
}

#[allow(clippy::too_many_lines)] // one match arm per streaming request
pub(super) async fn takeover(
    ctx: &ServerCtx,
    conn: Conn,
    mut writer: WriteHalf,
    started: Instant,
    body: RequestBody,
) -> Result<(), RuntimeError> {
    let store = &ctx.store;
    let collaboration = &ctx.collaboration;
    let ask = &ctx.ask;
    let fleet = &ctx.fleet;
    let pipeline_runs = &ctx.pipeline_runs;
    let Conn {
        mut stopping,
        negotiated,
        ..
    } = conn;
    let kind: &'static str;
    match body {
        RequestBody::PipelineSubscribe => {
            let changes = pipeline_runs.subscribe();
            let stream_proto = negotiated.unwrap_or(PROTOCOL_VERSION);
            let ack_bytes = encode_line(&Response::ok(), stream_proto)?;
            if !write_subscription_ack(&mut writer, &ack_bytes, &mut stopping).await? {
                return Ok(());
            }
            return until_server_shutdown(
                &mut stopping,
                stream_revision_updates(writer, changes, stream_proto),
            )
            .await
            .unwrap_or(Ok(()));
        }
        RequestBody::FleetSubscribe { selector } => {
            kind = "fleet_subscribe";
            let Some(fleet) = &fleet else {
                let response = Response::err("fleet is not enabled in tendd");
                let bytes = encode_line(&response, negotiated.unwrap_or(PROTOCOL_VERSION))?;
                let _ = write_line_or_closed(&mut writer, &bytes).await?;
                return Ok(());
            };
            let selector = match selector
                .as_deref()
                .map(str::parse::<LabelSelector>)
                .transpose()
            {
                Ok(selector) => selector,
                Err(error) => {
                    let response = Response::err(format!("invalid label selector: {error}"));
                    let bytes = encode_line(&response, negotiated.unwrap_or(PROTOCOL_VERSION))?;
                    let _ = write_line_or_closed(&mut writer, &bytes).await?;
                    return Ok(());
                }
            };
            // Subscribe before observing the initial membership and
            // before ACK. The client fetches a fresh snapshot after
            // ACK, so every mutation is represented either there or
            // in this already-live receiver (duplicates are harmless).
            let updates = fleet.store.subscribe();
            let visible_hosts = fleet
                .store
                .snapshot_selected(selector.as_ref())
                .await
                .hosts
                .into_iter()
                .map(|host| host.alias)
                .collect::<HashSet<_>>();
            let stream_proto = negotiated.unwrap_or(PROTOCOL_VERSION);
            let ack_bytes = encode_line(&Response::ok(), stream_proto)?;
            if !write_subscription_ack(&mut writer, &ack_bytes, &mut stopping).await? {
                return Ok(());
            }
            tracing::debug!(
                elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
                kind,
                "ipc.handle (fleet stream takeover)",
            );
            return until_server_shutdown(
                &mut stopping,
                stream_fleet_updates(
                    writer,
                    fleet.store.clone(),
                    updates,
                    stream_proto,
                    selector,
                    visible_hosts,
                ),
            )
            .await
            .unwrap_or(Ok(()));
        }
        RequestBody::AskSubscribe => {
            let changes = ask.subscribe();
            let stream_proto = negotiated.unwrap_or(PROTOCOL_VERSION);
            let ack_bytes = encode_line(&Response::ok(), stream_proto)?;
            if !write_subscription_ack(&mut writer, &ack_bytes, &mut stopping).await? {
                return Ok(());
            }
            return until_server_shutdown(
                &mut stopping,
                stream_revision_updates(writer, changes, stream_proto),
            )
            .await
            .unwrap_or(Ok(()));
        }
        RequestBody::CollaborationSubscribe { requests_only } => {
            kind = "collaboration_subscribe";
            let changes = collaboration.subscribe();
            // Capture before ACK: a mutation after this baseline stays dirty,
            // even when it races the caller's first history snapshot.
            let mut request_revision = collaboration.request_revision();
            let request_store = collaboration.clone();
            let stream_proto = negotiated.unwrap_or(PROTOCOL_VERSION);
            let ack_bytes = encode_line(&Response::ok(), stream_proto)?;
            if !write_subscription_ack(&mut writer, &ack_bytes, &mut stopping).await? {
                return Ok(());
            }
            tracing::debug!(
                elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
                kind,
                "ipc.handle (collaboration stream takeover)",
            );
            return until_server_shutdown(
                &mut stopping,
                stream_revision_updates_filtered(writer, changes, stream_proto, move || {
                    let current = request_store.request_revision();
                    let dirty = current != request_revision;
                    request_revision = current;
                    !requests_only || dirty
                }),
            )
            .await
            .unwrap_or(Ok(()));
        }
        RequestBody::SubscribeAgentChanges => {
            let protocol = negotiated.unwrap_or(PROTOCOL_VERSION);
            let changes = store.subscribe_changes();
            let ack_line = encode_line(&Response::ok(), protocol)?;
            if !write_subscription_ack(&mut writer, &ack_line, &mut stopping).await? {
                return Ok(());
            }
            return until_server_shutdown(
                &mut stopping,
                stream_agent_changes(writer, changes, protocol),
            )
            .await
            .unwrap_or(Ok(()));
        }
        RequestBody::Subscribe { lagged_markers } => {
            kind = "subscribe";
            let stream_proto = negotiated.unwrap_or(PROTOCOL_VERSION);
            // Arm the receiver before acknowledging the subscription.
            // Once the client sees the ack, every later transition is
            // therefore either queued here or already being written.
            let transitions = store.subscribe();
            // Stream takeover. Send ack, then write transitions
            // until the client disconnects or tendd shuts down.
            // We deliberately do NOT return to the request loop
            // — this connection is now owned by the streaming
            // pump.
            let ack_bytes = encode_line(&Response::ok(), stream_proto)?;
            if !write_subscription_ack(&mut writer, &ack_bytes, &mut stopping).await? {
                return Ok(());
            }
            tracing::debug!(
                elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
                kind,
                "ipc.handle (stream takeover)",
            );
            return until_server_shutdown(
                &mut stopping,
                stream_transitions(writer, transitions, stream_proto, lagged_markers),
            )
            .await
            .unwrap_or(Ok(()));
        }
        other => unreachable!("dispatch routed {other:?} to the stream takeover"),
    }
}
