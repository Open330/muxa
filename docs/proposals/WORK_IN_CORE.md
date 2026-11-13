# Work in core: tendd stops running `tend` for Work logic

Status: in progress. Step 1 (the move into `crates/tend-work`) is #502.
`work up` runs in-process (step 3, first call site); see "Done so far".

## Problem

`tendd` reaches the Work, launch and reconcile logic by running the
sibling `tend` binary, because that logic lived in the CLI binary crate.
The child then calls back over IPC.

| Daemon call site | Child command | Trigger |
| --- | --- | --- |
| `work_control::execute_work_up` (IPC `work_up`, dashboard, Mac app) | `tend work up --json …` | a person or agent starting Work |
| `pipeline_reconciler::reconcile` | `tend work reconcile --all` | every reconcile tick |
| `agent_control` (IPC `agent_start`) | `tend agent start --json …` | Mac app New Agent, MCP start |
| `mux_snapshot_control` | `tend work …` restore steps | snapshot restore |
| `fleet_manager`, `ipc/work.rs`, relay | allowlisted `tend work …` | Fleet and relay commands |

What goes wrong:
- During an upgrade the daemon and the CLI can be different versions.
- Errors come back only as an exit code plus a stderr tail.
- Every call pays a process spawn and a second IPC handshake.

## What the moved code assumes today

Moving the code into a library (step 1) changes none of these. Each must
become explicit before the daemon can call it in-process.

1. **Process environment.**
   - `TEND_SOCKET` (which daemon the child calls back).
   - `TEND_HOST`/`TEND_HOSTS` (backend selection in `tend::active_backends`).
   - `TEND_TMUX_SOCKET` (`tend::tmux::tmux_command_scoped`).
   - `TMUX`/`TMUX_PANE`/`RMUX`/`RMUX_PANE` (the caller's pane and endpoint).
   - `CODEX_THREAD_ID`.

   The child runs with a curated set; `execute_tmux_work` clears `TMUX*`
   and pins `TEND_HOST`. The daemon cannot set these per call, because
   process env is shared by every task.
2. **`mux_control::with_endpoint`'s thread-local endpoint.** It is a
   synchronous scope, so it cannot cover an async future that awaits or
   moves between worker threads.
3. **The caller.** `caller::own_tmux_pane`, `pane_env_is_borrowed` and
   `current_collaboration_origin` describe the process that runs the code.
   In the daemon that is tendd, not the agent that asked.
4. **Blocking IPC back into the daemon.**
   - `agent_launch` uses `tend::ipc::blocking_call` (hello, worker
     purpose, launched-agent registration).
   - `spawn_provenance` does the same.
   - Called inside tendd, these would block a worker thread on a request
     the same daemon must serve.
5. **Presentation and interaction.** The `work close` / `work up` runners
   ask `cliclack` confirmations and `println!` results. Popup helpers use
   `current_exe()`, which inside the daemon names `tendd`.
6. **The working directory** for relative paths in `work up`.
7. **Blocking subprocesses inside async entry points.** `work up` runs
   synchronous mux discovery and `git` through `std::process::Command`
   (`work_up.rs` `apply`, `branch_exists`, `git`). Today the child process
   absorbs those waits. Inside tendd they would stall an executor thread.

## Target shape

```rust
pub struct WorkContext {
    /// The daemon to call (in-process: a handle, not a socket path).
    pub daemon: DaemonHandle,
    /// Which mux server and backend this operation addresses.
    pub endpoint: BackendEndpoint,
    /// Who asked: their pane/origin when known, never the daemon's own.
    pub caller: Option<CollaborationOrigin>,
    /// Absolute working directory.
    pub cwd: PathBuf,
    /// Never prompt; a decision the CLI would ask for is an error.
    pub interactive: bool,
}
```

- Entry points return structured results (`UpResult`, `StartResult`,
  `ReconcileReport`) and never print.
- The CLI builds a `WorkContext` from its own environment, so its
  behaviour stays as it is.
- The daemon builds one from the request.
- `DaemonHandle` is an internal service interface. In-process it calls the
  store and mailbox directly instead of looping back through the socket.
  That removes item 4 rather than guarding it.

## Steps

1. **Move** (#502). The modules live in `crates/tend-work`; the CLI keeps
   its views.
2. **Context.** Thread `WorkContext` through `work_up`, `agent_launch` and
   reconcile, replacing items 1–7.
   - Mux commands take the endpoint explicitly. `with_endpoint` stays only
     as a CLI convenience.
   - Synchronous mux and `git` work either moves to `tokio::process` or
     runs in bounded `spawn_blocking` sections, decided per call (item 7).
   - Unit tests per entry point use a fake mux and an in-memory daemon.
   - No call site changes yet.
3. **Daemon call sites**, one PR each, in this order: `work up`,
   `reconcile`, `agent start`, snapshot restore, Fleet commands.
   - Each keeps the child path as a fallback, chosen *before* any effect.
     Once the in-process path has started mutating tmux, it never retries
     as a child.
   - Each is proven on a scratch daemon (`XDG_DATA_HOME`/`XDG_STATE_HOME`
     in a temp dir) against a scratch tmux server before merge. The proof
     includes a responsiveness check: other IPC requests keep answering
     while a slow `work up` runs.
4. **Remove the fallback** for a call site after one release with no
   fallback use in the logs.

## Non-goals

- No change to what `work up`, reconcile or agent start do.
- `tend work …` stays a CLI command with the same output.

## Done so far

- `work up` from IPC, the dashboard and the Mac app runs in tendd. The code
  is in `crates/tendd/src/work_up_in_process.rs`; the hook is
  `work_control::install_in_process_work_up`. It mirrors the child exactly
  rather than introducing `WorkContext` yet:
  - the same argv, parsed by `UpArgs`;
  - the CLI's default config file;
  - the daemon's own env and cwd, which the child inherited;
  - a client on the socket the child had pinned;
  - no prompts (`UpArgs::non_interactive`).

  It runs on a blocking thread with its own current-thread runtime, so the
  thread-local endpoint, blocking IPC and synchronous `git`/tmux stay off
  the daemon's workers. Unreadable argv or config declines before any
  effect, and the child runs instead. `TEND_WORK_UP_CHILD=1` forces the
  child.
- Proven on a scratch daemon (scratch `HOME`, XDG homes, `TMUX_TMPDIR`
  tmux server with a plain `sh`, a fake `codex`):
  - in-process launch with `TEND_CLI` pointing nowhere;
  - a converging re-run;
  - the forced child;
  - the forced child failing without a CLI;
  - `health` answering in under 100 ms during a `work up`.
