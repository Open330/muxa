# Windows

Status: **native, through [rmux](https://rmux.io).** rmux runs natively on
Windows and speaks tmux's command surface over a named pipe, so it is the
pane host muxa observes and drives there. `muxa` and `muxad` build for
`x86_64-pc-windows-msvc` and install with `cargo install` like anywhere else.
WSL2 remains an option if you would rather run tmux.

This document records what is true today, why, and what the platform gates in
the tree are protecting.

## Native install (rmux)

```powershell
winget install Helvesec.RMUX          # or any rmux >= 0.10 on PATH (see below)
cargo install --path crates/muxad --locked
cargo install --path crates/muxa-cli --locked
muxa init --component tmux-popup,tmux-peek,claude-hooks,codex-hooks,muxad-shellrc
```

`muxa init` writes its bindings to `~/.rmux.conf` on Windows — rmux reads that
file (then `%APPDATA%\rmux\rmux.conf`) and never `~/.tmux.conf` — and accepts
rmux in place of tmux in its pre-flight. Run it from inside an rmux pane so the
live reload reaches the right server.

`muxad-shellrc` is the autostart: on Windows it adds a block to the PowerShell
profile (`$PROFILE.CurrentUserCurrentHost`), so the first pane rmux opens
starts the daemon. `muxa daemon start` creates the daemon through
`Win32_Process.Create` (see *Detaching the daemon* below), so it survives the
shell — and the rmux server — that started it. It writes its log itself
(`muxad --log-file`) to `%TEMP%\muxad.log`, which `muxa daemon logs` reads.

rmux 0.10.0 redraws a whole popup every frame, so the watch popup flickers.
[Open330/rmux](https://github.com/Open330/rmux)'s `main` carries the renderer
fix ([RMUX.md](RMUX.md#popup-rendering-on-rmux-0100)); until a release does,
`cargo install --git https://github.com/Open330/rmux --locked rmux` builds it.

### After a reboot

muxad snapshots the workspace on an interval, and follows the server across
restarts even though its pipe name changes.

```powershell
muxa init --component restore-at-login
```

registers a Task Scheduler task, `muxa-restore`, that runs `muxa restore
--boot` at logon: when the rmux server has no sessions it starts it and
rebuilds the newest snapshot, resuming the agents, and otherwise it changes
nothing. Output goes to `%TEMP%\muxa-restore.log`. The task runs at the
level of the terminal that installed it — an elevated terminal's rmux server
is a different server from a limited one's — and starts rmux through
`Win32_Process.Create`, since a task's job object forbids the breakaway rmux
needs for its daemon. By hand, `muxa restore --run` does the same into a
running server.

Verified on Windows 11 with rmux 0.10.0 and Open330/rmux `main` by driving a
real client:

- prefix `s`/`S`/`D`/`q`/`M` open watch, Fleet watch, the dashboard, peek and
  agent move in popups; Enter in watch jumps the client across sessions;
  agent move relocates a pane.
- hooks and the Claude status line attach agents to their rmux panes;
  `muxa status`, `recap`, `msg` (peer delivery and inbox) and `snapshot` work.
- `muxa restore` rebuilds sessions, windows, panes and directories, relaunches
  their commands, and resumes Claude with `--resume <id>`.
- `muxa agent start` places an agent in an rmux pane or window.
- `muxa daemon start/stop/restart/logs` and the PowerShell profile autostart.
- `muxa agent start --agent codex` launches Codex (an npm `.cmd` shim) in a
  window, and its hooks track it. Codex asks once to trust the hooks
  `muxa init` added.
- `muxa upgrade` end to end: build, swap the running binaries, re-exec muxad.
- SSH Fleet with Windows as the controller (Windows OpenSSH): `fleet host
  doctor`, `status`, `panes`, `capture`, `send`, `attach` and `fleet watch`
  against a Linux host. A WSL host on `localhost` needs `HostName 127.0.0.1`:
  WSL forwards localhost over IPv4 only.
- Dashboard pane sharing's whole flow — OIDC sign-in, share link, guest
  prompt, reconnect, daemon restart, revocation — through
  `scripts/dashboard-sharing-smoke.py` on a private rmux label.
- `muxa restore --boot` from a logon task, into a stopped server.

### Known gaps

- **Codex MCP live reload** needs Codex's Unix control socket, which Codex
  does not create on Windows; `muxa mcp reload` reports it unavailable.
- **Collaboration provenance** records no caller pid: a named pipe has no
  safe peer-credential call, so requests read "origin unverified".
- **`muxa onboard`'s live tour** builds a POSIX-shell sandbox on a private
  tmux server and is not expected to work.
- **`muxa fleet attach`** through Windows OpenSSH can let a remote tmux's
  terminal-query reply (`…;42c`) reach the remote shell as typed text.
- **`muxa upgrade`** needs a source checkout on Windows (no prebuilt release
  is published for it). It moves the running `muxa.exe`/`muxad.exe` aside
  before `cargo install`, since Windows refuses to overwrite a running
  executable but allows renaming one.
- Re-exec on upgrade spawns a successor and exits instead of `exec`ing in
  place, so the daemon's pid changes.

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

Five independent reasons. Each is resolved; each is kept here because what it
cost and what it could not recover are the useful part of the record.

### 1. No host to observe — resolved by rmux

Native rmux implements the tmux command surface on Windows, and muxa's rmux
backend drives it through that CLI. The reasoning below is still true of
Windows Terminal.

[`PaneBackend`] requires `list_panes`, `resolve_pane`, `capture_pane`,
`pane_pid_map`, `current_pane`, `focus_pane`, and `send_text`. Windows Terminal
provides none of them: `wt.exe` accepts split-pane arguments at launch and
returns, with no pane enumeration, no stable pane ids, no screen capture, and
no way to send input to a pane it is not focused on. ConPTY is a pseudoconsole
API, not a multiplexer.

### 2. IPC was a Unix-domain socket — resolved

`ipc.rs` used to bind a `UnixListener` and thread
`tokio::net::unix::OwnedWriteHalf` through handler signatures. It now goes
through `crates/muxa/src/transport.rs`, which resolves to a Unix socket or a
Windows named pipe. See [PROTOCOL.md](../PROTOCOL.md#transport).

What did not port cleanly:

- **Protection is weaker.** `0600` on a socket file has no named-pipe
  equivalent. A pipe's DACL is fixed at creation and setting a custom one needs
  an unsafe raw call `unsafe_code = "forbid"` rules out, so the pipe inherits
  the creating token's default DACL — an administrator can connect.
  `first_pipe_instance` and `reject_remote_clients` cover name squatting and
  SMB reachability, which are the other two exposures.
- **No peer credentials.** `SO_PEERCRED` has no safe counterpart, so
  collaboration provenance records `None` for pid/uid/gid on Windows.
- **The synchronous client** (`ipc::blocking_call`, used to register a
  launched agent and reserve its alias) cannot bound a read on a pipe opened
  as a file. On Windows it runs the async transport under a timeout on a
  thread of its own instead.
- **Accept has a gap a socket does not have.** One pipe instance serves one
  client, and the successor is created only once the pending instance is
  claimed. A client connecting in that window gets `ERROR_PIPE_BUSY`, so
  clients retry it briefly; a socket's backlog hides the same burst.

### 3. No process source — resolved through sysinfo

Linux reads `/proc`; other Unix hosts take one `ps` snapshot. Windows has
neither, and `unsafe_code = "forbid"` rules out calling Toolhelp32 directly.
The Windows arms read the same table through `sysinfo`, which wraps those APIs
safely:

| File | Windows arm |
| --- | --- |
| `process_snapshot.rs` | whole table (pid, parent, argv), one process's argv, start time, liveness |
| `adapters/proc_ancestry.rs` | parent lookup for hook ancestry |
| `state.rs` | pid liveness for `muxa proc`/`muxa run` rows |
| `account_swap.rs` | the agent's command line; `taskkill` in place of signals |
| `muxa-cli` `stale_agents.rs`, `reload/process.rs` | stale `muxa mcp` scan; each pane's command for snapshots |

rmux reports `#{pane_pid}` (the pane's shell) but, a console having no
foreground process group, always the shell as `#{pane_current_command}`. The
program running in a pane is therefore read from the shell's children:
snapshot records it (an agent child wins over a helper such as
`WindowsPackageManagerServer.exe`), and restore confirms a relaunch from it.

### 4. Commands are read by PowerShell, not `/bin/sh` — resolved

tmux runs `new-window`, `split-window`, `display-popup` and `run-shell`
commands through `/bin/sh`. rmux on Windows runs them through its
`default-shell`, PowerShell by default, where POSIX quoting (`'\''`), `exec`,
`2>/dev/null`, `|| true`, `printf`/`read`, and a leading quoted program are
all errors. A popup whose command fails closes at once under `-E`, which is
why prefix `s` appeared to do nothing.

`muxa::shell::Dialect` is the single place a command line is built:
PowerShell quoting doubles `'`, a program that needs quoting goes through the
call operator `&`, `exec` is dropped, and `cd` is `Set-Location -LiteralPath`.
Popups, agent launch, restore, account swap, watch's work-up window and spawn
form all go through it. `muxa::shell` also names programs without their path
or `.exe`/`.cmd`, recognises `pwsh`/`powershell`/`cmd` as shells, runs scripts
(`work up`'s `prepare`, the status-line `--forward`) through PowerShell, and
finds npm's `.cmd` shims, which `Command::new` alone does not.

### 5. Unix process management — resolved

- **Detaching the daemon.** `nohup` does not exist, and a process created
  with `CreateProcess` inherits every inheritable handle of its creator — so a
  daemon started by `muxa daemon start *> $null` held the pipe PowerShell
  reads from, and the profile hook would have waited for the daemon to exit.
  It also stayed in the launching pane's job object, which rmux closes with
  its server. `muxa daemon start` therefore creates muxad through
  `Win32_Process.Create`: the WMI service inherits nothing to it and puts it
  in no job.
- **Signals.** `muxad` installs Ctrl+C / console-close / shutdown handlers in
  place of SIGTERM and SIGINT, and re-exec spawns a successor rather than
  calling `exec`.
- **The ambient multiplexer** is rmux's default server on Windows when the
  shell is in no pane, as tmux's is elsewhere; Fleet Work runs there.
- **Paths.** `std::fs::canonicalize` returns verbatim paths (`\\?\F:\…`) that
  no hook reports; `muxa::paths::canonicalize` drops the prefix so cwd
  comparisons hold.

Windows also gives the main thread a 1 MiB stack. An unoptimized build of the
CLI overflows it before `muxa init --help` prints, so `.cargo/config.toml`
links MSVC targets with the 8 MiB stack Unix hosts default to.

## What builds and runs today

`muxa`, `muxad`, and `muxa-cli` build for `x86_64-pc-windows-msvc` with no
clippy warnings. Tests that exercise Unix-only behaviour — `#!/bin/sh`
fixtures through `test_support::write_executable`, `0600`/`0700` mode
assertions, and socket send-buffer backpressure — are gated `#[cfg(unix)]`.

The remaining Windows test failures are Unix assumptions in fixtures — 23 of
1,229 in `cargo test -p muxa --lib`: paths like `/tmp/...` that are not
absolute without a drive prefix, so request validation rejects them, and
bounded-command tests that run `sh`. `muxa-cli`'s unit tests import Unix-only
APIs. CI runs on Linux and macOS only.

## What is left

Teaching the test fixtures a second path grammar so Windows can join CI; the
gaps listed above.

[`PaneBackend`]: ../crates/muxa/src/backend/mod.rs
