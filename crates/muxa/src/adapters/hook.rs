//! Shared machinery for stdin-JSON hook adapters.

use crate::event::{AgentEvent, AgentKind, SurfaceKind, SurfaceRef};
use serde::de::DeserializeOwned;
use std::io::Read;

#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("unknown hook event: {0}")]
    UnknownEvent(String),
    #[error("i/o error reading stdin: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid hook JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// Contract implemented by each stdin-JSON adapter.
pub trait HookAdapter {
    /// Per-adapter typed hook-event enum.
    type Event;
    /// Per-adapter stdin payload shape.
    type Input: DeserializeOwned;

    /// Which agent CLI this adapter targets.
    const KIND: AgentKind;

    /// Parse the `--event` flag value into a typed event variant.
    fn parse_event(flag: &str) -> Result<Self::Event, AdapterError>;

    /// Translate one typed event + parsed stdin payload into an `AgentEvent`.
    /// `pane` is `$TMUX_PANE` if the hook was invoked inside tmux.
    fn normalize(event: Self::Event, input: Self::Input, pane: Option<String>) -> AgentEvent;
}

/// Shared hook entrypoint. Binaries call this after parsing `--event`.
///
/// Reads stdin to EOF, parses as `A::Input`, normalizes to `AgentEvent`.
///
/// `pane` resolution, in order (see [`host_pane_env`] for the tie-break
/// rationale — herdr wins presence ties over tmux, `MUXA_HOST` overrides):
/// 1. `$MUXA_HOST` override — forces the named host's pane var when present.
/// 2. `$ZELLIJ_PANE_ID` (zellij's "this pane" var).
/// 3. `$HERDR_PANE_ID` (herdr's analog), namespaced to `herdr:<id>`.
/// 4. `$TMUX_PANE` (tmux sets this on every shell inside a pane).
/// 5. Walk the parent-pid chain and match against the active backend's
///    `pane_pid_map()`. Linux reads `/proc`; macOS/BSD take one `ps` process
///    snapshot and walk it in memory. Useful when an agent hook subprocess
///    didn't inherit the host env var. Skipped when the backend's
///    `caps().pane_pid_map` reports the lookup is structurally unsupported
///    (zellij CLI today) rather than transiently empty — saves a fruitless
///    walk on every sub-process hook.
///
/// Any failure (no host, process table unreadable, no match) yields
/// `pane: None`. The daemon's IPC layer accepts paneless events; agent
/// state still flows, the watch UI just hides them by default.
pub fn run_hook<A, R>(event_flag: &str, stdin: &mut R) -> Result<AgentEvent, AdapterError>
where
    A: HookAdapter,
    R: Read,
{
    let event = A::parse_event(event_flag)?;
    let mut buf = String::new();
    stdin.read_to_string(&mut buf)?;
    let input: A::Input = serde_json::from_str(&buf)?;
    let surface = muxa_session_env();
    let pane = if surface.is_some() {
        None
    } else {
        host_pane_env()
            .or_else(|| resolve_pane_via_ancestry(crate::backend::default_backend().as_ref()))
    };
    let mut ev = A::normalize(event, input, pane);
    if let Some(surface) = surface {
        ev.id_mut().surface = Some(surface);
    }
    if ev.id_mut().tmux_socket.is_none() {
        ev.id_mut().tmux_socket = tmux_socket_env();
    }
    Ok(ev)
}

/// The tmux server socket path from `$TMUX` (`"<socket>,<pid>,<session>"`),
/// when the hook process runs inside tmux. Empty/absent yields `None`.
fn tmux_socket_env() -> Option<String> {
    let value = std::env::var("TMUX").ok()?;
    let path = value.split(',').next()?.trim();
    if path.is_empty() {
        None
    } else {
        Some(path.to_string())
    }
}

fn muxa_session_env() -> Option<SurfaceRef> {
    let id = std::env::var("MUXA_SESSION_ID")
        .ok()
        .filter(|s| !s.is_empty())?;
    let backend = std::env::var("MUXA_SESSION_BACKEND").unwrap_or_else(|_| "pty".into());
    let kind = match backend.trim() {
        "tmux" => SurfaceKind::Tmux,
        "zellij" => SurfaceKind::Zellij,
        _ => SurfaceKind::Pty,
    };
    Some(SurfaceRef { kind, id })
}

/// Read whichever host-set "this pane" env var identifies the *innermost*
/// host, in `MUXA_HOST` override → `ZELLIJ_PANE_ID` → `HERDR_PANE_ID` →
/// `TMUX_PANE` order. Empty string is treated as unset. This mirrors
/// `crate::backend::detect_from`'s host-selection precedence exactly, so the
/// pane a hook is stamped onto and the backend that observes it always agree.
///
/// tmux/zellij ids are returned verbatim (`%N`, `zellij:<id>` — the latter
/// already carries its namespace). herdr's raw pane id is *not* namespaced
/// by herdr, so we stamp `crate::backend::herdr::PANE_ID_PREFIX` (`herdr:`)
/// here to match the `herdr:<id>` shape muxa uses everywhere internally
/// (registry rows, `by_pane`, and the cross-host reaping guard's
/// `pane_id_host_kind`); the prefix is stripped again before the id goes
/// back over the herdr socket.
///
/// **herdr wins presence ties over tmux.** Nesting can't be inferred from env
/// presence alone (both vars are inherited either way), so we pick the common
/// real-world case: launching herdr from a tmux shell. Those herdr pane shells
/// *do* inherit the outer `$TMUX_PANE`, so preferring tmux would (wrongly)
/// stamp every herdr hook onto the single outer tmux pane. The rarer nesting —
/// tmux running *inside* a herdr pane — is served by the `MUXA_HOST=tmux`
/// escape hatch, which forces `$TMUX_PANE` here (and the tmux backend in
/// `detect_from`). `MUXA_HOST=herdr`/`zellij` force the corresponding var.
fn host_pane_env() -> Option<String> {
    host_pane_env_from(|name| std::env::var(name).ok())
}

/// Decoupled-from-process-env variant of [`host_pane_env`] for tests, mirroring
/// `crate::backend::detect_from`. `read("VAR")` yields the env var's value if
/// set, else `None`; tests pass a closure so we never mutate `std::env`
/// (forbidden by the workspace's `forbid(unsafe_code)` posture, and racy under
/// parallel test threads).
fn host_pane_env_from(read: impl Fn(&str) -> Option<String>) -> Option<String> {
    use crate::backend::HostKind;

    // `MUXA_HOST` override — explicit operator intent wins, same as
    // `detect_from`. When the named host's pane var is actually present, use
    // it; otherwise fall through to auto-detect (a typo or a host with no
    // pane var set shouldn't strand the hook paneless).
    if let Some(raw) = read("MUXA_HOST") {
        let forced = match raw.trim().to_ascii_lowercase().as_str() {
            "tmux" => Some(HostKind::Tmux),
            "zellij" => Some(HostKind::Zellij),
            "herdr" => Some(HostKind::Herdr),
            _ => None,
        };
        if let Some(host) = forced {
            if let Some(v) = pane_env_for(host, &read) {
                return Some(v);
            }
        }
    }

    // Auto-detect: zellij, then herdr (wins over tmux on nested ties — see the
    // `host_pane_env` doc), then tmux. Byte-for-byte the same order as
    // `detect_from`, so hook stamping and backend observation never disagree.
    pane_env_for(HostKind::Zellij, &read)
        .or_else(|| pane_env_for(HostKind::Herdr, &read))
        .or_else(|| pane_env_for(HostKind::Tmux, &read))
}

/// The muxa pane id for one host from its "this pane" env var, or `None` when
/// that var is unset/empty. tmux/zellij ids pass through verbatim; herdr's raw
/// id is stamped with the `herdr:` namespace prefix.
fn pane_env_for(
    host: crate::backend::HostKind,
    read: &impl Fn(&str) -> Option<String>,
) -> Option<String> {
    use crate::backend::HostKind;
    match host {
        HostKind::Tmux => read("TMUX_PANE").filter(|v| !v.is_empty()),
        HostKind::Zellij => read("ZELLIJ_PANE_ID").filter(|v| !v.is_empty()),
        HostKind::Herdr => read("HERDR_PANE_ID")
            .filter(|v| !v.is_empty())
            .map(|v| format!("{}{v}", crate::backend::herdr::PANE_ID_PREFIX)),
    }
}

/// Walk our parent PID chain and look each ancestor up in the
/// backend's pane-pid map. Returns the matching `pane_id` string
/// when an ancestor is the shell of a known pane.
///
/// Skips entirely when the backend reports `caps().pane_pid_map == false` —
/// for zellij CLI-only the map is structurally never going to populate, so
/// walking the chain is wasted process-table traffic. Tmux backends keep the
/// existing behaviour.
fn resolve_pane_via_ancestry(backend: &dyn crate::backend::PaneBackend) -> Option<String> {
    use crate::adapters::proc_ancestry::ancestor_in_set;
    if !backend.caps().pane_pid_map {
        return None;
    }
    let pid_map = backend.pane_pid_map();
    if pid_map.is_empty() {
        return None;
    }
    let pids: std::collections::HashSet<u32> = pid_map.keys().copied().collect();
    let me = std::process::id();
    #[cfg(target_os = "linux")]
    let matched = ancestor_in_set(me, &pids, crate::adapters::proc_ancestry::parent_pid)?;
    #[cfg(not(target_os = "linux"))]
    let matched = {
        let parents = crate::process_snapshot::read_parent_pid_map();
        ancestor_in_set(me, &pids, |pid| parents.get(&pid).copied())?
    };
    pid_map.get(&matched).cloned()
}

/// Utility: truncate a prompt/message to `max` bytes, appending a single
/// ellipsis. Used by every adapter so long prompts don't blow out the event.
pub(crate) fn truncate(mut s: String, max: usize) -> String {
    if s.len() > max {
        // Truncate to a char boundary <= max, preserving UTF-8 validity.
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s.push('…');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `read` closure from a static lookup table, matching the
    /// `backend::tests::env_reader` shape. Missing keys read as `None`.
    fn env_reader(
        pairs: &'static [(&'static str, &'static str)],
    ) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_string())
        }
    }

    /// tmux and zellij pane ids pass through verbatim: `$TMUX_PANE` is
    /// already `%N` and `$ZELLIJ_PANE_ID` already carries its `zellij:`
    /// namespace, so muxa must not re-wrap either.
    #[test]
    fn host_pane_env_returns_tmux_and_zellij_verbatim() {
        assert_eq!(
            host_pane_env_from(env_reader(&[("TMUX_PANE", "%7")])),
            Some("%7".to_string()),
        );
        assert_eq!(
            host_pane_env_from(env_reader(&[("ZELLIJ_PANE_ID", "zellij:3")])),
            Some("zellij:3".to_string()),
        );
    }

    /// herdr's raw `$HERDR_PANE_ID` is un-namespaced, so the adapter stamps
    /// the `herdr:` prefix — matching the `herdr:<id>` shape muxa uses in the
    /// registry and the reaping guard's `pane_id_host_kind`.
    #[test]
    fn host_pane_env_prefixes_herdr_pane_id() {
        assert_eq!(
            host_pane_env_from(env_reader(&[("HERDR_PANE_ID", "42")])),
            Some(format!("{}42", crate::backend::herdr::PANE_ID_PREFIX)),
        );
    }

    /// When both `$TMUX_PANE` and `$HERDR_PANE_ID` are set, herdr wins by
    /// default — the common case is a herdr pane launched from a tmux shell
    /// (whose herdr shell inherits the outer `$TMUX_PANE`). Preferring tmux
    /// would stamp every herdr hook onto the single outer tmux pane. Mirrors
    /// `detect_from`'s herdr-before-tmux order.
    #[test]
    fn host_pane_env_herdr_takes_precedence_over_tmux() {
        assert_eq!(
            host_pane_env_from(env_reader(&[("TMUX_PANE", "%1"), ("HERDR_PANE_ID", "9")])),
            Some(format!("{}9", crate::backend::herdr::PANE_ID_PREFIX)),
            "herdr pane id must win the presence tie over tmux",
        );
    }

    /// `MUXA_HOST=tmux` is the escape hatch for the rarer nesting (tmux running
    /// inside a herdr pane): it forces `$TMUX_PANE` even though `$HERDR_PANE_ID`
    /// is also present. Symmetric with `detect_from`'s override.
    #[test]
    fn host_pane_env_muxa_host_tmux_forces_tmux_pane() {
        assert_eq!(
            host_pane_env_from(env_reader(&[
                ("MUXA_HOST", "tmux"),
                ("TMUX_PANE", "%1"),
                ("HERDR_PANE_ID", "9"),
            ])),
            Some("%1".to_string()),
            "MUXA_HOST=tmux must force the tmux pane id",
        );
    }

    /// `MUXA_HOST=herdr` forces the herdr pane var (case/whitespace tolerant),
    /// even against a present `$TMUX_PANE` — though that's also the default.
    #[test]
    fn host_pane_env_muxa_host_herdr_forces_herdr_pane() {
        assert_eq!(
            host_pane_env_from(env_reader(&[
                ("MUXA_HOST", " Herdr "),
                ("TMUX_PANE", "%1"),
                ("HERDR_PANE_ID", "9"),
            ])),
            Some(format!("{}9", crate::backend::herdr::PANE_ID_PREFIX)),
        );
    }

    /// A `MUXA_HOST` naming a host whose pane var isn't set falls through to
    /// auto-detect rather than stranding the hook paneless.
    #[test]
    fn host_pane_env_muxa_host_falls_through_when_var_absent() {
        assert_eq!(
            host_pane_env_from(env_reader(&[("MUXA_HOST", "zellij"), ("TMUX_PANE", "%2")])),
            Some("%2".to_string()),
            "no ZELLIJ_PANE_ID ⇒ fall through to the present TMUX_PANE",
        );
    }

    /// An empty `$HERDR_PANE_ID` is treated as unset (never returned as a
    /// bare `herdr:` prefix), matching the empty-string handling for the
    /// tmux/zellij vars.
    #[test]
    fn host_pane_env_ignores_empty_herdr_pane_id() {
        assert_eq!(
            host_pane_env_from(env_reader(&[("HERDR_PANE_ID", "")])),
            None
        );
        // Empty herdr but a real zellij id → the zellij id, not a stray prefix.
        assert_eq!(
            host_pane_env_from(env_reader(&[
                ("HERDR_PANE_ID", ""),
                ("ZELLIJ_PANE_ID", "zellij:5"),
            ])),
            Some("zellij:5".to_string()),
        );
    }

    /// No host env at all → `None`; the caller then falls back to the
    /// parent-pid ancestry walk.
    #[test]
    fn host_pane_env_none_when_unset() {
        assert_eq!(host_pane_env_from(env_reader(&[])), None);
    }
}
