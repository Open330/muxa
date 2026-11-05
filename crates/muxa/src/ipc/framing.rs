use super::{
    broadcast, watch, Arc, AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, FleetUpdate, HashSet,
    LabelSelector, RuntimeError, Serialize, WriteHalf, MAX_IPC_LINE_BYTES, PROTOCOL_VERSION,
    STREAM_KEEPALIVE_INTERVAL,
};

/// Rewrite enum string values introduced in a newer protocol to their
/// older-protocol equivalents so a client that negotiated an older
/// protocol doesn't choke on unknown variants. Walks the JSON tree and
/// mutates standalone string values; substrings inside larger strings
/// (e.g. a prompt that happens to contain the word `waiting_choice`) are
/// deliberately left alone. Called on the write path only when the
/// negotiated protocol is below the current `PROTOCOL_VERSION`.
pub(super) fn downgrade_wire(v: &mut serde_json::Value, protocol: u32) {
    match v {
        serde_json::Value::String(s) => {
            // `task` AgentKind is a v3 addition → Unknown for older peers.
            if protocol < 3 && s == "task" {
                *s = "unknown".to_string();
            }
            // `waiting_choice` / `needs_choice` are v2 additions.
            if protocol < 2 {
                if s == "waiting_choice" {
                    *s = "waiting_input".to_string();
                } else if s == "needs_choice" {
                    *s = "needs_input".to_string();
                }
            }
        }
        serde_json::Value::Array(xs) => xs.iter_mut().for_each(|x| downgrade_wire(x, protocol)),
        serde_json::Value::Object(m) => {
            // `agent_session_id` is the v4 canonical name. Older peers still
            // expect the historical `session_id` key, so preserve it on
            // negotiated v1-v3 responses without emitting both names.
            if protocol < 4 {
                if let Some(value) = m.remove("agent_session_id") {
                    m.insert("session_id".to_string(), value);
                }
            }
            m.values_mut().for_each(|x| downgrade_wire(x, protocol));
        }
        _ => {}
    }
}

/// Serialize a payload as a single JSON line, applying the wire downgrade
/// when the connection negotiated a protocol older than the current
/// `PROTOCOL_VERSION`. Keeps the fast path (no negotiation, or current)
/// on a direct `to_vec` so we don't pay for a `Value` round-trip on every
/// response.
pub(super) fn encode_line<T: Serialize>(
    value: &T,
    protocol: u32,
) -> Result<Vec<u8>, serde_json::Error> {
    let mut bytes = if protocol < PROTOCOL_VERSION {
        let mut v = serde_json::to_value(value)?;
        downgrade_wire(&mut v, protocol);
        serde_json::to_vec(&v)?
    } else {
        serde_json::to_vec(value)?
    };
    bytes.push(b'\n');
    Ok(bytes)
}

pub(super) fn is_client_disconnect(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
    )
}

pub(super) async fn write_line_or_closed(
    writer: &mut WriteHalf,
    bytes: &[u8],
) -> Result<bool, RuntimeError> {
    match writer.write_all(bytes).await {
        Ok(()) => {}
        Err(e) if is_client_disconnect(&e) => return Ok(false),
        Err(e) => return Err(RuntimeError::Io(e)),
    }
    match writer.flush().await {
        Ok(()) => Ok(true),
        Err(e) if is_client_disconnect(&e) => Ok(false),
        Err(e) => Err(RuntimeError::Io(e)),
    }
}

pub(super) async fn read_limited_line<R>(
    reader: &mut R,
    line: &mut String,
) -> Result<usize, RuntimeError>
where
    R: AsyncBufRead + Unpin,
{
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if bytes.is_empty() {
                return Ok(0);
            }
            break;
        }
        let take = available
            .iter()
            .position(|b| *b == b'\n')
            .map_or(available.len(), |pos| pos + 1);
        if bytes.len().saturating_add(take) > MAX_IPC_LINE_BYTES {
            return Err(RuntimeError::MessageTooLarge(MAX_IPC_LINE_BYTES));
        }
        bytes.extend_from_slice(&available[..take]);
        reader.consume(take);
        if bytes.last() == Some(&b'\n') {
            break;
        }
    }
    *line = String::from_utf8(bytes)
        .map_err(|e| RuntimeError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;
    Ok(line.len())
}

/// Only use at cancellation-safe boundaries: waiting for a new request or
/// streaming read-only updates. In-flight mutations retain the bounded drain.
pub(super) async fn until_server_shutdown<T>(
    stopping: &mut watch::Receiver<bool>,
    future: impl std::future::Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        biased;
        _ = stopping.wait_for(|value| *value) => None,
        result = future => Some(result),
    }
}

/// The stream takeover acknowledgement is read-only too. A peer may stop
/// reading before it arrives, so cancellation must cover this first write.
pub(super) async fn write_subscription_ack(
    writer: &mut WriteHalf,
    bytes: &[u8],
    stopping: &mut watch::Receiver<bool>,
) -> Result<bool, RuntimeError> {
    until_server_shutdown(stopping, write_line_or_closed(writer, bytes))
        .await
        .unwrap_or(Ok(false))
}

/// Encode the `{"event":"lagged","dropped":N}` overflow control frame — but
/// only for a subscriber that opted in (`emit = true`, from
/// `subscribe { lagged_markers: true }`).
///
/// Returns `Ok(None)` when the connection did NOT opt in, so the caller writes
/// nothing and the stream silently continues past the lag — the pre-marker
/// behavior a legacy client's `Transition` parser needs (it would otherwise
/// choke on the marker and abandon push mode). Split out so the opt-in gate is
/// unit-testable without forcing a real broadcast overflow.
pub(super) fn lagged_marker_bytes(
    emit: bool,
    dropped: u64,
    protocol: u32,
) -> Result<Option<Vec<u8>>, serde_json::Error> {
    if !emit {
        return Ok(None);
    }
    let marker = serde_json::json!({ "event": "lagged", "dropped": dropped });
    encode_line(&marker, protocol).map(Some)
}

/// Pump every state transition from `store` to `writer` as a JSON
/// line. Runs until the broadcast channel closes (daemon shutting
/// down) or the client closes its half of the socket — the first
/// failed write returns Ok(()) so the per-connection task wraps
/// cleanly.
///
/// `Lagged` errors are logged but do not terminate the stream:
/// dropping a few transitions on a slow consumer is preferable to
/// disconnecting them. The next snapshot the client takes (via the
/// fallback polling tick) will reconcile any holes.
pub(super) async fn stream_transitions(
    mut writer: WriteHalf,
    mut rx: broadcast::Receiver<crate::state::Transition>,
    protocol: u32,
    emit_lagged: bool,
) -> Result<(), RuntimeError> {
    // Periodic keepalive so a watch client that dies without a clean close is
    // detected on the next write (broken pipe) rather than lingering until the
    // next real transition — which, on an idle daemon, might be never.
    let mut keepalive = tokio::time::interval(STREAM_KEEPALIVE_INTERVAL);
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The first tick fires immediately; consume it so we don't emit a
    // keepalive the instant the stream opens.
    keepalive.tick().await;
    loop {
        tokio::select! {
            recv = rx.recv() => match recv {
                Ok(t) => {
                    let bytes = encode_line(&t, protocol)?;
                    if writer.write_all(&bytes).await.is_err() {
                        return Ok(());
                    }
                    if writer.flush().await.is_err() {
                        return Ok(());
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        dropped = n,
                        "subscribe lagged; client will reconcile via fallback poll"
                    );
                    // Emit a lagged marker ONLY for clients that opted in
                    // (`subscribe { lagged_markers: true }`). It's a distinct
                    // object shape (`event` tag, no `from`/`to`), which an
                    // opted-in reader (`TransitionStream::recv`) skips — but a
                    // pre-marker client's `Transition` parser would choke on it
                    // and abandon push mode, so an un-opted client gets the
                    // historical behavior: silently continue after the lag and
                    // let its fallback snapshot poll reconcile the gap.
                    if let Some(bytes) = lagged_marker_bytes(emit_lagged, n, protocol)? {
                        if writer.write_all(&bytes).await.is_err() {
                            return Ok(());
                        }
                        if writer.flush().await.is_err() {
                            return Ok(());
                        }
                    }
                }
            },
            _ = keepalive.tick() => {
                // A bare newline: an empty line the client's stream reader
                // skips (see `TransitionStream::recv`). Its only purpose is to
                // provoke a write error against a dead peer so this task exits
                // and frees the fd.
                if writer.write_all(b"\n").await.is_err() {
                    return Ok(());
                }
                if writer.flush().await.is_err() {
                    return Ok(());
                }
            }
        }
    }
}

/// Bound update traffic to ten invalidations per second per watcher. A watch
/// channel retains only a pending bit; sustained activity cannot grow a queue
/// or postpone delivery indefinitely. Idle streams only send keepalives.
pub(super) async fn stream_agent_changes(
    mut writer: WriteHalf,
    mut changes: watch::Receiver<()>,
    protocol: u32,
) -> Result<(), RuntimeError> {
    let mut keepalive = tokio::time::interval(STREAM_KEEPALIVE_INTERVAL);
    keepalive.tick().await;
    loop {
        tokio::select! {
            result = changes.changed() => {
                if result.is_err() { return Ok(()); }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                changes.borrow_and_update();
                let bytes = encode_line(&serde_json::json!({"event": "agents_changed"}), protocol)?;
                if !write_line_or_closed(&mut writer, &bytes).await? { return Ok(()); }
            }
            _ = keepalive.tick() => {
                if !write_line_or_closed(&mut writer, b"\n").await? { return Ok(()); }
            }
        }
    }
}

/// Stream compact Fleet cache invalidations. A notification names the host and
/// revision but is not itself a snapshot; clients coalesce bursts and fetch a
/// coherent selector-filtered snapshot. This keeps one busy remote agent from
/// making the central TUI clone and redraw every host on a fixed timer.
pub(super) async fn stream_fleet_updates(
    mut writer: WriteHalf,
    store: Arc<crate::fleet::FleetStore>,
    mut rx: broadcast::Receiver<FleetUpdate>,
    protocol: u32,
    selector: Option<LabelSelector>,
    mut visible_hosts: HashSet<String>,
) -> Result<(), RuntimeError> {
    let mut keepalive = tokio::time::interval(STREAM_KEEPALIVE_INTERVAL);
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    keepalive.tick().await;
    loop {
        tokio::select! {
            recv = rx.recv() => match recv {
                Ok(update) => {
                    let was_visible = visible_hosts.contains(&update.host);
                    let is_visible = if selector.is_none() {
                        true
                    } else {
                        store
                            .host_matches_selector(&update.host, selector.as_ref())
                            .await
                    };
                    if is_visible {
                        visible_hosts.insert(update.host.clone());
                    } else {
                        visible_hosts.remove(&update.host);
                    }
                    // A host entering or leaving the selector must invalidate
                    // the filtered snapshot. Unrelated hosts remain silent.
                    if !was_visible && !is_visible {
                        continue;
                    }
                    let bytes = encode_line(&update, protocol)?;
                    if writer.write_all(&bytes).await.is_err()
                        || writer.flush().await.is_err()
                    {
                        return Ok(());
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
                Err(broadcast::error::RecvError::Lagged(dropped)) => {
                    tracing::warn!(dropped, "fleet subscribe lagged; fallback snapshot will reconcile");
                    let selected = store.snapshot_selected(selector.as_ref()).await;
                    visible_hosts = selected
                        .hosts
                        .into_iter()
                        .map(|host| host.alias)
                        .collect();
                    let update = FleetUpdate {
                        host: "*".into(),
                        state: crate::fleet::FleetHostState::Degraded,
                        revision: None,
                        resync: true,
                        mailbox_revision: None,
                    };
                    let bytes = encode_line(&update, protocol)?;
                    if writer.write_all(&bytes).await.is_err()
                        || writer.flush().await.is_err()
                    {
                        return Ok(());
                    }
                }
            },
            _ = keepalive.tick() => {
                if writer.write_all(b"\n").await.is_err()
                    || writer.flush().await.is_err()
                {
                    return Ok(());
                }
            }
        }
    }
}

/// Stream only the monotonic durable-mailbox revision. This signal contains
/// no request content or participant identity; it is safe to propagate
/// through Fleet as a cache invalidation while mailbox reads remain scoped.
pub(super) async fn stream_revision_updates(
    writer: WriteHalf,
    changes: watch::Receiver<u64>,
    protocol: u32,
) -> Result<(), RuntimeError> {
    stream_revision_updates_filtered(writer, changes, protocol, || true).await
}

/// Drop invalidations unrelated to the consumer while retaining keepalives.
/// The predicate must be initialized before the subscription acknowledgement.
pub(super) async fn stream_revision_updates_filtered(
    mut writer: WriteHalf,
    mut changes: watch::Receiver<u64>,
    protocol: u32,
    mut should_send: impl FnMut() -> bool,
) -> Result<(), RuntimeError> {
    let mut keepalive = tokio::time::interval(STREAM_KEEPALIVE_INTERVAL);
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    keepalive.tick().await;
    loop {
        tokio::select! {
            signal = changes.changed() => {
                if signal.is_err() {
                    return Ok(());
                }
                let revision = *changes.borrow_and_update();
                if !should_send() {
                    continue;
                }
                let frame = serde_json::json!({ "revision": revision });
                let bytes = encode_line(&frame, protocol)?;
                if writer.write_all(&bytes).await.is_err()
                    || writer.flush().await.is_err()
                {
                    return Ok(());
                }
            }
            _ = keepalive.tick() => {
                if writer.write_all(b"\n").await.is_err()
                    || writer.flush().await.is_err()
                {
                    return Ok(());
                }
            }
        }
    }
}
