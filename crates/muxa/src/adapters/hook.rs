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
/// rationale — native rmux wins its tmux-compatibility tie, `MUXA_HOST` overrides):
/// 1. `$MUXA_HOST` override — forces the named host's pane var when present.
/// 2. `$RMUX_PANE`, namespaced to `rmux:%N`.
/// 3. `$ZELLIJ_PANE_ID` (zellij's "this pane" var).
/// 4. `$HERDR_PANE_ID` (herdr's analog), namespaced to `herdr:<id>`.
/// 5. `$TMUX_PANE` (tmux and rmux compatibility set this).
/// 6. `$CMUX_SURFACE_ID`, namespaced to `cmux:<id>` — after tmux because a
///    GUI terminal is always the outermost host (see [`host_pane_env`]).
/// 7. Walk the parent-pid chain and match against the active backend's
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
    } else if ev
        .id()
        .pane
        .as_deref()
        .and_then(crate::backend::pane_id_host_kind)
        == Some(crate::backend::HostKind::Cmux)
    {
        ev.id_mut().surface = cmux_surface_env();
    }
    // Endpoint metadata belongs to an external pane binding. A muxa-owned PTY
    // may inherit CMUX/TMUX variables from the terminal that requested it, but
    // that outer socket does not own the daemon-created PTY surface.
    //
    // The endpoint is read for the host that *owns the pane id*, never for
    // whichever host `detect_host_env` would pick on its own. A tmux pane
    // inside a cmux tab inherits `CMUX_*` variables, and an independent
    // detection used to stamp `%N` rows with the cmux socket — a pairing no
    // pane scan can match, which made the agent invisible to collaboration.
    if ev.id().tmux_socket.is_none() {
        if let Some(pane) = ev.id().pane.clone() {
            ev.id_mut().tmux_socket = pane_endpoint_env(&pane);
        }
    }
    Ok(ev)
}

/// The tmux server socket path from `$TMUX` (`"<socket>,<pid>,<session>"`),
/// when the hook process runs inside tmux. Empty/absent yields `None`. The
/// path is kept verbatim (`/private/tmp/tmux-501/default`); the daemon
/// shortens it to the scanner's socket name (`default`) on ingest through
/// `crate::backend::pane_endpoint_identity`.
fn tmux_socket_env_from(read: impl Fn(&str) -> Option<String>) -> Option<String> {
    let value = read("TMUX")?;
    let path = value.split(',').next()?.trim();
    if path.is_empty() {
        None
    } else {
        Some(path.to_string())
    }
}

/// Control endpoint for the host that owns `pane`. The persisted field retains
/// its historical `tmux_socket` name for protocol compatibility, but rmux and
/// cmux rows carry their native socket paths here.
fn pane_endpoint_env(pane: &str) -> Option<String> {
    pane_endpoint_env_from(pane, |name| std::env::var(name).ok())
}

/// Decoupled-from-process-env variant of [`pane_endpoint_env`] for tests.
///
/// The host is taken from the pane id's namespace (`%N` tmux, `cmux:<id>`,
/// `rmux:%N`, …) rather than re-detected from the environment, so the pane and
/// its endpoint can never disagree: a `%N` pane always pairs with `$TMUX`'s
/// socket even when the shell also carries a parent cmux's variables. The MCP
/// server applies the same rule when it builds a collaboration origin, which
/// is what lets the daemon match the two.
///
/// zellij and herdr panes keep the historical behaviour of recording the
/// enclosing `$TMUX` socket when there is one (their hosts have no endpoint of
/// their own on this protocol field); only tmux-vs-cmux ownership changed.
fn pane_endpoint_env_from(pane: &str, read: impl Fn(&str) -> Option<String>) -> Option<String> {
    use crate::backend::HostKind;
    match crate::backend::pane_id_host_kind(pane) {
        Some(HostKind::Rmux) => crate::backend::rmux::endpoint_from_value(&read("RMUX")?),
        Some(HostKind::Cmux) => Some(crate::backend::cmux::endpoint_from(read)),
        Some(HostKind::Tmux | HostKind::Zellij | HostKind::Herdr) | None => {
            tmux_socket_env_from(read)
        }
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
    Some(SurfaceRef {
        kind,
        id,
        workspace: None,
    })
}

fn cmux_surface_env() -> Option<SurfaceRef> {
    cmux_surface_env_from(|name| std::env::var(name).ok())
}

fn cmux_surface_env_from(read: impl Fn(&str) -> Option<String>) -> Option<SurfaceRef> {
    let id = read("CMUX_SURFACE_ID").filter(|id| !id.trim().is_empty())?;
    let workspace = read("CMUX_WORKSPACE_ID").filter(|id| !id.trim().is_empty());
    Some(SurfaceRef {
        kind: SurfaceKind::Cmux,
        id,
        workspace,
    })
}

/// Read whichever host-set "this pane" env var identifies the *innermost*
/// host, in `MUXA_HOST` override → `RMUX_PANE` → `ZELLIJ_PANE_ID` →
/// `HERDR_PANE_ID` → `TMUX_PANE` → `CMUX_SURFACE_ID` order. Empty string is
/// treated as unset. This mirrors
/// `crate::backend::detect_from`'s host-selection precedence exactly, so the
/// pane a hook is stamped onto and the backend that observes it always agree.
///
/// tmux/zellij ids are returned verbatim (`%N`, `zellij:<id>` — the latter
/// already carries its namespace). cmux and herdr raw ids are namespaced
/// with their respective prefixes; herdr uses
/// `crate::backend::herdr::PANE_ID_PREFIX` (`herdr:`) to match the
/// `herdr:<id>` shape muxa uses everywhere internally
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
///
/// **tmux wins presence ties over cmux.** cmux is a GUI terminal application
/// and therefore always the outermost layer: a tmux server started in a cmux
/// tab hands every pane shell the cmux variables, and cmux can never run
/// inside a tmux pane. Preferring cmux would stamp such hooks onto the cmux
/// surface — or, on a cmux build that exports no `CMUX_SURFACE_ID`, leave the
/// pane on tmux while `detect_from` chose cmux for the endpoint, producing a
/// `%N` row with a cmux socket that no pane scan can match. `MUXA_HOST=cmux`
/// forces the surface id when an operator really wants it.
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
            "cmux" => Some(HostKind::Cmux),
            "rmux" => Some(HostKind::Rmux),
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

    // Auto-detect: native rmux first (it also sets TMUX_PANE), then zellij,
    // herdr, tmux, and finally cmux. Byte-for-byte the same order as
    // `detect_from`, so hook stamping and backend observation never disagree.
    pane_env_for(HostKind::Rmux, &read)
        .or_else(|| pane_env_for(HostKind::Zellij, &read))
        .or_else(|| pane_env_for(HostKind::Herdr, &read))
        .or_else(|| pane_env_for(HostKind::Tmux, &read))
        .or_else(|| pane_env_for(HostKind::Cmux, &read))
}

/// The muxa pane id for one host from its "this pane" env var, or `None` when
/// that var is unset/empty. tmux/zellij ids pass through verbatim; cmux and
/// herdr raw ids receive their host namespace prefixes.
fn pane_env_for(
    host: crate::backend::HostKind,
    read: &impl Fn(&str) -> Option<String>,
) -> Option<String> {
    use crate::backend::HostKind;
    match host {
        HostKind::Tmux => read("TMUX_PANE").filter(|v| !v.is_empty()),
        HostKind::Cmux => read("CMUX_SURFACE_ID")
            .filter(|v| !v.is_empty())
            .map(|v| crate::backend::cmux::namespace_pane_id(&v)),
        HostKind::Rmux => read("RMUX_PANE")
            .filter(|v| !v.is_empty())
            .map(|v| format!("{}{v}", crate::backend::rmux::PANE_ID_PREFIX)),
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

    #[test]
    fn host_pane_env_prefixes_cmux_surface_id() {
        assert_eq!(
            host_pane_env_from(env_reader(&[("CMUX_SURFACE_ID", "surface-7")])),
            Some("cmux:surface-7".to_string()),
        );
        assert_eq!(
            host_pane_env_from(env_reader(&[
                ("MUXA_HOST", "cmux"),
                ("CMUX_SURFACE_ID", "surface-8"),
                ("TMUX_PANE", "%3"),
            ])),
            Some("cmux:surface-8".to_string()),
        );
    }

    /// A tmux pane inside a cmux tab: the shell carries `CMUX_*` next to the
    /// real `$TMUX_PANE`, and tmux must win because a GUI terminal is always
    /// the outermost host — with or without a `CMUX_SURFACE_ID`.
    #[test]
    fn host_pane_env_prefers_tmux_pane_over_inherited_cmux_env() {
        assert_eq!(
            host_pane_env_from(env_reader(&[
                ("TMUX_PANE", "%31"),
                ("CMUX_WORKSPACE_ID", "workspace-2"),
                ("CMUX_TAB_ID", "tab-2"),
                ("CMUX_SOCKET_PATH", "/Users/me/.local/state/cmux/cmux.sock"),
            ])),
            Some("%31".to_string()),
        );
        assert_eq!(
            host_pane_env_from(env_reader(&[
                ("TMUX_PANE", "%31"),
                ("CMUX_SURFACE_ID", "surface-7"),
                ("CMUX_WORKSPACE_ID", "workspace-2"),
            ])),
            Some("%31".to_string()),
            "tmux pane id must win the presence tie over cmux",
        );
    }

    /// `MUXA_HOST=tmux` stays an explicit override in the same situation (it
    /// is also the default now); `MUXA_HOST=cmux` is covered above.
    #[test]
    fn host_pane_env_muxa_host_tmux_beats_cmux_surface() {
        assert_eq!(
            host_pane_env_from(env_reader(&[
                ("MUXA_HOST", "tmux"),
                ("CMUX_SURFACE_ID", "surface-7"),
                ("TMUX_PANE", "%3"),
            ])),
            Some("%3".to_string()),
        );
    }

    /// The endpoint follows the pane's host namespace, not env detection: a
    /// `%N` pane records `$TMUX`'s socket path even when cmux variables are
    /// present, a `cmux:` pane records the cmux socket, and an `rmux:` pane
    /// records `$RMUX`'s socket.
    #[test]
    fn pane_endpoint_follows_pane_namespace_not_env_detection() {
        let nested = env_reader(&[
            ("TMUX", "/private/tmp/tmux-501/default,3338,4"),
            ("TMUX_PANE", "%31"),
            ("CMUX_WORKSPACE_ID", "workspace-2"),
            ("CMUX_SOCKET_PATH", "/Users/me/.local/state/cmux/cmux.sock"),
        ]);
        assert_eq!(
            pane_endpoint_env_from("%31", &nested),
            Some("/private/tmp/tmux-501/default".to_string()),
        );
        // The daemon shortens that path to the scanner's socket name, which
        // is what `participants_from` and `resolve_origin` compare against.
        assert_eq!(
            crate::backend::pane_endpoint_identity(Some("%31"), "/private/tmp/tmux-501/default"),
            "default",
        );
        assert_eq!(
            pane_endpoint_env_from("cmux:surface-7", &nested),
            Some("/Users/me/.local/state/cmux/cmux.sock".to_string()),
        );
        assert_eq!(
            pane_endpoint_env_from("cmux:surface-7", env_reader(&[])),
            Some(crate::backend::cmux::DEFAULT_SOCKET_PATH.to_string()),
        );
        assert_eq!(
            pane_endpoint_env_from(
                "rmux:%3",
                env_reader(&[
                    ("RMUX", "/tmp/rmux.sock,42,$1"),
                    ("TMUX", "/tmp/rmux.sock,42,$1"),
                ]),
            ),
            Some("/tmp/rmux.sock".to_string()),
        );
        // A tmux pane whose hook env lost `$TMUX` stays endpoint-less rather
        // than borrowing the cmux socket.
        assert_eq!(
            pane_endpoint_env_from("%31", env_reader(&[("CMUX_SOCKET_PATH", "/tmp/cmux.sock")]),),
            None,
        );
    }

    /// `$TMUX` is `<socket>,<server pid>,<session index>`; only the socket
    /// path is the endpoint, and blank values read as unset.
    #[test]
    fn tmux_socket_env_takes_first_field() {
        assert_eq!(
            tmux_socket_env_from(env_reader(&[(
                "TMUX",
                "/private/tmp/tmux-501/default,3338,4"
            )])),
            Some("/private/tmp/tmux-501/default".to_string()),
        );
        assert_eq!(
            tmux_socket_env_from(env_reader(&[("TMUX", " /tmp/t ,1,0")])),
            Some("/tmp/t".to_string()),
        );
        assert_eq!(tmux_socket_env_from(env_reader(&[("TMUX", "")])), None);
        assert_eq!(tmux_socket_env_from(env_reader(&[("TMUX", ",1,0")])), None);
        assert_eq!(tmux_socket_env_from(env_reader(&[])), None);
    }

    #[test]
    fn cmux_surface_metadata_retains_workspace_identity() {
        assert_eq!(
            cmux_surface_env_from(env_reader(&[
                ("CMUX_SURFACE_ID", "surface-7"),
                ("CMUX_WORKSPACE_ID", "workspace-2"),
            ])),
            Some(SurfaceRef {
                kind: SurfaceKind::Cmux,
                id: "surface-7".into(),
                workspace: Some("workspace-2".into()),
            }),
        );
    }

    /// rmux's native pane id has the same `%N` shape as the compatibility
    /// `TMUX_PANE`, so muxa adds a namespace and prefers it on a presence tie.
    #[test]
    fn host_pane_env_prefixes_and_prefers_rmux_pane_id() {
        assert_eq!(
            host_pane_env_from(env_reader(&[("RMUX_PANE", "%7"), ("TMUX_PANE", "%7"),])),
            Some(format!("{}%7", crate::backend::rmux::PANE_ID_PREFIX)),
        );
    }

    #[test]
    fn host_pane_env_muxa_host_tmux_can_override_rmux() {
        assert_eq!(
            host_pane_env_from(env_reader(&[
                ("MUXA_HOST", "tmux"),
                ("RMUX_PANE", "%7"),
                ("TMUX_PANE", "%4"),
            ])),
            Some("%4".to_string()),
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
