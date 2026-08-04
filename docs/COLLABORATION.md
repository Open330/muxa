# Agent collaboration

Muxa can treat a tmux window as a collaboration room and broker durable
request/reply messages between its top-level agents. tmux supplies topology;
the message body travels through muxad's owner-only Unix socket and local
mailbox, not through terminal screen scraping.

## Enable

```toml
[collaboration]
enabled = true
wake = "idle_only" # or "never" for pull-only delivery
```

Restart `muxad` and register `muxa mcp` with each agent host. For Claude Code:

```bash
claude mcp add muxa -- muxa mcp
```

The MCP process inherits `TMUX` and `TMUX_PANE`. muxad validates that origin
against its live agent and pane registries; collaboration tools do not accept
an arbitrary sender pane.

## Addressing and CLI

Agents sharing `(tmux socket, stable window id)` are peers. `peer` selects the
only other agent; with three or more participants use `%N` or `pane:%N`.
Cross-window targets are refused. Requests are also pinned to the target's
current agent session, so a new process reusing that pane cannot inherit old
work.

```bash
muxa peers
muxa msg send peer "review the auth change" --kind review
muxa msg inbox
muxa msg reply req_... "review complete" --status completed
muxa msg wait req_... --timeout-secs 300
```

`question` and `review` are read-only contracts by default. Use `--execute`
and one or more `--path` arguments to explicitly delegate edits. Path scopes
are advisory collaboration contracts, not an OS sandbox; separate git
worktrees remain the safest choice for concurrent edits.

## MCP tools

| Tool | Purpose |
| --- | --- |
| `muxa_room_context` | Identify self, list same-window peers, and show unread count. |
| `muxa_send_message` | Create a durable peer request. |
| `muxa_inbox` | Claim/read requests for this exact agent session. |
| `muxa_reply` | Return a completed/blocked/declined/failed response. |
| `muxa_wait_reply` | Wait for the structured terminal response. |

## Wake-up safety

The mailbox is persisted to `$XDG_DATA_HOME/muxa/collaboration.json` before
delivery. With `idle_only`, muxad injects only a short inbox notification, and
only when the exact hook-authoritative recipient is `Idle`. It never injects
into `Working`, `WaitingInput`, `WaitingChoice`, or `Error` panes, and never
auto-wakes synthetic screen-detected agents. The recipient reads the body and
atomically claims it through `muxa_inbox`, making repeated wake notifications
idempotent at the request level.
