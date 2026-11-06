use super::{
    claude, run_hook, tmux_work, watch, AgentKind, AntigravityAdapter, ClaudeAdapter, Client,
    CodexAdapter, Context, GeminiAdapter, HookCmd, OpencodeAdapter, Read, Result, Stdio, Write,
};

/// Hook commands are invoked on the agent's critical path (every prompt,
/// every tool call). If the daemon is down we MUST NOT block or fail — a
/// best-effort ingest with a stderr warning keeps the agent healthy.
pub(super) async fn best_effort_ingest(client: &Client, ev: &muxa::event::AgentEvent) {
    if let Err(e) = client.ingest(ev).await {
        tracing::debug!(error = %e, "muxa ingest failed (daemon down?)");
    }
}

/// Name the agent's pane the first time a session starts in it, so it is
/// addressable as `@claude` rather than only as `%1242`.
///
/// Gated on `Started` — the one hook event that fires once per session —
/// because everything else on this path fires per tool call, and a tmux
/// round-trip per tool call is a tax on the agent's critical path for a
/// fact that cannot have changed.
///
/// Best-effort like the ingest above, and for a stronger reason: this runs
/// inside the agent's own hook, where a non-zero exit is the agent's
/// problem. A pane stuck with `%1242` as its only handle is a worse
/// interface, not a broken agent.
pub(super) async fn best_effort_default_alias(client: &Client, ev: &muxa::event::AgentEvent) {
    let Some((pane, base)) = alias_target(ev) else {
        return;
    };
    // The answer for every session start after the first, and one tmux call
    // rather than an IPC round-trip on the agent's critical path.
    if tmux_work::pane_is_named(&pane).unwrap_or(true) {
        return;
    }
    let request = muxa::collaboration::HandleRequest::Mint {
        base: base.to_string(),
    };
    let issued = client
        .collaboration_issue_handle(&pane, None, &request, HANDLE_IPC_TIMEOUT)
        .await;
    match issued {
        Ok(Some(handle)) => match tmux_work::claim_alias(&pane, &handle) {
            Ok(Some(alias)) => tracing::debug!(pane, alias, "named pane"),
            Ok(None) => tracing::debug!(pane, handle, "pane was named first"),
            Err(e) => tracing::debug!(error = %e, pane, "could not name pane"),
        },
        // No free name, no room for the pane, or no daemon to referee. Naming
        // a pane without the arbiter is exactly what this path stopped doing,
        // so the pane keeps `%1242` until its next session start.
        Ok(None) => {}
        Err(e) => tracing::debug!(error = %e, pane, "handle refused"),
    }
}

/// Budget for the one namespace round-trip. This runs inside an agent's
/// session-start hook, so a wedged daemon must cost the pane its name rather
/// than the agent its startup.
pub(super) const HANDLE_IPC_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// The pane this event should name and the runtime to name it after, or
/// `None` when the event names nothing.
pub(super) fn alias_target(ev: &muxa::event::AgentEvent) -> Option<(String, &'static str)> {
    let muxa::event::AgentEvent::Started { id, .. } = ev else {
        return None;
    };
    // `Task` rows are pid-tracked subagents rather than panes, and an
    // unrecognised runtime has no name worth minting from.
    if matches!(id.kind, AgentKind::Task | AgentKind::Unknown) {
        return None;
    }
    // No second-guessing the hook layer's pane resolution. `run_hook` has
    // already tried the host env vars and walked the parent-pid chain, and
    // it deliberately reports `None` for a muxa-owned PTY surface: that
    // agent's shell inherits `$TMUX_PANE` from the terminal that *requested*
    // the PTY, which owns nothing. Reading the environment again here would
    // name that outer pane after the runtime running inside the PTY.
    let pane = id.pane.clone()?;
    Some((pane, watch::agent_kind_short(id.kind)))
}

/// Spawn `cmd` through the local shell (`/bin/sh -c`, `PowerShell` on
/// Windows), feed it `stdin_bytes`, stream its stdout to our stdout, and
/// return its exit code (128 + signal if killed by signal).
pub(super) fn run_forward(cmd: &str, stdin_bytes: &[u8]) -> Result<i32> {
    let mut child = muxa::shell::script_command(cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // stderr is inherited so the forwarded tool can report errors.
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to spawn forward command: {cmd}"))?;

    // Write the captured stdin in a scope so the pipe closes and the child
    // sees EOF — otherwise `npx`/`ccstatusline` would hang.
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(stdin_bytes)
            .with_context(|| "failed to write stdin to forward command")?;
    }

    if let Some(mut stdout) = child.stdout.take() {
        let mut out = std::io::stdout().lock();
        std::io::copy(&mut stdout, &mut out)
            .with_context(|| "failed to relay forward command stdout")?;
    }

    let status = child
        .wait()
        .with_context(|| "failed to wait on forward command")?;
    #[cfg(unix)]
    let code = status.code().unwrap_or_else(|| {
        use std::os::unix::process::ExitStatusExt;
        status.signal().map_or(1, |s| 128 + s)
    });
    // No signals on Windows: an exit code is always there.
    #[cfg(not(unix))]
    let code = status.code().unwrap_or(1);
    Ok(code)
}

pub(super) async fn handle_hook(client: &Client, cmd: HookCmd) -> Result<()> {
    match cmd {
        HookCmd::Claude { event } => {
            let ev = run_hook::<ClaudeAdapter, _>(&event, &mut std::io::stdin())?;
            best_effort_ingest(client, &ev).await;
            best_effort_default_alias(client, &ev).await;
            if let Some(output) = turn_boundary_delivery(client, &ev, &event).await {
                println!("{output}");
            }
        }
        HookCmd::ClaudeStatusline { forward } => {
            if let Some(cmd) = forward {
                // Forward mode: capture stdin, fire Heartbeat best-effort,
                // then tee stdin to the forwarded command and pass its
                // stdout + exit code back to our parent unchanged.
                let mut buf = Vec::new();
                std::io::stdin().read_to_end(&mut buf)?;

                // Best-effort Heartbeat: parse errors on stdin must never
                // cause the status line to fail, so log and move on.
                if let Ok(input) = serde_json::from_slice::<claude::StatusLineInput>(&buf) {
                    let pane = muxa::adapters::hook::host_pane_env();
                    let mut ev = claude::statusline_heartbeat(input, pane);
                    muxa::adapters::hook::stamp_pane_endpoint(&mut ev);
                    best_effort_ingest(client, &ev).await;
                } else {
                    tracing::debug!("claude-statusline: stdin was not valid statusline JSON");
                }

                let code = run_forward(&cmd, &buf)?;
                std::process::exit(code);
            }

            let input = claude::parse_statusline(&mut std::io::stdin())?;
            let label = input
                .model
                .as_ref()
                .and_then(|m| m.display_name.clone())
                .unwrap_or_else(|| "claude".into());
            let ctx = input
                .context_window
                .as_ref()
                .and_then(|c| c.used_percentage)
                .map(|p| format!(" · ctx {p:.0}%"))
                .unwrap_or_default();
            let cost = input
                .cost
                .as_ref()
                .and_then(|c| c.total_cost_usd)
                .map(|u| format!(" · ${u:.2}"))
                .unwrap_or_default();
            println!("{label}{ctx}{cost}");
            // Same resolution as the event hooks, so an rmux pane is stamped
            // `rmux:%N` rather than read back as a bare tmux `%N`.
            let pane = muxa::adapters::hook::host_pane_env();
            let mut ev = claude::statusline_heartbeat(input, pane);
            muxa::adapters::hook::stamp_pane_endpoint(&mut ev);
            best_effort_ingest(client, &ev).await;
        }
        HookCmd::Codex { event } => {
            let ev = run_hook::<CodexAdapter, _>(&event, &mut std::io::stdin())?;
            best_effort_ingest(client, &ev).await;
            best_effort_default_alias(client, &ev).await;
            if let Some(output) = turn_boundary_delivery(client, &ev, &event).await {
                println!("{output}");
            }
        }
        HookCmd::Gemini { event } => {
            let ev = run_hook::<GeminiAdapter, _>(&event, &mut std::io::stdin())?;
            best_effort_ingest(client, &ev).await;
            best_effort_default_alias(client, &ev).await;
        }
        HookCmd::Agy { event } => {
            // FAIL-OPEN, and deliberately unlike the other hook arms.
            //
            // agy treats a non-zero hook exit as a verdict: a `PreToolUse`
            // handler that dies takes the tool call down with it
            // ("tool call denied by pre-tool hook"). Observability must never
            // be able to block the agent, so an unparseable payload — a shape
            // change in a future agy, a truncated stdin — is logged and
            // swallowed rather than propagated to a non-zero exit.
            match run_hook::<AntigravityAdapter, _>(&event, &mut std::io::stdin()) {
                Ok(ev) => {
                    best_effort_ingest(client, &ev).await;
                    best_effort_default_alias(client, &ev).await;
                }
                Err(e) => tracing::debug!(error = %e, event, "agy hook payload ignored"),
            }
        }
        HookCmd::Opencode { event } => {
            let ev = run_hook::<OpencodeAdapter, _>(&event, &mut std::io::stdin())?;
            best_effort_ingest(client, &ev).await;
            best_effort_default_alias(client, &ev).await;
        }
    }
    Ok(())
}

/// Budget for handing user-authorized work to a human primary at its turn
/// boundary. This runs inside the agent's Stop / `UserPromptSubmit` hook, so a
/// slow daemon must cost the request one turn of delay, never the agent its
/// turn.
const TURN_BOUNDARY_IPC_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);

/// The hook output that hands queued user-authorized requests to this
/// session at a turn boundary, or `None` when there is nothing to hand over.
///
/// muxad never types such a request into a human primary's terminal, where
/// it could merge with an unsent draft. Instead the provider's own hook
/// carries it: at `stop` the turn continues with the request (Stop
/// `decision: block`), at `user_prompt_submit` it rides along with the
/// user's prompt as additional context. Nothing is written to the composer.
pub(super) async fn turn_boundary_delivery(
    client: &Client,
    ev: &muxa::event::AgentEvent,
    event: &str,
) -> Option<String> {
    if event != "stop" && event != "user_prompt_submit" {
        return None;
    }
    let requests = tokio::time::timeout(
        TURN_BOUNDARY_IPC_TIMEOUT,
        claim_turn_boundary_requests(client, ev.id()),
    )
    .await
    .ok()
    .flatten()?;
    turn_boundary_output(event, &requests)
}

async fn claim_turn_boundary_requests(
    client: &Client,
    id: &muxa::event::AgentId,
) -> Option<Vec<muxa::collaboration::CollaborationRequest>> {
    let pane = id.pane.clone()?;
    // No capability probe on the agent's critical path: an older daemon
    // never stores `user_authorized`, so it has nothing to hand over here.
    let socket = id
        .tmux_socket
        .as_deref()
        .map(|path| muxa::backend::pane_endpoint_identity(Some(&pane), path));
    let origin = muxa::collaboration::CollaborationOrigin {
        exact_session: true,
        pane,
        socket,
        console: false,
        agent_session_id: Some(id.session_id.clone()),
    };
    let incoming = client
        .collaboration_list(&origin, muxa::collaboration::RequestMailbox::Incoming)
        .await
        .ok()?;
    let mut claimed = Vec::new();
    for request in incoming.iter().filter(|request| {
        request.status == muxa::collaboration::RequestStatus::Queued
            && request.awaits_turn_boundary(&request.to)
    }) {
        if let Ok(requests) = client.collaboration_claim(&origin, &request.id).await {
            claimed.extend(requests.into_iter().filter(|r| r.id == request.id));
        }
    }
    (!claimed.is_empty()).then_some(claimed)
}

fn turn_boundary_output(
    event: &str,
    requests: &[muxa::collaboration::CollaborationRequest],
) -> Option<String> {
    if requests.is_empty() {
        return None;
    }
    let instructions: Vec<_> = requests
        .iter()
        .map(muxa::collaboration::instruction::inbox_instruction)
        .collect();
    let text = format!(
        "Muxa delivered {} request(s) the user authorized for this session. Each is already claimed for you: \
         handle it within its kind, work_mode and paths, and when expects_reply is true reply exactly once \
         (muxa_reply or `muxa msg reply REQUEST_ID`). Requests: {}",
        requests.len(),
        serde_json::Value::Array(instructions)
    );
    let output = if event == "stop" {
        serde_json::json!({"decision": "block", "reason": text})
    } else {
        serde_json::json!({"hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": text,
        }})
    };
    Some(output.to_string())
}

#[cfg(test)]
mod turn_boundary_tests {
    use super::turn_boundary_output;

    fn request() -> muxa::collaboration::CollaborationRequest {
        let mut request = crate::collaboration_output::fixture();
        request.id = "req_1".into();
        request.body = "plan the import".into();
        request.user_authorized = true;
        request
    }

    #[test]
    fn stop_continues_the_turn_and_prompt_submit_adds_context() {
        let requests = [request()];
        let stop: serde_json::Value =
            serde_json::from_str(&turn_boundary_output("stop", &requests).unwrap()).unwrap();
        assert_eq!(stop["decision"], "block");
        assert!(stop["reason"].as_str().unwrap().contains("plan the import"));
        let submit: serde_json::Value =
            serde_json::from_str(&turn_boundary_output("user_prompt_submit", &requests).unwrap())
                .unwrap();
        assert_eq!(
            submit["hookSpecificOutput"]["hookEventName"],
            "UserPromptSubmit"
        );
        assert!(submit["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("req_1"));
        assert!(turn_boundary_output("stop", &[]).is_none());
    }
}
