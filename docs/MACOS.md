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
- presents a native Live Watch hierarchy across Fleet hosts;
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
  editor area remains stable. Muxa applies that to **Work**, **Agents**,
  **Hosts**, and **Shells**, with a filter scoped to the active context.
- Lens separates resource navigation from contextual details and supports
  drilling into a resource without flattening infrastructure identity. Muxa
  keeps each fleet agent session independent and treats host, tmux session,
  window, and pane as execution-location metadata rather than Work identity.

The detail area is deliberately task-oriented:

- **Work** exists only for a durable managed run with an explicit
  `workspace_id/work_id`; names observed from tmux do not create fake Work.
- **Agent** opens on a Markdown summary and provides separate
  **Conversation** and read-only live **Shell** tabs. The Shell tab can switch
  between a safe screen capture and an escaped **Raw** byte view.
- **Host** summarizes multi-host connectivity, latency, panes, and the
  independent agent sessions running there.
- **Shell** is the interactive native Ghostty surface owned by a muxad PTY.
  It also has a read-only **Raw** mode for the bounded PTY output stream.

The app now opens on a **Work Command Center**, not a terminal list. **Start
Work** collects the stable Work id, project folder, optional workspace,
pipeline, external issue reference, and initial task. muxad runs the canonical
bundled `muxa work up` implementation as an asynchronous operation, so ticket
resolution cannot freeze refreshes or terminal I/O. The resulting Work opens
as the same logical `{workspace_id, work_id}` used by CLI and dashboard
surfaces.

If no `[[route]]` or pipeline is configured yet, the Start Work sheet does
not strand the user on the CLI error. It offers **Configure Work…**, which
opens the canonical interactive `muxa work init` wizard in a native Shell tab;
after setup, the same sheet can start or dry-run the Work directly.

**Live Watch** is the native replacement for the common `muxa watch` operator
loop. It nests **Host → workspace/session → Work/window → agent/pane**, orders
the local host first, and keeps each host's stable laptop/server badge visible.
Selecting a pane opens an inspector with Markdown summary, exact execution
metadata, prompt control, and a bottom **Live Pane**. The Live Pane starts as a
clearly marked read-only screen preview. Clicking the preview or **Click to
Type** attaches the exact pane in place, swaps the bottom panel to an
interactive Ghostty surface, and focuses it for immediate keyboard input. Raw
bytes are intentionally not exposed in Live Pane. Agents in the sidebar are
likewise grouped by host instead of flattened into one ambiguous fleet list.

Shells and monitored Fleet panes are workspace modules. They participate in
preview/pin tabs and can be detached into independent macOS windows. Attaching
from Live Watch runs the bundled `muxa fleet attach` inside a muxad-owned PTY
and reuses the bottom Live Pane instead of navigating away. **Open in Shell**
remains available when a dedicated Shell tab is useful. A tmux detach returns
the bottom panel to its read-only preview; in a dedicated Shell tab, the same
detach closes the ended tab. This keeps the monitoring safety boundary while
still making exact local and remote panes directly usable.

Selecting a new item opens it as a replaceable preview tab. Pinning preserves
the tab while another Work, Agent, Host, or Shell is inspected. `Command-Shift-P`
opens the workspace command palette, and each sidebar context supports text and
status filters (`All`, `Attention`, `Active`). Attention counts remain visible
on the activity rail even when another context is selected.

Raw Fleet captures are carried as a bounded `capture_raw_base64` field. The UI
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
```

Click coordinates are relative to the captured Muxa window and are rejected if
they fall outside that window.

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
`session_bytes_v1` capability:

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
