# Configuration

`tendd` reads `$XDG_CONFIG_HOME/tend/config.toml` when present. Start from
`config.example.toml` for a full annotated file.

## Socket

```toml
socket = "/tmp/tend.sock"
```

The CLI also honors `TEND_SOCKET`. tmux environments are healed on daemon
startup so existing panes can reach the current socket.

## History

```toml
[history]
enabled = true
path = "$XDG_DATA_HOME/tend/prompts.ndjson"
max_per_pane = 50
max_age_days = 30
```

Prompt history is retained, not an unbounded warehouse. `tend recap` and
prompt totals in `tend stats` use this retained window.

## Activity

```toml
[activity]
enabled = true
path = "$XDG_DATA_HOME/tend/activity.ndjson"
max_age_days = 30
```

The activity ledger stores agent state intervals, tmux foreground
intervals, and tend human interaction intervals. See
[ACTIVITY.md](ACTIVITY.md).

## Session Activity

```toml
[session_activity]
enabled = true
path = "$XDG_DATA_HOME/tend/session-activity.json"
interval_secs = 5
```

This tmux foreground sampler remains as a compatibility source and helps
stats while newer activity ledger intervals are accumulating.

## Storage

```toml
[storage]
audit_max_bytes = 8388608 # 8 MiB; 0 never rotates
backups_keep = 5          # 0 keeps every backup
```

`audit_max_bytes` rotates `collaboration-audit.ndjson` to
`collaboration-audit.ndjson.1` when the live ledger reaches that size; one
previous generation is kept, so the ledger stays under about twice the cap.
`backups_keep` keeps only the newest entries (by modification time) in
`$XDG_DATA_HOME/tend/backups`, where install and upgrade backups go; older
entries are deleted at daemon startup and hourly. Hidden entries are left
alone and symlinks are removed without being followed.

tendd also expires its own launch-provenance archives under
`$XDG_DATA_HOME/tend/spawns`: completed or failed launches after 30 days, and
launches that never completed after 90 days, unless a live agent or an open
worker ticket still refers to them.

## Snapshots

```toml
[snapshot]
auto_interval_minutes = 15   # 0 turns automatic snapshots off
keep_auto = 10               # newest automatic snapshots kept
```

tendd takes an automatic workspace snapshot (`tend snapshot --auto`) every
`auto_interval_minutes`, starting one interval after it starts. Nothing is
written when the workspace has not changed since the newest snapshot of the
same server — the comparison covers sessions, windows, layouts, pane
directories, commands and agent conversation ids, not timestamps or which
window has focus. Only the newest `keep_auto` automatic snapshots are kept;
snapshots taken with `tend snapshot`, Save Snapshot in Tend.app, or `tend
reload` are never pruned. When tend's agents span several multiplexer servers
the automatic snapshot is skipped with a warning in tendd's log.

Snapshots live in `$XDG_DATA_HOME/tend/snapshots/<id>/snapshot.json`
(`~/Library/Application Support/tend/snapshots` on macOS). `tend snapshot
--list` shows them, `tend snapshot --delete <id>` removes one, and `tend
restore --only-missing` recreates only the sessions the server does not
already have. `tend restore` with no argument uses the newest snapshot,
automatic or manual. Its dry run also lists the sessions and windows that are
new since the snapshot, which a restore never closes; with `--json` each
recorded session, window and pane carries a `state` of `missing` or
`present`.

## MCP orchestration guide

`tend init` can install a canonical collaboration skill and global agent entry
points beside this config. Keep user preferences here; see
[Global agent integration](AGENT_INTEGRATION.md) for the bundle and host adapters.

```toml
[mcp.guide]
placement = "window" # pane (default) | window | session
agent = "codex"      # claude | codex | agy | opencode
options = ["--model", "preferred-model"]
direction = "right"  # right (default) | down
instructions = "Keep one task per window; use a new session for unrelated projects."
```

This section records how the user normally organizes agent work. Tend includes
it in the MCP initialization instructions and exposes it through `tend_guide`,
so a connected agent does not have to guess whether a new task belongs in a
pane, window, or separate session. `tend_start_agent` also uses these values
when the matching arguments are omitted. If `agent` is not configured, the
tool still requires the caller to choose one.

`options` here is the older, single-provider spelling of what
[`[agent.<program>]`](#agent-launch-arguments) now expresses per provider. It
still applies, but only to the agent `mcp.guide.agent` names. Managed Work
follows its fixed Workspace=session/Run=window/Agent=pane layout, and
collaboration peer spawning still uses a pane in the current window. Restart an
MCP-connected agent after changing this section because its stdio MCP process
loads config at startup.

## Agent launch arguments

```toml
[agent.claude]
options = ["--model", "claude-opus-5"]

[agent.codex]
options = ["--model", "gpt-5-codex", "--search"]
```

Which model an agent runs is not a property of the surface it lands on, so this
is keyed by the provider rather than attached to any one launcher. Every path
that starts an agent resolves here — `tend agent start`, `tend work start`, a
`tend work up` pipeline, `tend_start_agent`, and the automatic peer spawn behind
`tend_call_peer` — so a model configured once applies to all of them.

Keying by provider is also what makes it safe: `--model` names different things
to different CLIs, and a single shared list either fits one provider or has to
be guarded at every launch site to keep one CLI's model name away from another.
An unrecognised key (`[agent.claud]`) fails the load rather than sitting inert.

Arguments land after Tend's built-in provider profile — `codex` already gets
`--yolo`, `claude` `--dangerously-skip-permissions` — and before any initial
prompt. Each value is shell-quoted independently.

Anything named explicitly **replaces** the configured list rather than adding to
it, so an override never puts `--model` on the command line twice:

| Named by | Wins over |
| --- | --- |
| `tend agent start --option`, `tend work start --option` | `[agent.<program>]` |
| An MCP `options` array | `[agent.<program>]` |
| A pipeline agent's own `options` | `[agent.<program>]` |

`options = []` on a provider means "launch it bare", which is how an operator
turns an inherited `[mcp.guide].options` off.

A pipeline pins arguments per pane, because panes in one line-up are not doing
the same job:

```toml
[[pipeline.triad.agent]]
alias   = 'review'
program = 'claude'
role    = 'reviewer'
options = ['--model', 'claude-sonnet-5']
```

`tend work up --dry-run` prints the resolved list for every pane, so what it
shows is what the launch will use.

### Starting on an account

```toml
[agent.claude]
account = "auto"          # or a pinned account: "work"
```

Without it, a bare `claude` starts on whichever account the CLI is logged
into, even while that one is capped. `account` makes every launch path above
start on a chosen account instead. `auto` lets the account provider choose;
a name pins one of that provider's accounts, used even when capped. `claude` and `codex` only.

The accounts come from an account provider (see [Account
swap](ACCOUNT_SWAP.md)):

- `[[accounts.claude]]` / `[[accounts.codex]]` entries, with no external
  tool: the launch is `env K=V … claude …`, and `auto` takes the first
  listed account (ranking configured accounts by observed usage is coming in
  a follow-up);
- otherwise [`aas`](ACCOUNT_SWAP.md), when it is installed: the launch is
  `aas e <account> …`, `auto` takes the best-ranked eligible account from
  `aas candidates` (it never changes `aas active`). The binary is
  `[swap].aas`; it is run, never linked.

When there is no provider, `aas` takes longer than 5 seconds, or no account
has quota left, the agent starts bare as before, which may still be the
capped account. The reason goes to stderr and to the `account` field of
`tend agent start --json`. An agent already capped mid-run is
[`tend swap`](ACCOUNT_SWAP.md)'s job, and with no accounts at all the
`resume-after-limit` rule, once enabled, resumes it after its reset.

### Advanced launch options form

In Tend.app, **Settings → Advanced** has a segmented **Launch options / Raw
TOML** editor. The form edits provider defaults under `[agent.<program>]`
and options for **existing** pipeline agent panes. Create or restructure
pipelines in Raw TOML; the form does not edit the CLI runtime `--option`
flags or change running agents.

Each provider or pane has three distinct states:

| Form state | Saved configuration | Effect |
| ---------- | ------------------- | ------ |
| Inherit (override off) | No `options` key | Use the next applicable default. |
| Override with empty list | `options = []` | Explicitly suppress inherited extra arguments. |
| Override with argument rows | `options = ["--model", "preferred-model"]` | Replace the inherited list with these tokens in order. |

One row is **one literal argument**. There is no shell splitting: enter
`--model` and its value on separate rows, and enter a value containing spaces
in a single row without adding shell quotes. An empty string row (`[""]`)
passes one empty argument; it is not the same as an empty list (`[]`).
An empty list suppresses extra options, not Tend's built-in provider profile.

Replacement precedence is **explicit CLI options → pipeline pane options →
provider defaults → legacy `[mcp.guide].options` → no extra options**. Only
applicable layers participate; the legacy fallback applies only to the
program named by `[mcp.guide].agent`. An explicit MCP `options` array likewise
overrides configured defaults. Lists replace, never append, even when empty.
The form previews effective configured options; a later explicit launch
override can still replace them.

The form uses optimistic concurrency: a save includes the configuration text
it was based on, and a changed file causes a conflict rather than an overwrite.
Reload and review the current file before reapplying launch edits. Form saves
change only the requested option fields and preserve unrelated TOML, comments,
and legacy guide settings. Legacy settings remain visible as a fallback and
can be edited in Raw TOML. Unsaved raw and form edits are kept from overwriting
each other; reload asks before discarding unsaved changes.

Saved options apply to **new launches**, not existing sessions. Launchers
that read the file afresh see the new configuration; restart `tendd` for
daemon-held launch defaults and reconnect/restart MCP processes that loaded
their defaults at startup. Reloading the editor only refreshes its view of
the file. The form requires daemon capability `config_launch_v1`; use Raw
TOML or update the daemon if it is unavailable.

## Ask

Codex and Claude turns publish user-facing messages while they run. Watch and
the macOS app show these as provisional output; only process exit and the final
provider result settle the turn. Tool payloads and reasoning are excluded.
The CLI waits on change notifications (with polling for older/remote daemons),
prints progress only to an interactive stderr, and keeps stdout/`--json` final-only.
CLI waits fetch just their own turn; watch subscribes to Ask changes separately
from agent hooks and fetches the selected conversation. Legacy clients may still
request full history. The macOS app retains its full history projection for
cross-conversation activity badges.
Progress is bounded and coalesced, without a disk write per message. Other
providers retain their final-response behavior.

New CLI conversations receive a short preparation guide: read relevant project
instructions and requested skills once, discover tools narrowly, and bound tool
output. This preserves the provider's configured tools and permissions. Set
`cwd` to the usual project when appropriate to avoid searching from `$HOME`.

```toml
[ask]
enabled = true
agent = "claude"     # any provider id: a built-in or one you added below
cwd = "~"            # where the headless process runs; defaults to $HOME
permission_mode = "bypass" # bypass (default) | edit | plan | default
additional_dirs = [] # extra real paths, e.g. ["/nfs/home/june"]
timeout_secs = 1800    # 30-minute wall-clock limit
keep = 200           # answers retained before the oldest are dropped

[ask.providers.anthropic]           # tune a built-in: the id is the engine
model = "claude-opus-5"             # defaults: claude-sonnet-5 (anthropic), gpt-5 (openai); CLIs use their own
api_key_env = "WORK_ANTHROPIC_KEY"  # the NAME of a variable holding the key — never the key itself

[ask.providers.anthropic-work]      # or add your own, under any id you like
engine = "anthropic"                # required for an id that is not built in
title = "Anthropic (work)"          # optional; defaults to a humanized id
model = "claude-opus-5"             # optional
api_key_env = "WORK_ANTHROPIC_KEY"  # optional; this instance's own key
executable = "/opt/homebrew/bin/claude"  # optional; CLI engines only

[ask.providers.apple-cloud]         # Apple's models (macOS 26+): no key at all
engine = "apple"
model = "private-cloud"             # on-device (default) | private-cloud (macOS 27+)
```

Opt-in headless questions from `tend watch`: `a` composes one, `A` browses
the answers. tendd runs the agent in print mode and captures the reply, so
there is no session to manage and completion is an exit code rather than a
guess. Each provider can keep several resumable conversations; `Ctrl-E` in
the composer marks a draft as new, while `n` in the panel opens a new draft.
The conversation and its first turn are created together on send. `path` defaults to
`$XDG_DATA_HOME/tend/ask.json` and holds both the history and conversation
ids. Off by default because enabling it lets the daemon spawn a CLI
that bills your account. See [WATCH.md](WATCH.md).

`permission_mode = "bypass"` is the default because the headless agent cannot
answer approval prompts. It is intended for unattended workflows such as a
full issue resolver and disables approvals/sandboxing; use ask only for prompts
and directories you trust. `edit` enables workspace edits while retaining the
agent's sandbox or automated review layer, and `default` preserves the agent
CLI's normal permissions. `additional_dirs` is also passed to the agent CLI.
Add the resolved target when files under `cwd` are symlinks outside it—for
example `["/nfs/home/june"]` when `/home/june/workspace` points there.
`timeout_secs` defaults to 30 minutes so skills have time to prepare a
persistent worker. Reaching it terminates the headless agent process; it is a
wall-clock safety limit, not an inactivity detector. `plan` is a read-only
mode (claude `--permission-mode plan`, codex `--sandbox read-only`, gemini
`--approval-mode plan`); `tend work compose` always drafts under it.

### Providers

Automation's optional `[automation.rule.ask_condition]` is a separate,
strictly tool-free API judgment path: it requires Ask enabled and a configured
key available to the daemon, accepts only OpenAI/Anthropic API providers, and
does **not** replay Ask conversation history. Observe-only is the default,
but judgments still disclose bounded pane context externally and may be
billed. Its timeout and attempt budget are separate from ordinary Ask turns
and automation actions. See [Optional Ask condition](AUTOMATION.md#optional-ask-condition)
for the schema, limits, privacy boundary, and free versus paid tests.

An **engine** is the code that drives a provider: the argv a CLI takes, the
JSON an API answers with, the environment variable its key lives in. There
are six and they are fixed — `claude`, `codex`, and `gemini` drive the
agent CLIs in print mode (`claude -p`, `codex exec --json`, `gemini -p
--output-format json`) and resume their own sessions between questions;
`anthropic` and `openai` call the Messages and Chat Completions APIs
directly over HTTPS; `apple` runs the Mac's own Foundation Models. API
engines have no session to resume, so tendd replays the conversation's
earlier turns from its own history — the most recent 40 turns or 60k
characters — ahead of each question; `cwd`, `additional_dirs`, and
`permission_mode` do not apply to them.

`apple` (macOS 26 or later, with Apple Intelligence turned on) takes no key
and sends nothing to a vendor account. `model` picks between the two system
models: `on-device`, the default, which never leaves the Mac and has an
8,192-token context; and `private-cloud`, Apple's Private Cloud Compute
(macOS 27 or later, 32,768 tokens, subject to Apple's usage quota). As of
this writing `private-cloud` does not work: it needs a helper built with the
macOS 27 SDK, which no GitHub runner image offers yet, and even such a build,
signed with a Developer ID, was refused by the system with
`ModelManagerError 1046` — whatever Private Cloud Compute requires of a
process, a command-line helper does not have it. The option stays so a
future build can pick it up; a turn on it says why it cannot answer. The
framework is Swift-only, so tendd reaches it through `tend-afm`, a small
helper that ships in `Tend.app/Contents/Helpers`. tendd looks for it beside
its own executable, then in `/Applications/Tend.app`, then in
`~/Applications/Tend.app`, then on `PATH`;
`executable` names another copy. Like the APIs it keeps no session, so the
conversation is replayed — cut to 6k characters to fit the on-device
window, and cut further by the helper if the model still reports an
overflow. `cwd`, `additional_dirs`, and `permission_mode` do not apply:
these models answer questions and touch no files.

The agent CLIs can look at your workspace because they have a shell; a bare
model cannot, and would answer that it has no access to other applications.
So on a Global Ask turn the helper gives the model two read-only tools,
answered by `tend status --json` and `tend recap` against the daemon the
question came from: `list_agent_sessions` (every tracked agent — pane, state,
title, directory, latest prompt and reply) and `read_agent_session` (one pane
in detail). It can read; it cannot send input to an agent or change anything.
The model decides when to call them, so nothing is read for a question that
is not about your agents. With `on-device` what it reads never leaves the
Mac; with `private-cloud` it is sent to Apple's Private Cloud Compute along
with the question. One-shot turns such as `tend work init` get no tools. The
built-in `apple` provider is listed on every host, so one config.toml can
serve a Mac and a Linux box; a turn on a host that cannot run it fails with a
plain "runs only on macOS".

A **provider** is an `[ask.providers.<id>]` table you compose. The id is
yours to pick (a TOML bare key: letters, digits, `-`, `_`), and `engine`
says which of the six drives it. Several providers may share one engine —
a work and a personal OpenAI account, two Anthropic keys, a second `claude`
binary — and each keeps its own conversation, so switching between them
resumes rather than restarts. `[ask] agent` and `tend ask --agent` name a
provider id, not an engine.

The engine ids are also providers of themselves, so a fresh install
works with no `[ask.providers]` at all: `tend ask providers` lists what you
composed first, then every built-in no id of yours has taken over. A table
named after a built-in and *without* `engine` tunes that built-in — which
is why an `[ask.providers.anthropic] model = "…"` written before any of
this still means what it always did. Removing such a table clears the
settings; the built-in itself stays.

An API key comes from, in order: the one-turn key a client sends, the
engine's own variable in tendd's environment (`ANTHROPIC_API_KEY`,
`OPENAI_API_KEY`; `CODEX_API_KEY` and `GEMINI_API_KEY` for the CLIs), then
the variable that provider's `api_key_env` names. The key itself is never
written to config.toml. Two providers on one engine share the engine's
variable, so give each its own `api_key_env` (or send a one-turn key) when
they must not use the same account. `model` overrides the model for any
provider, CLIs included, and `executable` points a CLI engine at a
particular binary.

The same daemon-owned history is available without opening the TUI:

```bash
tend ask --agent codex "summarize the current implementation"
tend ask --agent claude --detach --json "review the deployment plan"
tend ask --agent anthropic "which files does the reaper touch?"
security find-generic-password -w -s my-codex-key \
  | tend ask --agent codex --api-key-stdin "review this repository"
tend ask --agent anthropic-work "…"   # a provider you added, by its id (until the next restart)
tend ask provider use codex           # make it the default; rewrites [ask] agent
tend ask providers [--json]           # every provider: engine, kind, model, whether a key resolves, selected
tend ask provider add anthropic-work --engine anthropic \
  --title "Anthropic (work)" --api-key-env WORK_ANTHROPIC_KEY
tend ask provider add claude-work --engine claude --executable /opt/homebrew/bin/claude
tend ask provider set anthropic --model claude-opus-5 --api-key-env WORK_ANTHROPIC_KEY
tend ask provider set anthropic --clear-model         # flags you omit leave that key unchanged
tend ask provider remove anthropic-work               # a built-in id only loses its settings
```

`add` refuses an id that already exists, an id that is not a TOML bare key,
an engine that is not one of the six, and a built-in id asked to run
someone else's engine. The engine is fixed once added — `set` changes the
title, model, key variable, and binary, and to change the engine you remove
the provider and add it again, because a live conversation cannot resume a
`claude` session id against `codex`. Removing the provider `[ask] agent`
points at hands the selection to the first one left.

`--api-key-stdin` refuses an interactive terminal, crosses only the owner-only
Unix socket, and supplies the key to one matching provider child. It is never
stored in tend config/history or placed in argv. Without it, the headless CLI
uses its existing login or provider environment as before.

## Collaboration

```toml
[collaboration]
enabled = true
auto_spawn_workers = true
worker_bypass_permissions = false
wake = "idle_only" # idle_only | never
wake_payload = "operator_full" # notice | operator_full | full
scope = "window"   # window | host
max_message_bytes = 16384
# path = "$XDG_DATA_HOME/tend/collaboration.json"
retention_days = 90 # default; 0 retains indefinitely
```

`auto_spawn_workers` lets high-level peer calls reuse a worker or create one
when missing. `worker_bypass_permissions` defaults to false: worker launches
inherit provider permissions/options and do not add bypass flags. Enable it
explicitly to use Tend's bypass profile for automatic workers. These settings
do not change manual launches or pipeline launch profiles.

Opt-in durable request/reply between agents in the same stable tmux window.
The historical optional `path` default is
`$XDG_DATA_HOME/tend/collaboration.json`; tend imports that JSON once into the
authoritative sibling `collaboration.sqlite3` and retains the JSON as a
migration backup. A configured `.sqlite`, `.sqlite3`, or `.db` path is used
directly. The database stores mailbox state and exact-session aliases/roles.
`retention_days` (default 90) prunes eligible fully delivered terminal threads
at daemon startup and hourly afterwards; `0` retains all history. The retained JSON backup is a duplicate
copy of bodies and is not pruned.
`idle_only` injects only at a hook-authoritative top-level Idle prompt.
The default `wake_payload = "operator_full"` directly delivers requests sent
from operator surfaces such as watch and dashboard, while agent-originated MCP
and CLI requests remain mailbox notices. `notice` keeps every body in the
mailbox; `full` atomically claims and directly delivers every request. Reply
wakes remain body-free notifications in every mode.
`scope = "host"` lets watch address the selected tracked agent in another
tmux window or session by its exact pane id.
See [COLLABORATION.md](COLLABORATION.md).

Per-message `notify` is independent of `wake_payload`: one-way `kind=notice`
messages are quiet by default. Use MCP `notify=true` or CLI `--notify true` for
an actionable handoff; `notify=false` is pull-only. These overrides still respect
`collaboration.wake=never` and idle-only delivery. See
[notification discipline](COLLABORATION.md#quiet-progress-and-actionable-notifications).

## Message skills

Reusable prompt templates live in a regular TOML table. They are shared by the
watch and dashboard `m` composers, the watch `a` composer, and MCP
`tend_call_peer` requests:

```toml
[message.skills]
agent-review = "create a new pane with codex (use the cx alias), then pass our changes to it for review"
```

Manage the table without hand-editing it:

```bash
tend skill add agent-review 'create a new pane with codex (use the cx alias), then pass our changes to it for review'
tend skill list
tend skill show agent-review
tend skill remove agent-review
```

At any point in a draft, press `/`, type to filter, move with arrow keys or
`Tab`, then press `Enter`. Selection inserts the prompt at the current cursor
without replacing existing text; inspect or edit it and press `Enter` again to
send. Multiple skills can be combined in one draft. In `tend watch`, `F2` opens
an add/update form inside the palette and `Delete` removes the selected skill
after confirmation. `Ctrl-A` and `Ctrl-D` remain compatibility aliases.
Pass `-` as the CLI add prompt to read a multi-line template from stdin.

In an MCP-connected agent conversation, `/name` selects the same template for
`tend_call_peer`; its optional body and context are appended without changing
the template. The MCP process loads the skill table at startup, so restart an
already-running agent after adding, updating, or removing a skill.

A skill stores prompt text only. It has no request kind, collaboration mode,
agent, cwd, timeout, or permission scope of its own. The `m` composer keeps its
currently selected kind and mode; the `a` composer keeps the daemon's `[ask]`
agent and execution settings, including `permission_mode`. Inserting a skill
therefore cannot broaden either contract.

## Watch

```toml
[watch]
view = "window"
columns = ["pane", "state_age", "model", "ctx", "cost", "prompt", "activity"]
sort = ["state", "name", "latest"]
hide_paneless = true
collaboration_kind = "question"   # question | review | task | notice
collaboration_mode = "read_only"  # read_only | execute | just_send
collab_layout = "table"            # table | sequence

[watch.widths]
prompt = "min:20"
workload = 8
activity = 6

[watch.detail]
enabled = true
template = "{last_response || last_prompt || last_notification}"
```

Watch rewrites `collaboration_kind` and `collaboration_mode` when `Tab` or
`Ctrl-E` changes the `m` composer badges. The last selection therefore
survives both closing the composer and restarting watch.
`collab_layout` controls only the collaboration-history screen; it is
independent from the topology `layout` and can be toggled with `v`.

See [WATCH.md](WATCH.md) for TUI behavior, columns, sort, and keybindings.

## UI

```toml
[ui]
theme = "classic"
icons = "unicode"
```

Shared visual defaults for human-facing terminal output (`status`,
`status-line`, `attend`, and `watch`).

- `theme` — visual preset: `classic`, `oh-my-tend`, `focus`, `ops`, or a
  monochrome preset. A one-shot `--theme` flag overrides it per run.
- `icons` — agent-state glyph set:
  - `unicode` (default) — Geometric Shapes glyphs (`●` working, `▶` input,
    `◆` choice, `■` error, `○` idle, `◌` starting, `×` stopped).
  - `ascii` — single-character fallbacks (`*` working, `>` input, `?`
    choice, `!` error, `o` idle, `~` starting, `x` stopped) for terminals
    whose font lacks the Unicode glyphs or substitutes a mismatched-size
    fallback font for them.

## Discovery

```toml
[discovery]
enabled = true
interval_secs = 30
```

Discovery scans tmux panes for known agent CLIs (`claude` / `codex` /
`agy`) and backfills the registry without waiting for a hook to fire. It
runs once at daemon startup and then every `interval_secs`, so a fresh agent
session in a new tmux session shows up in `tend status` within that window
instead of only after its first hook. Set `interval_secs = 0` to keep the
legacy run-once-at-startup behavior; `enabled = false` turns discovery off
entirely. The rescan reuses the same `tmux list-panes` the reconciler
already runs, so the cost is negligible.

## Daemon

```toml
[daemon]
restart_on_new_binary = true
binary_poll_secs = 30
```

Installing a new tend does not restart the daemon on its own. The package
manager writes the new build and repoints whatever is on `PATH`, while the
running process keeps its open inode and serves the old logic — and no service
manager intervenes, because `KeepAlive` and `Restart=always` react to a process
*exiting*, which nothing did. Left alone, the daemon can serve a build that is
weeks old, answering `protocol mismatch` to every CLI call once the wire format
moves on.

So tendd watches the binary it would re-exec through and, when that path
resolves to a different file for two consecutive polls, re-execs onto it. The
confirmation poll is what keeps a half-finished install from being adopted. The
daemon replaces itself in place (same pid), so this behaves identically under
launchd, systemd, and a bare terminal, and a failed re-exec leaves the old image
running rather than nothing at all.

Set `restart_on_new_binary = false` when something else owns the upgrade
sequence — say a deploy that installs several binaries and restarts them in a
particular order. `tend daemon restart` then does it on demand, and `tend
doctor` reports a daemon whose version has drifted from the CLI's.

## Reconciler

Native PTY references are checked against tendd's own session registry at
startup and on each reconciler pass. A missing session clears the reference
and stops the agent row; an exited session stops it while retaining its output
surface for capture. Failed session observations leave rows unchanged. A
surviving pid-tracked task remains governed by process liveness even if its old
PTY was lost on daemon restart. `tend registry prune` also checks missing PTY references,
including when periodic reconciliation is disabled, and retains its age filter.

```toml
[reconciler]
enabled = true
interval_secs = 30
stuck_working_timeout_secs = 0
stuck_waiting_timeout_secs = 0
codex_rollout_enabled = true
paneless_stale_timeout_secs = 86400
paneless_max_live_rows = 256
```

The reconciler keeps stale states from staying misleading forever. Timeout
values of `0` disable that timeout. The same loop also runs the pid-liveness
sweep that flips registered background tasks (see `tend proc register`) to
`stopped` once their process exits.

Codex rollout polling runs every two seconds independently of the full
`interval_secs` reconciliation. It uses original event timestamps for activity
and recovers running/completed turns and actual errors. Used percentages and
credit balances alone do not change state. Set `codex_rollout_enabled = false`
to disable it. `paneless_stale_timeout_secs` stops fully orphaned registry rows
after 24 hours by default; `0` disables that cleanup. `paneless_max_live_rows`
keeps at most that many live orphan rows at once (the least recently active
beyond it are stopped), so a script running agent CLIs headlessly cannot pile
thousands up within that day; stopped orphan rows are removed after five
minutes. `0` disables the cap.

## Sharing settings between hosts

```sh
tend config export -o tend-settings.toml      # on the host you set up
tend config diff  tend-settings.toml          # on another host: preview
tend config apply tend-settings.toml          # then apply
```

The bundle is a plain TOML file, so move it however you move dotfiles: a git
repo, `scp`, a gist. It works the way editor and dotfile sync tools do:

- **Only portable settings travel.** Categories are `pipeline`, `skills`
  (`[message.skills]`), `automation` (`enabled` and each rule by name),
  `agent` (`[agent.<program>].options`), `watch`, `collaboration` and
  `ticket` (including `[ticket.source.<name>]`). Settings tied to one
  machine never leave it and are refused if a bundle carries them:
  `[dashboard]`, `[ask]`, `[fleet]`, `[[route]]` (its paths are local),
  `collaboration.path`, `ticket.cwd` / `additional_dirs`, the composer state
  in `[watch]`, `automation.paused_until`, and `[agent.<program>].account`.
  An automation rule that switches accounts (`swap_account`) stays home
  whole, since it depends on tooling only that host has; export says so.
- **Apply is three-way.** Each host remembers what it last applied from a
  source (`$XDG_DATA_HOME/tend/config-sync/`). An item changes only where the
  local copy still matches that base: a local edit survives an unchanged
  upstream, an upstream removal deletes only what sync put there, and an item
  changed on both sides is a conflict. Conflicts stop the apply until you
  resolve them, ignore the item, or choose `--prefer local|incoming`. The
  first apply from a source adds and replaces but never deletes.
- **Preview first.** `diff` writes nothing and exits like `kubectl diff`:
  `0` nothing to change, `1` changes, `2` an error or conflict. It lists only
  the changed items, never the neighbouring config.
- **Safe writes.** `apply` edits only the planned items, so comments and
  layout elsewhere stay as they were; it backs the file up, writes through
  tendd (validated, and refused if the file changed meanwhile), and shares
  Fleet sync's lock and recovery. Restart tendd afterwards to load it.

A bundle remembers which host it came from. Applying one from a different
source needs `--switch-source`, after which its items count as a first sync.

What a host takes part in is its own choice, and is never exported:

```toml
[config_sync]
categories = ["pipeline", "skills", "automation"]   # default: all
ignore = ["pipeline.solo", "automation.rule.confirm-selected-yes", "ticket"]
```

Free-form text travels as written: pipeline prompts, skills, ticket prompts
and provider options can hold paths or secrets, so read a bundle before you
publish it, or `ignore` what should stay. Agent integration (hooks, MCP,
skills) is not part of the bundle; run `tend init` on each host, setting
`CLAUDE_CONFIG_DIR` / `CODEX_HOME` for every provider home you use.

## Fleet

```toml
[fleet]
enabled = true              # outbound SSH hosts; local is always visible
refresh_secs = 15
keepalive_secs = 10
offline_after_secs = 30
connect_timeout_secs = 10
command_timeout_secs = 10
max_parallel_connects = 6
capture_policy = "selected" # selected | never

[fleet.local.labels]
environment = "development"

[fleet.local.annotations]
"tend.dev/owner" = "platform"

[fleet.hosts.dev]
ssh = "tend-devbox"
tend_path = "tend"
enabled = true
connect = "auto"            # auto | on_demand
mode = "observe"            # observe | control
# remote_socket = "/run/user/1000/tend.sock"

[fleet.hosts.dev.labels]
environment = "development"
region = "icn"

[fleet.hosts.dev.annotations]
"tend.dev/owner" = "platform"
```

Fleet always publishes the controller as the first `local` host using an
in-process snapshot; this remains available with `enabled = false`. The flag
controls outbound SSH hosts. Fleet maintains one persistent OpenSSH stdio
relay per enabled remote physical host.
`offline_after_secs` must be at least twice `keepalive_secs`, and all timeout
and concurrency values must be non-zero. `capture_policy = "never"` disables
both pane and window capture at the manager even on control hosts.

`ssh` is an OpenSSH destination/Host alias, not a place for flags. Put port,
identity, ProxyJump, and host-key policy in `~/.ssh/config`. `tend_path` and
`remote_socket` are validated as fixed remote command tokens. Labels implement
Kubernetes-style selectors; annotations allow descriptive values but use the
same namespaced key syntax. Prefer `tend fleet host add/label/annotate` to edit the
inventory atomically. Use `tend fleet host label local` and `tend fleet host annotate
local` for controller metadata; tendd-managed identity labels cannot be
overridden. See [FLEET.md](FLEET.md).

## Dashboard

```toml
[dashboard]
enabled = false
bind = "127.0.0.1:7878"
auth = "token"
token = ""
allow_public = false
```

The dashboard is loopback-only unless public binding is explicitly
allowed. Use `auth = "public_read"` with a token to expose anonymous reads
while keeping browser control actions PAT-gated. `auth = "none"` exposes reads
and disables control actions entirely. See [DASHBOARD.md](DASHBOARD.md).

The dashboard is single-user: the bearer token is its only credential. Older
configs that still carry `[dashboard.login]` or `[dashboard.sharing]` load with
a warning and the sections are ignored; delete them.

## External Sinks

Sinks are opt-in fan-out targets. The current documented sink forwards
prompts to oh-my-prompt. See [SINKS.md](SINKS.md).

## Pane host selection

`TEND_HOST=tmux|cmux|rmux|herdr|zellij` pins a single host. `TEND_HOSTS` accepts an
ordered comma-separated set, for example `TEND_HOSTS=rmux,tmux`. rmux's native
`RMUX` variables take precedence over the `TMUX` compatibility variables it
also exports. See [CMUX.md](CMUX.md), [RMUX.md](RMUX.md), [HERDR.md](HERDR.md), and
[ZELLIJ.md](proposals/ZELLIJ.md).
