//! Pane-host abstraction.
//!
//! Today muxa shells out to tmux for everything that has to do with panes —
//! enumerating them, finding the foreground command per pane, mapping a
//! `pane_id` to the OS pid that owns its shell, focusing a pane on attach.
//! That coupling is small (~600 LOC under `crate::tmux`) but it lives in
//! the wrong layer: every caller above the IPC seam thinks in
//! `pane_id` strings and never needs to know that those ids came from
//! `tmux list-panes -F`.
//!
//! This module pulls the surface up into a [`PaneBackend`] trait so a
//! second host (zellij — see [`docs/ZELLIJ.md`](../../../../docs/ZELLIJ.md))
//! can plug in without forking the daemon. The tmux implementation in
//! [`tmux::TmuxBackend`] is a thin delegating wrapper around the existing
//! `crate::tmux::*` free functions; nothing about the production code path
//! has changed shape, this is just a seam.
//!
//! ## Why a trait, not an enum
//!
//! The set of backends is small (tmux today, zellij next, maybe screen /
//! `WezTerm` later) so an enum dispatch would compile fine. But:
//!
//! - Tests want to inject a fake backend without compiling tmux into the
//!   test binary. Trait objects make that ergonomic.
//! - The reconciler already takes a generic `LivenessSource` for the
//!   same reason; a trait keeps both abstractions consistent.
//! - Adding a backend in a downstream crate (e.g. an out-of-tree
//!   experiment) is one trait impl rather than a fork.
//!
//! ## Capability surface
//!
//! The trait is intentionally narrow — only operations the rest of the
//! codebase actually performs against the host today:
//!
//! | Method            | Used by                                          |
//! | ----------------- | ------------------------------------------------ |
//! | `list_panes`      | reconciler liveness, discovery, watch refresh    |
//! | `resolve_pane`    | hook ancestry walk, recap by-pane                |
//! | `capture_pane`    | watch preview live mode (`c` toggle)             |
//! | `pane_pid_map`    | hook ancestry walk fallback                      |
//! | `current_pane`    | watch initial-pane hint, status-line             |
//! | `focus_pane`      | watch attach action (`Enter` to jump)            |
//! | `kind`            | telemetry, log lines, debug                      |
//! | `caps`            | callers that need to know "method is plugin-only |
//! |                   | and would be a silent no-op" up front            |
//!
//! Backends that don't naturally support a method (e.g. zellij has no
//! multi-server enumeration concept) return an empty vec or `None`
//! rather than `Result::Err` — callers already treat host-down /
//! pane-gone as ephemeral. Where "transient empty" and "structurally
//! unsupported" matter (e.g. the hook adapter wants to log differently
//! when zellij CLI cannot populate `pane_pid_map`), call sites consult
//! [`BackendCaps`] returned by [`PaneBackend::caps`] before degrading.

pub mod cmux;
pub mod herdr;
pub mod rmux;
pub mod tmux;
pub mod zellij;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::tmux::PaneInfo;

/// Grace period between literal prompt injection and the submit key.
/// Codex's TUI treats an immediate trailing Enter as part of a pasted burst,
/// leaving the prompt composed but unsubmitted. All control-plane send paths
/// use the same delay so dashboard sends, MCP sends, and collaboration wakes
/// behave consistently.
pub const PROMPT_SUBMIT_GRACE: Duration = Duration::from_millis(120);

/// Whether a pane observation is authoritative enough for destructive use.
///
/// A snapshot can still carry useful panes when it is partial or incomplete.
/// `Partial` is a backend's stable contract (for example, cmux deliberately
/// exposes only the invoking surface) and must never age out rows outside that
/// subset. `Incomplete` means a normally-authoritative scan failed or timed
/// out; absence is not immediate death evidence, but chronically unreachable
/// hosts may still be aged out after the configured stale window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationCompleteness {
    Complete,
    Partial,
    Incomplete,
}

/// Pane rows observed in one backend scan, plus whether the scan covered the
/// backend's full pane namespace.
#[derive(Debug, Clone)]
pub struct PaneObservation {
    pub panes: Vec<PaneInfo>,
    pub completeness: ObservationCompleteness,
}

impl PaneObservation {
    pub fn complete(panes: Vec<PaneInfo>) -> Self {
        Self {
            panes,
            completeness: ObservationCompleteness::Complete,
        }
    }

    pub fn incomplete(panes: Vec<PaneInfo>) -> Self {
        Self {
            panes,
            completeness: ObservationCompleteness::Incomplete,
        }
    }

    /// Construct an intentionally partial observation. Unlike a transient
    /// [`Self::incomplete`] result, this backend is not expected to enumerate
    /// its whole namespace, so cross-host stale aging must keep its hook rows.
    pub fn partial(panes: Vec<PaneInfo>) -> Self {
        Self {
            panes,
            completeness: ObservationCompleteness::Partial,
        }
    }

    pub fn is_complete(&self) -> bool {
        self.completeness == ObservationCompleteness::Complete
    }

    /// Whether this observation proves the backend is intentionally present
    /// and therefore protects its namespace from cross-host age-out.
    pub fn protects_stale_rows(&self) -> bool {
        self.completeness != ObservationCompleteness::Incomplete
    }
}

/// Shared, multi-thread-safe handle to a pane backend.
///
/// The daemon constructs one of these at startup based on
/// [`detect_host_env`] and threads it to every consumer that previously
/// imported `crate::tmux::*` directly — reconciler liveness, discovery,
/// the hook ancestry walker, the watch refresh task, the daemon's
/// `enrich_from_history` step. CLI commands either receive an `Arc`
/// from the caller or build a fresh one through [`default_backend`].
///
/// `Arc` rather than `Box` so background tokio tasks can each hold a
/// clone without contending on a lock; the trait is `Send + Sync` so
/// concurrent calls into the same backend are safe.
pub type SharedBackend = Arc<dyn PaneBackend>;

/// Build the backend that matches the current process environment.
///
/// Resolution mirrors [`detect_host_env`]: `MUXA_HOST` override first,
/// then native host variables (`RMUX` before its `TMUX` compatibility
/// variable), then a default. When no host is
/// detectable we fall back to [`tmux::TmuxBackend`] — its methods
/// degrade gracefully on a host with no tmux server (`list_panes`
/// returns empty, `capture_pane` returns `None`) and most operators
/// running muxa outside a multiplexer at all are debugging the
/// daemon, not driving it. A future "noop" backend could replace
/// this fallback if that assumption stops holding.
pub fn default_backend() -> SharedBackend {
    match detect_host_env() {
        Some(HostKind::Cmux) => Arc::new(cmux::CmuxBackend::new()),
        Some(HostKind::Rmux) => Arc::new(rmux::RmuxBackend::new()),
        Some(HostKind::Zellij) => Arc::new(zellij::ZellijBackend::new()),
        Some(HostKind::Herdr) => Arc::new(herdr::HerdrBackend::new()),
        _ => Arc::new(tmux::TmuxBackend::new()),
    }
}

/// Build one backend of the given kind.
fn backend_of(kind: HostKind) -> SharedBackend {
    match kind {
        HostKind::Cmux => Arc::new(cmux::CmuxBackend::new()),
        HostKind::Tmux => Arc::new(tmux::TmuxBackend::new()),
        HostKind::Rmux => Arc::new(rmux::RmuxBackend::new()),
        HostKind::Zellij => Arc::new(zellij::ZellijBackend::new()),
        HostKind::Herdr => Arc::new(herdr::HerdrBackend::new()),
    }
}

/// Build every backend the daemon should observe simultaneously — the
/// multi-host analog of [`default_backend`] (see `docs/MULTI_HOST.md`).
///
/// Resolution:
/// 1. `MUXA_HOSTS` — comma-separated explicit set (`"rmux,tmux,herdr"`),
///    unknown names ignored, order preserved, duplicates dropped.
/// 2. `MUXA_HOST` — the existing single-host override still means
///    "exactly this one"; it wins over auto-detect but loses to the
///    explicit set, which exists precisely to widen it.
/// 3. Auto-detect: the env-preferred host (whatever [`detect_host_env`]
///    resolves the current shell to) leads so `backends[0]` is that host
///    — consumers treat the first backend as "primary" (dashboard, watch
///    initial cursor). tmux is always in the set (its methods degrade to
///    empty when no server is running, and it remains muxa's default
///    market); cmux is always kept ready as a partial observer so a GUI
///    started after muxad can still route hook control; rmux joins when its
///    native env is present or its CLI is installed, so a server started after
///    the daemon is still discovered;
///    herdr joins when a herdr server actually **answers** on its
///    socket (a live connect, not a stale socket file — see
///    [`herdr::server_reachable`]) or its pane env is present; zellij only
///    via env presence — the CLI baseline can't enumerate without a
///    plugin, so a speculative zellij backend would only add an
///    incomplete-observation source.
///
/// Never returns an empty set — the [`default_backend`] fallback rules
/// keep a lone tmux backend when nothing is detectable. Multiple
/// observers converging one registry is safe: each observation only
/// governs rows in its own pane-id namespace (see
/// [`pane_id_host_kind`] and the cross-host reaping guard).
pub fn active_backends() -> Vec<SharedBackend> {
    active_backends_from(
        |name| std::env::var(name).ok(),
        |kind| match kind {
            // herdr requires a live server probe so a stale socket cannot
            // ghost a backend into the set. rmux only requires a runnable CLI:
            // its backend must exist before a server starts so login-launched
            // muxad can discover later sessions and route hook endpoints.
            HostKind::Herdr => herdr::server_reachable(&herdr::default_socket_path()),
            HostKind::Rmux => rmux::binary_available(),
            // tmux/zellij reachability is not probed here; see the
            // resolution rules above.
            HostKind::Cmux | HostKind::Tmux | HostKind::Zellij => false,
        },
    )
}

/// Decoupled-from-process-env variant of [`active_backends`] for tests.
/// `probe(kind)` answers whether an env-independent host prerequisite is
/// available: a runnable CLI for rmux, a reachable server for herdr.
fn active_backends_from(
    read: impl Fn(&str) -> Option<String>,
    probe: impl Fn(HostKind) -> bool,
) -> Vec<SharedBackend> {
    let kinds = active_kinds_from(&read, &probe);
    kinds.into_iter().map(backend_of).collect()
}

fn active_kinds_from(
    read: &impl Fn(&str) -> Option<String>,
    probe: &impl Fn(HostKind) -> bool,
) -> Vec<HostKind> {
    fn add(kinds: &mut Vec<HostKind>, kind: HostKind) {
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
    }

    // 1. Explicit set.
    if let Some(raw) = read("MUXA_HOSTS") {
        let mut kinds = Vec::new();
        for name in raw.split(',') {
            let kind = match name.trim().to_ascii_lowercase().as_str() {
                "tmux" => Some(HostKind::Tmux),
                "cmux" => Some(HostKind::Cmux),
                "rmux" => Some(HostKind::Rmux),
                "zellij" => Some(HostKind::Zellij),
                "herdr" => Some(HostKind::Herdr),
                _ => None,
            };
            if let Some(k) = kind {
                if !kinds.contains(&k) {
                    kinds.push(k);
                }
            }
        }
        if !kinds.is_empty() {
            return kinds;
        }
        // Entirely-unparsable value falls through to the narrower rules
        // rather than silently observing nothing.
    }

    // 2. Single-host override keeps its exact meaning.
    if let Some(k) = detect_from_override(read) {
        return vec![k];
    }

    // 3. Auto-detect. The env-preferred host — whatever `detect_from`
    // resolves the current shell to (rmux > zellij > herdr > cmux > tmux on a nested-env
    // tie) — leads the set so `backends[0]` is the host the shell actually
    // lives in; consumers (dashboard, watch initial cursor) treat the first
    // backend as primary. The remaining detected hosts follow in a stable
    // order, deduped: tmux unconditionally (its methods degrade to empty when
    // no server runs), zellij on pane env, herdr on a live socket probe or
    // pane env; rmux on native env or an installed CLI. Keeping the backend
    // present before a server starts lets a login daemon discover it later
    // and route named endpoints learned from hook events.
    // `MUXA_HOSTS` (step 1) keeps its verbatim order — that's
    // explicit operator intent, not auto-detect.
    let mut kinds: Vec<HostKind> = Vec::new();
    if let Some(env_host) = detect_from(read) {
        add(&mut kinds, env_host);
    }
    add(&mut kinds, HostKind::Tmux);
    // Keep a capability-honest cmux backend ready even when muxad was
    // launched before cmux and inherited none of its environment. Its first
    // slice reports partial observations, so it cannot reap other rows.
    add(&mut kinds, HostKind::Cmux);
    if read("RMUX").is_some() || read("RMUX_PANE").is_some() || probe(HostKind::Rmux) {
        add(&mut kinds, HostKind::Rmux);
    }
    if read("ZELLIJ").is_some() {
        add(&mut kinds, HostKind::Zellij);
    }
    if read("HERDR_PANE_ID").is_some() || read("HERDR_ENV").is_some() || probe(HostKind::Herdr) {
        add(&mut kinds, HostKind::Herdr);
    }
    kinds
}

/// Just the `MUXA_HOST` step of [`detect_from`], shared with
/// [`active_kinds_from`] so the override means the same thing to both
/// resolution paths.
fn detect_from_override(read: &impl Fn(&str) -> Option<String>) -> Option<HostKind> {
    let raw = read("MUXA_HOST")?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "tmux" => Some(HostKind::Tmux),
        "cmux" => Some(HostKind::Cmux),
        "rmux" => Some(HostKind::Rmux),
        "zellij" => Some(HostKind::Zellij),
        "herdr" => Some(HostKind::Herdr),
        _ => None,
    }
}

/// Identity of the pane host. Surfaced through telemetry and log lines so
/// operators running both backends can tell which one a given event went
/// through.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    strum::Display,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum HostKind {
    Tmux,
    Cmux,
    Rmux,
    Zellij,
    Herdr,
}

/// Static capability descriptor. Callers that *need to know* whether a
/// method is structurally available (vs transiently failing) consult
/// this before degrading. The fields name specific behaviors rather
/// than methods because some methods are partial — `list_panes`
/// always works on zellij CLI but its `current_command` field is
/// always empty without the WASM plugin.
///
/// All fields default to "supported" so the tmux backend (and any
/// fully-featured future backend) doesn't have to spell out the
/// capability table — only backends with gaps zero out the relevant
/// flag.
///
/// The multi-bool shape trips clippy's `struct_excessive_bools` lint;
/// allowed here because each flag really is an independent capability
/// (no state-machine ordering between them) and a `bitflags!` macro
/// would be overkill at this scale.
///
/// A prior version of this comment said "add an enum if a fifth flag
/// lands". The fifth flag has now landed (`send_text`, added for the
/// control-plane `send_prompt` path), and we **consciously supersede**
/// that guidance: an enum/bitflags representation would force every
/// call site that reads a single field (e.g. `caps().capture_pane`) to
/// go through a lookup and lose the grep-ability that makes the current
/// shape easy to audit. Named bools remain the clearest form; revisit
/// only if the set grows past a handful.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendCaps {
    /// Whether `list_panes()`'s `PaneInfo.current_command` is populated.
    /// Zellij CLI returns the field empty until the WASM plugin lands;
    /// discovery falls back to "trust the stdin hook" when this is
    /// false instead of trying to classify panes by command.
    pub current_command: bool,
    /// Whether `pane_pid_map()` returns real data. Hook ancestry only
    /// walks the parent-pid chain when this is true; otherwise it
    /// trusts the env (`TMUX_PANE` / `ZELLIJ_PANE_ID`) and gives up
    /// quietly if that's missing too.
    pub pane_pid_map: bool,
    /// Whether `capture_pane()` returns real screen contents. The
    /// `muxa watch` live preview falls back to the prompt/response
    /// view when this is false.
    pub capture_pane: bool,
    /// Whether `focus_pane()` actually moves the user's view.
    /// Backends that can't focus (e.g. a future read-only adapter)
    /// return false here and the watch picker hides the "Enter to
    /// jump" hint accordingly.
    pub focus_pane: bool,
    /// Whether `send_text()` can inject keystrokes into a pane. This is
    /// the control-plane capability the daemon's `send_prompt` IPC (and
    /// the `muxa mcp` server on top of it) gates on: a backend that
    /// returns `false` here is refused with a structured error rather
    /// than silently dropping the prompt. tmux (`send-keys`) and herdr
    /// (`pane.send_text`) support it; zellij does not — `zellij action
    /// write-chars` only reaches the *focused* pane, so it can't safely
    /// target an arbitrary pane id.
    pub send_text: bool,
}

impl Default for BackendCaps {
    fn default() -> Self {
        Self {
            current_command: true,
            pane_pid_map: true,
            capture_pane: true,
            focus_pane: true,
            send_text: true,
        }
    }
}

/// Operations the daemon and CLI perform against the pane host.
///
/// Operational methods are best-effort: a transient failure (no tmux server,
/// the pane just closed, the env got truncated) returns an empty result rather
/// than an error. [`Self::observe_panes`] is the exception needed by liveness
/// callers: it still returns any useful rows, but also says whether absence is
/// authoritative enough for destructive reconciliation.
///
/// Implementors must be `Send + Sync + 'static` because the daemon
/// stashes them on background tasks that own their own runtimes.
pub trait PaneBackend: Send + Sync + 'static {
    /// Which host this backend talks to. Cheap; safe to call per event.
    fn kind(&self) -> HostKind;

    /// Snapshot of every pane the host considers alive right now.
    ///
    /// This remains best-effort for non-destructive consumers such as
    /// discovery and previews. Reconciliation must use [`Self::observe_panes`]
    /// so it can distinguish an empty host from a failed observation.
    fn list_panes(&self) -> Vec<PaneInfo>;

    /// Pane snapshot with an explicit completeness signal for callers that
    /// treat missing rows as liveness evidence.
    ///
    /// Backends whose `list_panes` cannot be partial may use this default.
    /// Multi-source or cache-backed implementations override it to report
    /// transient gaps without taking best-effort rows away from other callers.
    fn observe_panes(&self) -> PaneObservation {
        PaneObservation::complete(self.list_panes())
    }

    /// Look up a single pane by id. Returns `None` for unknown ids and
    /// for hosts that can't answer the query (rare).
    fn resolve_pane(&self, pane_id: &str) -> Option<PaneInfo>;

    /// Capture the visible contents of a pane with ANSI escapes intact.
    /// `None` when the host doesn't support screen capture or the pane
    /// has gone away. Used by the `muxa watch` preview's live mode.
    fn capture_pane(&self, pane_id: &str) -> Option<String>;

    /// Map of OS pid → `pane_id` for every pane the host knows about.
    /// The hook adapter walks an event's parent-pid chain and looks
    /// each ancestor up in this map to recover a `TMUX_PANE` /
    /// `ZELLIJ_PANE_ID` that wasn't inherited through env.
    fn pane_pid_map(&self) -> HashMap<u32, String>;

    /// Best-effort identification of the currently-focused pane —
    /// usually keyed off the host's "this pane" env var
    /// (`TMUX_PANE` / `ZELLIJ_PANE_ID`) plus whatever fallback the
    /// host exposes.
    fn current_pane(&self) -> Option<String>;

    /// Move the user's view to `pane_id` — `select-pane -t <id>` on
    /// tmux, `zellij action focus-pane-with-id <id>` on zellij. The
    /// trait method is only the *navigation* step; full attach
    /// semantics (tmux `switch-client` / `attach-session`) stay in the
    /// CLI because they cross the daemon/CLI process boundary in ways
    /// the backend can't see. Returns `true` when the focus call
    /// succeeded; `false` (best-effort) when the pane is gone or the
    /// host couldn't action the request.
    fn focus_pane(&self, pane_id: &str) -> bool;

    /// Inject `text` into the pane as if typed. This is a **control
    /// action** — the entry point for the daemon's `send_prompt` IPC
    /// (see `docs/PROTOCOL.md`) and the `muxa mcp` server that proxies
    /// it — so callers MUST gate on [`BackendCaps::send_text`] and refuse
    /// (structured error) rather than call a backend that can't honor it.
    ///
    /// The text is sent *literally*: no key-name interpretation, no
    /// implicit submit. To submit the pane's current line, send a
    /// carriage return as a separate call — `send_text(pane, "\r")` — which
    /// is byte-identical to tmux's `send-keys Enter` and to writing a CR to
    /// a herdr pane's pty (the daemon does exactly this for `submit: true`).
    /// Splitting text from submit keeps the primitive minimal and lets one
    /// trait method + one capability flag cover both hosts.
    ///
    /// Returns `true` when the host accepted the injection; `false`
    /// (best-effort) when the pane is gone, the host errored, or the
    /// backend doesn't support injection at all. The default impl returns
    /// `false` so a backend that never overrides it is safely inert.
    fn send_text(&self, _pane_id: &str, _text: &str) -> bool {
        false
    }

    /// Like [`Self::send_text`] but targeting the pane on a specific host
    /// *server* named by `socket`. This is the control-plane entry point: a
    /// tmux pane id like `%5` exists on every running tmux server, so the
    /// daemon threads the agent row's recorded `tmux_socket` here to inject
    /// into the RIGHT server rather than whichever one answers first.
    ///
    /// `socket` is the pane row's recorded short socket name (`default` /
    /// `amux`), or `None` when the row has no recorded socket. The default
    /// impl ignores `socket` and delegates to [`Self::send_text`] — correct
    /// for hosts without a per-server socket concept (herdr, zellij); tmux
    /// overrides it to pin the server.
    fn send_text_on(&self, _socket: Option<&str>, pane_id: &str, text: &str) -> bool {
        self.send_text(pane_id, text)
    }

    /// Like [`Self::capture_pane`] but targeting the pane on the specific host
    /// server named by `socket` — the control-plane `capture` counterpart to
    /// [`Self::send_text_on`]. The default impl ignores `socket` and delegates
    /// to [`Self::capture_pane`]; tmux overrides it to pin the server so a
    /// shared pane id can't capture the wrong screen.
    fn capture_pane_on(&self, _socket: Option<&str>, pane_id: &str) -> Option<String> {
        self.capture_pane(pane_id)
    }

    /// Static capability descriptor. Default impl returns "everything
    /// supported" because that's the tmux shape and most backends
    /// model their gaps as exceptions to that baseline.
    fn caps(&self) -> BackendCaps {
        BackendCaps::default()
    }

    /// Ingest a wholesale pane snapshot pushed by an out-of-process
    /// source — today the zellij WASM plugin, which forwards zellij
    /// `PaneUpdate` events to the daemon over the `BackendPaneSnapshot`
    /// IPC command (see [`docs/ZELLIJ.md`](../../../../docs/ZELLIJ.md)
    /// Step 2). Default no-op: backends that enumerate panes themselves
    /// (tmux) ignore external pushes; the zellij backend overrides this
    /// to replace its cached snapshot.
    fn ingest_pane_snapshot(&self, _panes: Vec<PaneInfo>) {}
}

/// `Arc<dyn PaneBackend>` is itself a backend — every method
/// delegates to the inner trait object. Lets the daemon construct one
/// `Arc` at startup and hand `.clone()`s to the reconciler, the watch
/// refresh task, the IPC server, etc., without each consumer having
/// to learn a different signature. Also unblocks
/// [`crate::reconcile::LivenessSource`]'s blanket impl: an
/// `Arc<dyn PaneBackend>` flows through as a `LivenessSource`
/// transparently because it is a `PaneBackend`.
///
/// `?Sized` so the impl covers `Arc<dyn PaneBackend>` directly, not
/// just `Arc<ConcreteBackend>`. The default `caps()` impl is
/// overridden here so `Arc::caps()` reaches into the concrete
/// implementation rather than returning the trait default.
impl<T: PaneBackend + ?Sized> PaneBackend for Arc<T> {
    fn kind(&self) -> HostKind {
        (**self).kind()
    }
    fn list_panes(&self) -> Vec<PaneInfo> {
        (**self).list_panes()
    }
    fn observe_panes(&self) -> PaneObservation {
        (**self).observe_panes()
    }
    fn resolve_pane(&self, pane_id: &str) -> Option<PaneInfo> {
        (**self).resolve_pane(pane_id)
    }
    fn capture_pane(&self, pane_id: &str) -> Option<String> {
        (**self).capture_pane(pane_id)
    }
    fn pane_pid_map(&self) -> HashMap<u32, String> {
        (**self).pane_pid_map()
    }
    fn current_pane(&self) -> Option<String> {
        (**self).current_pane()
    }
    fn focus_pane(&self, pane_id: &str) -> bool {
        (**self).focus_pane(pane_id)
    }
    fn send_text(&self, pane_id: &str, text: &str) -> bool {
        (**self).send_text(pane_id, text)
    }
    fn send_text_on(&self, socket: Option<&str>, pane_id: &str, text: &str) -> bool {
        (**self).send_text_on(socket, pane_id, text)
    }
    fn capture_pane_on(&self, socket: Option<&str>, pane_id: &str) -> Option<String> {
        (**self).capture_pane_on(socket, pane_id)
    }
    fn caps(&self) -> BackendCaps {
        (**self).caps()
    }
    fn ingest_pane_snapshot(&self, panes: Vec<PaneInfo>) {
        (**self).ingest_pane_snapshot(panes);
    }
}

/// Inspect the environment for an active host.
///
/// Resolution order:
///
/// 1. **`MUXA_HOST`** — if set to `"tmux"`, `"cmux"`, `"rmux"`, `"zellij"`, or `"herdr"`
///    (case-insensitive), that wins regardless of what `TMUX` / `ZELLIJ` /
///    `HERDR_*` look like. Provides an unambiguous override for
///    nested-multiplexer setups (zellij inside tmux, `tmux new-session` from
///    inside a herdr pane, …) where auto-detect can't tell which host the
///    current shell really lives in. Other values are ignored (treated as
///    unset) so a typo doesn't pin the daemon to the wrong host silently.
/// 2. **`RMUX` / `RMUX_PANE`** set → [`HostKind::Rmux`].
/// 3. **`ZELLIJ`** set → [`HostKind::Zellij`].
/// 4. **`HERDR_PANE_ID` / `HERDR_ENV`** set → [`HostKind::Herdr`].
/// 5. **`CMUX_SURFACE_ID` / `CMUX_WORKSPACE_ID`** set → [`HostKind::Cmux`].
/// 6. **`TMUX`** set → [`HostKind::Tmux`].
///
/// The tie-break for nested hosts (all ancestors' vars are inherited) is
/// **native rmux first**, then zellij, herdr, and tmux. rmux must precede tmux
/// because it intentionally exports tmux compatibility variables. Launching herdr *from*
/// a tmux shell is the common migration path, so herdr beats tmux on a
/// presence tie — matching the hook adapter's `host_pane_env` so the pane a
/// hook is stamped onto and the backend that observes it always agree. The
/// rarer nesting (tmux inside a herdr pane) is served by `MUXA_HOST=tmux`.
///
/// Returns `None` outside all hosts; callers fall through to a no-op
/// backend in that case.
pub fn detect_host_env() -> Option<HostKind> {
    detect_from(|name| std::env::var(name).ok())
}

/// Decoupled-from-process-env variant of [`detect_host_env`] for tests.
/// `read("VAR")` returns the env var's value if set, else `None`.
/// Production calls go through [`detect_host_env`] which inspects the
/// real process environment; tests pass a closure so we don't mutate
/// `std::env` (forbidden by the workspace's `forbid(unsafe_code)`
/// posture).
fn detect_from(read: impl Fn(&str) -> Option<String>) -> Option<HostKind> {
    // 1. `MUXA_HOST` override — explicit operator intent always wins.
    if let Some(raw) = read("MUXA_HOST") {
        match raw.trim().to_ascii_lowercase().as_str() {
            "tmux" => return Some(HostKind::Tmux),
            "cmux" => return Some(HostKind::Cmux),
            "rmux" => return Some(HostKind::Rmux),
            "zellij" => return Some(HostKind::Zellij),
            "herdr" => return Some(HostKind::Herdr),
            // Empty / unknown / typo → fall through to auto-detect.
            // Logging the bad value here would be noisy at startup;
            // the daemon traces the resolved host kind on the first
            // backend call, which is enough to diagnose mismatches.
            _ => {}
        }
    }

    // 2.–5. Auto-detect from host-set env vars. Ordering is a tie-break
    // for nested hosts (all ancestors' vars are inherited): newer hosts
    // are checked before tmux because launching herdr/zellij *from* a tmux
    // shell is the common migration path — and the inner shell inherits the
    // outer `$TMUX`, so it can't be disambiguated by presence alone. The
    // rarer nesting (tmux inside a herdr pane) uses the `MUXA_HOST` override.
    // `host_pane_env` in the hook adapter mirrors this exact order.
    // rmux deliberately exports tmux compatibility variables, so its native
    // identity must be checked before every inherited/compatibility host.
    if read("RMUX").is_some() || read("RMUX_PANE").is_some() {
        return Some(HostKind::Rmux);
    }
    if read("ZELLIJ").is_some() {
        return Some(HostKind::Zellij);
    }
    if read("HERDR_PANE_ID").is_some() || read("HERDR_ENV").is_some() {
        return Some(HostKind::Herdr);
    }
    if read("CMUX_SURFACE_ID").is_some() || read("CMUX_WORKSPACE_ID").is_some() {
        return Some(HostKind::Cmux);
    }
    if read("TMUX").is_some() {
        return Some(HostKind::Tmux);
    }
    None
}

/// Classify a namespaced pane id back to the host that governs it.
///
/// muxa namespaces every non-tmux pane id — `cmux:<id>` for cmux, `rmux:%N`
/// for rmux, `zellij:<id>` for zellij, `herdr:<id>` (see
/// [`herdr::PANE_ID_PREFIX`]) for herdr — and leaves
/// tmux's native `%N` ids as-is. The reconciler's cross-host reaping guard
/// uses this to tell whether a registry row belongs to the *observing*
/// backend: a `%`-id is tmux's, a `herdr:`-id is herdr's, a `zellij:`-id is
/// zellij's. When it does not, that row's pane can't appear in this
/// backend's observation, so its absence is not liveness evidence.
///
/// Returns `None` for shapes muxa doesn't recognize (legacy rows, paneless
/// synthetic ids, a future host's ids) — those stay governed by whatever
/// backend is active today, preserving pre-guard behavior.
#[must_use]
pub fn pane_id_host_kind(pane_id: &str) -> Option<HostKind> {
    if pane_id.starts_with(rmux::PANE_ID_PREFIX) {
        Some(HostKind::Rmux)
    } else if pane_id.starts_with(cmux::PANE_ID_PREFIX) {
        Some(HostKind::Cmux)
    } else if pane_id.starts_with('%') {
        Some(HostKind::Tmux)
    } else if pane_id.starts_with("zellij:") {
        // No shared const for zellij's prefix yet (its ids are minted by the
        // WASM plugin as `zellij:<terminal-id>`); the literal is the single
        // source of truth until one is introduced.
        Some(HostKind::Zellij)
    } else if pane_id.starts_with(herdr::PANE_ID_PREFIX) {
        Some(HostKind::Herdr)
    } else {
        None
    }
}

/// Canonical endpoint identity for a pane host.
///
/// tmux pane scans store a short socket name while hooks inherit a full path,
/// so tmux keeps its historical basename normalization. rmux and cmux use
/// native full socket paths; shortening them would make control operations
/// unable to target the recorded server and could collide across directories.
#[must_use]
pub fn pane_endpoint_identity(pane_id: Option<&str>, endpoint: &str) -> String {
    if matches!(
        pane_id.and_then(pane_id_host_kind),
        Some(HostKind::Rmux | HostKind::Cmux)
    ) {
        endpoint.to_string()
    } else {
        crate::tmux::socket_short_name(endpoint)
    }
}

/// Compare two endpoint spellings using the identity rules of `pane_id`.
#[must_use]
pub fn pane_endpoints_match(pane_id: Option<&str>, left: &str, right: &str) -> bool {
    pane_endpoint_identity(pane_id, left) == pane_endpoint_identity(pane_id, right)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper that builds a `read` closure from a static lookup table.
    /// Tests pass `&[("TMUX", "1"), ...]`; missing keys read as
    /// `None`. Keeps the call sites short.
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

    /// All host env vars unset → `None`. The daemon uses this signal to
    /// fall back to a no-op backend rather than spamming `tmux` calls
    /// on a host without a multiplexer running.
    #[test]
    fn detect_returns_none_when_no_host_env_set() {
        assert!(detect_from(env_reader(&[])).is_none());
    }

    /// Only `TMUX` set → tmux. Only `ZELLIJ` set → zellij. Locks the
    /// happy-path mapping for both backends.
    #[test]
    fn detect_picks_tmux_or_zellij_from_env() {
        assert_eq!(
            detect_from(env_reader(&[("TMUX", "1")])),
            Some(HostKind::Tmux),
        );
        assert_eq!(
            detect_from(env_reader(&[("ZELLIJ", "1")])),
            Some(HostKind::Zellij),
        );
        assert_eq!(
            detect_from(env_reader(&[("RMUX", "/tmp/rmux.sock,42,$1")])),
            Some(HostKind::Rmux),
        );
        assert_eq!(
            detect_from(env_reader(&[("CMUX_SURFACE_ID", "surface-7")])),
            Some(HostKind::Cmux),
        );
        assert_eq!(
            detect_from(env_reader(&[
                ("CMUX_SURFACE_ID", "surface-7"),
                ("TMUX", "/tmp/outer,1,0"),
            ])),
            Some(HostKind::Cmux),
        );
    }

    /// rmux publishes `TMUX` and `TMUX_PANE` compatibility variables in every
    /// pane. Its native variables must win or rmux agents are misclassified as
    /// tmux and collide with real tmux `%N` pane ids.
    #[test]
    fn detect_prefers_native_rmux_over_tmux_compatibility_env() {
        assert_eq!(
            detect_from(env_reader(&[
                ("RMUX", "/tmp/rmux.sock,42,$1"),
                ("RMUX_PANE", "%3"),
                ("TMUX", "/tmp/rmux.sock,42,$1"),
                ("TMUX_PANE", "%3"),
            ])),
            Some(HostKind::Rmux),
        );
    }

    /// When both env vars are present (nested multiplexers — rare but
    /// possible), zellij wins by default. The `MUXA_HOST` override
    /// covers the case where this default picks wrong.
    #[test]
    fn detect_prefers_zellij_when_both_env_vars_set() {
        assert_eq!(
            detect_from(env_reader(&[("TMUX", "1"), ("ZELLIJ", "1")])),
            Some(HostKind::Zellij),
        );
    }

    /// herdr beats tmux on a presence tie (herdr launched from a tmux shell,
    /// the common migration path). Locks the policy that `host_pane_env`
    /// mirrors so hook stamping and backend observation agree.
    #[test]
    fn detect_prefers_herdr_over_tmux() {
        assert_eq!(
            detect_from(env_reader(&[("HERDR_PANE_ID", "9"), ("TMUX", "1")])),
            Some(HostKind::Herdr),
        );
        // `HERDR_ENV` alone (no pane id) still identifies herdr.
        assert_eq!(
            detect_from(env_reader(&[("HERDR_ENV", "1"), ("TMUX", "1")])),
            Some(HostKind::Herdr),
        );
    }

    /// `MUXA_HOST=tmux` forces tmux even against a present herdr env — the
    /// escape hatch for tmux running inside a herdr pane.
    #[test]
    fn detect_muxa_host_tmux_overrides_herdr() {
        assert_eq!(
            detect_from(env_reader(&[
                ("MUXA_HOST", "tmux"),
                ("HERDR_PANE_ID", "9"),
                ("TMUX", "1"),
            ])),
            Some(HostKind::Tmux),
            "MUXA_HOST=tmux must beat a present HERDR env var",
        );
    }

    /// `MUXA_HOST=tmux` wins over `ZELLIJ` being set — the escape hatch
    /// for the "I'm in tmux even though my env still carries the parent
    /// zellij's vars" case. Mirror test for `MUXA_HOST=zellij`.
    #[test]
    fn detect_muxa_host_overrides_auto_detect() {
        assert_eq!(
            detect_from(env_reader(&[("MUXA_HOST", "tmux"), ("ZELLIJ", "1")])),
            Some(HostKind::Tmux),
            "MUXA_HOST=tmux must beat a present ZELLIJ env var",
        );
        assert_eq!(
            detect_from(env_reader(&[("MUXA_HOST", "zellij"), ("TMUX", "1")])),
            Some(HostKind::Zellij),
            "MUXA_HOST=zellij must beat a present TMUX env var",
        );
    }

    /// Override is case-insensitive and tolerates whitespace — users
    /// hand-type these in shell rcfiles where `MUXA_HOST=Zellij`,
    /// `MUXA_HOST=" tmux "`, etc., are realistic.
    #[test]
    fn detect_muxa_host_is_case_and_whitespace_tolerant() {
        assert_eq!(
            detect_from(env_reader(&[("MUXA_HOST", "TMUX")])),
            Some(HostKind::Tmux),
        );
        assert_eq!(
            detect_from(env_reader(&[("MUXA_HOST", " Zellij ")])),
            Some(HostKind::Zellij),
        );
    }

    /// Unknown / empty `MUXA_HOST` falls through to auto-detect rather
    /// than pinning the daemon to a "no host" decision. Typos surface
    /// as auto-detect outcomes — consistent with the daemon picking
    /// up *some* working host rather than refusing to run.
    #[test]
    fn detect_unknown_muxa_host_falls_through_to_auto() {
        // Unknown value, no host env → None (no host running).
        assert!(detect_from(env_reader(&[("MUXA_HOST", "screen")])).is_none());
        // Unknown value, but TMUX is up → tmux wins via auto-detect.
        assert_eq!(
            detect_from(env_reader(&[("MUXA_HOST", "screen"), ("TMUX", "1")])),
            Some(HostKind::Tmux),
        );
        // Empty MUXA_HOST is ignored too.
        assert_eq!(
            detect_from(env_reader(&[("MUXA_HOST", ""), ("ZELLIJ", "1")])),
            Some(HostKind::Zellij),
        );
    }

    /// `HostKind` round-trips through its `Display` impl as a stable
    /// lowercase string — the format telemetry pipelines depend on.
    #[test]
    fn host_kind_display_is_lowercase_stable() {
        assert_eq!(HostKind::Tmux.to_string(), "tmux");
        assert_eq!(HostKind::Cmux.to_string(), "cmux");
        assert_eq!(HostKind::Rmux.to_string(), "rmux");
        assert_eq!(HostKind::Zellij.to_string(), "zellij");
    }

    /// The cross-host reaping guard's classifier maps each host's pane-id
    /// namespace back to its `HostKind`: tmux `%N`, `zellij:<id>`,
    /// `herdr:<id>`. Unknown shapes return `None` so they stay governed by
    /// the active backend (pre-guard behavior). Locks the exact prefixes the
    /// reconciler relies on to avoid reaping another host's live rows.
    #[test]
    fn pane_id_host_kind_classifies_each_namespace() {
        assert_eq!(pane_id_host_kind("%3"), Some(HostKind::Tmux));
        assert_eq!(pane_id_host_kind("%0"), Some(HostKind::Tmux));
        assert_eq!(pane_id_host_kind("rmux:%3"), Some(HostKind::Rmux));
        assert_eq!(pane_id_host_kind("cmux:surface-7"), Some(HostKind::Cmux));
        assert_eq!(pane_id_host_kind("zellij:7"), Some(HostKind::Zellij));
        assert_eq!(
            pane_id_host_kind("zellij:terminal:7"),
            Some(HostKind::Zellij),
        );
        // Reference the const the herdr backend actually stamps, so a rename
        // there fails this test rather than silently desyncing the guard.
        assert_eq!(
            pane_id_host_kind(&format!("{}42", herdr::PANE_ID_PREFIX)),
            Some(HostKind::Herdr),
        );
        // Unknown / legacy / paneless-synthetic shapes: governed by whoever
        // is the active backend, exactly as before the guard existed.
        assert_eq!(pane_id_host_kind("synthetic-%1"), None);
        assert_eq!(pane_id_host_kind("weird-id"), None);
        assert_eq!(pane_id_host_kind(""), None);
    }

    #[test]
    fn endpoint_identity_preserves_rmux_path_but_shortens_tmux_path() {
        assert_eq!(
            pane_endpoint_identity(Some("rmux:%3"), "/tmp/rmux-501/default"),
            "/tmp/rmux-501/default",
        );
        assert_eq!(
            pane_endpoint_identity(Some("cmux:surface-7"), "/tmp/cmux-debug.sock"),
            "/tmp/cmux-debug.sock",
        );
        assert_eq!(
            pane_endpoint_identity(Some("%3"), "/tmp/tmux-501/default"),
            "default",
        );
        assert!(pane_endpoints_match(
            Some("%3"),
            "/tmp/tmux-501/default",
            "default"
        ));
        assert!(!pane_endpoints_match(
            Some("rmux:%3"),
            "/tmp/one/default",
            "/tmp/two/default"
        ));
    }

    /// A minimal fake the rest of the test module reuses — keeps
    /// the boilerplate of a full `PaneBackend` impl out of every
    /// individual test body.
    struct FakeBackend {
        panes: Vec<PaneInfo>,
        caps: BackendCaps,
    }

    impl FakeBackend {
        fn with_panes(panes: Vec<PaneInfo>) -> Self {
            Self {
                panes,
                caps: BackendCaps::default(),
            }
        }
    }

    impl PaneBackend for FakeBackend {
        fn kind(&self) -> HostKind {
            HostKind::Zellij
        }
        fn list_panes(&self) -> Vec<PaneInfo> {
            self.panes.clone()
        }
        fn resolve_pane(&self, id: &str) -> Option<PaneInfo> {
            self.panes.iter().find(|p| p.pane_id == id).cloned()
        }
        fn capture_pane(&self, _: &str) -> Option<String> {
            Some("hello\n".into())
        }
        fn pane_pid_map(&self) -> HashMap<u32, String> {
            self.panes
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    (
                        u32::try_from(100 + i).unwrap_or(u32::MAX),
                        p.pane_id.clone(),
                    )
                })
                .collect()
        }
        fn current_pane(&self) -> Option<String> {
            self.panes.first().map(|p| p.pane_id.clone())
        }
        fn focus_pane(&self, _: &str) -> bool {
            true
        }
        fn caps(&self) -> BackendCaps {
            self.caps
        }
    }

    fn fake_pane(id: &str) -> PaneInfo {
        PaneInfo {
            session_group: None,
            agent_role: None,
            agent_alias: None,
            socket: None,
            pane_id: id.into(),
            session_id: String::new(),
            session: "z".into(),
            window_id: String::new(),
            window_name: String::new(),
            window_index: "0".into(),
            pane_index: "0".into(),
            tty: String::new(),
            current_command: "claude".into(),
            title: String::new(),
            pane_pid: 0,
            current_path: String::new(),
        }
    }

    /// Trait surface contract: every method on a hand-rolled fake
    /// behaves as documented. Adding a new required method to
    /// `PaneBackend` breaks this test loudly.
    #[test]
    fn fake_backend_satisfies_trait_contract() {
        let b: Box<dyn PaneBackend> = Box::new(FakeBackend::with_panes(vec![fake_pane("zj-1")]));
        assert_eq!(b.kind(), HostKind::Zellij);
        assert_eq!(b.list_panes().len(), 1);
        assert_eq!(b.resolve_pane("zj-1").unwrap().pane_id, "zj-1");
        assert!(b.resolve_pane("nope").is_none());
        assert_eq!(b.capture_pane("zj-1").as_deref(), Some("hello\n"));
        assert_eq!(b.pane_pid_map().get(&100).map(String::as_str), Some("zj-1"));
        assert_eq!(b.current_pane().as_deref(), Some("zj-1"));
        assert!(b.focus_pane("zj-1"));
        assert_eq!(b.caps(), BackendCaps::default());
    }

    /// End-to-end through the reconciler: a `PaneBackend` plugged into
    /// `Reconciler::new` drives stale-pane reaping exactly the way the
    /// daemon expects. Locks the bridge between the two abstractions
    /// so a future change to either side surfaces here instead of in
    /// production.
    #[tokio::test]
    async fn pane_backend_drives_reconciler_via_blanket_liveness_impl() {
        use crate::event::{AgentEvent, AgentId, AgentKind};
        use crate::reconcile::Reconciler;
        use crate::state::Store;
        use std::time::Duration;
        use time::macros::datetime;

        let store = Store::shared();
        let t0 = datetime!(2026-04-28 12:00:00 UTC);
        // `FakeBackend::kind()` is `Zellij`, so use zellij-namespaced pane ids
        // here: the cross-host reaping guard only lets an observation reap
        // rows whose pane id belongs to the observing host. Tmux `%N` rows
        // under a zellij observation are (correctly) exempt, so this test
        // must speak the observing host's namespace to exercise reaping.
        for sid in ["alive", "ghost"] {
            store
                .apply(&AgentEvent::Started {
                    id: AgentId {
                        tmux_socket: None,
                        kind: AgentKind::ClaudeCode,
                        session_id: sid.into(),
                        surface: None,
                        pane: Some(format!("zellij:{sid}")),
                        cwd: None,
                    },
                    at: t0,
                })
                .await;
        }
        // Backend reports only zellij:alive as live; zellij:ghost is reaped.
        let backend = FakeBackend::with_panes(vec![fake_pane("zellij:alive")]);
        let r = Reconciler::new(store.clone(), backend, Duration::from_millis(10));
        let report = r.reconcile_once().await;
        assert_eq!(report.stale_panes_reaped, 1);
        let snap = store.snapshot().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].session_id, "alive");
    }

    /// `caps()` defaults to "everything supported" so backends with
    /// gaps must opt out explicitly. Locks down the default-impl
    /// behavior so future fields land with backwards-compatible
    /// semantics.
    #[test]
    fn backend_caps_default_is_all_true() {
        let caps = BackendCaps::default();
        assert!(caps.current_command);
        assert!(caps.pane_pid_map);
        assert!(caps.capture_pane);
        assert!(caps.focus_pane);
        assert!(caps.send_text);
    }

    /// `MUXA_HOSTS` is an explicit ordered set: names normalize, unknown
    /// entries and duplicates drop, and a fully-unparsable value falls
    /// through to auto-detect instead of observing nothing.
    #[test]
    fn active_kinds_explicit_set() {
        let kinds = active_kinds_from(
            &env_reader(&[("MUXA_HOSTS", " Herdr, tmux ,herdr,bogus")]),
            &|_| false,
        );
        assert_eq!(kinds, vec![HostKind::Herdr, HostKind::Tmux]);

        let fallthrough = active_kinds_from(&env_reader(&[("MUXA_HOSTS", "bogus,")]), &|_| false);
        assert_eq!(fallthrough, vec![HostKind::Tmux, HostKind::Cmux]);

        let rmux = active_kinds_from(&env_reader(&[("MUXA_HOSTS", "rmux,tmux")]), &|_| false);
        assert_eq!(rmux, vec![HostKind::Rmux, HostKind::Tmux]);

        let cmux = active_kinds_from(&env_reader(&[("MUXA_HOSTS", "cmux,tmux")]), &|_| false);
        assert_eq!(cmux, vec![HostKind::Cmux, HostKind::Tmux]);
    }

    /// `MUXA_HOST` (singular) keeps meaning "exactly this one" even in
    /// the set-valued resolution, and loses only to `MUXA_HOSTS`.
    #[test]
    fn active_kinds_single_override_stays_exact() {
        let kinds = active_kinds_from(
            &env_reader(&[("MUXA_HOST", "herdr"), ("TMUX", "/tmp/t,1,0")]),
            // Even a reachable herdr socket must not widen an explicit
            // single-host override.
            &|_| true,
        );
        assert_eq!(kinds, vec![HostKind::Herdr]);
    }

    /// Auto-detect: tmux is unconditional; rmux joins when its CLI is runnable;
    /// herdr joins on env presence or a live socket probe; zellij joins on env
    /// presence only. With no available optional host, tmux simply leads.
    #[test]
    fn active_kinds_auto_detect() {
        assert_eq!(
            active_kinds_from(&env_reader(&[]), &|_| false),
            vec![HostKind::Tmux, HostKind::Cmux],
        );
        assert_eq!(
            active_kinds_from(&env_reader(&[]), &|k| k == HostKind::Herdr),
            vec![HostKind::Tmux, HostKind::Cmux, HostKind::Herdr],
        );
        assert_eq!(
            active_kinds_from(&env_reader(&[]), &|k| k == HostKind::Rmux),
            vec![HostKind::Tmux, HostKind::Cmux, HostKind::Rmux],
        );
        // Both HERDR and ZELLIJ env present: zellij is the env-preferred host
        // (nested-env tie-break), so it leads; tmux is auto-added; herdr trails
        // on its env presence. `backends[0]` is the env-preferred host.
        assert_eq!(
            active_kinds_from(&env_reader(&[("HERDR_ENV", "1"), ("ZELLIJ", "1")]), &|_| {
                false
            },),
            vec![
                HostKind::Zellij,
                HostKind::Tmux,
                HostKind::Cmux,
                HostKind::Herdr
            ],
        );
    }

    /// Fix 2: the env-preferred host leads the auto-detected set so
    /// `backends[0]` is the host the current shell lives in. A herdr shell
    /// (`HERDR_ENV`) with tmux auto-added yields `[Herdr, Tmux]`, not
    /// `[Tmux, Herdr]` — the migration case where the operator is *in* herdr
    /// but the tmux server is also observed. The probe is irrelevant here
    /// (env presence already includes herdr), and no duplicate is produced.
    #[test]
    fn active_kinds_env_preferred_host_leads() {
        assert_eq!(
            active_kinds_from(&env_reader(&[("HERDR_ENV", "1")]), &|_| false),
            vec![HostKind::Herdr, HostKind::Tmux, HostKind::Cmux],
        );
        // A herdr pane env plus a reachable-socket probe must not double-add
        // herdr; it still leads, tmux trails.
        assert_eq!(
            active_kinds_from(&env_reader(&[("HERDR_PANE_ID", "9")]), &|k| k
                == HostKind::Herdr),
            vec![HostKind::Herdr, HostKind::Tmux, HostKind::Cmux],
        );
        // A plain tmux shell (only `TMUX`) is already tmux-first; the env
        // preference and the unconditional tmux add resolve to a single entry.
        assert_eq!(
            active_kinds_from(&env_reader(&[("TMUX", "/tmp/t,1,0")]), &|_| false),
            vec![HostKind::Tmux, HostKind::Cmux],
        );
        // rmux exports TMUX too; native rmux remains primary while tmux stays
        // in the observed set for any real tmux server also running.
        assert_eq!(
            active_kinds_from(
                &env_reader(&[
                    ("RMUX", "/tmp/rmux.sock,42,$1"),
                    ("TMUX", "/tmp/rmux.sock,42,$1"),
                ]),
                &|_| false,
            ),
            vec![HostKind::Rmux, HostKind::Tmux, HostKind::Cmux],
        );
    }
}
