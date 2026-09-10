# Windows

Status: **not a supported host. Use WSL2.** muxa observes and drives agents
running in multiplexer panes, and no Windows-native multiplexer exposes the
control surface [`PaneBackend`] requires. Under WSL2 muxa is the complete
product, unchanged.

This document records what is true today, why, and what the platform gates in
the tree are protecting — so that a future port starts from measurements
rather than from a fresh survey.

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

## Why not native

Four independent reasons, in increasing order of how hard they are to remove.

### 1. No host to observe

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
solution, so the other three should not be started on its account.

### 2. IPC is a Unix-domain socket

`crates/muxa/src/ipc.rs` binds a `UnixListener`, `chmod`s it to `0600`, and
threads `tokio::net::unix::OwnedWriteHalf` through handler signatures. See
[PROTOCOL.md](../PROTOCOL.md#transport).

The encoding itself is transport-agnostic — line-delimited JSON — and Fleet
already runs the same shape of protocol over an SSH stdio byte stream
(`crates/muxa-cli/src/relay.rs`), so there is precedent for carrying it on
something other than a Unix socket. The security model is the part that does
not port directly: `0600` on a socket file has no exact named-pipe equivalent,
and a pipe's protection comes from creation-time flags and its DACL instead.

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

### 4. Unix assumptions throughout the CLI

`muxa-cli` depends on `nix` and `signal-hook`, neither of which builds for a
Windows target, so the CLI fails at dependency resolution rather than at
compile time. Service installation shells out to `launchctl` and `systemctl`,
and roughly twenty other call sites invoke `sh`, `bash`, `id`, `kill`, `ps`,
or `nohup`.

## What compiles today

`cargo check -p muxa --target x86_64-pc-windows-msvc`.

The whole dependency tree builds on Windows, including `rusqlite` (bundled
SQLite, so a C toolchain is exercised), `portable-pty` (ConPTY), `axum`,
`reqwest`, `tokio`, and `notify-rust` (which pulls `tauri-winrt-notification`
for toasts). The dependencies are not the obstacle; muxa's own code is.

Remaining errors are confined to `ipc.rs`.

A caution for anyone reading that error count as an estimate: they are `E0432`
/ `E0433` name-resolution failures, and rustc stops before type-checking. The
9,921 lines of `ipc.rs` have not been checked at all. Stubbing the imports
reveals a considerably larger second wave.

## If the blocker ever lifts

Sequence the work as 1 → 2 → 3 → 4, not the reverse. Reasons 2 through 4 are
tractable and individually reviewable, but they buy nothing on their own: a
daemon that starts on Windows and observes no panes is not a product. Settle
what a Windows `PaneBackend` would talk to first.

[`PaneBackend`]: ../crates/muxa/src/backend/mod.rs
[`BackendCaps`]: ../crates/muxa/src/backend/mod.rs
