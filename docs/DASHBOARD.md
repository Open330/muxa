# tend dashboard

A work-oriented HTTP UI bolted onto the daemon. Its primary view reads the
canonical tend hierarchy: **Workspace → Work → Run → Agent session**, with tmux
session/window/pane retained only as the current execution binding. An optional
Linear/GitHub/Jira issue is a reference attached to Work; it is not the Work
identity itself.

The board never promotes every visible tmux window into a Work card. Managed or
persisted Work appears on the four-stage board, while generic windows are listed
separately as **Unlinked executions**. Runs and panes remain expandable when
execution detail or controls are needed.

The timeline and raw agent/pane/terminal inventories remain available as
collapsed secondary panels. Data is updated live over Server-Sent Events, and
optional control actions are protected by a bearer token that can be pasted
into the browser like a PAT.

The dashboard is **off by default** and **loopback-only when on by default**.
Token authentication is the default auth mode, and an enabled dashboard must
have an explicit token. Binding directly to a non-loopback address requires explicit public-bind
acknowledgement.

## Surfaces

| Path                | What it returns                                                                |
| ------------------- | ------------------------------------------------------------------------------ |
| `GET /`             | The dashboard HTML (loads the JS bundle).                                      |
| `GET /static/*`     | Embedded JS/CSS/font assets.                                                   |
| `GET /api/health`   | `{ ok, version, protocol }`                                                    |
| `GET /api/access`   | Current read/control access mode and whether the supplied PAT can edit.        |
| `GET /api/agents`   | Current `Store` snapshot.                                                      |
| `GET /api/fleet?selector=...` | Cached hierarchy, including the always-present local node.          |
| `GET /api/panes`    | Global tmux pane list (every readable socket), with per-socket scan errors.   |
| `GET /api/works`    | Canonical v2 Workspace/Work/Run snapshot plus unlinked executions.            |
| `GET /api/work-metadata` | Durable titles, goals, next actions, and manual workflow stages.        |
| `GET /api/collaboration` | Indexed collaboration requests for the node/edge graph and sequence drill-down. |
| `GET /api/terminal-sessions` | Tend-owned PTY sessions.                                           |
| `GET /api/timeline` | Timeline document from `activity.ndjson` plus currently-open agent/tmux spans. |
| `GET /api/events`   | SSE stream: `snapshot` (initial), `transition` (live), `lagged` (backpressure)|
| `GET /api/windows/{window}/layout?socket=` | Pane geometry (`left`/`top`/`width`/`height` in cells, `active`, `zoomed`) for one tmux window. |
| `GET /api/panes/{pane}/output?socket=&lines=N` | Operator only: `{ pane, text, captured_at }`, the pane's newest `N` lines (default 200, max 1000; 64 KiB cap) as plain text, escapes stripped. |
| `GET /api/panes/{pane}/output/stream?socket=&lines=N` | Operator only, SSE: `output` events (`{ pane, text, captured_at }`, the full bounded text) on connect and whenever it changes; `gone` when the pane closes. See [Live pane output](#live-pane-output). |
| `POST /api/panes/{pane}/prompt` | Send and optionally submit text to a pane.                       |
| `POST /api/panes/{pane}/abort` | Send Ctrl-C to a pane.                                             |
| `POST /api/fleet/{host}/command` | Execute a serialized Fleet operation; host mode is rechecked.   |
| `PUT /api/work-metadata` | Save a definition by logical `{workspace_id, work_id}` identity.       |
| `POST /api/work-control/prompt` | Prompt every live agent run linked to one Work. Accepts `text`, or shared-composer `skill`/`body`/`context`. |
| `POST /api/work-control/up` | Start or converge a Work pipeline and optionally link an external issue. Requires `allow_work_start`. |
| `POST /api/work-control/abort` | Send Ctrl-C to every live agent run linked to one Work.          |
| `POST /api/terminal-sessions/{id}/input` | Send input to a Tend-owned PTY.                         |
| `POST /api/terminal-sessions/{id}/terminate` | Terminate a Tend-owned PTY.                          |

`auth = "token"` protects all API endpoints. `auth = "public_read"` leaves
GET/SSE endpoints public but always requires the bearer token for write/control
actions. `auth = "none"` leaves reads public and disables writes entirely.
Collaboration bodies and attached details require an authenticated operator
(valid PAT) in `token` or `public_read` mode. Anonymous users receive a
redacted topology/status summary. `none` mode
always redacts details. History responses use `Cache-Control: private, no-store`.
Static routes are public in every mode — see "Why the HTML is public" below.
Fleet routes always expose the in-process `local` node. `[fleet] enabled =
true` adds outbound SSH hosts; reads reuse the daemon's per-host cache and
never create request-scoped SSH connections. See
[FLEET.md](FLEET.md).

## Quick start

### Recommended: generate and persist a token with `tend init`

```sh
tend init --component dashboard
```

The initializer enables the loopback dashboard, generates a token, persists it
in `~/.config/tend/config.toml`, and prints a one-time bootstrap URL such as
`http://127.0.0.1:7878/#token=...`.

### Manual loopback setup with a token

For a manual setup, provide the token explicitly:

```sh
TOK=$(openssl rand -hex 32)
tendd --dashboard --dashboard-token "$TOK"
# then in the browser:  http://127.0.0.1:7878/#token=$TOK
```

The page captures `#token=...` into `localStorage` and rewrites the URL.
Subsequent requests carry `Authorization: Bearer <tok>`. The token persists
across tab close and browser restart, so you only need to paste it once per
browser profile. To revoke, clear the `tend.token` key (DevTools → Application
→ Local Storage) or restart `tendd` with a different token.

Legacy `?token=...` URLs are still accepted for compatibility and immediately
scrubbed, but fragments are the supported bootstrap format because browsers do
not send them in HTTP requests or `Referer` headers.

A browser that can read nothing (reads return 401: no working token) gets a
landing page instead of an empty dashboard. It says what tend is and points at
the way in: the dashboard link `tend init` printed (`#token=…`). A stored token
that the server rejects is reported there. `public_read` and `auth = "none"`
dashboards never show it, since their reads succeed without credentials. The
page is served from the daemon itself and loads nothing from other origins.
Its dashboard screenshots show a made-up fleet; regenerate them with
`node scripts/landing-shots/capture.mjs` (Playwright, Chrome and `cwebp`).
The hero film is captured from a real tend in the isolated sandbox (tmux,
the status line, `tend attend`, `tend peek`) with
`scripts/landing-shots/film.sh`, which writes `landing-frames.mjs`.

### Loopback-only without authentication (dev / single-user)

Disabling auth requires an explicit opt-out:

```toml
# ~/.config/tend/config.toml
[dashboard]
enabled = true
auth = "none"
# bind defaults to 127.0.0.1:7878
```

Or via flags:

```sh
tendd --dashboard --dashboard-auth none
```

Open <http://127.0.0.1:7878/>.

### Public read-only dashboard with PAT editing (LAN / VPN)

This is the mode for a dashboard that anyone on the reachable network may
view, while only a browser with the PAT can display and use edit controls:

```toml
[dashboard]
enabled = true
bind = "0.0.0.0:7878"
allow_public = true
auth = "public_read"
token = "replace-with-a-long-random-token"
```

Open `http://<host>:7878/` for anonymous read-only access. Select **unlock
edit** and paste the token to enable prompt, abort, input, and terminate
controls. The browser stores it in `localStorage`; selecting **lock edit**
removes it again.

The equivalent CLI invocation is:

```sh
TOK=$(openssl rand -hex 32)
tendd --dashboard \
      --dashboard-bind 0.0.0.0:7878 \
      --dashboard-auth public_read \
      --dashboard-token "$TOK" \
      --allow-public
```

### Public bind with fully token-protected reads (LAN / VPN)

You must opt in to *both* a non-loopback bind *and* a token. Either alone
fails at startup:

```sh
TOK=$(openssl rand -hex 32)
tendd --dashboard \
      --dashboard-bind 0.0.0.0:7878 \
      --dashboard-token "$TOK" \
      --allow-public
```

Open `http://<host>:7878/#token=$TOK` in the browser.

If you skip `--allow-public` or `--dashboard-token`, `tendd` refuses to start
with a clear message — same applies to TOML configs.

### Public bind without API auth or editing

For a trusted private network, you can intentionally expose the read-only API
without a bearer token:

```toml
[dashboard]
enabled = true
bind = "0.0.0.0:7878"
allow_public = true
auth = "none"
```

Or via flags/env:

```sh
tendd --dashboard \
      --dashboard-bind 0.0.0.0:7878 \
      --allow-public \
      --dashboard-auth none
```

This exposes all GET/SSE data to anyone who can reach the port. POST control
routes return `403 Forbidden`, so this mode cannot become anonymously writable.
Use it only on a network you already trust.

> ⚠️ **TLS is out of scope.** Use a reverse proxy (nginx, Caddy, Traefik) to
> terminate TLS in front of the dashboard. Set `proxy_buffering off;` for the
> SSE endpoint or live updates will batch.

## Configuration reference

All knobs live under `[dashboard]` in the TOML config. The CLI / env layer
overrides per-field, with **env > CLI flag > TOML > built-in default** (clap
already enforces env-beats-flag for the fields it covers).

| Key                  | Type    | Default            | CLI / env                                                       |
| -------------------- | ------- | ------------------ | --------------------------------------------------------------- |
| `enabled`            | bool    | `false`            | `--dashboard` / `--no-dashboard` / `TEND_DASHBOARD_ENABLED`     |
| `bind`               | string  | `"127.0.0.1:7878"` | `--dashboard-bind` / `TEND_DASHBOARD_BIND`                      |
| `auth`               | string  | `"token"`          | `--dashboard-auth` / `TEND_DASHBOARD_AUTH` (`token`, `public_read`, or `none`) |
| `token`              | string  | unset (required with `token`/`public_read`) | `--dashboard-token` / `TEND_DASHBOARD_TOKEN` |
| `allow_public`       | bool    | `false`            | `--allow-public` / `TEND_DASHBOARD_ALLOW_PUBLIC`                |
| `pane_cache_ttl_ms`  | u64     | `2000`             | (TOML only)                                                     |

## How "global" works

The daemon's agent registry is already global per user — every tmux server
forwards `AgentEvent`s to the same `tendd`. The dashboard's *new* trick is
the pane scanner: it enumerates every tmux socket under `$TMUX_TMPDIR`,
`/tmp/tmux-$UID/`, and (on macOS) `/private/tmp/tmux-$UID/`, runs `tmux -S
<sock> list-panes -a` against each, and folds the results.

Per-socket failures are captured into `errors[]` rather than blanking the
whole list — a wedged tmux server cannot kill the dashboard's view of healthy
ones. Every per-socket invocation has a 1 s timeout.

Results are cached for `pane_cache_ttl_ms` (default 2 s, lazy pull) so a
hammering refresh loop doesn't fork tmux 60 times a minute.

## Work-oriented projection

`tend::work::build_snapshot` is the single projection used by the HTTP and CLI
dashboards. The browser consumes `/api/works`; it does not infer Work from a
session name, a ticket-shaped window name, or repository cwd.

| Concept | Stable identity / source | Dashboard responsibility |
| --- | --- | --- |
| Workspace | `workspace_id` | Scope and aggregate Work progress. |
| Work | `{workspace_id, work_id}` plus durable metadata | Outcome, local stage, goal, next action. |
| External issue | source, provider stable ID, display key, URL, external status | Context/reference only; never the Work primary key or board stage. |
| Run | host/socket/session/window execution identity | One current or previous execution attempt for Work. |
| Agent session | daemon agent session attached to a Run pane | Runtime state, model, prompt/response, control target. |
| Signal | attention, blocked, error | Overlay on Work; never a board lane. |

### Workspace rail

tend's managed layout is Workspace = tmux session, Run = window, Agent = pane,
so the **Workspaces** rail lists every live tmux session, whether or not
anything in it was started with `tend work up`. It is the union of the
sessions in `/api/panes` (plus any that only appear in `unlinked_executions`)
and the managed workspaces in `/api/works`; a managed workspace whose id equals
a session name is the same entry. Each entry shows its window and agent
counts, a rolled-up state (error > waiting > working > idle, as in the
[Sessions navigator](#sessions-windows-and-panes), with a Work's error and
attention signals counted too) and its Work count when it has any. A session
name that exists on several tmux servers is shown as `name · socket`; a managed
workspace with no live session keeps its own entry. The header shows the
number of entries as a badge (its tooltip spells out sessions, managed
workspaces and Works); the **all workspaces** row counts Works and agents. Sort by
**priority** (rolled-up state), **latest** (last agent or Work activity) or
**A-Z**.

This is presentation only: an entry built from a session never creates Work
and never tags a tmux window. Work still only comes from managed runs.

Selecting an entry sets the page **scope**:

- the Work board shows that workspace's managed Works, and **Windows not
  tracked as Work** that session's windows;
- the Sessions navigator tree shows only that session, expanded, with a
  **show all sessions** switch (the filter box still works inside the scope);
- the Agents and Panes tables and the Timeline's workspace filter follow it;
- the Collaboration panel keeps only messages whose sender or recipient pane
  is in that session (panes are mapped to sessions by the live pane scan, and
  a pane that has since closed by the session name the message recorded). A
  participant without a pane — the operator console, an agent outside tmux —
  appears only through a message it exchanged with an in-scope participant.
  The panel header reads **scoped to <name> · clear**. Session and socket
  filters are applied by the API before counting and pagination; a managed
  workspace with no live session keeps the messages filed under its Work.
  Rooms are grouped under their durable session, and selecting a group scopes
  the graph and sequence to it.

Picking a session in the Timeline filter selects its rail entry too, and **all
workspaces** (or ∅, or **clear** in the Collaboration header) clears the scope.
Selecting a session in the navigator also changes the page scope. Selecting
a window or pane only chooses what the navigator's detail side and pane drawer
show. Visitors without the token see the same rail; it carries only metadata.

Scope and detail selection live in separate URL fragment parameters, so a link
restores both:

| Parameter | Set by | Meaning |
| --- | --- | --- |
| `workspace=<name>` | rail | Page scope: a tmux session name, or a managed workspace id with no live session. |
| `wsocket=<socket>` | rail | The tmux server of the scoped session; only written when that session name exists on several servers. |
| `session=<name>` | navigator | Detail selection: a session. |
| `window=<@id>` | navigator, untracked-window rows | Detail selection: a window (with `session=`). |
| `pane=<id>` | pane drawer | The open pane drawer. |
| `socket=<socket>` | navigator, pane drawer | The tmux server of the selected session/window or open pane. |

A link from before the split (`#session=<name>` with no `workspace=`) used to
scope the whole page; on load it is migrated once to the same
`workspace=<name>` (and `wsocket=` from its `socket=`), and the navigator
selection is kept.

When there is no managed Work at all, the six summary cards and the empty board
collapse into one line (with the **start work** form when
`allow_work_start` is on) and the Sessions navigator follows it directly. The
live windows are still listed under a compact **Windows not tracked as Work
(N)** section, collapsed by default in that case (the browser remembers when
you expand it). It follows the rail scope, and each row opens that window in
the navigator (`#window=`).

### Work board

Selecting a workspace filters the four local
Work stages: **Queued**, **In progress**, **Review**, and **Done**. Attention,
blocked, and error render as signal badges on the card so a waiting agent does
not silently rewrite the operator's workflow stage. Selecting a Work opens a
drawer that shows local stage, external issue status, Run state, and Agent state
as separate facts. Expanding execution reveals all linked Runs and panes.

Durable records live in `$XDG_DATA_HOME/tend/dashboard-work.json` (normally
`~/.local/share/tend/dashboard-work.json`). Schema v2 keys records by logical
Work identity. Schema v1 host/socket/session/window annotations are migrated
with their old binding retained for compatibility. Writes remain atomic and
serialized by the daemon.

Managed tmux options (`@tend_workspace_id`, `@tend_work_id`, agent role/task)
link a Run to Work. `tend work up` also stores optional external-source metadata
on the window, which `/api/works` discovers and persists. Unmanaged windows stay
in `unlinked_executions`; they are never guessed into Work from their names.
Their sessions still appear as workspaces in the rail (see
[Workspace rail](#workspace-rail)), but Work only comes from managed runs.

## Collaboration graph and history API

The collaboration panel projects retained requests into two linked views. The
graph uses stable host/socket/agent-session nodes (plus one operator-console
node) and aggregates directed request edges by peer pair, count, kind, and
status; replies appear as dashed reverse edges. Selecting an edge or room
drills the adjacent sequence into chronological request/reply arrows. Selecting
an event opens its thread/parent, Work/Run, artifact, link, message, and
reply details when the response is authorized. The sequence caps the rendered
tail at 200 requests, while **load more** extends the retained result used by
the graph and drill-down.

The toolbar filters by time range, exact room, Work, thread, kind, and status.
The API form is:

```text
GET /api/collaboration?since=7d&session=webapp&socket=%2Ftmp%2Frmux%2Fdefault&workspace=webapp&work=ENG-7345&thread=thread-id&parent=req-id&kind=review&status=completed&limit=100&cursor=...
```

All supplied filters are conjunctive. `session` matches the durable tmux
session name (including its numeric `~view~` aliases); `socket` pins its exact
server endpoint. These filters apply before counts and pagination. `work_id`, `workspace_id`, `thread_id`,
and `parent_request_id` are accepted as aliases for their short names. `room`
is a URL-encoded JSON `RoomId` and matches host, socket, and window exactly.
`limit` defaults to 100 and must be 1–500. Pagination is newest-first keyset
pagination: pass opaque `pagination.next_cursor` back as `cursor`; do not parse
it or replace it with an offset. The response contains `generated_at`,
`details_included`, `requests`, and
`pagination { total, limit, has_more, next_cursor }`. Invalid filters return
`400`; a disabled collaboration subsystem returns `503`.

When `details_included` is false, request/reply/update bodies, provenance, paths,
generic artifacts, and links are omitted. Participant, room,
thread/parent, Work/Run, kind, status, timestamps, and counts remain available
for a useful summary graph. In the Sessions panel, use the Collaboration tab
and select a session or window to filter its history. Unlock details with the
existing dashboard PAT control to read requests, replies, and artifacts. Locking access immediately clears loaded details.

Collaboration opens as a message list beside a detail reader. The list shows
participants, status, timestamps, response latency, update counts, and body
previews for authenticated operators. Search filters retained history on the server before pagination. Public
searches cover visible metadata; authenticated operators can also
search request, reply, and progress bodies. Counts reflect all matching rows,
while summaries of loaded pages are labeled when older pages remain. Custom
period controls accept a start and end time. Graph and Sequence
are separate views; the graph supports zoom and fit in both dimensions.
Drag the separators (or use arrow keys while focused) to resize the navigator,
list/detail split, and panel height. Sizes persist across reloads. Expand fills
the viewport; restore or Escape returns to the normal layout. Narrow screens
stack the list and detail reader and hide the desktop height handle.
Unchanged refreshes preserve text selection and reading position. Thread and
parent buttons open related history across rooms, and message links reopen an
exact retained request even outside the default period. Copy-link strips all
other URL parameters; copy-conversation is available to authenticated operators.

## Fleet hosts

When `[fleet] enabled = true` and `[fleet.hosts]` lists SSH hosts, the
dashboard shows a **Fleet** panel above the session tree: one card per host
(this node first) with its connection state, tend version, relay latency,
agent and pane counts, and how many agents need you (waiting on input or a
choice, or in error). A host that is offline shows the relay's error.
Selecting an online host lists its agents: state, kind, session and pane,
task, and last activity, with the ones that need you first.

The panel reads `GET /api/fleet` every five seconds while it is open, and
stays hidden when the daemon has no fleet or only this node. It follows the
same read policy as `/api/agents`; it is read-only, and
`POST /api/fleet/{host}/command` is not wired to it.

## Sessions, windows and panes

tend tracks every agent by where it runs: a tmux (or rmux) **session**, a
**window** in it, and a **pane** in that window. The **Sessions** panel near
the top of the page is organized the same way.

- **Tree.** Sessions → windows → panes, built from `/api/panes` (which
  carries the socket, session, window and pane ids) joined with the agents
  list by pane id and socket. Each node shows its agent kinds, pane and agent
  counts (`3p · 2a`) and a rolled-up state: the most urgent state below it,
  in the order error, waiting (input or choice), working, starting, idle,
  stopped. With more than one tmux server the top level is the socket. The
  tree is keyboard navigable (↑/↓, → expands or enters, ← collapses or goes
  up, Home/End, Enter selects). The filter box matches session and window
  names, working directories, commands and agent kinds; several words must
  all match along one session → window → pane path.
- **Session view.** Selecting a session lists its windows with their rolled-up
  state. With nothing selected, the panel lists the windows that need
  attention.
- **Window view.** Selecting a window draws its panes the way tmux lays them
  out, from `GET /api/windows/{window}/layout` (tmux's own pane geometry; a
  zoomed window shows only its zoomed pane, and other hosts or a geometry
  that no longer matches the pane list fall back to a plain grid). Each tile shows the pane,
  its agent kind and state, working directory and last prompt; operators also
  see the pane's last 15 lines while the window is shown and the tab is
  visible: up to six tiles (agent panes first) stream live, and any others
  are polled every 3 seconds (one request per pane at a time, four at most).
  Click a tile to open the pane drawer.
- **Agents table.** Still available under Execution inventory; its pane column
  reads `session › window › pane`, and the session and window parts jump to
  the navigator.

While the [workspace rail](#workspace-rail) scopes the page to a session, the
tree shows only that session; **show all sessions** lists every session again
without changing the scope. Selecting a session also scopes diagnostics to
that session; selecting a window remains a detail selection. The selection is kept in
the URL fragment: `#session=<name>`, `#window=<@id>` (with `&socket=<socket>`
when more than one tmux server is running) and `#pane=<id>` for an open
drawer, next to the rail's `#workspace=`, so a link restores all of them.

The layout endpoint is metadata, so `public_read` visitors can read it; the
socket is looked up in the scanned inventory, never used as a caller-supplied
path. Without the token you see the tree and the window layout but no output
tails and no composer.

## Talking to an agent

Click a pane tile in the Sessions panel, an agent in **Execution inventory →
Agents** (or a pane in the **Panes** tab, or a pane row in a Work card's
execution list) to open its drawer. The **agents** chip in the header jumps to that table and shows how
many agents are tracked. Rows are focusable: Tab to one and press Enter.

The drawer shows the pane as `session:window.pane`, the agent kind, state,
model, working directory and last activity, and below that the pane's live
output, streamed live while the drawer is open and the tab is visible (the
header reads `live · changed 5s ago`). Output stays pinned to the newest line unless you scroll up; then a
**jump to latest** chip appears. Type in **Message this agent…** and press
Enter to send it (Shift+Enter for a new line; Enter that confirms an IME
candidate never sends). **abort (Ctrl-C)** asks first. Esc or × closes the drawer.

`#pane=<id>` (plus `&socket=<socket>` when the id exists on several tmux
servers) links straight to a drawer; closing it removes the fragment.

Output and sending are for operators (the token).
Pane output can contain anything the agent printed, including secrets, so
`GET /api/panes/{pane}/output` is gated like a write even though it is a GET:
in `public_read` an anonymous visitor needs the token. Visitors without it
can still open the drawer, but it shows only
the metadata above and never requests output. With `auth = "none"` nobody
can read output. The live stream (below) is gated the same way.

### Live pane output

The drawer and the window tiles read
`GET /api/panes/{pane}/output/stream?socket=&lines=N`, a server-sent event
stream with the same access rules, validation and `Cache-Control: no-store` as
`/output`.

- **One capture loop per pane.** Every viewer of a pane (both tabs, the drawer
  and a tile) shares one loop in the daemon. It runs only while someone is
  connected: it captures the pane every 300 ms, strips escapes with the same
  sanitizer as `/output`, and publishes only when the text changed. It stops
  within one tick after the last viewer leaves. A capture still running when
  the next tick comes skips that tick, so tmux calls never pile up.
- **Events.** `event: output` carries `{ pane, text, captured_at }` with the
  whole bounded text (newest `N` lines, 64 KiB cap), first as a snapshot on
  connect and then on every change to that viewer's lines; an unchanged pane
  sends nothing but a keep-alive comment every 15 seconds. `event: gone`
  (`{ pane }`) means two captures in a row failed, usually because the pane
  closed; it is the last event.
- **Limits.** At most 32 panes stream at once across all clients, and 8
  viewers per pane; beyond that the request gets `429`. A stream closes after
  30 minutes and the client reconnects, so a revoked credential cannot hold a
  stream open.
- **Client.** The dashboard reads the stream with `fetch()` (like
  `/api/events`), so the bearer token travels in a header. It
  reconnects with exponential backoff (0.5 s doubling to 15 s, jittered),
  closes streams while the tab is hidden or the drawer or window is closed,
  and caps a window at six streaming tiles. Against an older daemon that
  answers `404`, it falls back to polling `/output` (1.5 s in the drawer, 3 s
  for tiles).

### Nerd Font glyphs

Shell prompts (powerlevel10k, starship) and agent status lines often print
Nerd Font icons, which live in Unicode's Private Use Areas. The dashboard
bundles a subset of **Symbols Nerd Font Mono**
and lists it after the primary monospace font with a `unicode-range` of
U+E000–U+F8FF and U+F0000–U+FFFFF, so ordinary text keeps your system's
monospace font and browsers fetch the icon font (about 1.2 MB, served from
`/static/fonts/`) only when output actually contains such a glyph.
See [Third-party notices](#third-party-notices).

## Starting work from the board

The board could steer Work it had not started — prompt its Runs, abort them,
annotate it — but not create the team. `POST /api/work-control/up`, and the
board header's **start work** control, close that: give it a stable Work id and,
optionally, a separate external issue key. Tend routes it to a workspace and
directory and creates whichever agent panes the pipeline declares but the Run
does not have yet.
Pressing it twice fills gaps rather than duplicating the team; with a `body` it
also prompts the agents already running. See [PIPELINE.md](PIPELINE.md) for the
`[[route]]` and `[pipeline.*]` configuration it needs.

It is off until you set `[dashboard] allow_work_start = true`, **on top of** the
control token. Every other write route steers a process you already started;
this one starts new ones with permissions bypassed. That is a different kind of
authority, so it gets its own grant — the way `[ask]` and `[collaboration]` do.
Disabled, the route answers `501` rather than `403`, because the token is fine;
the capability simply is not offered here.

The daemon runs the `tend` binary for this rather than reimplementing the
pipeline. The launcher and the managed-tmux registry live in the CLI crate,
which depends on the library the daemon links, so calling them in-process would
mean inverting that dependency or keeping a second implementation alive.

An external issue title and provider status stay in `external_items`; they do
not overwrite the local Work title or stage. The response reports
`linked_external_item`, while the compatibility field `seeded_metadata`
remains false. This keeps provider sync and operator-authored workflow state
independent.

## Timeline

The all-session overview places activity on the same wall-clock axis as the
selected session timeline. Overlapping agent intervals are unioned into rows
for working, waiting, error, idle, and starting; interaction and tmux presence
have their own rows. Gaps remain empty. Totals still report summed agent time
and the shared `tend stats` attribution, so simultaneous agents can accumulate
more work time than the width of a wall-clock interval.

Numeric client-view sessions are folded into their base session in the
workspace rail, pane inventory, diagnostics, and retained session filters.

The timeline panel calls `/api/timeline?since=7d` by default and can switch
to `24h`, `today`, `last week`, `month`, `last month`, `30d`, `12w`, or a clicked `YYYY-MM-DD` calendar day in the
browser. The endpoint also accepts `session=<name>` and `agent=<kind>`
(`codex`, `claude_code`, `gemini_cli`, `antigravity`, `opencode`, `unknown`).
API callers can also pass comma-separated `exclude-pane` / `exclude-session`
case-sensitive globs, for example
`/api/timeline?since=month&exclude-session=monitor*`.
The browser renders a daily contribution-map style heatmap above the lane
graph with ISO-style Monday-first week rows; clicking a day drills the graph
into that local calendar day. Timeline
lanes are grouped by session by default, with agent, human, and tmux
foreground lanes shown under the same session header.

The overview's `act` metric follows the same meaning as `tend stats ACT`:
engaged human time estimated from submitted prompts, tmux input ticks, and
time spent present while agents are waiting for a human answer. Prompt/input
padding is clipped to active presence (tmux foreground, prompt input, or tmux
attach; not a plain open `tend watch` interval). The `human` metric follows
`tend stats HUMAN`: the union of tmux foreground time and tend-recorded human
interaction intervals. Timeline lanes still show raw human-interaction spans
separately as `interaction`.

The workspace sidebar sorts by workflow priority, latest agent activity, or
name. Timeline session filtering remains independent from logical workspace
selection because one repository workspace may contain several physical tmux
sessions.

Closed intervals come from `activity.ndjson`. Currently-open agent states
come from the live `Store` snapshot, and currently-open tmux foreground spans
come from `session-activity.json` when that tracker is enabled.

## Live updates

The frontend opens a single fetch-streamed SSE connection at `/api/events`.
Three event types appear on the wire:

- **`snapshot`** — sent once on connect. Payload: `{ "agents": [...] }`.
  Lets a freshly-loaded page paint without a `/api/agents` round-trip.
- **`transition`** — emitted on every state change. Payload: a serialized
  `Transition`. The client mutates one row in place.
- **`lagged`** — when the broadcast receiver falls behind the server's ring
  buffer. The client refetches `/api/agents` for a clean baseline.

(`EventSource` can't carry `Authorization` headers, so the frontend uses
`fetch()` and parses SSE frames manually. ~30 lines of JS.)

Pane output has its own per-pane stream; see
[Live pane output](#live-pane-output).

## Why the HTML is public

The static HTML/JS/CSS routes intentionally sit *outside* the auth middleware.
Three reasons:

1. **Bootstrap.** Browsers can't inject custom headers on top-level
   navigation. The first GET has to succeed unauthenticated for the JS to
   start, read `#token=...`, and persist it in `localStorage`.
2. **No embedded data.** The bundle holds no agent state. It's a thin client
   that asks `/api/*` for everything. API access follows the selected auth
   mode.
3. **The cost is bounded.** An unauthenticated GET to `/` returns the same
   bytes for everyone. There's nothing to leak.

If this tradeoff doesn't work for you, front the daemon with a reverse proxy
that strips the carve-out (or use mTLS).

## Scope and limits

- External issue references are captured at `tend work up` time. Continuous
  two-way Linear/GitHub/Jira synchronization, dependencies, and estimates are
  not implemented.
- Control is intentionally narrow: Work/pane prompt and abort plus
  Tend-owned PTY input/terminate. There are no direct configuration/file editing
  APIs. Sending a prompt to a shell pane can execute shell commands.
- No mobile UI. The CSS scales OK to ~600 px but isn't designed for phones.
- Operator access uses the bearer token only. The dashboard is a single-user
  tool: OIDC sign-in, viewer accounts and pane sharing were removed.

The [`dashboard::router`](../crates/tend/src/dashboard/server.rs) function is
public for embedding and integration tests.

### Third-party notices

The dashboard embeds `tend-nerd-symbols.woff2`, a subset of Symbols Nerd Font
Mono from [Nerd Fonts](https://github.com/ryanoasis/nerd-fonts) v3.5.1 (MIT,
copyright Ryan L McIntyre). The icon sets it contains keep their own licenses
(Font Awesome and Codicons CC BY 4.0, Material Design Apache 2.0, Pomicons and
Weather Icons SIL OFL 1.1, the rest MIT). The license text, source URL,
checksums and the command that produced the subset are in
[`crates/tend/src/dashboard/web/fonts/`](../crates/tend/src/dashboard/web/fonts/README.md);
the daemon also serves the license at `/static/fonts/LICENSE-nerd-fonts-symbols.txt`.
