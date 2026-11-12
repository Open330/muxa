//! Reply wakes that follow their sender.
//!
//! A reply wake is typed into the sender's own session once it is Idle. When
//! that session ended (it exited, or the provider replaced it, as Claude Code
//! does on `/clear`), the wake had nowhere to go and the reply sat unread with
//! nobody told. Recipients already follow their slot to a successor
//! (`successor_for`); this does the same for senders.
//!
//! A request records the names its sender held when it was sent: its alias
//! and first role, in its window. When the sender's session is gone, meaning
//! not in the pane scan and with no live registry row, and exactly one other
//! live session holds that alias (or, failing that, that role), the request's
//! sender is rebound to it. A session that merely reuses the sender's pane
//! inherits nothing, as everywhere else in the mailbox. The reply wake and the right to read the reply move with
//! it. With no single successor, a human item is opened after
//! [`SENDER_GONE_GRACE`] instead of dropping the wake silently. The wake stays
//! owed either way, so a returning session still gets it.
use super::{
    next_request_id, CollaborationError, CollaborationRequest, CollaborationStore, HumanAction,
    Participant, RecipientSlot, RequestKind, RequestStatus, WorkMode,
};
use crate::state::Agent;
use crate::tmux::PaneInfo;
use crate::{AgentKind, AgentState};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// How long a terminal reply waits for its vanished sender (or a successor)
/// before a person is told.
pub const SENDER_GONE_GRACE: time::Duration = time::Duration::minutes(2);

/// Sender-side slot following, kept on the request.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SenderFollowState {
    /// The slot the sender held when it sent the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<RecipientSlot>,
    /// Sender sessions this request followed away from, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rebound_from: Vec<String>,
    /// The human item opened because the sender is gone with no successor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gone_action_request_id: Option<String>,
}

/// The names `sender` can be followed by: its alias and its first role, in
/// its window. None when it has neither.
pub(super) fn sender_slot(sender: &Participant) -> Option<RecipientSlot> {
    if sender.console
        || super::is_pending_session(&sender.agent_session_id)
        || (sender.alias.is_none() && sender.roles.is_empty())
    {
        return None;
    }
    Some(RecipientSlot {
        room: sender.room.clone(),
        workspace_id: None,
        work_id: None,
        role: sender.roles.first().cloned(),
        alias: sender.alias.clone(),
    })
}

/// Where an owed reply wake for `request` can go now.
#[derive(Debug, Clone, PartialEq)]
pub enum SenderWhereabouts {
    /// The sender's session is still around (perhaps busy, or briefly out of
    /// the pane scan); the ordinary wake handles it.
    Present,
    /// The session is gone and exactly this one session holds its slot.
    Successor(Box<Participant>),
    /// The session is gone and no single session holds its slot.
    Gone,
}

/// Judge the sender of an owed reply against the current scan and registry.
pub fn sender_whereabouts(
    request: &CollaborationRequest,
    participants: &[Participant],
    agents: &[Agent],
    panes: &[PaneInfo],
) -> SenderWhereabouts {
    let sender = &request.from;
    if sender.console
        || super::is_pending_session(&sender.agent_session_id)
        || participants
            .iter()
            .any(|p| !p.console && p.same_session(sender))
        || agents.iter().any(|agent| {
            agent.session_id == sender.agent_session_id && agent.state != AgentState::Stopped
        })
    {
        return SenderWhereabouts::Present;
    }
    let candidate = |p: &&Participant| {
        !p.console
            && !p.same_session(sender)
            && !p.same_session(&request.to)
            && !p
                .agent_session_id
                .starts_with(crate::state::SYNTHETIC_SESSION_PREFIX)
    };
    let single = |holders: Vec<&Participant>| match holders.as_slice() {
        [successor] => Some((*successor).clone()),
        _ => None,
    };
    // The one session that now holds the sender's alias, else its role.
    // Never a session merely because it reuses the sender's pane: identity
    // does not follow pane reuse.
    let Some(slot) = request
        .sender_follow
        .as_ref()
        .and_then(|follow| follow.slot.as_ref())
    else {
        return SenderWhereabouts::Gone;
    };
    let narrowed = |role: Option<&String>, alias: Option<&String>| {
        (role.is_some() || alias.is_some()).then(|| RecipientSlot {
            role: role.cloned(),
            alias: alias.cloned(),
            ..slot.clone()
        })
    };
    [
        narrowed(None, slot.alias.as_ref()),
        narrowed(slot.role.as_ref(), None),
    ]
    .into_iter()
    .flatten()
    .find_map(|slot| {
        single(
            participants
                .iter()
                .filter(candidate)
                .filter(|p| slot.held_by(p, panes))
                .collect(),
        )
    })
    .map_or(SenderWhereabouts::Gone, |successor| {
        SenderWhereabouts::Successor(Box::new(successor))
    })
}

fn gone_sender_action(
    parent: &CollaborationRequest,
    id: String,
    now: OffsetDateTime,
) -> CollaborationRequest {
    let mut action = parent.clone();
    action.id = id;
    action.late_replies.clear();
    action.retirement = None;
    action.task_contract = None;
    action.attempt = None;
    action.supersedes_request_id = None;
    action.superseded_by = None;
    action.dispatch_id = None;
    action.source_node_id = None;
    action.delegation_parent = None;
    action.interruption = None;
    action.turn_offer = None;
    action.sender_follow = None;
    action.from = Participant::console(parent.from.room.clone());
    action.from.console = false;
    action.from.agent_kind = AgentKind::Task;
    action.from.agent_session_id = "tend:reply-wake".into();
    action.from.pane = "tend-reply-wake".into();
    action.from.alias = Some("Tend reply wake".into());
    action.to = Participant::console(parent.from.room.clone());
    action.initiator = None;
    action.provenance = None;
    action.parent_request_id = Some(parent.id.clone());
    action.kind = RequestKind::Question;
    action.work_mode = WorkMode::ReadOnly;
    action.human_action = Some(HumanAction::Information);
    let status = parent.status;
    action.body = format!(
        "The reply to request {id} ({status:?}) arrived, but its sender session {session} \
         ({label}) is gone and no single session holds its slot, so nobody was told.\n\
         Read the reply with `tend msg show {id}` and decide whether to hand it to another \
         agent. Answering this item records that you have seen it; it does not move the \
         reply. If the sender's session returns, it is still woken.",
        id = parent.id,
        session = parent.from.agent_session_id,
        label = parent.from.label(),
    );
    action.status = RequestStatus::Queued;
    action.created_at = now;
    action.claimed_at = None;
    action.notified_at = None;
    action.reply_notified_at = None;
    action.reply_read_at = None;
    action.reply_wake = None;
    action.wake_delivery = None;
    action.reply = None;
    action.updates.clear();
    action
}

impl CollaborationStore {
    /// Move an owed reply's sender from the vanished `old` session to its
    /// slot's successor. Refused unless the reply wake is still owed and
    /// `old` is still the recorded sender.
    pub async fn rebind_sender(
        &self,
        request_id: &str,
        old: &Participant,
        new: Participant,
    ) -> Result<Option<CollaborationRequest>, CollaborationError> {
        self.ensure_enabled()?;
        let _transaction = self.transaction_lock.lock().await;
        let mut staged = self.stage_request_rows(&[request_id]).await;
        let Some(request) = staged.get_mut(request_id) else {
            return Ok(None);
        };
        if !request.reply_wake_owed() || !request.from.same_session(old) || new.console {
            return Ok(None);
        }
        let previous = std::mem::replace(&mut request.from, new);
        request
            .sender_follow
            .get_or_insert_with(SenderFollowState::default)
            .rebound_from
            .push(previous.agent_session_id);
        let rebound = request.clone();
        self.commit_request_rows(std::slice::from_ref(&rebound))
            .await?;
        self.publish_change();
        Ok(Some(rebound))
    }

    /// Open one human item for an owed reply whose sender is gone with no
    /// successor, once [`SENDER_GONE_GRACE`] has passed since the reply.
    /// Idempotent: the item's id is kept on the request.
    pub async fn report_gone_sender(
        &self,
        request_id: &str,
    ) -> Result<Option<CollaborationRequest>, CollaborationError> {
        self.ensure_enabled()?;
        let _transaction = self.transaction_lock.lock().await;
        let now = OffsetDateTime::now_utc();
        let mut staged = self.stage_request_rows(&[request_id]).await;
        let Some(parent) = staged.get_mut(request_id) else {
            return Ok(None);
        };
        let replied_at = parent.reply.as_ref().map(|reply| reply.at);
        if !parent.reply_wake_owed()
            || replied_at.is_none_or(|at| now - at < SENDER_GONE_GRACE)
            || parent
                .sender_follow
                .as_ref()
                .is_some_and(|follow| follow.gone_action_request_id.is_some())
        {
            return Ok(None);
        }
        let id = next_request_id(now);
        let action = gone_sender_action(parent, id.clone(), now);
        parent
            .sender_follow
            .get_or_insert_with(SenderFollowState::default)
            .gone_action_request_id = Some(id);
        let rows = [parent.clone(), action.clone()];
        self.commit_request_rows(&rows).await?;
        self.publish_change();
        tracing::info!(
            request_id,
            action_request_id = action.id,
            "reply arrived for a sender that is gone; opened a human item",
        );
        Ok(Some(action))
    }

    /// Withdraw unanswered gone-sender items whose reply has since been read
    /// or delivered (the sender came back, or someone read it).
    pub async fn retire_gone_sender_items(&self) -> Result<(), CollaborationError> {
        self.ensure_enabled()?;
        let _transaction = self.transaction_lock.lock().await;
        let rows: Vec<_> = {
            let requests = self.requests.read().await;
            requests
                .values()
                .filter(|parent| !parent.reply_wake_owed())
                .filter_map(|parent| {
                    let id = parent
                        .sender_follow
                        .as_ref()?
                        .gone_action_request_id
                        .as_ref()?;
                    let action = requests.get(id)?;
                    (action.status == RequestStatus::Queued && action.reply.is_none()).then(|| {
                        let mut retired = action.clone();
                        retired.status = RequestStatus::Cancelled;
                        retired
                    })
                })
                .collect()
        };
        if !rows.is_empty() {
            self.commit_request_rows(&rows).await?;
            self.publish_change();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collaboration::{CollaborationOptions, NewRequest};
    use std::sync::Arc;

    fn participant(session: &str, pane: &str, alias: Option<&str>) -> Participant {
        serde_json::from_value(serde_json::json!({
            "agent_kind":"claude_code", "agent_session_id":session, "pane":pane,
            "socket":"default", "alias": alias,
            "room":{"host":"tmux","socket":"default","window_id":"@1"}, "state":"idle"
        }))
        .unwrap()
    }

    fn worker(session: &str, pane: &str) -> Participant {
        let mut worker = participant(session, pane, Some("codex"));
        worker.launch_purpose = super::super::LaunchPurpose::Worker;
        worker
    }

    async fn replied(store: &Arc<CollaborationStore>, sender: &Participant) -> String {
        let reviewer = worker("reviewer", "%9");
        let id = store
            .create(
                sender.clone(),
                reviewer.clone(),
                NewRequest {
                    body: "review".into(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap()
            .id;
        store.claim_by_id(&reviewer, &id).await.unwrap();
        store
            .reply(
                &reviewer,
                &id,
                RequestStatus::Completed,
                "lgtm".into(),
                vec![],
            )
            .await
            .unwrap();
        id
    }

    async fn row(store: &CollaborationStore, id: &str) -> CollaborationRequest {
        store.requests.read().await[id].clone()
    }

    #[tokio::test]
    async fn a_reply_follows_its_sender_to_the_successor_in_its_slot() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let lead = participant("lead-1", "%1", Some("lead"));
        let id = replied(&store, &lead).await;
        let request = row(&store, &id).await;
        assert_eq!(
            request
                .sender_follow
                .as_ref()
                .and_then(|f| f.slot.as_ref())
                .and_then(|s| s.alias.as_deref()),
            Some("lead")
        );
        // The stall: the sender's session was replaced (e.g. /clear) before
        // the reply came back. Its successor holds the same alias.
        let successor = participant("lead-2", "%1", Some("lead"));
        let scan = [successor.clone(), worker("reviewer", "%9")];
        assert_eq!(
            sender_whereabouts(&request, &scan, &[], &[]),
            SenderWhereabouts::Successor(Box::new(successor.clone()))
        );
        let rebound = store
            .rebind_sender(&id, &lead, successor.clone())
            .await
            .unwrap()
            .unwrap();
        assert!(rebound.from.same_session(&successor));
        assert_eq!(
            rebound.sender_follow.unwrap().rebound_from,
            vec!["lead-1".to_string()]
        );
        // The successor is now owed the wake and may read the reply.
        let batch = store
            .reserve_reply_wakes(&successor)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(batch.requests[0].id, id);
        assert!(store.get_for(&successor, &id).await.is_ok());
        // A second rebind from the old session is refused.
        assert!(store
            .rebind_sender(&id, &lead, participant("x", "%3", None))
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn a_present_sender_stays_and_ambiguous_or_unnamed_slots_are_gone() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let lead = participant("lead-1", "%1", Some("lead"));
        let id = replied(&store, &lead).await;
        let request = row(&store, &id).await;
        // Still in the scan (perhaps busy): not ours to move.
        assert_eq!(
            sender_whereabouts(&request, std::slice::from_ref(&lead), &[], &[]),
            SenderWhereabouts::Present
        );
        // Two holders of the alias: no choice is made.
        let scan = [
            participant("a", "%2", Some("lead")),
            participant("b", "%3", Some("lead")),
        ];
        assert_eq!(
            sender_whereabouts(&request, &scan, &[], &[]),
            SenderWhereabouts::Gone
        );
        // No alias or role to follow.
        let anonymous = participant("anon", "%4", None);
        let id = replied(&store, &anonymous).await;
        let request = row(&store, &id).await;
        assert!(request.sender_follow.is_none());
        assert_eq!(
            sender_whereabouts(&request, &[participant("z", "%5", None)], &[], &[]),
            SenderWhereabouts::Gone
        );
        // A new session that merely reuses the sender's pane inherits nothing.
        let in_place = participant("anon-2", "%4", None);
        assert_eq!(
            sender_whereabouts(&request, std::slice::from_ref(&in_place), &[], &[]),
            SenderWhereabouts::Gone
        );
        // A role-only successor is followed when the alias is not held.
        let mut lead = participant("lead-9", "%6", Some("old-alias"));
        lead.roles = vec!["implementer".into()];
        let id = replied(&store, &lead).await;
        let request = row(&store, &id).await;
        let mut successor = participant("lead-10", "%6", None);
        successor.roles = vec!["implementer".into()];
        assert_eq!(
            sender_whereabouts(&request, std::slice::from_ref(&successor), &[], &[]),
            SenderWhereabouts::Successor(Box::new(successor))
        );
    }

    #[tokio::test]
    async fn a_gone_sender_opens_one_human_item_after_the_grace_and_it_retires() {
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let lead = participant("lead-1", "%1", None);
        let id = replied(&store, &lead).await;
        assert!(
            store.report_gone_sender(&id).await.unwrap().is_none(),
            "within the grace"
        );
        {
            let mut requests = store.requests.write().await;
            let reply = requests.get_mut(&id).unwrap().reply.as_mut().unwrap();
            reply.at -= SENDER_GONE_GRACE;
        }
        let action = store.report_gone_sender(&id).await.unwrap().unwrap();
        assert!(action.needs_human_response());
        assert_eq!(action.parent_request_id.as_deref(), Some(id.as_str()));
        assert!(action.body.contains(&format!("tend msg show {id}")));
        assert_eq!(store.pending_human_notifications().await.len(), 1);
        assert!(
            store.report_gone_sender(&id).await.unwrap().is_none(),
            "only one item"
        );
        assert!(
            row(&store, &id).await.reply_wake_owed(),
            "the wake stays owed for a returning session"
        );
        // Someone reads the reply: the unanswered item is withdrawn.
        store
            .requests
            .write()
            .await
            .get_mut(&id)
            .unwrap()
            .mark_reply_read(OffsetDateTime::now_utc());
        store.retire_gone_sender_items().await.unwrap();
        assert_eq!(
            row(&store, &action.id).await.status,
            RequestStatus::Cancelled
        );
        assert!(store.pending_human_notifications().await.is_empty());
    }
}
