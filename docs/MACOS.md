# Muxa for Mac

Muxa for Mac is a native SwiftUI/AppKit client with a libghostty terminal. It
does not make the app process the owner of shells or agents: `muxad` owns each
PTY and its retained output, while libghostty parses and renders the byte stream
and encodes user input.

## Current product slice

- launches the bundled `muxad` when no compatible daemon is reachable;
- lists muxa-owned native PTY sessions;
- starts a shell without tmux;
- starts or converges configured `muxa work up` pipelines from a native form;
- presents a native Live Watch hierarchy across registered hosts;
- attaches a libghostty surface to an existing session;
- sends input, resize, and attach state over the owner-only Unix socket;
- reconnects terminal output by byte offset;
- keeps the PTY alive when its native window closes; and
- provides both a main window and a menu-bar control.

External tmux, cmux, rmux, herdr, and zellij surfaces remain execution
bindings. A sanitized screen capture is used only for read-only monitoring;
keyboard input is enabled only after Muxa creates an exact interactive attach
session backed by a muxad-owned PTY.

## Workspace information architecture

The native workspace follows two proven desktop navigation patterns:

- VS Code's Activity Bar switches one contextual sidebar at a time while the
  editor area remains stable. Muxa applies that to **Work**, **Explore**,
  **Inbox**, and **Shells**, with a filter scoped to the active context.
  Explore owns topology and host registration, while Inbox owns the operator's
  sent commands, durable replies, global Ask, and agents that require operator
  attention. Explore already uses hosts as its
  roots, so it does not duplicate them in a separate Hosts activity.
- Lens separates resource navigation from contextual details and supports
  drilling into a resource without flattening infrastructure identity. Muxa
  keeps each host's agent sessions independent and treats host, tmux session,
  window, and pane as execution-location metadata rather than Work identity.

The detail area is deliberately task-oriented:

- **Work** exists for a pipeline run or for panes carrying an explicit complete
  `workspace_id/work_id` stamp, including panes started by `muxa work` on
  another host. Session and window names alone never create fake Work.
- **Agent** opens on a Markdown summary and provides separate
  **Conversation** and read-only live **Shell** tabs. The Shell tab shows a
  sanitized screen capture; raw bytes are not exposed there.
- **Host** leads with execution-session cards and their retained agent
  summaries. Selecting a session opens a second resource summary grouped by
  window; connection strings and topology identifiers remain available behind
  a Details disclosure instead of competing with the work context.
- **Window** has its own editor resource. It combines recap, latest response,
  prompt/notification fallbacks, activity and state timestamps, model/context/
  cost, live subagents, process workload, and the window's collaboration
  replies. The small session cards lead with the most urgent/current outcome
  and open this report instead of truncating a multi-agent window into two
  indistinguishable pane rows.
- **Inbox** is an operator queue rather than another agent list. It reads the
  console's sent mailbox once per reachable host, deduplicates requests,
  and exposes waiting, replied, and unread states together with Global Ask
  history. Opening a command returns to its exact agent pane; reading a reply
  uses the durable collaboration get operation.
- **Shell** is the interactive native Ghostty surface owned by a muxad PTY.
  It also has a read-only **Raw** mode for the bounded PTY output stream.

The app now opens on a **Work Command Center**, not a terminal list. Its
**Pipelines** section reads `muxa work options --json` through the bundled
CLI and draws every configured pipeline as a launchable card: the agents are
laid out in the stages muxa will start them in (one column per `after`
level), with the routes that select the pipeline underneath and a **Start…**
button that opens the sheet with that pipeline preselected. The config file
stays the source of truth; the app never keeps its own copy of routes or
pipelines.

**Start Work** collects the stable Work id, project folder, optional
workspace, pipeline, external issue reference, and initial task. Typing a
Work id shows the route it would take (`Route ^cal- → pipeline triad →
workspace callabo → own git worktree`), the workspace field suggests the
route's workspace and existing local sessions, the pipeline is a picker whose
default is the route's choice, the selected pipeline's stage diagram is shown
before launch, and message skills come from `[message.skills]`. The Start
button is enabled only for combinations the CLI would accept. muxad then runs
the canonical bundled `muxa work up` implementation as an asynchronous
operation, so ticket resolution cannot freeze refreshes or terminal I/O. The
resulting Work opens as the same logical `{workspace_id, work_id}` used by
CLI and dashboard surfaces.

Work is not limited to this Mac. Pipelines are one **library**: this Mac's
config is the source, and every fleet host in `control` mode shows a badge
per pipeline (in sync, differs, missing, or unreadable) computed by reading
that host's config through muxad's `work_command` operation. **Sync to
hosts** writes the library definition to the hosts where it is missing or
differs, and pipelines that exist only on a host can be pulled into the
library. Pipelines are small, portable TOML; **routes** are not, because
they carry that host's folders and workspaces, so the Routes editor keeps a
host switcher and edits each host's `[[route]]` table separately. The Start
Work sheet has a host picker: the project folder is then a path on that
host, workspace suggestions come from that host's sessions, and the launch
itself runs `muxa work up` on the host through the daemon while the app
keeps the same asynchronous operation status. A daemon without
`work_command_v1` still serves the local host through the bundled CLI and
says so.

Pipelines are editable in place. **Edit…** on a card (or **New Pipeline…**
and **Design your own…**) opens a visual editor: agents with alias, program,
role, task, split direction, prompt, and `after` toggles, a shared prompt
prefix, and the tmux layout, with the launch-stage picture redrawn as edges
change. Saving goes through `muxa work pipeline set <name> --from-json -`,
so the same validation the CLI applies (allowlisted programs, unique
aliases, acyclic `after` edges) runs before and after the round trip, and
**Delete…** uses `muxa work pipeline remove`. The **Routes** list under the
cards edits `[[route]]` entries (match, pipeline, workspace, folder) through
`muxa work route set` and `route remove`, keeping order and untouched
`worktree`/`prepare` tables as written.

When the config has no pipeline yet, both the Command Center and the sheet
show muxa's built-in presets (`solo`, `pair`, `triad`) with their stage
diagrams. **Install** writes the preset through `muxa work preset apply`
(adding a catch-all route only when no route exists), and **Describe with an
agent…** opens the canonical interactive `muxa work init` wizard in a native
Shell tab for a custom setup. An older bundled CLI without `work options`
falls back to the free-text fields.

**Live Watch** is the native replacement for the common `muxa watch` operator
loop. It nests **Host → Session → Window → Pane**, labels windows with their
logical Work identity when one exists, orders the local host first, and keeps
each host's stable laptop/server badge visible. A one-window session expands
straight to its panes. Selecting a host summarizes its sessions; selecting a
session summarizes each window; selecting a pane opens the working inspector.
The pane Overview leads with the selected agent's Markdown summary and latest
response, keeps the other agents in that window in a compact disclosure, and
moves exact host/session/window/pane identifiers into a Details popover. The
bottom **Live Pane** starts as a clearly marked read-only screen preview and
uses an explicit copy action so clicking text cannot switch renderers or alter
wrapping. **Click to Type** attaches the exact pane in place, swaps the panel to
an interactive Ghostty surface, and focuses it for immediate keyboard input.
Raw bytes are intentionally not exposed in Live Pane.

Shells and monitored panes are workspace modules. They participate in
preview/pin tabs and can be detached into independent macOS windows. Attaching
from Live Watch runs the bundled `muxa fleet attach` inside a muxad-owned PTY
and reuses the bottom Live Pane instead of navigating away. **Open in Shell**
remains available when a dedicated Shell tab is useful. A tmux detach returns
the bottom panel to its read-only preview; in a dedicated Shell tab, the same
detach closes the ended tab. Interactive app attach temporarily applies tmux's
`latest` window sizing and zooms the exact pane, making it follow the Live Pane
viewport instead of retaining a narrow split width. Detach restores the prior
window dimensions, sizing policy, zoom state, and active pane. The controller
applies this over SSH for remote nodes, so it remains compatible with a
host running the previous additive CLI endpoint. This keeps the monitoring
safety boundary while still making exact local and remote panes directly
usable.

Agent summaries, latest responses, collaboration bodies, and Ask answers are
Markdown. Apple's Markdown parser records paragraphs, headings, lists, tables,
and code blocks only as presentation intents that SwiftUI `Text` ignores, so
Muxa rebuilds those block boundaries before display: `MuxaMarkdownText` keeps
a single line-limited `Text` for cards and inspectors, and
`ReadableMarkdownContent` renders full block layouts (including pipe tables)
for the Inbox and Ask conversations. The user-facing vocabulary is **Hosts**;
`fleet` survives only in protocol and type names.

Selecting a new item opens it as a replaceable preview tab. Pinning preserves
the tab while another Work, Agent, Host, or Shell is inspected. `Command-Shift-P`
opens the workspace command palette, and each sidebar context supports text and
status filters (`All`, `Attention`, `Active`). The Inbox activity badge keeps
agent attention, waiting commands, unread replies, and running Ask counts
visible even when another context is selected. Editor tab
close controls and all Explorer disclosure/row targets use full-size hit areas
rather than relying on the visible glyph alone.

Raw pane captures are carried as a bounded `capture_raw_base64` field. The UI
never sends those bytes to a terminal parser or renders control sequences:
`ESC`, `CR`, `LF`, C0, and C1 controls are converted to visible diagnostic
notation. The existing sanitized `capture` field remains the default and older
clients continue to ignore the additive raw field.

This preserves the semantic boundary between a user's logical unit of work,
the collaborators assigned to it, the infrastructure on which they execute,
and the terminal surface used for direct inspection. The patterns are based on
the official [VS Code UI documentation](https://code.visualstudio.com/docs/editing/userinterface)
and [Lens Navigator documentation](https://docs.k8slens.dev/k8slens/using-lens/navigator/).

## Build and run

Requirements:

- macOS 13 or newer;
- Xcode 26 or another Xcode capable of Swift 6;
- `xcodegen` (`brew install xcodegen`);
- Git, curl, tar, and a Rust 1.88+ toolchain.

From the repository root:

```bash
apps/muxa-macos/Scripts/build-app.sh --open
```

The first build downloads source dependencies and can take a while. Zig does
not need to be installed globally: the build downloads the exact compiler
required by the pinned Ghostty release and verifies its SHA-256 digest.

The resulting development application is located at:

```text
apps/muxa-macos/.build/DerivedData/Build/Products/Debug/Muxa.app
```

Build and smoke-test a universal Release app with:

```bash
CONFIGURATION=Release apps/muxa-macos/Scripts/build-app.sh
CONFIGURATION=Release apps/muxa-macos/Scripts/smoke-test.sh
```

Run the launch/daemon/session/renderer smoke test with:

```bash
apps/muxa-macos/Scripts/smoke-test.sh
```

Set `MUXA_SOCKET` before opening from a shell to select a non-default daemon
socket. Otherwise, the app uses `/tmp/muxa-<uid>.sock`, matching muxa's macOS
fallback.

## Headless providers and API keys

Global Ask uses the installed `claude` and `codex` CLIs in their structured
non-interactive modes. The app augments a GUI-launched muxad PATH with
`~/.local/bin`, `~/.cargo/bin`, and Homebrew locations, while preserving
normal Claude Code/Codex CLI sign-in.

When `[ask].enabled` is absent or false, the Ask view shows an explicit
**Enable & Reload** action instead of allowing the first question to fail.
This writes the same opt-in grant as `muxa init --component ask` and reloads
muxad; native PTY sessions require confirmation before replacement, while tmux
sessions remain running. Submit from the composer with **Command-Return** or
the provider-neutral **Send** button.

The Providers sheet can optionally store `ANTHROPIC_API_KEY` or
`CODEX_API_KEY` in the macOS login Keychain. A key is fetched only when its
matching provider is selected and crosses the owner-only muxad socket as a
one-turn credential. muxad puts it in only that child process environment; it
is redacted from debug output and never appears in config, Ask history, logs,
or command arguments.

## Local QA helper

`MuxaQAHelper.app` is an optional, separately signed development tool for
testing the native UI. It is not embedded in or launched by the production
Muxa app. Keeping it separate means Accessibility and Screen Recording access
is granted to the QA tool rather than to Muxa or the terminal process running
the test client.

Build, sign, install, and open the helper with:

```bash
apps/muxa-macos/Scripts/install-qa-helper.sh
```

The installer uses an available Apple Development identity so that the helper
keeps a stable code identity across rebuilds. Override it when necessary with
`MUXA_QA_CODESIGN_IDENTITY`, or install somewhere other than
`~/Applications` with `MUXA_QA_INSTALL_DIR`. The helper requires macOS 14 or
newer because window capture uses ScreenCaptureKit's screenshot API.

In the helper window, choose **Request Permissions**, enable **Muxa QA Helper**
under Privacy & Security > Accessibility and Screen & System Audio Recording,
then relaunch the helper. The permission status updates in the window.

The owner-only local client supports permission checks, window inspection,
capture, and focused test input:

```bash
apps/muxa-macos/Scripts/muxa-qa-helper-client.py status
apps/muxa-macos/Scripts/muxa-qa-helper-client.py inspect
apps/muxa-macos/Scripts/muxa-qa-helper-client.py capture --output /tmp/muxa.png
apps/muxa-macos/Scripts/muxa-qa-helper-client.py type --text 'printf qa-ok' --return
apps/muxa-macos/Scripts/muxa-qa-helper-client.py click --x 600 --y 260
apps/muxa-macos/Scripts/muxa-qa-helper-client.py resize --width 920 --height 580
apps/muxa-macos/Scripts/muxa-qa-helper-client.py key --key , --mod command
apps/muxa-macos/Scripts/muxa-qa-helper-client.py key --key p --mod command --mod shift
apps/muxa-macos/Scripts/muxa-qa-helper-client.py key --key escape
```

Click coordinates are relative to the captured Muxa window and are rejected if
they fall outside that window. `resize` moves and resizes the largest Muxa
window through the Accessibility API, which makes minimum-size and responsive
layouts reproducible; the window's own minimum size still applies and the
resulting geometry is returned.

`key` presses one key with optional modifiers so automated checks can open
Settings (`⌘,`), the command palette (`⌘⇧P`), or close an editor (`⌘W`).
`--key` takes a single character or one of `return`, `escape`, `tab`, `space`,
`up`, `down`, `left`, `right`, `delete`; `--mod` is repeatable and accepts
`command`, `shift`, `option`, `control`. A character is resolved to the key
that produces it on the current ASCII-capable keyboard layout (the layout macOS
uses for menu shortcuts, so `⌘W` still works while an input method such as
2-Set Korean is active), then on the active layout, with a US layout table as
the last fallback; Shift is added when the layout needs it, and the resolved
virtual key code is returned as `key_code`.

The helper listens on `/tmp/muxa-qa-helper-<uid>.sock` with mode `0600`,
rejects peers owned by another user, caps request and text sizes, and always
targets the fixed Muxa bundle identifier (`dev.muxa.mac`). It cannot capture or
focus an arbitrary application. Because any process running as the same user
can reach this QA bridge, quit the helper when UI automation is finished; it is
intended for local development, not distribution.

## Reproducible libghostty supply chain

[`apps/muxa-macos/Dependencies.lock`](../apps/muxa-macos/Dependencies.lock)
pins all non-package-manager source inputs:

- Ghostty release and full Git commit;
- the full Git commit containing the audited host-managed I/O patch set and
  native Swift terminal surface;
- Zig version and both macOS archive checksums.

[`build-libghostty.sh`](../apps/muxa-macos/Scripts/build-libghostty.sh) checks
out those exact commits, verifies that the patch set names the same Ghostty
commit, applies it, builds arm64 and x86_64 static archives, combines them into
Muxa's local `GhosttyKit.xcframework`, and points the local Swift package at
that artifact. The local compatibility patches are included in the cache key,
and the build rejects archives that omit the required Ghostty C ABI. No
release-hosted libghostty binary is linked into Muxa.

The Xcode embed phase builds `muxad` for every architecture in the app target
and also builds the canonical `muxa` CLI used for Work control and Fleet
attach. It combines both binaries when producing a universal app, so the
renderer, process-owning daemon, and execution implementation cannot drift to
incompatible architectures.

Build outputs and source checkouts live under `apps/muxa-macos/.build` and are
not committed.

## IPC contract

The existing newline-delimited JSON control protocol remains compatible with
older clients. Native terminal clients require the additive
`session_bytes_v1` capability. Current clients also prefer the additive
`session_wait_v1` path:

- `read_session` includes `data_base64`, whose decoded bytes correspond
  exactly to `offset..<next_offset`;
- the legacy lossy `data` field remains for older text clients;
- `write_session_bytes` accepts base64-encoded arbitrary input bytes;
- reconnect begins at the last confirmed `next_offset`; and
- previously attached sessions replay retained history with terminal protocol
  responses suppressed, while a never-attached PTY performs normal first-time
  terminal negotiation;
- `truncated: true` explicitly tells the UI that the requested offset fell
  behind muxad's bounded retention window.

`read_session_wait` blocks on the PTY output/exit signal for at most 15
seconds, replacing the former 8–45 ms empty-read loop. MuxaIPCClient assigns
terminal reads their own serialized lane; keyboard input, attachment changes,
and resize control remain responsive while that lane waits.

## Event-driven refresh and render budget

Muxa.app opens compact `fleet_subscribe`, `pipeline_subscribe`, and
`ask_subscribe` streams after its initial coherent load. Host update bursts
are coalesced for 75 ms and capped at four snapshot reads per second. A slow
15-second full reconciliation repairs stream disconnects and lag gaps; it is
not the primary freshness path. Collaboration revisions travel per host, so
the operator inbox and an open Collaborate module re-read only the affected
host rather than polling every mailbox every two seconds. Inbox safety
reconciliation runs at most once per minute while it is visible, and equal
mailboxes do not republish SwiftUI state.

Execution snapshots build the host/session/window/pane tree, agent-to-pane
matches, and selection indexes once at decode time. Work and hosted-agent
projections are rebuilt only when their source snapshot or pipeline runs
change. Equal snapshots do not reassign `@Published` properties or increment
`workspaceRevision`, avoiding an otherwise full SwiftUI text/layout pass.

Live Pane capture remains selected-only because tmux exposes capture as a
snapshot operation rather than an output event. It runs at 750 ms while the
screen changes, backs off through 1.5/3/5 seconds when stable, and suspends
capture while its view or the application is inactive. Only changed text or
error values are published.

Native Work launch uses the additive `work_control_v1` capability. `work_up`
returns a bounded operation immediately, and `work_up_status` reports
`running`, `succeeded`, or `failed` plus the structured canonical CLI result.
At most four launches run concurrently and only the newest 32 operations are
retained. The owner-only socket is the authority boundary; the HTTP
dashboard's separate network-facing `allow_work_start` gate remains unchanged.

The app refuses to open a terminal against an older daemon that does not
advertise this capability instead of silently corrupting terminal traffic. If
an older daemon already owns the selected socket, the app presents **Use
Bundled muxad**. After explicit confirmation it verifies that the socket and
daemon belong to the current user, stops the existing service, and launches the
version embedded in Muxa.app. The confirmation is required because native PTY
sessions owned by the old daemon end during replacement; external tmux sessions
are not terminated.

For a Homebrew-managed daemon, replacement also stops the persistent
`open330/tap/muxa` background service and disables the legacy
`dev.open330.muxad` LaunchAgent so neither can immediately reclaim the socket
with an older binary. Re-enable those services only after their installed
version advertises `session_bytes_v1` (`launchctl enable
gui/$(id -u)/dev.open330.muxad` for the legacy label).

## Process ownership and lifecycle

```text
Muxa.app
├── Work/session UI
├── GhosttyTerminal (VT parser, Metal renderer, input encoder)
└── MuxaIPCClient
        │ byte-safe local IPC
        ▼
muxad
├── PTY child processes
├── retained output and offsets
└── agent / Work / Run state
```

Closing a terminal view only decrements its attached-client count. Termination
requires the explicit Stop button. This separation also means a renderer or app
crash does not kill the user's agent process.

## Upstream and licensing

Ghostty and the Swift embedding sources used by the build are MIT licensed.
Their repositories and exact commits remain visible in `Dependencies.lock`.
The host-managed I/O API is a maintained downstream patch until the equivalent
public upstream embedding API is stable; all calls stay isolated behind
`GhosttyTerminal` and `TerminalPaneModel`.
