# Muxa Fleet

Muxa Fleet is the physical-host control plane above the existing
session/window/pane model:

```text
local muxad
  FleetManager
    ├─ in-process      ─ host local ─ session ─ window ─ pane (agent)
    ├─ SSH stdio relay ─ host dev ─ session ─ window ─ pane (agent)
    ├─ SSH stdio relay ─ host gpu ─ session ─ window ─ pane (agent)
    └─ per-host cache  ─ host prod ─ last known snapshot while offline
```

The controller is always published as the first, in-process `local` host.
Each configured remote host gets one independent, persistent OpenSSH process. The
remote command is `muxa relay --stdio`, which talks only to that user's local
owner-only muxad Unix socket. Fleet does not expose a remote TCP port, forward
an agent socket, or copy terminal contents continuously.

This is distinct from Muxa's historical “pane host” terminology. A Fleet host
is a physical node. tmux, rmux, herdr, and zellij remain backend kinds inside
that node.

## Prerequisites

- Install the same Muxa version on the controller and every remote host.
- Run muxad for the same Unix user on every remote host.
- Configure and test non-interactive OpenSSH authentication and host-key trust.
- Put users, ports, keys, ProxyJump, and other SSH policy in `~/.ssh/config`.

For example:

```sshconfig
Host muxa-devbox
  HostName devbox.example.com
  User june
  IdentityFile ~/.ssh/id_ed25519
  IdentitiesOnly yes
```

Then verify the transport before adding it:

```bash
ssh -T -o BatchMode=yes muxa-devbox muxa relay --stdio
```

The command emits one JSON hello frame and waits for requests; `Ctrl-C` is
expected for this manual probe.

## Inventory and metadata

No setup is required for the controller node:

```bash
muxa fleet status
muxa host show local
muxa host doctor local
muxa host label local environment=development
muxa host annotate local muxa.dev/owner=platform
```

`local` has the same stable NodeId and topology shape as a remote node, but it
does not use SSH and is always connected in control mode. It cannot be added,
removed, disabled, or disconnected. User metadata is stored under
`[fleet.local]`. muxad owns the truthful `muxa.io/local`,
`muxa.io/transport`, and `kubernetes.io/{hostname,os,arch}` labels; selectors
may use them, but user configuration cannot override them.

Register hosts through the CLI so Muxa validates and atomically updates the
TOML inventory:

```bash
muxa host add dev muxa-devbox \
  --label environment=development \
  --label region=icn \
  --annotation muxa.dev/owner=platform \
  --mode observe

muxa host doctor dev
muxa host list
muxa host show dev
```

The first remote-host command enables outbound SSH Fleet connections. The
local node remains available when `[fleet] enabled = false`. Host mutations
ask muxad to restart itself so the connection set and authorization policy are reloaded together.
If the daemon cannot be reached, the config edit remains valid and the CLI
prints a reminder to restart it manually.

Labels follow Kubernetes key/value rules and are meant for selection:

```bash
muxa host label dev tier=worker accelerator=gpu
muxa host label dev tier=worker --overwrite
muxa host label dev accelerator-              # remove
muxa host tag dev region=icn                   # visible alias of label
```

Annotation keys use the same namespaced key rules but their values may hold
descriptive text or URLs:

```bash
muxa host annotate dev muxa.dev/runbook=https://example.invalid/dev
```

The controller alias is human-friendly configuration identity. The relay also
creates `$XDG_DATA_HOME/muxa/host-id` as an owner-only stable UUID. Rename an
SSH alias, hostname, or inventory alias without changing node identity. Muxa
refuses two live aliases that report the same node UUID, preventing accidental
double control of one machine.

Supported selector requirements are:

- `environment=production` and `environment==production`
- `environment!=production`
- `region in (icn,nrt)` and `region notin (iad,sfo)`
- `accelerator` (key exists) and `!accelerator` (key does not exist)
- comma-separated AND, such as `environment=production,region in (icn,nrt)`

## Access policy and connection policy

Every host has two independent policies:

The local node is always connected and always uses `control`; these settings
apply to remote inventory entries:

- `mode = "observe"` allows snapshots and on-demand captures but rejects
  prompt delivery. This is the default.
- `mode = "control"` additionally permits exact-pane prompt delivery.
- `connect = "auto"` keeps a relay connected with bounded exponential
  reconnect backoff.
- `connect = "on_demand"` remains disconnected until `muxa fleet connect`.

Use `muxa host disable` to keep metadata but suppress all connections, and
`muxa host enable` to restore it.

## Operating the fleet

```bash
muxa fleet status
muxa fleet status -l 'environment=production,region in (icn,nrt)'
muxa fleet status -o wide
muxa fleet status -L environment,region
muxa fleet status --show-labels
muxa fleet status -o json                 # `--json` remains compatible
muxa fleet watch
muxa watch --fleet --selector 'accelerator=gpu'

muxa fleet panes local
muxa fleet capture local '%12'
muxa fleet send local '%12' 'Please summarize the current result.'
muxa fleet attach local '%12'

muxa fleet connect dev
muxa fleet disconnect dev
muxa fleet refresh dev
muxa fleet panes dev
muxa fleet capture dev '%12'
muxa fleet send dev '%12' 'Please summarize the current result.'
muxa fleet attach dev '%12'
```

A bare pane id or `session/window/pane` path is accepted only when unique
across all backend endpoints on that physical host. If even the display path
is ambiguous, `muxa fleet panes HOST --json` prints the complete `PaneKey` JSON
accepted by capture/send/attach and MCP tools. Internally, every command carries
the complete node/backend/session/window/pane identity, and the relay verifies
that identity again against a fresh pane list before a control action.
The local adapter performs the same exact-key verification in process; local
attach jumps directly without opening an SSH TTY.

The default status table is deliberately compact (`HOST/STATE/MODE/AGENTS/
PANES/ATTN/AGE`) and never prints the full managed-label set into a normal
terminal row. Use `-L` for selected Kubernetes-style label columns,
`--show-labels` for the complete label map, `-o wide` for host/version/latency
details as space permits, or `-o json` for the lossless machine interface.

When the selector contains only the controller's `local` node, Fleet watch
delegates to the full native `muxa watch` surface. There is no redundant host
row, and all existing inspectors, tree/swarm modes, collaboration/mailbox,
ask, previews, command palette, message-skill editing, and configuration
persistence remain available. Once a second physical node is visible, the
host level becomes meaningful and the central Fleet hierarchy is shown.

The multi-node Fleet TUI uses the same focused navigation conventions:

- `j`/`k` and Arrow Up/Down move between siblings in focus mode. A singleton
  session/window/pane chain automatically bubbles to the nearest parent with
  siblings, so it never traps the cursor. In always/manual expansion they move
  through visible rows.
- Uppercase `J`/`K` jumps directly between actionable panes across the Fleet.
- `h`/`l` or Left/Right collapses/descends; Space toggles a parent.
- `/` filters, `Alt-a` toggles attention-only, `a` opens Ask, `A` opens Ask
  history, `r` refreshes, and `c` connects or disconnects a remote host
  (`local` reports that it is always connected).
- `o`/`p` captures the selected pane. `m` works on a session, window, or pane;
  parent nodes resolve to the lowest-index pane that owns a live agent, while
  an exact pane stays exact. `Tab` cycles the durable request kind,
  `Ctrl-E` cycles read-only/execute/just-send, and `/` opens the shared
  message-skill palette. In that palette, `F2`/`Ctrl-A` registers a skill and
  Delete/`Ctrl-D` removes the selected skill. `M` (or `b`) opens that pane's mailbox, where `i`
  claims queued requests and `e` replies. These operations use the selected
  physical host's local muxad over the existing SSH relay.
  Each mailbox tab returns its 32 newest requests to keep the interactive
  response bounded; the node remains the durable source of truth.
- Enter attaches directly on `local` or through a separate remote SSH TTY,
  and `?` shows help. `muxa init` binds `prefix+s` to local watch and
  `prefix+S` to Fleet watch.
- `--view`, `--layout tree|swarm`, `--sort`, and `--theme` use the same values
  as native watch. `focus` expansion follows the selected view depth,
  `manual` changes only through structural keys, and direct `j`/`k` jumps
  reveal only the target's ancestors.
- `--include-paneless` exposes agents hidden by the default setting as explicit
  rows with inspectors; pane-only attach/capture/message actions stay disabled.

Session inspectors roll up their windows and panes. A selected window fetches
its panes on demand and renders their actual tmux split geometry. Captures are
rate-limited and never performed for collapsed or unselected windows.

## Health and consistency

Host state is one of `disabled`, `connecting`, `online`, `degraded`, `offline`,
`auth_failed`, or `version_skew`. The last good snapshot remains visible while
a host is offline. A successful new handshake clears stale remote identity
before accepting the new snapshot.

Relay snapshots and transitions carry monotonic revisions. A gap marks the
host degraded and triggers reconciliation; a lost subscription reconnects the
relay. Keepalives detect a silent transport, while each host's state machine,
backoff, and task remain independent so one slow host cannot stall the fleet.
`max_parallel_connects` caps simultaneous SSH handshakes.

The central TUI subscribes to compact, selector-scoped Fleet cache
invalidations and coalesces bursts before fetching one coherent filtered
snapshot. The server installs the stream before acknowledging it and the
client then performs a fresh snapshot, closing the startup race. Refreshes are
coalesced for 75 ms and capped at four snapshots per second. With a current
daemon it performs a slow 15-second reconciliation poll; an older daemon that
does not advertise `fleet_subscribe` falls back to one-second polling. Idle
frames redraw at most once per second, while input and state changes repaint
immediately.

The local adapter consumes Store transitions directly and refreshes backend
topology at `refresh_secs`; backend scans run on blocking workers rather than
the async IPC executor. It maintains its own revisioned Fleet snapshot, so
selectors and UI rendering never clone or rewrite the authoritative local
Store.

The central cache stores agent/topology metadata only. Pane captures are
bounded, sanitized for terminal control sequences, and requested only when a
consumer selects them. Window capture parallelism and payload sizes are also
bounded. Prompt text is sent over stdin, never interpolated into a remote shell
command, and is not included in manager audit logs.

Mutating requests are never retried automatically. A result distinguishes text
delivery from Enter submission so callers do not duplicate a prompt after a
partial acknowledgement.

## Other interfaces

`muxa mcp` exposes `muxa_fleet_status`, `muxa_fleet_capture`, and
`muxa_fleet_send_prompt`. Control tools require an explicit host and pane;
remote hosts apply the observe/control check, while `local` is owner-socket
control.

When the web dashboard is enabled, its authenticated read surface includes
`GET /api/fleet?selector=...`. PAT-gated control is available at
`POST /api/fleet/{host}/command` with a serialized `FleetOperation`; dashboard
`auth = "none"` still disables all writes.

Durable collaboration data remains owned by each physical node. Fleet watch
does not copy it into a central database: `m`, `M`, claim, and reply commands
are routed to the selected node's muxad over the authenticated SSH stdio relay.
The `collaboration` relay capability gates these commands, so mixed-version
nodes fail with an upgrade instruction instead of silently degrading durable
requests into keystrokes.

## Security checklist

- Keep the controller dashboard loopback-only unless its documented PAT and
  public-bind controls are intentional.
- Prefer OpenSSH `Host` aliases and review `known_hosts`; Fleet uses
  `BatchMode=yes` and never disables host-key checking.
- Fleet forces `ClearAllForwardings=yes` and does not request agent forwarding.
- Start new hosts in observe mode, then promote only the hosts that need
  control with `muxa host add ... --mode control --overwrite`.
- Treat access to the controller's muxad socket, config file, and SSH keys as
  shell-equivalent authority.
- Use labels for selection, not secrets. Annotations are displayed in the
  inspector and must not contain credentials either.

See [CONFIGURATION.md](CONFIGURATION.md) for every setting and
[MULTI_HOST.md](MULTI_HOST.md) for local multi-backend observation.
