# Prompt delivery and submission

Muxa sends prompt text and the submit key as separate operations. Native tmux
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
The fallback and host selection live in `crates/muxa/src/backend/mod.rs`, rather
than an automation rule or a user-editable timing setting.

The IPC `send_prompt` response distinguishes:

- `sent`: the host accepted prompt text; do not resend it.
- `submitted`: the host accepted the separate Enter key.
- `submission_confirmed: true`: Muxa observed a new `PromptSubmitted` event
  matching the original agent session, socket, and exact prompt. The observer
  subscribes before injection and waits at most 750ms after Enter. Absent evidence
  leaves this field omitted; it does not mean submission failed.

MCP reports an unconfirmed submission explicitly instead of treating a successful
Enter write as proof of application execution. Collaboration mailbox/reply wakes
record the same acknowledgement in daemon debug logs. Other send interfaces
continue to report transport outcomes.

Muxa does not resend the body or repeat Enter merely because an acknowledgement
is missing. Hooks can arrive late or be unavailable, and a retry could submit a
different user draft. Inspect the pane before an explicit Enter-only recovery.
The `confirm-selected-yes` automation remains limited to affirmative choice
menus; it does not submit arbitrary pending drafts.

Protocol regression tests:

```sh
cargo test -p muxa --lib prompt_delivery
cargo test -p muxa --lib send_prompt_
cargo test -p muxa --test prompt_paste_boundary -- --ignored
```

The last command requires native tmux, rmux, and Python 3. It creates temporary
isolated servers and verifies the exact paste-start/text/paste-end/Enter bytes
with no delay for one-line, Unicode, multiline, and empty Enter-only inputs,
then removes those servers. It does not use existing agent panes.
