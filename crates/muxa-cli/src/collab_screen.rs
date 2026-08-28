//! The fleet-wide collaboration screen for `muxa watch`.
//!
//! `M` opens one agent's mailbox — whichever row the cursor is on. This screen
//! answers the question no per-row overlay can: what is the *whole fleet*
//! saying. Its rows are collaboration requests rather than topology nodes,
//! which is why it is a screen of its own rather than another `layout` over
//! the same node set.
//!
//! The [`WatchView`] axis carries over unchanged and means here what it means
//! everywhere else — how coarsely to group. A request is filed under the room
//! it was *raised* in, because that is the window an operator would go to in
//! order to ask about it.

use muxa::collaboration::{CollaborationRequest, Participant, RequestStatus};
use muxa::config::WatchView;
use ratatui::layout::Constraint;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Row};
use time::OffsetDateTime;

use crate::watch::WatchThemeSpec;

/// Longest body excerpt kept on a row. The full text is one `Enter` away in
/// the mailbox, so the list optimises for scanning many rows at once.
const BODY_EXCERPT: usize = 60;

#[derive(Debug, Default)]
pub(crate) struct CollabScreen {
    /// Newest first, exactly as the daemon returned them.
    requests: Vec<CollaborationRequest>,
    /// Index into the *visible* (filtered) requests, not into `requests`.
    selected: usize,
    /// Why the listing is missing, when it is. Kept separate from an empty
    /// listing: "the daemon refused" and "the fleet is quiet" are different
    /// answers and must not render the same.
    pub(crate) unavailable: Option<String>,
    /// A listing has been attempted. Distinguishes the first paint (before
    /// any fetch has returned) from a genuinely empty fleet.
    pub(crate) loaded: bool,
}

/// One rendered line: either a grouping header or a request.
#[derive(Debug, PartialEq)]
pub(crate) enum CollabRow<'a> {
    Group(String),
    Request(&'a CollaborationRequest),
}

impl CollabScreen {
    pub(crate) fn set_requests(&mut self, requests: Vec<CollaborationRequest>) {
        self.requests = requests;
        self.unavailable = None;
        self.loaded = true;
        self.clamp();
    }

    pub(crate) fn fail(&mut self, reason: String) {
        self.requests.clear();
        self.unavailable = Some(reason);
        self.loaded = true;
        self.selected = 0;
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.requests.is_empty()
    }

    pub(crate) fn selected_index(&self) -> usize {
        self.selected
    }

    /// The requests a filter leaves on screen, newest first.
    pub(crate) fn visible<'a>(&'a self, filter: &str) -> Vec<&'a CollaborationRequest> {
        let needle = filter.trim().to_lowercase();
        self.requests
            .iter()
            .filter(|request| needle.is_empty() || matches_filter(request, &needle))
            .collect()
    }

    pub(crate) fn selected_request(&self, filter: &str) -> Option<&CollaborationRequest> {
        self.visible(filter).get(self.selected).copied()
    }

    /// Rows to paint: the visible requests with a header inserted whenever the
    /// group changes. `WatchView::Pane` groups nothing — at pane granularity
    /// every row already names both of its endpoints.
    pub(crate) fn rows<'a>(&'a self, view: WatchView, filter: &str) -> Vec<CollabRow<'a>> {
        let mut rows = Vec::new();
        let mut current: Option<String> = None;
        for request in self.visible(filter) {
            if let Some(group) = group_of(request, view) {
                if current.as_ref() != Some(&group) {
                    rows.push(CollabRow::Group(group.clone()));
                    current = Some(group);
                }
            }
            rows.push(CollabRow::Request(request));
        }
        rows
    }

    /// Move the cursor within the filtered listing, saturating at both ends.
    ///
    /// Saturating rather than wrapping: this list is time-ordered, so running
    /// off the bottom and landing on the newest request would read as the list
    /// having jumped on its own.
    pub(crate) fn move_selection(&mut self, delta: isize, filter: &str) {
        let len = self.visible(filter).len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let last = len - 1;
        self.selected = if delta >= 0 {
            self.selected.saturating_add(delta.unsigned_abs()).min(last)
        } else {
            self.selected.saturating_sub(delta.unsigned_abs())
        };
    }

    pub(crate) fn select_first(&mut self) {
        self.selected = 0;
    }

    pub(crate) fn select_last(&mut self, filter: &str) {
        self.selected = self.visible(filter).len().saturating_sub(1);
    }

    /// Keep the cursor inside the listing after a refresh replaced it.
    fn clamp(&mut self) {
        let len = self.requests.len();
        self.selected = if len == 0 {
            0
        } else {
            self.selected.min(len - 1)
        };
    }
}

/// Where a participant sits, in the names an operator navigates by.
///
/// A room id alone (`@7` on some socket) locates nothing a human can act on;
/// the tmux session and window names do. Fall back to the ids only when the
/// scan that would have supplied the names has not run yet.
pub(crate) fn location(participant: &Participant) -> String {
    if participant.console {
        return "console".to_string();
    }
    let session = participant
        .tmux_session_name
        .as_deref()
        .or(participant.tmux_session_id.as_deref())
        .unwrap_or("?");
    let window = participant
        .window_name
        .as_deref()
        .unwrap_or(&participant.room.window_id);
    format!("{session}:{window}")
}

/// The group a request is filed under, or `None` when the view groups nothing.
fn group_of(request: &CollaborationRequest, view: WatchView) -> Option<String> {
    // The sender's room: an operator chasing a request goes to where it was
    // raised. Console-dispatched requests carry no room of their own, so they
    // file under the peer they were aimed at instead of a "console" bucket
    // that would collect every unrelated operator message into one heap.
    let anchor = if request.from.console {
        &request.to
    } else {
        &request.from
    };
    match view {
        WatchView::Session => Some(
            anchor
                .tmux_session_name
                .clone()
                .or_else(|| anchor.tmux_session_id.clone())
                .unwrap_or_else(|| "?".to_string()),
        ),
        WatchView::Window => Some(location(anchor)),
        WatchView::Pane => None,
    }
}

fn matches_filter(request: &CollaborationRequest, needle: &str) -> bool {
    let ends = [&request.from, &request.to];
    ends.iter().any(|p| {
        p.label().to_lowercase().contains(needle) || location(p).to_lowercase().contains(needle)
    }) || request.body.to_lowercase().contains(needle)
        || format!("{:?}", request.kind)
            .to_lowercase()
            .contains(needle)
        || format!("{:?}", request.status)
            .to_lowercase()
            .contains(needle)
}

/// Compact age, one unit wide enough to scan a column of them at a glance.
pub(crate) fn age(now: OffsetDateTime, at: OffsetDateTime) -> String {
    let secs = (now - at).whole_seconds().max(0);
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

fn excerpt(body: &str) -> String {
    let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= BODY_EXCERPT {
        return flat;
    }
    let cut: String = flat.chars().take(BODY_EXCERPT - 1).collect();
    format!("{cut}…")
}

pub(crate) const COLUMNS: [Constraint; 5] = [
    Constraint::Length(5),
    Constraint::Percentage(32),
    Constraint::Length(9),
    Constraint::Length(10),
    Constraint::Min(20),
];

pub(crate) const HEADERS: [&str; 5] = ["AGE", "FROM → TO", "KIND", "STATUS", "MESSAGE"];

/// Render one row. Group headers span the table as a single labelled line.
pub(crate) fn row<'a>(
    entry: &CollabRow<'a>,
    now: OffsetDateTime,
    theme: WatchThemeSpec,
    selected: bool,
) -> Row<'a> {
    match entry {
        CollabRow::Group(label) => Row::new(vec![
            Cell::from(""),
            Cell::from(Line::from(Span::styled(
                label.clone(),
                theme.table_header_style(),
            ))),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
        ]),
        CollabRow::Request(request) => {
            let route = format!(
                "{} [{}] → {} [{}]",
                request.from.label(),
                location(&request.from),
                request.to.label(),
                location(&request.to),
            );
            let cells = vec![
                Cell::from(age(now, request.created_at)),
                Cell::from(route),
                Cell::from(kind_label(request)),
                Cell::from(status_label(request.status)),
                Cell::from(excerpt(&request.body)),
            ];
            let row = Row::new(cells);
            if selected {
                row.style(theme.selected_style())
            } else {
                row
            }
        }
    }
}

fn kind_label(request: &CollaborationRequest) -> String {
    format!("{:?}", request.kind).to_lowercase()
}

fn status_label(status: RequestStatus) -> String {
    format!("{status:?}").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use muxa::collaboration::{RequestKind, RoomId, WorkMode};
    use muxa::event::{AgentKind, AgentState};

    fn participant(pane: &str, session: &str, window: &str) -> Participant {
        Participant {
            agent_kind: AgentKind::ClaudeCode,
            agent_session_id: format!("session-{pane}"),
            pane: pane.into(),
            socket: Some("default".into()),
            room: RoomId {
                host: "tmux".into(),
                socket: Some("default".into()),
                window_id: format!("@{}", window.len()),
            },
            tmux_session_id: Some("$1".into()),
            tmux_session_name: Some(session.into()),
            window_name: Some(window.into()),
            state: AgentState::Idle,
            cwd: None,
            alias: None,
            roles: Vec::new(),
            console: false,
        }
    }

    fn request(id: &str, from: Participant, to: Participant, body: &str) -> CollaborationRequest {
        CollaborationRequest {
            id: id.into(),
            from,
            to,
            provenance: None,
            kind: RequestKind::Question,
            body: body.into(),
            expects_reply: true,
            work_mode: WorkMode::ReadOnly,
            paths: Vec::new(),
            air_artifacts: Vec::new(),
            status: RequestStatus::Queued,
            created_at: OffsetDateTime::now_utc(),
            claimed_at: None,
            wake_delivery: None,
            notified_at: None,
            reply_notified_at: None,
            reply_read_at: None,
            reply: None,
        }
    }

    fn screen() -> CollabScreen {
        let mut screen = CollabScreen::default();
        screen.set_requests(vec![
            request(
                "req_1",
                participant("%1", "callabo", "CAL-7330"),
                participant("%2", "callabo", "CAL-7330"),
                "review the auth change",
            ),
            request(
                "req_2",
                participant("%3", "callabo", "CAL-7331"),
                participant("%4", "callabo", "CAL-7331"),
                "upload chunking looks wrong",
            ),
            request(
                "req_3",
                participant("%5", "muxa", "watch"),
                participant("%6", "muxa", "watch"),
                "who owns the refresh path",
            ),
        ]);
        screen
    }

    #[test]
    fn window_view_files_each_request_under_the_room_it_was_raised_in() {
        let screen = screen();
        let rows = screen.rows(WatchView::Window, "");
        let groups: Vec<_> = rows
            .iter()
            .filter_map(|row| match row {
                CollabRow::Group(label) => Some(label.clone()),
                CollabRow::Request(_) => None,
            })
            .collect();
        assert_eq!(
            groups,
            vec!["callabo:CAL-7330", "callabo:CAL-7331", "muxa:watch"]
        );
        assert_eq!(rows.len(), 6, "three headers, three requests");
    }

    #[test]
    fn session_view_collapses_windows_into_one_header_each() {
        let screen = screen();
        let groups: Vec<_> = screen
            .rows(WatchView::Session, "")
            .into_iter()
            .filter_map(|row| match row {
                CollabRow::Group(label) => Some(label),
                CollabRow::Request(_) => None,
            })
            .collect();
        // Two callabo requests sit under one header; muxa opens the next.
        assert_eq!(groups, vec!["callabo", "muxa"]);
    }

    #[test]
    fn pane_view_groups_nothing_because_every_row_names_both_ends() {
        let screen = screen();
        let rows = screen.rows(WatchView::Pane, "");
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|row| matches!(row, CollabRow::Request(_))));
    }

    #[test]
    fn a_console_dispatch_files_under_the_peer_it_was_aimed_at() {
        // The console has no room of its own. Filing its traffic under a
        // "console" bucket would pile every unrelated operator message into
        // one heap, which is the opposite of what grouping is for.
        let mut screen = CollabScreen::default();
        let console = Participant::console(RoomId {
            host: "tmux".into(),
            socket: Some("default".into()),
            window_id: "@1".into(),
        });
        screen.set_requests(vec![request(
            "req_1",
            console,
            participant("%2", "callabo", "CAL-7330"),
            "ship it",
        )]);
        let groups: Vec<_> = screen
            .rows(WatchView::Window, "")
            .into_iter()
            .filter_map(|row| match row {
                CollabRow::Group(label) => Some(label),
                CollabRow::Request(_) => None,
            })
            .collect();
        assert_eq!(groups, vec!["callabo:CAL-7330"]);
    }

    #[test]
    fn the_filter_reaches_the_session_a_request_happened_in() {
        // Finding "everything CAL-7331 said" is the reason to widen the
        // listing in the first place, so the window name has to be matchable
        // and not just the message text.
        let screen = screen();
        assert_eq!(screen.visible("cal-7331").len(), 1);
        assert_eq!(screen.visible("muxa:watch").len(), 1);
        assert_eq!(screen.visible("chunking").len(), 1);
        assert_eq!(screen.visible("callabo").len(), 2);
        assert!(screen.visible("nothing here").is_empty());
    }

    #[test]
    fn selection_saturates_instead_of_wrapping() {
        let mut screen = screen();
        screen.move_selection(-1, "");
        assert_eq!(screen.selected_index(), 0, "already at the newest");
        screen.move_selection(99, "");
        assert_eq!(screen.selected_index(), 2, "stops at the oldest");
        assert_eq!(screen.selected_request("").unwrap().id, "req_3");
    }

    #[test]
    fn a_refresh_that_shrinks_the_listing_keeps_the_cursor_in_range() {
        let mut screen = screen();
        screen.select_last("");
        assert_eq!(screen.selected_index(), 2);
        screen.set_requests(vec![request(
            "req_9",
            participant("%1", "callabo", "CAL-7330"),
            participant("%2", "callabo", "CAL-7330"),
            "only one left",
        )]);
        assert_eq!(screen.selected_index(), 0);
        assert_eq!(screen.selected_request("").unwrap().id, "req_9");
    }

    #[test]
    fn a_refused_listing_is_not_an_empty_fleet() {
        let mut screen = screen();
        screen.fail("mailbox unavailable".into());
        assert!(screen.is_empty());
        assert_eq!(screen.unavailable.as_deref(), Some("mailbox unavailable"));
        assert!(screen.loaded);
    }

    #[test]
    fn ages_read_in_one_unit() {
        let now = OffsetDateTime::now_utc();
        assert_eq!(age(now, now), "0s");
        assert_eq!(age(now, now - time::Duration::seconds(90)), "1m");
        assert_eq!(age(now, now - time::Duration::hours(5)), "5h");
        assert_eq!(age(now, now - time::Duration::days(3)), "3d");
        // A clock that jumped backwards must not render a negative age.
        assert_eq!(age(now, now + time::Duration::minutes(5)), "0s");
    }

    #[test]
    fn a_long_body_is_cut_to_one_scannable_line() {
        let long = "word ".repeat(40);
        let cut = excerpt(&long);
        assert_eq!(cut.chars().count(), BODY_EXCERPT);
        assert!(cut.ends_with('…'));
        assert_eq!(excerpt("multi\n  line   body"), "multi line body");
    }
}
