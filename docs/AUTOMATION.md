# Automation

An *automation* is a rule that watches an agent's state and acts on it.

The case it was built for: you are running Claude and Codex all day, one
of them hits `You've hit your limit · resets 2:40pm`, and the session sits
there red until you happen to look. muxa already knows the cap happened
and, usually, when it lifts. A rule turns that knowledge into a `continue`
typed at the right moment.

The cap is only the first rule anyone wants, so the mechanism is general —
an **event**, some **filters**, a **delay**, and an **action** — and the
first rule is just one instance of it.

**Automation ships enabled with no rules.** A fresh install runs the
engine and does nothing. Nothing is typed into any pane until you write a
rule, or Muxa.app writes one for you.

## Quick start

Put this in `$XDG_CONFIG_HOME/muxa/config.toml` and restart `muxad`:

```toml
[[automation.rule]]
name = "resume-after-limit"
on = "rate_limited"
action = "send_prompt"
text = "continue"
wait = "{{reset}}+2m"
fallback = "20m"
```

Then check it against what is running right now, without firing anything:

```console
$ muxa automation test resume-after-limit
resume-after-limit: rule enabled, engine on
%42	claude_code	error	fire	at 2026-09-03T14:42:00Z	"continue"
nothing was fired; this was a dry run
```

Or write the same rule through the daemon, which takes effect immediately
and needs no restart:

```console
$ echo '{"name":"resume-after-limit","on":"rate_limited","action":"send_prompt",
         "text":"continue","wait":"{{reset}}+2m","fallback":"20m"}' |
    muxa automation add --from-json -
wrote resume-after-limit
```

## Commands

| Command | What it does |
| ------- | ------------ |
| `muxa automation list [--json]` | Every rule with its effective timing, guards, and recent activity. |
| `muxa automation log [--limit N] [--json]` | What the engine did: fired, skipped, and why. |
| `muxa automation enable <name>` / `disable <name>` | Turn one rule on or off, live and in `config.toml`. |
| `muxa automation pause [--for 1h]` / `resume` | Hold every rule. A pause expires on its own; `resume` lifts it early. |
| `muxa automation add --from-json <path\|->` | Write or replace one rule from JSON. |
| `muxa automation remove <name>` | Delete one rule. |
| `muxa automation test <name> [--json]` | Evaluate the rule against the live registry and print what it *would* do. |

Every subcommand goes through the daemon rather than editing
`config.toml` behind its back. The daemon reads config once at startup and
does not watch the file, so a hand edit takes effect at the next restart —
while `enable`, `pause`, `add`, and `remove` change the running engine
*and* the file, in that order, and refuse the edit outright if the merged
file would not load.

## Writing a rule

```toml
[automation]
enabled = true            # master switch, default true
# paused_until = "..."    # RFC3339; written by `muxa automation pause`

[[automation.rule]]
name = "resume-after-limit"      # unique, TOML-safe, required
on = "rate_limited"              # the event this rule watches
enabled = true

# --- filters: every one is optional; all present ones must match ---
agent = ["claude", "codex"]      # agent kinds
workspace = "callabo"            # exact workspace id stamped on the pane
work = "^CAL-"                   # regex on the Work id
pane = "%42"                     # exact pane id
host = "local"                   # `local`, or tmux / cmux / rmux / zellij / herdr
scope = ["five_hour"]            # rate-limit window (rate_limited only)

# --- when to act ---
wait = "{{reset}}+2m"                # anchor on the cap's own reset time
fallback = "20m"                 # used when the cap carries no reset time
jitter = "30s"                   # random 0..jitter added; default 15s

# --- what to do ---
action = "send_prompt"
text = "continue"

# --- guards: tunable, but a rule cannot opt out of them ---
max_per_hour = 2                 # per rule per pane; default 3
cooldown = "5m"                  # per rule per pane; default 2m
only_if_still = "rate_limited"   # re-checked at fire time
```

### Events

Closed set. Each names a live agent state, so a rule is armed the moment a
row enters that state.

| `on` | Fires when | Needs |
| ---- | ---------- | ----- |
| `rate_limited` | The agent hit a usage cap — `Error` **with** a rate-limit scope. | — |
| `waiting_input` | The agent is blocked on you (`WaitingInput` / `WaitingChoice`). | — |
| `idle_for` | The agent has been `Idle` for `for`. | `for = "10m"` |
| `error` | The agent is in `Error` for a reason that is *not* a cap. | — |

`rate_limited` and `error` are deliberately distinct: a capped row and a
crashed row are both `AgentState::Error`, and the rate-limit scope is the
only thing that tells them apart.

### Actions

Closed set.

| `action` | Keys it reads | What happens |
| -------- | ------------- | ------------ |
| `send_prompt` | `text` (required), `submit` (default `true`) | Types `text` into the pane, waits the same submit grace the collaboration waker uses, then sends Enter. |
| `notify` | `message` (required) | Emits a daemon-side notice and a ledger entry. Types nothing. |
| `interrupt` | — | Sends the pane's interrupt (`Ctrl-C`). Types no prompt. |

`text` is refused at load time if it contains terminal control sequences.
An automation types into a live TUI; escape sequences reaching one are how
a "resume" turns into arbitrary key bindings.

### Timing

`wait` is either a plain duration counted from the event (`5m`) or
anchored on the cap's own reset time (`{{reset}}`, `{{reset}}+2m`,
`{{reset}}-30s`). The bare `reset` spelling from earlier builds still loads.
A `reset` anchor only makes sense for `on = "rate_limited"` — nothing else
carries a reset time — and is refused elsewhere.

Not every cap names its reset time. The Claude statusline and the Codex
rollout do; a `StopFailure` 429 and the transcript scan do not. When the
anchor has nothing to anchor to, `fallback` (default 15m) replaces the
whole expression.

`jitter` adds a random `0..jitter` to every fire time, so a dozen agents
capped in the same window do not all resume on the same second. Default
15s; set `jitter = "0s"` to disable it.

A fire time that has already passed fires *now*, never retroactively.

Durations are written `45s`, `5m`, `2h`, `1d`. A bare number is refused:
`20` reads as seconds to one person and minutes to the next, and the
difference between those two is a runaway.

### Filters

All present filters must match. `workspace` and `work` read the tmux user
options a muxa-managed launch stamps on the pane, so they never match a
pane muxa did not launch. `host = "local"` matches every pane this daemon
governs — it only ever sees its own node — while a backend name (`tmux`,
`herdr`, …) narrows to one pane-id namespace.

## What stops a runaway

An automation types into a live agent. Every firing has to pass all of
this, and each layer is independent:

1. **`[automation] enabled`** — the master switch. `false` stops every
   rule without editing any of them.
2. **`paused_until`** — a time-boxed hold over everything. It expires on
   its own; `muxa automation pause --for 2h` is safe to use and forget.
3. **The rule's own `enabled`** — one rule off, the rest running.
4. **`cooldown`** (default 2m) — minimum gap between firings of one rule
   against one pane.
5. **`max_per_hour`** (default 3, max 60) — firings per hour, per rule,
   per pane.
6. **A global ceiling of 30 firings/hour** across every rule and every
   pane. Not configurable: `max_per_hour` bounds one rule against one
   pane, and this bounds the whole engine against a pathological fan-out.
   Reaching it is a bug, and it is logged as one.
7. **The fire-time re-check** — `only_if_still` is evaluated against the
   live store at the moment of firing, not when the rule was armed. An
   agent you resumed yourself while the timer ran is left alone.
8. **One firing per episode** — an *episode* is one uninterrupted stay in
   the state that armed the rule. A soft cap upgrading to a hard one, or
   a second `RateLimited` landing on a row already in `Error`, is the same
   episode and cannot produce a second firing. The key is
   `(rule, pane, episode)` and it lives in the ledger, so it survives a
   daemon restart. A firing that was *attempted* and failed also consumes
   its episode: retrying would re-send keystrokes into a pane that may
   have taken the first ones.
9. **The ledger** — every decision, including every fire-time skip and its
   reason, is appended to `$XDG_DATA_HOME/muxa/automation.json` (last 500
   entries) and readable with `muxa automation log`.

A rule is also skipped, silently, when the row has no pane — there is
nothing to type into.

Arm-time skips (every rule sees every agent, so most evaluations are a
filter mismatch) stay in the daemon's trace log rather than flooding the
ledger. Fire-time skips are recorded.

## How it works

The engine subscribes to the same in-process transition broadcast the
collaboration waker uses. On each transition it evaluates every enabled
rule against that agent, and a matching rule's firing goes into a heap
keyed by fire time. When one comes due it is re-checked against the live
store, acted on, and recorded.

Two cases a transition cannot cover are handled by a 30-second
authoritative rescan: a row that was already capped when the daemon
started, and a `RateLimited` landing on a row already in `Error` — which
changes no state and therefore broadcasts no transition.

Pane metadata (`workspace`, `work`) costs a pane scan, so it is only read
when at least one enabled rule actually filters on it, and the result is
cached for ten seconds.

The decision logic itself is pure: given a rule, an agent snapshot, the
guard state, an injected `now`, and an injected jitter ratio, it answers
"fire, and when" or "skip, because". That is what makes the guards
testable to the second — see `crates/muxa/src/automation.rs`.

## IPC

Capability tag: `automation_v1`.

| Request | Answers in |
| ------- | ---------- |
| `{"kind":"automation_list"}` | `automation_rules` |
| `{"kind":"automation_log","limit":20}` | `automation_log` |
| `{"kind":"automation_set_enabled","name":"…","enabled":false}` | `automation_rules` |
| `{"kind":"automation_pause","until":"2026-09-03T15:00:00Z"}` (`null` resumes) | `automation_rules` |
| `{"kind":"automation_set_rule","rule":{…}}` | `automation_rules` |
| `{"kind":"automation_remove_rule","name":"…"}` | `automation_rules` |
| `{"kind":"automation_test","name":"…"}` | `automation_test` |

`automation_set_rule` takes one rule in the same shape as a
`[[automation.rule]]` table, with `name` required. It replaces the rule
with that name in place, or appends it, and the merged document has to
read back as a full `Config` before anything touches disk.
`automation_remove_rule` refuses an unknown name rather than silently
succeeding — an editor that lost sync should be told.

`automation_rules` is flat, so a table view needs no duration grammar:

```json
{
  "enabled": true,
  "paused_until": null,
  "rules": [
    {
      "name": "resume-after-limit",
      "on": "rate_limited",
      "enabled": true,
      "action": "send_prompt",
      "agent": ["claude_code", "codex"],
      "work": "^CAL-",
      "scope": ["five_hour"],
      "text": "continue",
      "submit": true,
      "wait": "{{reset}}+2m",
      "fallback": "20m",
      "jitter": "30s",
      "cooldown": "5m",
      "max_per_hour": 2,
      "only_if_still": "rate_limited",
      "filters": "agent=claude_code,codex work=^CAL- scope=five_hour",
      "fired_last_hour": 1,
      "last_fired_at": "2026-09-03T13:42:11Z"
    }
  ]
}
```

A row is a *complete* description of its rule: the filters (`agent`,
`workspace`, `work`, `pane`, `host`, `scope`, `for`) and the action's
payload (`text` / `message` / `submit`) are carried verbatim, exactly as
the rule declares them, so an editor can load a rule, change one field,
and hand the whole thing back to `automation_set_rule` without dropping
what it did not render. Absent filters are omitted rather than sent null.

`wait`, `fallback`, `jitter`, `cooldown`, `max_per_hour`, and
`only_if_still` are the *effective* values with defaults already resolved,
so a table view never has to know the defaults; `filters` is the same
information as a one-line human summary. Agent kinds are always the
canonical names (`claude_code`, `gemini_cli`), never the shorthand a rule
may have been written with.

`automation_log` is a list of:

```json
{
  "rule": "resume-after-limit",
  "pane": "%42",
  "agent": "claude_code",
  "fired_at": "2026-09-03T13:42:11Z",
  "action": "send_prompt",
  "outcome": "fired",
  "detail": "continue",
  "episode": "error@2026-09-03T12:40:02Z"
}
```

`outcome` is `fired`, `skipped`, or `failed`; for a skip, `detail` is the
reason (`condition_cleared`, `pane_gone`, `cooldown`, `hourly_cap`,
`global_cap`, `episode_already_handled`, `paused`, `engine_disabled`,
`rule_disabled`).

`automation_test` reports one candidate per live agent:

```json
{
  "rule": "resume-after-limit",
  "enabled": true,
  "engine_enabled": true,
  "paused_until": null,
  "candidates": [
    {
      "pane": "%42",
      "agent_session_id": "sess-1",
      "agent": "claude_code",
      "state": "error",
      "decision": "fire",
      "fire_at": "2026-09-03T14:42:00Z",
      "detail": "continue"
    }
  ]
}
```

`decision` is `fire` or the skip reason. `fire_at` is the earliest the
rule could act — a real firing adds up to `jitter` on top. Nothing is
fired and nothing is recorded.

## Adding an event or an action

Both vocabularies are closed enums so a new variant is a compile error
everywhere it has to be handled, not a silent no-op.

**An event** needs: a variant on `AutomationEvent`; an arm in
`AutomationSubject::current_event` saying which live agent state produces
it; an arm in `AutomationCondition::from_event` naming the condition it
re-checks as; and, if it needs an extra key the way `idle_for` needs
`for`, an arm in `AutomationRule::validate`.

**An action** needs: a variant on `AutomationAction`; validation of the
keys it reads in `AutomationRule::validate`; an arm in the daemon's
`AutomationEngine::perform`; and a row in the Actions table above.
