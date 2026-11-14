# Prompt delivery and submission

Tend sends prompt text and the submit key as separate operations. Native tmux
and rmux use `load-buffer` followed by `paste-buffer -p -r` for every nonempty
prompt, including a one-line `continue`. The terminal supplies bracketed-paste
start/end markers when the application enables that mode. Enter follows the end
marker in the same PTY input stream; these hosts no longer wait an arbitrary
120ms between text and Enter. The `-r` option preserves the prompt's LF
line breaks rather than converting them to CR, keeping multiline text intact.

This avoids Codex's heuristic for detecting simulated typing as a paste burst.
In Codex CLI 0.162.1, the heuristic suppresses Enter through a 120ms window;
explicit paste clears that state. See the upstream
[paste burst implementation](https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/tui/src/bottom_pane/paste_burst.rs).

Raw control keys still use `send_text_on`, without a paste wrapper. Empty prompt
text is a no-op before an Enter-only operation. Hosts without an explicit paste
transport retain a 250ms best-effort fallback; that delay cannot prove submission.
The fallback and host selection live in `crates/tend/src/backend/mod.rs`, rather
than an automation rule or a user-editable timing setting.

An IPC `send_prompt` request may carry `force` (`send_prompt_state_gate_v1`).
`force: false` refuses with `target_not_idle`, before anything is typed and
under the pane input guard, unless every live agent row on the resolved
endpoint (and `session_id`, when given) is `idle`; an untracked pane is
refused too. `force: true` and an absent field (older operator clients) type
as before. MCP `tend_send_prompt` sends `force: false` unless the caller
passes `force=true`.

The IPC `send_prompt` response distinguishes:

- `sent`: the host accepted prompt text; do not resend it.
- `submitted`: the host accepted the separate Enter key.
- `submission_confirmed: true`: Tend observed a new `PromptSubmitted` event
  matching the original agent session, socket, and exact prompt. The observer
  subscribes before injection and waits at most 750ms after Enter. Absent evidence
  leaves this field omitted; it does not mean submission failed.

MCP reports an unconfirmed submission explicitly instead of treating a successful
Enter write as proof of application execution. Collaboration mailbox/reply wakes
record the same acknowledgement in daemon debug logs without blocking mailbox
bookkeeping or subsequent peer wakes. Other send interfaces
continue to report transport outcomes.

A collaboration mailbox wake (the short notice that names a request id) is the
one bounded exception. The waker only types into an Idle recipient, and it
records when each attempt began, before the paste (`wake_retry` on the
request). A wake that started a turn moves the recipient's registry row: a
prompt, a state change (a tool hook, even an Idle inferred by a timeout), a
turn's response, or any activity-bearing hook (a response-less `Stop`
included; status-line heartbeats do not count). If two minutes after the attempt began the request is still
unclaimed and the row is Idle with none of those recorded since, tendd types
the notice once more (re-checking the row just before typing; movement since
cancels it), and the next prompt the recipient submits also carries
the request as a structured `UserPromptSubmit` offer. A lost
`PromptSubmitted` hook alone never causes a second paste. If the second
attempt also leaves the row untouched, nothing is typed again: the request is
marked `peer_interrupted` with reason `wake_unsubmitted`, which returns the
sender's wait and, after the usual grace, opens a recovery item asking a
person to look at the pane. `notified_at` keeps the first delivery, so such a
request is never handed to a slot successor as undelivered work. Full-body
direct wakes keep their own phase-based recovery and are never re-typed.

A wake that is never typed at all is bounded too. If a request that expects a
typed wake (to a worker, with `wake` not `never`) is still queued, unclaimed
and unoffered five minutes after it was created, and its exact recipient has
been Idle for those five minutes, it is marked `peer_interrupted` with reason
`wake_undelivered`. That returns the sender's wait and, after the grace, opens
a recovery item; it clears once the request is delivered or claimed. The
recipient's pane refused the wake, for example because a stop recorded for
that pane id was still unresolved, or the terminal write failed. An unresolved
(`uncertain`) worker stop pauses Tend input to its pane id; once its fenced
process has definitely exited, the waker closes it as exited, so a pane id
reused after a tmux restore takes wakes again.

Apart from that, Tend does not resend the body or repeat Enter merely because an acknowledgement
is missing. Hooks can arrive late or be unavailable, and a retry could submit a
different user draft. Inspect the pane before an explicit Enter-only recovery.
The `confirm-selected-yes` automation remains limited to affirmative choice
menus; it does not submit arbitrary pending drafts.

Protocol regression tests:

```sh
cargo test -p tend --lib prompt_delivery
cargo test -p tend --lib send_prompt_
cargo test -p tend --test prompt_paste_boundary -- --ignored
```

The last command requires native tmux, rmux, and Python 3. It creates temporary
isolated servers and verifies the exact paste-start/text/paste-end/Enter bytes
with no delay for one-line, Unicode, multiline, and empty Enter-only inputs,
then removes those servers. It does not use existing agent panes.
