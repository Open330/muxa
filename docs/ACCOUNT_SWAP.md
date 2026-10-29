# Account swap

When one account's usage cap is spent, the conversation is still fine. As
long as the accounts share `projects/` (Claude) and `sessions/` (Codex), the
same provider session id resumes under any account. `muxa swap` moves agents
onto another account and resumes each one in its own pane.

Accounts come from one of two places, per provider:

- **`[[accounts.<provider>]]` in `config.toml`** (no external tool). Each
  account is a name and the environment that selects it. muxa launches with
  that environment and recognises a running agent's account from its own
  environment, however it was started.
- **`aas`** (Agent Account Switcher), used for a provider with no
  `[[accounts]]` entries: candidates from `aas candidates --json`, launches
  through `aas e <account>`.

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
to recognise the account. `auto` tries accounts in the listed order and skips
one an agent here was capped on. `[accounts]` is host-local: config sync
never copies it.

Once a provider lists accounts, its swaps use only those: an `aas` name is
not one of them, and `--bare` no longer applies, since an agent started bare
is relaunched with the account's environment like any other. An `env_file`
must be an absolute or `~/` path.

```console
$ muxa swap --dry-run
planned  %1         claude_code e-ed@claude -> jelly@claude
         $ aas e jelly@claude --dangerously-skip-permissions --resume 090b45a6-…
         then send "continue"
```

For each agent, muxa:

1. reads how the agent was started from its pane's process tree (`aas e
   <account> …`, or a bare `claude` / `codex`);
2. picks the next account (`aas candidates --json`, or `--to <account>`). A
   bare agent counts as being on the account `aas` marks active, so `auto`
   never "moves" it onto the account it is already spending;
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
   the startup dialogs a new account brings up: Claude's folder trust and
   auto-mode tour, Codex's folder trust, "Working directory · resume" (the
   session's directory), "Resume goal", and "open in another app" (retried a
   few times while the old process lets go). Any other dialog is reported and
   nothing is typed into it;
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
| `--to auto` (default) | The best-ranked account `aas candidates` marks eligible, other than the agent's current one. |
| `--to jelly@claude` | This account. |
| `--spread` | Round-robin over every eligible account, so a batch does not drain one account at once. |

When no other account has quota left, the agent is skipped and the report says
why each account was passed over.

## Options and `[swap]`

```toml
[swap]
bare = "skip"            # skip | wrap: agents started without aas
continue_text = "continue"
continue_when = "limited" # limited | always | never
aas = "aas"              # the aas binary; the daemon also looks in /opt/homebrew/bin etc.
```

| Flag | Overrides |
| ---- | --------- |
| `--bare skip\|wrap` | `bare`. `wrap` relaunches a bare `claude …` as `aas e <account> …`. |
| `--continue-text "<text>"` | `continue_text`: the wording only. Who gets it is still `continue_when`. |
| `--continue-when limited\|always\|never` | `continue_when`: which swapped agents get the follow-up. |
| `--no-continue` | Sends nothing (`--continue-when never`). |
| `--dry-run` | Prints the plan and touches nothing. |
| `--json` | Machine-readable report. |

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

Each planned follow-up names why the agent is due one — `rate_limited`, then
`cut_off`, then `always`, the first that applies — in the dry-run note and as
`continue_reason` in `--json`. It records eligibility when the plan is made,
not that the prompt was delivered or acted on.

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
$ muxa swap --host rtzr --to work --dry-run
```

## Automatic swap

The same swap is an automation action. This rule moves any capped agent to the
next account as soon as the cap is seen, instead of waiting for its reset:

```toml
[[automation.rule]]
name = "swap-on-limit"
on = "rate_limited"
action = "swap_account"
# account = "jelly@claude"   # default: auto
# text = "continue"          # default: [swap] continue_text / continue_when
```

A `swap_account` rule fires right away by default (`wait = "0s"`); set `wait`
to delay it. The usual guards apply: one firing per cap episode, the per-pane
hourly cap, and a fire-time re-check that the agent is still capped. A new
account that is also capped is a new episode, so the next swap excludes it;
`max_per_hour` bounds how many times one pane can be moved.

## New agents

`muxa swap` moves agents that are already running. To keep new ones off a
capped account in the first place, set `[agent.<program>].account = "auto"`
(see [Configuration](CONFIGURATION.md#starting-on-an-aas-account)): every muxa
launch then starts `aas e <best eligible account> …` instead of the bare CLI.

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
- Codex keeps its thread index per `aas` profile, but `codex resume <id>`
  finds the thread's rollout in the shared `sessions/`, so it resumes under an
  account that never ran it. If the pane's shell is no longer in the thread's
  directory, Codex first asks which directory to use; muxa keeps the
  session's directory.
