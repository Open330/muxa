# Collaboration follow-up — 2026-10-29

This follow-up implements the five remaining findings from the additional
audit. Raw local evidence lives in
`~/.local/share/muxa/validation/followup-20261029/`.

## Database lifecycle and reuse

The loaded store owns one initialized SQLite connection, serializes its use,
and caches recurring request and query statements. Runtime operations no longer
create a database or rerun schema initialization. Requests, identities, purpose
grants, worker spawns, and worker drains use the same guarded connection.

The store pins the database file before schema initialization and loads every
projection through that same guarded connection. It retains the open handle. On Unix,
device/inode checks before and after operations detect pathname disappearance
or replacement. A detected change permanently fences that store instance,
including normal mailbox reads. Restoring the file does not revive stale memory;
repair and reload are required. A transactional SQLite failure on the intact
file remains retryable. Permission checks run before operations so their failure
cannot turn a committed mutation into a rejected memory update. Already delivered terminal notifications retain their
in-memory markers even if their persistence fails.

This is not an atomic transaction with external filesystem edits. Changes
racing a database operation are reported as unavailable, and its result is not
published as a successful durable mutation. An already committed write to an
unlinked old file cannot be undone. Arbitrary in-place edits by other programs
are outside this fence. Non-Unix systems require a matching creation timestamp;
the implementation was exercised on Linux.

## Fleet relay compatibility

Relays advertise `collaboration_claim_bounded_v1`. The controller refuses to
forward a claim to a relay without it, while read-only mailbox operations remain
available. This complements the existing daemon-side
`collaboration_inbox_limit_v1` check. Both relay and daemon must support bounded
claims; an old relay may no longer claim everything and return only 32 rows.

## First visible history read

Watch establishes the collaboration subscription acknowledgement before reading
the first history snapshot. Its revision baseline is recorded before each read,
so invalidations racing the read remain dirty and trigger a necessary refresh.
Subscription rejection preserves the legacy polling fallback. The handshake
uses the client's existing three-second timeout.

The pre-change live observation read the same 5,264,553-byte history twice with
zero mailbox revision events. These are internal IPC response bytes, not model
tokens or body-relative framing overhead.

## Missing recipient bindings

A claimed, partially delivered, or notified-but-unclaimed request whose exact recipient is absent from
the authoritative registry receives `binding_unavailable` after two minutes of
observed absence. This reports uncertain delivery/execution, not process death.
Undelivered queued requests and pending startup identities are excluded. The absence tracker
resets on daemon restart; a previously persisted active interruption remains
visible. Returning recipients clear binding uncertainty in any state, including permission
waits and startup. A distinct Error/Stopped observation uses its own reason.

The existing coordinator-first recovery and later human escalation are reused.
No request is automatically resent, reassigned, cancelled, or marked failed.

## Current attention

Compact Fleet attention uses notifications only for states that currently need
attention: waiting for input, waiting for a choice, or error. Starting, working,
and idle agents no longer inherit a waiting alert from an older notification.
The original notification remains available in full diagnostic output.

## Verification

Regression tests cover file disappearance/replacement, replacement during an
operation, retryable transaction failures, subscription-before-snapshot ordering,
racing invalidations, missing-binding grace and recovery, relay capabilities,
and current attention projection. Final test and live observation results are
recorded with the implementation evidence.

Real remote SSH fault injection and macOS builds are outside this environment's
verification scope. No operating mailbox file is moved or replaced for testing.
