# Launch identity and provenance (D1)

Status: design proposal, 2026-11-05. **No implementation in this PR.**
Audit base: main `c619904ab8e803e99573cec5f15c517e8592f3e8`. PR #365 observations below refer separately to
`9602fce0d9c5a6c694de5855ba69df84846271f4`; that PR is not part of this base.
This develops D1 and the C1/C3/C9/C10/C11 dependencies in
[DETERMINISTIC_FIRST.md](DETERMINISTIC_FIRST.md).

## Decision

Allocate one opaque UUID **before each physical provider launch**. Carry it as
`MUXA_LAUNCH_ID` in the provider's initial execution environment and as
`@muxa_launch_id` on its exact pane. Persist the intent, actual process identity,
and owning execution surface before allowing the provider to run. Join a
provider conversation only from evidence tied to that exact process. Otherwise
report **unknown**, with a reason. Time, arrival order, cwd, screen text, process
names, and the number of candidates never complete this join.

“Deterministic” describes the association, not a UUID derived from cwd or time.
A random UUID allocated once is suitable; the tmux launcher already allocates
one, or takes a worker ticket's ID, but exports neither into the provider's
exec line ([crates/muxa-cli/src/agent_launch.rs:737–750](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/agent_launch.rs#L737-L750),
[crates/muxa-cli/src/agent_launch.rs:778–787](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/agent_launch.rs#L778-L787)).

A launch mark is **necessary but not sufficient** proof. Children inherit an
environment. A nested agent can therefore carry its parent's ID without being
the process muxa launched. Likewise, a shared Codex app-server can carry the
environment of an unrelated pane; the current hook adapter explicitly guards
that case ([crates/muxa/src/adapters/hook.rs:68–83](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/adapters/hook.rs#L68-L83)). The design must retain
those guards, rather than replace them with “the environment has an ID.”

## Current code and launch coverage

All references in this table are to the audit base, not a future implementation.

| Surface | Current ownership boundary | D1 change |
| --- | --- | --- |
| tmux and rmux agent start | `start` builds the exec line, runs one create command, and receives a pane ID; rmux resolution is explicit ([crates/muxa-cli/src/agent_launch.rs:778–838](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/agent_launch.rs#L778-L838), [crates/muxa-cli/src/agent_launch.rs:992–1008](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/agent_launch.rs#L992-L1008)). Registration still polls discovery for up to five seconds ([crates/muxa-cli/src/agent_launch.rs:1032–1073](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/agent_launch.rs#L1032-L1073)). | Prepare a launch record before create; run the gated executor described below; stamp the returned pane on its pinned endpoint; replace discovery registration for these launches with its receipt. |
| Native `run_native` | Builds provider argv and calls `spawn_session`; output has a native session and no pane ([crates/muxa-cli/src/agent_launch.rs:596–635](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/agent_launch.rs#L596-L635)). PTY creation owns `spawn_command`, child PID, and the exit waiter ([crates/muxa/src/session.rs:287–310](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/session.rs#L287-L310), [crates/muxa/src/session.rs:330–346](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/session.rs#L330-L346)). | Carry a typed launch intent to the PTY backend, set the environment there, and commit the session/PID receipt before release. No fake tmux pane. |
| Workers | The worker ticket already contains `launch_id` ([crates/muxa/src/collaboration/spawn_coordination.rs:82–89](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/collaboration/spawn_coordination.rs#L82-L89)); start records `@muxa_worker_launch` after create and reports the observation to the worker coordinator ([crates/muxa-cli/src/agent_launch.rs:840–860](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/agent_launch.rs#L840-L860), [crates/muxa-cli/src/agent_launch.rs:1000–1018](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/agent_launch.rs#L1000-L1018)). | Reuse that ticket ID for this same physical attempt. `@muxa_worker_launch` remains a compatibility alias, written alongside the universal mark. A logical retry that creates a different process gets a new attempt ID. |
| Swap relaunch | The executor can respawn the same pane and update its root PID after respawn ([crates/muxa/src/account_swap.rs:3581–3605](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/account_swap.rs#L3581-L3605)). | Close the old launch generation, prepare a new one before either typing the relaunch or respawning, and require its new receipt before account verification/follow-up. A resumed conversation keeps its provider ID, never its old launch ID. |
| `muxa work up` pipelines | Initial launch and reconciliation both call `agent_launch::start` with pipeline generation/alias metadata ([crates/muxa-cli/src/work_up.rs:484–506](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/work_up.rs#L484-L506), [crates/muxa-cli/src/work_up.rs:1264–1305](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/work_up.rs#L1264-L1305)). | Use the common allocator/executor; persist `(work, alias, generation) -> launch_id` as a link, not as the launch ID. Re-prompting an existing process does not allocate another launch. |
| Muxa.app, local and Fleet | The app uses `agent_start_v1`, or its bundled CLI for local fallback; the daemon executes the CLI with a pinned endpoint ([apps/muxa-macos/Sources/AppModel+Agents.swift:232–245](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/apps/muxa-macos/Sources/AppModel+Agents.swift#L232-L245), [crates/muxa/src/agent_control.rs:350–389](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/agent_control.rs#L350-L389)). | Allocate on the execution host, once; relay the intent/receipt without re-minting at each hop. Both app paths return the launch ID and open the matching verified instance. Do not invent a separate Swift launcher. |
| Restore/reload launches | Reload currently watches shell readiness and whether a non-shell program appeared ([crates/muxa-cli/src/reload/launch.rs:1–15](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/reload/launch.rs#L1-L15), [crates/muxa-cli/src/reload/launch.rs:98–110](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/reload/launch.rs#L98-L110)). | Any actual restart receives a fresh ID through the same executor. Restore may retain historical metadata, but must not replay an old active launch ID or pane mark. |
| Headless CLI agents (`ask`) | `ask` sanitizes inherited session environment, executes a CLI, and receives a parsed session through its execution observer ([crates/muxa/src/ask.rs:2088–2128](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/ask.rs#L2088-L2128)); the streaming path owns the child handle ([crates/muxa/src/ask/cli_stream.rs:17–28](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/ask/cli_stream.rs#L17-L28)). | Give each CLI execution its own launch record and process receipt, retaining its headless origin and no pane. API-only providers do not create an OS process and need a request identity instead; do not pretend they have a PID ([crates/muxa/src/ask.rs:2120–2125](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/ask.rs#L2120-L2125)). |

This extends existing creation history, rather than treating it as live ownership.
`spawn_provenance` records only launches with a known creator, archives
`spawns/<launch_id>.json`, and stamps `@muxa_spawn`
([crates/muxa-cli/src/spawn_provenance.rs:40–61](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/spawn_provenance.rs#L40-L61),
[crates/muxa-cli/src/spawn_provenance.rs:70–103](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/spawn_provenance.rs#L70-L103)). A creator-less human launch
still needs D1. Preserve creator history after the live binding is revoked.

## Identity model

Proposed types, not fields currently implemented:

```text
LaunchId       = UUID for one physical launch attempt/process generation
ProcessKey     = (execution_host, boot_identity, pid, os_start_identity)
ServerKey      = (execution_host, canonical_endpoint, server_incarnation)
PaneInstance   = (ServerKey, pane_id, launch_id)
NativeInstance = (execution_host, daemon_session_id, launch_id)
SessionBinding = (provider, provider_session_id, launch_id, ProcessKey, evidence)
```

Use existing `PaneKey` host/socket/session/window ancestry for topology, plus
an explicit server incarnation for lifetime fencing. `PaneKey` currently has
that ancestry but no process-generation field, and `PaneInfo` supplies stable
session/window IDs, not a server incarnation
([crates/muxa/src/topology.rs:39–54](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/topology.rs#L39-L54), [crates/muxa/src/tmux/mod.rs:574–607](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/tmux/mod.rs#L574-L607)).
On local tmux, obtain the server's actual PID/start identity and host boot
identity through the backend. On remote rmux require a server-instance UUID or
equivalent generation from that execution host. Do not assume its endpoint
string or `%N` proves an incarnation. An adapter unable to expose one cannot
certify a reusable pane binding; add that backend capability before promotion.

The provider conversation ID and native PTY session ID remain distinct from
`LaunchId`. Existing `AgentId.session_id` already means provider runtime session,
while the PTY backend independently creates `MUXA_SESSION_ID`
([crates/muxa/src/event.rs:138–154](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/event.rs#L138-L154), [crates/muxa/src/session.rs:287–289](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/session.rs#L287-L289)).
Do not repurpose either, or change existing row keys in D1. Store launch-instance
relations separately. If one provider conversation is active in two launches,
a bare conversation selector is ambiguous: require a launch selector for an
account-changing or destructive operation. Never collapse them by recency.

A launcher invoked from an agent must replace any inherited `MUXA_LAUNCH_ID`.
Parent creation provenance is a separate relation. Provider-created subagents
may retain the environment for family tracing, but cannot claim the parent's
root binding. The same rule covers CLI/app environments inherited from a shell;
the app currently passes its process environment to its bundled CLI
([apps/muxa-macos/Sources/AppModel+Agents.swift:310–324](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/apps/muxa-macos/Sources/AppModel+Agents.swift#L310-L324)).

## Launch transaction and carriers

Introduce a small internal launch executor with a start gate. This is a proposed
primitive; current `Dialect::exec_line` only replaces the POSIX shell or invokes
the PowerShell command ([crates/muxa/src/shell.rs:98–106](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/shell.rs#L98-L106)).

1. **Prepare:** execution-host launch authority writes an immutable intent with
   ID, provider, requested account decision, expected conversation when known,
   operation/worker ticket, and launch surface kind. It reserves an idempotency
   key for one physical create attempt. Persist before invoking the backend.
2. **Create gated:** start a helper that cannot invoke the provider yet. Its
   receipt names its own PID/start identity. Bind the exact returned pane and
   endpoint/server incarnation, or the daemon-owned native session. A lost
   create reply is `uncertain`, not permission to repeat the create.
3. **Stamp and commit:** set `@muxa_launch_id` on that exact pane, commit the
   process/surface receipt, then acknowledge the helper's release. A provider
   hook cannot race ahead of its intent. A helper that receives no release
   exits without starting the provider. Explicit cancellation revokes the ID;
   elapsed time alone never certifies that an uncertain create failed.
4. **Exec:** on POSIX, export the ID into the new exec environment and replace
   the gate with the provider for an exec-transparent launch. Conceptually:
   `export MUXA_LAUNCH_ID='<uuid>'; exec <provider argv>`. Use `Dialect` quoting,
   not string concatenation. The helper PID/start identity is then retained
   across exec. PowerShell needs its own `$env:MUXA_LAUNCH_ID` assignment and an
   exact child-process receipt; it has no POSIX exec semantics
   ([crates/muxa/src/shell.rs:36–48](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/shell.rs#L36-L48), [crates/muxa/src/shell.rs:98–106](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/shell.rs#L98-L106)).
5. **Observe:** verify exec/provider identity and join provider conversation
   evidence. A successful pane-create or gate registration is not proof that
   the provider exec succeeded. Record exec failure explicitly; otherwise stay
   `spawned_unbound` until evidence arrives. Return `launch_id` even when the
   provider has not emitted a hook yet, with a separate verification status.

Do not export the ID into the tmux server's global environment: that would
mark unrelated windows. Write it into **each exec line** and the pane option.
A pane option is a locator/cross-check, never sole process or account proof.
The existing post-create worker mark demonstrates the create-to-stamp gap
([crates/muxa-cli/src/agent_launch.rs:840–860](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/agent_launch.rs#L840-L860)); gating closes it for verified
launches rather than hoping a hook arrives later.

### Account wrappers and actual provider PID

Do not assume an account wrapper preserves its PID or environment. Today
`RoutedLaunch` may prepend `aas e <account>` or an account environment prefix;
`on_account` substitutes that whole command for the provider command
([crates/muxa/src/account_route.rs:74–99](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/account_route.rs#L74-L99),
[crates/muxa-cli/src/agent_launch.rs:119–139](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/agent_launch.rs#L119-L139)).

The execution receipt must identify the process that actually runs the provider,
not merely the wrapper. A configured-env adapter can put the executor immediately
before the real provider. A wrapper that forks needs an explicit child/provider
receipt or a provider record tying the exact child to the intended session.
The current `aas e` contract shown here does **not** establish arbitrary-command
passthrough; do not invent `aas ... -- muxa ...` syntax. Until a tested adapter
or provider receipt supplies this link, carry the ID but leave process/session
verification and account attribution unknown. Do not select a descendant by
basename, shallowest depth, first sighting, or a unique remaining candidate.
This is a rollout gate, not a new fallback heuristic.

### Claude

For a fresh Claude conversation, allocate a separate expected session UUID and
pass `--session-id` exactly once, recording it in the prepared intent. Honor an
explicit valid session choice; reject conflicting resume/fork/session arguments
before creating a surface. A swap's resume keeps the known provider ID and
receives a new launch ID. Existing parsing already recognizes Claude session
files and session-selection flags ([crates/muxa/src/agent_conversation.rs:23–29](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/agent_conversation.rs#L23-L29),
[crates/muxa/src/account_swap.rs:921–929](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/account_swap.rs#L921-L929)); the current generic builder does not
allocate a provider session ([crates/muxa-cli/src/agent_launch.rs:70–109](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/agent_launch.rs#L70-L109)).
The [Claude CLI reference](https://code.claude.com/docs/en/cli-reference) documents
`--session-id` as a UUID-valued conversation selector.

The expected session UUID plus exact process evidence prevents an inherited ID
on a nested Claude from becoming the root conversation. An exact Claude
`sessions/<pid>.json` may corroborate it only when PID **and start identity**
match, and all matching records agree; this is already the contract in
[crates/muxa/src/agent_conversation.rs:66–89](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/agent_conversation.rs#L66-L89). The file does not currently carry
a muxa launch ID, so it must be joined through the registered ProcessKey or
that process's readable launch environment, never a filename alone.

### Codex and shared servers

Do not pass Claude's flag to Codex or equate a thread UUID with a launch UUID.
The current builder simply appends configured options/prompt; workers alone
force `--no-daemon` to avoid borrowed pane environments
([crates/muxa-cli/src/agent_launch.rs:70–109](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/agent_launch.rs#L70-L109),
[crates/muxa-cli/src/agent_launch.rs:742–748](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/agent_launch.rs#L742-L748)).

Prefer a private, owned Codex execution for the initial verified implementation,
subject to a version-tested provider adapter. A hook must provide a demonstrable
provider-origin process identity equal to the registered provider ProcessKey,
plus its structured thread ID. The hook transport's caller PID is usually a
helper, not automatically the provider PID: IPC currently records its peer's
PID ([crates/muxa/src/ipc/identity.rs:171–188](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/ipc/identity.rs#L171-L188)). Walking from that helper to
*some* ancestor carrying the ID does not distinguish a nested provider.
A verified immediate execution relationship or provider-owned process/thread
receipt is required. **Do not pin the first hook that happens to carry the ID.**

For a shared app-server, ignore its inherited launch/pane environment for thread
ownership. Require an explicit frontend/connection-to-thread receipt from a
provider API connection owned by the launch. A generic `threadId` proves which
conversation sent an event, not which terminal owns it. Whether the supported
Codex version exposes this link is an implementation spike, not an API promised
by this design. If it does not, shared-server sessions remain unknown; promote
private mode only after its exact hook-origin contract passes the tests below.

This deliberately does not claim an environment variable alone solves C3.
Today's `codex_binding` uses resume argv, screen prompts, and cwd elimination
([crates/muxa/src/codex_binding.rs:1–25](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/codex_binding.rs#L1-L25),
[crates/muxa/src/codex_binding.rs:90–106](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/codex_binding.rs#L90-L106)). Such observations may remain labeled
for display during rollout but cannot replace or revoke a verified launch
binding. `/new` needs an explicit process/connection-scoped thread-switch
receipt; append a new SessionBinding under the same live launch and close the
old active association. It is not another process launch. Without that receipt,
the new thread remains unknown instead of stopping the previous owner.

## Daemon join rule

Run this on hook ingest, row creation, and launch receipt arrival; arrival order
does not matter. Extend the hook normalization boundary, where `AgentId` and its
surface/endpoint are currently stamped
([crates/muxa/src/adapters/hook.rs:84–105](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/adapters/hook.rs#L84-L105)).

1. Treat forwarded `launch_id`, PID and pane as claims. Resolve a prepared,
   nonterminal launch on the **execution host**, never by a controller's local
   `/proc`. Authenticate registration by its owned spawn/gate connection and
   peer credentials. The ID is correlation metadata, not a secret or authority
   to mutate any launch record.
2. Read the exact registered provider PID's start identity, then its NUL-delimited
   environment, then start identity again. Require the same ProcessKey before
   and after and the exact `MUXA_LAUNCH_ID`. An already verified owned-exec
   receipt or exact provider record may provide the alternate proof where
   environment inspection is unavailable. Preserve the source of that proof.
3. Require the provider session/thread evidence to name that same provider
   process or owned connection. For Claude also require the prepared expected
   session; for Codex apply the adapter contract above. No arbitrary-ancestor
   match and no substitution of the hook helper PID for the provider PID.
4. Cross-check the current pane mark, endpoint and server incarnation against
   the committed receipt, or validate the daemon-owned native session. Reject
   conflicting sources. A temporary backend outage gives unknown, not “pane
   closed.” The endpoint matters: current `AgentId` explicitly records it
   because pane IDs repeat across servers ([crates/muxa/src/event.rs:148–154](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/event.rs#L148-L154)).
5. Persist an immutable SessionBinding and publish the row's verified instance
   in the same ordered commit. Replayed evidence is idempotent. An attempt to
   bind that process generation to a different launch is a conflict, not a
   last-writer-wins update. Subsequent quota/liveness consumers use the pinned
   record, never recompute a winner from the current row set.

Unknown means no account credit, no identity-based stop/swap/rebind authority,
and no automatic retry of a launch whose outcome is uncertain. Preserve normal
provider status/display, with a reason such as `legacy_launcher`,
`environment_unreadable`, `provider_origin_unproved`, `server_unreachable`, or
`conflicting_evidence`. An older unmarked row may gain proof later; absence of
proof never clears or steals another row's verified binding. An exact external
provider record may establish external identity without fabricating a muxa
launch history; the Claude reader already returns Unknown/Conflict/Unstable
rather than falling back to a registry guess
([crates/muxa/src/agent_conversation.rs:32–44](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/agent_conversation.rs#L32-L44)).

## Lifetime and recovery

Proposed state machine:

```text
prepared -> spawned_unbound -> verified -> exited / revoked
    |              |
    +-> failed     +-> uncertain -> verified or revoked by exact evidence
```

Terminal states never return to active. Keep a tombstone so an old ID cannot be
adopted again; garbage-collect details only after no binding/swap/pipeline record
references them, retaining enough terminal identity to reject replay.

- **Definite failure:** validation/create rejection, proven exec failure, or a
  canceled gate invalidates the record. If the pane is known to be ours, clean
  up that exact PaneInstance. A lost acknowledgement or failed listing is
  uncertain, not definite failure. The worker launch path already separates
  definite create rejection from uncertain outcomes
  ([crates/muxa-cli/src/agent_launch.rs:799–812](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/agent_launch.rs#L799-L812)).
- **Exit/closed pane:** an exact process exit or confirmed pane destruction on
  the matching server incarnation revokes active routing/binding. Keep earned
  account caps/history. A matching PID with a different start identity is a
  different process. Liveness must check ProcessKey, not merely `pid_alive` as
  current task reaping does ([crates/muxa/src/state.rs:2091–2100](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/state.rs#L2091-L2100)).
- **Pane ID reuse:** a new process must get a new ID, even if `%N`, endpoint,
  cwd, provider session or account is the same. A copied pane option or restored
  `@muxa_spawn` does not revive its tombstone. An old mark with a live foreign
  process is a conflict, never evidence to kill that process.
- **tmux/rmux server restart:** change server incarnation, invalidate all old
  PaneInstances, and require new launch/adoption receipts. Reused socket paths,
  session/window IDs and pane IDs cannot validate old records. A surviving
  provider may be recorded as detached, but cannot automatically reacquire a
  pane by its old ID.
- **Daemon restart only:** reload durable intents/tombstones, then revalidate
  exact processes and the unchanged backend incarnation. The daemon's own PID
  is not the tmux server generation. Never replay a physical create during
  recovery. Native sessions that cannot be recovered with their exact owning
  PTY handle remain detached/unknown, even if their child still exists.
- **Unavailable platform information:** keep the record unknown; do not call a
  missing environment or unreadable start time “dead.” Destructive consumers
  require their existing authorization plus a current ProcessKey fence. D1
  supplies identity evidence, not permission to terminate an agent.

## Wire, storage, and compatibility

Proposed additive fields use `#[serde(default, skip_serializing_if =
"Option::is_none")]`. Existing optional surface/socket/PID fields supply the
pattern ([crates/muxa/src/event.rs:141–154](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/event.rs#L141-L154),
[crates/muxa/src/state.rs:177–206](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/state.rs#L177-L206)).

| Boundary | Proposed addition |
| --- | --- |
| `AgentId` / hook envelope | `launch_id` and optional provider-origin process claim; evidence is verified by the daemon, not trusted from JSON. |
| `Agent` snapshot / Fleet projection | Optional `launch_identity` with launch ID, ProcessKey, surface instance, evidence source and verification/revocation status. Keep `Agent.pid`'s existing Task semantics; do not silently make legacy PID-only reaping govern providers. |
| `AgentStartResult` and Swift result | Optional `launch_id` plus optional verification status; old outputs stay readable. The Rust/Swift envelopes currently expose placement/session/pane but no launch ID ([crates/muxa/src/agent_control.rs:120–147](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/agent_control.rs#L120-L147), [apps/muxa-macos/Sources/AgentLaunch.swift:343–360](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/apps/muxa-macos/Sources/AgentLaunch.swift#L343-L360)). |
| Native `SpawnSession` / session result | Optional typed launch intent/reference and launch ID. The daemon injects reserved environment after validating the intent, then returns the actual child/session receipt. Today `SpawnSession` carries generic env and `SessionRef` has an optional PID ([crates/muxa/src/session.rs:65–75](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/session.rs#L65-L75), [crates/muxa/src/session.rs:126–137](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/session.rs#L126-L137)). |
| Control requests | Capability-gated `prepare/register/query/revoke launch` operations and optional launch selectors on operations that need exact identity. Only the owning execution host registers or revokes. |

Advertise `launch_identity_v1` through hello capabilities. Keep wire protocol 6
if mixed-version fixture tests confirm these additions are tolerated; capability
tags already gate additive features ([crates/muxa/src/event.rs:15](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/event.rs#L15),
[crates/muxa/src/ipc/protocol.rs:836–839](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/ipc/protocol.rs#L836-L839)). Old daemons do not know new request
kinds: never send them without negotiation. New clients can still launch through
legacy paths, carrying an ID where possible, but must label the result
unverified. New daemons accept old event shapes and leave their identity unknown.
Do not add mandatory fields, change provider/session ID meaning, or serialize a
new enum value that an old reader must understand. Use optional status fields
with a tolerant representation; test Rust and Swift decoders, not just Serde.

**No config keys or migration of config.toml.** Native identity is a dedicated
internal field, not a client-added env exception: the current native environment
allowlist intentionally contains only terminal/path/locale keys
([crates/muxa/src/agent_control.rs:26–39](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/agent_control.rs#L26-L39)). Existing account config structs
reject unknown fields ([crates/muxa/src/accounts.rs:29–38](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/accounts.rs#L29-L38)); repeat none of
#300's cross-version config failure.

Use a separate versioned runtime store under `$XDG_DATA_HOME/muxa/launches/`,
with 0700 directory/0600 files and atomic durable commits. Record intent,
process/surface receipts, bindings, terminal tombstones and monotonically
ordered revisions. The committed launch journal is canonical; Agent/account
projections publish only after its revision commits and replay idempotently. Do
not claim that renaming two independent files is a cross-store transaction.
A single execution-host authority serializes updates; any
CLI recovery spool uses exclusive creation/locking and must be adopted through
that authority before verification. Detect unsupported store versions and leave
identity unknown without rewriting them. This is a live registry distinct from
the existing immutable creator archive
([crates/muxa-cli/src/spawn_provenance.rs:95–103](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/spawn_provenance.rs#L95-L103)).

## Platforms and evidence availability

- **Linux:** reuse the exact-PID NUL parser and Linux start-tick reader
  ([crates/muxa/src/accounts.rs:296–300](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/accounts.rs#L296-L300),
  [crates/muxa/src/accounts.rs:385–400](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/accounts.rs#L385-L400),
  [crates/muxa/src/agent_conversation.rs:92–116](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/agent_conversation.rs#L92-L116)). Include boot identity and
  read start identity before/after environment inspection. `/proc/<pid>/environ`
  reflects the initial exec environment, not later `putenv` changes, and access
  is subject to a ptrace credential check; this is why injection must happen
  before exec ([Linux proc_pid_environ(5)](https://www.man7.org/linux/man-pages/man5/proc_pid_environ.5.html)).
- **macOS:** the current helper uses sysinfo, then text-parsed `ps eww`; neither
  an empty result nor its text fallback can certify D1
  ([crates/muxa/src/accounts.rs:303–353](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/accounts.rs#L303-L353)). The examined XNU source permits
  `KERN_PROCARGS2` environment omission for restricted targets and also checks
  caller credentials; same UID alone does not guarantee readable environment
  ([kern_sysctl.c:1251–1369](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/kern/kern_sysctl.c#L1251-L1369)).
  Parse argv count and NUL-delimited env separately. Use a new macOS ProcessKey
  adapter with kernel start seconds/microseconds; XNU exposes those fields
  ([proc_info.h:54–77](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/sys/proc_info.h#L54-L77)).
  Do not compare them to Linux Claude `procStart` ticks: muxa currently returns
  no compatible process start off Linux
  ([crates/muxa/src/agent_conversation.rs:92–105](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/agent_conversation.rs#L92-L105)). Validate supported macOS
  releases/signing modes explicitly. No requirement to disable SIP, run as root,
  or obtain a private entitlement. Where blocked, use an owned execution receipt
  or a compatible exact provider record, otherwise unknown.
- **Other hosts / remote execution:** inspect on the owning host and forward
  the typed receipt; never inspect the controller's same-numbered PID. Windows
  needs an equivalent creation-time/process-handle fence and actual child
  receipt; current non-Linux environment code can return unavailable
  ([crates/muxa/src/accounts.rs:310–333](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/accounts.rs#L310-L333)). Unsupported adapters stay unknown.

OS/provider references were checked on 2026-11-05. They describe external
contracts, not functionality newly claimed for muxa by this document.

## PR #365 round-3 follow-ups on top of D1

These are additional work after the identity foundation, not fixes already on
main. References in this section are pinned to PR #365 head `9602fce`.

### 1. Per-window quota provenance

The PR attributes all row quota fields using one `rate_limits_at` against
`bound_at` ([crates/muxa/src/account_provider.rs:314–329](https://github.com/Open330/muxa/blob/9602fce0d9c5a6c694de5855ba69df84846271f4/crates/muxa/src/account_provider.rs#L314-L329)), while main's
Claude heartbeat retains absent windows and updates present ones independently
([crates/muxa/src/state.rs:1045–1066](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/state.rs#L1045-L1066)). A fresh 5h observation must not make a
retained pre-swap 7d value belong to the new account.

Follow-up: store provenance separately for 5h, 7d and explicit cap observations:
`{value, reset, source_observed_at, source_cursor, launch_id, provider_session}`.
Partial updates replace only the corresponding sample. Attribute each sample
only through its own verified launch/account binding. Do not stamp ingestion
or reconciliation time onto a cached record. In particular, do not label an old
rollout with the **current** launch ID merely because it was reread after swap.
Capture a source-generation/cursor boundary at handoff; samples without evidence
of their writer's generation remain display-only/unknown. D1 provides the key;
per-window/source propagation is still required. A timestamp can reject stale
samples but cannot establish process ownership by itself.

### 2. Pinned binding independent of the row set

The PR chooses a pane binding by earliest `started_at` among rows supplied to
`binding_for`, later pins it, and `leave` can consult a singleton row set
([crates/muxa/src/account_provider.rs:279–311](https://github.com/Open330/muxa/blob/9602fce0d9c5a6c694de5855ba69df84846271f4/crates/muxa/src/account_provider.rs#L279-L311),
[crates/muxa/src/account_provider.rs:427–448](https://github.com/Open330/muxa/blob/9602fce0d9c5a6c694de5855ba69df84846271f4/crates/muxa/src/account_provider.rs#L427-L448),
[crates/muxa/src/account_provider.rs:457–473](https://github.com/Open330/muxa/blob/9602fce0d9c5a6c694de5855ba69df84846271f4/crates/muxa/src/account_provider.rs#L457-L473)). Filtering/removal of the
real first agent can change such a choice; once a binding is pinned, a lookup
must not recover another account through a weaker fallback.

Follow-up: commit account decision and immutable `(launch_id, ProcessKey,
provider_session)` pin at the successful exact join. All read-only ranking,
transition recording and swap `leave` calls look up that same persisted pin,
not a ranking recomputed from their local slice. Missing, conflicting or stale
pins are unknown; never use provider-record/environment fallback to override a
known binding whose sample is too old. A nested or later agent, reused pane,
or row removed by GC cannot inherit it. A new account/swap gets a new launch,
and therefore a distinct binding, even when the conversation ID is reused.

### 3. Shutdown drain after the last producer

The PR ledger subscribes to the general shutdown broadcast, takes a final
snapshot on that signal, and is awaited before other Store producers
([crates/muxad/src/main.rs:3601–3616](https://github.com/Open330/muxa/blob/9602fce0d9c5a6c694de5855ba69df84846271f4/crates/muxad/src/main.rs#L3601-L3616),
[crates/muxad/src/main.rs:3652–3687](https://github.com/Open330/muxa/blob/9602fce0d9c5a6c694de5855ba69df84846271f4/crates/muxad/src/main.rs#L3652-L3687),
[crates/muxad/src/main.rs:616–630](https://github.com/Open330/muxa/blob/9602fce0d9c5a6c694de5855ba69df84846271f4/crates/muxad/src/main.rs#L616-L630)). Awaiting it does not prevent it from
having already exited before those producers' last events. Main's dedicated
activity/writer/snapshot shutdown stages are the model to extend
([crates/muxad/src/main.rs:585–636](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxad/src/main.rs#L585-L636)).

Follow-up order: stop accepting creates; drain accepted launch/IPC/swap work;
await all direct Store producers; then send a **dedicated** provenance/account
writer barrier; drain every accepted launch/binding/quota event through that
revision and fsync the ledger; finally finish the existing state snapshot stage.
CLI/swap writers must submit through that same acceptance fence; work arriving
after admission closes is rejected or durably pending, not silently written
outside the barrier. Legacy direct writers do not acquire this new guarantee.
Quota events must retain the attributed sample even if the row stops or is
removed before the barrier. A final snapshot alone cannot recover such an event;
use ordered durable event acceptance/backpressure, not an unbounded assumption
that broadcast lag never occurs. A timeout/write failure leaves recovery
explicitly incomplete; it never reports a successfully persisted binding/cap.
D1 supplies identity for the queued events, not the drain ordering automatically.

### 4. Native bind without a pane

PR #365's account bind is after tmux discovery, while `run_native` returns the
PTY result without that bind
([crates/muxa-cli/src/agent_launch.rs:943–948](https://github.com/Open330/muxa/blob/9602fce0d9c5a6c694de5855ba69df84846271f4/crates/muxa-cli/src/agent_launch.rs#L943-L948),
[crates/muxa-cli/src/agent_launch.rs:617–636](https://github.com/Open330/muxa/blob/9602fce0d9c5a6c694de5855ba69df84846271f4/crates/muxa-cli/src/agent_launch.rs#L617-L636)). Main already owns the
native child PID and creates a surface Task row from it
([crates/muxa/src/session.rs:291–310](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/session.rs#L291-L310),
[crates/muxa/src/ipc/dispatch.rs:2534–2550](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/ipc/dispatch.rs#L2534-L2550)).

Follow-up: pass the same resolved account decision and launch intent to the
native spawn authority; commit `(launch_id, native_session, exact provider
ProcessKey, account)` before releasing the process. Join its provider hook to
that receipt; retain `pane = None`. Bind on both tmux and native through one
surface-independent operation, not by calling a pane-only helper with a made-up
pane. Apply the same wrapper limitations, per-window sample rules and rollback
as other surfaces. The accompanying Task row and provider row share a receipt;
they do not become two competing account owners.

## Test plan and rollout

Tests below are implementation acceptance criteria. This documentation PR does
not add tests or change runtime code.

| Layer | Required cases |
| --- | --- |
| Carriers | Real child process on each supported OS reads its initial ID; quotes/spaces/newlines in prompt or account env do not change it; inherited parent ID is replaced; unrelated panes never inherit a server-global ID. Claude argv contains one recorded session UUID; resume keeps its conversation but gets a fresh launch. |
| Exact join | Hook before/after registration yields the same pin; repeated hook/receipt is idempotent; wrong/missing ID, PID reuse, changed start identity during read, and mismatched provider session are unknown/conflict. A nested process with the same environment and a different provider session cannot pin. Test opaque/forking wrappers, not just direct exec. |
| Codex | Private supported hook-origin contract; shared server with two frontends and two thread IDs; first-pane borrowed env; two panes at one cwd; identical prompts; `/new`; a second thread arriving first. Only an explicit process/connection receipt may bind. Missing provider support must keep all these cases unknown. |
| Lifetime | Exec failure; pane closed before/after hook; lost create reply and crash at every prepare/create/stamp/release boundary; daemon restart versus backend restart; reused socket/PID/pane IDs; copied restore mark. No case creates twice or revives a tombstone. Backend timeout is not pane destruction. |
| Surface matrix | tmux and rmux pane/window/session placement, workers/tickets, native PTY with account route, swap typed and respawn paths, both pipeline launch functions, local/Fleet Muxa.app IPC and bundled-CLI fallback, reload, and headless CLI ask. Verify actual provider PID, not just wrapper PID. |
| Consumers | Remove/reorder/filter the true bound row, then rank/record/swap from a singleton nested row: no rebinding. Old 7d + fresh 5h after account switch, explicit cap with a separate source, unchanged Codex rollout reread, and stopped/removed row during shutdown: no cross-account credit. |
| Drain | Pause an accepted producer after general shutdown, let it emit a cap/binding, then finish; the dedicated final barrier must persist it before exit. Also exercise lag/backpressure, write failure, and crash recovery. |
| Compatibility | Old/new Rust JSON round-trips with all proposed fields missing/present; old Swift result decode; unknown statuses; no new request sent to an old daemon; unchanged config loads on both versions; unsupported/newer store remains untouched. |
| Platforms | Linux restricted `/proc`, PID reuse and boot fencing; supported macOS versions, signed/restricted and ordinary providers, denied/omitted env and unavailable start identity; remote same-numbered PID; Windows child receipt or explicit unknown. No `ps` text or process-name fallback may pass a verification test. |

Ship in stages, with each consumer disabled until its proof contract passes:

1. **Contract spikes:** validate Codex's exact process/thread receipt, account
   wrapper handoff, and backend server incarnation; establish macOS ProcessKey
   and environment availability. A missing contract is a documented unknown,
   not an invitation to use “first seen.” No API or flag syntax is assumed from
   a process name.
2. **Carriers and registry:** add optional wire fields, hello capability, durable
   intents/tombstones and gated executor; wire every surface in the inventory.
   Observe and test receipts without changing account attribution or stopping
   legacy agents. Old launchers/daemons remain explicitly unverified.
3. **Verified joins:** enable Claude and each tested private-provider adapter,
   then native/App/Fleet/worker/pipeline/swap/reload paths. Shared Codex remains
   unknown until its connection proof exists. Confirm the full matrix before
   claiming that all muxa launches can be deterministically bound.
4. **Consumers:** replace account pane/timing joins with pinned launch bindings,
   implement per-window provenance, then the dedicated writer drain. Migrate
   C3 binding and C1/C10 liveness to the same ProcessKey. Existing heuristic
   evidence can veto or annotate, never overwrite a verified binding or grant
   destructive authority.
5. **Retire legacy inference for marked launches:** remove five-second discovery
   registration and any first-row/timestamp ownership choice once receipts
   cover them. Keep external/manual agents unknown unless provider evidence
   proves them; do not manufacture launch records for historical observations.

Release gates: all surface/negative tests pass; mixed-version decoding and the
full workspace checks pass; unknown reasons are visible in JSON/UI; every
account-changing or stop operation is fenced by a current verified instance.
Rollback stops promoting new identities while retaining records and tombstones;
it does not rewrite config or reinterpret old rows as proven launches.
