# muxa dashboard

A small HTTP UI bolted onto the daemon. It shows the same agents you see on the
tmux status line, a timeline graph of work/wait/error intervals, plus
**every tmux pane on the box** (across all running servers), updated live
over Server-Sent Events. Optional control actions are protected by a bearer
token that can be pasted into the browser like a PAT.

The dashboard is **off by default** and **loopback-only when on by default**.
Token authentication is the default auth mode, and an enabled dashboard must
have an explicit token. There is no path that exposes data beyond your local
machine without explicit public-bind acknowledgement.

## Surfaces

| Path                | What it returns                                                                |
| ------------------- | ------------------------------------------------------------------------------ |
| `GET /`             | The dashboard HTML (loads the JS bundle).                                      |
| `GET /static/*`     | Embedded JS/CSS assets.                                                        |
| `GET /api/health`   | `{ ok, version, protocol }`                                                    |
| `GET /api/access`   | Current read/control access mode and whether the supplied PAT can edit.        |
| `GET /api/agents`   | Current `Store` snapshot.                                                      |
| `GET /api/fleet?selector=...` | Cached physical-host hierarchy, optionally label-filtered.          |
| `GET /api/panes`    | Global tmux pane list (every readable socket), with per-socket scan errors.   |
| `GET /api/terminal-sessions` | Muxa-owned PTY sessions.                                           |
| `GET /api/timeline` | Timeline document from `activity.ndjson` plus currently-open agent/tmux spans. |
| `GET /api/events`   | SSE stream: `snapshot` (initial), `transition` (live), `lagged` (backpressure)|
| `POST /api/panes/{pane}/prompt` | Send and optionally submit text to a pane.                       |
| `POST /api/panes/{pane}/abort` | Send Ctrl-C to a pane.                                             |
| `POST /api/fleet/{host}/command` | Execute a serialized Fleet operation; host mode is rechecked.   |
| `POST /api/terminal-sessions/{id}/input` | Send input to a Muxa-owned PTY.                         |
| `POST /api/terminal-sessions/{id}/terminate` | Terminate a Muxa-owned PTY.                          |

`auth = "token"` protects all API endpoints. `auth = "public_read"` leaves
GET/SSE endpoints public but always requires the bearer token for POST control
actions. `auth = "none"` leaves reads public and disables POST control entirely.
Static routes are public in every mode — see "Why the HTML is public" below.
Fleet routes exist only when `[fleet] enabled = true`; reads reuse the daemon's
per-host cache and never create request-scoped SSH connections. See
[FLEET.md](FLEET.md).

## Quick start

### Recommended: generate and persist a token with `muxa init`

```sh
muxa init --component dashboard
```

The initializer enables the loopback dashboard, generates a token, persists it
in `~/.config/muxa/config.toml`, and prints a one-time bootstrap URL such as
`http://127.0.0.1:7878/#token=...`.

### Manual loopback setup with a token

For a manual setup, provide the token explicitly:

```sh
TOK=$(openssl rand -hex 32)
muxad --dashboard --dashboard-token "$TOK"
# then in the browser:  http://127.0.0.1:7878/#token=$TOK
```

The page captures `#token=...` into `localStorage` and rewrites the URL.
Subsequent requests carry `Authorization: Bearer <tok>`. The token persists
across tab close and browser restart, so you only need to paste it once per
browser profile. To revoke, clear the `muxa.token` key (DevTools → Application
→ Local Storage) or restart `muxad` with a different token.

Legacy `?token=...` URLs are still accepted for compatibility and immediately
scrubbed, but fragments are the supported bootstrap format because browsers do
not send them in HTTP requests or `Referer` headers.

### Loopback-only without authentication (dev / single-user)

Disabling auth requires an explicit opt-out:

```toml
# ~/.config/muxa/config.toml
[dashboard]
enabled = true
auth = "none"
# bind defaults to 127.0.0.1:7878
```

Or via flags:

```sh
muxad --dashboard --dashboard-auth none
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
muxad --dashboard \
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
muxad --dashboard \
      --dashboard-bind 0.0.0.0:7878 \
      --dashboard-token "$TOK" \
      --allow-public
```

Open `http://<host>:7878/#token=$TOK` in the browser.

If you skip `--allow-public` or `--dashboard-token`, `muxad` refuses to start
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
muxad --dashboard \
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
| `enabled`            | bool    | `false`            | `--dashboard` / `--no-dashboard` / `MUXA_DASHBOARD_ENABLED`     |
| `bind`               | string  | `"127.0.0.1:7878"` | `--dashboard-bind` / `MUXA_DASHBOARD_BIND`                      |
| `auth`               | string  | `"token"`          | `--dashboard-auth` / `MUXA_DASHBOARD_AUTH` (`token`, `public_read`, or `none`) |
| `token`              | string  | unset (required with `token`/`public_read`) | `--dashboard-token` / `MUXA_DASHBOARD_TOKEN` |
| `allow_public`       | bool    | `false`            | `--allow-public` / `MUXA_DASHBOARD_ALLOW_PUBLIC`                |
| `pane_cache_ttl_ms`  | u64     | `2000`             | (TOML only)                                                     |

## How "global" works

The daemon's agent registry is already global per user — every tmux server
forwards `AgentEvent`s to the same `muxad`. The dashboard's *new* trick is
the pane scanner: it enumerates every tmux socket under `$TMUX_TMPDIR`,
`/tmp/tmux-$UID/`, and (on macOS) `/private/tmp/tmux-$UID/`, runs `tmux -S
<sock> list-panes -a` against each, and folds the results.

Per-socket failures are captured into `errors[]` rather than blanking the
whole list — a wedged tmux server cannot kill the dashboard's view of healthy
ones. Every per-socket invocation has a 1 s timeout.

Results are cached for `pane_cache_ttl_ms` (default 2 s, lazy pull) so a
hammering refresh loop doesn't fork tmux 60 times a minute.

## Timeline

The timeline panel calls `/api/timeline?since=7d` by default and can switch
to `24h`, `today`, `last week`, `month`, `last month`, `30d`, `12w`, or a clicked `YYYY-MM-DD` calendar day in the
browser. The endpoint also accepts `session=<name>` and `agent=<kind>`
(`codex`, `claude_code`, `gemini_cli`, `opencode`, `unknown`).
API callers can also pass comma-separated `exclude-pane` / `exclude-session`
case-sensitive globs, for example
`/api/timeline?since=month&exclude-session=monitor*`.
The browser renders a daily contribution-map style heatmap above the lane
graph with ISO-style Monday-first week rows; clicking a day drills the graph
into that local calendar day. Timeline
lanes are grouped by session by default, with agent, human, and tmux
foreground lanes shown under the same session header.

The overview's `act` metric follows the same meaning as `muxa stats ACT`:
engaged human time estimated from submitted prompts, tmux input ticks, and
time spent present while agents are waiting for a human answer. Prompt/input
padding is clipped to active presence (tmux foreground, prompt input, or tmux
attach; not a plain open `muxa watch` interval). The `human` metric follows
`muxa stats HUMAN`: the union of tmux foreground time and muxa-recorded human
interaction intervals. Timeline lanes still show raw human-interaction spans
separately as `interaction`.

The session sidebar can sort by `active`, `human`, or `tmux`. `active` uses
the same last-touch attribution as `muxa stats`, so overlapping work across
multiple sessions is counted once and assigned to the most recently touched
session.

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

## What it doesn't do (yet)

- No dashboard-specific database. The daemon rehydrates live state from
  `state.json`, while prompt and activity history use the configured local
  NDJSON files.
- Control is intentionally narrow: pane prompt/abort and Muxa-owned PTY
  input/terminate. Configuration, files, and arbitrary commands are not
  writable through the dashboard.
- No mobile UI. The CSS scales OK to ~600 px but isn't designed for phones.
- No multi-user auth. One token = one bearer.

These are deliberate v1 cuts; the [`dashboard::router`](../crates/muxa/src/dashboard/server.rs)
function is `pub` so a future PR can `.merge()` extra routes without a
rewrite.
