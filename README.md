<div align="center">

<img src="assets/logo.svg" alt="muxa" width="260" />

**Agent CLI observability & orchestration layer for tmux.**

See which agents are working, waiting, or idle — right from your status line, or in a full-screen dashboard.

[![CI](https://github.com/Open330/muxa/actions/workflows/ci.yml/badge.svg)](https://github.com/Open330/muxa/actions/workflows/ci.yml)
![MSRV](https://img.shields.io/badge/MSRV-1.88-informational)
![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)
![status](https://img.shields.io/badge/status-beta-yellow)

**English** · [한국어](README.ko.md)

</div>

---

`muxa` is a small daemon that watches agent CLIs — **Claude Code, OpenAI Codex,
Google Gemini CLI** — running inside terminal-multiplexer panes and
surfaces their state to the multiplexer's status line, a live TUI dashboard,
desktop notifications, and a thin CLI.

It does **not** fork the multiplexer. It talks to **tmux** (default, full
support) and **zellij** (CLI baseline; richer metadata via the muxa zellij
plugin) via their respective CLIs, and to each agent via that agent's own
hook / event-emission system. The host is auto-detected at startup or pinned
via `MUXA_HOST=tmux|zellij`.

<div align="center">
  <img src="docs/demo.gif" alt="muxa demo — status table, tmux status-right glyphs, fullscreen watch TUI" width="900" />
</div>

```text
┌─ tmux status-right ──────────────────────────────────────────────────┐
│   ...   │ ⚙ main:2 claude_code │ ! work:1 codex │ · review:0 gemini_cli │
└─────────┼──────────────────────┼────────────────┼──────────────────────┘
          │                      │                └─ idle
          │                      └─ waiting for input
          └─ working
```

> [!IMPORTANT]
> Beta. Event ingest, three adapters (Claude/Codex/Gemini), daemon, CLI, live TUI,
> and desktop notifications work end-to-end with all tests green. APIs are stabilizing
> but minor breaking changes still possible until 1.0. opencode integration deferred.

## Contents

- [Quickstart for Agents](#quickstart-for-agents)
- [Features](#features)
- [Agent support](#agent-support)
- [Install](#install)
- [Quick start](#quick-start)
- [Commands](#commands)
- [Live TUI](#live-tui)
- [Web dashboard](#web-dashboard)
- [Desktop notifications](#desktop-notifications)
- [Companion: oh-my-prompt](#companion-oh-my-prompt)
- [Configuration](#configuration)
- [Architecture](#architecture)
- [Development](#development)
- [License](#license)

## Features

|                          |                                                                                  |
| ------------------------ | -------------------------------------------------------------------------------- |
| **Pan-agent**            | One daemon. One CLI. Three adapters (Claude · Codex · Gemini). opencode integration tracked for post-1.0. |
| **tmux-native**          | Pane correlation via `$TMUX_PANE`; output labelled `session:window.pane`.        |
| **Zero coupling**        | No changes to tmux or to agent CLIs — just their existing hook systems.          |
| **Live TUI**             | `muxa watch` — agents on top, every other tmux pane below, 2 Hz, configurable columns. |
| **Web dashboard**        | Opt-in HTTP UI + SSE — every agent and **every tmux pane on the box**, in one tab. See [`docs/DASHBOARD.md`](docs/DASHBOARD.md). |
| **Desktop alerts**       | Opt-in libnotify / native-toast pings on `WaitingInput` / `Error` transitions.   |
| **External sinks**       | Opt-in fan-out — today: forward prompts to [oh-my-prompt](https://github.com/jiunbae/oh-my-prompt). See [`docs/SINKS.md`](docs/SINKS.md). |
| **Safe by default**      | Socket is `0600`; dashboard is loopback-only until you flip two flags; `SIGTERM` drains; `unsafe_code = forbid`. |
| **Versioned protocol**   | Explicit `PROTOCOL_VERSION`; mismatched clients are rejected.                    |
| **Fast**                 | In-memory registry; no database, no external services.                           |

## Agent support

| Agent               | Integration                                        | Config file                 |
| ------------------- | -------------------------------------------------- | --------------------------- |
| Claude Code         | ✓ shell hooks + status-line Heartbeat              | `~/.claude/settings.json`   |
| OpenAI Codex        | ✓ shell hooks (Claude-protocol clone upstream)     | `~/.codex/config.toml`      |
| Google Gemini CLI   | ✓ shell hooks (Claude-compatible upstream)         | `~/.gemini/settings.json`   |
| opencode            | not yet supported — see [tracking](https://github.com/Open330/muxa/issues/14) | —                           |

## Quickstart for Agents

Paste the prompt below to an AI coding agent (Claude Code, Codex, Gemini
CLI, etc.) running in a tmux pane. It installs `muxa`, wires the active
agent's hook config, and updates `~/.tmux.conf` so this very pane starts
reporting state within seconds.

<div><img src="https://quickstart-for-agents.vercel.app/api/header.svg?theme=claude-code&mascot=thinking&title=install+muxa&lang=Agents" width="100%" /></div>

```text
You're helping install muxa (https://github.com/Open330/muxa), an agent
CLI observability layer for tmux. Requirements: Rust 1.88+, tmux 3.x.

1) Clone and install binaries
   git clone https://github.com/Open330/muxa.git /tmp/muxa
   cargo install --path /tmp/muxa/crates/muxad --locked
   cargo install --path /tmp/muxa/crates/muxa-cli --locked

2) Start the daemon (foreground is fine; detach with `&` or systemd)
   muxad &

3) Wire the CLI you're running under (detect from $0 / process tree)
   - Claude Code: merge /tmp/muxa/examples/claude-settings.json into
     ~/.claude/settings.json. Do NOT overwrite existing hooks — jq-append
     to each hooks.<event> array.
   - Codex:       append the [[hooks.*]] blocks from
                  crates/muxa/src/adapters/codex.rs module doc to
                  ~/.codex/config.toml.
   - Gemini CLI:  merge the hooks block from
                  crates/muxa/src/adapters/gemini.rs module doc into
                  ~/.gemini/settings.json.

4) Wire tmux (append if not already present)
   set -g status-interval 2
   set -g status-right "#(muxa status-line --pane #{pane_id}) | %H:%M"
   tmux source-file ~/.tmux.conf

5) Verify. The current pane should appear in both outputs below.
   muxa status
   muxa status-line --pane $TMUX_PANE

Rollback: every file edited above was backed up to <file>.muxa-backup-<ts>;
kill muxad with pkill, restore backups, tmux source-file to reload.
```

<div><img src="https://quickstart-for-agents.vercel.app/api/footer.svg?theme=claude-code&tokens=0.4k&model=Any+agent" width="100%" /></div>

Prefer to do it yourself? Keep reading.

## Install

Requires **Rust 1.88+**, **tmux 3.x**, and a Unix-y OS.

<details open>
<summary><strong>One-shot (recommended)</strong></summary>

```bash
curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/install.sh | sh
```

Builds + installs `muxa` and `muxad` from source, then hands off to
the `muxa init` wizard for tmux/agent/systemd wiring. Forward args to
the wizard with `sh -s --`:

```bash
# Non-interactive (CI / dotfile bootstrap):
curl -fsSL https://… | sh -s -- --preset standard --yes

# Preview only — write nothing:
curl -fsSL https://… | sh -s -- --dry-run
```

</details>

<details>
<summary><strong>From source</strong></summary>

```bash
git clone https://github.com/Open330/muxa.git && cd muxa
cargo install --path crates/muxad --locked
cargo install --path crates/muxa-cli --locked
muxa init                       # interactive wizard
# muxa init --preset standard --yes   # for headless installs
```

Installs to `~/.cargo/bin/`. Make sure it's on your `PATH`.

</details>

<details>
<summary><strong>Pre-built binaries</strong></summary>

Grab the archive for your platform from the
[Releases page](https://github.com/Open330/muxa/releases), drop `muxa`
+ `muxad` somewhere on your `PATH`, then run `muxa init` to wire up
tmux / agent hooks / systemd / dashboard.

Artifacts are built for:

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`

</details>

### Install wizard (`muxa init`)

```text
┌  muxa init
│
◇  Pre-flight
│  ✔ cargo: cargo 1.92.0
│  ✔ tmux: tmux 3.4
│  ✔ Claude Code config: ~/.claude/settings.json
│  ✔ systemctl --user available
│
◇  What should I set up?
│  ◼ tmux: replace prefix+s with muxa watch popup
│  ◼ tmux: per-pane agent glyphs in status-right
│  ◼ Claude Code: shell hooks + statusLine    ← auto-detected
│  ◻ muxad: systemd user service
│  ◻ Web dashboard: generate token + enable
│
◇  Review changes
│  + edit  ~/.tmux.conf (tmux-popup) [+6 lines]
│  + edit  ~/.tmux.conf (tmux-statusline) [+7 lines]
│  ~ edit  ~/.claude/settings.json (claude-hooks) [+24 lines]
│  ⟳ tmux source-file ~/.tmux.conf
│
└  Done. Try `prefix + s` for the muxa picker.
```

Every edit is wrapped in a per-component marker block
(`# >>> muxa managed (tmux-popup) >>>` … `# <<< muxa managed (tmux-popup) <<<`)
or a command-prefix match (in JSON / TOML), so `muxa init --uninstall`
is a clean reverse — your hand-edited config around our blocks stays
intact. Originals are backed up to `<file>.muxa-backup-<unix_ts>`
before any write.

Common flag combos:

| Goal | Command |
| --- | --- |
| Interactive wizard | `muxa init` |
| Headless / CI | `muxa init --preset standard --yes` |
| Preview only | `muxa init --dry-run` |
| Reverse install | `muxa init --uninstall` |
| Just one thing | `muxa init --component tmux-popup --yes` |
| Preset minus a piece | `muxa init --preset standard --no muxad-systemd --yes` |

Presets: `minimal` (tmux only), `standard` (+ auto-detected agents +
systemd), `full` (+ dashboard).

## Quick start

### 1. Start the daemon

```bash
muxad
```

Or run it as a systemd user service — see [`examples/muxad.service`](examples/muxad.service):

```bash
mkdir -p ~/.local/share/systemd/user
cp examples/muxad.service ~/.local/share/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now muxad.service
```

> Vendor-shipped units live under `$XDG_DATA_HOME` (`~/.local/share/systemd/user/`).
> To override fields without forking the file, drop a same-named unit into
> `~/.config/systemd/user/` — systemd searches that path first.
>
> **Upgrading from an earlier muxa?** If your unit is still at
> `~/.config/systemd/user/muxad.service`, it keeps working as-is. To match
> the current layout, move it once:
> `mv ~/.config/systemd/user/muxad.service ~/.local/share/systemd/user/ && systemctl --user daemon-reload`.

### 2. Wire your agent

Claude Code — merge [`examples/claude-settings.json`](examples/claude-settings.json)
into `~/.claude/settings.json`.

<details>
<summary>Already using <code>ccstatusline</code> (or another statusLine tool)?</summary>

Claude Code only runs a single `statusLine.command`, so you can't stack
muxa on top of `ccstatusline` with the default config. Use `--forward` to
tee the status-line JSON to your existing tool — muxa captures a
Heartbeat (model, context %, cost) out of band, then pipes the same JSON
into the forwarded command and passes its stdout + exit code through
unchanged:

```json
"statusLine": {
  "type": "command",
  "command": "muxa hook claude-statusline --forward 'npx -y ccstatusline@latest'",
  "refreshInterval": 5
}
```

See [`examples/claude-settings-with-ccstatusline.json`](examples/claude-settings-with-ccstatusline.json)
for the full drop-in. The forwarded command runs under `/bin/sh -c`, so
any shell one-liner works. If the daemon is down muxa still forwards
normally — hook paths are best-effort.

</details>

Codex and Gemini CLI follow the same pattern with different config files.
See each adapter's module-level docs in `crates/muxa/src/adapters/`.

### 3. Wire tmux

Append to `~/.tmux.conf` (or `source-file` the drop-in
[`examples/muxa.tmux.conf`](examples/muxa.tmux.conf)):

```tmux
# Agent glyph per pane on the right-hand status
set -g status-interval 2
set -g status-right "#(muxa status-line --pane #{pane_id}) | #[fg=white]%H:%M"

# Replace the stock session-switcher with an agent-aware popup.
# Enter on a row attaches to that pane; popup closes on exit.
bind-key s display-popup -E -w 90% -h 85% "muxa watch"
```

Reload: `tmux source-file ~/.tmux.conf`.

> [!TIP]
> `prefix + s` now opens `muxa watch` as a floating popup — a drop-in
> replacement for tmux's built-in `choose-tree`, with live agent state
> baked in. `prefix + w` still opens the stock window/session tree if
> you need it.

### 4. Confirm

```bash
muxa status         # human-readable table
muxa watch          # live TUI
```

## Commands

|                                            |                                                                        |
| ------------------------------------------ | ---------------------------------------------------------------------- |
| `muxa status`                              | Human-readable table of all tracked agents.                            |
| `muxa watch [--include-paneless] [--view pane\|session]` | Full-screen live TUI — see [Live TUI](#live-tui). Flags override `[watch]` for one invocation. |
| `muxa status-line [--pane %N]`             | One-liner for tmux `status-right`; scoped to `$TMUX_PANE` by default.  |
| `muxa recap [--pane %N] [--limit N\|--all]`| Show recent prompts for the given pane. Pulls from the disk audit log so it survives daemon restarts. |
| `muxa stats [--since 7d] [--group-by day\|project\|agent\|session]` | Summarize retained prompt history, live agents, agent state duration, and tmux foreground time. |
| `muxa report [--since 7d]`                 | Emit a Markdown report with day/project/agent/session breakdowns.       |
| `muxa sync`                                | Backfill the registry by scanning tmux panes — see [Sync](#sync).      |
| `muxa panes`                               | Debug: dump tmux pane inventory.                                       |
| `muxa hook <agent> --event <e>`            | Hook adapter entry point. Invoked by the agent CLIs themselves.        |
| `muxa hook claude-statusline --forward CMD` | Tee Claude's status-line JSON to muxa + a downstream tool.             |
| `muxad`                                    | The daemon. Listens on `$XDG_RUNTIME_DIR/muxa.sock` by default.        |

### Stats and reports

`muxa stats` turns the daemon's retained prompt history plus the live
agent snapshot into a quick local analytics view:

```bash
muxa stats --since 7d --group-by project
muxa stats --since 24h --group-by agent --format json
muxa report --since 7d > muxa-weekly.md
```

`--since` accepts `24h`, `7d`, `4w`, an RFC3339 timestamp, or `all`.
`--format` is `table`, `json`, or `markdown`. The prompt totals are
bounded by `[history].max_per_pane` / `max_age_days`, because muxa keeps
a retained audit log rather than an unbounded warehouse. Duration
columns come from `activity.ndjson`: `WORK`, `WAIT`, and `ERR` track
closed agent state intervals, `TMUX` tracks foreground tmux session
intervals, and `BLOCK` counts transitions into Waiting/Error states.
Live attached tmux sessions are included up to "now"; older
`session-activity.json` totals are used as a legacy fallback until
`activity.ndjson` has foreground intervals.

### Sync

`muxa sync` scans `tmux list-panes`, matches `pane_current_command` against
known agent CLIs (`claude`, `codex`, `gemini` / `gemini-cli`), and asks the
daemon to register them as synthetic agents. The same one-shot pass runs
automatically on `muxad` startup so a daemon restart doesn't blank out
agents that are still alive in their panes. Idempotent: synthetic entries
are replaced in place when a real hook later fires. Toggle via
`[discovery] enabled = false`.

The startup path is layered for richer recovery — synthetic placeholders
are the last fallback, not the primary mechanism:

1. **Hydrate from `state.json`.** The daemon mirrors its in-memory
   registry to disk on every event (debounced; see [State snapshot](#state-snapshot)),
   so a restart restores real `session_id`s, `last_prompt`/`last_response`,
   model + cost metadata for every agent that was alive at shutdown.
2. **Enrich from `prompts.ndjson`.** For panes that hooked a prompt but
   never made it into the snapshot (e.g. the previous run died before
   its first debounce window), the most recent prompt-history entry is
   replayed as a real `Idle` agent so its row is rich on first paint.
3. **Discovery synthesizes placeholders** for whatever's left — panes
   running an agent CLI with no on-disk record at all.

## Live TUI

`muxa watch` is the headline UI: a full-screen dashboard refreshed at 2 Hz,
rendered with [ratatui](https://ratatui.rs). The terminal is restored
cleanly even on panic.

It lists **two row kinds** in one table — the cumulative effect is a
drop-in replacement for tmux's `prefix + s`:

- **Tracked agents** at the top (full color), with kind, state, model,
  prompt, context %, cost, last activity.
- **Untracked tmux panes** below (dim), labelled `session:window.pane`
  with the pane title or current command. These come straight from
  `tmux list-panes -a`, so a freshly-spawned pane shows up on the next
  poll without any muxad knowledge.

Both kinds are selectable. `Enter` on either attaches you to that pane.

The selected row expands to two visual lines: a dim italic `↳ <detail>`
hint underneath, useful for glancing at the agent's last response while
it's in `WaitingInput` without leaving the picker. The detail line is
templated — the default `{last_response|last_prompt}` falls back from
the assistant's reply to the user's prompt, so the row stays useful
both during a turn and after one. Configure via `[watch.detail]` (see
[Configuration](#watch-detail-row)).

Rows are grouped by tmux session, with the most recently active agent
floated to the top of each group. Reorder via `[watch] sort` (see
[Configuration](#watch-sort)) — useful sort keys: `session`, `activity`,
`pane`, `pane_id`.

Prefer one row per tmux session? Run `muxa watch --view session` (or set
`[watch] view = "session"`). All panes in the same tmux session collapse
into one row, represented by the most recently active agent in that
session. In session view, `Enter` jumps to that representative pane and
the `DUR` column shows the cumulative time the session has been foregrounded
by an interactive tmux client.

When the detail line isn't enough — long prompt, multi-paragraph
assistant response — press **`p`** on the selected row to pop open a
centred preview window. The full last prompt + last response render in
a 80% × 70% box over the table, scrollable with the usual `↑`/`↓` /
`PgUp`/`PgDn` / `Home` keys. The popup keeps the surrounding rows
visible behind it so you don't lose context; **`f`** toggles full-
screen for very long content where the popup wraps too aggressively.
`q` / `Esc` / `p` returns to the picker.

By default the preview opens straight into a **live tmux pane
snapshot** — same shape as tmux's `prefix + s` choose-tree preview.
Internally it shells out to `tmux capture-pane -ep`, parses the ANSI
escapes through [`ansi-to-tui`](https://crates.io/crates/ansi-to-tui),
and re-fetches on every refresh tick (≤2 Hz, debounced) so you see
what the pane actually looks like — colors, prompt glyphs, mid-turn
output. **`c`** flips the content to the agent's last prompt + last
response (the text-only view) and back. `f` and `c` are independent
axes: e.g. `f` then `c` gives you a fullscreen live pane view.

To start in the text view instead, set
`[watch.preview] default_content = "prompt_response"` in your
config — `c` still toggles either direction at runtime.

Agents whose pane is unknown — usually Claude Code SDK sub-processes
whose env didn't carry `TMUX_PANE` and whose process-ancestry walk
didn't recover one — are **hidden from the picker by default** because
`Enter` can't attach to them anyway. The footer surfaces a dim
`+N paneless (use --include-paneless to show)` hint so the rows aren't
silently lost. Pass `muxa watch --include-paneless` (or set
`[watch] hide_paneless = false`) to bring them back; when visible they
render `(no pane)` in the PANE column with a yellow
`no tmux pane — attach unavailable` footer hint on selection.

The best way to use it is via a tmux popup (see the
[tmux wiring](#3-wire-tmux) above). Press `prefix + s` from any pane →
popup with the live dashboard → `Enter` on the target row → popup closes
and your client switches to that pane. Press `prefix + s` again to bounce
back.

Run `muxa watch` directly from a bare shell to attach into an existing
tmux session — same Enter semantics, except muxa execs `tmux
attach-session` for you instead of `switch-client`.

**Keybindings**

| Key                   | Action                                                                 |
| --------------------- | ---------------------------------------------------------------------- |
| `↑` / `↓` / `k` / `j` | Move the selection cursor.                                             |
| `Enter`               | Attach to the selected pane (`tmux select-pane` + `switch-client`).    |
| `p`                   | Pop open a centred preview popup of the selected row's prompt + response — useful when the detail line gets truncated. `f` toggles popup ↔ full-screen for very long content; `c` toggles content between prompt/response and a live `tmux capture-pane` snapshot of the actual pane contents (ANSI colors preserved). `q` / `Esc` / `p` returns to the table; `↑` / `↓` / `PgUp` / `PgDn` / `Home` scroll. |
| `r`                   | Force an immediate refresh.                                            |
| `q` / `Esc`           | Quit.                                                                  |
| `Ctrl-C`              | Quit.                                                                  |

## Web dashboard

A read-only HTTP UI bolted onto `muxad`. Same agents you see on the tmux
status line, plus **every tmux pane on the box** across all running tmux
servers, updated live over Server-Sent Events. Off by default; loopback-
only when on by default.

```bash
muxad --dashboard      # then open http://127.0.0.1:7878/
```

Three deployment shapes — pick the one that matches who needs to see it:

| Shape                     | Command                                                                                                                  | URL                                       |
| ------------------------- | ------------------------------------------------------------------------------------------------------------------------ | ----------------------------------------- |
| **Loopback, no token**    | `muxad --dashboard`                                                                                                      | `http://127.0.0.1:7878/`                  |
| **Loopback, token**       | `muxad --dashboard --dashboard-token "$TOK"`                                                                             | `http://127.0.0.1:7878/?token=$TOK`       |
| **LAN / public**          | `muxad --dashboard --dashboard-bind 0.0.0.0:7878 --dashboard-token "$TOK" --allow-public`                                | `http://<host>:7878/?token=$TOK`          |

Non-loopback binds are rejected at startup unless you pass *both*
`--allow-public` and a non-empty `--dashboard-token` — the daemon won't
let you open an unauthenticated socket beyond this host. The `?token=`
query param is captured by the page on first load, stashed in
`localStorage`, and stripped from the URL bar; subsequent requests
attach it as `Authorization: Bearer …`. The token persists across tab
close and browser restart — paste once per browser profile.

Generate a token with `openssl rand -hex 32`. Front with nginx/Caddy if
you need TLS — it's intentionally out of scope.

> **Note:** `MUXA_DASHBOARD_TOKEN` is read at config-load time, in the
> running daemon's environment. If you set the variable in your shell
> but run `muxad` under a systemd service, the systemd unit's
> environment is what counts — set the var via `Environment=` (or
> `EnvironmentFile=`) in the unit file, not in your `~/.bashrc`.

JSON / SSE endpoints (under `/api/*`):

| Method | Path            | What it returns                                                  |
| ------ | --------------- | ---------------------------------------------------------------- |
| GET    | `/api/health`   | `{ ok, version, protocol }`                                      |
| GET    | `/api/agents`   | Current `Store` snapshot.                                        |
| GET    | `/api/panes`    | Global tmux pane list (every readable socket), TTL-cached.       |
| GET    | `/api/events`   | SSE: `snapshot` (initial), `transition` (live), `lagged` (drop). |
| GET    | `/api/metrics`  | JSON counters and timings — see source for full schema (unstable until 1.0). |

Full operator guide — config reference, security model rationale, the
"global tmux" mechanism, and the SSE wire contract — in
[`docs/DASHBOARD.md`](docs/DASHBOARD.md).

## Desktop notifications

Opt in via config. On `*→WaitingInput` and `*→Error`, or `Working→Stopped`
(task complete), `muxa` fires a native notification via
[`notify-rust`](https://crates.io/crates/notify-rust) — DBus / libnotify on
Linux (verified in CI), NSUserNotification / UNUserNotificationCenter on
macOS, WinRT toast on Windows. macOS and Windows paths compile-test only —
please file an issue if your platform misbehaves. Useful when an agent has
been crunching for 10 minutes and finally needs your attention.

Enable in `~/.config/muxa/config.toml`:

```toml
[notifier]
enabled = true
backend = "libnotify"
```

Then restart the daemon.

## Companion: oh-my-prompt

[`oh-my-prompt`](https://github.com/jiunbae/oh-my-prompt) is `muxa`'s
natural counterpart on the history side. Where `muxa` shows what
your agents are doing **right now** — pane state, status-line glyphs,
dashboard tiles — `omp` captures every prompt over time and turns
the history into searchable, scored, project-tagged analytics. Same
four agents on both sides; orthogonal axes.

Forward `muxa`'s prompt events to your `omp` instance with a few
lines of config:

```toml
[sinks.oh_my_prompt]
enabled  = true
endpoint = "https://prompt.jiun.dev"   # or your self-hosted URL
# token is read from $OMP_SERVER_TOKEN by default — never put secrets in TOML
```

Default-off, endpoint required, best-effort with retry/backoff. Full
guide in [`docs/SINKS.md`](docs/SINKS.md).

## Configuration

`muxad` reads `$XDG_CONFIG_HOME/muxa/config.toml` if it exists. All fields
have sensible defaults — see [`config.example.toml`](config.example.toml)
for the full schema. The `muxa` CLI reads the same file (mainly for
`[watch]`); override with `MUXA_CONFIG=…`.

### Watch columns

The `muxa watch` TUI columns are configurable. The default view leads with
the last prompt — `model` / `ctx` / `cost` are opt-in:

```toml
[watch]
view = "pane" # or "session"
# Display order. Omitted keys are hidden.
columns = ["pane", "state", "prompt", "activity"]

[watch.widths]
# Numeric         -> Constraint::Length
# "min:N" string  -> Constraint::Min(N)         (takes leftover space)
# "pct:N" string  -> Constraint::Percentage(N)
pane     = 22
state    = 3
prompt   = "min:20"
activity = 5
# Used automatically by `view = "session"` unless already configured.
session_time = 6
```

Valid column keys: `pane`, `kind`, `state`, `model`, `ctx`, `cost`,
`limits`, `prompt`, `activity`, `session_time`. Unknown keys log a
warning and are skipped — they don't prevent muxa from starting.

The compact `state` column uses markers: `●` working, `?` waiting for
input, `◆` waiting for a choice, `!` error, `·` idle, `…` starting, `×`
stopped.

<a name="watch-sort"></a>
### Sort order

Agent rows are sorted by an ordered list of keys, evaluated left-to-right
with `pane_id` as a final stable tiebreaker. Stale agents (pane already
closed) always sink to the bottom regardless of the sort keys.

```toml
[watch]
# Default: group by session, then float the most recently active agent
# in each group to the top.
sort = ["session", "activity"]

# sort = ["activity"]            # global newest-first, no grouping
# sort = ["session", "pane"]     # tmux-native order within session
# sort = ["pane_id"]             # raw pane id lex asc (screenshot-friendly)
```

Available keys:

| Key       | Effect                                                          |
| --------- | --------------------------------------------------------------- |
| `session` | tmux session name ascending (groups same-session panes)         |
| `activity`| `last_activity_at` descending — most recently updated first     |
| `pane`    | window then pane index, parsed numerically (`10` after `2`)     |
| `pane_id` | raw pane id (`%42`) lexicographic ascending                     |

Unknown keys surface as a parse error — typos don't fail silently.

<a name="watch-detail-row"></a>
### Detail row

The selected row in `muxa watch` expands to a 2-line cell — original
content on top, a dim `↳ <detail>` hint below. Templated via
`[watch.detail]`:

```toml
[watch.detail]
enabled  = true
template = "{last_response|last_prompt}"        # default — response, falling back to prompt
# template = "{last_response}"                  # response only (suppresses until first turn)
# template = "{last_prompt} → {last_response}"  # combined view (heavily truncated)
# template = "{cwd} · {last_prompt}"            # whatever fits your workflow
```

Available placeholders: `pane`, `kind`, `state`, `model`, `ctx`, `cost`,
`activity`, `last_prompt`, `last_response`, `last_notification`, `cwd`.
Unknown placeholders are preserved verbatim so typos surface visually.

Pipe-separated alternatives (`{a|b|c}`) resolve left-to-right and pick
the first non-dash value. The default template uses this to gracefully
fall back from `last_response` to `last_prompt` so the detail row stays
useful for older agents, agents mid-turn, and adapters that don't read
transcripts yet (Codex / Gemini today). When every alternative is empty
the detail line suppresses itself — that's normal for a freshly-
discovered pane with no activity yet, not a bug.

### Prompt history

`muxad` records every `PromptSubmitted` event into a bounded NDJSON
audit log plus an in-memory ring per pane. Powers `muxa recap --all` /
`--limit N` so prompts survive daemon restarts and pane closes
(unlike the live `Agent.last_prompt` field, which gets reaped with the
record).

```toml
[history]
enabled               = true
# path                = "$XDG_DATA_HOME/muxa/prompts.ndjson"   # default
max_per_pane          = 200
max_age_days          = 30
compact_interval_secs = 3600
```

Set `enabled = false` only if you're routing history exclusively
through a sink (e.g. oh-my-prompt) — otherwise you lose `muxa recap`'s
ability to look back after a daemon restart or pane close.

New history entries include `cwd` when the adapter can supply it; `muxa
stats --group-by project` uses that basename for project attribution.

The audit log is chmod 0600 — same posture as the IPC socket, since
prompt content is sensitive.

### State snapshot

`muxad` mirrors its in-memory agent registry to a single JSON file
(`$XDG_DATA_HOME/muxa/state.json` by default) so a restart rehydrates
real `session_id`s, `last_prompt`/`last_response`, and full state +
metadata instead of falling back to discovery's `synthetic-%X`
placeholders.

Writes are event-driven: every `Store::apply` (and every `gc` /
`reconcile`) wakes a writer task via `tokio::sync::Notify`, which
debounces and writes the registry to disk via temp-file + atomic
rename + parent-dir fsync. Idle steady-state produces zero disk
traffic; a tool-heavy turn that fires four events in milliseconds
collapses into one disk write.

```toml
[state]
enabled     = true
# path        = "$XDG_DATA_HOME/muxa/state.json"   # default
debounce_ms = 200
```

The snapshotter is the last task to die on shutdown — `muxad` drains
its in-flight IPC handlers first so events that landed mid-shutdown
make it into the final flush. SIGKILL skips that final flush, but
the per-event debounce-write keeps the on-disk picture within ~200 ms
of reality.

The file is chmod 0600 — same as `prompts.ndjson` and the IPC socket.
Loader is tolerant: missing / corrupt / unknown-schema-version all
warn and fall through to an empty initial state, so a bad state.json
on disk degrades cleanly to the synthetic-placeholder baseline rather
than wedging the daemon.

### Session activity

`muxad` also tracks cumulative tmux foreground time per session. A tmux
session counts as active while an interactive tmux client has that
session foregrounded (`tmux list-clients`, grouped by `client_session`;
control-mode clients are ignored). The rolling total is shown in the
`DUR` column of `muxa watch`'s session view, and closed
foreground intervals are appended to `activity.ndjson` so stats/report
duration survives tmux session deletion.

```toml
[session_activity]
enabled       = true
# path        = "$XDG_DATA_HOME/muxa/session-activity.json"   # default
interval_secs = 5
```

The file is chmod 0600 and is updated on foreground attach/detach edges.
Set `enabled = false` if you do not want muxad polling tmux clients.

### Reconciler

A periodic control loop that converges the in-memory registry against
tmux ground truth. Each pass reaps stale records, drops synthetic
placeholders that lost to a real session, and collapses duplicate rows
for the same pane.

```toml
[reconciler]
enabled       = true
interval_secs = 30
```

Idempotent and safe to run on a timer; the cadence is a tuning knob,
not a correctness one. Disable only when driving reconciliation
externally (e.g. integration tests with a fake `LivenessSource`).

### Zellij support

muxa runs against **tmux** out of the box and **zellij** in a
CLI-baseline mode that doesn't need any extra setup beyond the
`muxa` and `muxad` binaries. Resolution honors `MUXA_HOST` first,
then `$ZELLIJ`, then `$TMUX`.

| Capability                | tmux | zellij CLI | zellij + plugin (planned) |
| ------------------------- | :--: | :--------: | :-----------------------: |
| `muxa status` agent table | ✅   | ✅         | ✅                        |
| `muxa watch` picker       | ✅   | ✅ (no rich pane labels) | ✅ |
| Press-Enter to jump       | ✅   | ✅          | ✅                        |
| Discovery (auto-classify panes by command) | ✅ | ❌ | ✅ |
| Live pane preview (`p`+`c`) | ✅ | ❌ (zellij `dump-screen` requires focus) | ✅ |
| Hook ancestry walk        | ✅   | ❌          | ✅                        |
| Multi-server `/api/panes` | ✅   | n/a (single server) | n/a            |

The unsupported zellij rows aren't broken — they're structurally
gated. `muxa watch` hides the live-preview affordance (`c` is a
no-op) and the auto-discovery synthesizes nothing on zellij CLI;
agents still flow through the regular hook → daemon → status path
because the hook adapter reads `$ZELLIJ_PANE_ID` the same way it
reads `$TMUX_PANE`.

The forthcoming **muxa zellij plugin** (Rust → WASM) closes every
remaining row of the table by subscribing to zellij's plugin-API
`PaneUpdate`/`TabUpdate` events and pushing snapshots to the
daemon. Tracking issue / branch: `feat/zellij-plugin` (not yet
landed).

To pin the host explicitly — useful for nested setups (e.g. zellij
inside tmux, or `tmux new-session` from inside zellij) where
auto-detect picks wrong:

```sh
export MUXA_HOST=zellij   # or "tmux"
muxa watch
```

A starter zellij layout is at
[`examples/muxa.zellij.kdl`](examples/muxa.zellij.kdl).

<details>
<summary>Environment variables</summary>

| Variable       | Purpose                                                |
| -------------- | ------------------------------------------------------ |
| `MUXA_SOCKET`  | Override the unix socket path.                         |
| `MUXA_CONFIG`  | Override the config file path.                         |
| `MUXA_HOST`    | Pin the pane backend: `tmux` or `zellij`. Wins over auto-detect. |
| `RUST_LOG`     | Tracing filter. Example: `muxa=debug,tokio=warn`.      |
| `NO_COLOR`     | Disable ANSI color in `muxa status`.                   |

</details>

## Architecture

```text
agent CLIs (Claude, Codex, Gemini)
      │
      │  shell hook runs `muxa hook <agent> --event <e>`
      │  — stdin JSON, ~1 ms per event
      ▼
    muxad  ───  0600 unix socket  ───  muxa CLI
      │                                  │
      ├── in-memory agent registry       └── status / watch TUI / status-line / recap
      ├── dirty-Notify ──▶ snapshotter ──▶ state.json   (event-driven, debounced, 0600)
      ├── PromptSubmitted ──▶ history   ──▶ prompts.ndjson  (audit log, 0600)
      ├── tmux client sampler ──▶ session-activity.json  (foreground time, 0600)
      ├── state/tmux intervals ──▶ activity.ndjson  (duration ledger, 0600)
      ├── transition broadcast ──▶ notifier task (libnotify / native)
      ├── reconciler (tmux ground truth, idempotent control loop)
      ├── GC task (stopped-agent TTL)
      └── SIGTERM → drain in-flight IPC handlers → snapshotter final flush → unlink socket
```

Restart shape: `state.json` rehydrates first, `prompts.ndjson` enriches
panes the snapshot missed, discovery synthesizes placeholders for the
remainder. The reconciler converges any drift on its first tick.

Three-crate workspace:

- `muxa`     — single library: types, state, config, IPC, tmux wrapper, notifier, adapters, dashboard
- `muxad`    — daemon binary
- `muxa-cli` — CLI binary (`muxa`; ships `status`, `watch` TUI, `sync`, `recap`, `hook`)

See [`PROTOCOL.md`](PROTOCOL.md) for the wire-protocol contract.

## Development

```bash
# build + test + lint
cargo build --workspace
cargo test  --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt   --all
```

CI runs `fmt`, `clippy`, `test` (Linux + macOS), MSRV check, and
`cargo-deny`. See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the full guide —
especially the step-by-step for adding a new agent adapter.

Regenerate the demo GIF (requires [`vhs`](https://github.com/charmbracelet/vhs)):

```bash
vhs docs/demo.tape
```

## License

Dual-licensed under [MIT](LICENSE-MIT) **or** [Apache 2.0](LICENSE-APACHE),
at your option.
