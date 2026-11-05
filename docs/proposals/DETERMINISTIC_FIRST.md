# Deterministic first: audit and conversion plan

Status: proposal (2026-11-05). Audit base: `origin/main` 81503b0.
Principle: [DIRECTION.md › Deterministic first](../DIRECTION.md) (#360).

Every fact muxa acts on (who an agent is, which pane it is in, what state it
is in, whether a prompt landed, whether it hit a limit) must come from a
deterministic source whenever one exists. A heuristic is allowed only where
no deterministic source exists, must be labeled as one, and never triggers a
destructive action on its own.

This document has three parts:

1. [Policy](#1-policy): provenance tags, the destructive-action rule, and precedence.
2. [Inventory](#2-inventory): every place muxa infers state or identity today, with a verdict.
3. [Backlog](#3-ranked-conversion-backlog): the ranked conversion order. The first two items ship with this proposal.

Line numbers refer to 81503b0 and drift. Grep for the named function.

---

## 1. Policy

### 1.1 Provenance

Every signal that can change a row (state, pane, pid, rate limit, submission
ack) carries a provenance tag. The tags fall into two classes.

| Class | Tag | Source | Examples today |
| --- | --- | --- | --- |
| deterministic | `hook` | provider hook event, with its payload ids | Claude/Codex `UserPromptSubmit`, `Stop`, `StopFailure`, `SessionStart` |
| deterministic | `provider_record` | provider-written session or transcript record, with structured fields | Codex rollout `payload.rate_limits`, Claude transcript `error:"rate_limit"` / `apiErrorStatus:429`, Claude `sessions/<pid>.json` |
| deterministic | `provider_api` | provider API answer or provider-issued id | Codex app-server JSON-RPC, `_meta.threadId`, `$CODEX_THREAD_ID` |
| deterministic | `launch_mark` | a mark muxa set itself at launch | `@muxa_worker_launch`, `@muxa_agent_*`, `@muxa_spawn`, `MUXA_SESSION_ID` on owned PTYs |
| deterministic | `process_exact` | the exact pid, its start time (`/proc/<pid>/stat` field 22), its env | `procStart` check in swap verification |
| heuristic | `screen` | pane capture text, glyphs, SGR attributes | screen manifests, composer and draft reading, dialog matching |
| heuristic | `process_name` | argv/comm basename matching, "shallowest match" | `primary_pid`, discovery classifier, `find_launch` |
| heuristic | `elimination` | "the only candidate left", cwd matching | `correlate_paneless_codex` |
| heuristic | `timing` | sleeps, quiet periods, staleness timers, recency ordering | `PROMPT_SUBMIT_GRACE`, startup window, "newest `last_activity_at` wins" |
| heuristic | `text_match` | free-text substring or regex over a message | `^Selected model is at capacity`, "You've hit your limit" in `tool_result` |
| heuristic | `model` | an LLM's judgement | `automation_judge` |

A process-tree walk keyed on the exact pid and its env is `process_exact`. The
same walk keyed on a program *name* is `process_name`.

Implementation note: muxa already has the seed of this. `RateLimitSource`
(`CodexRollout`, `StopFailure`, `Statusline`, `Transcript`) and the synthetic
row prefix are provenance in all but name. The proposal is to make it one
enum, store it next to every inferred field on `Agent` (`state_source`,
`pane_source`, `rate_limit_source`), and show it in `muxa agents --json`,
watch and the board. A heuristic-sourced state renders with a marker such as
`~idle`, so an operator can tell a guess from a fact.

### 1.2 Destructive actions

Destructive means anything that stops, removes or rewrites what an agent or
the operator owns:

- stop a provider: `workers prune --execute`, swap exit, SIGTERM/SIGKILL;
- swap or relaunch an account;
- reap, stop or delete a registry row; rebind a row's pane;
- type into a pane on muxa's own initiative (automation Enter, dialog answers, `/exit`, auto-`continue`).

Rule: **the fact that authorizes a destructive action must be
deterministic. A heuristic may only veto it, that is, make muxa do less.**

| Allowed | Not allowed |
| --- | --- |
| prune is authorized by hook-reported idle time and a `process_exact` fence. The screen is read only to refuse when a draft or dialog *might* be there. | stop a row because the screen *looks* empty or because no process *named* `claude` was found |
| swap is authorized by a `provider_record` or `hook` rate-limit. Screen dialog matching only steers keystrokes inside an operation that was already authorized. | swap because a transcript record merely carries `isApiErrorMessage` (fixed by this proposal, B1) |
| a screen observation clears a waiting row to `Idle` | a screen observation stops, swaps or rebinds a row |

When the authorizing fact is only heuristic, the action degrades to a
**suggestion**: an attention item, or a dry-run row with
`provenance: heuristic`. It is never executed.

### 1.3 Precedence

1. A deterministic signal beats a heuristic one for the same field, however
   recent the heuristic is. A heuristic never overwrites a deterministic value
   while that value is valid. It may fill a field that has no deterministic
   value. This generalizes the existing "hooked row wins" rule
   (`synthetic.rs` `apply_if_unowned`, `pane_ownership`).
2. Among deterministic signals for one field, the newer event wins by its
   provider timestamp, not by arrival order.
3. Among heuristic signals, the narrower source wins: `launch_mark` beats `screen`, which beats `process_name`, which beats `elimination`, which beats `timing`.
4. An identity bound heuristically (pane by elimination or by screen) is
   *provisional*. It is good for display and routing a message, but it does not
   authorize stop, swap or rebind-and-stop until a deterministic source confirms
   it.
5. A heuristic that contradicts a deterministic fact is logged with both
   provenances and changes nothing. That log is the evidence for converting
   the heuristic.

---

## 2. Inventory

Verdicts:

- **CONVERT**: a deterministic source exists or can be made; replace the heuristic.
- **KEEP-AS-FALLBACK**: no deterministic source is possible; keep it, tag it, and keep it veto-only.
- **DROP**: remove, or demote to display only.

Effort is S (≤1 day, one file), M (a few files or a new field), or L (a protocol, schema or launch-path change). Risk is the regression risk of making the change.

### A. Agent state from the screen

| # | Mechanism and evidence | Decides | Failures (fix commits) | Deterministic alternative | Verdict | Effort / risk |
| --- | --- | --- | --- | --- | --- | --- |
| A1 | Screen-manifest detector for hookless CLIs: `muxad/src/screen_detect.rs` (40-line tail), `screen.rs:139` `classify`, `screen/agents/*.toml` | synthetic rows Working/WaitingInput/Idle; Stopped when the command stops matching (`synthetic.rs:216`) | 6dee770 (#205) stuck idle; 70dd5cc (#236) idle/stopped flap every 3s; 87e3712; d25c7f2; 54e18ef; the doc's "Excluded" list | none for aider, amp, copilot, cursor, goose. OSC 133/9 if a CLI emits them; muxa parses none today. | KEEP-AS-FALLBACK (no hooks exist) | — / — |
| A2 | Attention refinement on hooked rows: `synthetic.rs:150` `attention_refinement_events`, `STARTUP_ATTENTION_WINDOW` 5 min (`:95`) | a hooked Claude/Codex row that is still `Idle` within 5 min of launch can be moved to WaitingInput from the screen | Codex trust gate fires before hooks exist | end the window on the row's **first hook event**, not on a timer. agy has no approval hook, so it keeps the refinement. | CONVERT (window); KEEP (agy) | M: needs a "hook seen" field on `Agent` / low |
| A3 | Claude auto-mode dialog recovery: `screen_detect.rs:101,518`, `adapters/claude.rs:31` `idle_composer_from_capture`, `state.rs:1983` | WaitingInput → Idle (CAS on an unchanged row) | 0046653 | none: Claude fires `Notification` when the dialog opens and nothing when it is resolved. A later `UserPromptSubmit`/`PreToolUse` already supersedes it. | KEEP-AS-FALLBACK (only clears, never stops) | — / — |
| A4 | Codex "Conversation recap" from the screen: `adapters/codex.rs:37` | `Agent::recap` text only | 7d8d18c, 0a3b7a2 | the rollout's compaction item is opaque | KEEP-AS-FALLBACK (display) | — / — |
| A5 | Turn running: `account_swap.rs:3484` `turn_running` ("esc to interrupt", spinner counter) | worker stop refuses; swap follow-up evidence `ScreenActivity` | 7ea3868 (a dim placeholder must not override a running turn) | the row's hook state (`Working` since `UserPromptSubmit`/`PreToolUse`) and Codex rollout `task_started`, both already ingested | CONVERT: consult hook state first; keep the screen as an extra veto | S / low |
| A6 | Composer and draft reading: `worker_stop.rs:69` `read_composer`, `:126` dim-SGR `codex_placeholder_shown`; `account_swap.rs:3512` `composer_draft` | worker stop types `/exit`/`/quit` only on Empty; swap re-types the draft | bd0c681 (every Codex worker refused); 4412044 (narrow pane cuts the placeholder short); 7ea3868 | none: no provider reports a human's unsent draft | KEEP-AS-FALLBACK, **veto-only** (Unknown ⇒ refuse; the stop itself is authorized by idle time and the process fence) | — / — |
| A7 | Swap exit and resume screen machine: `account_swap.rs` `exit_claude` (~3650), `EXIT_DIALOGS` 4496, `STARTUP_DIALOGS` 4380, `PROMPT_MARKS` 4064, `blocked_screen` 4134 | keystrokes inside an authorized swap; SIGTERM/SIGKILL fallback | 2520efb, de8de61, d043ab0, d2a321c, 03d98f9, b7f79ce, a547198, a5c9997 (#345), 8022e10 (#355) | process exit is `process_exact` and already used. `SessionStart` partly proves resume. Pre-seeding trust/bypass acceptance at launch removes most startup dialogs. | KEEP-AS-FALLBACK for dialogs; CONVERT readiness to `SessionStart`/process | M / medium (swap is the riskiest path) |
| A8 | Automation PressEnter gate: `automation_choice.rs:24`, `muxad/src/main.rs:2527` | presses Enter on a confirmation menu (rule disabled by default) | — | answer through the provider's permission hook decision (`PreToolUse` / `PermissionRequest`) instead of keystrokes | CONVERT | M / medium |
| A9 | Automation LLM judge: `automation_judge.rs:113` | whether a rule fires (default `observe_only`) | — | none by construction | KEEP, tagged `model`; must not execute a destructive rule unless another deterministic fact authorizes it | — / — |
| A10 | Staleness timers: `state.rs:1934` `mark_stuck_idle_from` (off by default), `AGENTLESS_PANE_IDLE` 1h (`reconcile.rs:54`), `mark_stale_paneless_stopped` 24h, `cap_live_orphans` | flip rows to Idle or Stopped; GC deletes later | aed15da, 44c158c, ee709a9 | process exit of the exact pid (see C1), `SessionEnd` hook | KEEP as tagged `timing` *housekeeping* for rows with no process; never as the sole cause of a stop for a row whose pid is alive | — / — |

### B. Rate limits and errors

| # | Mechanism and evidence | Decides | Failures | Deterministic alternative | Verdict | Effort / risk |
| --- | --- | --- | --- | --- | --- | --- |
| B1 | Claude transcript fallback treated `isApiErrorMessage:true` as a limit: `adapters/transcript.rs:161` | `RateLimited{Transcript}` → `Error` → arms resume-after-limit / swap-on-limit | **Real transcripts on this host: 36 `authentication_failed`, 3 `server_error` (one 529), 2 `invalid_request` records, all classified as limits.** Every real limit carries `error:"rate_limit"` + 429. | the structured tags themselves | **CONVERT, done in this PR** | S / low |
| B2 | Subagent limit by substring in `tool_result`: `transcript.rs:196` ("You've hit your limit", "Claude usage limit reached") | same as B1 | current wording is "You've hit your **weekly** limit" / "**session** limit", which the substring no longer matches | none: subagent errors reach the parent only as text | KEEP-AS-FALLBACK, tagged `text_match`; it must not arm swap alone (show as attention) | S / low |
| B3 | Codex rollout `payload.rate_limits.rate_limit_reached_type`: `codex_rollout.rs:163`, `reconcile.rs:478` | `RateLimited{CodexRollout}` | 0f2f186 inferred caps from saturation; reverted in 4402349 | — (already deterministic; the file format is unofficial) | KEEP (deterministic). Add a format canary test. | — |
| B4 | Claude `StopFailure` `error=="rate_limit"`: `claude.rs:261`; statusline `rate_limits` (telemetry, not `Error`): `state.rs:1068` | limit fact | — | — | KEEP (deterministic) | — |
| B5 | `resume-codex-capacity` matches `^Selected model is at capacity\.` on the notification text: `automation_defaults.rs:13` | sends `continue` | — | a structured error code in the Codex rollout or hook, if Codex writes one. Check before converting. | KEEP-AS-FALLBACK until a code is found | S / low |
| B6 | Claude resume timing: limits from B1/B4 have no reset time, so resume-after-limit waits 20 min | when `continue` is typed | — | parse the reset from the statusline `rate_limits.*.resets_at`, which is already ingested | CONVERT | S / low |

### C. Identity, location and liveness

| # | Mechanism and evidence | Decides | Failures | Deterministic alternative | Verdict | Effort / risk |
| --- | --- | --- | --- | --- | --- | --- |
| C1 | `primary_pid` by comm basename (`process_tree.rs:293,375`) drives "stop rows whose pane runs no agent" (`state.rs:2030`, after 1h) | row → Stopped | dcb41d3 (#240) and its revert (#242): **~18 live agents stopped**; 06dddac (#243) | record the exact agent pid + start time at the first hook (hooks run as children of the agent) and test liveness of *that* pid | CONVERT | M / medium |
| C2 | Stale-pane reap and duplicate collapse: `state.rs:2897-3032`; several rows on one pane → **newest `last_activity_at` wins**; unknown socket counts as live | deletes rows | 2108767, a81c0a5, 70a70e2, 5f41d98, 6ba4eca | identity = (socket, server pid/start, pane) + hook session id; the loser is whichever has no deterministic claim, not the older one | CONVERT (ranking); KEEP pane-gone reap (tmux listing is deterministic) | M / medium |
| C3 | Codex thread → pane binding, `codex_binding.rs:69`: (a) `codex resume <id>` argv; (b) thread prompt text found on screen; (c) cwd elimination (`state.rs:2458`) | binds rows; **(b) stops the previous owner** (`state.rs:2853`) | f806eca, e17cf5b, b932607, ee60e00, 033969e, 2625278, 4402349, 689a91b (#239), 668d098 (#244); the module's own "known gap" (short `/new` prompt) | (a) is deterministic. For new TUIs, a launch mark joined to the thread id (D1 below). The app-server knows thread → connection; check whether its JSON-RPC exposes the client pid. | CONVERT: (a) + launch join. (b) and (c) become provisional (policy 1.3 #4): routing only, never stop or swap. | L / medium |
| C4 | Hook pane from env order `MUXA_HOST`, `RMUX_PANE`, `ZELLIJ_PANE_ID`, `HERDR_PANE_ID`, `TMUX_PANE`, `CMUX_SURFACE_ID` (`adapters/hook.rs:70,243`) | which pane a hook is stamped onto | a212bd0, 34d5bf5, e6639a7, 3070f64, 7a3556a (#238) | the env of the exact hook process is deterministic for a private agent; the nesting order is a guess when two hosts are present | KEEP (deterministic for private agents); nesting → `launch_mark` when present | S / low |
| C5 | Shared app-server detection by walking ancestor argv with a hardcoded `CODEX_VALUE_OPTIONS` (`proc_ancestry.rs:122-221`) | whether the pane env is "borrowed" | c6c9802, 1ff79f6, 23f06a6, f7ceb31 | the presence of `CODEX_THREAD_ID` / `_meta.threadId` already proves "Codex thread". Shared vs private still needs the argv walk. | KEEP-AS-FALLBACK; add a canary for new Codex global options | S / low |
| C6 | **CLI defaults read raw `$TMUX_PANE`** with no borrowed check: `agent_launch.rs:1220,1325` (placement), `tmux_work.rs:1019` (`work done`), `:1788` (window identity), `worker_cmd.rs:81,660` (release/prune origin) | placement target; which pane's alias/work is marked done | under a shared app-server these name the **first** pane | `pane_env_is_borrowed()` already exists | **CONVERT, done in this PR** (`own_tmux_pane`) | S / low |
| C7 | IPC caller provenance reads `TMUX_PANE=` from `/proc/<peer>/environ` with no shared-server guard and no `RMUX_PANE` (`ipc.rs:3405-3444`) | `origin_match` and "origin mismatch" text only | — | the same borrowed check, or `_meta.threadId` | CONVERT | S / low; deferred because `ipc.rs` is being edited by another session |
| C8 | Recap default reads `$TMUX_PANE`: `muxa-cli/src/main.rs:149` | which pane's recap is shown | — | `own_tmux_pane()` | CONVERT | S / low; deferred because `main.rs` is being edited by another session |
| C9 | Backfill discovery by `pane_current_command` + descendants within 4 levels (`discovery.rs:54-115`); launch registration polls the same classifier for 5s (`agent_launch.rs:1032`) | synthetic rows; which row a launched agent gets | 81503b0 (#359), 792ec8f, 9a351b0, 17a3966, 87e3712, ca5a6f1 | the launcher knows pane, program and launch id. Register by launch mark (D1). | CONVERT for muxa-launched agents; KEEP-AS-FALLBACK for agents started outside muxa | M / low |
| C10 | `reap_dead_pids` checks the pid alone (`state.rs:2091`) | task rows → Stopped | — | pid + start time | CONVERT (with C1) | S / low |
| C11 | Swap target verification: Claude `sessions/<pid>.json` + `procStart` (deterministic); Codex has no such file → registry value or resume argv (`account_swap.rs:941-977`) | which conversation is resumed after the swap | 40d05c3, cf0c99b, 622a2dc, c30d854 (#346) | — | CONVERT: refuse a Codex swap unless the binding is deterministic (C3 (a) or launch join) | S / medium; deferred because `account_swap` is being edited by another session |
| C12 | `agent_locator.rs:154` weighted keyword search, newest first | `muxa agent find` candidates | — | — | KEEP (scoped as "candidates, not proof"; read-only) | — |
| C13 | Stale MCP scan by argv + exe inode (`stale_agents.rs:143`) | report only, never kills | b277de9 | — | KEEP (read-only) | — |

### D. Launch marks (the missing join key)

| # | Mechanism and evidence | Gap | Verdict | Effort / risk |
| --- | --- | --- | --- | --- |
| D1 | Launches write pane options `@muxa_worker_launch` (workers only, `agent_launch.rs:845`), `@muxa_agent*`, `@muxa_spawn`. The exec line is `exec <agent argv>` (`agent_launch.rs:778`). | No launch id reaches the agent's environment, so hooks cannot carry it. Claude is not given `--session-id <uuid>`. The session ↔ pane join is therefore always inferred (C3, C9). | CONVERT: set `@muxa_launch_id` on every launch, export `MUXA_LAUNCH_ID` in the exec line, have the hook adapter forward it, and the daemon joins hook session ↔ pane on it. For Claude, pass `--session-id`. | M / medium. This is the keystone: it unblocks C1, C3, C9 and C11. |

### E. Prompt delivery

| # | Mechanism and evidence | Decides | Failures | Deterministic alternative | Verdict | Effort / risk |
| --- | --- | --- | --- | --- | --- | --- |
| E1 | Bracketed paste on tmux/rmux, raw typing elsewhere (`backend/mod.rs:538`); `PROMPT_SUBMIT_GRACE` 250ms before Enter on non-tmux hosts (`backend/mod.rs:70`) | delivery mechanics | 3341c96 (#39) sleep; f8bb739 (#226) paste fixes swallowed Enter | Codex app-server `turn/start` (socket already reachable, `mcp_reload.rs`) | KEEP keystrokes as the universal transport, tagged; CONVERT Codex to `turn/start` where the server is reachable | L / medium |
| E2 | Submission ack: wait ≤750ms for a `PromptSubmitted` hook with the same session/socket/text (`prompt_delivery.rs:41`); `None` = unknown, never a resend | `submission_confirmed` | — | this *is* deterministic. Extend it to relay (`relay.rs:402`), fleet (`fleet_manager.rs:825`), dashboard, automation and reload, which hard-code `None`. Add Codex rollout `user_message` as a second ack. | CONVERT (coverage) | S–M / low |
| E3 | `reload.rs:1478` types with `send-keys -l`, sleeps 300ms, sends Enter, gated by "screen stopped changing" | relaunch `continue` | the same paste-burst class f8bb739 fixed elsewhere | `send_prompt_text_on` + the E2 ack | CONVERT | S / low |

### F. Backend parity

| Backend | Pane list | Pane pid | Capture | Send | Paste | Marks | Push events |
| --- | --- | --- | --- | --- | --- | --- | --- |
| tmux | complete | yes | yes | yes | yes | yes | — |
| rmux | complete | yes | yes | yes | yes | yes | — |
| herdr | complete or incomplete | `process_info` | `pane.read` | `pane.send_text` | no | no | `agent_status_changed` (herdr's own screen heuristics) |
| cmux | own surface only | no | no | `surface.send_text` | no | no | none |
| zellij | plugin snapshot, 10s TTL | plugin only | no | no | — | no | plugin `PaneUpdate` |

| # | Finding | Verdict | Effort / risk |
| --- | --- | --- | --- |
| F1 | herdr `SocketMissing` ⇒ authoritative empty host ⇒ reap (`herdr.rs:236`). This is right when the server is down, and wrong if the socket path was resolved wrongly. | CONVERT: authoritative only when the socket path comes from env or config, not a guessed default; otherwise `incomplete` | S / low |
| F2 | herdr state is imported from herdr's own screen detection (`docs/HERDR.md`) | KEEP, tagged `screen` (provenance travels with the row) | S / low |
| F3 | zellij id mismatch (code reading, not verified at runtime): the plugin mints `zellij:terminal:<id>` (`muxa-zellij-plugin/src/lib.rs:60`); the hook passes `$ZELLIJ_PANE_ID` verbatim (`hook.rs:292`). Real zellij exports a bare number, and `pane_id_host_kind` needs the `zellij:` prefix, so hook rows likely never join plugin rows. | verify, then fix as a bug (namespace the env value in the hook adapter) | S / low |
| F4 | cmux and zellij cannot capture, so screen-gated actions (worker stop, swap, PressEnter) refuse there. That is the correct veto-only behavior. | KEEP | — |

---

## 3. Ranked conversion backlog

Rank = (destructive reach of the heuristic × how often it misfires) ÷ effort.
Items 1–2 are implemented in this PR, each in its own commit with tests.

| Rank | Item | Why first | Effort | Status |
| --- | --- | --- | --- | --- |
| 1 | **B1**: only `error:"rate_limit"` / `apiErrorStatus:429` mark a Claude transcript limit | a misclassified auth or server error is the trigger for resume/swap; observed in real transcripts | S | **done** (`fix(claude): only structured rate_limit/429 tags…`) |
| 2 | **C6**: CLI defaults ignore a `$TMUX_PANE` borrowed from a shared Codex app-server | acts on another thread's pane; the deterministic check already existed | S | **done** (`fix(cli): never default to a pane inherited…`) |
| 3 | **D1**: launch id on every launch (`@muxa_launch_id` + `MUXA_LAUNCH_ID` + Claude `--session-id`) | keystone for C1, C3, C9, C11 | M | next |
| 4 | **C3**: Codex binding via resume argv + launch join; prompt-on-screen and cwd elimination become provisional (no stop, no swap) | removes a screen guess that can stop a row and feed a swap | L | after D1 |
| 5 | **C1 + C10**: exact pid + start time liveness instead of comm-name `primary_pid` | the #240 class (18 live agents stopped) | M | after D1 |
| 6 | **A5**: hook state decides "turn running" before the screen does | cheap; already-ingested signal | S | ready |
| 7 | **C7 / C8 / C11**: the same borrowed-pane and deterministic-binding rules in `ipc.rs`, `main.rs`, swap | finishes C6 | S each | blocked on files other sessions are editing |
| 8 | **E2 + E3**: submission ack on every send path; reload stops using `send-keys -l` | delivery claims become facts | S–M | ready |
| 9 | **F1 + F3**: herdr empty-host authority; zellij pane-id namespace | a wrong reap / never-joined rows | S | F3 needs a runtime check first |
| 10 | **A2**: end the startup attention window on the first hook | removes a 5-min timer | M | ready |
| 11 | **B6**: resume-after-limit uses the statusline reset time | removes a 20-min guess | S | ready |
| 12 | **A8**: approvals through hook decisions instead of Enter | removes automated keystrokes | M | design |
| 13 | **C2**: dedup by deterministic claim, not recency | reap correctness | M | after D1 |
| 14 | **Provenance enum** on `Agent` + UI marker (§1.1) | makes the policy visible and testable | M | can start any time; each conversion adds its tag |

Not converting (KEEP-AS-FALLBACK, tagged, veto-only): A1, A3, A4, A6, A7 dialogs, A9, B2, B5, C5, C9 for agents started outside muxa, C12, C13, F2, F4.

## 4. Acceptance for each conversion

- The deterministic path decides whenever its source is present. The
  heuristic runs only when the source is absent, and its output carries the
  heuristic tag.
- A test feeds a contradicting heuristic signal next to the deterministic one
  and asserts that the deterministic one wins.
- A mutation check: reverting the conversion fails at least one test.
- Any rule that mutates rows (stop, reap, rebind) gets a live dry-run before
  merge, as required since #240/#242.
