//! Bounded, body-free relationship data for operator overlays.
use super::{
    request_visible, CollaborationError, CollaborationStore, MailboxScope, Participant,
    RequestKind, RequestMailbox, RequestStatus,
};
use serde::{Deserialize, Serialize};

/// Multiplexer location, kept with endpoints even when the peer is outside
/// the visible window or no longer running. Older snapshots omit it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollaborationLocation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    pub window_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollaborationEndpoint {
    pub agent_session_id: String,
    pub pane: String,
    pub socket: Option<String>,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<CollaborationLocation>,
}
impl From<&Participant> for CollaborationEndpoint {
    fn from(value: &Participant) -> Self {
        Self {
            agent_session_id: value.agent_session_id.clone(),
            pane: value.pane.clone(),
            socket: value.socket.clone(),
            label: value.label(),
            location: Some(CollaborationLocation {
                session_id: value.tmux_session_id.clone(),
                session_name: value.tmux_session_name.clone(),
                window_id: value.room.window_id.clone(),
                window_name: value.window_name.clone(),
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollaborationLink {
    pub request_id: String,
    pub from: CollaborationEndpoint,
    pub to: CollaborationEndpoint,
    pub kind: RequestKind,
    pub status: RequestStatus,
    pub reply_received: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CollaborationOverview {
    pub participants: Vec<Participant>,
    pub links: Vec<CollaborationLink>,
}

impl CollaborationStore {
    pub async fn overview_for(
        &self,
        caller: &Participant,
        participants: &[Participant],
    ) -> Result<CollaborationOverview, CollaborationError> {
        if !caller.console {
            return Err(CollaborationError::ScopeDenied);
        }
        let requests = self.requests.read().await;
        let mut visible: Vec<_> = requests
            .values()
            .filter(|request| {
                request_visible(request, caller, RequestMailbox::All, MailboxScope::Room)
            })
            .collect();
        // Active work first; terminal replies are observations, never success judgments.
        visible.sort_by_key(|request| {
            (
                request.status.is_terminal(),
                std::cmp::Reverse(
                    request
                        .reply
                        .as_ref()
                        .map_or(request.created_at, |reply| reply.at),
                ),
                std::cmp::Reverse(&request.id),
            )
        });
        let links = visible
            .into_iter()
            .take(64)
            .map(|request| CollaborationLink {
                request_id: request.id.clone(),
                from: (&request.from).into(),
                to: (&request.to).into(),
                kind: request.kind,
                status: request.status,
                reply_received: request.reply.is_some(),
            })
            .collect();
        Ok(CollaborationOverview {
            participants: participants
                .iter()
                .filter(|p| p.room == caller.room)
                .cloned()
                .collect(),
            links,
        })
    }
}
