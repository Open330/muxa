# Launch identity and provenance (D1)

Status: implementation design, revised after cross-provider review (2026-11-05).
This PR changes documentation only. Code references use main `c619904ab8e803e99573cec5f15c517e8592f3e8`;
PR #365 references are separately pinned to `9602fce`.
See [DETERMINISTIC_FIRST.md](DETERMINISTIC_FIRST.md), D1 and C1/C3/C9/C10.

## Decision and scope

Give each physical launch a fresh UUID, export it as `MUXA_LAUNCH_ID`, and stamp
`@muxa_launch_id` on its pane. Bind only through a create receipt and an exact
provider record. A missing or conflicting proof is `unknown(reason)`, never a
choice by timing, arrival order, cwd, screen text, or process name.

The first implementation PR is **“launch receipt: tmux/rmux + Claude,
display-only.”** It publishes `Agent.launch_identity` only in
`muxa agents --json`. Account routing/ranking, stopping, swapping, liveness,
Codex pane binding and other UI behavior do not consume it in that PR. Native
comes next; Codex fd validation is a separate spike; consumers follow last.
Each stage is gated only by the tests for the paths it changes.

The launcher already allocates a UUID or takes a worker ticket's `launch_id`,
but currently builds the provider exec line without exporting it
([crates/muxa-cli/src/agent_launch.rs:737–750](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/agent_launch.rs#L737-L750),
[crates/muxa-cli/src/agent_launch.rs:778–787](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/agent_launch.rs#L778-L787)). Reuse the ticket ID for that
same physical attempt; a genuinely new process launch gets a new ID. Never
inherit the caller's ID. Provider conversation IDs and native PTY session IDs
remain separate identities ([crates/muxa/src/event.rs:138–154](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/event.rs#L138-L154),
[crates/muxa/src/session.rs:287–289](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/session.rs#L287-L289)).

## Existing proof sources

### Claude: receipt PID to provider session record

Use `agent_conversation::claude_conversation(receipt.pid)`. It reads
`sessions/<pid>.json`, validates `pid`, `procStart`, and `sessionId`, and rejects
unstable or conflicting records
([crates/muxa/src/agent_conversation.rs:23–29](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/agent_conversation.rs#L23-L29),
[crates/muxa/src/agent_conversation.rs:66–89](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/agent_conversation.rs#L66-L89),
[crates/muxa/src/agent_conversation.rs:165–176](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/agent_conversation.rs#L165-L176)).

For a fresh Claude launch, generate an expected session UUID and pass
`--session-id <uuid>` once. Do not inject it when resume/session-selection flags
are already present, including their short or `=` forms; preserve explicit
choices rather than changing resume/fork semantics. An explicit UUID becomes
the expected session; a selector resolved by Claude is checked against its exact
process record. The current builder appends options/prompt, while swap parsing
already recognizes session-selection flags
([crates/muxa-cli/src/agent_launch.rs:70–109](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/agent_launch.rs#L70-L109),
[crates/muxa/src/account_swap.rs:921–929](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/account_swap.rs#L921-L929)).
The [Claude CLI reference](https://code.claude.com/docs/en/cli-reference)
documents the UUID-valued `--session-id` option.

For a fresh direct launch, the create receipt's ProcessKey plus the exact
Claude record and expected session UUID are the whole proof. **No additional
start/environ/start scan is needed on this main path.** Keep the reader's own
start-time stability check. A nested Claude has a different process generation
and session record even if it inherits `MUXA_LAUNCH_ID`; its hook cannot claim
the receipt PID's session.

### Codex: exact PID to an open rollout fd (stage 3 spike)

The review supplied these live observations: one private `codex --no-daemon`
process held exactly one rollout JSONL fd (its own) in 3 of 3 samples taken
one second apart, while three shared `app-server daemon` processes held no
rollout fds. These are evidence for
a spike, **not yet a lifetime guarantee**. The repository already knows the
`rollout-…-<threadId>.jsonl` naming scheme, but currently locates files by
session-ID/date scans ([crates/muxa/src/adapters/codex_rollout.rs:534–550](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/adapters/codex_rollout.rs#L534-L550)).

The concrete candidate receipt is:

```text
create receipt -> exact provider ProcessKey
               -> that PID's open rollout fd -> structured thread ID
```

On Linux inspect `/proc/<pid>/fd` for that exact PID. Validate the fd target and
its `session_meta` identity, not a recent filename in the directory. Recheck
the ProcessKey and fd identity around inspection. Do not use a descendant scan,
newest rollout, or the first matching file as proof. Multiple unresolved thread
fds mean unknown. The spike must answer:

- Does the fd persist throughout an idle/active thread's lifetime?
- On `/new`, when does the old fd close and the new one open? Can overlap be
  resolved from provider records without arrival-order inference?
- Can `session_meta.source` reliably distinguish root and subagent threads,
  including format/version changes and resumed threads?
- Does the exact PID returned by the launch receipt own the fd on supported
  provider builds, rather than a wrapper or shared server?

On macOS enumerate descriptors with `proc_pidinfo(PROC_PIDLISTFDS)`, then vnode
paths with `proc_pidfdinfo(PROC_PIDFDVNODEPATHINFO)`; these interfaces are defined
in [XNU proc_info.h:674–731](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/sys/proc_info.h#L674-L731).
Permission denial remains unknown. Shared servers stay unknown: their borrowed
environment cannot identify a frontend. Retain the existing shared-server
hook guard ([crates/muxa/src/adapters/hook.rs:68–83](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/adapters/hook.rs#L68-L83)). There is no assumed
private Codex hook-origin contract or new provider API in this plan.

### Native: reuse ownership already supplied by muxad (stage 2)

Native D1 is nearly free: the PTY backend already sets `MUXA_SESSION_ID`, owns
the spawned child PID/handle, and the hook adapter reads that session ID
([crates/muxa/src/session.rs:287–304](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/session.rs#L287-L304),
[crates/muxa/src/adapters/hook.rs:170–184](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/adapters/hook.rs#L170-L184)). Link the fresh launch ID to this
existing native session and exact owned child, then apply the same provider
record check. No pane discovery, fake tmux pane, or executor handshake is needed.
`run_native` already submits its environment through `spawn_session`
([crates/muxa-cli/src/agent_launch.rs:596–616](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/agent_launch.rs#L596-L616)).

## Create receipt and order-independent join

For local tmux, extend the single create reply on **all three** shapes:
`split-window`, `new-window`, and `new-session`:

```text
-P -F "#{pane_id} #{pane_pid} #{pid} #{start_time}"
```

The code currently asks only for `#{pane_id}` at each call site
([crates/muxa-cli/src/agent_launch.rs:1341–1356](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/agent_launch.rs#L1341-L1356),
[crates/muxa-cli/src/agent_launch.rs:1359–1368](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/agent_launch.rs#L1359-L1368),
[crates/muxa-cli/src/agent_launch.rs:1389–1401](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/agent_launch.rs#L1389-L1401)). The review verified that a
direct POSIX exec launch returns the provider as `pane_pid`, with the exported
launch ID in its environment. An account wrapper is a separate case below.

Use `(pid, /proc start ticks)` for process generation, scoped to execution host
and boot. `#{pid}` and `#{start_time}` identify the tmux server incarnation;
cross-check the server PID with its OS start ticks. Keep tmux's server start
time and Linux ticks as distinct representations, not values to compare
numerically. Include the pinned endpoint. Only rmux/remote backends need a new
capability to supply an equivalent incarnation; local tmux already has it.
Unsupported rmux fields or an unavailable process start yield unknown, not a
retry of a create that may have succeeded. Existing topology already preserves
endpoint/session/window/pane ancestry
([crates/muxa/src/topology.rs:39–54](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/topology.rs#L39-L54)), and Linux start ticks already have a
reader ([crates/muxa/src/agent_conversation.rs:92–116](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/agent_conversation.rs#L92-L116)).

The join has three inputs: persisted launch intent, optional create receipt,
and provider hook/row. They may arrive in any order:

1. Persist intent under the launch ID before create, with provider and expected
   session when known. Use the existing `spawns/<launch_id>.json` location;
   attach the receipt after the create reply. A failed intent write is reported
   before create. No gated executor, release acknowledgement or new journal.
2. Export the fresh ID in the exec line, create once, collect its receipt and
   stamp `@muxa_launch_id` on the returned pane at its pinned endpoint. Keep
   `@muxa_worker_launch` as an alias. A mark failure makes identity unknown;
   never kill/relaunch an already created provider just to repair its display
   metadata. Existing worker stamping provides the call site
   ([crates/muxa-cli/src/agent_launch.rs:840–860](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/agent_launch.rs#L840-L860)).
3. The daemon joins receipt/intent and exact provider record by launch ID and
   ProcessKey. A hook's launch ID is an **optional claim beside the event in
   the ingestion envelope, not a field in `AgentId`**. An early hook waits as
   pending until its receipt is available; no sleeps or “first hook wins.”
4. Require a current matching process generation, server incarnation, pane root
   and mark, or the owned native session. A dead pane is not a live root receipt.
   The hook's session must match the
   receipt PID's provider record. Receipt/hook replay is idempotent; conflicting
   evidence stays unknown. No match is unknown, never inferred.

Piggyback optional receipt/claim metadata on the existing launch result and
`ingest` path, rather than adding prepare/register/query/revoke request families.
Under `launch_identity_v1`, the existing `ingest` envelope also accepts a
receipt-only observation (no fabricated provider event); require an event or a
receipt. Never send that form to an old daemon; old event-bearing payloads
remain valid. A direct CLI launch must deliver/archive its receipt
even if discovery cannot synthesize a row. Pending claims are rechecked by
exact launch ID when receipt metadata becomes available, including archive
lookup; ordering affects when display becomes verified, not which row wins.
The existing ingestion envelope is at
[crates/muxa/src/ipc/protocol.rs:45–52](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/ipc/protocol.rs#L45-L52). Existing discovery registration may
remain for compatibility, but its five-second polling result is not D1 proof
([crates/muxa-cli/src/agent_launch.rs:1032–1073](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/agent_launch.rs#L1032-L1073)).

**Legacy adoption fallback only:** when there is no owned create receipt, read
start identity, the exact PID's NUL-delimited environment, then start identity
again. Require stability and an exact ID/record match. Unreadable environment,
missing start information or an unmarked process stays unknown; `ps` text is
not proof. This is not the normal Claude receipt path. Linux `/proc/environ`
reflects the initial exec environment and has credential-based access limits
([proc_pid_environ(5)](https://www.man7.org/linux/man-pages/man5/proc_pid_environ.5.html));
existing helpers are [crates/muxa/src/accounts.rs:296–300](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/accounts.rs#L296-L300) and
[crates/muxa/src/accounts.rs:385–400](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/accounts.rs#L385-L400). Adoption is not required for stage 1.

## Wrapper decision

The review observed `aas e … codex` as a parent process with Codex in its child.
The current route composes an `aas e <account>` prefix with provider arguments
([crates/muxa/src/account_route.rs:74–99](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/account_route.rs#L74-L99)); treating its receipt PID as the
provider would be wrong. **Stage 1 reports routed/wrapper launches as
`unknown(wrapper_forks)`**, including account="auto" deployments. It is still
useful and shippable for direct launches, and never claims that this existing
account configuration is verified.

For the later consumer phase, choose **(a) configured-env direct exec**:
`AccountProfile::launch_prefix` uses `env` and, for env files, a shell that
explicitly execs its argv ([crates/muxa/src/accounts.rs:118–143](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/accounts.rs#L118-L143)). This is
provable once tests show the create PID is the actual provider. Enable it after
the routed-account integration tests, not by assuming every routing backend
has that behavior.

**(b) Exactly one direct child of the exact wrapper ProcessKey carrying the ID
is not allowed in stages 1–3.** Exact parentage is deterministic evidence, not
candidate elimination, but supporting it needs its own contract/tests for
child replacement, helper children, process generation and provider records.
It may be a later adapter; any such adapter must reject zero or multiple
matching direct children and must not generalize to arbitrary descendants.
Until then forked `aas` launches stay unknown. Shared Codex servers stay unknown
regardless of how many children are visible.

## Lifetime, minimal storage, and compatibility

Stages 1–3 use the daemon snapshot and existing spawn archive, not a versioned
`launches/` journal. The existing archive currently depends on a known creator;
make the new intent/receipt metadata usable even when creator is unknown,
without inventing a parent ([crates/muxa-cli/src/spawn_provenance.rs:40–60](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/spawn_provenance.rs#L40-L60),
[crates/muxa-cli/src/spawn_provenance.rs:70–103](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/spawn_provenance.rs#L70-L103)). Retain creation history as
history. Old archive entries lacking receipts remain unknown.

- Failed exec or a confirmed closed pane invalidates the live binding. A lost
  create response is uncertain, not a failed launch to retry. Existing worker
  handling already makes that distinction
  ([crates/muxa-cli/src/agent_launch.rs:799–812](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/agent_launch.rs#L799-L812)).
- Revalidate ProcessKey and the current pane root on each attempted join.
  PID/pane reuse cannot revive the old record because start ticks or server
  incarnation differ. A copied mark is not sufficient. No durable tombstone
  mechanism is needed for this receipt-based stage.
- A tmux server restart changes `#{pid}`/`#{start_time}` and its OS generation;
  invalidate its old pane bindings even if socket and `%N` repeat. A transient
  listing failure means unknown, not confirmed closure.
- On daemon restart, reload only snapshot/archive metadata that still matches
  the exact live receipt tuple. Missing receipts or unrecoverable native child
  ownership mean unknown. Never replay a physical create from the archive.

Use optional `launch_id` and `launch_receipt` on `AgentStartResult` and optional
`Agent.launch_identity = {launch_id, status: verified | unknown, reason?}`.
Keep richer ProcessKey evidence in the join/receipt; do not change provider
session IDs or Task PID semantics. Optional fields use serde defaults and
omit-none serialization, following existing result fields
([crates/muxa/src/agent_control.rs:120–145](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/agent_control.rs#L120-L145),
[crates/muxa/src/state.rs:177–206](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/state.rs#L177-L206)). Leave `AgentId` unchanged. Gate new envelope
semantics with `launch_identity_v1`; keep protocol 6 if mixed-version fixtures
pass ([crates/muxa/src/event.rs:15](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/event.rs#L15),
[crates/muxa/src/ipc/protocol.rs:836–839](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/ipc/protocol.rs#L836-L839)). No config keys or config migration;
account config rejects unknown keys today ([crates/muxa/src/accounts.rs:29–38](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/accounts.rs#L29-L38)).

A journal, ordered revisions, extra fsync discipline, CLI recovery spool,
tombstones and new launch control requests are **deferred unless a concrete
restart-loss case demonstrates the need**. They are not stage 1–3 prerequisites.

macOS environment visibility is not guaranteed: current code uses sysinfo and
then `ps eww` ([crates/muxa/src/accounts.rs:303–353](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/accounts.rs#L303-L353)), while XNU may omit env
for restricted targets and checks caller credentials
([kern_sysctl.c:1251–1369](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/kern/kern_sysctl.c#L1251-L1369)).
A macOS adapter can use kernel process start fields
([proc_info.h:54–77](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/sys/proc_info.h#L54-L77));
the current Claude-compatible start reader returns None outside Linux
([crates/muxa/src/agent_conversation.rs:92–105](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/agent_conversation.rs#L92-L105)). Unsupported proof remains
unknown without root/private entitlements/SIP changes; it does not block the
Linux stage. PowerShell/Windows are later and initially `unknown(no_exec)`.

## Consumers after the foundation: #365 and C1/C10

These are stage 4, not new consumers in stages 1–3. #365 source references use
its reviewed head, not this document's main base.

1. **Pin by launch ID.** Replace account ownership recomputed from the first
   row in a pane with a stored `(launch_id, ProcessKey, provider_session)` pin.
   The reviewed PR ranks `started_at` in the supplied row slice before pinning
   ([crates/muxa/src/account_provider.rs:279–311](https://github.com/Open330/muxa/blob/9602fce0d9c5a6c694de5855ba69df84846271f4/crates/muxa/src/account_provider.rs#L279-L311),
   [crates/muxa/src/account_provider.rs:427–448](https://github.com/Open330/muxa/blob/9602fce0d9c5a6c694de5855ba69df84846271f4/crates/muxa/src/account_provider.rs#L427-L448)). Ranking, recording and
   singleton swap lookups must all use the same pin; removing/reordering a row
   cannot transfer it to a nested agent. A known pin never falls back to a
   different account when a sample is stale. New process/account launch, new pin.
2. **Per-window provenance from the actual emitter.** The reviewed PR has one
   `rate_limits_at` gate ([crates/muxa/src/account_provider.rs:314–329](https://github.com/Open330/muxa/blob/9602fce0d9c5a6c694de5855ba69df84846271f4/crates/muxa/src/account_provider.rs#L314-L329)).
   Attribute a sample to the launch whose exact process emitted it: the Claude
   statusline's child/parent relationship is checked against the receipt Claude
   and its `session_id`; Codex rollout samples belong through their exact fd
   owner once stage 3 establishes that contract. Claude's payload already has
   that session ID and separate windows
   ([crates/muxa/src/adapters/claude.rs:363–377](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/adapters/claude.rs#L363-L377),
   [crates/muxa/src/adapters/claude.rs:423–450](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/adapters/claude.rs#L423-L450)).
   Keep `{value, reset, source_time/cursor, launch_id}` per 5h/7d/cap sample.
   Replace each window independently. A new launch ID gets **no credit for
   retained windows**; a fresh 5h report cannot freshen an old 7d value. A cached
   rollout reread retains its original ownership, never the current launch ID.
   Main's partial heartbeat updates show why one timestamp is insufficient
   ([crates/muxa/src/state.rs:1045–1066](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/state.rs#L1045-L1066)).
3. **Native account bind.** Use `MUXA_SESSION_ID` and the owned child receipt
   from stage 2, keeping `pane = None`. The reviewed PR binds accounts after
   tmux discovery but not in `run_native`
   ([crates/muxa-cli/src/agent_launch.rs:943–948](https://github.com/Open330/muxa/blob/9602fce0d9c5a6c694de5855ba69df84846271f4/crates/muxa-cli/src/agent_launch.rs#L943-L948),
   [crates/muxa-cli/src/agent_launch.rs:617–636](https://github.com/Open330/muxa/blob/9602fce0d9c5a6c694de5855ba69df84846271f4/crates/muxa-cli/src/agent_launch.rs#L617-L636)). Both surfaces should
   feed the same launch pin; a native Task row and provider row are not two
   competing account owners.
4. **C1/C10 liveness.** Check exact ProcessKey, not provider basename or
   `pid_alive(pid)` alone; the current PID-only Task reaper is at
   [crates/muxa/src/state.rs:2091–2100](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/state.rs#L2091-L2100). Unknown is not dead, and D1 display
   evidence does not itself authorize stopping/swapping anything.

Shutdown drain is **not D1 work**. #365/account-ranking work should give its
ledger a staged shutdown after producers drain, following the existing daemon
sequence ([crates/muxad/src/main.rs:585–636](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxad/src/main.rs#L585-L636)). It is not a prerequisite for
shipping launch receipts and does not add a D1 journal or writer barrier.

## First implementation PR: files and tests

**Stage 1 — launch receipt: tmux/rmux + Claude, display-only.**

| Files | Bounded change |
| --- | --- |
| `crates/muxa/src/shell.rs` | Add `exec_line_with_env`: safely export the reserved ID, then exec on POSIX. PowerShell is unknown(no_exec), not an invented POSIX equivalent. |
| `crates/muxa-cli/src/agent_launch.rs` | Replace inherited ID on every launch through this path; use the four-field `-F` receipt for all three create shapes; stamp every pane; retain worker mark alias; implement the fresh-Claude session-ID argv rule; label routed wrappers unknown(wrapper_forks). Preserve existing launch permissions/options. |
| `crates/muxa/src/agent_control.rs` | Optional `launch_id` and `launch_receipt` on `AgentStartResult`; propagate the CLI envelope. No App UI work. |
| `crates/muxa/src/adapters/hook.rs` and existing ingestion sender/protocol | Forward an optional launch claim beside the event, not in `AgentId`; add optional receipt metadata in `ipc/protocol.rs`, handle it in `ipc/dispatch.rs`, and send it from the CLI registration path. Capability-gate receipt-only ingestion. |
| New `crates/muxa/src/launch_identity.rs`, `state.rs`, existing snapshot/spawn metadata plumbing | Join receipt/Claude proof and pending claims in either order; expose optional `Agent.launch_identity` only in `muxa agents --json`; reuse snapshot and spawn archive, including creator-less launches. No consumer changes. |

Stage 1 gates **only** on: POSIX quoting (spaces/quotes/newlines), inherited ID
replacement, receipt parsing for pane/window/session and rmux unsupported-field
handling, Claude fresh/resume/session flag argv rules, hook-before-receipt and
receipt-before-hook equivalence, PID reuse, nested Claude, forked wrapper unknown,
old payload/result/archive decode, and a scratch-tmux live smoke. The smoke creates a
private server and checks receipt PID/start, env, mark and Claude session record;
never exercise these by restarting the operator's server. Replayed events and
malformed/lost receipts must not create twice. Run the normal Rust workspace
checks for the implementation, but do not gate on later App/Fleet/Windows work.

## Remaining stages and later surfaces

| Stage | Deliverable | Its own gate |
| --- | --- | --- |
| 2 — native | Link launch ID to existing `MUXA_SESSION_ID` and exact owned child; forward receipt and provider join. Native unknown reasons stay surface-specific. | Native env/session/PID receipt, early hook, child exit/PID reuse, old payload decode, no pane fabrication. No account consumer changes yet. |
| 3 — Codex rollout-fd spike | Validate the exact-PID/fd/thread relation above; implement private binding only if the lifetime and subagent questions pass. Shared servers remain unknown. | Idle/active fd lifetime, `/new` transitions and overlap, `session_meta.source`, root versus subagent, PID/fd reuse, denied inspection and shared-server negative cases on each enabled OS/provider version. |
| 4 — consumers | #365 launch pins, per-window source attribution and native account bind; C1/C10 exact liveness. Enable configured-env direct routes after proof; forked wrappers remain unknown. | Filtered/singleton row-set regressions, nested agent, retained 7d + fresh 5h after swap, cached rollout, native routing, and exact-process liveness. Only these consumers change. |

Existing workers and `work up` launches already pass through `agent_launch::start`
([crates/muxa-cli/src/work_up.rs:484–506](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/work_up.rs#L484-L506),
[crates/muxa-cli/src/work_up.rs:1264–1305](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/work_up.rs#L1264-L1305)); stage 1 gives that common path
carriers/receipts without redesigning pipelines. Swaps must later allocate a new
physical launch ID even when resuming the same conversation in the same pane;
the current respawn path keeps the pane and follows its new PID
([crates/muxa/src/account_swap.rs:3581–3605](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/account_swap.rs#L3581-L3605)). Swap consumer integration belongs
to stage 4, not to the display-only first PR.

**Later, separately gated:** Muxa.app/Fleet result selection and remote receipt
transport; headless CLI `ask`; reload/restore; PowerShell and Windows. Their
existing entry points are the app's IPC/bundled-CLI choice
([apps/muxa-macos/Sources/AppModel+Agents.swift:232–245](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/apps/muxa-macos/Sources/AppModel+Agents.swift#L232-L245)),
`ask`'s owned CLI spawn ([crates/muxa/src/ask.rs:2088–2128](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa/src/ask.rs#L2088-L2128)), and reload's
launch observation ([crates/muxa-cli/src/reload/launch.rs:98–110](https://github.com/Open330/muxa/blob/c619904ab8e803e99573cec5f15c517e8592f3e8/crates/muxa-cli/src/reload/launch.rs#L98-L110)).
Carry the same identity model when each is implemented, with its own small
matrix. Restored panes must get fresh receipts, not revive copied launch marks.
This remains the coverage roadmap, not a claim that stage 1 instruments every
launch surface or that those later matrices block it.
