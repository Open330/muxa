# Account swap

When one account's usage cap is spent, the conversation is still fine. With
`aas` (Agent Account Switcher) profiles sharing `projects/` (Claude)
and `sessions/` (Codex), the same provider session id resumes under any
account. `muxa swap` moves agents onto another `aas` account and resumes each
one in its own pane.

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
| `--continue "<text>"` | Sends this text to every swapped agent. |
| `--no-continue` | Sends nothing. |
| `--dry-run` | Prints the plan and touches nothing. |
| `--json` | Machine-readable report. |

`continue_when = "limited"` sends the follow-up only to agents whose turn was
cut off: by a usage cap, or by the swap itself when `--force` stopped them
mid-turn. An idle agent moved with `--all` is only resumed.

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

- The agent must run under a shell in its pane. A pane started *with* the
  agent (`new-window 'aas e …'`) would close when the agent stops, so it is
  skipped.
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
