use super::{
    read_limited_line, response_error, server_protocol_ceiling, Arc, AsyncWriteExt, BufReader,
    Client, CollaborationClientKind, CollaborationUpdateStream, Duration, Hello, Path, PathBuf,
    RuntimeError, Stream, TransitionStream, CLIENT_CALL_TIMEOUT, MAX_IPC_LINE_BYTES,
    PROTOCOL_VERSION, WORK_COMMAND_CLIENT_TIMEOUT,
};
impl Client {
    pub fn new(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            collaboration_client_kind: CollaborationClientKind::Unknown,
            mailbox_cache: None,
        }
    }

    /// The daemon this client talks to.
    ///
    /// Callers that hand a socket to something else — a blocking helper, a
    /// child process — need the one this client resolved, not whatever
    /// `paths::default_socket()` would pick.
    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket_path
    }

    /// Label this client for collaboration provenance. This changes audit
    /// metadata only; it grants and removes no authority.
    #[must_use]
    pub fn with_collaboration_client_kind(mut self, kind: CollaborationClientKind) -> Self {
        self.collaboration_client_kind = kind;
        self
    }

    /// Only relay compatibility paths need to retain full display rows.
    /// Ordinary daemon clients and token-aware controllers retain none here.
    #[must_use]
    pub fn with_mailbox_compat_cache(mut self) -> Self {
        self.mailbox_cache = Some(Arc::default());
        self
    }

    pub(super) async fn forget_mailbox_cache(&self, key: &str) {
        if let Some(cache) = &self.mailbox_cache {
            cache.lock().await.remove(key);
        }
    }

    pub(super) async fn revision_subscribe_inner(
        &self,
        kind: &str,
    ) -> Result<CollaborationUpdateStream, RuntimeError> {
        let stream =
            Stream::connect(&self.socket_path)
                .await
                .map_err(|error| match error.kind() {
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound => {
                        RuntimeError::NotConnected(self.socket_path.clone())
                    }
                    _ => RuntimeError::Io(error),
                })?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        self.send_hello(&mut reader, &mut writer).await?;

        let mut request = serde_json::to_vec(&serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": kind,
        }))?;
        request.push(b'\n');
        writer.write_all(&request).await?;
        writer.flush().await?;

        let mut ack = String::new();
        read_limited_line(&mut reader, &mut ack).await?;
        let ack: serde_json::Value = serde_json::from_str(ack.trim())?;
        if !ack["ok"].as_bool().unwrap_or(false) {
            return Err(RuntimeError::Json(serde::de::Error::custom(format!(
                "{kind} rejected: {}",
                ack["error"].as_str().unwrap_or("(no error message)")
            ))));
        }
        drop(writer);
        Ok(CollaborationUpdateStream {
            reader,
            line: String::new(),
        })
    }

    // WS-F: snapshot
    /// The workspace snapshots, newest first, as `muxa snapshot --list
    /// --json` prints them. Requires `mux_snapshot_v1`.
    pub async fn mux_snapshot_list(&self) -> Result<serde_json::Value, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "mux_snapshot_list",
        });
        let response = self
            .call_with_timeout(&req, WORK_COMMAND_CLIENT_TIMEOUT)
            .await?;
        if !response["ok"].as_bool().unwrap_or(false) {
            return Err(response_error(&response));
        }
        Ok(response["mux_snapshot"].clone())
    }

    /// Ask the daemon which additive features it supports and, when it can
    /// self-restart, which process-image generation is currently serving.
    pub async fn hello(&self, deadline: Duration) -> Result<Hello, RuntimeError> {
        let hello = |protocol: u32| {
            serde_json::json!({
                "protocol": protocol,
                "kind": "hello",
                "client": self.collaboration_client_kind.hello_label(),
            })
        };
        let mut resp = self
            .call_with_timeout(&hello(PROTOCOL_VERSION), deadline)
            .await?;
        // A daemon too old to speak our protocol refuses the handshake
        // outright, naming the range it does support. Take it at its word and
        // ask again at its ceiling.
        //
        // Without this, `hello` fails against exactly the daemon a caller most
        // needs to reach: the stale one. `muxa daemon restart` opens with a
        // `hello`, so the remedy for a version-skewed daemon was itself
        // refused by that daemon — the diagnosis was reachable, the fix was
        // not. Negotiation is per-connection and `hello` gets its own, so
        // stepping down here cannot narrow any other call's protocol.
        if !resp["ok"].as_bool().unwrap_or(false) {
            let ceiling = server_protocol_ceiling(resp["error"].as_str())
                .filter(|supported| *supported < PROTOCOL_VERSION);
            if let Some(supported) = ceiling {
                resp = self.call_with_timeout(&hello(supported), deadline).await?;
            }
        }
        if !resp["ok"].as_bool().unwrap_or(false) {
            return Err(RuntimeError::Json(serde::de::Error::custom(format!(
                "hello rejected: {}",
                resp["error"].as_str().unwrap_or("(no error message)")
            ))));
        }
        Ok(Hello {
            collaboration_error: resp["collaboration_error"].as_str().map(str::to_owned),
            capabilities: resp["capabilities"]
                .as_array()
                .map(|capabilities| {
                    capabilities
                        .iter()
                        .filter_map(|capability| capability.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            generation: resp["generation"].as_u64(),
            version: resp["version"].as_str().map(str::to_string),
            protocol: resp["protocol"]
                .as_u64()
                .unwrap_or(0)
                .try_into()
                .unwrap_or(0),
        })
    }

    /// Ask the daemon on this socket to drain and re-exec itself. Acceptance
    /// is not completion; callers confirm completion by waiting for `hello`'s
    /// generation to advance.
    pub async fn restart(&self, deadline: Duration) -> Result<(), RuntimeError> {
        // Sent unversioned on purpose. `restart` carries no payload for two
        // builds to disagree about, and a daemon stale enough to need
        // restarting is exactly the one that would refuse a request stamped
        // with this CLI's protocol number. `protocol: 0` is the wire's
        // "unversioned" value, which every daemon accepts.
        let req = serde_json::json!({ "protocol": 0, "kind": "restart" });
        let resp = self.call_with_timeout(&req, deadline).await?;
        if !resp["ok"].as_bool().unwrap_or(false) {
            return Err(RuntimeError::Json(serde::de::Error::custom(format!(
                "restart rejected: {}",
                resp["error"].as_str().unwrap_or("(no error message)")
            ))));
        }
        Ok(())
    }

    /// Ask the daemon on this socket to drain and stop. Acceptance is not
    /// completion; callers confirm completion by waiting for the socket to
    /// stop answering.
    pub async fn stop(&self, deadline: Duration) -> Result<(), RuntimeError> {
        // Unversioned for the same reason as `restart` above: stopping a
        // daemon must not depend on agreeing with it about the wire format.
        let req = serde_json::json!({ "protocol": 0, "kind": "stop" });
        let resp = self.call_with_timeout(&req, deadline).await?;
        if !resp["ok"].as_bool().unwrap_or(false) {
            return Err(RuntimeError::Json(serde::de::Error::custom(format!(
                "stop rejected: {}",
                resp["error"].as_str().unwrap_or("(no error message)")
            ))));
        }
        Ok(())
    }

    /// Open a long-lived subscription to state transitions. Returns
    /// a stream-like handle whose `recv()` yields the next
    /// `Transition` from the daemon, or `None` when the daemon
    /// closes the connection (shutdown).
    ///
    /// Designed for `muxa watch` to drop polling latency from 500 ms
    /// to ~1 ms while keeping a slower fallback poll for catch-up
    /// after reconnects or `Lagged` drops on the server side.
    pub async fn subscribe(&self) -> Result<TransitionStream, RuntimeError> {
        // Only the handshake is bounded — the returned stream is long-lived by
        // design. A wedged daemon must not block watch's background setup here.
        tokio::time::timeout(CLIENT_CALL_TIMEOUT, self.subscribe_inner("subscribe"))
            .await
            .map_err(|_| RuntimeError::Timeout(CLIENT_CALL_TIMEOUT))?
    }

    /// Subscribe to bounded, coalesced registry invalidations for watch.
    pub async fn subscribe_agent_changes(&self) -> Result<TransitionStream, RuntimeError> {
        tokio::time::timeout(
            CLIENT_CALL_TIMEOUT,
            self.subscribe_inner("subscribe_agent_changes"),
        )
        .await
        .map_err(|_| RuntimeError::Timeout(CLIENT_CALL_TIMEOUT))?
    }

    pub(super) async fn subscribe_inner(
        &self,
        kind: &str,
    ) -> Result<TransitionStream, RuntimeError> {
        let stream = Stream::connect(&self.socket_path)
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound => {
                    RuntimeError::NotConnected(self.socket_path.clone())
                }
                _ => RuntimeError::Io(e),
            })?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        self.send_hello(&mut reader, &mut writer).await?;

        let mut req = serde_json::to_vec(&serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": kind,
            // muxa's `TransitionStream::recv` understands the lagged marker
            // frame, so opt in — `muxa watch` and `muxa mcp`'s
            // `muxa_wait_for_change` both consume the stream through this
            // client and want the explicit overflow signal.
            "lagged_markers": true,
        }))?;
        req.push(b'\n');
        if req.len() > MAX_IPC_LINE_BYTES {
            return Err(RuntimeError::MessageTooLarge(MAX_IPC_LINE_BYTES));
        }
        writer.write_all(&req).await?;
        writer.flush().await?;

        // Server replies with a one-shot ack before the streaming
        // pump takes over.
        let mut ack = String::new();
        read_limited_line(&mut reader, &mut ack).await?;
        let ack: serde_json::Value = serde_json::from_str(ack.trim())?;
        if !ack["ok"].as_bool().unwrap_or(false) {
            return Err(RuntimeError::Json(serde::de::Error::custom(format!(
                "subscribe rejected: {}",
                ack["error"].as_str().unwrap_or("(no error message)")
            ))));
        }

        // Drop the writer immediately — we never send another byte
        // on this connection. The server detects our close-when-done
        // via EOF on its read half.
        drop(writer);
        Ok(TransitionStream {
            reader,
            line: String::new(),
        })
    }

    /// Send the capability handshake as the first message on a freshly
    /// opened connection. Best-effort: a daemon that doesn't understand
    /// `hello` (older build) returns `ok:false` with a parse error or a
    /// protocol-mismatch error; we ignore the failure so the legacy
    /// strict-match path on the daemon stays usable.
    pub(super) async fn send_hello<R, W>(
        &self,
        reader: &mut BufReader<R>,
        writer: &mut W,
    ) -> Result<(), RuntimeError>
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let mut bytes = serde_json::to_vec(&serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "hello",
            "client": self.collaboration_client_kind.hello_label(),
        }))?;
        bytes.push(b'\n');
        if bytes.len() > MAX_IPC_LINE_BYTES {
            return Err(RuntimeError::MessageTooLarge(MAX_IPC_LINE_BYTES));
        }
        writer.write_all(&bytes).await?;
        writer.flush().await?;
        let mut ack = String::new();
        if read_limited_line(reader, &mut ack).await? == 0 {
            return Err(RuntimeError::Disconnected);
        }
        // Parse but don't fail on a non-ok response — legacy daemons
        // will reject the unknown `kind`, which is fine; the caller's
        // request still goes through.
        let _ = serde_json::from_str::<serde_json::Value>(ack.trim());
        Ok(())
    }

    pub async fn call(&self, req: &serde_json::Value) -> Result<serde_json::Value, RuntimeError> {
        self.call_with_timeout(req, CLIENT_CALL_TIMEOUT).await
    }

    /// Like [`Self::call`] but with an explicit overall deadline covering the
    /// whole round trip (connect + hello + write + read). Guarantees no caller
    /// blocks forever against a wedged or half-dead daemon — the failure mode
    /// where hung hook connections once exhausted the daemon's fd budget.
    pub(super) async fn call_with_timeout(
        &self,
        req: &serde_json::Value,
        deadline: Duration,
    ) -> Result<serde_json::Value, RuntimeError> {
        tokio::time::timeout(deadline, self.call_inner(req))
            .await
            .map_err(|_| RuntimeError::Timeout(deadline))?
    }

    pub(super) async fn call_inner(
        &self,
        req: &serde_json::Value,
    ) -> Result<serde_json::Value, RuntimeError> {
        // Connect-time ECONNREFUSED/ENOENT mean the daemon socket isn't there
        // or nothing is listening — surface a friendly message that names the
        // socket path. Other IO errors (timeouts, permission denied, …) keep
        // their existing display via the `Io(#[from] _)` impl.
        let stream = Stream::connect(&self.socket_path)
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound => {
                    RuntimeError::NotConnected(self.socket_path.clone())
                }
                _ => RuntimeError::Io(e),
            })?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        self.send_hello(&mut reader, &mut writer).await?;

        let mut bytes = serde_json::to_vec(req)?;
        bytes.push(b'\n');
        if bytes.len() > MAX_IPC_LINE_BYTES {
            return Err(RuntimeError::MessageTooLarge(MAX_IPC_LINE_BYTES));
        }
        writer.write_all(&bytes).await?;
        writer.flush().await?;

        let mut line = String::new();
        if read_limited_line(&mut reader, &mut line).await? == 0 {
            return Err(RuntimeError::Disconnected);
        }
        Ok(serde_json::from_str(line.trim())?)
    }

    pub(super) async fn call_checked(
        &self,
        req: &serde_json::Value,
    ) -> Result<serde_json::Value, RuntimeError> {
        let resp = self.call(req).await?;
        if resp["ok"].as_bool().unwrap_or(false) {
            Ok(resp)
        } else {
            // One error shape for every refusal, so a protocol mismatch reads
            // the same here as it does through `decode_agents`.
            Err(response_error(&resp))
        }
    }
}
