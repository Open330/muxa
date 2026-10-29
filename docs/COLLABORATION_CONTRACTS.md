# Versioned collaboration task contracts

Muxa's optional `collaboration_task_contract_v1` extension binds a task to an
immutable contract, execution-attempt identity and recipient-reported evidence.
Legacy requests and replies keep their existing behavior. A contract adds no
execution permission: `work_mode` and `paths` remain the existing declared scope.
References are data; Muxa does not open artifacts, execute test commands, contact
URLs, or infer authorization from them.

## Request and report

A request can carry this `task_contract`:

```json
{
  "version": 1,
  "task_id": "review-storage-change",
  "objective": "Review the submitted storage change",
  "basis_revision": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "acceptance_criteria": [
    "Check bucket creation and upload behavior",
    "Check compatibility and error handling"
  ],
  "inputs": [
    {"kind": "agent_event", "locator": "historical-event-id"}
  ]
}
```

Replace the example revision with the full actual Git commit. `basis_revision`
is the starting/reviewed commit, not a moving branch or an implementation's
output commit. Use input artifact digests as well when reviewing uncommitted
changes. Omitting the revision leaves it explicitly unbound.

The daemon persists one `attempt` with a generated UUID, `task_id` and the basis
revision. A retry with the same `dispatch_id` returns the same attempt. Changing
the contract, recipient scope or superseded request conflicts with that dispatch.
An attempt is a request binding, not a lease or an authenticated agent credential.

A claimed contracted request can report `completed` only with a matching report:

```json
{
  "attempt_id": "UUID-FROM-REQUEST-ATTEMPT",
  "basis_revision": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "criteria": [
    {"criterion": 0, "outcome": "pass"},
    {"criterion": 1, "outcome": "pass"}
  ],
  "evidence": [
    {"kind": "test_run", "locator": "artifact:test-results"},
    {"kind": "commit", "locator": "actual-output-commit"}
  ]
}
```

Criterion indices are zero-based and must be distinct and in range. Completion
requires every criterion to be reported as `pass` with criterion-specific or
shared evidence. Outcomes also include `fail` and `not_run`. `blocked`, `failed`
and `declined` may omit a report rather than fabricate completed checks. If a
report is supplied, its attempt/revision binding must still match.

Evidence kinds are `agent_event`, `commit`, `artifact` and `test_run`. Each has a
nonblank `locator` and optional 64-hex `sha256`. References do not establish that
a command ran or that its result is correct. Output labels evidence as
`not_independently_verified`; `reported_status` stays the recipient's declaration.
`requested_basis_revision` is request metadata, even before a reply exists.
`report_basis_matched` is emitted only with an actual matching structured reply,
not with a queued request or a blocker without a report. It describes equality
to the requested commit, not freshness against the working tree's current HEAD.
Muxa does not probe Git or independently run tests as part of this extension.

Limits: 1–32 unique acceptance criteria, 16 references per list, 128-byte task ID,
8 KiB objective, 1 KiB per criterion, and 2 KiB per evidence locator. A serialized
contract is capped at 16 KiB. Request body plus contract and reply body plus report
also obey the configured message-byte limit. New inputs reject unknown fields and unsupported versions. Stored/returned
models tolerate added metadata fields so one newer row cannot break a listing. Exact retries of a structured terminal report
are idempotent; a different report cannot overwrite the terminal result.

## Superseding a read-only investigation

Send a new contracted request with `supersedes_request_id` to replace your older
review after the code or investigation changes. Both requests must be read-only,
belong to the same sender session, workspace/work and logical `task_id`.
Managed Work identity survives a sender window move; requests without a Work
remain room-scoped. Console-dispatched requests must retain the same initiating
agent (or both be local operator requests without an initiator).
The new recipient may differ. The causal thread is inherited; conflicting
explicit thread/parent links are rejected. A source already superseded cannot be replaced a
second time; supersede its successor instead. The operation is atomic with
persistence of the new request and its attempt.

An unfinished old request becomes `cancelled` with `superseded_by`; its reply wake
is withdrawn. A completed result retains its original status and reply but is
labelled superseded. This does not interrupt a process or authorize another editor.
A claimed recipient receives a durable update and one-way relevance notice when
idle; this cannot interrupt an active turn. If a late report arrives first, queued
notifications are retired with it to avoid a redundant model turn. A previously
written input line retains its delivery phase and receives a supersession warning
before Enter completes it. This cannot recall text already submitted.
Editing tasks cannot use this operation: stop or isolate execution explicitly
before arranging an editing handoff.

An old recipient may still return. Up to four late reports are retained in the
old request's `late_replies` as historical evidence. They never populate `reply`,
complete the successor, resurrect a reply wake or count as `final_reply_received`.
Identical late-report retries do not consume another slot. Compact receipts show
`result_applicability: superseded` and `late_reply_count`; an outcome/full read
returns historical evidence. Recovery escalations close when their parent ends.

## CLI and MCP

```sh
muxa msg send role:reviewer 'Review the submitted change' --kind review \
  --task-contract contract.json --json
muxa msg accept REQUEST_ID --json
muxa msg reply REQUEST_ID 'Findings and evidence' --report report.json --json

# New revision; use a fresh dispatch and the actual old request ID.
muxa msg send role:reviewer 'Review the updated change' --kind review \
  --task-contract updated-contract.json --supersedes OLD_REQUEST_ID --json
```

MCP `muxa_send_message`, `muxa_call_peer` and `muxa_fleet_call_peer` accept
`task_contract` and `supersedes_request_id` objects/IDs. `muxa_reply` accepts
`report`. Inbox and direct-delivery prompts include the complete contract and
attempt; metadata/progress views omit instruction and evidence bodies. Outcome
reads include the new report and any historical late reports.

## Compatibility and transport

Contract-bearing local sends use `collaboration_send_contract`, and structured
reports use `collaboration_reply_contract`. They share implementation with legacy
operations but use distinct wire kinds: an old daemon rejects the operation
instead of silently discarding new fields. Peer launch also checks the advertised
capability before starting a worker. A restart between capability check and send
cannot downgrade the wire operation.

Fleet uses `fleet_command_contract` for the local controller hop, a capability
check on the destination, and distinct contract send/reply relay variants. The
remote relay uses the contract-aware local IPC operation too. No hop falls back
to an operation that could discard the contract. CLI on the recipient host can
submit a structured reply; the Fleet operation also carries an optional report.

Mailbox SQLite schema advances from 1 to 2 transactionally on the first
contract-bearing write. Legacy-only traffic remains compatible with older
stores. Version 2 preserves existing records and prevents older
daemons from opening and rewriting the new payloads without their contract fields.
Upgrade/restart the daemon before using this extension. A downgrade requires an
explicit compatible backup/migration; deleting the mailbox loses durable history.
The new daemon refuses unsupported mailbox schema versions. Other load errors
disable collaboration with an explicit error while registry and other daemon
services remain available; hello/health and doctor report the cause. It never
accepts new messages into a memory-only fallback after a load failure. This does
not upgrade the already-running daemon automatically. Do not downgrade after
using contracts: older daemons cannot preserve their fields and may fall back to
a memory-only mailbox when they encounter schema v2.

The current trust boundary remains the same OS user's local IPC and configured
Fleet hosts. Attempt IDs and evidence are attribution, not cryptographic caller
authentication, filesystem enforcement, automatic test verification or a mechanism
for taking ownership from a running editing agent.
