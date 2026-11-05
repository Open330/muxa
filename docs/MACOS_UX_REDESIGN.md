# Muxa.app UX redesign: from "where things are" to "what needs doing"

Status: every step shipped; see Delivery.

## Why

Muxa's purpose is to let one person run many coding agents across many hosts
without babysitting them. The loop the app must serve is:

1. **Notice** — something needs me (an agent is blocked, broken, out of quota,
   or a peer asked for a decision).
2. **Understand** — what happened, on which host and account, and what my
   options are.
3. **Act** — resolve it in one step, from where I noticed it.
4. **Delegate** — tell Muxa how to handle that case next time.

The app today is organised like an IDE (activity bar, side bar, tabs), which is
good at step 2 and poor at 1, 3 and 4:

- **No agent can be acted on directly.** No stop, resume, retry or account
  switch exists anywhere in the views; interrupt, press-Enter and
  switch-account exist only as automation rule actions.
- **Rate-limited agents fall through every net.** They are excluded from
  attention counts, the Inbox, Explore's pinned section and notifications
  "because they resume on their own" — but a fresh install ships every resume
  rule disabled, so by default they do not. The only action offered on a cap
  is "Set Up Auto-Resume…", which opens Settings. The "Add the Session-Limit
  Rule" shortcut is permanently disabled because muxad always installs a
  disabled starter of that name.
- **`muxa swap` (stop, relaunch on another `aas` account, resume the same
  conversation, carry the cut-off turn on) is invisible.** The aas module's
  "Switch account" changes the machine-wide default and explicitly does not
  touch running agents — the opposite of what the user wants at a cap.
- **Editing a `swap_account` rule in the app silently rewrote it** (dropped
  `account` and `text`).
- **"Needs attention" is counted five different ways** (Inbox badge, Explore
  badge, Dock, Command Center, Inbox metrics).
- **Several links are dead ends**: "N more in Explore › Needs attention" is
  plain text; "Your decision needed" switches the side bar without opening the
  message; opening an agent always yanks the side bar to Inbox.

## Principles

1. **State implies action.** Wherever an agent's state is shown, the action
   that state calls for is one click away, with the same name and icon
   everywhere.
2. **One attention model.** A single summary type defines "needs you",
   "out of quota", and "unread"; every badge and count reads from it.
3. **Policy over chores.** When the same action would be repeated (resume
   after a cap, switch account at a cap), offer to make it the policy right
   there, as a plain choice — not a rule editor.
4. **Operations are visible.** Anything Muxa does on its own (a swap job, an
   automation firing) shows up as an activity the user can follow, retry or
   cancel.
5. **Nothing is lost on round-trip.** The app never rewrites configuration it
   does not fully understand.

## Design

### 1. Agent actions (one model, every surface)

`MuxaAgentAction` is computed from an agent's status, host and the daemon's
capabilities. The same list drives:

- the pane editor header (primary action as a button, the rest in a menu),
- the Explore row context menu (today: only Open / Open Pinned),
- agent detail,
- Command Center "Look at now" and the Limited row,
- the rate-limit badge popover,
- the command palette (`Agent: …` commands on the focused agent),
- notifications (a limited agent gets a "Switch Account" action).

| State | Primary | Also |
|---|---|---|
| Limited | **Switch Account & Resume** | Resume at reset (policy), Open |
| Needs input | Open & Reply | Send "continue" |
| Error | Open | Send "continue" |
| Idle / Done | Send Prompt… | Move to Account… |
| Working | Open | Move to Account… (forces a stop; warned) |

### 2. Swap: switch account and resume

The app drives swaps through `work_command` running `muxa swap … --json` on the
agent's own host (local or Fleet). This reuses the CLI's full swap logic,
request keys, daemon jobs, `--detach`, `--status`, `--retry` and `--cancel`,
on every host, with no swap logic duplicated in Swift.

- **Swap sheet** (one agent, a selection, or "all limited agents"):
  target account (Auto, or an `aas` account with its quota bars), follow-up
  text (defaults to `[swap]`), and a live plan preview from `--dry-run`
  ("claude@work → claude@personal, resumes conversation 3f2a…, sends
  'continue'"). Start runs `--detach` under an app-minted request key.
- **Activity**: a status-bar item appears while swap jobs run; its popover
  lists jobs with per-agent outcomes, follow-up delivery, and Retry / Cancel.
  Jobs started by automation (`admitted` in the run log) link to the same view.

### 3. Limit policy, not a rule editor

"When an agent hits its usage limit" becomes one choice shown in the
rate-limit popover, the usage popover and Settings › Automations:

- **Switch to another account and continue** — enables a `swap_account` rule.
- **Wait for the reset, then continue** — enables the `resume-after-limit`
  starter.
- **Just tell me** — no resume rule; limited agents count as needing attention
  and notify.

Attention suppression for limited agents now depends on this policy: a capped
agent is only "taken care of" when a policy will actually act on it.

### 4. One attention summary

`MuxaAttentionSummary`, computed once per change by the attention center,
feeds every count:

- **Needs you** — agents whose state needs the person (blocked, errored, or
  capped with no limit policy acting), peers' requests for a decision, and
  unread replies. The Inbox badge, the menu bar, Command Center's "Need
  attention".
- **Unread** — agent panes with a turn or a wait not yet seen. Explore's
  badge.
- **Dock** — both, each item once.

Commands still waiting on an agent are listed but never counted. Every count
that looks clickable is a button that opens the matching list.

### 5. Navigation fixes

- Opening an agent keeps the current side bar unless it was opened from the
  side bar itself.
- "Your decision needed" and human-request notifications open the message.
- "N more …" rows are buttons.
- The palette gains Fleet Dispatch, Show Ask, Show Files, Settings, Usage,
  Swap and the per-agent actions.

### 6. Health

`swap_health` (and the reasons swaps are unavailable) appears in the swap
sheet and Settings, so "why didn't it switch?" has an answer in the app.

## Delivery

| Step | State |
|---|---|
| 1. Automation model: `swap_account`, `account`, `admitted`, job links (the data-loss fix) | done |
| 2. Agent actions, swap sheet, swap activity | done |
| 3. Limit policy, policy-aware attention, limited notifications with Switch Account | done |
| 4. Navigation fixes (Inbox routing, "N more" link), palette commands | done |
| 4b. Split the touched surfaces out of `ContentView.swift` / `WorkConsoleViews.swift` | done: Command Center, Ask, Inbox, Explore, side bar, workbench chrome and detail views each have a file |
| 5. Restore with continue (`continue` on `mux_snapshot_restore`, `mid_turn` in the plan) | done |
| 6. One attention summary feeding every badge and count | done (`MuxaAttentionSummary`) |
| 7. `swap_health` in the swap sheet and Settings | done (local daemon) |

Not yet covered by the app: per-agent account identity on the wire (the
daemon reports none; the swap plan names `from`/`to` per agent), and a
remote host's swap health (its swaps report their own refusals).
