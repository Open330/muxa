# Work in core: tendd stops running `tend` for Work logic

Status: in progress. Step 1 (the move into `crates/tend-work`) is #502.
`work up`, pipeline reconcile, `agent start`, snapshots and the Fleet `work up` / `agent start` run in-process (step 3); see "Done so far" and "Fallbacks and when they can go".

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
- Pipeline reconcile (`pipeline_reconciler`) runs `run_reconcile` in tendd,
  with the same config, socket and subprocess scope, and the child's 60 s
  deadline. Dropping the pass (supervisor shutdown) cancels the scope: the
  running git/tmux command is killed and no further one starts, as the
  child was killed on drop. `TEND_RECONCILE_CHILD=1` forces the child, and
  `TEND_PIPELINE_CLI` still selects a program.
  - Proven on a scratch daemon, with the `tend` binary moved away: a `duo`
    pipeline's `review` (after `impl`) launched once `impl` reported done.
    The forced child did the same.
- `agent start` (IPC `agent_start`, the Mac app's New Agent) runs
  `agent_launch::launch_json` in tendd (`crates/tendd/src/agent_start_in_process.rs`,
  hook `agent_control::install_in_process_agent_start`). What the child got
  from its environment is passed explicitly:
  - the target's tmux server, which was `TEND_TMUX_SOCKET`, is the selected
    mux endpoint, and the new pane is looked up on that server only
    (`mux_control::resolve_pane`);
  - a native session's environment, which the child's `caller_env` read, is
    the daemon's with `TEND_SOCKET` pinned;
  - every tmux command runs under the subprocess scope with the child's 30 s
    deadline, and dropping the call cancels the scope (no further tmux
    command or native spawn), as the child was killed on drop.

  Two cases decline and run the child, because the child had a different
  process environment that code outside the mux endpoint reads:
  - an rmux server (`$RMUX` set, pane variables cleared: backend detection,
    the caller's pane);
  - a native request whose `env` changes a daemon variable (the Mac app's
    `PATH`/`TERM`: finding `aas` and the provider, the terminal size).

  Both need the environment passed explicitly through those lookups, which
  is the `WorkContext` step. `TEND_AGENT_START_CHILD=1` forces the child.
  - Proven on a scratch daemon with `TEND_CLI` pointing nowhere: window
    (full socket path), session (short socket name) and native launches (a
    native one with a changed `TERM`, before that case declined); an
    unknown server refused before anything ran; `health` answering in under
    10 ms during a launch; the forced child failing without a CLI and
    succeeding with one.
- Snapshots (`mux_snapshot_control`: list, save, the automatic snapshot,
  plan, delete, restore) run `tend_work::reload` in tendd
  (`crates/tendd/src/snapshot_in_process.rs`, hook
  `mux_snapshot_control::install_in_process_snapshot`). `reload` moved from
  the CLI into `tend-work`; `tend reload mcp` stays in the CLI.
  - The answer has the child's shape, so `MuxSnapshotControl` is unchanged
    past `run`: the `--json` document as stdout (`reload::capture_json`
    takes what `print_json` would have printed), exit code 0 or 1, and
    `Error: ` and the error chain as stderr, as the CLI's `main` printed
    it (`printed`, without a backtrace).
  - The code reads no config and no request environment, so the daemon's
    own environment is what the child had, apart from `TEND_SOCKET`, which
    the client and subprocess scope carry.
  - The scope's deadline is the child's budget (30 s, 60 s, 15 min).
    Dropping the call cancels it, as `kill_on_drop` did to the child. Past
    either, the restore's per-pane waits (shell ready, screen settled, trust
    prompts, launch confirmation) stop at once, each phase boundary fails
    the restore (its report so far is kept), and no snapshot is written,
    pruned or deleted.
  - `TEND_SNAPSHOT_CHILD=1` forces the child.
  - Proven on a scratch daemon with `TEND_CLI` pointing nowhere: a tracked
    agent's server saved and listed; a killed session planned and restored
    (its agent pane relaunched); the forced child failing without a CLI and
    succeeding with one.
- Fleet commands (`fleet_manager::run_local_work_command`, the relay side
  of a controller's `work_command`): `work up …` and `agent start …` run
  in-process through the code their local requests use
  (`relay_in_process.rs`; `work_up_in_process::run_argv`,
  `agent_start_in_process::launch_argv`). The answer keeps the child's
  shape (exit code, JSON on stdout, `Error: ` and the chain on stderr), so the
  controller's parsing is unchanged. Over Fleet an `agent start` is always a
  tmux launch on the host's default server, without `tmux_socket` or `env`.
  Each honours its own forcing variable. The rest of the allowlist (`work
  options|preset|pipeline|route|dispatch…|fleet-ask`, `swap`) still runs the
  child, because that code is still in the CLI crate.
  - A Fleet `work up` declines what only the CLI's wrapper does (see
    below), and a frame with stdin runs the child.
  - Proven on a scratch daemon with `[fleet] enabled`, through
    `fleet_command` to `local` with `TEND_CLI` pointing nowhere: a relayed
    `agent start` created and stamped its window; a relayed `work up
    --no-ticket` answered exit 1 with the CLI's error line; one without
    `--no-ticket` declined to the (missing) CLI; with both forcing variables
    set, both failed for the missing CLI, and with the built `tend` the
    forced child worked.

## Fallbacks and when they can go

Every call site listed under Problem now runs in tendd when it can. What
still reaches the CLI child, and when it can be removed:

| Fallback | Shipped in-process in | Remove the forcing variable in |
| --- | --- | --- |
| `TEND_RECONCILE_CHILD` (pipeline reconcile) | 0.10.17 | 0.10.18, if 0.10.17 hosts never set it |
| `TEND_WORK_UP_CHILD` (`work up`: local in 0.10.17, Fleet in 0.10.18) | 0.10.17 / 0.10.18 | 0.10.19, after the Fleet path's first release |
| `TEND_AGENT_START_CHILD` (`agent start`, local and Fleet) | 0.10.18 | 0.10.19 |
| `TEND_SNAPSHOT_CHILD` (snapshots) | 0.10.18 | 0.10.19 |

These variables are operator escape hatches, and nothing in tend sets
them. Step 4's rule is one release with no use; check each host's service
environment before removing one. `TEND_PIPELINE_CLI` is a test seam that
names the program, not a fallback, and stays.

Removing a forcing variable does not remove the child path itself. These
cases decline to it by design, and each logs `in-process … declined`:
- a `work up` whose argv asks for what only the CLI's wrapper does: a
  ticket lookup (no `--no-ticket`, with or without `--external`), which
  starts a headless agent outside the subprocess scope; `--accept` /
  `--check`; `--show-prompts`; or no `--yes`. A local request never does
  (`WorkUpRequest::arguments`), but a Fleet argv can;
- an `agent start` for an rmux server, or a native one whose `env` changes
  the daemon's values;
- a Fleet frame that carries stdin, which the child handed to a route's
  `prepare`;
- any argv without `--json`, or one that does not parse.

These never go in-process at all, because their code is still in the CLI
crate: IPC `work_command` (`work options|preset|…`), the same subcommands
over Fleet, `swap`, and the dashboard sharing routes.

These go when `WorkContext` (step 2) passes the environment and endpoint
explicitly and the remaining command code moves into `tend-work`. That
needs a proposal of its own. It is not a release-count decision.
