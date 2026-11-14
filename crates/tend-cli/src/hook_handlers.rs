use super::{
    claude, run_hook, tmux_work, watch, AgentKind, AntigravityAdapter, ClaudeAdapter, Client,
    CodexAdapter, Context, HookCmd, Read, Result, Stdio, Write,
};

/// Hook commands are invoked on the agent's critical path (every prompt,
/// every tool call). If the daemon is down we MUST NOT block or fail — a
/// best-effort ingest with a stderr warning keeps the agent healthy.
///
/// A failed ingest is appended to the bounded hook spool instead of being
/// dropped: `tendd` replays what is still current when it next starts, and
/// `tend doctor` reports the failures (a protocol mismatch included).
pub(super) async fn best_effort_ingest(client: &Client, ev: &tend::event::AgentEvent) {
    let launch_id = tend::adapters::hook::launch_claim(ev.id().kind);
    if let Err(e) = client.ingest_with_launch_claim(ev, launch_id.clone()).await {
        tracing::debug!(error = %e, "tend ingest failed; spooling the event");
        let headless_ask = std::env::var_os(tend::ask::ASK_ENV_MARKER).is_some();
        if !tend::hook_spool::spoolable(ev, headless_ask) {
            return;
        }
        let Some(path) = tend::hook_spool::default_spool_file() else {
            return;
        };
        let entry = tend::hook_spool::SpoolEntry::new(ev, launch_id, &e);
        if let Err(error) = tend::hook_spool::append(&path, &entry) {
            tracing::debug!(%error, "hook spool append failed");
        }
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
pub(super) async fn best_effort_default_alias(client: &Client, ev: &tend::event::AgentEvent) {
    let Some((pane, base)) = alias_target(ev) else {
        return;
    };
    // The answer for every session start after the first, and one tmux call
    // rather than an IPC round-trip on the agent's critical path.
    if tmux_work::pane_is_named(&pane).unwrap_or(true) {
        return;
    }
    let request = tend::collaboration::HandleRequest::Mint {
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
pub(super) fn alias_target(ev: &tend::event::AgentEvent) -> Option<(String, &'static str)> {
    let tend::event::AgentEvent::Started { id, .. } = ev else {
        return None;
    };
    // `Task` rows are pid-tracked subagents rather than panes, and an
    // unrecognised runtime has no name worth minting from.
    if matches!(id.kind, AgentKind::Task | AgentKind::Unknown) {
        return None;
    }
    // No second-guessing the hook layer's pane resolution. `run_hook` has
    // already tried the host env vars and walked the parent-pid chain, and
    // it deliberately reports `None` for a tend-owned PTY surface: that
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
    let mut child = tend::shell::script_command(cmd)
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
            if let Some(output) = ingest_and_deliver(client, &ev, &event).await {
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
                    let pane = tend::adapters::hook::host_pane_env();
                    let mut ev = claude::statusline_heartbeat(input, pane);
                    tend::adapters::hook::stamp_pane_endpoint(&mut ev);
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
            let pane = tend::adapters::hook::host_pane_env();
            let mut ev = claude::statusline_heartbeat(input, pane);
            tend::adapters::hook::stamp_pane_endpoint(&mut ev);
            best_effort_ingest(client, &ev).await;
        }
        HookCmd::Codex { event } => {
            let ev = run_hook::<CodexAdapter, _>(&event, &mut std::io::stdin())?;
            if let Some(output) = ingest_and_deliver(client, &ev, &event).await {
                println!("{output}");
            }
        }
        HookCmd::Gemini { .. } => {
            // Drain stdin so the Gemini CLI never sees a broken pipe.
            let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
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
        HookCmd::Copilot { event } => copilot_hook(client, &event).await,
        HookCmd::Opencode { event } => {
            // Fail-open like agy: the plugin awaits this process from inside
            // opencode, so a payload tend cannot use is logged and dropped
            // rather than surfaced as a failing command.
            let mut text = String::new();
            std::io::stdin().read_to_string(&mut text)?;
            let place = tend::adapters::hook::HookPlace::resolve(AgentKind::Opencode);
            let parsed = tend::adapters::opencode::check_flag(&event)
                .and_then(|()| tend::adapters::opencode::parse(&text, place.pane.clone()));
            match parsed {
                Ok(Some(ev)) => {
                    let ev = place.bind(ev);
                    // Only the plugin knows a turn ended cleanly; asking
                    // tendd after Esc or a failure would spend the
                    // request's limited Stop offers on output it discards.
                    // At that boundary the offers are asked for before the
                    // idle is recorded, as `ingest_and_deliver` does at a
                    // Claude/Codex Stop, so the waker cannot type a wake for
                    // the same request first.
                    let output = if tend::adapters::opencode::is_turn_boundary(&text) {
                        opencode_turn_delivery(client, &ev).await
                    } else {
                        None
                    };
                    best_effort_ingest(client, &ev).await;
                    best_effort_default_alias(client, &ev).await;
                    if let Some(output) = output {
                        println!("{output}");
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::debug!(error = %e, event, "opencode hook payload ignored"),
            }
        }
    }
    Ok(())
}

/// `tend hook copilot`. Fail-open like agy: a payload tend cannot use is
/// logged and dropped, never surfaced as output or a failing exit, since
/// Copilot reads a tool hook's stdout and status as its verdict.
async fn copilot_hook(client: &Client, event: &str) {
    let mut text = String::new();
    if std::io::stdin().read_to_string(&mut text).is_err() {
        return;
    }
    let place = tend::adapters::hook::HookPlace::resolve(AgentKind::Copilot);
    match tend::adapters::copilot::parse(event, &text, place.pane.clone()) {
        Ok(Some(ev)) => {
            let ev = place.bind(ev);
            best_effort_ingest(client, &ev).await;
            best_effort_default_alias(client, &ev).await;
        }
        Ok(None) => {}
        Err(e) => tracing::debug!(error = %e, event, "copilot hook payload ignored"),
    }
}

/// Record the hook event and ask for turn-boundary offers. At `stop` the
/// offers are asked for first, while the session still reads as working:
/// recording the stop first would mark it idle and let the waker type a wake
/// for the same request before the hook could take it. Elsewhere the event
/// is recorded first, so a starting session is known before it asks.
async fn ingest_and_deliver(
    client: &Client,
    ev: &tend::event::AgentEvent,
    event: &str,
) -> Option<String> {
    if event == "stop" {
        let output = turn_boundary_delivery(client, ev, event).await;
        best_effort_ingest(client, ev).await;
        best_effort_default_alias(client, ev).await;
        return output;
    }
    best_effort_ingest(client, ev).await;
    best_effort_default_alias(client, ev).await;
    turn_boundary_delivery(client, ev, event).await
}

/// Budget for asking tendd for turn-boundary offers. This runs inside the
/// agent's Stop / `UserPromptSubmit` hook, so a slow daemon must cost an offer
/// one boundary of delay, never the agent its turn. Nothing is consumed by
/// the call, so a timeout loses nothing.
const TURN_BOUNDARY_IPC_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);

/// Hook output offering queued user-authorized requests to this session at a
/// turn boundary, or `None` when there is nothing to offer.
///
/// tendd never types such a request into a human primary's terminal, where
/// it could merge with an unsent draft. The provider's own hook carries an
/// offer instead: at `stop` the turn continues with it (Stop `decision:
/// block`), at `user_prompt_submit` it rides along with the user's prompt as
/// additional context. The model claims each request by id before acting, so
/// nothing is consumed until it does.
pub(super) async fn turn_boundary_delivery(
    client: &Client,
    ev: &tend::event::AgentEvent,
    event: &str,
) -> Option<String> {
    let boundary = match event {
        "stop" => tend::collaboration::turn_offer::TurnBoundaryEvent::Stop,
        "user_prompt_submit" => tend::collaboration::turn_offer::TurnBoundaryEvent::PromptSubmit,
        "session_start" => tend::collaboration::turn_offer::TurnBoundaryEvent::SessionStart,
        _ => return None,
    };
    let origin = turn_boundary_origin(ev.id())?;
    let offers = tokio::time::timeout(
        TURN_BOUNDARY_IPC_TIMEOUT,
        client.collaboration_turn_boundary_offer(&origin, boundary),
    )
    .await
    .ok()?
    .ok()?;
    turn_boundary_output(event, &offers)
}

/// opencode's turn boundary: a confirmed idle (`session.idle`, from either
/// plugin). The offers Claude Code's Stop hook would carry in `reason` are
/// printed as `{"prompt": TEXT}`, and the plugin hands TEXT to opencode's
/// own prompt API for this session, so nothing is typed into the terminal.
/// A request is not claimed until the model claims it by id, so a prompt
/// the plugin drops (the user typed first) leaves it queued; the attempt
/// still counts toward the request's Stop offers, which is why the caller
/// asks only at a boundary the plugin flagged as clean.
async fn opencode_turn_delivery(client: &Client, ev: &tend::event::AgentEvent) -> Option<String> {
    if !matches!(
        ev,
        tend::event::AgentEvent::TurnStopped {
            idle_confirmed: true,
            ..
        }
    ) {
        return None;
    }
    let output = turn_boundary_delivery(client, ev, "stop").await?;
    opencode_prompt_output(&output)
}

/// `{"decision": "block", "reason": TEXT}` (the Stop-hook shape) to the
/// opencode plugins' `{"prompt": TEXT}`.
fn opencode_prompt_output(stop_output: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(stop_output).ok()?;
    let text = value.get("reason")?.as_str()?.trim();
    (!text.is_empty()).then(|| serde_json::json!({ "prompt": text }).to_string())
}

/// The exact session this hook speaks for. A Codex shared app-server hook has
/// no trustworthy pane (inherited pane variables may name another thread), so
/// it is addressed by session alone and tendd resolves only a live binding.
fn turn_boundary_origin(
    id: &tend::event::AgentId,
) -> Option<tend::collaboration::CollaborationOrigin> {
    if id.session_id.trim().is_empty() {
        return None;
    }
    let pane = id.pane.clone().unwrap_or_default();
    let socket = (!pane.is_empty())
        .then(|| {
            id.tmux_socket
                .as_deref()
                .map(|path| tend::backend::pane_endpoint_identity(Some(&pane), path))
        })
        .flatten();
    Some(tend::collaboration::CollaborationOrigin {
        exact_session: true,
        pane,
        socket,
        console: false,
        agent_session_id: Some(id.session_id.clone()),
    })
}

/// The few lines a starting (or compacted) session gets about what it owes
/// and awaits.
fn obligations_text(summary: &serde_json::Value) -> String {
    let list = |key: &str| {
        let section = &summary[key];
        let total = section["total"].as_u64().unwrap_or(0);
        let ids: Vec<String> = section["listed"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        let id = item["request_id"].as_str()?;
                        Some(match item["peer"].as_str() {
                            Some(peer) => format!("{id} ({peer})"),
                            None => id.to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let more = total.saturating_sub(ids.len() as u64);
        (
            total,
            if more > 0 {
                format!("{}, and {more} more", ids.join(", "))
            } else {
                ids.join(", ")
            },
        )
    };
    let mut lines =
        vec!["Tend mailbox state for this session (nothing was claimed or changed):".to_string()];
    for (key, label) in [
        ("owed_replies", "You claimed and still owe a reply to"),
        (
            "queued_for_you",
            "Queued for you (claim with tend_inbox(request_id))",
        ),
        ("waiting_on", "You are waiting for replies to"),
        ("waiting_on_person", "Waiting for the person on"),
        (
            "unread_replies",
            "Replies ready to read (tend_inbox(view=report, request_id) or tend_wait)",
        ),
    ] {
        let (total, ids) = list(key);
        if total > 0 {
            lines.push(format!("- {label}: {ids}."));
        }
    }
    lines.push(
        "Re-read an owed request with tend_inbox(view=all, request_id, include_body=true) before \
         acting; finish each with one tend_reply. Do not resend requests you are waiting on, and \
         reuse an existing question to the person instead of asking again."
            .into(),
    );
    lines.join("\n")
}

fn turn_boundary_output(event: &str, offers: &[serde_json::Value]) -> Option<String> {
    if event == "session_start" {
        let summary = offers
            .iter()
            .find(|offer| offer["offer"] == "obligations")?;
        return Some(
            serde_json::json!({"hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": obligations_text(summary),
            }})
            .to_string(),
        );
    }
    let (replies, offers): (Vec<_>, Vec<_>) = offers
        .iter()
        .cloned()
        .partition(|offer| offer["offer"] == "reply_ready");
    let (reminders, offers): (Vec<_>, Vec<_>) = offers
        .into_iter()
        .partition(|offer| offer["offer"] == "reply_reminder");
    let mut parts = Vec::new();
    if !replies.is_empty() {
        let ids: Vec<String> = replies
            .iter()
            .filter_map(|reply| {
                let id = reply["request_id"].as_str()?;
                Some(match (reply["from"].as_str(), reply["status"].as_str()) {
                    (Some(from), Some(status)) => format!("{id} ({status}, from {from})"),
                    _ => id.to_string(),
                })
            })
            .collect();
        parts.push(format!(
            "Replies to your Tend requests are ready: {}. Read each with \
             tend_inbox(view=report, request_id=ID) or tend_wait(id=ID) and verify it before \
             relying on it; do not resend those requests.",
            ids.join("; ")
        ));
    }
    if !reminders.is_empty() {
        let ids: Vec<_> = reminders
            .iter()
            .filter_map(|reminder| reminder["request_id"].as_str())
            .collect();
        parts.push(format!(
            "You claimed Tend request(s) {} and ended your turn without replying; the sender is \
             still waiting. If the work is done, reply now with tend_reply(request_id=ID, status=completed, \
             body=...) (CLI: `tend msg reply ID BODY`). If you cannot finish, reply with \
             status=blocked, declined or failed and the reason; if you are still working, post a \
             progress update with tend_update_request(request_id=ID, body=...). Do not redo the \
             work and do not reply twice. Reminders: {}",
            ids.join(", "),
            serde_json::Value::Array(reminders.clone())
        ));
    }
    if !offers.is_empty() {
        parts.push(format!(
            "Tend is offering {} request(s) addressed to this session. These are offers, not \
             claimed instructions. For each one, first claim it with tend_inbox(request_id=ID) (CLI: \
             `tend msg accept ID --json`), then act only on what the claim returns: its current body, \
             kind, work_mode, paths and contract. If the claim is refused because the request was \
             cancelled, superseded or already handled, do not act on the offered text. When the claimed \
             request expects a reply, reply exactly once. Offers: {}",
            offers.len(),
            serde_json::Value::Array(offers)
        ));
    }
    if parts.is_empty() {
        return None;
    }
    let text = parts.join("\n\n");
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
    use super::{opencode_prompt_output, turn_boundary_output};

    #[test]
    fn opencode_gets_the_stop_offer_text_as_a_prompt() {
        let offers = vec![serde_json::json!({"request_id": "req_1", "offer": "request"})];
        let stop = turn_boundary_output("stop", &offers).unwrap();
        let out: serde_json::Value =
            serde_json::from_str(&opencode_prompt_output(&stop).unwrap()).unwrap();
        let prompt = out["prompt"].as_str().unwrap();
        assert!(prompt.contains("req_1"), "{prompt}");
        assert!(prompt.contains("tend msg accept"), "{prompt}");
        assert_eq!(out.as_object().unwrap().len(), 1);
        assert!(opencode_prompt_output("{\"decision\":\"block\",\"reason\":\"  \"}").is_none());
        assert!(opencode_prompt_output("not json").is_none());
    }

    #[test]
    fn stop_continues_the_turn_and_prompt_submit_adds_context() {
        let offers = [
            serde_json::json!({"request_id": "req_1", "body": "plan the import",
                                          "offer": "claim_before_acting"}),
        ];
        let stop: serde_json::Value =
            serde_json::from_str(&turn_boundary_output("stop", &offers).unwrap()).unwrap();
        assert_eq!(stop["decision"], "block");
        let reason = stop["reason"].as_str().unwrap();
        assert!(reason.contains("plan the import"));
        assert!(reason.contains("first claim it"));
        let submit: serde_json::Value =
            serde_json::from_str(&turn_boundary_output("user_prompt_submit", &offers).unwrap())
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

    #[test]
    fn a_reply_reminder_names_the_claimed_request_and_how_to_reply() {
        let offers = [
            serde_json::json!({"request_id": "req_claimed", "offer": "reply_reminder"}),
            serde_json::json!({"request_id": "req_new", "body": "fresh",
                               "offer": "claim_before_acting"}),
        ];
        let stop: serde_json::Value =
            serde_json::from_str(&turn_boundary_output("stop", &offers).unwrap()).unwrap();
        assert_eq!(stop["decision"], "block");
        let reason = stop["reason"].as_str().unwrap();
        assert!(reason.contains("You claimed Tend request(s) req_claimed"));
        assert!(reason.contains("tend_reply(request_id=ID, status=completed"));
        assert!(reason.contains("offering 1 request(s)"));
        assert!(reason.contains("req_new"));
    }

    #[test]
    fn session_start_lists_obligations_in_a_few_lines() {
        let summary = serde_json::json!({
            "offer": "obligations",
            "owed_replies": {"total": 1, "listed": [{"request_id": "req_a", "peer": "codex@%3"}]},
            "queued_for_you": {"total": 0, "listed": []},
            "waiting_on": {"total": 7, "listed": [
                {"request_id": "req_b", "peer": "claude@%5"},
                {"request_id": "req_c", "peer": "claude@%5"}]},
            "waiting_on_person": {"total": 1, "listed": [{"request_id": "req_h"}]},
            "unread_replies": {"total": 0, "listed": []},
        });
        let output: serde_json::Value =
            serde_json::from_str(&turn_boundary_output("session_start", &[summary]).unwrap())
                .unwrap();
        assert_eq!(
            output["hookSpecificOutput"]["hookEventName"],
            "SessionStart"
        );
        let text = output["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(
            text.contains("still owe a reply to: req_a (codex@%3)."),
            "{text}"
        );
        assert!(
            text.contains("req_b (claude@%5), req_c (claude@%5), and 5 more"),
            "{text}"
        );
        assert!(text.contains("person on: req_h."), "{text}");
        assert!(!text.contains("Queued for you"), "{text}");
        assert!(text.lines().count() <= 7);
        assert!(turn_boundary_output("session_start", &[]).is_none());
    }
}
