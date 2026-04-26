# muxa dashboard

A small read-only HTTP UI bolted onto the daemon. Same agents you see on the
tmux status line, plus **every tmux pane on the box** (across all running
servers), updated live over Server-Sent Events.

The dashboard is **off by default** and **loopback-only when on by default**.
Production-ready means default-secure: there is no path that exposes data
beyond your local machine without you flipping two flags.

## Surfaces

| Path                | What it returns                                                                |
| ------------------- | ------------------------------------------------------------------------------ |
| `GET /`             | The dashboard HTML (loads the JS bundle).                                      |
| `GET /static/*`     | Embedded JS/CSS assets.                                                        |
| `GET /api/health`   | `{ ok, version, protocol }`                                                    |
| `GET /api/agents`   | Current `Store` snapshot.                                                      |
| `GET /api/panes`    | Global tmux pane list (every readable socket), with per-socket scan errors.   |
| `GET /api/events`   | SSE stream: `snapshot` (initial), `transition` (live), `lagged` (backpressure)|

`/api/*` endpoints require a Bearer token if one is configured. The static
routes do not — see "Why the HTML is public" below.

## Quick start

### Loopback-only, no token (dev / single-user)

```toml
# ~/.config/muxa/config.toml
[dashboard]
enabled = true
# bind defaults to 127.0.0.1:7878
```

Or via flags:

```sh
muxad --dashboard
```

Open <http://127.0.0.1:7878/>.

### Loopback with a token

You don't need a token for loopback (it's already gated by the kernel). If
you want one anyway, e.g. for a multi-user box:

```sh
TOK=$(openssl rand -hex 32)
muxad --dashboard --dashboard-token "$TOK"
# then in the browser:  http://127.0.0.1:7878/?token=$TOK
```

The page captures `?token=...` into `sessionStorage` and rewrites the URL.
Subsequent requests carry `Authorization: Bearer <tok>`.

### Public bind (LAN / VPN)

You must opt in to *both* a non-loopback bind *and* a token. Either alone
fails at startup:

```sh
TOK=$(openssl rand -hex 32)
muxad --dashboard \
      --dashboard-bind 0.0.0.0:7878 \
      --dashboard-token "$TOK" \
      --allow-public
```

If you skip `--allow-public` or `--dashboard-token`, `muxad` refuses to start
with a clear message — same applies to TOML configs.

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
| `token`              | string  | `""` (no auth)     | `--dashboard-token` / `MUXA_DASHBOARD_TOKEN`                    |
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

1. **Bootstrap.** `<script src="?token=...">` doesn't fly — browsers can't
   inject custom headers on top-level navigation. The first GET has to
   succeed unauthenticated for the JS to even *start* and pick the token up.
2. **No data.** The bundle holds no agent state. It's a thin client that
   asks `/api/*` for everything. Those endpoints stay gated.
3. **The cost is bounded.** An unauthenticated GET to `/` returns the same
   bytes for everyone. There's nothing to leak.

If this tradeoff doesn't work for you, front the daemon with a reverse proxy
that strips the carve-out (or use mTLS).

## What it doesn't do (yet)

- No persistence — daemon restart drops state. Prompt history goes to a sink
  like [oh-my-prompt](https://github.com/jiunbae/oh-my-prompt), not muxa.
- No write API. Read-only dashboard. CSRF moot, but also no "stop this agent"
  button.
- No mobile UI. The CSS scales OK to ~600 px but isn't designed for phones.
- No multi-user auth. One token = one bearer.

These are deliberate v1 cuts; the [`dashboard::router`](../crates/muxa/src/dashboard/server.rs)
function is `pub` so a future PR can `.merge()` extra routes without a
rewrite.
