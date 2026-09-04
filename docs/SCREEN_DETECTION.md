# Screen-manifest fallback detection

Status: **implemented.** Core in `muxa::screen`, daemon task in
`muxad::screen_detect`, shared synthetic-row machinery in `muxad::synthetic`.

muxa gets authoritative agent state from **hooks** (Claude Code, Codex, Gemini,
Antigravity)
and, on herdr hosts, from **herdr's own detection** bridged into synthetic rows
(`docs/HERDR.md`). For every *other* agent CLI — cursor-agent, amp, copilot,
aider, goose, and anything a user declares — there is no event stream at all.
`agy` sits in both camps: muxa ships hooks for it, *and* a bundled manifest —
because agy's hooks cannot report an approval prompt, so screen inference stays
live on agy panes even when hooks are wired. See
[Attention refinement](#attention-refinement-the-one-exception).
Codex has a different, metadata-only exception: its compacted rollout payload
is opaque, while the TUI renders a plaintext `Conversation recap`. The screen
task captures that recap into the real row without inferring or changing state.
Screen-manifest detection is the last-resort fallback herdr validated: capture
the pane and match TOML-declared regex rules against the visible tail to infer
`Working` / `WaitingInput` / `Idle`.

This is deliberately the **weakest** signal in muxa's precedence chain, and the
mechanics are built so it can never override a stronger one.

## Precedence invariant

```
hooks  >  herdr bridge  >  screen detection
```

Screen rows are **synthetic** (session id `synthetic-…`), which buys the whole
invariant from existing machinery:

- **A real hook evicts a screen row instantly.** `Store::apply`'s
  synthetic-eviction pass drops any synthetic row for a pane the moment a real
  hook `Started`/tool/prompt event claims it (same rule the herdr bridge relies
  on). The hook row then owns the pane.
- **The screen task never clobbers a live authoritative row's state.** Before
  applying inferred state, it checks `synthetic::occupant_is_authoritative`: a
  live (non-`Stopped`), non-synthetic occupant means "no synthetic state
  update." Hook-owned Codex panes are still captured for their recap only;
  that metadata update does not touch state, activity time, or transitions. A
  `Stopped` real row is a stale tombstone, not an owner, so a fresh hook-less
  agent in that same pane is still detectable.
- **herdr hosts are skipped wholesale.** A herdr backend is never a screen
  candidate — herdr's own detection plus the herdr bridge already cover those
  panes. (`detectable_backends` filters `HostKind::Herdr` out.)

Because screen rows reuse the exact `synthetic_session_id` convention discovery
mints (`synthetic-<len>:<socket>:<pane_id>` on tmux, carrying the pane's tmux
socket in the row's `tmux_socket`), a discovery placeholder and a screen row
**collapse onto one registry key** and share that eviction precedence.

## Manifest format

A manifest is a TOML file:

```toml
[agent]
name    = "cursor"            # row display name — goes into `model`; kind stays Unknown
command = ["cursor-agent"]    # process basenames matched against PaneInfo.current_command

[rules]                       # evaluated top-to-bottom against the captured tail
blocked = ['(?i)\[y/n\]', '(?i)do you want to (allow|proceed|run)']  # regex list, ANY match
working = ['[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏]', '(?i)\b(thinking|generating)\b', '(?i)esc to interrupt']
idle    = ['(?m)^\s*>\s*$']
```

- `[agent].name` — the display name carried into the synthetic row's **`model`**
  field (the row's `AgentKind` stays `Unknown`, since these agents have no muxa
  kind). It is also the **override key** (see below).
- `[agent].command` — one or more command names. Matching is on the **basename**,
  **case-insensitive** (`/usr/local/bin/Cursor-Agent` → `cursor-agent`),
  mirroring `discovery::classify_command`. tmux only exposes the foreground
  command basename, and muxa matches that basename only — there is **no
  process-tree walk**, so an agent that runs under a `node`/`python` wrapper
  won't be picked up unless its foreground command is the agent binary itself.
- `[rules]` — three optional regex lists. Each compiles to a `regex::RegexSet`;
  a category is a match when **any** pattern in it matches. Patterns are Rust
  `regex` syntax; `(?i)` (case-insensitive), `(?m)` (multiline anchors), and
  Unicode literals (spinner glyphs) all work. An omitted category matches
  nothing.
- **Unknown top-level keys are tolerated** (not rejected) so a manifest written
  for a newer muxa still loads its known rules. A malformed regex, empty
  `name`, or empty `command`, or invalid TOML, makes the file fail to load —
  logged at `warn`, file skipped, never a crash.

## Classifier semantics

`AgentManifest::classify(prepared_text) -> Option<ScreenState>` tests categories
in a **fixed order**, and every choice below is deliberate:

1. **`blocked` first, and STRICT.** A false `blocked` is the worst possible
   outcome — a busy agent shown as "needs input" pulls the operator's eye to a
   pane that doesn't need them. So blocked is tested first *and* the bundled
   patterns require an unambiguous approval affordance (a `[y/n]`/`(y/n)`
   prompt, explicit "permission to run", "approve this command", aider's
   `(Y)es/(N)o`). This is herdr's lesson: only clear approval/permission UI
   counts; an unknown screen is **not** blocked.
2. **`working` next** — spinner glyphs or an interrupt hint (`esc to interrupt`).
3. **`idle` last** — a bare prompt marker (`^> $`).
4. **No match → `None` → keep previous state.** An unrecognized screen never
   *transitions* a row; only a positive match to a *different* category does.
   This "unknown transitions are dropped" rule is what stops screen noise (a
   half-drawn frame, log output between turns) from flapping a row. The daemon
   also ingests **only on a state change** — a capture that classifies to the
   same state as last tick is a no-op (no needless activity-timestamp churn).

### Capture preparation

`prepare_capture(raw, 40)` runs before classification:

- **ANSI-strip.** `tmux capture-pane -ep` keeps color/attribute escapes; those
  would defeat text regexes, so CSI/OSC sequences are removed. Spinner glyphs
  and box-drawing characters are Unicode, not ANSI, so they survive.
- **Bottom 40 lines.** `capture-pane` returns the visible screen; the active
  spinner/prompt/approval UI lives near the bottom, and the slice bounds regex
  work.

### State mapping (shared with the herdr bridge)

| `ScreenState` | synthetic event                       | resulting muxa state |
|---------------|---------------------------------------|----------------------|
| `Working`     | `ToolStarted { tool = <name> }`       | `Working`            |
| `Blocked`     | `NotificationFired(NeedsInput)`       | `WaitingInput`       |
| `Idle`        | `TurnStopped`                         | `Idle`               |

Each state change also emits a trailing `Heartbeat { model: Some(<name>) }` so an
`Unknown`-kind row still names its agent. This is the identical mapping and
event shape the herdr bridge uses — both go through `synthetic::state_events`.
Screen detection passes no `message`, so a `Blocked` row's notification reads
`"<name> is waiting"` (screen inference can't reliably extract the specific
prompt text).

### Row liveness

When a pane muxa was tracking stops being a candidate — its foreground command
changed away from the agent (the agent exited but the shell stayed open) — the
task drives that synthetic row to `Stopped` via `synthetic::stop_synthetic_row`
(a `SessionEnded`), so it doesn't freeze at `Working`/`WaitingInput` forever
(the shell is alive, so the reconciler won't reap it; the row isn't `Stopped`,
so GC won't evict it). Pane *close* is still handled by the reconciler as usual.

## Attention refinement (the one exception)

Hook-authoritative precedence has exactly one carve-out. An agent CLI whose
hooks cannot report "waiting on the operator" would otherwise be able to reach
`WaitingInput` only by *not* installing muxa's hooks — the wrong trade. Today
that is the Antigravity CLI (`agy`); `AgentKind::hooks_report_attention()` is
the predicate, and Claude Code, Codex, the Gemini CLI and opencode all return
`true` and keep the original behavior unchanged.

For an attention-blind row, `muxad::synthetic::pane_ownership` returns
`AttentionBlind` instead of `Hooked`, and the detector applies
`attention_refinement_events` to the **real** row:

* `Blocked` on a row that isn't already waiting (and isn't `Error`) →
  `NotificationFired(NeedsInput)`.
* `Idle` on a row that *is* waiting → `TurnStopped { idle_confirmed: true }`,
  the backstop for a hook stream that stopped arriving mid-wait.
* Everything else → no events. `Working` in particular is left to the hooks,
  which report it in milliseconds against a seconds-late screen tick.

No `Heartbeat` is emitted (it would overwrite the row's hook-supplied `model`),
and no synthetic row is minted — `Store::apply` evicts synthetic rows from a
pane a real row owns, so one would flap in and out on every hook event. Such
panes are also never added to the detector's `tracked` set: that set drives the
stop-sweep, and a real row is not screen detection's to stop.

## Codex conversation recap observation

Codex emits only an opaque compaction item in its rollout, but renders a short
plaintext block in the terminal:

```text
─ Conversation recap ─────

  Reviewed all 18 tickets; the remaining step is to verify their states.

› Ask Codex to do anything
```

For a live hook-owned Codex row, the detector captures the pane and strictly
matches that box-rule header, then keeps only the indented body before the next
composer/status row. Ordinary prompt text mentioning "Conversation recap" does
not match. The newest visible recap is capped at 2 KB and stored in
`Agent::recap`; an identical observation is a no-op. The update does not advance
`last_activity_at`, change state, create a synthetic row, or emit a transition.

That single field already feeds the existing summary priority (`recap → title
→ prompt`) in `muxa watch`, `muxa status`, Fleet/macOS views, and `muxa
peek`. Peek also parses the fresh backdrop capture locally, closing the interval
between Codex rendering a recap and the daemon's next detection tick.

## Manifest sources

1. **Bundled** — `agy`, `cursor`, `amp`, `copilot`, `aider`, `goose` ship in the
   binary via `include_str!` (`crates/muxa/src/screen/agents/*.toml`).
2. **User overrides** — `$XDG_CONFIG_HOME/muxa/agents/*.toml`. `XDG_CONFIG_HOME`
   is honored **first and explicitly** so the path works cross-platform: on
   macOS `dirs::config_dir()` returns `~/Library/Application Support` and ignores
   `XDG_CONFIG_HOME`, which would otherwise silently miss overrides. Falls back
   to the platform config dir's `muxa/agents` when `XDG_CONFIG_HOME` is unset.

A user file whose `[agent].name` **matches** a bundled manifest **replaces** it
wholesale; a new name is **appended**. Files are loaded in sorted order; parse
failures `warn` + skip.

### Bundled-manifest confidence

The bundled manifests are **best-effort and conservative**, written **without
running** the CLIs (muxa's CI can't drive them), and every file says so in a
header comment. `agy` is the exception: its patterns were derived from a real
agy 1.1.17 session — captured tmux panes for `working`/`idle`, and agy's own
confirmation-widget labels for `blocked` — so its confidence is materially
higher than the rest of the table. The design bias when unsure is **no match over a wrong match** —
an unmatched screen just keeps the row's previous state.

| Pattern class | Confidence | Notes |
|---------------|-----------|-------|
| Braille spinner glyphs (`⠋⠙…`, `⣾⣽…`) as `working` | **Medium-high** | Near-universal across modern TUI CLIs; low false-positive risk in normal prose. |
| `thinking` / `generating` (and cursor's `reasoning`) as `working` | **Medium** | Canonical spinner labels; uncommon in ordinary code/git output. |
| `esc to interrupt` / interrupt hints as `working` | **Medium** | Common idiom (Claude/others show it); not verified per-CLI. |
| `[y/n]` / `(y/n)` as `blocked` | **Medium-high** | Strong, specific approval affordance. |
| aider `(Y)es/(N)o` as `blocked` | **Medium-high** | Aider's documented confirm format is distinctive. |
| copilot `❯ Yes` selection widget as `blocked` | **Low-medium** | The highlighted-row arrow is specific to an actual menu, not prose. |
| Generic "do you want to allow/proceed/run", "approve this command", "permission to run" as `blocked` | **Low-medium** | Plausible phrasings; specific enough to avoid most prose, but not verified against each CLI's exact copy. |
| goose "would like to call" / literal `Allow?` as `blocked` | **Low** | Speculative wording; `Allow?` requires the `?` affordance, not the bare word. |
| agy's `Requesting permission for:` header as `blocked` | **High** | Captured verbatim from a live prompt; agy prints it above every permission request, whatever the choice rows look like. |
| agy's `> N. Yes` / `> N. No` selection row as `blocked` | **High** | Requires the cursor marker AND the number, so a plain numbered list in tool output cannot match. |
| agy's `Yes, and always allow` / `No, and always deny` choice rows as `blocked` | **High** | Verbatim strings from agy's permission widget; they exist nowhere else in its output. |
| agy's `esc to cancel` footer + `Generating...`/`Running command...` as `working` | **High** | Captured from a live agy pane; the footer holds for the whole turn. |
| bare `^> $` as `idle` | **Medium** | Common prompt shape; may miss CLIs with a richer prompt (model name, token count). |
| **Excluded:** bare single English words (`working`, a lone `allow`) and loose `yes … no … ?` prose as a state marker | **Rejected** | Too common in ordinary output ("working tree clean", "these settings allow …", any sentence with yes/no/?); they falsely froze rows as busy or blocked. Removed in favor of spinner glyphs + unambiguous affordances above. `no match > wrong match`. |

Operators who run these CLIs should refine the patterns against the real UI via
a user override — that is the intended path to high-confidence detection.

## Daemon task

Configured by `[screen_detect]` (`crates/muxa/src/config.rs`, `enabled` default
`true`, `interval_secs` default `3`). Spawned in `muxad` alongside the herdr
tasks, **only** when: the feature is enabled, at least one **detectable**
backend exists (non-herdr with `caps().capture_pane`), and at least one manifest
loaded. Otherwise it returns `None` and never runs.

Each tick (`interval` with `MissedTickBehavior::Skip`; a tick is fully awaited
before the next, so **a slow tick skips rather than overlaps**):

1. **Gather candidates** — one `list_panes` per detectable backend inside a
   single `spawn_blocking`, keeping panes selected by a direct command match,
   supported wrapper/process discovery, or a live Codex registry row needing
   recap observation. When **nothing matches, zero captures run** — idle cost
   is ≈ one pane list.
2. **Stop dropped rows** — any previously-tracked pane no longer a candidate is
   driven to `Stopped` (see Row liveness).
3. **Per candidate** — skip if a live non-Codex hook owns it; for live Codex,
   capture only the recap metadata; otherwise run endpoint-scoped
   `capture_pane_on` inside `spawn_blocking` (bounded by tmux's 1s command
   timeout), `prepare_capture`, `classify`, and **on a state change** ingest the
   synthetic events through `synthetic::apply_if_unowned` (which re-checks
   authoritative ownership as a final guard against races).

The task applies directly to the in-process `Store` (like the reconciler and the
herdr bridge), not over IPC, and is drained on daemon shutdown like the other
background producers.

## Shared helpers (refactor)

The synthetic-row mechanics the herdr bridge already had were **extracted** into
`muxad::synthetic` and are now shared verbatim, rather than copy-pasted:

- `occupant_is_authoritative` — the "live non-synthetic row owns the pane" rule.
- `apply_if_unowned` — the by-pane authoritative check + apply (herdr's
  `apply_update` now delegates here).
- `stop_synthetic_row` — the liveness stop (herdr's `stop_agentless_synthetic`
  now delegates here).
- `state_events` + `SyntheticState` — the working/blocked/idle → event builder
  with the name-bearing heartbeat (herdr's `translate` now maps herdr's status
  onto `SyntheticState` and calls this; the event shapes are byte-identical, so
  every herdr-bridge test still passes).

`muxa::discovery::synthetic_session_id` was made `pub` so screen detection mints
the same registry key.

## Limitations

- Codex recap capture is visible-screen, best-effort observation: a recap longer
  than the pane or already scrolled away before a detection tick cannot be
  recovered from the rollout's opaque compaction item. Once observed, it is
  retained in the agent snapshot. Paneless Codex sessions have no screen source.
- Hook-less custom launch wrappers remain undiscoverable unless their command
  is declared in a manifest. A live hooked Codex row can still select the
  metadata-only recap path even when the launcher owns `pane_current_command`.
- Best-effort bundled patterns (see the confidence table).
- Blocked notifications carry a generic `"<name> is waiting"` message; screen
  inference does not extract the specific prompt text.
- zellij without its plugin reports `capture_pane = false` and is skipped.
