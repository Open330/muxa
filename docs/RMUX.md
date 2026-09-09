# rmux backend

Status: **CLI backend and terminal controls implemented.** Muxa can discover rmux panes,
correlate agent hooks, capture screens, focus panes, and send targeted literal
input through rmux's public command-line surface.

## Why rmux is a separate backend

rmux deliberately sets both its native environment variables (`RMUX`,
`RMUX_PANE`) and tmux-compatible variables (`TMUX`, `TMUX_PANE`). Treating it
as tmux would make a native rmux pane such as `%3` collide with a real tmux
pane `%3`, and tmux socket discovery would target the wrong server.

Muxa therefore:

- detects `RMUX` / `RMUX_PANE` before the compatibility `TMUX` variables;
- stores rmux pane ids as `rmux:%N` internally;
- stores the full native socket path from the first field of `$RMUX`;
- strips `rmux:` and passes `-S <socket>` when issuing rmux control commands;
- skips rmux compatibility shims when resolving the native tmux executable,
  so the same rmux panes do not appear a second time as tmux panes.

`MUXA_HOST=rmux` forces a single rmux backend. `MUXA_HOSTS=rmux,tmux` observes
both explicitly. With the rmux CLI installed, a login-launched daemon keeps an
endpoint-less rmux backend ready even before the first server starts. Inside a
native rmux pane, rmux becomes primary while tmux remains in the multi-host set.

Muxa resolves the binary through `PATH`, the official installer's
`~/.local/bin/rmux`, Cargo's `~/.cargo/bin/rmux`, and the common Homebrew
prefixes. This also covers systemd/launchd environments with a restricted
`PATH`.

## Capability baseline

| Capability | Initial backend |
| --- | --- |
| Pane/session enumeration | `rmux list-panes -a -F ...` |
| Current command, cwd, tty, pid | rmux format fields |
| Capture | `rmux capture-pane -ep -t ...` |
| Focus | Client-scoped session/window/pane jump with private grouped views |
| Rename | Session, window, and pane title; CLI buffer-based window naming |
| Lifecycle | Workspace/work and agent launch, window creation, pane splitting, close |
| Interaction | Prompt submission, interrupt, capture, window inspector, `muxa peek` |
| Auto view | `muxa workspace view` attach/session-change hooks; view reuse and cleanup |
| Targeted input | literal `send-keys`; bracketed paste for multiline text |
| Hook identity | `RMUX_PANE` → `rmux:%N` |
| Session activity duration | Not yet sampled |
| Watch jump / bare-terminal attach | Switch the invoking client to the target session/window/pane; attach from a bare terminal |

The CLI transport is intentional for the first slice because it matches
Muxa's synchronous `PaneBackend` contract and keeps rmux out of the dependency
graph. The backend boundary permits a later move to `rmux-sdk` for streaming
events and fewer process launches without changing callers.

## Current limits

- Auto-discovery observes the endpoint inherited through `$RMUX`, or rmux's
  default endpoint when the daemon has no pane environment. Arbitrary named
  endpoints are not enumerated, but hook-registered agents retain their full
  endpoint for capture and input and are not reaped by another endpoint's scan.
- Session/client activity sampling is absent, so rmux rows report no DUR/ACT
  time instead of borrowing tmux's same-shaped `$N` session ids.
- `muxa watch` switches the invoking rmux client to the selected session,
  window, and pane. Busy sessions get a grouped view per terminal, and jumps
  preserve an existing view. Window creation is detached until the invoking
  client jumps, so it does not move another terminal's selected window.
  Popup callers preserve `--caller-client`; topology rows
  preserve the target server endpoint. From a bare terminal, the command uses
  `rmux attach-session` with a temporary grouped view when the source is busy.
  Switching an existing rmux client across different
  servers is rejected with an explicit message.
- The implementation follows the public CLI/format contract verified against
  rmux 0.10.x.

## Popup rendering on rmux 0.10.0

The generated watch/Fleet bindings use full width, 99% height, and top-left
placement. This leaves at least one bottom row for the host status bar:
rmux continues writing that bar while a popup is open, so a 100%-height
popup and its footer otherwise share the same terminal row. Existing users
can re-run the popup component of `muxa init`, or change only the watch
bindings from `-h 100%` to `-h 99%` while keeping `-x 0 -y 0`.

This mitigates the bottom-row collision, **not all popup flicker**. In an
isolated 120x30 client, six idle seconds of watch produced about 58 KB and
180 full-width blank rows both with the status bar enabled and disabled.
A static `sleep` popup produced status updates with the bar enabled and no
output with it disabled. The rmux popup renderer fills the entire popup
before drawing its contents; running `muxa watch` directly in a pane avoids
that popup redraw path. Fixing that renderer requires a change in rmux.

## Validation

Unit coverage locks down native-env precedence, pane-id namespacing, endpoint
preservation, malformed observation handling, multi-host routing, and
same-basename endpoint disambiguation. A live smoke test requires an installed
`rmux` binary. Run it against a disposable, explicit socket so it cannot touch
an existing rmux server:

```sh
muxa_rmux_smoke_dir=$(mktemp -d)
muxa_rmux_smoke_socket="$muxa_rmux_smoke_dir/rmux.sock"
cleanup_muxa_rmux_smoke() {
  rmux -S "$muxa_rmux_smoke_socket" kill-server >/dev/null 2>&1 || true
  rmdir "$muxa_rmux_smoke_dir" >/dev/null 2>&1 || true
}
trap cleanup_muxa_rmux_smoke EXIT INT TERM

rmux -S "$muxa_rmux_smoke_socket" new-session -d -s muxa-rmux-smoke
muxa_rmux_smoke_pane=$(
  rmux -S "$muxa_rmux_smoke_socket" \
    list-panes -t muxa-rmux-smoke -F '#{pane_id}'
)

MUXA_RMUX_TEST_ENDPOINT="$muxa_rmux_smoke_socket" \
MUXA_RMUX_TEST_PANE="$muxa_rmux_smoke_pane" \
  cargo test -p muxa \
    backend::rmux::tests::live_backend_smoke_against_explicit_endpoint \
    -- --ignored --exact
```

The client/session/window jump regression uses a disposable rmux server and a
PTY client, then checks the client's session and the selected window/pane:

```sh
python3 scripts/rmux-jump-check.py
```

## Compatibility details

The CLI lifecycle and layout helpers prefer the native `$RMUX` endpoint over
rmux's `$TMUX` compatibility variables. Watch actions use the selected row's
host and socket instead of the invoking terminal's backend. Pane ids retain
`rmux:` internally and lose that prefix only at the native command boundary.

rmux 0.10 interprets `new-session -t` as a session/group **name**. Passing `$N`
can silently create an unrelated group, so view creation uses the resolved
session name on rmux. Client membership and pid are read from `list-clients`;
`display-message` client context is insufficient to identify a popup caller.

The scanner appends `socket_path` after the entire shared pane format. Its
column index must track that format: column 12 contains workspace metadata,
not the endpoint. Full rows retain session groups for topology folding.

Run `python3 scripts/rmux-jump-check.py` to test two PTY clients, isolated
window selection and view reuse, rename, creation/splitting, prompt submission,
interrupt/close, capture, CLI buffer naming, and peek against a disposable server.

Grouped windows also need session-qualified targets for commands such as
`set-option`: a bare `@N` can be ambiguous even though every link represents
the same window. Muxa resolves those ids within the selected endpoint before
sending control commands. Window rename reads link identities from enumeration
to avoid empty `display-message` context fields on rmux 0.10.

Auto-view hooks use `#{hook_client}`, which names the client that triggered
the event on both tmux and rmux. rmux can leave `#{client_name}` empty in that
hook context; popup key bindings continue to use `#{client_name}` at keypress.
