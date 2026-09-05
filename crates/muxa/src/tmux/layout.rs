//! On-screen geometry of the client's current window.
//!
//! Everything else in this crate cares about *which* panes exist. `muxa
//! peek` and the watch window inspector also care *where they are*: peek
//! paints a borderless full-client `display-popup`, while watch projects an
//! arbitrary selected window into its inspector canvas.
//!
//! The geometry columns are deliberately kept out of [`super::PANE_FMT`].
//! That format runs on every reconciler tick, against every socket; peek
//! runs once per keypress, against one window. Widening the hot query to
//! serve the cold one would tax every tick for nothing.
//!
//! ## Targeting
//!
//! Every query here is scoped to an explicit [`WindowTarget`] resolved
//! once, when the overlay opens. An unscoped tmux command resolves
//! against the *current client*, which tmux defines as the most recently
//! active one — so with several terminals attached to the same server,
//! typing in another tab silently reroutes the next query to that
//! session, and the overlay repaints itself with somebody else's panes
//! mid-read. Pinning the window id at open makes the overlay describe the
//! window it was raised over for as long as it is up.
//!
//! ## Coordinate systems
//!
//! tmux reports `pane_left`/`pane_top` relative to the **window**, while a
//! popup is placed relative to the **client** (terminal). Those differ by
//! the status line: with `status-position top` the window starts below it.
//! [`WindowFrame::pane_origin_y`] resolves the offset, and it is the only
//! place that conversion should live.
//!
//! ## Whether an overlay can be drawn at all
//!
//! Geometry answers "where does the box go". [`ClientSurface`] answers the
//! question before it: whether the client showing the window can render a
//! `display-popup` in the first place. Not every tmux client can, and the
//! one that cannot fails silently — see that type.

use super::{command_output_with_timeout, tmux_command, TMUX_COMMAND_TIMEOUT};

/// `tmux -F` columns behind [`current_window_panes`]. Tab-separated,
/// parsed by [`parse_pane_geometry_lines`].
const PANE_GEOMETRY_FMT: &str = "#{pane_id}\t#{pane_index}\t#{pane_left}\t#{pane_top}\t#{pane_width}\t#{pane_height}\t#{pane_active}\t#{window_zoomed_flag}\t#{pane_current_command}\t#{@muxa_agent_alias}";

/// `tmux -F` columns behind [`current_window_frame`].
const FRAME_FMT: &str =
    "#{window_width}\t#{window_height}\t#{client_width}\t#{client_height}\t#{status-position}";

/// `tmux -F` column behind [`client_surface`]. Expands to the empty string
/// on a server with no attached client, which is what makes
/// [`ClientSurface::Unknown`] reachable.
const CONTROL_MODE_FMT: &str = "#{client_control_mode}";

/// `tmux -F` column behind [`server_config_files`].
const CONFIG_FILES_FMT: &str = "#{config_files}";

/// Where one pane sits on screen, plus the little bit of identity the
/// overlay needs to label it when no agent is attached.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PaneGeometry {
    pub pane_id: String,
    /// tmux's per-window pane index — the digit `display-panes` shows, and
    /// the digit peek accepts to jump.
    pub pane_index: String,
    pub left: u16,
    pub top: u16,
    pub width: u16,
    pub height: u16,
    pub active: bool,
    /// Foreground command (`zsh`, `claude`, …). The fallback label for a
    /// pane muxa has no agent row for.
    pub command: String,
    /// The pane's room-local handle (`claude`, `reviewer`), read straight
    /// off `@muxa_agent_alias`.
    ///
    /// This is the *slot's* name — what the launcher declared about the
    /// pane, which is also what muxa mints when nobody else did. An agent
    /// that later registers its own identity through the daemon overrides
    /// it for routing purposes without rewriting the option, so treat this
    /// as the durable label rather than as the last word on where a peer
    /// call lands.
    pub alias: Option<String>,
}

/// Client/window dimensions needed to place window-relative pane
/// coordinates inside a client-relative popup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowFrame {
    pub window_width: u16,
    pub window_height: u16,
    pub client_width: u16,
    pub client_height: u16,
    /// `status-position top`. When set, the window is pushed down by the
    /// status line(s) and pane coordinates need the same shift.
    pub status_top: bool,
}

impl WindowFrame {
    /// Row within the client where window row 0 lands.
    ///
    /// With the status line at the bottom (tmux's default) the window
    /// starts at the top of the client, so the offset is zero. With it at
    /// the top, the offset is however many rows the status line takes —
    /// derived from the client/window height difference rather than
    /// assumed to be 1, because `set -g status 2` is legal.
    pub fn pane_origin_y(&self) -> u16 {
        if self.status_top {
            self.client_height.saturating_sub(self.window_height)
        } else {
            0
        }
    }
}

/// Parse the tab-separated stdout of a [`PANE_GEOMETRY_FMT`] query.
///
/// Returns `(panes, zoomed)`. Malformed rows are skipped rather than
/// failing the batch, matching the best-effort posture of the rest of this
/// module — a half-readable overlay beats no overlay.
///
/// When a pane is zoomed, tmux keeps reporting the *unzoomed* geometry for
/// its siblings, which would paint boxes over screen the zoomed pane now
/// owns. The `zoomed` flag lets the caller drop everything but the active
/// pane; this parser reports the flag and leaves that policy to peek.
pub fn parse_pane_geometry_lines(stdout: &str) -> (Vec<PaneGeometry>, bool) {
    let mut panes = Vec::new();
    let mut zoomed = false;
    for line in stdout.lines() {
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 8 {
            continue;
        }
        let (Ok(left), Ok(top), Ok(width), Ok(height)) = (
            cols[2].trim().parse::<u16>(),
            cols[3].trim().parse::<u16>(),
            cols[4].trim().parse::<u16>(),
            cols[5].trim().parse::<u16>(),
        ) else {
            continue;
        };
        if width == 0 || height == 0 {
            continue;
        }
        if is_flag_set(cols[7]) {
            zoomed = true;
        }
        panes.push(PaneGeometry {
            pane_id: cols[0].into(),
            pane_index: cols[1].into(),
            left,
            top,
            width,
            height,
            active: is_flag_set(cols[6]),
            command: cols.get(8).map(|s| (*s).to_string()).unwrap_or_default(),
            alias: cols
                .get(9)
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(ToString::to_string),
        });
    }
    (panes, zoomed)
}

fn is_flag_set(raw: &str) -> bool {
    matches!(raw.trim(), "1" | "true")
}

/// Parse the tab-separated stdout of a [`FRAME_FMT`] query.
pub fn parse_window_frame_line(stdout: &str) -> Option<WindowFrame> {
    let line = stdout.lines().next()?;
    let cols: Vec<&str> = line.split('\t').collect();
    if cols.len() < 4 {
        return None;
    }
    Some(WindowFrame {
        window_width: cols[0].trim().parse().ok()?,
        window_height: cols[1].trim().parse().ok()?,
        client_width: cols[2].trim().parse().ok()?,
        client_height: cols[3].trim().parse().ok()?,
        status_top: cols.get(4).map(|s| s.trim()) == Some("top"),
    })
}

/// The window an overlay is describing, resolved once at open time.
///
/// Both ids come from `$TMUX`, which names the session whose client
/// triggered the popup. Holding them means later queries never have to
/// ask tmux "which client is current?" — an answer that changes the
/// moment the user touches another terminal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowTarget {
    /// tmux session target (`$0`), used for client-scoped queries.
    pub session: Option<String>,
    /// tmux window id (`@0`), used for window-scoped queries. Pinning the
    /// *window* rather than the session additionally survives another
    /// client switching that session's current window while we're up.
    pub window: Option<String>,
}

impl WindowTarget {
    /// Resolve from `$TMUX`. Both fields fall back to `None` outside tmux
    /// or on a malformed env, where queries go unscoped — no worse than
    /// having never pinned anything.
    pub fn resolve() -> Self {
        let session = std::env::var("TMUX")
            .ok()
            .and_then(|raw| super::parse_tmux_session_target(&raw));
        let window = session.as_deref().and_then(window_id_for);
        Self { session, window }
    }
}

fn window_id_for(session: &str) -> Option<String> {
    let mut cmd = tmux_command();
    cmd.args(["display-message", "-p", "-t", session, "-F", "#{window_id}"]);
    let out = command_output_with_timeout(
        cmd,
        TMUX_COMMAND_TIMEOUT,
        "tmux display-message (window id)".into(),
    )
    .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8(out.stdout).ok()?;
    let id = stdout.lines().next()?.trim();
    (!id.is_empty()).then(|| id.to_string())
}

/// Panes of the target window, with on-screen geometry.
///
/// Note that `$TMUX_PANE` inside a popup names the *popup's own* pane, so
/// callers must take "which pane is focused" from
/// [`PaneGeometry::active`] rather than from [`super::current_pane`].
///
/// Returns `(panes, zoomed)`; empty when tmux is unavailable or errors.
pub fn current_window_panes(target: &WindowTarget) -> (Vec<PaneGeometry>, bool) {
    let mut cmd = tmux_command();
    cmd.args(["list-panes", "-F", PANE_GEOMETRY_FMT]);
    if let Some(window) = &target.window {
        cmd.args(["-t", window]);
    }
    let Ok(out) = command_output_with_timeout(
        cmd,
        TMUX_COMMAND_TIMEOUT,
        "tmux list-panes (geometry)".into(),
    ) else {
        return (Vec::new(), false);
    };
    if !out.status.success() {
        return (Vec::new(), false);
    }
    match String::from_utf8(out.stdout) {
        Ok(stdout) => parse_pane_geometry_lines(&stdout),
        Err(_) => (Vec::new(), false),
    }
}

/// Panes of an explicitly identified window on an explicitly identified tmux
/// server.
///
/// Unlike [`current_window_panes`], this does not consult the popup's `$TMUX`
/// context. It is intended for global surfaces such as `muxa watch`, where the
/// selected window can belong to a different session or tmux socket than the
/// client that opened the UI. A supplied socket that no longer exists fails
/// closed through [`super::tmux_command_on`].
pub fn window_panes_on(socket: Option<&str>, window_id: &str) -> (Vec<PaneGeometry>, bool) {
    let mut cmd = super::tmux_command_on(socket);
    cmd.args(["list-panes", "-F", PANE_GEOMETRY_FMT, "-t", window_id]);
    let Ok(out) = command_output_with_timeout(
        cmd,
        TMUX_COMMAND_TIMEOUT,
        format!("tmux list-panes (geometry) -t {window_id}"),
    ) else {
        return (Vec::new(), false);
    };
    if !out.status.success() {
        return (Vec::new(), false);
    }
    match String::from_utf8(out.stdout) {
        Ok(stdout) => parse_pane_geometry_lines(&stdout),
        Err(_) => (Vec::new(), false),
    }
}

/// The pane's currently visible text, with no escape sequences.
///
/// Deliberately *not* [`super::capture_pane`], which passes `-e` to keep
/// the pane's colors. The overlay paints this as a uniformly dimmed
/// backdrop behind its own boxes — the pane's real colors bleeding
/// through would compete with the foreground it exists to set off, so we
/// ask tmux for plain text and style it ourselves.
///
/// Returns `None` when the pane is gone or tmux errors; the caller draws
/// an empty backdrop rather than failing the frame.
pub fn capture_pane_plain(pane_id: &str) -> Option<String> {
    let mut cmd = tmux_command();
    cmd.args(["capture-pane", "-p", "-t", pane_id]);
    let out = command_output_with_timeout(
        cmd,
        TMUX_COMMAND_TIMEOUT,
        format!("tmux capture-pane -t {pane_id}"),
    )
    .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// Short name of the tmux server this process is talking to, read from
/// `$TMUX` (`<socket_path>,<server_pid>,<session_id>`).
///
/// Pane ids are unique only per server, so consumers that match agent rows
/// by pane id use this to reject a same-id row recorded against a
/// different server. `None` outside tmux, or when `$TMUX` is malformed —
/// callers must treat that as "unknown", never as "no match".
pub fn current_socket_name() -> Option<String> {
    socket_name_from_tmux_env(&std::env::var("TMUX").ok()?)
}

/// Pure half of [`current_socket_name`], split out so the parsing is
/// testable without mutating process-wide environment.
fn socket_name_from_tmux_env(raw: &str) -> Option<String> {
    let path = raw.split(',').next().filter(|p| !p.is_empty())?;
    Some(super::socket_short_name(path))
}

/// Dimensions of the target window and the client showing it. `None`
/// when tmux is unavailable or the response can't be parsed.
///
/// Scoped to the *session* rather than the window: the reading includes
/// client dimensions, and a bare window id doesn't tell tmux which
/// client's geometry to report.
pub fn current_window_frame(target: &WindowTarget) -> Option<WindowFrame> {
    let mut cmd = tmux_command();
    cmd.args(["display-message", "-p", "-F", FRAME_FMT]);
    if let Some(session) = &target.session {
        cmd.args(["-t", session]);
    }
    let out = command_output_with_timeout(
        cmd,
        TMUX_COMMAND_TIMEOUT,
        "tmux display-message (frame)".into(),
    )
    .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_window_frame_line(&String::from_utf8(out.stdout).ok()?)
}

/// What the client showing the target window is able to draw.
///
/// tmux has two kinds of client. A *terminal* client owns a tty and tmux
/// paints everything onto it — panes, the status line, and the
/// client-drawn overlays (`display-popup`, `display-menu`,
/// `display-panes`, copy-mode). A *control-mode* client (`tmux -CC`) owns
/// no tty: tmux streams pane content to it as `%output` notifications and
/// the front-end draws the panes itself, natively. Overlays have no such
/// notification, so tmux never mentions them to that client at all.
///
/// The silence is the trap. `display-popup -E` raised on a control-mode
/// client still *runs* its command and still exits 0 — the program starts,
/// attaches to a pane nobody renders, and waits for input that can never
/// arrive. So a caller whose entire output reaches the user through an
/// overlay has to ask this first and say plainly that it cannot draw,
/// rather than starting and appearing to hang.
///
/// This is the current client specifically, not a survey of the server:
/// with both a terminal and a control-mode client attached, the popup
/// lands on whichever one raised it. [`super::list_clients`] is the
/// server-wide counterpart, used for activity tracking rather than for
/// deciding whether to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientSurface {
    /// A terminal client. Overlays draw normally.
    Terminal,
    /// A control-mode client — amux/cmux's native window mirroring,
    /// iTerm2's tmux integration, any `tmux -CC` consumer. Overlays are
    /// silently dropped.
    ControlMode,
    /// Nothing conclusive could be read: no server, no attached client, or
    /// a tmux old enough that `#{client_control_mode}` expands to the empty
    /// string instead of erroring.
    ///
    /// Callers must read this as "assume overlays work". Refusing to draw
    /// on an unreadable answer would break the overlay everywhere the
    /// probe is merely inconclusive, which is a far larger population than
    /// the control-mode clients it exists to catch.
    Unknown,
}

impl ClientSurface {
    /// Whether a `display-popup` raised on this client would reach a human.
    /// [`Self::Unknown`] counts as yes, per that variant's contract.
    #[must_use]
    pub fn draws_overlays(self) -> bool {
        !matches!(self, Self::ControlMode)
    }
}

/// Read the [`ClientSurface`] of the client showing `target`.
///
/// Scoped to the session for the same reason as [`current_window_frame`]:
/// the reading is a property of the client, and a bare window id does not
/// tell tmux which client to report on.
pub fn client_surface(target: &WindowTarget) -> ClientSurface {
    let mut cmd = tmux_command();
    cmd.args(["display-message", "-p", "-F", CONTROL_MODE_FMT]);
    if let Some(session) = &target.session {
        cmd.args(["-t", session]);
    }
    let Ok(out) = command_output_with_timeout(
        cmd,
        TMUX_COMMAND_TIMEOUT,
        "tmux display-message (client control mode)".into(),
    ) else {
        return ClientSurface::Unknown;
    };
    if !out.status.success() {
        return ClientSurface::Unknown;
    }
    String::from_utf8(out.stdout).map_or(ClientSurface::Unknown, |stdout| {
        parse_client_surface(&stdout)
    })
}

/// Pure half of [`client_surface`].
fn parse_client_surface(raw: &str) -> ClientSurface {
    match raw.lines().next().map(str::trim) {
        Some("1") => ClientSurface::ControlMode,
        Some("0") => ClientSurface::Terminal,
        _ => ClientSurface::Unknown,
    }
}

/// Config files the running tmux server loaded at startup
/// (`#{config_files}`), in tmux's own order.
///
/// muxa installs its bindings by writing `~/.tmux.conf`, which is worth
/// nothing on a server started with `-f` pointed somewhere else. Front-ends
/// that drive a private tmux server do exactly that — amux starts its
/// engine as `tmux -f /dev/null -L amux` specifically so the user's
/// `~/.tmux.conf` (and any session-restoring plugin in it) stays out of the
/// server it owns. Reading the list is the only way to tell that apart from
/// a server that simply has not re-read the file yet, and the two want
/// opposite advice.
///
/// Empty when tmux is unavailable, when no server is running, or on a tmux
/// too old to know the format — never confuse that with "loaded nothing".
pub fn server_config_files() -> Vec<String> {
    let mut cmd = tmux_command();
    cmd.args(["display-message", "-p", "-F", CONFIG_FILES_FMT]);
    let Ok(out) = command_output_with_timeout(
        cmd,
        TMUX_COMMAND_TIMEOUT,
        "tmux display-message (config files)".into(),
    ) else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8(out.stdout).map_or_else(|_| Vec::new(), |s| parse_config_files(&s))
}

/// Pure half of [`server_config_files`]. tmux prints one comma-separated
/// line.
fn parse_config_files(raw: &str) -> Vec<String> {
    raw.lines()
        .next()
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(ToString::to_string)
        .collect()
}

/// Whether the server loaded no configuration a user could have put muxa's
/// bindings in.
///
/// `-f /dev/null` is the idiom for an isolated server, and tmux reports it
/// literally. A server that read nothing at all is *not* isolated — that is
/// an unknown reading (see [`server_config_files`]), and saying "isolated"
/// there would send a user chasing a problem they do not have.
#[must_use]
pub fn config_isolated(config_files: &[String]) -> bool {
    !config_files.is_empty() && config_files.iter().all(|path| path == "/dev/null")
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO_PANE: &str = "%0\t0\t0\t0\t120\t19\t0\t0\tzsh\n%1\t1\t0\t20\t120\t19\t1\t0\tclaude\n";

    #[test]
    fn parses_vertical_split() {
        let (panes, zoomed) = parse_pane_geometry_lines(TWO_PANE);
        assert!(!zoomed);
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].pane_id, "%0");
        assert_eq!(panes[0].pane_index, "0");
        assert_eq!((panes[0].left, panes[0].top), (0, 0));
        assert_eq!((panes[0].width, panes[0].height), (120, 19));
        assert!(!panes[0].active);
        assert_eq!(panes[0].command, "zsh");
        // The divider row between the two panes is tmux's, not a pane's:
        // pane 0 ends at row 18, pane 1 starts at row 20.
        assert_eq!(panes[1].top, 20);
        assert!(panes[1].active);
    }

    #[test]
    fn reports_zoom_from_any_row() {
        // tmux stamps the window-level zoom flag on every row, and leaves
        // the *unzoomed* geometry on the siblings — which is why peek must
        // know the window is zoomed rather than trusting the rectangles.
        let raw = "%0\t0\t0\t0\t120\t19\t0\t1\tzsh\n%1\t1\t0\t0\t120\t39\t1\t1\tclaude\n";
        let (panes, zoomed) = parse_pane_geometry_lines(raw);
        assert!(zoomed);
        assert_eq!(panes.len(), 2);
        // Overlapping rectangles are exactly the hazard the flag guards.
        assert_eq!(panes[0].top, panes[1].top);
    }

    #[test]
    fn skips_malformed_and_degenerate_rows() {
        let raw = concat!(
            "%0\t0\tnot-a-number\t0\t120\t19\t0\t0\tzsh\n",
            "too\tfew\tcols\n",
            "\n",
            "%2\t2\t0\t0\t0\t19\t0\t0\tzsh\n",
            "%3\t3\t0\t0\t80\t24\t1\t0\tclaude\n",
        );
        let (panes, _) = parse_pane_geometry_lines(raw);
        assert_eq!(
            panes.len(),
            1,
            "only the well-formed, non-empty row survives"
        );
        assert_eq!(panes[0].pane_id, "%3");
    }

    #[test]
    fn tolerates_missing_command_column() {
        let (panes, _) = parse_pane_geometry_lines("%0\t0\t0\t0\t80\t24\t1\t0");
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].command, "");
    }

    #[test]
    fn status_at_bottom_needs_no_offset() {
        let frame = parse_window_frame_line("120\t39\t120\t40\tbottom").unwrap();
        assert_eq!(frame.window_height, 39);
        assert!(!frame.status_top);
        assert_eq!(frame.pane_origin_y(), 0);
    }

    #[test]
    fn status_at_top_shifts_panes_down() {
        let frame = parse_window_frame_line("120\t39\t120\t40\ttop").unwrap();
        assert!(frame.status_top);
        assert_eq!(frame.pane_origin_y(), 1);

        // `set -g status 2` — two status rows, so the shift is two.
        let tall = parse_window_frame_line("120\t38\t120\t40\ttop").unwrap();
        assert_eq!(tall.pane_origin_y(), 2);
    }

    #[test]
    fn frame_offset_never_underflows() {
        // A torn read where the window looks taller than the client must
        // not wrap around to 65535 and paint the overlay off-screen.
        let frame = parse_window_frame_line("120\t40\t120\t39\ttop").unwrap();
        assert_eq!(frame.pane_origin_y(), 0);
    }

    #[test]
    fn socket_name_comes_from_the_tmux_env_path() {
        // `$TMUX` is `<socket_path>,<server_pid>,<session_id>`; only the
        // socket's basename identifies the server.
        assert_eq!(
            socket_name_from_tmux_env("/tmp/tmux-1044/amux,32037,30").as_deref(),
            Some("amux")
        );
        assert_eq!(
            socket_name_from_tmux_env("/private/tmp/tmux-501/default,900,3").as_deref(),
            Some("default")
        );
        // A malformed value means "unknown" — callers must not read that
        // as "no match".
        assert!(socket_name_from_tmux_env("").is_none());
        assert!(socket_name_from_tmux_env(",32037,30").is_none());
    }

    #[test]
    fn control_mode_clients_are_told_apart_from_terminal_ones() {
        // `tmux -CC` (amux/cmux, iTerm2) — overlays are dropped silently,
        // so callers must refuse to draw rather than start invisibly.
        assert_eq!(parse_client_surface("1\n"), ClientSurface::ControlMode);
        assert!(!ClientSurface::ControlMode.draws_overlays());

        assert_eq!(parse_client_surface("0\n"), ClientSurface::Terminal);
        assert!(ClientSurface::Terminal.draws_overlays());
    }

    #[test]
    fn an_unreadable_control_mode_answer_still_draws() {
        // A detached server expands the format to nothing, and a tmux that
        // predates it does the same. Neither is evidence of control mode,
        // and treating it as such would disable the overlay for everyone
        // on an older tmux.
        for raw in ["", "\n", "unexpected"] {
            assert_eq!(
                parse_client_surface(raw),
                ClientSurface::Unknown,
                "{raw:?} is not evidence either way"
            );
        }
        assert!(ClientSurface::Unknown.draws_overlays());
    }

    #[test]
    fn config_files_split_on_commas() {
        assert_eq!(
            parse_config_files("/etc/tmux.conf,/Users/x/.tmux.conf\n"),
            vec!["/etc/tmux.conf", "/Users/x/.tmux.conf"]
        );
        assert_eq!(parse_config_files("/dev/null\n"), vec!["/dev/null"]);
        assert!(parse_config_files("").is_empty());
    }

    #[test]
    fn only_a_dev_null_server_counts_as_isolated() {
        assert!(config_isolated(&["/dev/null".into()]));
        // A real config was read, so `~/.tmux.conf` edits can reach this
        // server — whatever else is wrong, isolation is not it.
        assert!(!config_isolated(&["/Users/x/.tmux.conf".into()]));
        assert!(!config_isolated(&[
            "/dev/null".into(),
            "/Users/x/.tmux.conf".into()
        ]));
        // Unknown reading (no server, no tmux, tmux too old) — not a claim
        // that the server loaded nothing.
        assert!(!config_isolated(&[]));
    }

    #[test]
    fn frame_rejects_short_and_unparsable_lines() {
        assert!(parse_window_frame_line("").is_none());
        assert!(parse_window_frame_line("120\t39\t120").is_none());
        assert!(parse_window_frame_line("120\tx\t120\t40\ttop").is_none());
    }
}
