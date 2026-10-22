//! Stable visual identity, independent of task state and relationship type.
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::{pane_address, PeekCell};

// Medium luminance: each color has at least 3:1 contrast on black and white.
// Identity still includes the complete address; a finite palette can collide.
const PALETTE: [Color; 6] = [
    Color::Rgb(52, 130, 190),
    Color::Rgb(180, 100, 30),
    Color::Rgb(153, 93, 180),
    Color::Rgb(20, 142, 132),
    Color::Rgb(114, 133, 33),
    Color::Rgb(190, 85, 110),
];

pub(super) fn pane_color(address: &str, socket: Option<&str>) -> Color {
    let server = socket.map(|s| muxa::backend::pane_endpoint_identity(Some(address), s));
    // Explicit FNV-1a rather than DefaultHasher: stable across process restarts
    // and Rust versions, without palette assignment state or additional IPC.
    let hash = server
        .as_deref()
        .unwrap_or("")
        .bytes()
        .chain([0])
        .chain(address.bytes())
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    PALETTE[usize::try_from(hash % 6).unwrap_or(0)]
}

pub(super) fn cell_color(cell: &PeekCell) -> Color {
    pane_color(
        pane_address(cell),
        cell.agent.as_ref().and_then(|a| a.tmux_socket.as_deref()),
    )
}

/// Highlight exact address occurrences only, preserving all text and other styles.
pub(super) fn address_line(text: String, address: &str, socket: Option<&str>) -> Line<'static> {
    if address.is_empty() {
        return Line::raw(text);
    }
    let mut spans = Vec::new();
    let mut previous = 0;
    for (start, _) in text.match_indices(address) {
        let end = start + address.len();
        // `%5` must never color the prefix of `%52`.
        if text.as_bytes().get(end).is_some_and(u8::is_ascii_digit) {
            continue;
        }
        spans.push(Span::raw(text[previous..start].to_string()));
        spans.push(Span::styled(
            text[start..end].to_string(),
            Style::default()
                .fg(pane_color(address, socket))
                .add_modifier(Modifier::BOLD),
        ));
        previous = end;
    }
    spans.push(Span::raw(text[previous..].to_string()));
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_identity_uses_server_and_complete_pane_id() {
        assert_eq!(
            pane_color("%47", Some("/tmp/tmux-1/default")),
            pane_color("%47", Some("default"))
        );
        let distinct: std::collections::HashSet<_> = (1..20)
            .map(|i| pane_color(&format!("rmux:%{i}"), Some("/tmp/server")))
            .collect();
        assert!(distinct.len() >= 4);
        let servers: std::collections::HashSet<_> = (1..20)
            .map(|i| pane_color("rmux:%47", Some(&format!("/tmp/server-{i}"))))
            .collect();
        assert!(servers.len() >= 4);
    }

    #[test]
    fn highlighting_preserves_unicode_and_does_not_confuse_pane_prefixes() {
        let text = "생성 rmux:%5 · 통신 rmux:%52 · rmux:%5";
        let line = address_line(text.into(), "rmux:%5", Some("/tmp/server"));
        assert_eq!(line.to_string(), text);
        let colored: Vec<_> = line.spans.iter().filter(|s| s.style.fg.is_some()).collect();
        assert_eq!(colored.len(), 2);
        assert!(colored.iter().all(|s| s.content == "rmux:%5"));
    }

    #[test]
    fn palette_is_legible_on_light_and_dark_backgrounds() {
        let linear = |byte: u8| {
            let value = f64::from(byte) / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        for color in PALETTE {
            let Color::Rgb(r, g, b) = color else {
                unreachable!()
            };
            let luminance = 0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b);
            assert!((luminance + 0.05) / 0.05 >= 3.0);
            assert!(1.05 / (luminance + 0.05) >= 3.0);
        }
    }
}
