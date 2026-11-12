# Review items

A review item is an explicit request that one result be looked at. It stays
visible until someone records what happened to it. Design and rationale:
[proposals/REVIEW_DEBT.md](proposals/REVIEW_DEBT.md).

tend never creates an item from what it observes. A turn stopping, a process
exiting, a Run completing or a branch merging does not mean a result is ready
or reviewed. Items come only from a claim: someone, a user or an agent, asked
for review.

## Commands

```sh
# Ask for review. Exactly one target.
tend review request --work tend/storage --alias impl \
  --summary "Check the migration and the retry path" --anchor commit:1a2b3c
tend review request --session <session-id> --provider codex --summary "…"
tend review request --request <collaboration-request-id> --summary "…"

# Ask for a decision, with the context to make it from the card.
tend review request --work tend/storage --alias impl \
  --summary "Storage migration ready" \
  --decision-needed "Merge now or wait for the perf run?" \
  --option "merge now" --option "wait for perf" \
  --recommend "merge now: the perf run only covers the read path" \
  --evidence commit:1a2b3c --evidence https://github.com/o/r/pull/12 \
  --evidence "test-pass:cargo test -p tend review"

tend review list            # open items, newest first, 50 per page
tend review list --oldest   # oldest first (where the status-line age comes from)
tend review list --all      # include disposed items
tend review list --offset 50   # next page; a page that has more says so
tend review show rv_…

tend review accept   rv_… [--note "…"]
tend review return   rv_… --note "tests missing"   # sent back for more work
tend review withdraw rv_… [--note "…"]
```

Every command takes `--json`. `request` and the dispositions take
`--as user|agent` (default `user`) and an optional `--label`.

## From agents

An agent files a result record with the `tend_file_result` MCP tool (target:
its managed Work alias, else its session), or inside a pipeline with

```sh
tend work done --summary "Migration and retry path done" --commit 1a2b3c \
  --evidence "test-pass:cargo test -p tend review" \
  --decision-needed "Ship behind the flag or on by default?" \
  --option "behind the flag" --option "on by default" --recommend "behind the flag"
```

which files the item, keyed by Work, alias and pipeline generation, before it
reports the alias done. A refused record leaves the alias open.

## Decision fields

An item may carry a `decision`: `needed` (the question), `options` (up to
8 one-line choices), `recommendation`, and `evidence` (up to 16 typed
pointers). Evidence kinds:

| Kind | Shorthand | Fields |
| --- | --- | --- |
| `commit` | `commit:SHA` | `sha` (4-64 hex), `repo` (absolute checkout path) |
| `pull_request` | `pr:URL` or a bare `https://` URL | `url` |
| `diff` | `diff:A..B` | `range` (revision syntax only), `repo` |
| `test` | `test:CMD`, `test-pass:CMD`, `test-fail:CMD` | `command`, `passed`, `tail` (last 2 KiB of output, escapes removed) |
| `path` | `path:PATH` | `path` |

`commit:` and `diff:` evidence records the current directory's git
checkout unless `--repo` names one. Every field is the asker's declaration;
tend validates its shape, never its truth. Human requests
(`tend_request_attention`) carry the same `decision` object next to their
`body`. Storing it needs a tendd advertising `review_decision_v1` (review
items) or `collaboration_human_decision_v1` (human requests).

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
- **No control characters.** Identifiers, labels, anchors and keys refuse all
  control characters; summaries and notes allow newlines and tabs only. Every
  stored string is printed by the CLI, so terminal escapes are refused at the
  boundary.
- **Bounded pages.** A list returns at most 500 items and reports whether more
  follow, so the oldest item is always reachable.

## Status line

`tend status-line --needs-attention` appends open review items after the
attention count:

```
⚠ 2 need you ◇ 3 to review · 4h
```

The age is the oldest open item. Both counts share the status line's tight
deadline; a daemon without review items, or with an unavailable store, adds
nothing.

## Storage

`$XDG_DATA_HOME/tend/reviews.sqlite3`, owned by tendd. The database is
created owner-only (0600), and an existing database and its WAL/SHM files
are tightened to 0600 on open. A symlinked path is refused; the 0700 data
directory, not an atomic no-follow open, is what keeps other users out.
Every request and disposition is also appended to an event log in the same
file for later flow metrics. If the file cannot be opened, tendd still
starts and refuses review requests with the reason.

Requires a tendd that advertises `review_items_v1`.
