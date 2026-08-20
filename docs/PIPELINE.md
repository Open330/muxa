# Work pipelines — `muxa work up`

`muxa work start` is the imperative primitive: one invocation, one agent
pane. `muxa work up` is the declarative one. You give it a ticket id; it
gives you a staffed tmux window.

```console
$ muxa work up cal-1234
work CAL-1234 is in workspace callabo via pipeline triad
  cwd      /home/june/worktrees/cal-1234 (worktree cal-1234, created)
  ticket   CAL-1234 Reaper double-reaps a lying pane  [In Progress]
           https://linear.app/rtzr/issue/CAL-1234
  + plan       codex     planner      %12
  + impl       codex     implementer  %13
  + review     claude    reviewer     %14
  layout   main-vertical
```

Under the hood that is muxa's existing domain model — workspace = session,
work = window, agent = pane (see [WORKSPACE_MODEL.md](WORKSPACE_MODEL.md)) —
with a declared line-up on top of it.

## The shape

```
work id ──▶ [[route]] ──▶ workspace + cwd/worktree + pipeline
   │                              │
   └──▶ [ticket.source] ──▶ ticket context ──┘
                                             ▼
                              desired panes vs. actual panes
                                             ▼
                                    create the difference
```

Four ideas carry the whole feature.

### Ticket lookup is delegated to an agent

muxa does not speak Linear, Jira, or GitHub, and deliberately never will.
It spends one headless agent turn (`claude -p` / `codex exec`, the same
bridge `muxa ask` uses) asking an agent to fetch the ticket — because you
already taught your agent CLI how, through a skill, an MCP server, `gh`, or
a token in the environment. Adding a provider is a prompt, not a release.

```toml
[ticket.source.linear]
match  = '^cal-\d+$'
prompt = '''
Look up Linear issue {{id}} using the linear skill and answer with one JSON
object and nothing else:
{"id": "...", "title": "...", "body": "...", "url": "...", "state": "..."}
'''
```

The reply is *scanned* for a JSON object rather than parsed from position
zero, because agents answer in prose around their JSON as often as not — a
fenced block, a sentence of preamble, a trailing offer to help. The last
balanced object wins, so the shape echoed back from your prompt does not
beat the real answer. Common field spellings (`description` for `body`,
`identifier` for `id`, `{"name": …}` for `state`) are accepted.

Results are cached for `[ticket].cache_secs` (15 minutes by default) so
re-running to add one pane does not spend another turn. `--refresh` ignores
the cache; `--no-ticket` skips lookup entirely and launches on the id alone.

### Routing is yours

```toml
[[route]]
match     = '^cal-'
workspace = 'callabo'
pipeline  = 'triad'

[route.worktree]
repo   = '~/workspace/callabo'
branch = '{{id}}'

[[route]]
match    = '.*'
pipeline = 'solo'
```

Routes are an ordered list and the first match wins, so specific rules go
above the catch-all. A route decides three things: which tmux session the
work lands in, what directory its agents run in, and which pipeline staffs
it.

With `[route.worktree]`, each work item gets its own git worktree — which is
what keeps three agents in one window from tripping over each other in a
shared checkout. The default path sits *beside* the repo
(`<repo>/../<repo-name>-worktrees/<id>`) rather than inside it, so it does
not show up in the parent's status or in every `find` the agents run. An
existing worktree is reused; an existing branch is checked out rather than
recreated.

You do not need a route to get started: `muxa work up cal-1234 --pipeline
triad` treats the explicit flag as its own routing decision and uses the
current directory.

### A pipeline is a desired state, not a script

```toml
[pipeline.triad]
layout = 'main-vertical'
prompt = '''
{{work}} — {{ticket.title}}
{{ticket.url}}

{{ticket.body}}
'''

[[pipeline.triad.agent]]
alias   = 'plan'
program = 'codex'
role    = 'planner'
prompt  = 'You own the approach. Write the plan first; do not edit code.'

[[pipeline.triad.agent]]
alias   = 'impl'
program = 'codex'
role    = 'implementer'
prompt  = 'You own the implementation. Follow the planner.'

[[pipeline.triad.agent]]
alias   = 'review'
program = 'claude'
role    = 'reviewer'
prompt  = 'You own review. Critique the implementer; do not edit.'
```

`alias` is the load-bearing field: it is the key the desired-vs-actual diff
runs on, and it is recorded on the pane itself (`@muxa_agent_alias`), so it
outlives muxad, the CLI process, and the agent restarting inside the pane.
Keep aliases unique within a pipeline and stable once panes exist under
them.

The pipeline's `prompt` is context every agent needs, stated once; each
agent's own `prompt` is appended to it. `role` is recorded on the pane too,
so peers can address an agent as `role:reviewer` through the collaboration
layer.

`layout` is applied only after every pane exists — splitting a window
repeatedly halves whichever pane happened to be active, so geometry is worth
fixing once at the end rather than three times along the way.

### Re-running converges

Run `muxa work up` again and it compares the pipeline against the panes the
window already has:

- alias has no pane → **launch** it
- alias has a live pane → **keep** it, untouched
- alias has a live pane and you passed `--prompt` → **send** the message

```console
$ muxa work up cal-1234           # the reviewer's pane was closed
  = plan       running                %12
  = impl       running                %13
  + review     claude    reviewer     %21
```

So the first call stands a team up, the second is a no-op, and a call after
something died refills exactly the gap. Improving work already in flight is
the `--prompt` path:

```console
$ muxa work up cal-1234 --prompt "rebase onto main before you continue"
  » plan       prompted               %12
  » impl       prompted               %13
  » review     prompted               %21
```

That is opt-in on purpose. Injecting a prompt into an agent that is mid-turn
is disruptive enough to deserve an explicit ask, so a bare re-run leaves
live agents alone.

A pane that no pipeline alias claims — one you started by hand, or one left
behind by a pipeline you have since edited — is **reported and never
touched**:

```console
  ? (no alias) gemini    unclaimed    %30
```

Reconciling toward a desired state is useful. Reconciling *away* from a pane
a human opened is how orchestration earns distrust.

## Placeholders

Templates use `{{double}}` braces, and only keys muxa knows are substituted —
everything else survives verbatim. That is what lets a resolver prompt
contain the literal `{"id": "..."}` shape it is asking for, and it makes a
typo show up in the prompt instead of silently blanking.

| Key | Value |
| --- | --- |
| `{{id}}` | work id, lowercased — for branch names and directories |
| `{{work}}` | work id as muxa stores and displays it (`CAL-1234`) |
| `{{workspace}}` | resolved workspace/session |
| `{{cwd}}` | resolved working directory |
| `{{alias}}`, `{{role}}`, `{{program}}` | the agent being rendered |
| `{{ticket.title}}` `{{ticket.body}}` `{{ticket.url}}` `{{ticket.state}}` `{{ticket.id}}` `{{ticket.branch}}` | ticket context, when resolved |

`{{ticket.body}}` is clipped to 4000 characters with a `…[truncated]` marker.
A launch prompt carries the shape of the task and the URL; the agent can read
the rest itself.

## Command reference

```console
muxa work up <id>                    # resolve, route, create what is missing
muxa work up <id> --dry-run          # print the plan, touch nothing
muxa work up <id> --pipeline triad   # override the route's pipeline
muxa work up <id> --prompt "..."     # also message agents already running
muxa work up <id> --no-ticket        # skip lookup, launch on the id alone
muxa work up <id> --refresh          # ignore the cached ticket
muxa work up <id> --json             # structured plan + result
muxa work down <id>                  # close the window and every agent in it
```

`muxa work down` is a spelling of `muxa work close`; both refuse to touch an
unmanaged window, and both need `--workspace` when the same work id exists in
more than one workspace.

## Configuration

See the `[ticket]`, `[[route]]`, and `[pipeline.*]` sections of
[`config.example.toml`](../config.example.toml) for the annotated reference.
