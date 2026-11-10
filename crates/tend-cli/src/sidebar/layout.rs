//! A sidebar-aware focus layout keeps every pane in its original window.
use super::Side;
use anyhow::{ensure, Context, Result};

fn leaf(width: u16, height: u16, x: u16, y: u16, pane: &str) -> String {
    format!("{width}x{height},{x},{y},{}", pane.trim_start_matches('%'))
}

pub(super) fn focus(
    width: u16,
    height: u16,
    sidebar_width: u16,
    side: Side,
    sidebar: &str,
    selected: &str,
    others: &[String],
) -> Result<String> {
    ensure!(
        width >= sidebar_width + 21,
        "window is too narrow for sidebar focus"
    );
    let content_width = width - sidebar_width - 1;
    let (sidebar_x, content_x) = match side {
        Side::Left => (0, sidebar_width + 1),
        Side::Right => (content_width + 1, 0),
    };
    let content = if others.is_empty() {
        leaf(content_width, height, content_x, 0, selected)
    } else {
        ensure!(height >= 5, "window is too short for sidebar focus");
        let count = u16::try_from(others.len())?;
        ensure!(
            content_width >= count.saturating_mul(2).saturating_sub(1),
            "too many panes for a focus strip; enlarge the terminal"
        );
        let usable = content_width - (count - 1);
        let mut folded = Vec::new();
        let mut x = content_x;
        let mut remaining = usable;
        for (index, pane) in others.iter().enumerate() {
            let span_width = if index + 1 == others.len() {
                remaining
            } else {
                usable / count
            };
            folded.push(leaf(span_width, 1, x, height - 1, pane));
            x += span_width + 1;
            remaining -= span_width;
        }
        let folded = if folded.len() == 1 {
            folded.remove(0)
        } else {
            format!(
                "{content_width}x1,{content_x},{}{{{}}}",
                height - 1,
                folded.join(",")
            )
        };
        format!(
            "{content_width}x{height},{content_x},0[{},{}]",
            leaf(content_width, height - 2, content_x, 0, selected),
            folded
        )
    };
    let sidebar = leaf(sidebar_width, height, sidebar_x, 0, sidebar);
    let children = match side {
        Side::Left => format!("{sidebar},{content}"),
        Side::Right => format!("{content},{sidebar}"),
    };
    let body = format!("{width}x{height},0,0{{{children}}}");
    let checksum = body.bytes().fold(0_u16, |sum, byte| {
        sum.rotate_right(1).wrapping_add(u16::from(byte))
    });
    Ok(format!("{checksum:04x},{body}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn focus_keeps_every_id_and_reserves_a_full_height_sidebar() {
        for side in [Side::Left, Side::Right] {
            let layout = focus(140, 40, 36, side, "%9", "%2", &["%1".into(), "%3".into()]).unwrap();
            assert!(layout.contains("36x40"));
            assert!(layout.contains("103x38"));
            assert!(layout.contains("x1,"));
            for id in [",9", ",2", ",1", ",3"] {
                assert!(layout.contains(id));
            }
        }
        assert!(focus(40, 30, 36, Side::Left, "%9", "%2", &[]).is_err());
        assert!(focus(140, 4, 36, Side::Left, "%9", "%2", &["%1".into()]).is_err());
    }
    #[test]
    fn docking_nested_content_keeps_all_panes_and_handles_both_sides() {
        let base = "0000,150x45,0,0[150x22,0,0,0,150x22,0,23{75x22,0,23,1,74x22,76,23,2}]";
        for side in [Side::Left, Side::Right] {
            let docked = dock(base, 150, 45, 36, side, "%9", None).unwrap();
            assert!(docked.contains("36x45"));
            let content = undock(&docked, 150, 45, "%9").unwrap();
            let tree = decode(&content).unwrap();
            assert_eq!(tree.children.len(), 2);
            assert_eq!(tree.children[1].children.len(), 2);
            for id in [",0", ",1", ",2"] {
                assert!(content.contains(id));
            }
            assert!(!content.ends_with(",9"));
        }
        assert!(dock(base, 40, 45, 36, Side::Left, "%9", None).is_ok());
        assert!(decode("broken").is_err());
    }
}

#[derive(Clone)]
struct Node {
    width: u16,
    height: u16,
    pane: Option<String>,
    split: char,
    children: Vec<Node>,
}
impl Node {
    fn parse(raw: &str, at: &mut usize) -> Result<Self> {
        fn number(raw: &str, at: &mut usize, separator: char) -> Result<u16> {
            let end = raw[*at..]
                .find(separator)
                .ok_or_else(|| anyhow::anyhow!("invalid multiplexer layout"))?
                + *at;
            let value = raw[*at..end].parse()?;
            *at = end + 1;
            Ok(value)
        }
        let width = number(raw, at, 'x')?;
        let height = number(raw, at, ',')?;
        let _x = number(raw, at, ',')?;
        let start = *at;
        while raw.as_bytes().get(*at).is_some_and(u8::is_ascii_digit) {
            *at += 1;
        }
        ensure!(*at > start, "invalid layout coordinate");
        let mut node = Self {
            width,
            height,
            pane: None,
            split: '{',
            children: Vec::new(),
        };
        match raw.as_bytes().get(*at).copied() {
            Some(b',') => {
                *at += 1;
                let start = *at;
                while raw.as_bytes().get(*at).is_some_and(u8::is_ascii_digit) {
                    *at += 1;
                }
                ensure!(*at > start, "invalid layout pane");
                node.pane = Some(raw[start..*at].into());
            }
            Some(open @ (b'{' | b'[')) => {
                node.split = char::from(open);
                *at += 1;
                loop {
                    node.children.push(Self::parse(raw, at)?);
                    match raw.as_bytes().get(*at).copied() {
                        Some(b',') => {
                            *at += 1;
                        }
                        Some(close) if close == if open == b'{' { b'}' } else { b']' } => {
                            *at += 1;
                            break;
                        }
                        _ => anyhow::bail!("invalid layout children"),
                    }
                }
            }
            _ => anyhow::bail!("invalid layout node"),
        }
        Ok(node)
    }
    fn remove(mut self, pane: &str) -> Option<Self> {
        if self.pane.as_deref() == Some(pane.trim_start_matches('%')) {
            return None;
        }
        if self.pane.is_none() {
            self.children = self
                .children
                .into_iter()
                .filter_map(|child| child.remove(pane))
                .collect();
            if self.children.is_empty() {
                return None;
            }
            if self.children.len() == 1 {
                return self.children.pop();
            }
        }
        Some(self)
    }
    fn minimum(&self, horizontal: bool) -> u16 {
        if self.pane.is_some() {
            return 1;
        }
        if (self.split == '{') == horizontal {
            self.children
                .iter()
                .map(|child| child.minimum(horizontal))
                .sum::<u16>()
                + u16::try_from(self.children.len() - 1).unwrap_or(u16::MAX)
        } else {
            self.children
                .iter()
                .map(|child| child.minimum(horizontal))
                .max()
                .unwrap_or(1)
        }
    }
    fn fit(&self, width: u16, height: u16, x: u16, y: u16) -> Result<String> {
        ensure!(
            width >= self.minimum(true) && height >= self.minimum(false),
            "terminal too small to preserve this pane layout"
        );
        if let Some(pane) = &self.pane {
            return Ok(format!("{width}x{height},{x},{y},{pane}"));
        }
        let horizontal = self.split == '{';
        let extent = if horizontal { width } else { height };
        let usable = extent - u16::try_from(self.children.len() - 1)?;
        let minimum: u16 = self
            .children
            .iter()
            .map(|child| child.minimum(horizontal))
            .sum();
        let free = usable - minimum;
        let weights: u32 = self
            .children
            .iter()
            .map(|child| {
                u32::from(if horizontal {
                    child.width
                } else {
                    child.height
                })
            })
            .sum();
        let mut used = 0;
        let mut position = if horizontal { x } else { y };
        let mut children = Vec::new();
        for (index, child) in self.children.iter().enumerate() {
            let weight = if horizontal {
                child.width
            } else {
                child.height
            };
            let size = if index + 1 == self.children.len() {
                usable - used
            } else {
                child.minimum(horizontal)
                    + u16::try_from(u32::from(free) * u32::from(weight) / weights.max(1))?
            };
            children.push(if horizontal {
                child.fit(size, height, position, y)?
            } else {
                child.fit(width, size, x, position)?
            });
            used += size;
            position += size + 1;
        }
        Ok(format!(
            "{width}x{height},{x},{y}{}{}{}",
            self.split,
            children.join(","),
            if horizontal { '}' } else { ']' }
        ))
    }
}
fn decode(layout: &str) -> Result<Node> {
    let body = layout.split_once(',').context("missing layout checksum")?.1;
    let mut at = 0;
    let node = Node::parse(body, &mut at)?;
    ensure!(at == body.len(), "trailing layout data");
    Ok(node)
}
fn checked(body: String) -> String {
    let checksum = body.bytes().fold(0_u16, |sum, byte| {
        sum.rotate_right(1).wrapping_add(u16::from(byte))
    });
    format!("{checksum:04x},{body}")
}
pub(super) fn dock(
    base: &str,
    width: u16,
    height: u16,
    side_width: u16,
    side: Side,
    sidebar: &str,
    remove: Option<&str>,
) -> Result<String> {
    let mut node = decode(base)?;
    if let Some(pane) = remove {
        node = node.remove(pane).context("sidebar has no content pane")?;
    }
    ensure!(width > side_width, "terminal too narrow for sidebar");
    let content_width = width - side_width - 1;
    let (sx, cx) = match side {
        Side::Left => (0, side_width + 1),
        Side::Right => (content_width + 1, 0),
    };
    let content = node.fit(content_width, height, cx, 0)?;
    let sidebar = leaf(side_width, height, sx, 0, sidebar);
    let children = match side {
        Side::Left => format!("{sidebar},{content}"),
        Side::Right => format!("{content},{sidebar}"),
    };
    Ok(checked(format!("{width}x{height},0,0{{{children}}}")))
}
pub(super) fn undock(base: &str, width: u16, height: u16, sidebar: &str) -> Result<String> {
    let node = decode(base)?
        .remove(sidebar)
        .context("sidebar has no content pane")?;
    Ok(checked(node.fit(width, height, 0, 0)?))
}
