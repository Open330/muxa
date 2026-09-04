# Configuration

`muxad` reads `$XDG_CONFIG_HOME/muxa/config.toml` when present. Start from
`config.example.toml` for a full annotated file.

## Socket

```toml
socket = "/tmp/muxa.sock"
```

The CLI also honors `MUXA_SOCKET`. tmux environments are healed on daemon
startup so existing panes can reach the current socket.

## History

```toml
[history]
enabled = true
path = "$XDG_DATA_HOME/muxa/prompts.ndjson"
max_per_pane = 50
max_age_days = 30
```

Prompt history is retained, not an unbounded warehouse. `muxa recap` and
prompt totals in `muxa stats` use this retained window.

## Activity

```toml
[activity]
enabled = true
path = "$XDG_DATA_HOME/muxa/activity.ndjson"
max_age_days = 30
```

The activity ledger stores agent state intervals, tmux foreground
intervals, and muxa human interaction intervals. See
[ACTIVITY.md](ACTIVITY.md).

## Session Activity

```toml
[session_activity]
enabled = true
path = "$XDG_DATA_HOME/muxa/session-activity.json"
interval_secs = 5
```

This tmux foreground sampler remains as a compatibility source and helps
stats while newer activity ledger intervals are accumulating.

## MCP orchestration guide

```toml
[mcp.guide]
placement = "window" # pane (default) | window | session
agent = "codex"      # claude | codex | gemini | agy | opencode
options = ["--model", "preferred-model"]
direction = "auto"   # auto (default) | right | down
instructions = "Keep one task per window; use a new session for unrelated projects."
```

This section records how the user normally organizes agent work. Muxa includes
it in the MCP initialization instructions and exposes it through `muxa_guide`,
so a connected agent does not have to guess whether a new task belongs in a
pane, window, or separate session. `muxa_start_agent` also uses these values
when the matching arguments are omitted. If `agent` is not configured, the
tool still requires the caller to choose one.

`options` are additional individual CLI arguments for the configured agent.
They are inserted after Muxa's built-in provider profile and before an initial
prompt, and every value is shell-quoted independently. An explicit `options`
array in `muxa_start_agent` replaces the configured array; pass an empty array
to suppress configured extras. Options are not applied when the caller selects
a different agent. Managed Work still follows its fixed
Workspace=session/Run=window/Agent=pane layout, and collaboration peer spawning
still uses a pane in the current window. Restart an MCP-connected agent after
changing this section because its stdio MCP process loads config at startup.

## Ask

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
```

Opt-in headless questions from `muxa watch`: `a` composes one, `A` browses
the answers. muxad runs the agent in print mode and captures the reply, so
there is no session to manage and completion is an exit code rather than a
guess. Each agent keeps its own conversation and every question after the
first resumes it, reusing the cached context the first one paid for; `n` in
the panel starts a fresh thread. `path` defaults to
`$XDG_DATA_HOME/muxa/ask.json` and holds both the history and the per-agent
thread ids. Off by default because enabling it lets the daemon spawn a CLI
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
`--approval-mode plan`); `muxa work compose` always drafts under it.

### Providers

An **engine** is the code that drives a provider: the argv a CLI takes, the
JSON an API answers with, the environment variable its key lives in. There
are five and they are fixed — `claude`, `codex`, and `gemini` drive the
agent CLIs in print mode (`claude -p`, `codex exec --json`, `gemini -p
--output-format json`) and resume their own sessions between questions;
`anthropic` and `openai` call the Messages and Chat Completions APIs
directly over HTTPS. API engines have no session to resume, so muxad
replays the conversation's earlier turns from its own history — the most
recent 40 turns or 60k characters — ahead of each question; `cwd`,
`additional_dirs`, and `permission_mode` do not apply to them.

A **provider** is an `[ask.providers.<id>]` table you compose. The id is
yours to pick (a TOML bare key: letters, digits, `-`, `_`), and `engine`
says which of the five drives it. Several providers may share one engine —
a work and a personal OpenAI account, two Anthropic keys, a second `claude`
binary — and each keeps its own conversation, so switching between them
resumes rather than restarts. `[ask] agent` and `muxa ask --agent` name a
provider id, not an engine.

The five engine ids are also providers of themselves, so a fresh install
works with no `[ask.providers]` at all: `muxa ask providers` lists what you
composed first, then every built-in no id of yours has taken over. A table
named after a built-in and *without* `engine` tunes that built-in — which
is why an `[ask.providers.anthropic] model = "…"` written before any of
this still means what it always did. Removing such a table clears the
settings; the built-in itself stays.

An API key comes from, in order: the one-turn key a client sends, the
engine's own variable in muxad's environment (`ANTHROPIC_API_KEY`,
`OPENAI_API_KEY`; `CODEX_API_KEY` and `GEMINI_API_KEY` for the CLIs), then
the variable that provider's `api_key_env` names. The key itself is never
written to config.toml. Two providers on one engine share the engine's
variable, so give each its own `api_key_env` (or send a one-turn key) when
they must not use the same account. `model` overrides the model for any
provider, CLIs included, and `executable` points a CLI engine at a
particular binary.

The same daemon-owned history is available without opening the TUI:

```bash
muxa ask --agent codex "summarize the current implementation"
muxa ask --agent claude --detach --json "review the deployment plan"
muxa ask --agent anthropic "which files does the reaper touch?"
security find-generic-password -w -s my-codex-key \
  | muxa ask --agent codex --api-key-stdin "review this repository"
muxa ask --agent anthropic-work "…"   # a provider you added, by its id
muxa ask providers [--json]           # every provider: engine, kind, model, whether a key resolves, selected
muxa ask provider add anthropic-work --engine anthropic \
  --title "Anthropic (work)" --api-key-env WORK_ANTHROPIC_KEY
muxa ask provider add claude-work --engine claude --executable /opt/homebrew/bin/claude
muxa ask provider set anthropic --model claude-opus-5 --api-key-env WORK_ANTHROPIC_KEY
muxa ask provider set anthropic --clear-model         # flags you omit leave that key unchanged
muxa ask provider remove anthropic-work               # a built-in id only loses its settings
```

`add` refuses an id that already exists, an id that is not a TOML bare key,
an engine that is not one of the five, and a built-in id asked to run
someone else's engine. The engine is fixed once added — `set` changes the
title, model, key variable, and binary, and to change the engine you remove
the provider and add it again, because a live conversation cannot resume a
`claude` session id against `codex`. Removing the provider `[ask] agent`
points at hands the selection to the first one left.

`--api-key-stdin` refuses an interactive terminal, crosses only the owner-only
Unix socket, and supplies the key to one matching provider child. It is never
stored in muxa config/history or placed in argv. Without it, the headless CLI
uses its existing login or provider environment as before.

## Collaboration

```toml
[collaboration]
enabled = true
wake = "idle_only" # idle_only | never
wake_payload = "operator_full" # notice | operator_full | full
scope = "window"   # window | host
max_message_bytes = 16384
# path = "$XDG_DATA_HOME/muxa/collaboration.json"
# retention_days = 90 # omitted: retain indefinitely
```

Opt-in durable request/reply between agents in the same stable tmux window.
The historical optional `path` default is
`$XDG_DATA_HOME/muxa/collaboration.json`; muxa imports that JSON once into the
authoritative sibling `collaboration.sqlite3` and retains the JSON as a
migration backup. A configured `.sqlite`, `.sqlite3`, or `.db` path is used
directly. The database stores mailbox state and exact-session aliases/roles.
`retention_days` prunes eligible fully delivered terminal threads at daemon
startup; omission retains all history. The retained JSON backup is a duplicate
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

## Message skills

Reusable prompt templates live in a regular TOML table. They are shared by the
watch and dashboard `m` composers, the watch `a` composer, and MCP
`muxa_call_peer` requests:

```toml
[message.skills]
agent-review = "create a new pane with codex (use the cx alias), then pass our changes to it for review"
```

Manage the table without hand-editing it:

```bash
muxa skill add agent-review 'create a new pane with codex (use the cx alias), then pass our changes to it for review'
muxa skill list
muxa skill show agent-review
muxa skill remove agent-review
```

At any point in a draft, press `/`, type to filter, move with arrow keys or
`Tab`, then press `Enter`. Selection inserts the prompt at the current cursor
without replacing existing text; inspect or edit it and press `Enter` again to
send. Multiple skills can be combined in one draft. In `muxa watch`, `F2` opens
an add/update form inside the palette and `Delete` removes the selected skill
after confirmation. `Ctrl-A` and `Ctrl-D` remain compatibility aliases.
Pass `-` as the CLI add prompt to read a multi-line template from stdin.

In an MCP-connected agent conversation, `/name` selects the same template for
`muxa_call_peer`; its optional body and context are appended without changing
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
view = "work"
columns = ["pane", "state_age", "model", "ctx", "cost", "prompt", "activity"]
sort = ["state", "workspace", "latest"]
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

- `theme` — visual preset: `classic`, `oh-my-muxa`, `focus`, `ops`, or a
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
`gemini`/`agy`) and backfills the registry without waiting for a hook to fire. It
runs once at daemon startup and then every `interval_secs`, so a fresh agent
session in a new tmux session shows up in `muxa status` within that window
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

Installing a new muxa does not restart the daemon on its own. The package
manager writes the new build and repoints whatever is on `PATH`, while the
running process keeps its open inode and serves the old logic — and no service
manager intervenes, because `KeepAlive` and `Restart=always` react to a process
*exiting*, which nothing did. Left alone, the daemon can serve a build that is
weeks old, answering `protocol mismatch` to every CLI call once the wire format
moves on.

So muxad watches the binary it would re-exec through and, when that path
resolves to a different file for two consecutive polls, re-execs onto it. The
confirmation poll is what keeps a half-finished install from being adopted. The
daemon replaces itself in place (same pid), so this behaves identically under
launchd, systemd, and a bare terminal, and a failed re-exec leaves the old image
running rather than nothing at all.

Set `restart_on_new_binary = false` when something else owns the upgrade
sequence — say a deploy that installs several binaries and restarts them in a
particular order. `muxa daemon restart` then does it on demand, and `muxa
doctor` reports a daemon whose version has drifted from the CLI's.

## Reconciler

```toml
[reconciler]
enabled = true
interval_secs = 30
stuck_working_timeout_secs = 0
stuck_waiting_timeout_secs = 0
```

The reconciler keeps stale states from staying misleading forever. Timeout
values of `0` disable that timeout. The same loop also runs the pid-liveness
sweep that flips registered background tasks (see `muxa register`) to
`stopped` once their process exits.

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
"muxa.dev/owner" = "platform"

[fleet.hosts.dev]
ssh = "muxa-devbox"
muxa_path = "muxa"
enabled = true
connect = "auto"            # auto | on_demand
mode = "observe"            # observe | control
# remote_socket = "/run/user/1000/muxa.sock"

[fleet.hosts.dev.labels]
environment = "development"
region = "icn"

[fleet.hosts.dev.annotations]
"muxa.dev/owner" = "platform"
```

Fleet always publishes the controller as the first `local` host using an
in-process snapshot; this remains available with `enabled = false`. The flag
controls outbound SSH hosts. Fleet maintains one persistent OpenSSH stdio
relay per enabled remote physical host.
`offline_after_secs` must be at least twice `keepalive_secs`, and all timeout
and concurrency values must be non-zero. `capture_policy = "never"` disables
both pane and window capture at the manager even on control hosts.

`ssh` is an OpenSSH destination/Host alias, not a place for flags. Put port,
identity, ProxyJump, and host-key policy in `~/.ssh/config`. `muxa_path` and
`remote_socket` are validated as fixed remote command tokens. Labels implement
Kubernetes-style selectors; annotations allow descriptive values but use the
same namespaced key syntax. Prefer `muxa host add/label/annotate` to edit the
inventory atomically. Use `muxa host label local` and `muxa host annotate
local` for controller metadata; muxad-managed identity labels cannot be
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

## External Sinks

Sinks are opt-in fan-out targets. The current documented sink forwards
prompts to oh-my-prompt. See [SINKS.md](SINKS.md).

## Pane host selection

`MUXA_HOST=tmux|cmux|rmux|herdr|zellij` pins a single host. `MUXA_HOSTS` accepts an
ordered comma-separated set, for example `MUXA_HOSTS=rmux,tmux`. rmux's native
`RMUX` variables take precedence over the `TMUX` compatibility variables it
also exports. See [CMUX.md](CMUX.md), [RMUX.md](RMUX.md), [HERDR.md](HERDR.md), and
[ZELLIJ.md](ZELLIJ.md).
