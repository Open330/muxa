//! Bounded, body-free relationship data for operator overlays.
use super::{
    request_visible, CollaborationError, CollaborationStore, MailboxScope, Participant,
    RequestKind, RequestMailbox, RequestStatus,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollaborationEndpoint {
    pub agent_session_id: String,
    pub pane: String,
    pub socket: Option<String>,
    pub label: String,
}
impl From<&Participant> for CollaborationEndpoint {
    fn from(value: &Participant) -> Self {
        Self {
            agent_session_id: value.agent_session_id.clone(),
            pane: value.pane.clone(),
            socket: value.socket.clone(),
            label: value.label(),
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
