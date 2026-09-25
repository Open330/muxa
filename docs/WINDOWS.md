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
The second has since been removed — it is kept here, marked, because what it
cost and what it could not recover are the useful part of the record.

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

### 4. Unix assumptions throughout the CLI

`muxa-cli` depends on `nix` and `signal-hook`, neither of which builds for a
Windows target, so the CLI fails at dependency resolution rather than at
compile time. Service installation shells out to `launchctl` and `systemctl`,
and roughly twenty other call sites invoke `sh`, `bash`, `id`, `kill`, `ps`,
or `nohup`.

## What builds and runs today

The `muxa` library crate compiles clean for `x86_64-pc-windows-msvc`, warnings
included, and its test suite runs:

```
cargo check -p muxa --all-targets     # clean
cargo test  -p muxa --lib             # 995 passed, 8 failed
```

The whole dependency tree builds, including `rusqlite` (bundled SQLite, so a C
toolchain is exercised), `portable-pty` (ConPTY), `axum`, `reqwest`, `tokio`,
and `notify-rust` (which pulls `tauri-winrt-notification` for toasts). The
dependencies were never the obstacle.

The eight failures are all pre-existing Unix assumptions in test fixtures, none
in transport or IPC:

| Test | Assumption |
| --- | --- |
| `ipc::work_command_tests` (4) | fixture cwd `/tmp/...`, which `Path::is_absolute` rejects without a drive prefix |
| `work_control::tests::native_work_request_...` | same |
| `adapters::antigravity::tests` (2) | transcript path shape |
| `state::tests::reap_dead_pids_...` | Unix process liveness |

They are left alone deliberately. Making them pass means teaching fixtures a
second path grammar, which is only worth doing for a host muxa intends to
support.

`muxa-cli` still does not build — `nix` fails at dependency resolution. `muxad`
has no such dependency and is the natural next crate, if anyone wants one.

## If the blocker ever lifts

Sequence the remaining work as 1 → 3 → 4, and settle 1 first. Reasons 3 and 4
are tractable and individually reviewable, but they buy nothing on their own: a
daemon that starts on Windows and observes no panes is not a product. Reason 2
was worth doing early only because it was the load-bearing one for *compiling*
at all, and because writing the transport down forced the security differences
above into one reviewable place.

[`PaneBackend`]: ../crates/muxa/src/backend/mod.rs
[`BackendCaps`]: ../crates/muxa/src/backend/mod.rs
