# Account swap

When one account's usage cap is spent, the conversation is still fine. As
long as the accounts share `projects/` (Claude) and `sessions/` (Codex), the
same provider session id resumes under any account. `muxa swap` moves agents
onto another account and resumes each one in its own pane.

Accounts come from an *account provider*, chosen per provider (Claude,
Codex) in this order:

1. **`[[accounts.<provider>]]` in `config.toml`** — no external tool. Each
   account is a name and the environment that selects it. muxa launches with
   that environment and recognises a running agent's account from its own
   environment however it was started. The accounts are used in the listed
   order (see below).
2. **`aas`** (Agent Account Switcher), optional: used only for a provider
   with no `[[accounts]]` entries, and only when the `aas` binary is
   installed (or `[swap].aas` names one). Candidates come from
   `aas candidates --json`, launches go through `aas e <account>`.
3. **None.** New agents start on whatever account the CLI is logged into,
   and a swap reports that there is no account to move to and touches
   nothing. A capped agent can still be recovered: the `resume-after-limit`
   rule (see [Automation](AUTOMATION.md)) waits for its reset and resumes it,
   and needs no accounts at all. It is installed disabled; turn it on with
   `muxa automation enable resume-after-limit`.

```toml
[[accounts.claude]]
name = "work"
env = { CLAUDE_CONFIG_DIR = "~/.claude-work" }

[[accounts.claude]]
name = "personal"
env_file = "~/.secrets/claude-personal.env"   # CLAUDE_CODE_OAUTH_TOKEN=…

[[accounts.codex]]
name = "work"
env = { CODEX_HOME = "~/.codex-work" }
```

`env_file` is sourced by a small `sh` wrapper at launch, so a token never
appears on the typed command line or in shell history; it is read again only
to recognise the account. `[accounts]` is host-local: config sync never
copies it.

`auto` over configured accounts follows a fixed rule: a new agent starts on
the first listed account, and a swap moves an agent to the first eligible
listed account other than its own, skipping the one it was capped on. muxa does not
yet rank configured accounts by the usage it observes on other agents: it
cannot yet tell deterministically which account a running agent's limit
reports belong to. Ranking by observed usage is coming in a follow-up (#367).
Name an account (`--to <account>`, `account = "<name>"`) to choose one
yourself.

Once a provider lists accounts, its swaps use only those: an `aas` name is
not one of them, and `--bare` no longer applies, since an agent started bare
is relaunched with the account's environment like any other. An `env_file`
must be an absolute or `~/` path.

```console
$ muxa swap --dry-run
planned  %1         claude_code work -> personal
         $ sh -c 'set -a; . "$0"; set +a; exec "$@"' ~/.secrets/claude-personal.env claude --dangerously-skip-permissions --resume 090b45a6-…
         then send "continue"
```

For each agent, muxa:

1. reads how the agent was started from its pane's process tree (a bare
   `claude` / `codex`, `env … claude`, or `aas e <account> …`) and which
   account it runs on (its environment, for configured accounts);
2. picks the next account (configured accounts in the listed order,
   `aas candidates --json` with the `aas` backend, or `--to <account>`). Under `aas`, a bare agent
   counts as being on the account `aas` marks active, so `auto` never
   "moves" it onto the account it is already spending;
3. stops the agent and waits for the pane's shell. Claude gets `/exit` (after
   an Escape when it is mid-turn), the way a person quits it, and its exit
   dialogs are answered so nothing is lost: background work is moved out of
   the way and a worktree is kept. A session that owns scheduled tasks is
   left on its account (exiting would cancel them) and reported as skipped.
   Codex, and a Claude that does not exit, get `SIGTERM`, then `SIGKILL`;
4. types the same launch line back, on the new account, with `--resume <id>`
   (Claude) or `resume <id>` (Codex). Flags and their values are kept; the
   initial prompt is dropped so the task is not restarted;
5. waits for the resumed agent to appear and its screen to settle, answering
   the startup dialogs a new account brings up: Claude's folder trust, Codex's
   folder trust, its update prompt (skipped, nothing installed), "Working
   directory · resume" (the session's directory), "Resume goal", and "open in
   another app" (retried a few times while the old process lets go). A dialog
   is answered only when the screen ends in it, by its own menu row or last
   line, and only for the relaunched process resuming the conversation.
   Claude's bypass-permissions warning is accepted only when the stopped
   agent and the relaunch both run with `--dangerously-skip-permissions`;
   trusting a folder's settings, the auto-mode tour, and any other dialog are
   left to you: the swap stops there, names it, and types nothing;
6. sends the follow-up prompt when one applies.

## Choosing which agents

| Command | Agents |
| ------- | ------ |
| `muxa swap` / `muxa swap --limited` | Every agent stopped by a usage cap. |
| `muxa swap %5 herdr:p9 <session-id>` | Exactly these panes or provider session ids. |
| `muxa swap --all` | Every idle Claude / Codex agent. Mid-turn agents are skipped. |
| `muxa swap --all --force` | Also stops mid-turn agents. |

`--provider claude|codex` narrows any of these to one provider.

A named target is resolved to exactly one live pane first. A bare `%81`
means `rmux:%81` when that is the only pane it can mean. A name that matches
no live pane, or several (the same `%N` on two servers), is reported and
swaps nothing. `--exclude` is resolved the same way, and an ambiguous
exclude protects every pane it may mean. A live pane muxa has no agent row
for is reported too, instead of silently missing.

A row muxa only saw on screen, never from the agent's own hooks
(`"registered": false`), has no trustworthy state. `--all` and `--limited`
list it without touching it. Naming it swaps it only with `--force`, and it
gets a follow-up only under `--continue-when always`.

Before stopping an agent, the swap checks which conversation the running
process is actually on. For Claude on Linux that is
`<config dir>/sessions/<pid>.json`, matched on the pid and the process's
start time (`conversation_verified: true`). A conversation muxa can verify
this way can be swapped even without a hook-registered id. One that
disagrees with the registry or the command line is reported as an identity
conflict and left alone. The process is checked again right before it is
stopped, together with the pane's agent process and, when it was verified,
the conversation. Where it cannot be read (Codex, macOS), the registry and
then the command line decide, as before, and the outcome says why
(`conversation_unverified_reason`). Not reconciled yet: a pane whose
top-level agent is another provider than the row it is matched to (for
example a Codex with a nested Claude child).

## Choosing the account

| Flag | Account |
| ---- | ------- |
| `--to auto` (default) | The next account other than the agent's current one (configured accounts: in the listed order, skipping the one it is capped on; `aas`: the best-ranked one `aas candidates` marks eligible). |
| `--to personal` | This account (`personal@claude` is accepted too). |
| `--spread` | Round-robin over every eligible account, so a batch does not drain one account at once. |

When no other account has quota left, the agent is skipped and the report says
why each account was passed over. When the provider has no account backend at
all, the swap fails for that agent with nothing done and says how to configure
one; with `resume-after-limit` enabled it is resumed after its reset.

## Options and `[swap]`

```toml
[swap]
bare = "skip"            # skip | wrap: agents started without aas (aas backend only)
continue_text = "continue"
continue_when = "limited" # limited | always | never
continue_evidence = "binding" # binding | verified
aas = "aas"              # optional aas backend; the daemon also looks in /opt/homebrew/bin etc.
```

| Flag | Overrides |
| ---- | --------- |
| `--bare skip\|wrap` | `bare`. `wrap` relaunches a bare `claude …` as `aas e <account> …`. |
| `--continue-text "<text>"` | `continue_text`: the wording only. Who gets it is still `continue_when`. |
| `--continue-when limited\|always\|never` | `continue_when`: which swapped agents get the follow-up. |
| `--no-continue` | Sends nothing (`--continue-when never`). |
| `--continue-evidence binding\|verified` | `continue_evidence`: what the relaunch must show before the follow-up is typed. |
| `--dry-run` | Prints the plan and touches nothing. |
| `--json` | Machine-readable report. |
| `--local` | Runs the swap in this process instead of as a daemon job (see below). |
| `--status <key>` | Shows a daemon swap job by the key it printed (or its job id) and waits for it to end. |
| `--retry <key>` | Retries an ended daemon swap job's safely retryable targets as a new job (see below). |
| `--detach` | Returns as soon as the daemon accepted the job, printing its key, instead of following it. |
| `--cancel <key>` | Asks a daemon swap job to start no further target, then waits for it to end. |

`continue_when = "limited"` sends the follow-up only to agents whose turn was
cut off: by a usage cap, or by the swap itself when `--force` stopped them
mid-turn. An idle or finished agent, and one waiting on your answer or choice,
is only resumed — nothing is typed into it. Naming the wording with
`--continue-text` does not change that; `--continue-when always` does.

`--continue "<text>"` is the deprecated spelling of `--continue-text`. It used
to send the text to every swapped agent, idle ones included; it now keeps the
`continue_when` policy and prints a notice. The MCP tool takes
`continue_text` and `continue_when`, with `continue` kept as the same alias.
A Fleet host running an older `muxa` refuses the new options rather than
reading the old meaning. A blank `--continue-text ""` sends nothing, here and
on a host alike.

A due follow-up is only typed into a relaunch that shows it resumed the right
conversation. With `continue_evidence = "binding"` (the default) the relaunched
agent process's own arguments must resume exactly the planned conversation
(`--resume <id>` for Claude, `resume <id>` for Codex), on one process only.
`"verified"` also needs the conversation to have been read from the stopped
process itself, and the relaunch confirmed on the assigned account. When the
evidence is missing the agent is still relaunched, but nothing is typed into
it: the report says `relaunched; resume binding unconfirmed` or `follow-up
withheld (<reason>)`, and `continue_withheld` in `--json` carries the reason.
The half-typed draft captured before the stop goes back only into a relaunch
that shows the binding, under either setting.

A typed follow-up is reported in two parts, kept apart on purpose. What the
transport said of its prompt and Enter (`delivered`, `refused` — it may still
have arrived — or `unconfirmed`), and what followed within 20 seconds: the
agent's own prompt hook for that conversation on that pane (`prompt_hook`,
read by the daemon), the screen showing activity (`screen_activity`), or
neither (`none_seen`, which is not proof that nothing happened). Neither part
claims the agent did what the text asked. Both are journalled, so `--status`
shows them after a restart, and `follow_up` in `--json` carries them. A swap
run in this process (`--local`, a directly-run pane) has no hooks to read and
reports the screen only.

Each planned follow-up names why the agent is due one — `rate_limited`, then
`cut_off`, then `always`, the first that applies — in the dry-run note and as
`continue_reason` in `--json`. It records eligibility when the plan is made,
not that the prompt was delivered or acted on.

## Daemon jobs

When the daemon runs swaps as jobs, `muxa swap` hands it the request: the
daemon plans it, records every step in its job journal before taking it, and
keeps any other writer (its own automation included) off a pane while the
swap acts on it. The CLI prints the request key first, then follows the job:

```console
$ muxa swap %5 --to work@claude
muxa swap: request cli-3f… (see it with `muxa swap --status cli-3f…`)
```

Ctrl-C only stops waiting; the job runs on, and `--status` shows it. The CLI
never sends the request again or falls back to swapping here once it was
sent: an unanswered start is reported with its key, to check before retrying.

A pane that runs its agent directly (no shell above it) is not respawned by
the daemon yet. The daemon names those panes in its plan, leaves them out of
its job, and the CLI swaps them itself once the job ended cleanly, bound to the
exact agents the daemon planned: a pane that changed in the meantime is
skipped, never swapped as whatever runs there now. Those panes are not in the
job's record, and get no exclusion against the daemon's automation.

`muxa swap --retry <key>` retries a daemon job that has ended. A target is
swapped again only if it failed before anything at or past its relaunch was
even attempted, nothing about it is uncertain, and the very agent the job
planned (same process) still runs on the same pane: it moves to the account
first assigned to it, and no follow-up is typed or draft restored, as those
belonged to the first run. Any other target that did not end swapped, kept
or skipped is only looked at, read-only, and reported as found
(`observed_swapped`, `observed_account_unconfirmed`, `observed_unbound`,
`observed_unknown` when no agent could be found, …); nothing is done to it. Each target is retried by
one retry only (a second one reports `retried_elsewhere`); retry the newest
job to go on from there. `--force` and `--continue-evidence` may be given.

`--detach` starts the job and returns with its key; `--status <key>` reads it
later. A swap with directly-run panes cannot be detached (their half runs in
this process after the job), and neither can one sent to a daemon from before
swap jobs: both are refused before anything is sent.

`--cancel <key>` stops a job at its next target: the target already started
finishes (its stop, relaunch, follow-up), nothing is interrupted or
signalled, and every target not yet started ends as skipped, `cancelled`. A
daemon that is shutting down or halted still takes a cancel. For whoever was
waiting for the whole swap a cancelled job is a failure (not everything was
swapped); for `--cancel` itself a job that ended cleanly is a success.

On a Fleet host the key is minted here and printed before the swap is sent,
so if the answer is lost `muxa swap --host <alias> --status <key>` still finds
it. The swap is never sent again under another key: a host whose `muxa` is too
old for relayed keys fails the request, and the hint to check the key stays.

A daemon from before swap jobs means the whole swap runs here, as before. A
daemon that runs swap jobs but cannot take one right now (starting, halted,
shutting down) refuses instead; `--local` runs the swap here anyway, without
the journal or the exclusion.

The journal keeps every job's record (its request key, plan, each target's
outcome, lineage and retry claims) for good. The step-by-step detail of a job
that ended cleanly (done or abandoned, nothing uncertain, nothing left to
reconcile) is pruned 30 days after it ended, at most once a day: `--status`
then says the detail was pruned, and such a job can no longer be retried,
since how far each target got is no longer known. A job with any uncertain
effect, or one needing reconciliation, keeps all of it. A failed pruning run
is shown by `muxa doctor` and retried the next day. The journal is at schema
v3 from this version on; an older muxa refuses it rather than misread it.

## On a Fleet host

`muxa swap --host <alias>` runs the same swap on a control-mode
`[fleet.hosts]` host: that host's daemon, agents and `[accounts]`, through
the Fleet work-command transport, with the report printed here. Agents use
the same thing through the `muxa_swap_account` MCP tool (`host` omitted for
this host). The host's own `muxa` must be new enough to know `swap`.

The swap executes on the remote host through Fleet's work-command transport.
Ctrl-C here only stops waiting for its report; it does not reliably cancel
the swap there, which may still be stopping and relaunching agents. Check
the host before running it again.

```console
$ muxa swap --host build --to work --dry-run
```

## Automatic swap

The same swap is an automation action. This rule moves any capped agent to the
next account as soon as the cap is seen, instead of waiting for its reset:

```toml
[[automation.rule]]
name = "swap-on-limit"
on = "rate_limited"
action = "swap_account"
# account = "personal"       # default: auto
# text = "continue"          # default: [swap] continue_text / continue_when
```

A `swap_account` rule fires right away by default (`wait = "0s"`); set `wait`
to delay it. The usual guards apply: one firing per cap episode, the per-pane
hourly cap, and a fire-time re-check that the agent is still capped. A new
account that is also capped is a new episode, so the next swap excludes it;
`max_per_hour` bounds how many times one pane can be moved.

The engine never waits on a swap. It admits one (at most two at a time, one
per pane; a firing with no room is checked again a few seconds later),
records it as `admitted` in the automation log, and that admission is what
the cooldown and hourly caps count, at once and once. The swap then takes
the pane, checks the firing is still due, and runs as a job; its end is
recorded when the job ends, and it is `fired` only if the job ended `done`
with its agent swapped and nothing uncertain. A job still running is waited
on, never taken as failed. A daemon that restarts follows every admitted
swap it finds unfinished to its job's end; it never starts one again.

## New agents

`muxa swap` moves agents that are already running. To start new ones on a
chosen account, set `[agent.<program>].account = "auto"`
(see [Configuration](CONFIGURATION.md#starting-on-an-account)): every muxa
launch then starts on the first configured account's environment, or, with
the `aas` backend, on `aas e <best eligible account>` instead of the bare
CLI. Only the `aas` backend skips a capped account at launch; configured
accounts always start on the first one listed. With no backend it starts the bare CLI and
says why.

## Configuration compatibility

Account routing adds no configuration keys: existing `[agent.*].account`,
`[[accounts.*]]` and `[swap]` settings keep their meaning, and a config that
uses them loads in older muxa builds unchanged. `aas` users need to change
nothing; it is still used for any provider that lists no accounts.

## Before the agent is stopped

Right before a target is touched (before its pane is even kept open), the
swap checks what its relaunch will need and journals the result, and refuses
the target with nothing done (`preflight_failed: …`, retryable once fixed) if
any of it is not there:

- the directory the relaunch runs in: the one the pane's shell is actually in
  (a removed one fails, even if another now has its name), or for a pane that
  runs its agent directly the directory its respawn is given;
- a configured account's `env_file`: a regular file, readable, and unchanged
  since the relaunch line was built for this target, just before the checks
  (a change made while the swap waited in a queue is not caught); only its
  digest is recorded, never its contents;
- for a directly-run pane, the shell it is respawned through, which is the
  very one then used;
- any program the line names by an absolute path, and the interpreter of a
  script among them.

A program found through the pane's own `PATH` (`aas`, a bare `claude`) is
resolved by that shell, in a context muxa does not reproduce: it is recorded
as `unknown`, not certified, and the swap goes on as before. Passing these
checks is not a promise the launch works.

## Limits

- A pane started *with* the agent (`new-window 'aas e …'`, `exec claude …`)
  has no shell to type a relaunch into. On tmux and rmux it is kept open
  past the exit (`remain-on-exit`) and respawned in place with the relaunch,
  keeping its pane id; other multiplexers skip it.
- An agent that did something in the last 20 seconds counts as mid-turn even
  when its state reads idle (a long tool call shows no transition), so it is
  skipped without `--force`. A capped agent is exempt: its last activity is
  the cap.
- What was half-typed into the agent's prompt is typed back after the
  resume (after the follow-up, so the two are not sent as one prompt).
- After the follow-up, muxa waits up to 20 seconds for the agent to show a
  turn in progress and says in the report whether it did.
- muxa needs the provider session id. A hook-fed row has it; a
  screen-detected row is resumed only when the agent's own command line names
  it (`--resume <id>` / `resume <id>`).
- On macOS, `ps` reports a command line joined, so a flag value containing a
  space (`--append-system-prompt "be brief"`) is relaunched split.
- Codex keeps its thread index per account (`CODEX_HOME`), but `codex resume <id>`
  finds the thread's rollout in the shared `sessions/`, so it resumes under an
  account that never ran it. If the pane's shell is no longer in the thread's
  directory, Codex first asks which directory to use; muxa keeps the
  session's directory.
