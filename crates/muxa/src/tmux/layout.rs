//! On-screen geometry of the client's current window.
//!
//! Everything else in this crate cares about *which* panes exist. `muxa
//! peek` is the one consumer that cares *where they are*: it paints a
//! borderless full-client `display-popup` and redraws each pane's box at
//! the pane's own coordinates, so the overlay lines up with what the user
//! is looking at.
//!
//! The geometry columns are deliberately kept out of [`super::PANE_FMT`].
//! That format runs on every reconciler tick, against every socket; peek
//! runs once per keypress, against one window. Widening the hot query to
//! serve the cold one would tax every tick for nothing.
//!
//! ## Coordinate systems
//!
//! tmux reports `pane_left`/`pane_top` relative to the **window**, while a
//! popup is placed relative to the **client** (terminal). Those differ by
//! the status line: with `status-position top` the window starts below it.
//! [`WindowFrame::pane_origin_y`] resolves the offset, and it is the only
//! place that conversion should live.

use super::{command_output_with_timeout, tmux_command, TMUX_COMMAND_TIMEOUT};

/// `tmux -F` columns behind [`current_window_panes`]. Tab-separated,
/// parsed by [`parse_pane_geometry_lines`].
const PANE_GEOMETRY_FMT: &str = "#{pane_id}\t#{pane_index}\t#{pane_left}\t#{pane_top}\t#{pane_width}\t#{pane_height}\t#{pane_active}\t#{window_zoomed_flag}\t#{pane_current_command}";

/// `tmux -F` columns behind [`current_window_frame`].
const FRAME_FMT: &str =
    "#{window_width}\t#{window_height}\t#{client_width}\t#{client_height}\t#{status-position}";

/// Where one pane sits on screen, plus the little bit of identity the
/// overlay needs to label it when no agent is attached.
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// Panes of the client's current window, with on-screen geometry.
///
/// Deliberately unscoped (no `-t`): run from inside a `display-popup`, a
/// bare `list-panes` resolves to the window the popup was raised over,
/// which is exactly the one we want to describe. Note that `$TMUX_PANE`
/// inside that popup names the *popup's own* pane, so callers must take
/// "which pane is focused" from [`PaneGeometry::active`] rather than from
/// [`super::current_pane`].
///
/// Returns `(panes, zoomed)`; empty when tmux is unavailable or errors.
pub fn current_window_panes() -> (Vec<PaneGeometry>, bool) {
    let mut cmd = tmux_command();
    cmd.args(["list-panes", "-F", PANE_GEOMETRY_FMT]);
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

/// Dimensions of the client's current window. `None` when tmux is
/// unavailable or the response can't be parsed.
pub fn current_window_frame() -> Option<WindowFrame> {
    let mut cmd = tmux_command();
    cmd.args(["display-message", "-p", "-F", FRAME_FMT]);
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
    fn frame_rejects_short_and_unparsable_lines() {
        assert!(parse_window_frame_line("").is_none());
        assert!(parse_window_frame_line("120\t39\t120").is_none());
        assert!(parse_window_frame_line("120\tx\t120\t40\ttop").is_none());
    }
}
