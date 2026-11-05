use super::{
    char_to_byte_idx, CollaborationOrigin, CollaborationRequest, MessageSkillPalette, PaneKey,
    Participant, RequestKind, RequestStatus, Result, RoomContext, SessionKey,
    WatchCollaborationMode, WindowKey,
};

#[derive(Debug, Default)]
pub(super) struct WatchHistoryCache {
    pub(super) changes: Option<tokio::sync::watch::Receiver<Option<u64>>>,
    pub(super) revision: Option<u64>,
    pub(super) previews: Vec<CollaborationRequest>,
    /// Full history is needed only while the operator explicitly searches bodies.
    pub(super) search_full: Option<Vec<CollaborationRequest>>,
    pub(super) detail: Option<CollaborationRequest>,
    pub(super) detail_failure: Option<(String, std::time::Instant)>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct WatchCollaboration {
    pub(super) human: Vec<CollaborationRequest>,
    pub(super) origin: Option<CollaborationOrigin>,
    pub(super) room: Option<RoomContext>,
    /// The agent whose inbox `incoming` holds — the row under the cursor when
    /// the mailbox was last refreshed. `None` when that row hosts no agent.
    pub(super) inbox: Option<MailboxAnchor>,
    /// Topology level whose history the overlay is presenting. Pane scopes
    /// retain claim/reply actions; window and session scopes are aggregate,
    /// read-only history assembled from the console-wide durable log.
    pub(super) history_scope: Option<MailboxHistoryScope>,
    pub(super) incoming: Vec<CollaborationRequest>,
    pub(super) sent: Vec<CollaborationRequest>,
    pub(super) unavailable: Option<String>,
}

/// The agent the mailbox overlay is acting as.
///
/// Claiming and replying are things a *recipient* does, so the `i` and `e`
/// keys speak for this agent rather than for the console — the console is only
/// ever a sender. Keeping it beside the fetched requests guarantees the keys
/// act on the same mailbox the panel is showing.
#[derive(Debug, Clone)]
pub(super) struct MailboxAnchor {
    pub(super) origin: CollaborationOrigin,
    pub(super) label: String,
}

#[derive(Debug, Clone)]
pub(super) enum MailboxHistoryScope {
    Pane(MailboxAnchor),
    Window { key: WindowKey, label: String },
    Session { key: SessionKey, label: String },
}

impl MailboxHistoryScope {
    pub(super) fn label(&self) -> &str {
        match self {
            Self::Pane(anchor) => &anchor.label,
            Self::Window { label, .. } | Self::Session { label, .. } => label,
        }
    }

    pub(super) fn aggregate(&self) -> bool {
        matches!(self, Self::Window { .. } | Self::Session { .. })
    }

    pub(super) fn matches(&self, participant: &Participant) -> bool {
        if participant.console {
            return false;
        }
        match self {
            Self::Pane(anchor) => {
                participant.pane == anchor.origin.pane
                    && anchor.origin.socket.as_deref().is_none_or(|socket| {
                        participant.socket.as_deref() == Some(socket)
                            || participant.room.socket.as_deref() == Some(socket)
                    })
                    // An anchor pinned to one agent session shows only that
                    // session's history, not a predecessor's in the same pane.
                    && anchor
                        .origin
                        .agent_session_id
                        .as_deref()
                        .is_none_or(|session| participant.agent_session_id == session)
            }
            Self::Window { key, .. } => {
                participant_matches_session(participant, &key.session)
                    && participant.room.window_id == key.window_id
            }
            Self::Session { key, .. } => participant_matches_session(participant, key),
        }
    }
}

fn participant_matches_session(participant: &Participant, key: &SessionKey) -> bool {
    participant.tmux_session_id.as_deref() == Some(key.session_id.as_str())
        && participant.room.host == key.endpoint.host.to_string()
        && participant
            .socket
            .as_deref()
            .or(participant.room.socket.as_deref())
            == Some(key.endpoint.socket.as_str())
}

pub(super) fn history_for_scope(
    requests: &[CollaborationRequest],
    scope: &MailboxHistoryScope,
) -> Vec<CollaborationRequest> {
    let aggregate = scope.aggregate();
    requests
        .iter()
        .filter(|request| scope.matches(&request.to) || (aggregate && scope.matches(&request.from)))
        .cloned()
        .collect()
}

impl WatchCollaboration {
    pub(super) fn peer_for_pane(&self, pane: &str) -> Option<&Participant> {
        self.room
            .as_ref()?
            .peers
            .iter()
            .find(|participant| participant.pane == pane)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum CollaborationMailboxTab {
    #[default]
    Human,
    Incoming,
    Sent,
}

#[derive(Debug, Clone, Default)]
pub(super) struct CollaborationMailboxState {
    pub(super) open: bool,
    pub(super) tab: CollaborationMailboxTab,
    pub(super) selected: usize,
    /// Extra rows for the selected-request detail, cycled with `|`:
    /// compact → half → most of the popup. Long request bodies are the
    /// point of opening the mailbox at all.
    pub(super) detail_expand: MailboxDetail,
}

/// Detail-pane sizing steps inside the collaboration mailbox.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum MailboxDetail {
    #[default]
    Compact,
    Half,
    Most,
}

impl MailboxDetail {
    pub(super) fn next(self) -> Self {
        match self {
            Self::Compact => Self::Half,
            Self::Half => Self::Most,
            Self::Most => Self::Compact,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Half => "half",
            Self::Most => "expanded",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CollaborationComposeTarget {
    Send {
        origin: CollaborationOrigin,
        /// Raw pane id of the recipient. `target` is the wire form
        /// (`pane:%N`); this copy exists so `just send` mode can type
        /// into the pane without re-parsing its own address.
        pane: String,
        /// Exact pane identity when the recipient came from the topology
        /// tree or endpoint-rich collaboration metadata.
        pane_key: Option<PaneKey>,
        target: String,
        kind: RequestKind,
        mode: ComposeSendMode,
    },
    Reply {
        origin: CollaborationOrigin,
        request_id: String,
        status: RequestStatus,
    },
    /// One body, several recipients — each delivered as its own ordinary
    /// request. Not a daemon-side broadcast: per-recipient authorization,
    /// provenance, wake gating and reply threading only survive if every
    /// recipient gets a request of its own.
    Broadcast {
        origin: CollaborationOrigin,
        /// `(pane, label)` per recipient, resolved and frozen when the
        /// composer opened. A refresh mid-compose cannot add or drop one.
        recipients: Vec<(String, String)>,
        kind: RequestKind,
        mode: ComposeSendMode,
    },
    /// Keystrokes-only composer for a pane that cannot receive a request —
    /// collaboration disabled, no room, no peer, or a row outside this
    /// window. `m` still owes the user a way to type at the agent they are
    /// pointing at; what it cannot do is dress those keystrokes up as a
    /// contract, so `Tab`/`Ctrl-E` explain instead of cycling.
    Prompt { pane: String },
    /// Endpoint-pinned keystrokes composer opened from a topology node.
    PromptTopology { pane: PaneKey },
    /// One prompt typed into several exact panes: every live agent of the
    /// Work selected on the board, frozen when the composer opened. Always
    /// keystrokes: it never becomes a collaboration request.
    PromptPanes {
        panes: Vec<PaneKey>,
        unavailable: Vec<String>,
    },
}

/// What Ctrl-E cycles: how the composed text leaves the composer.
///
/// The first two are the wire `WorkMode` contract on a durable request.
/// `JustSend` is watch-local — plain keystrokes typed into the pane, no
/// request, no reply, no contract. It lives here and not in `WorkMode`
/// because that enum is wire format (PROTOCOL.md, the MCP schema,
/// `collaboration.json`), and a variant that by construction never
/// appears on the wire would burden every consumer with a case that
/// cannot happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ComposeSendMode {
    ReadOnly,
    Execute,
    JustSend,
}

impl From<WatchCollaborationMode> for ComposeSendMode {
    fn from(mode: WatchCollaborationMode) -> Self {
        match mode {
            WatchCollaborationMode::ReadOnly => Self::ReadOnly,
            WatchCollaborationMode::Execute => Self::Execute,
            WatchCollaborationMode::JustSend => Self::JustSend,
        }
    }
}

impl From<ComposeSendMode> for WatchCollaborationMode {
    fn from(mode: ComposeSendMode) -> Self {
        match mode {
            ComposeSendMode::ReadOnly => Self::ReadOnly,
            ComposeSendMode::Execute => Self::Execute,
            ComposeSendMode::JustSend => Self::JustSend,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CollaborationComposeDefaults {
    pub(super) kind: RequestKind,
    pub(super) mode: ComposeSendMode,
}

impl Default for CollaborationComposeDefaults {
    fn default() -> Self {
        Self {
            kind: RequestKind::Question,
            mode: ComposeSendMode::ReadOnly,
        }
    }
}

/// One marked recipient, addressed the way a single send already addresses
/// one: by pane. The daemon resolves `pane:%N` itself and refuses what it
/// cannot route, so marking works the same for a room peer and for the
/// host-scope target `m` resolves — one path, not two.
///
/// `agent_session_id` is carried for pruning rather than addressing: a mark
/// whose pane is now occupied by a different agent session is dropped instead
/// of quietly addressing the newcomer.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct CollaborationMark {
    pub(super) pane: String,
    pub(super) agent_session_id: Option<String>,
}

/// What a broadcast did, one row per recipient.
///
/// Held rather than summarised: "sent to 5" is the shape of report that let a
/// tap sit a version behind and an empty reply close a request this week. A
/// recipient that failed stays on screen with its reason until dismissed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BroadcastReport {
    pub(super) rows: Vec<BroadcastRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BroadcastRow {
    /// Which pane this row is about. Carried so a delivered recipient can
    /// lose its mark while a failed one keeps it — retrying a partial
    /// failure should not mean re-marking the ones that already went.
    pub(super) pane: String,
    pub(super) label: String,
    pub(super) outcome: Result<String, String>,
}

impl BroadcastReport {
    pub(super) fn delivered(&self) -> usize {
        self.rows.iter().filter(|row| row.outcome.is_ok()).count()
    }

    pub(super) fn failed(&self) -> usize {
        self.rows.len() - self.delivered()
    }

    pub(super) fn title(&self) -> String {
        let failed = self.failed();
        if failed == 0 {
            format!("broadcast · {} delivered", self.delivered())
        } else {
            format!(
                "broadcast · {} delivered, {failed} failed",
                self.delivered()
            )
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CollaborationComposer {
    pub(super) target: CollaborationComposeTarget,
    pub(super) label: String,
    pub(super) input: String,
    pub(super) cursor: usize,
    /// `/` owns input while this is set. Choosing a skill inserts it at the
    /// saved composer cursor; it never sends without a second, deliberate
    /// Enter.
    pub(super) skill_palette: Option<MessageSkillPalette>,
}

impl CollaborationComposer {
    pub(super) fn new(target: CollaborationComposeTarget, label: String) -> Self {
        Self {
            target,
            label,
            input: String::new(),
            cursor: 0,
            skill_palette: None,
        }
    }

    pub(super) fn insert(&mut self, c: char) {
        let idx = char_to_byte_idx(&self.input, self.cursor);
        self.input.insert(idx, c);
        self.cursor += 1;
    }

    pub(super) fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = char_to_byte_idx(&self.input, self.cursor - 1);
        let end = char_to_byte_idx(&self.input, self.cursor);
        self.input.replace_range(start..end, "");
        self.cursor -= 1;
    }

    pub(super) fn delete(&mut self) {
        if self.cursor >= self.input.chars().count() {
            return;
        }
        let start = char_to_byte_idx(&self.input, self.cursor);
        let end = char_to_byte_idx(&self.input, self.cursor + 1);
        self.input.replace_range(start..end, "");
    }

    pub(super) fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub(super) fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.input.chars().count());
    }

    pub(super) fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub(super) fn move_end(&mut self) {
        self.cursor = self.input.chars().count();
    }
}
