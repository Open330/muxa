//! Pane-card adapter for the shared visual identity rules.
use super::{pane_address, PeekCell};
pub(super) use crate::pane_identity::{address_line, pane_color};
use ratatui::style::Color;

pub(super) fn cell_color(cell: &PeekCell) -> Color {
    pane_color(
        pane_address(cell),
        cell.agent.as_ref().and_then(|a| a.tmux_socket.as_deref()),
    )
}
