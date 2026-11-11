# Awaiting reply: agents that end a turn on a question

Status: accepted with Codex review amendments (2026-11-11, req_18e6f34110b2c340_1).

## Problem

An agent that ends its turn by *writing* a question ("Can I require Touch ID
for turning the setting off? If so I'll write v2") is shown as `idle`, the
same as an agent that finished. Observed: jiun-mbp `comp_use / swarm / %4`
(Claude Code) sat `idle` on a decision the user had to make; nothing in
attend, Home, the menu bar or notifications pointed at it.

Tend reaches "needs you" (`WaitingInput` / `WaitingChoice`) only from
structured signals: a permission prompt hook, a choice menu
(`AskUserQuestion` / `ExitPlanMode`), or an explicit `tend_request_attention`.
A prose question produces `Stop` → `TurnStopped { response }` → `Idle`.
`tend_request_attention` exists, but the rule to use it lives in the
agent-integration bootstrap, which the user's CLAUDE.md scopes to Tend
collaboration work, so ordinary agents never see it.

## Decision: A + B

**B — explicit signal (agents tell us).** Every MCP host receives
`MCP_SERVER_INSTRUCTIONS` at initialize, whatever the task. Add one sentence:

> Blocked on a person's decision? Call tend_request_attention, not just prose.

(Kept to one short sentence: every MCP session pays for the instructions,
and the initialize test bounds them under 200 bytes.)

The `tend_request_attention` description gains the same "ending a turn on a
question" case. Its contract is unchanged (keep the ID, wait via
`tend_wait_reply`); B does not invite answering in the pane, so a mailbox
request is never left open by a conversational answer. No protocol change.

**A — inferred signal (we notice when they don't).** On a `TurnStopped`
that carries a response, classify the response's *closing* text. When it
asks the person something, mark the row as awaiting a reply.

### Wire shape: an additive field, not a new state

A new `AgentState::WaitingReply` would be the obvious model, but
`AgentState` has no `#[serde(other)]` fallback, and Fleet mixes versions
(hosts upgrade one at a time; the Mac app decodes `state` as a string and
branches on known values). An unknown variant fails deserialization on older
peers. So the state stays `Idle` and the row gains:

```rust
/// Set when the latest turn ended by asking the person something; cleared by
/// the next prompt or any state change away from Idle. Inferred from text.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub awaiting_reply: Option<AwaitingReply>,

pub struct AwaitingReply {
    /// The closing sentence that asked, trimmed to ~200 chars, so the UI can
    /// show why the row is listed (and a person can judge a false positive).
    pub question: String,
    #[serde(with = "time::serde::rfc3339")]
    pub since: OffsetDateTime,
}
```

Advertised as capability `awaiting_reply_v1`. Older clients ignore the field
and keep showing `idle` — no regression.

### Response bound

Adapters bounded responses to their first 4 KB, which drops exactly the
closing question of a long answer. Every response site (Claude and Codex
`Stop`, the Codex rollout fold, agy) now uses `truncate_response`: the first
2.8 KB and the last 1.2 KB around a `…` paragraph.

### Lifecycle

- Every response-bearing `TurnStopped` assigns `classify(response)` (or
  `None`): an answer that no longer asks clears an earlier question. The same
  question observed again keeps its `since`.
- A response-less stop (synthetic, Codex permission gap) keeps the current
  marker; it says nothing about the text.
- Not set when the latest prompt's origin is a peer or automation
  (`last_prompt_origin`): the question is for that sender. `Unknown` is
  best-effort (it includes provider-generated prompts), and a Codex turn
  seen only through the rollout can leave `last_prompt_origin` stale.
- Invariant `state != Idle ⇒ no marker`: enforced in `Store::apply` after
  every override, on hydration, and on read (`Agent::awaiting_reply()`
  returns the marker only on an Idle row), so direct reconciler/reaper
  writers cannot surface a stale one.
- A newly asked question on a row that stays Idle (a rollout answer arriving
  after the stop) is broadcast as an `Idle → Idle` transition, so notifiers
  see it; repeats of the same question are not re-broadcast.

### Classifier (`tend::awaiting_reply::classify(&str) -> Option<String>`)

Pure function, table-tested. Prose only: fenced code, block quotes, tables,
rules, recap lines, inline code and double-quoted text are removed. Three
strengths of cue:

- **Decision requests** count anywhere — the motivating message put its
  decision mid-response and trailed with notes: `결정해 주실 것`, `결정이
  필요`, `정해 주세요/주시면`, `선택해 주세요`, `승인해 주세요`, `decision
  needed`, `need your decision/approval/go-ahead`, `your call`, `please
  confirm`, …
- **Requests** count in the last three paragraphs, even with a status note
  after them: `할까요`, `될까요`, `괜찮을까요`, `알려 주시면`, `말씀해
  주시면`, `should i`, `shall i`, `do you want`, `which one/option`, `let me
  know which/whether`, …
- **A bare `?`** counts only on the last sentence, or when all that follows
  is a conditional on the answer (`If so, …`, `괜찮으시면 …`). "Why did CI
  fail? The key was missing." asks nothing.

Never counted: courtesy closers (`더 필요한 것 있으면 말씀해 주세요`, `let me
know if you have any questions`, `anything else?`) and offers (`Would you
like…`, `원하시면 …`). Returns the asking sentence, trimmed to 200 chars.

### Consumers

`awaiting_reply` is a *softer* attention than WaitingInput: it never outranks
a permission prompt, choice or error.

- `tend attend` / `status-line --needs-attention`: included, ranked after
  WaitingInput.
- tendd notifier (opt-in `[notifier] enabled`): `* → Idle` with a question
  posts "tend · <kind> · asked you" with the question; same debounce.
- Mac app: status `.asked(question)` (after `.needsInput`, before limits);
  `needsAttention`, so Home's "Your turn" lane (the question is the card
  line, the reply field answers it), the menu bar and Dock counts, and the
  attention notification ("<name> asked you", Reply action). Its
  notification key is `asked:<question>`, so each new question notifies
  once while the agent stays idle.

Follow-ups, not in this change: watch/fleet TUI and web dashboard badges;
suppressing the inferred card when the same agent also has an open explicit
`tend_request_attention` (both show today).

### Non-goals

- No model call: the classifier is a deterministic phrase/punctuation check.
- No screen scraping: the text comes from the transcript the Stop hook
  already reads (Claude) / the rollout (Codex).

## Review outcome

Codex (req_18e6f34110b2c340_1) agreed with A+B and the additive field
(`AgentState` has no unknown fallback), and asked for: an end-preserving
response bound; no "answer here instead" in B; assign-or-clear on every
response-bearing stop with the Idle invariant enforced beyond
`mutate_for_event`; an `Idle → Idle` broadcast for a new question; and a
classifier that rejects self-answered questions, quotes and offers. All are
reflected above.
