# Doctor collaboration diagnostics — 2026-10-29

`muxa doctor` now reads a bounded, body-free `collaboration_health_v1` summary.
It no longer downloads agent snapshots or stale request bodies for these checks.
The daemon borrows its existing request and agent projections and reuses the
published topology inventory; a doctor run does not discover backends again,
claim messages, acknowledge replies, or change request status.

## Recovery visibility

The summary counts interrupted original requests through `peer_interrupted()`.
Recovery questions that mirror their parent's interruption are not counted as
additional interrupted work. Open daemon recovery questions are counted
separately, including orphaned questions, and at most three oldest request IDs
are shown. Oversized imported IDs are omitted from examples while counts stay
exact. Request bodies and progress text are never included.

Missing capability, timeout, or RPC failure produces an unknown/warning result.
It cannot produce a healthy zero-count report. A missing recipient is uncertain
delivery/execution, not proof of process exit or task completion.

## Synthetic agents

Classification uses the daemon's configured backends and its own published
observations, not the environment of the CLI running doctor:

- Unobserved backend: review backend scope and retention; no hook-repair advice.
- Fresh, complete observation with a matching pane: hook correlation guidance.
- Fresh, complete observation without a matching endpoint: verify topology and
  reconciliation; absence does not authorize restarting an agent.
- Missing, stale, invalidated, incomplete or ambiguous evidence: unknown.

Socketless rows require a unique pane within the observed host. Endpoint aliases
use the existing backend identity rules. Unknown reports include cache age,
freshness budget and invalidation state. Sample locations are bounded in size.

## Selective cleanup and protected decisions

```sh
muxa msg expire --older-than 24h --quiet-only --request-id REQUEST --dry-run
muxa msg expire --older-than 24h --quiet-only --request-id REQUEST
```

Selective expiry requires `collaboration_expire_filtered_v1`; a client refuses
before sending an expiry operation to an older daemon. It rechecks age, exact
ID, Queued status, Notice kind, no reply expectation, quiet notification intent,
and absence of delivery evidence under the mutation lock. A claim after preview
wins. Expired records retain their original contents in history.

Every expiry path, including legacy bulk expiry, now protects human decision
requests. Older bulk expiry could expire an unanswered recovery question while
its original request stayed interrupted. Reconciliation restores only that exact
daemon-owned, unanswered Expired question for an active parent. It does not
revive explicit cancellations, replies, or questions whose parent completed.
Restoration is idempotent and never resends or reassigns the agent's work.

## Verification

Regression coverage includes mirrored recovery records, bounded body-free
summaries, missing capability, endpoint uniqueness and scope, stale/incomplete
inventory, selective expiry races, legacy-server mutation refusal, protection of
human decisions, and one-time restoration of accidentally expired questions.

Local evidence, actual doctor output, cleanup previews and final results are in
`~/.local/share/muxa/validation/doctor-followup-20261029/`.
