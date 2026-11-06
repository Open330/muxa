# Review items

A review item is an explicit request that one result be looked at. It stays
visible until someone records what happened to it. Design and rationale:
[proposals/REVIEW_DEBT.md](proposals/REVIEW_DEBT.md).

muxa never creates an item from what it observes. A turn stopping, a process
exiting, a Run completing or a branch merging does not mean a result is ready
or reviewed. Items come only from a claim: someone, a user or an agent, asked
for review.

## Commands

```sh
# Ask for review. Exactly one target.
muxa review request --work muxa/storage --alias impl \
  --summary "Check the migration and the retry path" --anchor commit:1a2b3c
muxa review request --session <session-id> --provider codex --summary "…"
muxa review request --request <collaboration-request-id> --summary "…"

muxa review list            # open items, newest first
muxa review list --all      # include disposed items
muxa review show rv_…

muxa review accept   rv_… [--note "…"]
muxa review return   rv_… --note "tests missing"   # sent back for more work
muxa review withdraw rv_… [--note "…"]
```

Every command takes `--json`. `request` and the dispositions take
`--as user|agent` (default `user`) and an optional `--label`.

## Semantics

- **Retries.** `--key` makes a request idempotent: the same key and target
  return the existing item. The same key for another target is refused.
- **Generations.** Each new request for the same target (session, Work +
  alias, or collaboration request) takes the next generation. Accepting
  generation 1 does not affect generation 2.
- **Dispositions are final.** `accepted`, `returned` and `withdrawn` are
  recorded once. Repeating the same one is a no-op; a different one is
  refused. To review again, request again.
- **Declared, not proven.** Actors are what the caller declared. Local IPC
  does not authenticate a person, so `accepted` means someone said they
  accepted the result, not that a human understood it.
- **Targets are descriptors.** An item outlives its pane, session or
  transcript. Nothing retires an item except an explicit disposition.
- **Anchors are recorded, never opened.**

## Status line

`muxa status-line --needs-attention` appends open review items after the
attention count:

```
⚠ 2 need you ◇ 3 to review · 4h
```

The age is the oldest open item. Both counts share the status line's tight
deadline; a daemon without review items, or with an unavailable store, adds
nothing.

## Storage

`$XDG_DATA_HOME/muxa/reviews.sqlite3`, owner-only (0600), owned by muxad.
Every request and disposition is also appended to an event log in the same
file for later flow metrics. If the file cannot be opened, muxad still
starts and refuses review requests with the reason.

Requires a muxad that advertises `review_items_v1`.
