# Windows

Status: **native, through [rmux](https://rmux.io).** rmux runs natively on
Windows and speaks tmux's command surface over a named pipe, so it is the
pane host muxa observes and drives there. `muxa` and `muxad` build for
`x86_64-pc-windows-msvc` and install with `cargo install` like anywhere else.
WSL2 remains the complete, tested product if you would rather run tmux.

This document records what is true today, why, and what the platform gates in
the tree are protecting.

## Native install (rmux)

```powershell
winget install Helvesec.RMUX          # or any rmux >= 0.10 on PATH
cargo install --path crates/muxad --locked
cargo install --path crates/muxa-cli --locked
muxa init --component tmux-popup,tmux-peek
```

`muxa init` writes its bindings to `~/.rmux.conf` on Windows — rmux reads that
file (then `%APPDATA%\rmux\rmux.conf`) and never `~/.tmux.conf` — and accepts
rmux in place of tmux in its pre-flight. Run it from inside an rmux pane so the
live reload reaches the right server.

There is no service manager integration yet, so start the daemon yourself
(`Start-Process muxad -WindowStyle Hidden`, or a Task Scheduler logon task).

Verified on Windows 11 with rmux 0.10.0: `muxad` serves the named pipe,
`muxa doctor` reaches it and resolves the rmux server, and `muxa watch` renders
every rmux session, window, and pane.

### Known gaps

- **No daemon autostart.** `muxa doctor` reports the service checks as
  unsupported on Windows rather than failing them.
- **Agent skill install** (`muxa init --component agent-skills`) needs a
  symlink and bails on Windows.
- **Codex MCP live reload** needs Codex's Unix control socket and reports
  itself unavailable.
- **No process source** (reason 3 below): hook ancestry recovery and
  process-based agent attribution return empty, so agents are found through
  their hooks and screen detection only.
- **Dashboard pane sharing** pins a share to a tmux socket path and has not
  been mapped onto rmux's named pipe; its tests are Unix-only.
- **`muxa onboard`'s live tour** builds a POSIX-shell sandbox and is not
  expected to work.
- Re-exec on upgrade spawns a successor and exits instead of `exec`ing in
  place, so the daemon's pid changes.
- **Upgrading needs the daemon stopped.** Windows locks a running
  executable, so `cargo install` cannot replace `muxad.exe` while it runs
  (`Access is denied`). Stop it, install, start it again.

## Use WSL2

```bash
# inside your WSL distribution
curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/install.sh | sh
```

Requirements are the ordinary Linux ones: tmux, and Rust `1.89+` to build.
Windows Terminal attaches to the WSL distribution like any other shell, so
`muxa watch` runs in a Windows Terminal tab with no bridge in between.

Clone into the WSL filesystem (`~/`) rather than building from `/mnt/<drive>`.
The 9p mount that backs `/mnt` makes cargo builds several times slower.

## What stood in the way

Four independent reasons. The first two are resolved and the fourth is
resolved as far as compiling goes; each is kept here, marked, because what it
cost and what it could not recover are the useful part of the record.

### 1. No host to observe — resolved by rmux

*Historical.* Native rmux implements the tmux command surface on Windows, and
muxa's rmux backend drives it through that CLI, so the requirements below are
met. The reasoning is kept because it is still true of Windows Terminal.

[`PaneBackend`] requires `list_panes`, `resolve_pane`, `capture_pane`,
`pane_pid_map`, `current_pane`, `focus_pane`, and `send_text`. Windows Terminal
provides none of them: `wt.exe` accepts split-pane arguments at launch and
returns, with no pane enumeration, no stable pane ids, no screen capture, and
no way to send input to a pane it is not focused on. ConPTY is a pseudoconsole
API, not a multiplexer.

[`BackendCaps`] can already describe a host with gaps — that is how the zellij
CLI baseline is modelled — but `list_panes` has no capability flag because
every host must answer it. A Windows backend could not.

This is the blocker. The three below are ordinary work; this one has no known
solution, so the other three should not be started on its account — and
finishing one of them, as the next section did, changes nothing here.

### 2. IPC was a Unix-domain socket — resolved

`ipc.rs` used to bind a `UnixListener` and thread
`tokio::net::unix::OwnedWriteHalf` through handler signatures. It now goes
through `crates/muxa/src/transport.rs`, which resolves to a Unix socket or a
Windows named pipe. See [PROTOCOL.md](../PROTOCOL.md#transport).

The encoding was never the coupled part — line-delimited JSON, which Fleet
already carries over an SSH stdio byte stream (`crates/muxa-cli/src/relay.rs`).
The types were. Rather than make the dispatch table generic, the module exposes
one `Stream`/`ReadHalf`/`WriteHalf` family whose definition is platform-chosen,
so `ipc.rs` gained no type parameters.

What did **not** port cleanly, and is why this is a compile story rather than a
support story:

- **Protection is weaker.** `0600` on a socket file has no named-pipe
  equivalent. A pipe's DACL is fixed at creation and setting a custom one needs
  an unsafe raw call `unsafe_code = "forbid"` rules out, so the pipe inherits
  the creating token's default DACL — an administrator can connect.
  `first_pipe_instance` and `reject_remote_clients` cover name squatting and
  SMB reachability, which are the other two exposures.
- **No peer credentials.** `SO_PEERCRED` has no safe counterpart, so
  collaboration provenance records `None` for pid/uid/gid on Windows.
- **No synchronous client.** `blocking_call` needs a bounded read, and a pipe
  opened as a file cannot set one without `SetCommTimeouts`. It returns
  `Unsupported` rather than risk hanging the caller — which lands on the
  "daemon unavailable" path the function already documents.
- **Accept has a gap a socket does not have.** One pipe instance serves one
  client, and the successor is created only once the pending instance is
  claimed. A client connecting in that window gets `ERROR_PIPE_BUSY`, so
  clients retry it briefly; a socket's backlog hides the same burst.

### 3. No process source

Linux reads `/proc`. Every other platform took one `ps` snapshot. Windows has
neither.

Until recently the second arm was spelled `#[cfg(not(target_os = "linux"))]`,
which meant Windows *silently* selected the `ps` implementation and got an
empty table with one `tracing::debug!` line. Those arms are now
`#[cfg(all(unix, not(target_os = "linux")))]` with explicit `#[cfg(not(unix))]`
counterparts, so the absence is visible in the tree instead of being an
accident of cfg arithmetic:

| File | Unix arm | Non-Unix arm |
| --- | --- | --- |
| `process_snapshot.rs` | `/proc` or `ps` | empty table |
| `adapters/proc_ancestry.rs` | `/proc` or `ps` | `None` |
| `tmux/scanner.rs` | walk `$TMUX_TMPDIR`, probe sockets | no sockets |
| `backend/unix_socket.rs` | `std::os::unix::net::UnixStream` | uninhabited stand-in whose `connect` always fails |

Returning empty is the honest answer while reason 1 stands: with no Windows
backend there are no panes whose descendants could be attributed. Wiring a real
source (Toolhelp32, WTS) is only worth doing once those PIDs mean something.

Note that `unsafe_code = "forbid"` is set workspace-wide, so a future process
source cannot call the Win32 APIs directly — it needs a safe wrapper crate.

### 4. Unix assumptions throughout the CLI — compiles, partly gated

`muxa-cli` keeps `nix` and `signal-hook` for Unix; on Windows the interactive
attach wait is a plain `Child::wait` (a console close reaches the child
directly), owner-only creation modes go through `owner_only::OwnerOnly` (a
no-op under the per-user profile ACL), and daemon liveness probes use
`muxa::transport::endpoint_is_live` so they understand named pipes. `muxad`
installs Ctrl+C / console-close / shutdown handlers in place of SIGTERM and
SIGINT, and re-exec spawns a successor rather than calling `exec`.

Service installation still shells out to `launchctl` and `systemctl` only, and
the call sites that invoke `sh`, `bash`, `id`, `kill`, `ps`, or `nohup` are
reached only from features listed under *Known gaps*.

Windows also gives the main thread a 1 MiB stack. An unoptimized build of the
CLI overflows it before `muxa init --help` prints, so `.cargo/config.toml`
links MSVC targets with the 8 MiB stack Unix hosts default to.

## What builds and runs today

`muxa`, `muxad`, and `muxa-cli` build for `x86_64-pc-windows-msvc`. Tests that
exercise Unix-only behaviour — `#!/bin/sh` fixtures through
`test_support::write_executable`, `0600`/`0700` mode assertions, and socket
send-buffer backpressure — are gated `#[cfg(unix)]`.

The remaining Windows test failures are pre-existing Unix assumptions in
fixtures — 20 of 1,217 in `cargo test -p muxa --lib` (paths like `/tmp/...` that
are not absolute without a drive prefix,
Unix process liveness), left alone because teaching fixtures a second path
grammar is separate work. CI runs on Linux and macOS only.

## What is left

With rmux as the host, the order is: a process source (reason 3, through a
safe wrapper crate since `unsafe_code` is forbidden) so agents are attributed
to panes without hooks; a Task Scheduler daemon component for `muxa init`;
then the gaps listed above.

[`PaneBackend`]: ../crates/muxa/src/backend/mod.rs
[`BackendCaps`]: ../crates/muxa/src/backend/mod.rs
