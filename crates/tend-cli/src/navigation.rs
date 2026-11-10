use super::{
    interactive_child, mux_control, tmux, tmux_work, ActivityEntry, Context, HumanInteractionEntry,
    HumanInteractionInput, HumanInteractionKind, OffsetDateTime, Path, Result,
};

pub(super) async fn jump_to_pane_logged(pane_id: &str, activity_path: Option<&Path>) {
    let should_log_attach = tend::default_backend().kind() == tend::HostKind::Tmux
        && !tmux::inside_tmux()
        && activity_path.is_some();
    let target = should_log_attach
        .then(|| tmux_interaction_target(pane_id))
        .flatten();
    let started_at = OffsetDateTime::now_utc();
    jump_to_pane(pane_id);
    let ended_at = OffsetDateTime::now_utc();

    let (Some(path), Some((session_id, session_name))) = (activity_path, target) else {
        return;
    };
    if (ended_at - started_at).whole_seconds() <= 0 {
        return;
    }
    let entry =
        ActivityEntry::HumanInteraction(HumanInteractionEntry::new(HumanInteractionInput {
            kind: HumanInteractionKind::TmuxAttach,
            pane: Some(pane_id.to_string()),
            session_id,
            session_name,
            started_at,
            ended_at,
        }));
    if let Err(e) = tend::activity::append_entry(path, &entry).await {
        tracing::warn!(error = %e, path = %path.display(), "could not append tmux attach interval");
    }
}

pub(super) async fn jump_to_topology_pane_logged(
    key: &tend::PaneKey,
    activity_path: Option<&Path>,
) {
    let endpoint = &key.window.session.endpoint;
    let should_log_attach =
        endpoint.host == tend::HostKind::Tmux && !tmux::inside_tmux() && activity_path.is_some();
    let started_at = OffsetDateTime::now_utc();
    jump_to_topology_pane(key);
    let ended_at = OffsetDateTime::now_utc();
    if !should_log_attach || (ended_at - started_at).whole_seconds() <= 0 {
        return;
    }
    let Some(path) = activity_path else {
        return;
    };
    let entry =
        ActivityEntry::HumanInteraction(HumanInteractionEntry::new(HumanInteractionInput {
            kind: HumanInteractionKind::TmuxAttach,
            pane: Some(key.pane_id.clone()),
            session_id: Some(key.window.session.session_id.clone()),
            session_name: None,
            started_at,
            ended_at,
        }));
    if let Err(error) = tend::activity::append_entry(path, &entry).await {
        tracing::warn!(error = %error, path = %path.display(), "could not append tmux attach interval");
    }
}

pub(super) fn tmux_interaction_target(pane_id: &str) -> Option<(Option<String>, Option<String>)> {
    let pane = tmux::resolve_pane(pane_id)?;
    let session_id = tmux::list_sessions().ok().and_then(|sessions| {
        sessions
            .into_iter()
            .find(|session| session.name == pane.session)
            .map(|session| session.session_id)
    });
    Some((session_id, Some(pane.session)))
}

/// Attach the user to `pane_id`. Handles two cases:
///
/// * **Inside tmux** (`$TMUX` set): the user is already attached to some
///   client. Run `switch-client` to move that client to the target session,
///   with `select-window` + `select-pane` pre-positioning so the attach
///   lands on the exact pane.
///
/// * **Bare shell** (`$TMUX` unset): the user ran `tend watch` from a
///   terminal that isn't inside tmux. There's no client to "switch" — we
///   have to hand this terminal over to a new `tmux attach-session`, with
///   the target window+pane already selected so the attach lands there.
///
/// In both cases we pre-select the window and pane *before* attaching/
/// switching — `select-window`/`select-pane` are plain control messages to
/// the tmux server and don't need an attached client.
pub(super) fn jump_to_pane(pane_id: &str) {
    // Dispatch on the pane id's namespace FIRST: a `herdr:` row must jump
    // via a herdr backend even when the process-global detected host is
    // tmux (and vice versa) — the multi-host unified console lists rows
    // from every host, so the row's own id is the source of truth. Only
    // when the namespace is unrecognized (legacy/synthetic ids) do we fall
    // back to the process-global backend's kind.
    let fallback = tend::default_backend();
    let kind = dispatch_kind(pane_id, fallback.kind());
    match kind {
        // tmux jumps go straight through `tmux::` helpers and need no backend.
        tend::HostKind::Tmux => jump_to_pane_tmux(pane_id),
        tend::HostKind::Cmux => {
            let backend = backend_for_dispatch(kind, &fallback);
            jump_to_pane_cmux(backend.as_ref(), pane_id);
        }
        tend::HostKind::Rmux => {
            jump_to_pane_rmux(pane_id);
        }
        tend::HostKind::Zellij => {
            let backend = backend_for_dispatch(kind, &fallback);
            jump_to_pane_zellij(backend.as_ref(), pane_id);
        }
        tend::HostKind::Herdr => {
            let backend = backend_for_dispatch(kind, &fallback);
            jump_to_pane_herdr(backend.as_ref(), pane_id);
        }
    }
}

pub(super) fn jump_to_topology_pane(key: &tend::PaneKey) {
    match key.window.session.endpoint.host {
        tend::HostKind::Tmux => jump_to_pane_tmux_key(key),
        tend::HostKind::Cmux => {
            let backend = tend::backend::cmux::CmuxBackend::with_endpoint(
                key.window.session.endpoint.socket.clone(),
            );
            jump_to_pane_cmux(&backend, &key.pane_id);
        }
        tend::HostKind::Rmux => jump_to_pane_rmux_key(key),
        tend::HostKind::Zellij | tend::HostKind::Herdr => {
            jump_to_pane(&key.pane_id);
        }
    }
}

/// Address for the *window* half of a jump.
///
/// `switch-client -t %pane` names a pane but not a session, and tmux fills the
/// gap from recent client activity. That is unambiguous only while every
/// window belongs to exactly one session. A **session group** — created by
/// `tmux new-session -t <session>`, the supported way to keep two terminals on
/// two different windows of one workspace — links the same window into every
/// session in the group, and there the guess is routinely wrong. Measured on
/// tmux 3.4 with two attached clients: jumping client A pulled it out of its
/// own session into the grouped sibling and dragged client B's view along with
/// it, re-coupling the two terminals the group exists to separate.
///
/// `<session_id>:<window_id>` closes the gap. Both are backend-native ids, so
/// neither can prefix-match a neighbouring object the way session *names* do
/// (`app` against `app-set`), and together they name exactly one
/// window in exactly one session. Falls back to the bare pane id when no
/// session id is known, which is the behaviour every jump had before.
pub(super) fn window_target(session_id: Option<&str>, window_id: &str, pane_id: &str) -> String {
    match session_id {
        Some(session) if !session.is_empty() && !window_id.is_empty() => {
            format!("{session}:{window_id}")
        }
        _ => pane_id.to_string(),
    }
}

/// Session target for `attach-session`: the stable `$N` id when the scan
/// carried one, else the session name.
///
/// Ids are exact. Names match by prefix unless anchored with `=`, and real
/// session sets collide — `app` also matches `app-set`, so a
/// name-targeted attach can hand the terminal to the wrong workspace. The
/// name stays as a fallback only because a row parsed from an older
/// `PANE_FMT` has no session id to use.
pub(super) fn session_target<'a>(session_id: &'a str, session_name: &'a str) -> &'a str {
    if session_id.is_empty() {
        session_name
    } else {
        session_id
    }
}

/// Choose which session a jump should address, given the asking client's
/// current session and the session recorded for the target pane.
///
/// Prefers the client's own session whenever the target window is linked into
/// it: inside a session group that makes the jump a pure window change, so no
/// sibling session — and therefore no other terminal — moves. Otherwise this
/// is a genuine cross-session jump, and `pane_session` names a definite
/// destination where tmux would otherwise pick one from client activity.
///
/// `window_linked` is the membership probe, taken as a closure so the decision
/// is testable without a tmux server, and called *only* when it can change the
/// answer. Identical session ids already imply membership — the ordinary
/// same-session jump — so that case is answered without a round trip.
///
/// `None` only when neither session is known, leaving [`window_target`] to
/// fall back to the bare pane id.
pub(super) fn resolve_jump_session(
    client_session: Option<String>,
    pane_session: &str,
    window_linked: impl FnOnce(&str) -> bool,
) -> Option<String> {
    let fallback = || (!pane_session.is_empty()).then(|| pane_session.to_string());
    let Some(client_session) = client_session.filter(|session| !session.is_empty()) else {
        return fallback();
    };
    if client_session == pane_session || window_linked(&client_session) {
        return Some(client_session);
    }
    fallback()
}

/// [`resolve_jump_session`] wired to tmux on the server named by `socket`.
pub(super) fn jump_session_id(
    socket: Option<&str>,
    client: Option<&str>,
    window_id: &str,
    pane_session: &str,
) -> Option<String> {
    resolve_jump_session(
        client.and_then(|client| tend::tmux::client_session_id_on(socket, client)),
        pane_session,
        |session| tend::tmux::window_in_session_on(socket, session, window_id),
    )
}

pub(super) fn jump_to_pane_tmux_key(key: &tend::PaneKey) {
    let socket = Some(key.window.session.endpoint.socket.as_str());
    let pane = key.pane_id.as_str();
    let run = |args: &[&str]| {
        if let Err(error) = tend::tmux::run_control_on(socket, args) {
            eprintln!("tend: tmux {} failed: {error}", args.join(" "));
        }
    };
    if tmux::inside_tmux() {
        let pinned = CALLER_CLIENT.get().cloned().or_else(tmux::current_client);
        // Address the window by session, not just by pane — see `window_target`.
        let session = jump_session_id(
            socket,
            pinned.as_deref(),
            &key.window.window_id,
            &key.window.session.session_id,
        );
        let target = window_target(session.as_deref(), &key.window.window_id, pane);
        if let Some(client) = pinned.as_deref() {
            run(&["switch-client", "-c", client, "-t", &target]);
        } else {
            run(&["switch-client", "-t", &target]);
        }
        run(&["select-window", "-t", &target]);
        run(&["select-pane", "-t", pane]);
        return;
    }

    // Pre-position the session we are about to attach to. Its id is already
    // in the key, so qualify the window with it rather than letting a bare
    // pane id send `select-window` at a grouped sibling — that would move a
    // window in a session some *other* terminal is looking at, before this
    // terminal has even attached.
    let target = window_target(
        Some(key.window.session.session_id.as_str()),
        &key.window.window_id,
        pane,
    );
    run(&["select-window", "-t", &target]);
    run(&["select-pane", "-t", pane]);
    // Wait through a hang-up of our own terminal: the client is told to
    // detach and the caller's `--fit` restore guards still run afterwards.
    match interactive_child::run_interactive(tend::tmux::tmux_command_on(socket).args([
        "attach-session",
        "-t",
        key.window.session.session_id.as_str(),
    ])) {
        Ok(exit) if exit.is_clean_detach() => {}
        Ok(exit) => eprintln!(
            "tend: tmux attach-session exited with {}",
            exit.status
                .code()
                .map_or_else(|| "signal".into(), |code| code.to_string())
        ),
        Err(error) => eprintln!("tend: failed to spawn tmux attach-session: {error}"),
    }
}

/// Effective host for a per-row action: the pane id's namespace when
/// recognized, else the process-global `fallback`. Pure so the dispatch
/// table is unit-testable without constructing backends.
pub(super) fn dispatch_kind(pane_id: &str, fallback: tend::HostKind) -> tend::HostKind {
    tend::backend::pane_id_host_kind(pane_id).unwrap_or(fallback)
}

/// Reuse the already-built `fallback` backend when its kind matches, else
/// construct a fresh one for `kind` (cheap — the constructors are a unit
/// struct for tmux/zellij and a socket path for herdr).
pub(super) fn backend_for_dispatch(
    kind: tend::HostKind,
    fallback: &tend::SharedBackend,
) -> tend::SharedBackend {
    if fallback.kind() == kind {
        fallback.clone()
    } else {
        backend_for_kind(kind)
    }
}

/// Build one backend of the given kind. The CLI-side analog of the
/// (private) `tend::backend::backend_of`, used for per-row host dispatch
/// where the row's host differs from the process-global one.
pub(crate) fn backend_for_kind(kind: tend::HostKind) -> tend::SharedBackend {
    match kind {
        tend::HostKind::Tmux => std::sync::Arc::new(tend::TmuxBackend::new()),
        tend::HostKind::Cmux => std::sync::Arc::new(tend::backend::cmux::CmuxBackend::new()),
        tend::HostKind::Rmux => std::sync::Arc::new(tend::RmuxBackend::new()),
        tend::HostKind::Zellij => std::sync::Arc::new(tend::ZellijBackend::new()),
        tend::HostKind::Herdr => std::sync::Arc::new(tend::backend::herdr::HerdrBackend::new()),
    }
}

/// Aggregate pane inventories across every active backend — the multi-host
/// enumeration used by `tend panes`, `stats`, `timeline`, and `attend`.
/// Rows already carry their host namespace in `pane_id`, so a plain concat
/// keeps them distinct.
///
/// Each backend's `list_panes` blocks (tmux shells out; herdr hits a socket),
/// so we fan the calls out onto the blocking pool up front and join them —
/// the tick budget is one host's latency, not the sum, and a slow/failing
/// host can't stall the runtime or the others (a join error contributes an
/// empty list). Mirrors the concurrent fan-out `watch::compute_refresh` uses.
pub(crate) async fn all_panes() -> Vec<tend::tmux::PaneInfo> {
    let tasks: Vec<_> = tend::active_backends()
        .into_iter()
        .map(|backend| tokio::task::spawn_blocking(move || backend.list_panes()))
        .collect();
    let mut panes = Vec::new();
    for task in tasks {
        match task.await {
            Ok(list) => panes.extend(list),
            Err(e) => tracing::debug!(error = %e, "pane enumeration task failed"),
        }
    }
    panes
}

/// The tmux client that invoked this process, as expanded by the key
/// binding (`#{client_name}`) at the moment the key was pressed.
///
/// This is the only trustworthy client identity a popup-launched process
/// has: any `display-message` it runs resolves "current client" from
/// recent activity, which with two terminals attached is routinely the
/// other one — measured live, a popup opened from `/dev/pts/67` answered
/// `/dev/pts/87`. Empty when launched outside a managed binding.
pub(super) static CALLER_CLIENT: std::sync::OnceLock<String> = std::sync::OnceLock::new();

pub(super) fn jump_to_pane_tmux(pane_id: &str) {
    let Some(info) = tmux::resolve_pane(pane_id) else {
        eprintln!("tend: pane {pane_id} not found in tmux — it may have closed");
        return;
    };
    if tmux::inside_tmux() {
        // Switch after pre-positioning, addressed by stable ids, pinned to
        // the asking client.
        //
        // Each of those three matters, and the version before them had
        // none. It pre-positioned with `select-window -t "<name>:<idx>"`
        // *before* switching anyone: that mutates the target session's
        // current window immediately, so any other terminal already
        // attached to that session jumped on the spot — a window the user
        // never asked to move, in a terminal they were not even looking at.
        // The name-based target is its own hazard, because tmux matches
        // session names by prefix unless anchored with `=`, and real
        // session sets collide (`app` against `app-set`). And
        // without `-c`, tmux picks "the current client" from recent
        // activity, which with two terminals attached is routinely the
        // other one.
        //
        // A pane id alone is *not* the unambiguous identifier it looks
        // like. It names one pane, but `switch-client` needs a session,
        // and a window can be linked into more than one — that is exactly
        // what a session group is. `window_target` explains what tmux does
        // with the ambiguity and why the answer is wrong often enough to
        // matter. Address the window by `<session_id>:<window_id>` instead.
        // Prefer the binding-expanded client: it names who pressed the key.
        // `current_client()` is an activity-based guess and only acceptable
        // when nothing better exists (an old binding without the flag);
        // unpinned is last, safe only when a single client is attached.
        let pinned = CALLER_CLIENT.get().cloned().or_else(tmux::current_client);
        let session = jump_session_id(None, pinned.as_deref(), &info.window_id, &info.session_id);
        let target = window_target(session.as_deref(), &info.window_id, pane_id);
        if let Some(client) = pinned {
            run_tmux(&["switch-client", "-c", &client, "-t", &target]);
        } else {
            run_tmux(&["switch-client", "-t", &target]);
        }
        // Selecting the window *after* the switch confines the shared-state
        // mutation to the session we are entering — clients attached to *it*
        // follow, which is tmux's model for a session's current window, but
        // no bystander session is touched the way the old pre-switch
        // select-window did. With the session-qualified target above, a
        // grouped sibling is a bystander too: it keeps its own current
        // window, so the other terminal stays where the user left it.
        run_tmux(&["select-window", "-t", &target]);
        run_tmux(&["select-pane", "-t", pane_id]);
    } else {
        // Pre-position for the fresh attach below; there is no client of
        // ours yet to pin, and the session is about to become ours. Qualify
        // the window with that session all the same: a bare pane id lets
        // `select-window` land on a grouped sibling and move a window some
        // other terminal is looking at, before we have attached anywhere.
        let target = window_target(Some(&info.session_id), &info.window_id, pane_id);
        run_tmux(&["select-window", "-t", &target]);
        run_tmux(&["select-pane", "-t", pane_id]);
        // Bare shell — hand our terminal to a fresh tmux attach-session.
        // Target the session by id: names match by prefix unless anchored,
        // and real session sets collide (`app` against `app-set`).
        // `.status()` waits for tmux to exit; on detach the user is back at
        // this shell prompt, which is the least-surprising behaviour.
        match interactive_child::run_interactive(tend::tmux::tmux_command().args([
            "attach-session",
            "-t",
            session_target(&info.session_id, &info.session),
        ])) {
            Ok(exit) if exit.is_clean_detach() => {}
            Ok(exit) => eprintln!(
                "tend: tmux attach-session exited with {}",
                exit.status
                    .code()
                    .map_or_else(|| "signal".into(), |c| c.to_string())
            ),
            Err(e) => eprintln!("tend: failed to spawn tmux attach-session: {e}"),
        }
    }
}

/// Jump on zellij: a single `zellij action focus-pane-with-id <id>`
/// covers the whole story. There's no session/window analog to
/// pre-select, and no "outside zellij" attach equivalent — running
/// `tend watch` from a bare shell on a zellij host would land here
/// only if the user explicitly set `TEND_HOST=zellij`, in which case
/// failing to focus is just a "couldn't reach the zellij server"
/// stderr warning.
pub(super) fn jump_to_pane_zellij(backend: &dyn tend::PaneBackend, pane_id: &str) {
    if !backend.focus_pane(pane_id) {
        eprintln!("tend: zellij focus-pane-with-id {pane_id} failed — pane may have closed");
    }
}

pub(super) fn jump_to_pane_cmux(backend: &dyn tend::PaneBackend, pane_id: &str) {
    if !backend.focus_pane(pane_id) {
        eprintln!(
            "tend: cmux surface.focus {pane_id} failed — surface may have closed or socket access may be disabled"
        );
    }
}

/// Resolve legacy pane-only actions before using the same endpoint-aware jump.
pub(super) fn jump_to_pane_rmux(pane_id: &str) {
    use tend::PaneBackend;
    let backend = tend::RmuxBackend::new();
    let Some(pane) = backend.resolve_pane(pane_id) else {
        eprintln!("tend: rmux pane {pane_id} is unavailable");
        return;
    };
    let Some(socket) = pane.socket else {
        eprintln!("tend: rmux pane {pane_id} has no server endpoint");
        return;
    };
    jump_to_pane_rmux_target(&socket, &pane.session_id, &pane.window_id, pane_id);
}

pub(super) fn jump_to_pane_rmux_key(key: &tend::PaneKey) {
    jump_to_pane_rmux_target(
        &key.window.session.endpoint.socket,
        &key.window.session.session_id,
        &key.window.window_id,
        &key.pane_id,
    );
}

pub(super) fn rmux_client_from_context(listing: &str, context: &str) -> Option<String> {
    let clients: Vec<_> = listing
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .collect();
    if clients.len() == 1 {
        return Some((*clients[0].0).to_string());
    }
    let session = context.split(',').nth(2)?.trim_start_matches('$');
    let mut matches = clients
        .iter()
        .filter(|(_, id)| id.trim_start_matches('$') == session);
    let (client, _) = matches.next()?;
    matches.next().is_none().then(|| (*client).to_string())
}

pub(super) fn jump_to_pane_rmux_target(socket: &str, session: &str, window: &str, pane: &str) {
    let backend = tend::RmuxBackend::with_endpoint(socket);
    let pane = pane.strip_prefix("rmux:").unwrap_or(pane);
    let current = tend::backend::rmux::endpoint_from_env();
    let control = mux_control::Control::Endpoint(tend::BackendEndpoint {
        host: tend::HostKind::Rmux,
        socket: socket.to_string(),
    });
    let mut attach_view = None;
    let mut selected_session = session.to_string();
    if current.as_deref() == Some(socket) {
        let result = (|| -> Result<()> {
            let client = if let Some(client) = CALLER_CLIENT.get() {
                client.clone()
            } else {
                let listing = backend
                    .capture_control(&["list-clients", "-F", "#{client_name}\t#{session_id}"])
                    .map_err(anyhow::Error::msg)?;
                let context = std::env::var("RMUX").unwrap_or_default();
                rmux_client_from_context(&listing, &context).context(
                    "multiple rmux clients share this session: open watch using the popup binding or pass --caller-client"
                )?
            };
            let view = tmux_work::prepare_jump_view(&control, &client, session, window)?;
            let target = format!("{}:{window}.{pane}", view.id);
            // Validate and select the exact destination in the switch itself.
            // rmux terminates the invoking popup on a cross-session switch,
            // so no required commands may follow it.
            backend
                .run_control(&["switch-client", "-c", &client, "-t", &target])
                .map_err(anyhow::Error::msg)
        })();
        if let Err(error) = result {
            eprintln!("tend: rmux jump failed: {error}");
        }
        return;
    } else if current.is_some() {
        eprintln!("tend: cannot switch an rmux client across servers; attach to {socket} from a separate terminal");
        return;
    }
    if current.is_none() {
        match tmux_work::prepare_attach_view(&control, session) {
            Ok(view) => {
                selected_session.clone_from(&view.id);
                attach_view = Some(view);
            }
            Err(error) => {
                eprintln!("tend: rmux attach view failed: {error}");
                return;
            }
        }
    }
    let target = format!("{selected_session}:{window}.{pane}");
    for args in [
        ["select-window", "-t", target.as_str()],
        ["select-pane", "-t", target.as_str()],
    ] {
        if let Err(error) = backend.run_control(&args) {
            eprintln!("tend: rmux {} failed: {error}", args[0]);
            return;
        }
    }
    if current.is_none() {
        match interactive_child::run_interactive(backend.command(None).args([
            "attach-session",
            "-t",
            &selected_session,
        ])) {
            Ok(exit) if exit.is_clean_detach() => {}
            Ok(exit) => eprintln!("tend: rmux attach-session exited: {}", exit.status),
            Err(error) => eprintln!("tend: rmux attach-session failed: {error}"),
        }
    }
    drop(attach_view);
}

/// Jump on herdr: `pane.focus` over the herdr socket is the whole story,
/// same single-call shape as zellij. Focus moves the herdr UI wherever a
/// client is attached; there is no bare-shell attach handover analog.
pub(super) fn jump_to_pane_herdr(backend: &dyn tend::PaneBackend, pane_id: &str) {
    if !backend.focus_pane(pane_id) {
        eprintln!(
            "tend: herdr pane.focus {pane_id} failed — pane may have closed or the herdr server is down"
        );
    }
}

pub(super) fn run_tmux(args: &[&str]) {
    match tend::tmux::tmux_command().args(args).status() {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!(
            "tend: `tmux {}` exited with {}",
            args.join(" "),
            s.code().map_or_else(|| "signal".into(), |c| c.to_string())
        ),
        Err(e) => eprintln!("tend: failed to spawn `tmux {}`: {e}", args.join(" ")),
    }
}
