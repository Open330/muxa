# Fleet Work dispatch

Status: first implementation. One coordinator owns placement and durable dispatch
records. Any configured entry host can submit the same request and read its status.
Workers run a managed, detached **tmux** Workspace/Work pipeline, so the operator
can attach at any time. This is not a headless agent or live session migration.

## Configuration

On the coordinator (for example june-mbp):

```toml
[orchestration]
enabled = true
# coordinator omitted means this node owns placement and Global Ask.

[orchestration.paths]
root = "~/workspace-muxa"
repo = "repos/{repo}"
run = "runs/{workspace}/{work}/{attempt}"
artifacts = "artifacts/{workspace}/{work}/{attempt}"

[orchestration.workspaces.muxa]
repo = "open330-muxa"
url = "git@github.com:Open330/muxa.git"
pipeline = "solo"
selector = "organization=personal"

[orchestration.workspaces.muxa.paths]
root = "~/projects/muxa-execution"

# Prefer a stable NodeId; a coordinator-local Fleet alias also works.
[orchestration.workspaces.muxa.nodes.rtzr]
root = "/home/june/workspace-muxa"
```

On entry hosts:

```toml
[orchestration]
enabled = true
coordinator = "june-mbp"
```

The coordinator alias must be a configured control-mode Fleet host, reachable
from that entry host. Muxa does not establish a VPN or synthesize SSH routes.
Configure ProxyJump/HostName in OpenSSH when required. The coordinator itself
must omit `coordinator` (or set it to `local`); forwarding loops are rejected.
`local` always means the daemon handling the request, never a particular laptop.

Workers must enable orchestration and install the named pipeline and provider.
The coordinator stores repository, selector and path policy; worker-local paths
are resolved using the worker's home directory. No credentials or whole home
folders are synchronized. Install matching Muxa CLI and daemon versions; the
new `orchestration_v1` / `collaboration_dispatch_v1` capabilities prevent silent
use of older execution contracts.

Path precedence is global defaults → workspace overrides → node overrides.
NodeId overrides take precedence over aliases. Absolute paths and `~/` are
supported; relative templates resolve under root. Available placeholders are
`{repo}`, `{workspace}`, `{work}`, `{attempt}`. The latter is the dispatch UUID.
Run/artifact templates must include `{attempt}`. Overlapping paths, parent
traversal and unknown placeholders are rejected. Repo/workspace/work IDs are
safe ASCII components (letters, numbers, `-`, `_`, `.`; not `.` or `..`). Existing
checkouts can be mapped using the repo template; their origin must match exactly.

## Dispatch

Create one UUID before sending. Example input (replace commit with a real full
Git object ID available from the configured repository):

```json
{
  "dispatch_id": "d4ee3ec5-8a79-4ef2-9b26-50c9c975e5a1",
  "workspace": "muxa",
  "work": "linux-validation",
  "commit": "0123456789abcdef0123456789abcdef01234567",
  "body": "Run the Linux checks. Report commands, outcomes, and artifacts.",
  "selector": "kubernetes.io/os=linux"
}
```

```sh
muxa work dispatch --from-json request.json --plan
muxa work dispatch --from-json request.json
muxa work dispatch-status d4ee3ec5-8a79-4ef2-9b26-50c9c975e5a1
```

MCP exposes `muxa_dispatch_work` (same JSON, optional `plan: true`) and
`muxa_dispatch_status` (`dispatch_id`). A plan makes no filesystem changes and
spends no provider turn. The workspace selector and request selector intersect;
`host` can further restrict to an alias or NodeId. Eligible nodes must be online,
control-authorized and support orchestration. Selection prefers fewer active
agents and breaks ties by NodeId. This is advisory load selection, not a memory,
GPU or CPU reservation scheduler.

Execution journals the chosen node and path policy before sending. The worker
checks its actual NodeId, clones if needed, fetches the exact commit if missing,
serializes shared-clone preparation with an OS file lock, creates a detached
worktree and starts the pipeline with `--no-ticket`. It writes
`dispatch.json` into the attempt's artifact directory. Pipelines whose routes
also prepare a worktree must be adjusted: dispatch owns that preparation.

The returned state is `preparing`, `launched`, `blocked`, `completed`, `failed`
or `unknown`. `launched` only means the pipeline started. Status reads derive
completion from the durable pipeline aliases for the exact workspace/work/cwd,
not from agent idleness or terminal text. Results carry verification states and
an artifact directory locator; artifact bytes and commits are not automatically
copied back or merged. Agents should report deliverables and resulting commits.

## Delivery and recovery

* Reuse dispatch_id and the unchanged payload. The journal refuses collisions.
* The coordinator reserves a Work once; a new UUID is not permission to launch
  the same Work elsewhere, including case variants of its canonical tmux ID.
  Worker reservations survive crashes.
* A lost response yields `unknown`. Status reads can recover the worker's durable
  result without resending a launch. No automatic failover or duplicate run is
  attempted after a disconnect.
* A damaged/incomplete journal blocks execution. An operator must establish what
  ran before repairing it. Initial implementation deliberately has no automatic
  reservation expiry, destructive cleanup or rerun command.
* Journals live under the Muxa data directory, `dispatches/{coordinator,worker}`.
  Peer sends also write `dispatches/peer-outbox` before the remote effect.

`muxa_fleet_call_peer` accepts optional UUID `dispatch_id` and a
`delegation_parent: {node_id, request_id}` across nodes. The source node is stamped
by the sender. These causal fields do not grant authorization or replace local
`parent_request_id` participant checks. The sender receipt pins the target NodeId as well as its Fleet alias.
Repeated sends return the same durable request and reject changed payload/recipient. Dispatch-bearing mailbox records
are retained to prevent old IDs executing again; they are excluded from ordinary
history pruning in this version. Plan data-directory retention accordingly.

## Global Ask and agent instructions

With orchestration enabled and a remote coordinator, Global Ask send, new
conversation, history, conversation selection, provider selection, reset and
history deletion route to that coordinator. Entry hosts do not replay their own
history. Provider configuration remains coordinator-local. Shared Ask streaming
is not bridged yet: clients needing live subscriptions must connect to the
coordinator; snapshot methods work from any entry host. Existing CLI Ask providers
receive a short delegation hint on a new session. API-only/read-only providers
retain their existing tool/permission limitations.

Keep AGENTS.md small, for example:

> Delegate authorized Fleet work through muxa_dispatch_work. Muxa owns placement
> and paths. Use a stable dispatch ID, exact commit, bounded task and acceptance
> criteria. Reuse the ID after uncertain delivery. Report completion/artifacts
> or a real blocker; avoid repeated prompts and terminal polling.

Full policy belongs in Muxa configuration, not copied inventories in prompts.
Attachability is implemented through tmux. An explicit operator takeover state
that pauses dependency reconciliation, optional headless execution, hardware
capacity reservations, queues, artifact transfer and multi-node DAG dispatch are
not implemented by this first slice.

## Native macOS app

The Work Command Center's **Fleet Dispatch** opens the coordinator-backed flow;
**Start Work** retains direct, manually chosen host/pipeline execution. Dispatch
loads registered workspaces from the coordinator, accepts an exact commit and
optional node selector/host, and offers an optional placement preview before launching. A preview
is advisory; the returned dispatch record is the authoritative assignment.

The app saves the immutable request and UUID before sending, scoped to its daemon
socket and coordinator. Saved dispatches can be reopened after app restart. A lost
response leaves the request frozen: check its status or retry the same payload.
Closing the sheet does not cancel work. Completion and artifact locations come
from worker status, never pane idleness. **View worker** matches stable NodeId
against the entry host's inventory rather than interpreting coordinator aliases
as local aliases. Register the worker in that inventory to attach through the app.

Settings → **Dispatch** edits routing, labels, workspace policies, and global,
workspace, and node path templates. Empty path overrides inherit their parent.
Workspace and node path policy is edited on the coordinator; entry hosts configure
its alias. Policy writes pin the original config text and refuse concurrent edits;
reload and reapply instead of silently overwriting someone else's change. Unrelated
config sections are preserved. After editing labels, the app refreshes the config
baseline while retaining the policy draft; concurrent policy edits still conflict. These forms require `config_orchestration_v1` and a
matching CLI with `work dispatch-options`.

On entry hosts without a shared Ask stream, the app stops retrying that unsupported
subscription and uses its normal 15-second history reconciliation. Ordinary
transport errors still reconnect. The coordinator can continue using its local
Ask stream. App packages must embed the matching CLI and daemon; installing only
the new Swift UI against an older runtime leaves the new controls unavailable.
