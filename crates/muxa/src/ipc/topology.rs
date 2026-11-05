use super::{collaboration, Agent, CollaborationOrigin, HostKind, NewRequest, SharedBackend};

/// Resolve the backend that governs `pane`'s id namespace (`%…` → tmux,
/// `herdr:…` → herdr, `zellij:…` → zellij).
///
/// - A pane id that classifies to a KNOWN namespace whose backend is in the
///   active set → `Ok(backend)`.
/// - A pane id that classifies to a known namespace whose backend is NOT
///   observed → `Err(kind)`: a **structured refusal**. We must NOT fall back
///   to the primary here — routing e.g. a `herdr:` keystroke onto the tmux
///   backend would inject into the wrong host entirely. The caller turns this
///   into a `namespace-unavailable` error.
/// - An UNCLASSIFIED pane id (legacy/synthetic/unknown shape) → `Ok(primary)`:
///   only these fall back to `backends[0]`, which is never empty (the `Server`
///   builder guarantees it), preserving pre-guard behavior.
pub(super) fn resolve_backend<'a>(
    backends: &'a [SharedBackend],
    pane: &str,
) -> Result<&'a SharedBackend, HostKind> {
    match crate::backend::pane_id_host_kind(pane) {
        Some(kind) => backends.iter().find(|b| b.kind() == kind).ok_or(kind),
        None => Ok(&backends[0]),
    }
}

/// Resolve the one recorded endpoint for a pane-id control operation.
/// Pane ids repeat across tmux and rmux servers, so silently choosing the
/// first `HashMap` row could inject into an unrelated pane. Duplicate agents on
/// the same endpoint are harmless; distinct endpoints are an explicit error.
pub(super) fn unique_pane_endpoint(pane: &str, agents: &[Agent]) -> Result<Option<String>, String> {
    let mut endpoints = agents
        .iter()
        .filter_map(|agent| agent.tmux_socket.as_deref())
        .map(|endpoint| crate::backend::pane_endpoint_identity(Some(pane), endpoint))
        .collect::<Vec<_>>();
    endpoints.sort();
    endpoints.dedup();
    match endpoints.as_slice() {
        [] => Ok(None),
        [endpoint] => Ok(Some(endpoint.clone())),
        _ => Err(format!(
            "ambiguous pane {pane}: it exists on multiple endpoints; use socket-scoped dashboard control"
        )),
    }
}

/// The pane inventory a collaboration call is resolved against, plus the
/// participants derived from it. The raw panes travel alongside because an
/// operator console origin has no participant row — its room comes from the
/// pane it was opened from, which need not host an agent.
pub(super) struct CollaborationTopology {
    pub(super) participants: Vec<collaboration::Participant>,
    pub(super) panes: Vec<crate::tmux::PaneInfo>,
    /// The raw registry rows behind `participants`, kept so a pane whose agent
    /// has not registered yet can still be addressed explicitly — a real row
    /// is not the only evidence that an agent CLI occupies a pane.
    pub(super) agents: Vec<crate::state::Agent>,
}

impl CollaborationTopology {
    /// The room a pane belongs to, whether or not it hosts a tracked agent.
    /// A handle is allocated at session start, which is exactly when the pane
    /// may not be a participant yet.
    ///
    /// Pane ids are unique per server only, so a caller that names its server
    /// gets the pane on that server — or no room — never the first `%1` the
    /// scan happens to list from another one.
    pub(super) fn room_of(
        &self,
        pane: &str,
        socket: Option<&str>,
    ) -> Option<collaboration::RoomId> {
        self.panes
            .iter()
            .find(|info| {
                info.pane_id == pane
                    && socket.is_none_or(|socket| {
                        info.socket.as_deref().is_some_and(|candidate| {
                            crate::backend::pane_endpoints_match(Some(pane), candidate, socket)
                        })
                    })
            })
            .map(|info| {
                collaboration::room_of_pane(
                    pane,
                    info,
                    info.socket
                        .clone()
                        .or_else(|| socket.map(ToString::to_string)),
                )
            })
    }

    pub(super) fn resolve_origin(
        &self,
        origin: &CollaborationOrigin,
    ) -> Result<collaboration::Participant, collaboration::CollaborationError> {
        collaboration::resolve_origin(origin, &self.participants, &self.panes)
    }

    /// Fill execution context that older/minimal collaboration clients do not
    /// know how to send. Work identity comes from the exact pane inventory row
    /// for the represented endpoint; the run id is a snapshot of that
    /// endpoint's room rather than a durable Work identity.
    pub(super) fn stamp_request_metadata(
        &self,
        anchor: &collaboration::Participant,
        request: &mut NewRequest,
    ) {
        if request.workspace_id.is_none() || request.work_id.is_none() {
            if let Some(pane) = self
                .panes
                .iter()
                .find(|pane| pane.pane_id == anchor.pane && pane.socket == anchor.socket)
            {
                if request.workspace_id.is_none() {
                    request.workspace_id.clone_from(&pane.workspace_id);
                }
                if request.work_id.is_none() {
                    request.work_id.clone_from(&pane.work_id);
                }
            }
        }
        if request.run_id.is_none() {
            request.run_id = Some(format!(
                "{}:{}:{}",
                anchor.room.host,
                anchor.room.socket.as_deref().unwrap_or("default"),
                anchor.room.window_id
            ));
        }
    }

    /// Resolve `target`, and the logical slot to record when it named one:
    /// a `workspace/work/role[@alias]` path, `@alias` or `role:<role>`.
    /// Pane targets pin a session and record no slot.
    pub(super) fn resolve_target_with_slot(
        &self,
        sender: &collaboration::Participant,
        target: &str,
        scope: crate::config::CollaborationScope,
    ) -> Result<
        (
            collaboration::Participant,
            Option<collaboration::RecipientSlot>,
        ),
        collaboration::CollaborationError,
    > {
        if let Some(resolved) =
            collaboration::resolve_slot_path(sender, target, &self.participants, &self.panes)
        {
            // A path names its work explicitly, like a pane id, so it reaches
            // other windows under the same scope rule as pane targets.
            return resolved.and_then(|(recipient, slot)| {
                if scope == crate::config::CollaborationScope::Window
                    && recipient.room != sender.room
                {
                    return Err(collaboration::CollaborationError::UnknownTarget(
                        target.to_string(),
                    ));
                }
                Ok((recipient, Some(slot)))
            });
        }
        let recipient = self.resolve_pinned_target(sender, target, scope)?;
        let alias = target
            .strip_prefix('@')
            .or_else(|| target.strip_prefix("alias:"));
        let role = target.strip_prefix("role:");
        let slot = (alias.is_some() || role.is_some())
            .then(|| collaboration::RecipientSlot::of(&recipient, &self.panes, role, alias));
        Ok((recipient, slot))
    }

    pub(super) fn resolve_pinned_target(
        &self,
        sender: &collaboration::Participant,
        target: &str,
        scope: crate::config::CollaborationScope,
    ) -> Result<collaboration::Participant, collaboration::CollaborationError> {
        collaboration::resolve_target(sender, target, &self.participants, scope).or_else(|error| {
            collaboration::resolve_pending_pane_target(
                sender,
                target,
                &self.participants,
                &self.agents,
                &self.panes,
                scope,
            )
            .map_err(|_| error)
        })
    }
}
