# Review debt: keeping requested reviews visible

Status: proposal r3 (2026-11-06). Base: `origin/main` 8be12e6.
Principles: [DIRECTION.md](../DIRECTION.md) › Attention rule 2, Deterministic first.
Prompted by Addy Osmani, "Critical thinking when agents do the work" (2026-11-03).
Reviewed in two rounds with a Codex peer (`req_18e554aa14da754e_2`,
`req_18e554d7bdd6f968_4`). r1's observation-driven signals and contract v2
bundle are withdrawn; see [Rejected](#rejected-from-r1).

## Problem

muxa makes it cheap to run many agents. Attention surfaces agents that are
**waiting for input**. It does not surface results that **someone said need
review** and nobody has dealt with. The article calls the gap between what
agents produce and what a person can stand behind the *orchestration tax*:
"If finished work is piling up faster than you can get through it, another
session won't help."

What exists today:

- The Work board has a `review` stage (`crates/muxa/src/work.rs` `WorkStage`),
  set only manually; the auto board stage never enters it.
- Collaboration requests track `reply_read_at` (sender retrieved the reply).
  Retrieval is not review. A request's terminal status (`RequestStatus`) is
  not requester acceptance: `completed` is the recipient's report and
  `cancelled`/`expired` are lifecycle outcomes.
- `muxa work done` records an agent's *claim* of completion for one pipeline
  alias and generation, because idle cannot distinguish "finished" from
  "between thoughts" (`crates/muxa-cli/src/tmux_work.rs` `run_work_done`).
- The agent-records index pairs tool calls and results by `call_id`, indexes
  shell and patch invocations as tool calls, and keeps structured `is_error`
  where the provider supplies it (Claude). It has no command or exit-code
  fields, Codex tool output has `is_error = None`, and there is no normalized
  edit attribution or execution-to-file linkage
  (`crates/muxa/src/agent_records/parse.rs`).

## Principle: observation, claim, disposition

These are different facts and stay separate in storage and in UI:

| Fact | Example | Established by |
| --- | --- | --- |
| Observation | turn stopped, tool result had `is_error`, commit reachable | muxa, from a deterministic source |
| Claim | "ready for review", "tests passed", `work done` | an agent or user, recorded as a claim with its source |
| Disposition | accepted, returned, withdrawn | a declared actor: user, agent, or a named policy |

A deterministic observation does not make an inferred meaning deterministic.
A turn stopping is not a finished deliverable; reachability from main is not a
review; an agent accepting a peer's reply is not human review. Local IPC does
not authenticate a human, so dispositions record the *declared* actor kind
and are never presented as proof of human comprehension.

## Proposal

### Review items

A **review item** is an explicit, durable request that a specific result be
looked at.

- **Created only by a claim.** Never by Stop, process exit, Run completion or
  Git ancestry. Sources, in rollout order:
  1. explicit request by a user or agent (CLI, later MCP) naming a session
     or result;
  2. **template policy**: a Work template declares, per alias or role, that
     each `work done` claim from that alias requests review. Scoped per alias
     because `work done` also completes intermediate researcher/reviewer
     steps. The item records its source as "policy applied to completion
     claim";
  3. a requester `needs_review` flag on a contracted request. This is review
     intent only; it does not change contract success semantics.
- **Identity.** A daemon-assigned durable item id, plus a caller-supplied
  idempotency key so retries never create duplicates. For template policy the
  key is Work identity + pipeline generation + alias + completion episode.
  Result generation is separate from item id: a retry never creates a new
  generation; a new qualifying claim does. Provider turn ids are optional
  metadata. `work done --undo` invalidates the item for that episode
  (disposition `withdrawn`, actor `policy`).
- **Immutable result descriptor** stored at claim time: a bounded pointer
  (session, Work/alias/generation or request/attempt) and a short "what to
  inspect" text. Optional caller-supplied evidence anchor or start/end bounds.
  Because sessions continue or vanish and index ids are disposable, muxa
  promises navigation to the exact *item* plus best-available result context,
  not to an exact transcript location.
- **Disposition.** `accepted | returned | withdrawn`, with declared actor kind
  (`user | agent | policy`) and timestamp. `returned` means "sent back for
  more work". A target becoming unavailable (pane closed, registry row or
  transcript gone) does **not** retire an item; it stays open, marked
  `target_unavailable`, with its stored descriptor. Only explicit withdrawal,
  or a declared policy for canonical supersession/cancellation, retires one.
- **Not Work-only.** A session target lets hooks-only users use review items
  without Work (Attention rule 1). Work is a grouping unit when present;
  muxa never synthesizes Work to hold an item.

The promise is "explicit review requests stay visible until explicitly
disposed", not "muxa knows what a human has reviewed".

### Evidence drill-down (after the queue)

From an item, show recorded tool calls and results for its session using
`call_id` pairing, with structured `is_error` where it exists and explicit
coverage (index truncation, missing timestamps, redaction, unavailable files,
unsupported provider). Absence of a record is `unknown`. The window is the
caller-supplied anchor or bounds when given, otherwise "window unspecified";
item creation time is not an execution boundary, since review is requested
after the work. Reported evidence keeps `not_independently_verified`.

Later, provider-specific execution metadata (exit code, async exec with
polling, nested orchestration calls) with fixtures for incomplete transcripts.
Cross-checking reported evidence requires typed claims and a reference that
survives index rebuilds (today's `event_id` hashes path, offset, part and
text, so moving or reformatting a source or changing normalization can change
it).

### Later, each on its own evidence of use

- Flow metrics (daily in/out, request→disposition latency) from the review
  store's own transition log; never from `WorkMetadata.updated_at`.
- Integration ("commit reachable from <ref>") shown beside an item, never
  discharging it unless the user configures that as a policy, labelled so.
- Recorded patch/shell invocations touching paths, with coverage limits.
- Backlog warning when starting Work or peers with N open items; display only,
  off by default.
- `attend` routing, once unavailable targets have a sensible destination.

## Rollout

**PR 1: standalone queue.**
- New module `crates/muxa/src/review.rs` (or `review/{mod,store}.rs`) with an
  independent, versioned, owner-only store at
  `$XDG_DATA_HOME/muxa/reviews.sqlite3`, registered in `paths.rs`. Kept apart
  from `dashboard/work_store.rs` (Work-specific) and the collaboration
  mailbox (avoids coupling to contracts and mailbox health).
- Daemon ownership via `crates/muxad/src/main.rs`; IPC in
  `crates/muxa/src/ipc/{protocol,dispatch,server,client}.rs` exposing a
  bounded aggregate (count, oldest age) plus list/show/create/dispose.
- CLI: `muxa review request|list|show|accept|return|withdraw` (new command
  module, wired in `main.rs`/`command_dispatch.rs`).
- Status-line: count and oldest age in `cmd_status_line`
  (`crates/muxa-cli/src/status_handlers.rs`), fetched within the existing
  timeout budget, ideally in the same round trip as attention aggregation;
  empty on timeout as today. No transcript reads on this path.
- No watch keys, MCP, pipeline or contract hooks.
- Tests: retry deduplication, restart persistence, new claimed generation
  after `accepted`, unavailable-target survival, store write failure.

**PR 2: agents and templates.** MCP schemas/dispatch (`crates/muxa-cli/src/mcp.rs`);
Work grouping; template policy enrollment on `work done` through the daemon's
pipeline mutation handling, durable and idempotent with the completion itself
(a CLI-side "done, then create" can lose items on crash); `--undo` semantics.

**PR 3: contracts.** `needs_review` on contracted requests with
capability/schema negotiation; supersession/cancellation policy in
`collaboration.rs`.

**Then:** evidence drill-down.

## Rejected from r1

| r1 idea | Why dropped |
| --- | --- |
| Auto-enroll on Run end / Stop | Stop is a turn boundary; idle ≠ finished |
| Merge-to-default as review signal | Reachability ≠ review; misses squash/rebase, blesses untouched branches |
| `review = none/light/deep`, default `light` | Default-on enrollment is noise; explicit claims and per-alias template policy replace it |
| `self_authored` test label | No normalized edit attribution or execution→file linkage |
| `not_found` / `contradicted` on bare locators | Coverage gaps make absence unknown; locators are not typed claims |
| Contract v2 bundle (sealed expectation, typed decisions, `not_covered`) | Couples unrelated experiments; sealing is unenforceable against the same user; decisions stay self-reported prose. Free-text "what to inspect" covers the useful part; a skill can run the rest |
| Provenance hop depth | Inputs are untyped locators; only declared message ancestry is countable |
| `done_when` on Work | Overlaps `Work.goal`; revisit with a distinct use |
| Diff-size ordering | Assumes Git and clear output ownership |
