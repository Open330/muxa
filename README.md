<div align="center">

<img src="assets/logo.svg" alt="Muxa logo" width="144" />

**Work-oriented AI-agent observability & orchestration for tmux.**

See which agents are working, waiting, idle, or blocked from your tmux
status line, a live TUI, desktop notifications, and local reports.

[![CI](https://github.com/Open330/muxa/actions/workflows/ci.yml/badge.svg)](https://github.com/Open330/muxa/actions/workflows/ci.yml)
![MSRV](https://img.shields.io/badge/MSRV-1.89-informational)
![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)
![status](https://img.shields.io/badge/status-beta-yellow)

**English** · [한국어](README.ko.md)

</div>

---

## Try Muxa onboarding before reading

Launch the complete fullscreen tour. The script runs the real `muxa onboard`
from a temporary copy of the release binary — checksum-verified, deleted on
exit, nothing installed. It needs a supported release platform and network
access; the live tour also needs tmux. Use `muxa onboard --print` for a
non-interactive guide:

```bash
curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/onboard.sh | sh
```

<div align="center">
  <img src="docs/demo.gif" alt="muxa watch: sixteen agent sessions on one screen, the inspector, the swarm view, and muxa attend jumping to the agent that needs you" width="900" />
  <br />
  <sub><code>muxa watch</code> — the fleet, the inspector, the swarm view, and <code>muxa attend</code>.</sub>
</div>

`muxa` is a small daemon and CLI for observing — and now driving — AI
coding agents running inside terminal multiplexer panes. It reads agent
state from existing hook/event systems (Claude Code, OpenAI Codex, Google
Gemini CLI and its Antigravity successor), falls back to screen-manifest
detection for hook-less agents,
and correlates it all with multiplexer panes and sessions. Through `muxa
mcp` a coding agent can also orchestrate the others — inspect state, send
prompts, wait for changes.

It does not fork the multiplexer or modify agent binaries. tmux,
[rmux](https://rmux.io), and [herdr](https://herdr.dev) can be observed at the
same time; zellij has a CLI baseline. See the Hosts table below.

## Optimized for work-oriented tmux

Muxa treats tmux as a work execution adapter, not just a collection of terminal
panes. The durable logical model is Workspace → Work → Run → Agent session:

| tmux object | Muxa meaning | How it is used |
| --- | --- | --- |
| **session** | Workspace execution context | Contains active Run windows for one managed workspace. |
| **window** | One active Work Run | Carries a link to stable Work identity and cwd; closing it ends the Run. |
| **pane** | Agent execution surface | Binds an implementer, reviewer, or helper Agent session to that Run. |

The intended workflow is equally direct:

1. Start a work ID once; Muxa creates or reuses the workspace session, creates
   its work window, and starts the first agent pane.
2. Add implementer, reviewer, or helper agents as additional panes in that
   Work Run. Other Work items use sibling Run windows.
3. Observe, preview, message, and control those agents through `muxa watch`,
   or let an agent use the same policy through `muxa mcp`.
4. Close an agent pane, work window, or whole workspace session explicitly.
   Muxa refuses to terminate unmanaged tmux objects.

An optional Linear/GitHub/Jira issue is a reference attached to Work, not the
Work identity or local board stage. See [the Work domain model](docs/WORK_MODEL.md).
In short: **Workspace → Work → Run → Agent session**, with tmux as the current
binding. Run `muxa onboard` for one continuous safe scenario.
It creates a private throwaway tmux server, daemon, and mailbox, then lets you
type `tmux new-session`, see the hierarchy, prove detach/attach persistence,
start sandboxed agents, and use the real Muxa watch, attend, and messaging
workflow. It never touches your existing tmux server and removes the sandbox on
exit.

> [!IMPORTANT]
> Beta. Event ingest, the daemon, CLI, live TUI, desktop notifications,
> stats, and reports work end-to-end, but APIs may still change before 1.0.

## What You Get

| Surface | What it does |
| --- | --- |
| `muxa status-line` | One-line tmux `status-right` summary for the active pane. |
| `muxa peek` | `prefix + q` overlay: each pane's live screen dimmed under a box with its handle (`@claude`) and tmux pane id, its agent's state, summary, and latest prompt/response — including how long ago you last prompted it and which pane was prompted most recently; press a digit to jump. |
| `muxa watch` | Main TUI for agents, prompts, live previews, hierarchy-aware mailbox history, and table/sequence collaboration. |
| `muxa dashboard` | Work-first TUI console; `P` prompts and `A` aborts every live agent in the selected Work. |
| `muxa attend` | Jump to the agent blocked on input/choice/error longest. |
| `muxa stats` / `muxa report` | Local analytics for prompt history, agent state duration, tmux foreground time, and human thinking time. |
| `muxa timeline` | Full-screen TUI timeline of agent work, waiting, errors, human interaction, and tmux foreground time. |
| `muxa activity` | Raw duration ledger query for debugging exactly what fed stats/report. |
| `muxa automation` | Rules that act on agent state — the built-in one resumes a session after its usage cap resets. |
| BarShelf widget (macOS) | Menu-bar popover summary of active, working, waiting, and error agents. |
| Muxa for Mac | Native session browser and menu-bar app with a locally built libghostty terminal; native PTYs remain owned by `muxad`. |
| Dashboard | Optional loopback HTTP UI with SSE live updates, timeline, and collaboration node-edge/sequence graphs. |
| Notifications | Optional desktop alerts when agents need attention. |

## Install Muxa

If you decide to keep Muxa, install it with one of the following methods.

Requires tmux 3.x (or herdr) and a Unix-like OS.

Homebrew (pre-built binaries, no Rust toolchain needed):

```bash
brew install open330/tap/muxa
muxa init
```

For the Mac app (notarized, updates through Homebrew since it has no
built-in updater):

```bash
brew install --cask open330/tap/muxa-app
```

Or the one-shot installer (builds from source, requires Rust 1.89+):

```bash
curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/install.sh | sh
```

Or from source:

```bash
git clone https://github.com/Open330/muxa.git
cd muxa
cargo install --path crates/muxad --locked
cargo install --path crates/muxa-cli --locked
muxa init
```

Verify:

```bash
muxa daemon start
muxa daemon status
muxa status
muxa watch
```

### Build Muxa for Mac

The native macOS app embeds a libghostty terminal while keeping shells and
agents in `muxad`, so closing the app does not stop them. Ghostty, its Swift
surface, and the required Zig compiler are pinned and verified; Muxa builds its
own local XCFramework instead of consuming a prebuilt terminal binary.

```bash
brew install xcodegen
apps/muxa-macos/Scripts/build-app.sh --open
```

After building, run `apps/muxa-macos/Scripts/smoke-test.sh` for a
non-interactive product smoke test.

See [Muxa for Mac](docs/MACOS.md) for the architecture, build inputs, and IPC
contract.

### Collaborate from `muxa watch`

<div align="center">
  <img src="docs/demo-collab.gif" alt="muxa watch: the collaboration mailbox, messaging the session under the cursor, and asking claude a headless question" width="900" />
  <br />
  <sub><code>b</code> mailbox · <code>m</code> message the row under the cursor · <code>a</code>/<code>A</code> headless ask.</sub>
</div>

The model is simple: **one tmux window is one room**. Interactive watch and
dashboard messages are sent by the operator console, so either popup can be
opened from an agent or a spare shell pane. Agent-initiated MCP and `muxa msg`
requests still use that agent's pane identity.

One-time setup: add the following to `~/.config/muxa/config.toml`, restart
`muxad`, and run `muxa init` to install the `prefix+s` watch popup.

```toml
[collaboration]
enabled = true
wake = "idle_only"
# Default: deliver operator messages directly; keep agent messages as notices.
wake_payload = "operator_full"
```

Install shared collaboration instructions, the symlinked skill, and MCP for
detected Codex and Claude Code installations, then restart running agents.
The standard preset includes this setup.

```bash
muxa init --component agent-instructions,agent-skills,agent-mcp
```

Init adds Codex's required pane environment forwarding to `[mcp_servers.muxa]`
while preserving existing variables and other settings:

```toml
env_vars = ["RMUX", "RMUX_PANE", "TMUX", "TMUX_PANE", "MUXA_SOCKET"]
```

Muxa also recovers the pane from process ancestry across active pane backends
for existing default-endpoint Codex registrations, so older setups fail safely
rather than appearing paneless.

See [Global agent integration](docs/AGENT_INTEGRATION.md) for canonical paths,
updates, and removal; [MCP setup](docs/MCP.md) also covers manual registration.

Connected agents are told that room peers can serve as read-only reviewers or
narrowly scoped execution subagents. Requests and replies can also carry
validated AIR 1.0 artifact references, which watch visualizes with
profile-colored mailbox badges.

You can call a colleague directly from a connected Claude or Codex
conversation. The agent maps the mention to Muxa's durable peer-call tool:

```text
@peer review the current changes
@codex /review-plan-feedback using commit abc123 as context
@peer's report: summarize it and apply only valid advice
```

`@peer` and `@muxa-peer` are reserved for Muxa collaboration. New requests use
the peer-call tool; references to an existing peer report use the durable
mailbox report tool and never imply a GitHub PR without an explicit PR reference.
`@peer` chooses a healthy same-window agent deterministically; `@claude`,
`@codex`, `@gemini`, `@alias`, and `role:name` narrow the target. Calls default
to `REVIEW · READ-ONLY`. Executing changes requires an explicit task
authorization, and creating a new agent pane requires a separate confirmation.
Restart an already-running agent after upgrading Muxa or changing registered
skills so its MCP process loads the new tool and templates.

Agents reported as synthetic by `muxa doctor` are omitted from collaboration
until a hook event establishes a stable session identity. Submit a prompt or
restart that agent, then check again.

Then:

1. Run two agents in two panes of the same tmux window.
2. Press `prefix+s` from any pane and select the recipient's session, window,
   or pane. Parent rows resolve to the lowest numeric live agent and show the
   exact target in the composer title.
3. Press `m`, type the request, and press `Enter`. At any point in the draft,
   `/` opens reusable skills registered with `muxa skill add`; selection inserts
   at the cursor and a second `Enter` sends. Press `M` to read and reply from
   the mailbox (`b` remains an alias).

`M` on a window combines its room; on a session it combines all windows and
groups the read-only history by window. On the collaboration screen, `v`
toggles the newest-first table and chronological sequence (`muxa watch
--screen collab --collab-layout sequence` starts there directly). The web
dashboard adds a cross-room node-edge graph, sequence drill-down, filters, and
cursor pagination. Durable history is indexed in `collaboration.sqlite3`; an
existing `collaboration.json` is imported once and retained as a migration
backup until you remove or archive that duplicate copy.

For request/reply details, see
[docs/COLLABORATION.md](docs/COLLABORATION.md); for skill registration and
composer controls, see [docs/WATCH.md](docs/WATCH.md).

For install modes, `muxa init` presets, systemd, manual hook wiring, and
rollback details, see [docs/INSTALL.md](docs/INSTALL.md).

## Core Commands

Managed tmux policy binds a workspace context to a session, an active Work Run
to a window, and an Agent session to a pane. `muxa onboard` teaches that mapping
in sixteen live steps on a private sandbox. You create a real session and
window, inspect the tree, detach and reattach, split a pane and let the tour
bring up two scripted agents, then use real `muxa watch` — including the
`j`/`k`, `Enter` and `?` keys it is driven with — `muxa attend`, and `muxa msg`
against a real sandbox mailbox. The narration observes tmux and Muxa state; it
does not intercept your keys. Korean is selected automatically for a Korean
locale, can be requested with `--lang ko`, and can be toggled with `F2` during
the tour. `--print` emits the same sixteen-step workflow without starting tmux.

| Command | Purpose |
| --- | --- |
| `muxa status [--json]` | Human-readable table, or a versioned JSON snapshot for desktop integrations. |
| `muxa watch [--view session\|window\|pane]` | Live workspace → work → agent TUI picker/dashboard. |
| `muxa dashboard [--since today]` | Work-card TUI with Run capture, per-agent and Work-wide prompt/abort actions, ACT/WACT totals, and collaboration controls. |
| `muxa attend [--cycle] [--list]` | Focus or list agents needing attention. |
| `muxa status-line [--pane %N]` | tmux status-line output. |
| `muxa peek [--plain]` | Per-pane overlay for the current tmux window; `--plain` prints it as text. The overlay is a tmux popup, so where nothing can draw one — a front-end that attaches no client (cmux), or a control-mode client (`tmux -CC` — amux, iTerm2) — peek prints the text report instead and says why on stderr. |
| `muxa recap [--pane %N]` | Recent prompts from retained disk history. |
| `muxa peers` / `muxa identity` / `muxa msg` | Discover and name same-window agents, then exchange durable request/reply messages. |
| `muxa skill add/list/show/remove` | Manage reusable `/` prompt templates for watch/dashboard messages, watch ask, and MCP peer calls. |
| `muxa host add/list/label/annotate/doctor` | Manage physical SSH nodes and Kubernetes-style labels/annotations. |
| `muxa fleet status/watch/capture/send/attach` | Central host → session → window → pane(agent) observation and control. |
| `muxa stats --since today` | Focused WACT/ACT/WORK/WAIT summary; group by day/project/agent/session. Add `--graph` for graph-only WACT over time or `--verbose` for diagnostic columns. |
| `muxa report --since week` | All breakdowns (day/project/agent/session) as focused ACT/WACT tables; add `--json` or `--markdown` to export. |
| `muxa timeline --since today` | Interactive session-grouped timeline; filter with `--session main` / `--agent codex`, sort with `--sort waiting`, or use `--view heatmap`. |
| `muxa activity --type agent\|tmux\|human` | Raw activity ledger intervals. |
| `muxa sync` | Backfill the registry by scanning active pane hosts. |
| `muxa register --name X [--pid N]` | Surface an arbitrary background process (script, game, automation loop) as a pid-tracked row in `muxa status`. |
| `muxa run --detach --name X -- <cmd>` | Run a command in a muxa-owned PTY; it also appears in `muxa status` as a task. |
| `muxa agent start --agent codex [--host auto\|native\|tmux]` | Start an allowlisted agent. `auto` uses tmux inside tmux and a muxa-owned PTY in a plain terminal. |
| `muxa work init` | Describe a work pipeline in your own words; an agent writes the `[ticket]`/`[[route]]`/`[pipeline.*]` config, validated and shown before anything is written. |
| `muxa work compose "implementer in claude, reviewer in codex after it"` | Draft one pipeline from a description without writing anything: the reply is the same JSON `muxa work pipeline set --from-json` accepts, validated like it, so Muxa.app can show it for review first. |
| `muxa ask providers` / `muxa ask provider add anthropic-work --engine anthropic` | List the Ask providers with what each needs, and compose the list: an instance names its engine (Claude Code, Codex, Gemini CLIs; Anthropic, OpenAI APIs), its model, and its own key variable, so several keys for one engine can coexist. |
| `muxa automation list` / `muxa automation test resume-after-limit` | Inspect the rule engine and see what a rule would do right now without firing it. A rule reacts to an agent event (a session limit, a wait for input, an idle stretch) after a delay, with guards that a rule cannot opt out of. See [docs/AUTOMATION.md](docs/AUTOMATION.md). |
| `muxa config show` / `muxa config set --from new.toml` | Read and replace the daemon's `config.toml` through muxad, which parses and validates before writing. Muxa.app's Settings › Advanced edits the same file the same way. |
| `muxa work preset apply solo --route '.*'` | Write a built-in pipeline (`solo`, `pair`, `triad`) into `config.toml` without an agent turn; `muxa work options --json` prints the routes, pipelines, skills, and presets a launcher can offer. |
| `muxa work up cal-1234 --body "..."` | Resolve the ticket, route it to a workspace, and create whichever pipeline agent panes are missing — delivering the request to the ones already running. Re-running converges; also `muxa_start_work` over MCP. See [docs/PIPELINE.md](docs/PIPELINE.md), and [docs/AIR.md](docs/AIR.md) for how a pipeline maps onto the AIR interchange format. |
| `muxa work start muxa-onboarding --workspace muxa --agent codex ...` | Create/reuse workspace session `muxa`, create/reuse its work window, and add an agent pane. |
| `muxa workspace list/show/view/close` | Inspect, give the current terminal an independent grouped view of, or explicitly close workspace/project sessions. |
| `muxa window rename NAME` | Give a tmux window a stable normalized name, or restore process-based naming with `--auto`. |
| `muxa work list/show/close [--workspace muxa]` | Inspect Work and its current Run binding, or explicitly close that Run window. |
| `muxa agent start --host tmux --workspace muxa --work muxa-onboarding ...` | Add an allowlisted agent pane to one managed tmux Work window; also exposed as MCP `muxa_start_agent`. |
| `muxa agent control (--pane %N\|--session pty-N) --action interrupt` | Interrupt or explicitly terminate one managed tmux pane or muxa-owned PTY agent session. |
| `muxa onboard [--tour live] [--lang auto\|en\|ko]` | Sixteen live steps on a throwaway muxa: real tmux, watch, attend, and mailbox. Refuses to nest inside an existing tmux session. `F2` switches language, `--no-quiz` offers `F12` immediately, and `--print` emits the written guide. |
| `muxa automation list/test/log` | Rules that watch agent state and act on it — resume a session once its usage cap resets, nudge an idle agent, interrupt a stuck one. Ships enabled with no rules; `pause`, `cooldown`, per-rule hourly caps, a fire-time re-check, and a durable ledger keep it from running away. See [docs/AUTOMATION.md](docs/AUTOMATION.md). |
| `muxa mcp` | MCP stdio server so a coding agent can orchestrate muxa — inspect agents, send prompts, capture panes, wait for changes (`claude mcp add --scope user muxa -- muxa mcp`, see [docs/MCP.md](docs/MCP.md)). |
| `muxa init` | Interactive install/uninstall wizard. |
| `muxad` | Daemon process. |

Common stats queries:

```bash
muxa stats --since today --group-by session
muxa stats --since yesterday --group-by project
muxa report --since last-week
muxa timeline --since today --session main
muxa timeline --since today --exclude-session 'monitor*'
muxa stats --since month --exclude-pane '%42' --exclude-session 'monitor*'
muxa timeline --since today --group-by kind --sort waiting
muxa timeline --view heatmap --since 12w
muxa timeline --day 2026-06-06
muxa activity --since today --type human
```

`--since` accepts `today`, `yesterday`, `week` for a rolling 7-day window,
`month` for a rolling 30-day window, `last-week` / `"last week"` for the
previous Monday-Sunday calendar week, `last-month` / `"last month"` for the
previous calendar month, rolling durations like `24h`/`7d`/`4w`, local dates
like `2026-06-06`, RFC3339 timestamps, and `all`. See
[docs/ACTIVITY.md](docs/ACTIVITY.md) for ledger semantics, including
`HUMAN`, `THINK`, and `ACT`.

`muxa stats`, `muxa report`, and `muxa timeline` also accept
`--exclude-pane` and `--exclude-session` for long-lived monitoring scopes.
Patterns are case-sensitive and support `*` and `?`, e.g.
`--exclude-session 'monitor*'`.

## Supported Agents

**Hook-based (authoritative).** These wire into their existing hook/event
systems, so muxa gets exact state transitions:

| Agent | Status | Config |
| --- | --- | --- |
| Claude Code | Supported | `~/.claude/settings.json` |
| OpenAI Codex | Supported | `~/.codex/config.toml` |
| Google Gemini CLI | Supported | `~/.gemini/settings.json` |
| Google Antigravity CLI (`agy`) | Supported | `~/.gemini/config/hooks.json` — [details](docs/ANTIGRAVITY.md) |
| opencode | Planned | [tracking issue](https://github.com/Open330/muxa/issues/14) |

**Screen-detected (fallback).** Agents with no hooks are classified from
their pane contents via TOML manifests — bundled for `agy`, `cursor-agent`,
`amp`, `copilot`, `aider`, and `goose`, extensible per user. Hooks win when
present, with one carve-out: `agy` fires no hook for an approval prompt, so its
panes stay screen-inferred for that one signal. See
[docs/SCREEN_DETECTION.md](docs/SCREEN_DETECTION.md).

On [herdr](https://herdr.dev) hosts, muxa additionally surfaces every
agent herdr's own detection sees, with no manifest needed.

## Fleet Hosts

Run muxad locally as a central controller for this machine and several
SSH-reachable machines. The controller appears immediately as the `local`
node, without Fleet configuration. Each physical node keeps its own stable UUID and label/annotation metadata;
the controller maintains one persistent outbound SSH stdio relay and an
independent last-known cache per node. `observe` is the default, while
`control` must be granted per host. No remote TCP listener is opened.

```bash
muxa fleet status                         # local is already present
muxa fleet status -L environment,region   # opt-in label columns
muxa fleet status -o wide                 # hostname/version/latency when space permits
muxa host label local environment=development
muxa host add dev muxa-devbox --label environment=development --mode observe
muxa host doctor dev
muxa fleet watch
# equivalent entry point: muxa watch --fleet
# `muxa init` also binds this view to tmux prefix+S (local watch stays prefix+s)
# with only local, this is the full native watch with no redundant host row
```

See [docs/FLEET.md](docs/FLEET.md) for selectors, TUI controls, security,
performance, MCP tools, and dashboard APIs.

## Pane Backends

muxa observes agents across terminal-multiplexer backends and can watch
several at once (e.g. during a tmux→herdr migration):

| Host | Status | Notes |
| --- | --- | --- |
| tmux | Full | The default backend. |
| [rmux](https://rmux.io) | Initial CLI backend | Pane discovery, capture, focus, and targeted input; see [docs/RMUX.md](docs/RMUX.md). |
| [herdr](https://herdr.dev) | Full | Via herdr's socket API; see [docs/HERDR.md](docs/HERDR.md). |
| zellij | CLI baseline | Richer plugin path planned; see [docs/ZELLIJ.md](docs/ZELLIJ.md). |

See [docs/MULTI_HOST.md](docs/MULTI_HOST.md) for observing multiple hosts
simultaneously.

## More Docs

| Topic | Doc |
| --- | --- |
| Install and wiring | [docs/INSTALL.md](docs/INSTALL.md) |
| Onboarding and work/agent policy (한국어) | [docs/ONBOARDING.ko.md](docs/ONBOARDING.ko.md) |
| Work pipelines (`muxa work up`) | [docs/PIPELINE.md](docs/PIPELINE.md) · [한국어](docs/PIPELINE.ko.md) |
| MCP control plane (`muxa mcp`) | [docs/MCP.md](docs/MCP.md) |
| herdr host support | [docs/HERDR.md](docs/HERDR.md) |
| rmux host support | [docs/RMUX.md](docs/RMUX.md) |
| Multi-host observation | [docs/MULTI_HOST.md](docs/MULTI_HOST.md) |
| Physical SSH fleet | [docs/FLEET.md](docs/FLEET.md) |
| Antigravity CLI (`agy`) support | [docs/ANTIGRAVITY.md](docs/ANTIGRAVITY.md) |
| Screen-manifest detection | [docs/SCREEN_DETECTION.md](docs/SCREEN_DETECTION.md) |
| Live TUI and prompt composer | [docs/WATCH.md](docs/WATCH.md) |
| CLI dashboard | [docs/DASHBOARD_CLI.md](docs/DASHBOARD_CLI.md) |
| Stats, reports, activity ledger | [docs/ACTIVITY.md](docs/ACTIVITY.md) |
| Timeline TUI and dashboard graph | [docs/TIMELINE.md](docs/TIMELINE.md) |
| Configuration reference | [docs/CONFIGURATION.md](docs/CONFIGURATION.md) |
| Web dashboard | [docs/DASHBOARD.md](docs/DASHBOARD.md) |
| External sinks | [docs/SINKS.md](docs/SINKS.md) |
| Zellij plan | [docs/ZELLIJ.md](docs/ZELLIJ.md) |
| Architecture and development | [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) |
| Agent collaboration | [docs/COLLABORATION.md](docs/COLLABORATION.md) |

## Development

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

## License

MIT OR Apache-2.0.
