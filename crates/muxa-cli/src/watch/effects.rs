use super::{
    io, session_base_name, topology_key_label, App, CollaborationOrigin, Command, HintLevel,
    IconSet, OffsetDateTime, PaneKey, Path, SessionKey, Stdio, TopologyNodeKey, WindowKey, Write,
};

/// A power-user action requested against the currently-selected row.
///
/// The input handler returns one of these (or `None`) and a separate
/// helper executes it via [`dispatch_quick_action`]. Keeping the
/// "what should happen" decision separate from "shell out to tmux /
/// xclip" lets unit tests verify the selection logic without touching
/// the real subprocess world — see the `Effects` trait below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum QuickAction {
    /// Close the exact selected session/window/pane. The key carries the
    /// backend endpoint so repeated native ids cannot hit another server.
    TerminateNode(TopologyNodeKey),
    /// Interrupt the exact selected agent pane without closing it.
    InterruptPane(PaneKey),
    /// Kill the tmux pane the agent is running in. String is the
    /// `pane_id` (e.g. `%42`) we'll pass to `tmux kill-pane -t`.
    KillPane(String),
    /// Send Ctrl-C to the pane to abort the current turn. The spec
    /// originally called this "restart"; the pragmatic shape is just
    /// "abort" because we don't know the original launch command to
    /// re-run reliably across Claude/Codex/Gemini wrappers.
    AbortTurn(String),
    /// Copy the selected agent's `last_prompt` to the system clipboard.
    /// String is the prompt body; the dispatcher tries pbcopy / wl-copy
    /// / xclip in order and falls back to a temp file if none work.
    CopyPrompt(String),
    /// Send a freshly-authored prompt straight into the selected pane.
    /// The dispatcher writes the text literally, waits briefly, then
    /// sends Enter as a separate key so the target agent submits it.
    SendPrompt { pane_id: String, text: String },
    /// Endpoint-pinned form used by the topology tree. Raw `%N` is not a
    /// sufficient control target when multiple tmux sockets are visible.
    SendPromptTo { pane: PaneKey, text: String },
    /// The same prompt to several exact panes (every live agent of a Work).
    /// Each is attempted; the outcome reports how many succeeded.
    /// `unavailable` names intended agents that could not be resolved to a
    /// pane; they count as failures so the result never overstates reach.
    SendPromptToMany {
        panes: Vec<PaneKey>,
        unavailable: Vec<String>,
        text: String,
    },
    /// Interrupt several exact panes, reporting partial success.
    InterruptPanes {
        panes: Vec<PaneKey>,
        unavailable: Vec<String>,
    },
    /// Toggle the `?` help overlay. Pure UI — no side-effects.
    ShowHelp,
    /// Legacy no-selection fallback: create or reuse a session and window at `dir`, then
    /// start an agent pane with its first prompt on the command line. The launch
    /// string is fully assembled (and shell-quoted) by the spawn form so
    /// the dispatcher stays a dumb pipe.
    SpawnWork {
        dir: String,
        agent_label: &'static str,
        launch: String,
        /// Explicit Work id; `None` derives one from the directory.
        work: Option<String>,
    },
    /// Spawn relative to an exact selected topology node. Session targets
    /// create a new window; window/pane targets add a sibling pane.
    SpawnAt {
        target: TopologyNodeKey,
        dir: String,
        name: String,
        agent_label: &'static str,
        launch: String,
    },
}

impl QuickAction {
    pub(super) fn invalidates_directory(&self) -> bool {
        matches!(
            self,
            Self::TerminateNode(_)
                | Self::KillPane(_)
                | Self::SpawnWork { .. }
                | Self::SpawnAt { .. }
        )
    }
}

/// Payload behind the shared y/N confirmation popup. Most confirmations are
/// synchronous quick actions; clearing ask history is daemon-owned and must be
/// dispatched asynchronously by the run loop instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConfirmAction {
    Quick(QuickAction),
    /// Withdraw a request this console sent that nobody has claimed yet.
    CancelCollaboration {
        origin: CollaborationOrigin,
        request_id: String,
    },
    DeleteAskHistory(String),
    ClearAskHistory,
    DeleteMessageSkill(String),
}

/// Outcome of running a [`QuickAction`] — surfaced to the run loop
/// which then turns it into a transient footer hint or a state mutation.
///
/// Note that "not applicable" is handled at the **input-handler** stage
/// (via `Action::NotApplicable`) rather than here: by the time the
/// dispatcher runs we've already decided the action is going through,
/// so anything the dispatcher reports back is either success or a
/// real backend failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ActionOutcome {
    /// Action ran successfully. Message goes into the footer hint.
    Ok(String),
    /// Action failed (tmux exited non-zero, clipboard binaries all
    /// missing, …). Message goes into the footer hint with an error tone.
    Err(String),
    /// Help overlay toggled — no hint to surface; the renderer reads
    /// `app.help_open` directly.
    HelpToggled,
}

/// Side-effect surface for [`dispatch_quick_action`]. Lifting the three
/// shell-outs into a trait means tests can pass a `RecorderEffects`
/// stub that records calls without ever spawning a subprocess.
pub(crate) trait Effects {
    /// Run `tmux kill-pane -t <pane_id>`. Return Ok on exit 0.
    fn kill_pane(&mut self, pane_id: &str) -> std::result::Result<(), String>;
    /// Run `tmux send-keys -t <pane_id> C-c`. Return Ok on exit 0.
    fn send_ctrl_c(&mut self, pane_id: &str) -> std::result::Result<(), String>;
    fn terminate_node(&mut self, key: &TopologyNodeKey) -> std::result::Result<(), String> {
        match key {
            TopologyNodeKey::Pane(pane) => self.kill_pane(&pane.pane_id),
            TopologyNodeKey::Session(_) | TopologyNodeKey::Window(_) => {
                Err("hierarchy close is not implemented by this effect sink".into())
            }
        }
    }
    fn interrupt_pane(&mut self, key: &PaneKey) -> std::result::Result<(), String> {
        self.send_ctrl_c(&key.pane_id)
    }
    /// Pipe `text` into the system clipboard. Returns the name of the
    /// helper that succeeded (`pbcopy` / `wl-copy` / `xclip`) or
    /// `tmpfile:<path>` if all helpers were missing and we wrote a
    /// fallback file. `Err()` when even the fallback failed.
    fn copy_to_clipboard(&mut self, text: &str) -> std::result::Result<String, String>;
    /// Send `text` to `pane_id` as literal terminal input, then press
    /// Enter after a short grace delay. Return Ok only if both tmux
    /// calls succeed.
    fn send_prompt(&mut self, pane_id: &str, text: &str) -> std::result::Result<(), String>;
    fn send_prompt_to(&mut self, pane: &PaneKey, text: &str) -> std::result::Result<(), String> {
        self.send_prompt(&pane.pane_id, text)
    }
    /// Create or reuse one managed session/window and start an agent pane.
    /// The explicit name is the window/ticket id; otherwise cwd derives it.
    fn spawn_work(
        &mut self,
        dir: &str,
        launch: &str,
        work: Option<&str>,
    ) -> std::result::Result<String, String>;
    fn spawn_at(
        &mut self,
        target: &TopologyNodeKey,
        dir: &str,
        name: &str,
        agent_label: &str,
        launch: &str,
    ) -> std::result::Result<String, String> {
        let _ = target;
        self.spawn_work(dir, launch, (!name.is_empty()).then_some(name))
            .map(|result| format!("{result} ({agent_label})"))
    }
}

/// Real-world `Effects` impl — shells out to tmux and the system
/// clipboard helpers. Unit tests use a recorder stub instead.
pub(crate) struct RealEffects;

struct SpawnWorkPlan {
    created_workspace: bool,
    created_work: bool,
    args: Vec<String>,
}

fn plan_spawn_work(
    cwd: &Path,
    workspace: &str,
    work: &str,
    launch: &str,
) -> std::result::Result<SpawnWorkPlan, String> {
    let existing_workspace =
        crate::tmux_work::find_workspace(workspace).map_err(|error| error.to_string())?;
    if let Some(existing) =
        crate::tmux_work::find_work_in(work, Some(workspace)).map_err(|error| error.to_string())?
    {
        let existing_cwd = muxa::paths::canonicalize(&existing.cwd)
            .map_err(|error| format!("resolve {}: {error}", existing.cwd.display()))?;
        if existing_cwd != cwd {
            return Err(format!(
                "work {} already uses {}; requested {}",
                existing.work,
                existing_cwd.display(),
                cwd.display()
            ));
        }
        return Ok(SpawnWorkPlan {
            created_workspace: false,
            created_work: false,
            args: vec![
                "split-window".to_string(),
                "-h".into(),
                "-d".into(),
                "-P".into(),
                "-F".into(),
                "#{pane_id}".into(),
                "-t".into(),
                existing.window,
                "-c".into(),
                cwd.display().to_string(),
                launch.to_string(),
            ],
        });
    }

    let window_name =
        crate::tmux_work::window_name_for_work(work).map_err(|error| error.to_string())?;
    if let Some(existing) = existing_workspace {
        return Ok(SpawnWorkPlan {
            created_workspace: false,
            created_work: true,
            args: vec![
                "new-window".to_string(),
                "-d".into(),
                "-P".into(),
                "-F".into(),
                "#{pane_id}".into(),
                "-t".into(),
                existing.session_id,
                "-n".into(),
                window_name,
                "-c".into(),
                cwd.display().to_string(),
                launch.to_string(),
            ],
        });
    }

    let session = crate::tmux_work::session_name_for_workspace(workspace)
        .map_err(|error| error.to_string())?;
    Ok(SpawnWorkPlan {
        created_workspace: true,
        created_work: true,
        args: vec![
            "new-session".to_string(),
            "-d".into(),
            "-P".into(),
            "-F".into(),
            "#{pane_id}".into(),
            "-s".into(),
            session,
            "-n".into(),
            window_name,
            "-c".into(),
            cwd.display().to_string(),
            launch.to_string(),
        ],
    })
}

/// `tmux new-window` argv for the `c` binding: one plain shell window in
/// `session_id`, named by tmux exactly as `prefix + c` would name it, and
/// reported back as `@window %pane` so the caller can attach to it without
/// re-listing the server.
///
/// Deliberately no `-d`: `prefix + c` leaves the new window current, and this
/// binding is that keystroke with the attach round trip removed.
pub(super) fn plan_new_window(session_id: &str, cwd: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "new-window".to_string(),
        "-d".into(),
        "-P".into(),
        "-F".into(),
        "#{window_id} #{pane_id}".into(),
        "-t".into(),
        session_id.to_string(),
    ];
    // tmux inherits *watch's* cwd, not the session's, so an unset `-c` opens
    // the window wherever the console happens to live. A recorded directory
    // that has since been removed would fail the whole command, so a stale
    // one is dropped rather than passed on.
    if let Some(dir) = cwd.filter(|dir| Path::new(dir).is_dir()) {
        args.push("-c".into());
        args.push(dir.to_string());
    }
    args
}

/// Turn the `-P -F` line [`plan_new_window`] asks for into the key of the pane
/// the new window opened with. The session is the one we targeted: tmux only
/// reports ids local to that server, and a bare `@N`/`%N` is not an identity.
pub(super) fn parse_new_window_target(
    session: &SessionKey,
    reported: &str,
) -> std::result::Result<PaneKey, String> {
    let mut fields = reported.split_whitespace();
    match (fields.next(), fields.next()) {
        (Some(window_id), Some(pane_id))
            if window_id.starts_with('@') && pane_id.starts_with('%') =>
        {
            Ok(PaneKey {
                window: WindowKey {
                    session: session.clone(),
                    window_id: window_id.to_string(),
                },
                pane_id: if session.endpoint.host == muxa::HostKind::Rmux {
                    format!("rmux:{pane_id}")
                } else {
                    pane_id.to_string()
                },
            })
        }
        _ => Err(format!("tmux reported no new window ({reported:?})")),
    }
}

/// Run the `c` binding and hand back the pane to attach to.
pub(super) fn create_window(
    session: &SessionKey,
    cwd: Option<&str>,
) -> std::result::Result<PaneKey, String> {
    if !crate::mux_control::supported(session.endpoint.host) {
        return Err(format!(
            "{} window creation is not supported",
            session.endpoint.host
        ));
    }
    let args = plan_new_window(&session.session_id, cwd);
    let output = crate::mux_control::command(&session.endpoint)?
        .args(&args)
        .output()
        .map_err(|error| format!("tmux: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    let reported = String::from_utf8_lossy(&output.stdout);
    let line = reported
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with('@'))
        .ok_or_else(|| "tmux returned no window id".to_string())?;
    parse_new_window_target(session, line)
}

impl Effects for RealEffects {
    fn spawn_at(
        &mut self,
        target: &TopologyNodeKey,
        dir: &str,
        name: &str,
        agent_label: &str,
        launch: &str,
    ) -> std::result::Result<String, String> {
        let endpoint = match target {
            TopologyNodeKey::Session(session) => &session.endpoint,
            TopologyNodeKey::Window(window) => &window.session.endpoint,
            TopologyNodeKey::Pane(pane) => &pane.window.session.endpoint,
        };
        if !crate::mux_control::supported(endpoint.host) {
            return Err(format!("{} topology spawn is not supported", endpoint.host));
        }
        let cwd =
            muxa::paths::canonicalize(dir).map_err(|error| format!("resolve {dir}: {error}"))?;
        let cwd = cwd.display().to_string();
        let args = match target {
            TopologyNodeKey::Session(session) => {
                if name.trim().is_empty() {
                    return Err("a new window needs a name".into());
                }
                vec![
                    "new-window".to_string(),
                    "-d".into(),
                    "-P".into(),
                    "-F".into(),
                    "#{pane_id}".into(),
                    "-t".into(),
                    session.session_id.clone(),
                    "-n".into(),
                    name.to_string(),
                    "-c".into(),
                    cwd,
                    launch.to_string(),
                ]
            }
            TopologyNodeKey::Window(window) => vec![
                "split-window".to_string(),
                "-h".into(),
                "-d".into(),
                "-P".into(),
                "-F".into(),
                "#{pane_id}".into(),
                "-t".into(),
                format!("{}:{}", window.session.session_id, window.window_id),
                "-c".into(),
                cwd,
                launch.to_string(),
            ],
            TopologyNodeKey::Pane(pane) => vec![
                "split-window".to_string(),
                "-h".into(),
                "-d".into(),
                "-P".into(),
                "-F".into(),
                "#{pane_id}".into(),
                "-t".into(),
                format!(
                    "{}:{}",
                    pane.window.session.session_id, pane.window.window_id
                ),
                "-c".into(),
                cwd,
                launch.to_string(),
            ],
        };
        let output = crate::mux_control::command(endpoint)?
            .args(&args)
            .output()
            .map_err(|error| format!("tmux: {error}"))?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
        }
        let pane = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .find(|line| line.starts_with('%'))
            .ok_or_else(|| "tmux returned no pane id".to_string())?
            .to_string();
        crate::mux_control::run(
            endpoint,
            &["set-option", "-p", "-t", &pane, "@muxa_agent", agent_label],
        )
        .and_then(|()| {
            crate::mux_control::run(
                endpoint,
                &["set-option", "-p", "-t", &pane, "@muxa_managed_agent", "1"],
            )
        })
        .map_err(|error| format!("record pane metadata: {error}"))?;
        Ok(format!("{} pane {pane}", topology_key_label(target)))
    }

    fn terminate_node(&mut self, key: &TopologyNodeKey) -> std::result::Result<(), String> {
        let (endpoint, command, target) = match key {
            TopologyNodeKey::Session(session) => (
                &session.endpoint,
                "kill-session",
                session.session_id.as_str(),
            ),
            TopologyNodeKey::Window(window) => (
                &window.session.endpoint,
                "kill-window",
                window.window_id.as_str(),
            ),
            TopologyNodeKey::Pane(pane) => (
                &pane.window.session.endpoint,
                "kill-pane",
                pane.pane_id.as_str(),
            ),
        };
        if !crate::mux_control::supported(endpoint.host) {
            return Err(format!(
                "{} hierarchy close is not supported",
                endpoint.host
            ));
        }
        crate::mux_control::run(
            endpoint,
            &[
                command,
                "-t",
                target.strip_prefix("rmux:").unwrap_or(target),
            ],
        )
    }

    fn interrupt_pane(&mut self, key: &PaneKey) -> std::result::Result<(), String> {
        let endpoint = &key.window.session.endpoint;
        if !crate::mux_control::supported(endpoint.host) {
            return Err(format!("{} pane interrupt is not supported", endpoint.host));
        }
        crate::mux_control::run(
            endpoint,
            &[
                "send-keys",
                "-t",
                key.pane_id.strip_prefix("rmux:").unwrap_or(&key.pane_id),
                "C-c",
            ],
        )
    }

    fn kill_pane(&mut self, pane_id: &str) -> std::result::Result<(), String> {
        run_status("tmux", &["kill-pane", "-t", pane_id])
    }

    fn send_ctrl_c(&mut self, pane_id: &str) -> std::result::Result<(), String> {
        run_status("tmux", &["send-keys", "-t", pane_id, "C-c"])
    }

    fn send_prompt(&mut self, pane_id: &str, text: &str) -> std::result::Result<(), String> {
        match crate::mux_control::ambient_rmux() {
            Some(rmux) => send_prompt_to_backend(&rmux, None, pane_id, text),
            None => send_prompt_to_backend(&muxa::TmuxBackend::new(), None, pane_id, text),
        }
    }

    fn send_prompt_to(&mut self, pane: &PaneKey, text: &str) -> std::result::Result<(), String> {
        let endpoint = &pane.window.session.endpoint;
        let backend = muxa::active_backends()
            .into_iter()
            .find(|b| b.kind() == endpoint.host)
            .ok_or_else(|| format!("{} backend is not active", endpoint.host))?;
        send_prompt_to_backend(
            backend.as_ref(),
            Some(&endpoint.socket),
            &pane.pane_id,
            text,
        )
    }

    fn spawn_work(
        &mut self,
        dir: &str,
        launch: &str,
        explicit_work: Option<&str>,
    ) -> std::result::Result<String, String> {
        let base = explicit_work.map_or_else(|| session_base_name(dir), str::to_string);
        let work = crate::tmux_work::normalize_work_id(&base).map_err(|error| error.to_string())?;
        let cwd =
            muxa::paths::canonicalize(dir).map_err(|error| format!("resolve {dir}: {error}"))?;
        let workspace =
            crate::tmux_work::workspace_id_for_cwd(&cwd).map_err(|error| error.to_string())?;
        let plan = plan_spawn_work(&cwd, &workspace, &work, launch)?;
        let output = crate::mux_control::ambient_command_with_args(&plan.args)
            .map_err(|error| error.to_string())?
            .output()
            .map_err(|error| format!("tmux: {error}"))?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
        }
        let pane = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .find(|line| line.starts_with('%'))
            .ok_or_else(|| "tmux returned no pane id".to_string())?
            .to_string();
        let agent = launch.split_whitespace().next().unwrap_or("unknown");
        let window =
            crate::tmux_work::window_id_for_pane(&pane).map_err(|error| error.to_string())?;
        let marked = (|| {
            if plan.created_workspace {
                // From the pane, so the mark lands on the session the pane is
                // actually in — a session *name* can resolve to a neighbour.
                let session_id = crate::tmux_work::session_id_for_pane(&pane)?;
                crate::tmux_work::mark_workspace(&session_id, &workspace, &cwd)?;
            }
            if plan.created_work {
                crate::tmux_work::mark_work(&window, &workspace, &work, &cwd)?;
            }
            crate::tmux_work::mark_agent(
                &pane,
                agent,
                Some(&workspace),
                Some(&work),
                None,
                None,
                // No alias here, so the socket is never read — but passing
                // the real one keeps the contract honest if that changes.
                None,
                None,
                &muxa::paths::default_socket(),
            )
        })();
        if let Err(error) = marked {
            crate::tmux_work::cleanup_pane(&pane);
            return Err(format!("record muxa tmux metadata: {error}"));
        }
        Ok(format!("{workspace}/{work}"))
    }

    fn copy_to_clipboard(&mut self, text: &str) -> std::result::Result<String, String> {
        // Backend priority — first one that's *applicable* and
        // succeeds wins. We cascade past Failed (not just
        // NotFound) so an installed-but-broken backend (e.g.
        // `xclip` present without an X server) doesn't strand the
        // user; the /tmp fallback at the bottom is the safety
        // net.
        //
        //   1. tmux load-buffer when $TMUX is set — works in pure
        //      SSH/headless because tmux owns its own paste buffer
        //      (`prefix + ]`) and on tmux 3.2+ with
        //      `set -g set-clipboard on` it transparently forwards
        //      to the host terminal's clipboard via OSC 52. Single
        //      backend that covers most of the "remote dev" case.
        //   2. pbcopy on macOS (unconditional — pbcopy is always
        //      present on Apple shells).
        //   3. wl-copy when $WAYLAND_DISPLAY is set.
        //   4. xclip / xsel when $DISPLAY is set. Pre-flight env
        //      check skips them entirely on headless hosts so we
        //      don't even produce the misleading "exit 1" the user
        //      hit before this fix.
        //   5. /tmp/muxa-clip-<ts>.txt as the last resort.
        struct Cand<'a> {
            label: &'a str,
            bin: &'a str,
            args: &'a [&'a str],
            applicable: bool,
        }

        // rmux exports `$TMUX` too, and on Windows there is no tmux to take it.
        let inside_rmux = std::env::var_os("RMUX").is_some();
        let in_tmux = std::env::var_os("TMUX").is_some() && !inside_rmux;
        let in_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        let in_x11 = std::env::var_os("DISPLAY").is_some();
        let powershell = muxa::shell::powershell().to_string_lossy().into_owned();
        let candidates: &[Cand] = &[
            // The Windows clipboard first there: it is what a desktop user
            // pastes from, while an rmux buffer only answers `prefix ]`.
            Cand {
                label: "clipboard",
                bin: &powershell,
                args: &["-NoLogo", "-NoProfile", "-Command", WINDOWS_SET_CLIPBOARD],
                applicable: cfg!(windows),
            },
            Cand {
                label: "rmux",
                bin: "rmux",
                args: &["load-buffer", "-"],
                applicable: inside_rmux,
            },
            Cand {
                label: "tmux",
                bin: "tmux",
                args: &["load-buffer", "-"],
                applicable: in_tmux,
            },
            Cand {
                label: "pbcopy",
                bin: "pbcopy",
                args: &[],
                applicable: cfg!(target_os = "macos"),
            },
            Cand {
                label: "wl-copy",
                bin: "wl-copy",
                args: &[],
                applicable: in_wayland,
            },
            Cand {
                label: "xclip",
                bin: "xclip",
                args: &["-selection", "clipboard"],
                applicable: in_x11,
            },
            Cand {
                label: "xsel",
                bin: "xsel",
                args: &["--clipboard", "--input"],
                applicable: in_x11,
            },
        ];
        for c in candidates {
            if !c.applicable {
                continue;
            }
            // Cascade past Failed AND NotFound — the user never saw
            // a successful copy, so trying the next backend is
            // strictly more useful than surfacing one backend's
            // error.
            if pipe_to_command(c.bin, c.args, text).is_ok() {
                return Ok(c.label.to_string());
            }
        }
        // Last-resort fallback: dump to the temp dir so the user can `cat` it.
        let path = std::env::temp_dir().join(format!(
            "muxa-clip-{}.txt",
            OffsetDateTime::now_utc().unix_timestamp()
        ));
        let path = path.display().to_string();
        std::fs::write(&path, text).map_err(|e| format!("write {path}: {e}"))?;
        Ok(format!("tmpfile:{path}"))
    }
}

/// `PowerShell` reads its standard input in the console code page unless told
/// otherwise, which mangles anything outside ASCII — Korean included.
const WINDOWS_SET_CLIPBOARD: &str = "[Console]::InputEncoding = [Text.Encoding]::UTF8; \
     Set-Clipboard -Value ([Console]::In.ReadToEnd())";
pub(super) const WINDOWS_GET_CLIPBOARD: &str =
    "[Console]::OutputEncoding = [Text.Encoding]::UTF8; \
     $text = Get-Clipboard -Raw; if ($text) { [Console]::Out.Write($text) }";

fn send_prompt_to_backend(
    backend: &dyn muxa::PaneBackend,
    socket: Option<&str>,
    pane: &str,
    text: &str,
) -> std::result::Result<(), String> {
    if !backend.send_prompt_text_on(socket, pane, text) {
        return Err("prompt input failed".into());
    }
    std::thread::sleep(muxa::backend::prompt_submit_grace(pane));
    if !backend.send_text_on(socket, pane, "\r") {
        return Err("prompt text was sent but Enter failed; do not resend text".into());
    }
    Ok(())
}

/// Result of attempting to spawn-and-pipe to a clipboard helper.
enum PipeErr {
    /// The binary itself wasn't on PATH — caller should try the next.
    NotFound,
    /// The binary ran but returned non-zero, or stdin write failed.
    /// The String describes the failure for ad-hoc logging; current
    /// callers cascade past `Failed` to the next backend so the
    /// payload isn't surfaced anywhere user-visible — but it stays
    /// for debug traces and future use.
    #[allow(dead_code)]
    Failed(String),
}

fn pipe_to_command(bin: &str, args: &[&str], text: &str) -> std::result::Result<(), PipeErr> {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(PipeErr::NotFound),
        Err(e) => return Err(PipeErr::Failed(format!("{bin}: {e}"))),
    };
    if let Some(mut stdin) = child.stdin.take() {
        if let Err(e) = stdin.write_all(text.as_bytes()) {
            return Err(PipeErr::Failed(format!("{bin} stdin: {e}")));
        }
    }
    match child.wait() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(PipeErr::Failed(format!(
            "{bin} exited with {}",
            s.code().map_or_else(|| "signal".into(), |c| c.to_string())
        ))),
        Err(e) => Err(PipeErr::Failed(format!("{bin}: {e}"))),
    }
}

fn run_status(bin: &str, args: &[&str]) -> std::result::Result<(), String> {
    let mut command = if bin == "tmux" {
        crate::mux_control::ambient_command_with_args(args).map_err(|error| error.to_string())?
    } else {
        let mut command = Command::new(bin);
        command.args(args);
        command
    };
    match command.status() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!(
            "{bin} {} exited with {}",
            args.join(" "),
            s.code().map_or_else(|| "signal".into(), |c| c.to_string())
        )),
        Err(e) => Err(format!("{bin}: {e}")),
    }
}

/// Run one effect per pane, once each, and say how many of the intended
/// agents it reached. The denominator is everything the operator was shown:
/// resolved panes plus `unavailable` ones, which count as failures. Any
/// failure makes the outcome an error naming the server and pane. Nothing is
/// retried: a write that may have landed must not be typed twice.
pub(super) fn many_pane_outcome(
    panes: &[PaneKey],
    unavailable: &[String],
    verb: &str,
    mut run: impl FnMut(&PaneKey) -> std::result::Result<(), String>,
) -> ActionOutcome {
    let mut failures: Vec<String> = unavailable
        .iter()
        .map(|label| format!("{label}: no longer open"))
        .collect();
    let mut seen = Vec::new();
    let mut done = 0usize;
    for pane in panes {
        if seen.contains(&pane) {
            continue;
        }
        seen.push(pane);
        match run(pane) {
            Ok(()) => done += 1,
            Err(error) => failures.push(format!("{}: {error}", pane_target_label(pane))),
        }
    }
    let intended = seen.len() + unavailable.len();
    if failures.is_empty() {
        ActionOutcome::Ok(format!("✔ {verb} {done}/{intended} Work agents"))
    } else {
        ActionOutcome::Err(format!(
            "✗ {verb} {done}/{intended} Work agents · {}",
            failures.join("; ")
        ))
    }
}

/// `socket:pane` — a pane id alone is ambiguous across servers.
pub(super) fn pane_target_label(pane: &PaneKey) -> String {
    format!("{}:{}", pane.window.session.endpoint.socket, pane.pane_id)
}

/// Run a [`QuickAction`] against an [`Effects`] sink and return what
/// the run loop should surface in the footer. Pure with respect to UI
/// state — the caller mutates `App` based on the returned outcome.
pub(crate) fn dispatch_quick_action(action: QuickAction, fx: &mut dyn Effects) -> ActionOutcome {
    match action {
        QuickAction::TerminateNode(key) => match fx.terminate_node(&key) {
            Ok(()) => ActionOutcome::Ok(format!("✔ closed {}", topology_key_label(&key))),
            Err(error) => ActionOutcome::Err(format!("✗ close failed: {error}")),
        },
        QuickAction::InterruptPane(key) => match fx.interrupt_pane(&key) {
            Ok(()) => ActionOutcome::Ok(format!(
                "✔ interrupted {}",
                topology_key_label(&TopologyNodeKey::Pane(key))
            )),
            Err(error) => ActionOutcome::Err(format!("✗ interrupt failed: {error}")),
        },
        QuickAction::KillPane(pane_id) => match fx.kill_pane(&pane_id) {
            Ok(()) => ActionOutcome::Ok(format!("✔ killed pane {pane_id}")),
            Err(e) => ActionOutcome::Err(format!("✗ kill-pane failed: {e}")),
        },
        QuickAction::AbortTurn(pane_id) => match fx.send_ctrl_c(&pane_id) {
            Ok(()) => ActionOutcome::Ok(format!("✔ sent Ctrl-C to {pane_id}")),
            Err(e) => ActionOutcome::Err(format!("✗ abort failed: {e}")),
        },
        QuickAction::CopyPrompt(text) => match fx.copy_to_clipboard(&text) {
            Ok(via) if via.starts_with("tmpfile:") => {
                let path = via.trim_start_matches("tmpfile:");
                ActionOutcome::Ok(format!(
                    "✔ wrote prompt to {path} (no clipboard tool found)"
                ))
            }
            Ok(via) => ActionOutcome::Ok(format!("✔ copied prompt via {via}")),
            Err(e) => ActionOutcome::Err(format!("✗ copy failed: {e}")),
        },
        QuickAction::SpawnWork {
            dir,
            agent_label,
            launch,
            work,
        } => match fx.spawn_work(&dir, &launch, work.as_deref()) {
            Ok(name) => ActionOutcome::Ok(format!(
                "started {agent_label} in session/window {name} — Enter on its row to attach"
            )),
            Err(e) => ActionOutcome::Err(format!("spawn failed: {e}")),
        },
        QuickAction::SpawnAt {
            target,
            dir,
            name,
            agent_label,
            launch,
        } => match fx.spawn_at(&target, &dir, &name, agent_label, &launch) {
            Ok(result) => ActionOutcome::Ok(format!("started {agent_label} in {result}")),
            Err(error) => ActionOutcome::Err(format!("spawn failed: {error}")),
        },
        QuickAction::SendPrompt { pane_id, text } => match fx.send_prompt(&pane_id, &text) {
            Ok(()) => ActionOutcome::Ok(format!("✔ sent prompt to {pane_id}")),
            Err(e) => ActionOutcome::Err(format!("✗ send failed: {e}")),
        },
        QuickAction::SendPromptTo { pane, text } => match fx.send_prompt_to(&pane, &text) {
            Ok(()) => ActionOutcome::Ok(format!(
                "✔ sent prompt to {}",
                topology_key_label(&TopologyNodeKey::Pane(pane))
            )),
            Err(error) => ActionOutcome::Err(format!("✗ send failed: {error}")),
        },
        QuickAction::SendPromptToMany {
            panes,
            unavailable,
            text,
        } => many_pane_outcome(&panes, &unavailable, "prompted", |pane| {
            fx.send_prompt_to(pane, &text)
        }),
        QuickAction::InterruptPanes { panes, unavailable } => {
            many_pane_outcome(&panes, &unavailable, "interrupted", |pane| {
                fx.interrupt_pane(pane)
            })
        }
        QuickAction::ShowHelp => ActionOutcome::HelpToggled,
    }
}

/// Push an [`ActionOutcome`] back into `App` state — sets the footer
/// hint with the appropriate severity level, or toggles `help_open`
/// for `HelpToggled`. Centralised so the run loop and tests can both
/// drive it.
pub(crate) fn apply_outcome_to_app(app: &mut App, outcome: ActionOutcome) {
    match outcome {
        ActionOutcome::Ok(msg) => app.set_hint(msg, HintLevel::Ok),
        ActionOutcome::Err(msg) => app.set_hint(msg, HintLevel::Err),
        ActionOutcome::HelpToggled => {
            app.help_open = !app.help_open;
        }
    }
}

/// The lines of text rendered into the `?` help overlay. Returned as
/// a Vec so the snapshot test can assert on the exact contents
/// without going through ratatui. Keep this in sync with the actual
/// keybinding matrix in `handle_event` — the overlay is the user's
/// canonical reference.
pub(crate) fn help_overlay_text() -> Vec<&'static str> {
    vec![
        "Filter & navigation",
        "  /               filter rows; any key types into it once armed",
        "  Backspace/C-W   edit filter / delete previous word",
        "  Ctrl-U / Esc    clear filter; Esc again backs out / quits",
        "  ↑/↓ · j/k       move siblings in focus mode; visible nodes otherwise",
        "  ←/→ · h/l       parent/child in focus mode; collapse/expand otherwise",
        "  gg/G · Home/End first / last selectable row",
        "  PgUp/PgDn       page; Ctrl-U/Ctrl-D half page",
        "  Enter          attach via active window/pane or exact pane",
        // One line, not two: the overlay is sized to its line count and
        // clipped by the terminal, so a row added here pushes the last
        // binding off a short screen.
        "  C / n / w / R  shell window / agent pane / work up / rename the row",
        "  a/A · Ctrl-E/n ask / conversations · new mode/draft · Enter read · d/D delete",
        "",
        "Commands & inspection",
        "  :              command palette (Tab completes)",
        "  o / Alt-P      open preview overlay",
        "  Alt-I / Alt-E  inspector / persistent event inbox",
        "  |              cycle list/inspector split (50/50 → 70/30 → 30/70)",
        "  Alt-A          attention-only filter",
        "  [/] · f/c      (in preview) agent / geometry / content",
        "  Enter          (in preview) jump to pinned pane",
        "  m / M / Alt-M  message / mailbox / move pane or window; Space marks",
        "  Alt-1/2/3 · W  screen topology / collab / board · W is the work table",
        "  v              (in collab) toggle table / sequence history",
        "  i / e          (in mailbox) claim inbox / reply",
        "Sorting",
        "  Alt-S/L/D/T    sibling name / latest / duration / state",
        "State markers",
        match crate::icon_set() {
            IconSet::Unicode => {
                "  ● working  ▶ input  ◆ choice  ■ error  ○ idle  ◌ starting  × stopped"
            }
            IconSet::Ascii => {
                "  * working  > input  ? choice  ! error  o idle  ~ starting  x stopped"
            }
        },
        "  TREE: ◇ subagent  ▸ shell  + process; helper-only trees hidden",
        "",
        "Quick actions (act on selected node)",
        "  Alt-C          copy last prompt to clipboard",
        "  Alt-K          close session/window/pane (confirm popup)",
        "  Alt-X          interrupt selected agent pane (confirm popup)",
        "  r/Ctrl-R/Alt-R force refresh while browsing",
    ]
}
