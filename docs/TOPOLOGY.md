# Topology contract

Muxa has one canonical runtime hierarchy:

```text
TopologySnapshot
└── SessionNode
    └── WindowNode
        └── PaneNode
            └── Option<Agent>
```

The structural names are `session`, `window`, and `pane`. “Workspace” and
“work” may describe product meaning in prose, but they are not topology type
names or public API fields.

## Meaning and identity

| Level | Meaning | Stable identity |
| --- | --- | --- |
| session | project/workspace container | `(host, socket, session_id)` |
| window | ticket/work unit and collaboration room | `(session key, window_id)` |
| pane | one agent execution unit | `(window key, pane_id)` |

For tmux, `session_id` is tmux’s `$N` and `window_id` is `@N`. The mutable
session/window names are display metadata only. A tmux pane id such as `%5`
is unique only inside one server, so it must never be used as a join key by
itself.

Every key contains both `host` and `socket`. Hosts without a multi-server
socket concept use a documented stable sentinel (`herdr`, `zellij`, or
`default`) so consumers never have two key shapes.

Agent-runtime identity is separate from topology identity. Claude/Codex
session identity is called `agent_session_id`; tmux’s session identity is
called `tmux_session_id` at user-facing boundaries. Protocol v4 serializes
agent records with `agent_session_id` and downgrades the key to `session_id`
only for an explicitly negotiated v1–v3 daemon peer. The Rust registry field
keeps its old spelling temporarily for source compatibility.

## Backend mapping

Mappings preserve a backend’s native structure:

| Backend | session | window | pane |
| --- | --- | --- | --- |
| tmux | session | window | pane |
| rmux | session | window | pane |
| zellij | session | tab | pane |
| herdr | workspace | tab | pane |

`BackendTopologyCapabilities` marks each level as `native`, `mapped`, or
`unsupported`. A backend must report `unsupported` instead of manufacturing a
fake level.

## Join rules

Agent state joins a pane using `(host, socket, pane_id)`. Endpoint spellings
are normalized with the same rules as control operations: tmux full socket
paths and short socket names compare by basename; rmux retains its full native
endpoint.

An older agent record without an endpoint may join only when exactly one pane
on that host has its pane id. If two sockets both contain `%5`, the record is
left in `unassigned_agents`; choosing a representative pane would be unsafe.

Selection and expansion state use `TopologyNodeKey`, not row positions,
names, or representative panes. Search/filter render matching descendants
with their ancestor session/window nodes. Sorting is applied independently to
sibling sessions, windows, and panes.
