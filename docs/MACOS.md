# Tend for Mac

Tend for Mac is a native SwiftUI/AppKit client with a libghostty terminal. It
does not make the app process the owner of shells or agents: `tendd` owns each
PTY and its retained output, while libghostty parses and renders the byte stream
and encodes user input.

## Current product slice

- launches the bundled `tendd` when no compatible daemon is reachable;
- lists tend-owned native PTY sessions;
- starts a shell without tmux;
- starts or converges configured `tend work up` pipelines from a native form;
- presents a native Live Watch hierarchy across registered hosts;
- attaches a libghostty surface to an existing session;
- sends input, resize, and attach state over the owner-only Unix socket;
- reconnects terminal output by byte offset;
- keeps the PTY alive when its native window closes;
- saves and restores workspace snapshots (Explore "…" → Save Snapshot /
  Restore Snapshot…) through tendd's `mux_snapshot_v1` requests, which run
  `tend snapshot` / `tend restore --only-missing --json`; the restore
  preview compares the snapshot with what runs now, and with
  `mux_snapshot_plan_v1` "Only missing sessions" can be turned off to also
  fill in running sessions, and with `mux_snapshot_continue_v1` agents the
  snapshot caught mid-turn can be told to `continue` once resumed; and
- provides both a main window and a menu-bar control.

External tmux, cmux, rmux, herdr, and zellij surfaces remain execution
bindings. A sanitized screen capture is used only for read-only monitoring;
keyboard input is enabled only after Tend creates an exact interactive attach
session backed by a tendd-owned PTY.

## Workspace information architecture

The native workspace follows two proven desktop navigation patterns:

- **Home** is the first screen and sits at the top of the activity rail, with
  a badge counting the units waiting for you. It shows Work, not agents: each
  unit of work (a solo agent counts as a one-stage unit) is a card in one of
  three lanes, **Your turn**, **In progress** and **Finished**, grouped by
  project, with an inspector (answer a waiting agent, send work back, mark it
  **Looks Good**), **Start Work**, and the Review sheet (⇧⌘R). The Work list has no rail entry of its own; Home manages Work units,
  and the list column folds while Home or a Work page is focused.
- **Your turn** also holds the daemon's decision queue (`decision_queue_v1`,
  see [REVIEW](REVIEW.md#one-queue)): questions agents asked the person
  (`tend_request_attention`) and explicit requests for review, as cards above
  the Work cards, in the daemon's order. A card shows what to decide, the
  asker's options as buttons, their recommendation, and the evidence as
  chips. An option fills the answer for a look before **Send**; the answer is
  a human-request reply, and **Accept** / **Return…** / **Withdraw** dispose
  a review item, so the decision is recorded by tendd. A commit or a revision
  range opens a read-only Changes view of exactly that revision in one click,
  in the checkout the asker recorded (else the asking agent's working
  directory), on this Mac only; a pull request opens in the browser, a path
  in Finder. Other hosts' questions come from their console mailbox. Agents
  blocked in their pane stay Work cards. The Review sheet lists the same
  decisions at its top. Against a tendd without the queue, Home shows this
  Mac's open review items and every host's questions instead.
- Decisions made from Home are recorded by tendd, not in the app's settings
  (`review_record_v1`): **Looks Good** is an `accepted` review record keyed
  to the unit and its exact revision, and that record is what hides the
  result in Finished; **Send Back** and an answer typed or chosen for a
  waiting agent are `returned` records with the words as the note, after the
  message went. They show in `tend review history`. Marks an older Tend kept
  in its settings are recorded once and then forgotten; against a tendd
  without `review_record_v1` and `decision_queue_v1`, Home keeps local marks
  as before.
- Below Home, VS Code's Activity Bar pattern switches one contextual sidebar
  at a time while the editor area remains stable. Tend applies that to
  **Explore**, **Inbox**, **Ask**, **Shells**, and **Files**, with a filter scoped
  to the active context. Explore owns topology and host registration; Inbox owns the
  operator's sent commands, durable replies, and agents that require operator
  attention; Ask owns Global Ask conversations, which are questions to a
  provider rather than requests from an agent. Explore already uses hosts as its
  roots, so it does not duplicate them in a separate Hosts activity.
- Lens separates resource navigation from contextual details and supports
  drilling into a resource without flattening infrastructure identity. Tend
  keeps each host's agent sessions independent and treats host, tmux session,
  window, and pane as execution-location metadata rather than Work identity.

The detail area is deliberately task-oriented:

- **Work** exists for a pipeline run or for panes carrying an explicit complete
  `workspace_id/work_id` stamp, including panes started by `tend work` on
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
  and exposes waiting, replied, and unread states. Opening a command returns
  to its exact agent pane; reading a reply uses the durable collaboration get
  operation.
- **Ask** lists the selected provider's Global Ask conversations, newest
  activity first; picking one opens it in the editor, where each exchange and
  the whole conversation can be copied as Markdown or as a prompt, and a
  conversation saved as a Markdown file.
- **Shell** is the interactive native Ghostty surface owned by a tendd PTY.
  It also has a read-only **Raw** mode for the bounded PTY output stream.

The app now opens on a **Work Command Center**, not a terminal list. Its
**Pipelines** section reads `tend work options --json` through the bundled
CLI and draws every configured pipeline as a launchable card: the agents are
laid out in the stages tend will start them in (one column per `after`
level), with the routes that select the pipeline underneath and a **Start…**
button that opens the sheet with that pipeline preselected. The config file
stays the source of truth; the app never keeps its own copy of routes or
pipelines.

**Start Work** collects the stable Work id, project folder, optional
workspace, pipeline, external issue reference, and initial task. Typing a
Work id shows the route it would take (`Route ^eng- → pipeline triad →
workspace webapp → own git worktree`), the workspace field suggests the
route's workspace and existing local sessions, the pipeline is a picker whose
default is the route's choice, the selected pipeline's stage diagram is shown
before launch, and message skills come from `[message.skills]`. The Start
button is enabled only for combinations the CLI would accept. tendd then runs
the canonical bundled `tend work up` implementation as an asynchronous
operation, so ticket resolution cannot freeze refreshes or terminal I/O. The
resulting Work opens as the same logical `{workspace_id, work_id}` used by
CLI and dashboard surfaces.

Work is not limited to this Mac. Pipelines are one **library**: this Mac's
config is the source, and every fleet host in `control` mode shows a badge
per pipeline (in sync, differs, missing, or unreadable) computed by reading
that host's config through tendd's `work_command` operation. **Sync to
hosts** writes the library definition to the hosts where it is missing or
differs, and pipelines that exist only on a host can be pulled into the
library. Pipelines are small, portable TOML; **routes** are not, because
they carry that host's folders and workspaces, so the Routes editor keeps a
host switcher and edits each host's `[[route]]` table separately. The Start
Work sheet has a host picker: the project folder is then a path on that
host, workspace suggestions come from that host's sessions, and the launch
itself runs `tend work up` on the host through the daemon while the app
keeps the same asynchronous operation status. A daemon without
`work_command_v1` still serves the local host through the bundled CLI and
says so.

Pipelines are editable in place. **Edit…** on a card (or **New Pipeline…**
and **Design your own…**) opens a visual editor: agents with alias, program,
role, task, split direction, prompt, and `after` toggles, a shared prompt
prefix, and the tmux layout, with the launch-stage picture redrawn as edges
change. Saving goes through `tend work pipeline set <name> --from-json -`,
so the same validation the CLI applies (allowlisted programs, unique
aliases, acyclic `after` edges) runs before and after the round trip, and
**Delete…** uses `tend work pipeline remove`. The **Routes** list under the
cards edits `[[route]]` entries (match, pipeline, workspace, folder) through
`tend work route set` and `route remove`, keeping order and untouched
`worktree`/`prepare` tables as written.

When the config has no pipeline yet, both the Command Center and the sheet
show tend's built-in presets (`solo`, `pair`, `triad`) with their stage
diagrams. **Install** writes the preset through `tend work preset apply`
(adding a catch-all route only when no route exists), and **Describe with an
agent…** opens the canonical interactive `tend work init` wizard in a native
Shell tab for a custom setup. An older bundled CLI without `work options`
falls back to the free-text fields.

**Live Watch** is the native replacement for the common `tend watch` operator
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
from Live Watch runs the bundled `tend fleet attach` inside a tendd-owned PTY
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
Tend rebuilds those block boundaries before display: `TendMarkdownText` keeps
a single line-limited `Text` for cards and inspectors, and
`ReadableMarkdownContent` renders full block layouts (including pipe tables)
for the Inbox and Ask conversations. The user-facing vocabulary is **Hosts**;
`fleet` survives only in protocol and type names.

Selecting a new item opens it as a replaceable preview tab. Pinning preserves
the tab while another Work, Agent, Host, or Shell is inspected. `Command-Shift-P`
opens the workspace command palette, and each sidebar context supports text and
status filters (`All`, `Attention`, `Active`); in Ask, Active is a
conversation still being answered and Attention one whose last turn failed.
Every attention count reads one summary (`TendAttentionSummary`). The Inbox
badge, the menu bar and Command Center's "Need attention" show what needs the
person — agents that need them, peers' requests for a decision, and unread
replies — even when another context is selected; commands still waiting on an
agent are listed but not counted. Explore's badge counts unread panes, the
Dock both (each once), and the Ask badge questions still being answered. Editor tab
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

## AAS accounts in the menu bar

Enable **Settings → Modules → aas — Agent Account Switcher** with `aas` on
the login shell's PATH. AAS joins the Tend cluster as a hoverable gauge chip;
it does not create a separate status item. Hover to see the highest
reported usage among active accounts. Click it to compare every stored
account by provider, filter active accounts, or search by name/email. Each
limit window has its own usage bar and reset time; limits are never added
across accounts. Expired readings are unavailable, and old or failed readings
are marked with a warning rather than presented as fresh quota.

The widget reads `aas usage --json` on opening and at most once every two
minutes while its chip is visible. Recent manual reads postpone the next
background read; failed reads also wait before retrying. **Refresh live**
explicitly requests `--fresh`. It does not switch
accounts or export credentials. Disabling the module removes its chip and
stops background usage queries. Hiding the chip also pauses those queries.
An existing AAS installation is reused; a missing
installation is explained in the module settings.

**Settings → Menu Bar** controls grouping by **Agent**, **Work**, or **Fleet
host**, up to three activity icons by default, and the AAS chip's visibility.
The same settings can be opened with the cluster popover's gear button.
**Maximum activity icons** allows zero through eight icons, with `+N` for
the rest; the available screen width may
reduce the count further. Zero keeps activity reachable through `+N` without
individual activity icons. Group chips open their agents rather than
acknowledging the entire group. Work identity includes host and workspace;
unmanaged panes use their actual socket/session/window boundary. Opening an agent card acknowledges
that session. Reviewed icons disappear after the card is closed and the
pointer leaves; a new state change can show the agent again. With no unseen
activity, only the Tend launcher and any enabled extension chip remain.

Chips unfold on entry without an intent delay. Leaving schedules a 0.2-second
fold, or 0.45 seconds toward an already-visible peek; returning cancels it.
A 4pt side/10pt lower margin retains the current state. Unfolding takes 0.16s,
folding 0.26s, and Reduce Motion applies the final layout immediately. The
focused capsule stays on top; otherwise left chips cover right chips, including
their badges. Hit tests use this paint order and the in-flight geometry.

Render keys contain visible chip styling, the focused label, live/selected
state, geometry, appearance and display scales. Hidden label/location changes
still update accessibility without assigning a new status image. The renderer
builds a point-sized bitmap for each display scale (including 1x) before drawing.

The cluster polls the pointer at 15 Hz while idle and 60 Hz while tracking
or animating, and pauses polling during display sleep. Snapshot notifications
in the same run-loop turn share one refresh. Unchanged render and accessibility
snapshots reuse the existing image/elements instead of rebuilding them.

## Build and run

Requirements:

- macOS 13 or newer;
- Xcode 26 or another Xcode capable of Swift 6;
- `xcodegen` (`brew install xcodegen`);
- Git, curl, tar, and a Rust 1.89+ toolchain.

From the repository root:

```bash
apps/tend-macos/Scripts/build-app.sh --open
```

The build reads the app version from the workspace `Cargo.toml` and rebuilds
the bundled `tend` and `tendd`. `TEND_SKIP_EMBED=1` is only for local UI
iteration: it keeps the existing helpers, so it can leave their versions and
behavior behind the app. Rebuild without it when checking runtime changes.

The first build downloads source dependencies and can take a while. Zig does
not need to be installed globally: the build downloads the exact compiler
required by the pinned Ghostty release and verifies its SHA-256 digest.

The resulting development application is located at:

```text
apps/tend-macos/.build/DerivedData/Build/Products/Debug/Tend.app
```

Build and smoke-test a universal Release app with:

```bash
CONFIGURATION=Release apps/tend-macos/Scripts/build-app.sh
CONFIGURATION=Release apps/tend-macos/Scripts/smoke-test.sh
```

Run the launch/daemon/session/renderer smoke test with:

```bash
apps/tend-macos/Scripts/smoke-test.sh
```

Set `TEND_SOCKET` before opening from a shell to select a non-default daemon
socket. Otherwise, the app uses `/tmp/tend-<uid>.sock`, matching tend's macOS
fallback.

## Distribution

Tend for Mac ships as a **notarized DMG on the GitHub release**, not through
the Mac App Store. That is not a preference. The app runs the operator's own
binaries (tmux, the agent CLIs, `ssh`), talks to a daemon that outlives it
over a socket in `/tmp`, and reads whatever project directory the work is in.
The App Sandbox forbids all three, and an unsandboxed app cannot be submitted.
Developer ID with notarization is the supported route for exactly this kind of
tool.

Users install it with:

```bash
brew install --cask open330/tap/tend-app
```

The cask is named `tend-app` rather than `tend` because the tap already
ships `Formula/tend.rb`, and a formula and cask of one name make
`brew install tend` ambiguous. It matters more here than it does for a
typical app: Tend.app has no Sparkle and no updater of its own, so
`brew upgrade` is the only way an installed copy ever moves forward.
`tap-bump` rewrites the cask from the release's DMG checksum, and skips it
when a release carries no DMG rather than pointing at a download that 404s.

Build one locally:

```bash
TEND_SIGN_IDENTITY="Developer ID Application: NAME (TEAMID)" \
TEND_TEAM_ID=TEAMID \
TEND_NOTARY_PROFILE=tend-notary \
apps/tend-macos/Scripts/package-dmg.sh
```

The script builds Release, signs every nested executable before the bundle
that contains them, builds the DMG, submits it to Apple, staples the ticket,
and finishes with the only check that matters:

```text
spctl -a -vvv -t install dist/Tend-<version>.dmg
  → accepted, source=Notarized Developer ID
```

That is what a stranger's Mac decides about the file, and the script fails if
the answer is anything else.

Two escape hatches. `TEND_SKIP_NOTARIZE=1` signs without submitting, for a
fast local check. Omitting `TEND_SIGN_IDENTITY` entirely produces a DMG named
`-unsigned`, which exercises the packaging path on a machine with no
certificate and will not open anywhere else.

Notarization credentials come either from a keychain profile
(`xcrun notarytool store-credentials`) or, in CI, from an App Store Connect
API key via `NOTARY_KEY_PATH`, `NOTARY_KEY_ID` and `NOTARY_ISSUER_ID`.

### On a tag

`release.yml`'s `macos-app` job does the same thing on `macos-15`, importing
the certificate into a keychain it creates for the job and deletes afterwards,
so the runner's login keychain is never touched. It reads:

| Secret | What it holds |
|---|---|
| `MACOS_CERT_P12` | base64 of a Developer ID Application `.p12` |
| `MACOS_CERT_PASSWORD` | that file's export password |
| `MACOS_NOTARY_KEY_P8` | base64 of the App Store Connect `.p8` |
| `MACOS_NOTARY_KEY_ID` | the key's ID |
| `MACOS_NOTARY_ISSUER_ID` | the issuer UUID |
| `MACOS_TEAM_ID` (variable) | the team the submission is filed under |

With no certificate secret the job still builds, warns, and uploads the
unsigned DMG as a workflow artifact instead of attaching it to the release —
a fork stays green, and a release never carries a DMG Gatekeeper would refuse.
`publish` waits for this job along with the Rust matrix, so a release is only
flipped out of draft once every asset it advertises exists.

The version comes from `[workspace.package]` in `Cargo.toml` and is pushed
into the app's `MARKETING_VERSION` at build time, then verified against the
built bundle. The daemon and the app that embeds it cannot disagree about
which release a user is running.

## Headless providers and API keys

Global Ask uses the installed `claude`, `codex`, and `gemini` CLIs in their
structured non-interactive modes, the Anthropic and OpenAI APIs with a key, or
Apple Intelligence through the bundled `tend-afm` helper on macOS 26 or later
(see [Configuration](CONFIGURATION.md)). The app augments a GUI-launched tendd PATH with
`~/.local/bin`, `~/.cargo/bin`, and Homebrew locations, while preserving
normal Claude Code/Codex CLI sign-in.

When `[ask].enabled` is absent or false, the Ask view shows an explicit
**Enable & Reload** action instead of allowing the first question to fail.
This writes the same opt-in grant as `tend init --component ask` and reloads
tendd; native PTY sessions require confirmation before replacement, while tmux
sessions remain running. Submit from the composer with **Command-Return** or
the provider-neutral **Send** button.

The Providers sheet can optionally store `ANTHROPIC_API_KEY` or
`CODEX_API_KEY` in the macOS login Keychain. A key is fetched only when its
matching provider is selected and crosses the owner-only tendd socket as a
one-turn credential. tendd puts it in only that child process environment; it
is redacted from debug output and never appears in config, Ask history, logs,
or command arguments.

## Local QA helper

`TendQAHelper.app` is an optional, separately signed development tool for
testing the native UI. It is not embedded in or launched by the production
Tend app. Keeping it separate means Accessibility and Screen Recording access
is granted to the QA tool rather than to Tend or the terminal process running
the test client.

Build, sign, install, and open the helper with:

```bash
apps/tend-macos/Scripts/install-qa-helper.sh
```

The installer uses an available Apple Development identity so that the helper
keeps a stable code identity across rebuilds. Override it when necessary with
`TEND_QA_CODESIGN_IDENTITY`, or install somewhere other than
`~/Applications` with `TEND_QA_INSTALL_DIR`. The helper requires macOS 14 or
newer because window capture uses ScreenCaptureKit's screenshot API.

In the helper window, choose **Request Permissions**, enable **Tend QA Helper**
under Privacy & Security > Accessibility and Screen & System Audio Recording,
then relaunch the helper. The permission status updates in the window.

The owner-only local client supports permission checks, window inspection,
capture, and focused test input:

```bash
apps/tend-macos/Scripts/tend-qa-helper-client.py status
apps/tend-macos/Scripts/tend-qa-helper-client.py inspect
apps/tend-macos/Scripts/tend-qa-helper-client.py capture --output /tmp/tend.png
apps/tend-macos/Scripts/tend-qa-helper-client.py type --text 'printf qa-ok' --return
apps/tend-macos/Scripts/tend-qa-helper-client.py click --x 600 --y 260
apps/tend-macos/Scripts/tend-qa-helper-client.py resize --width 920 --height 580
apps/tend-macos/Scripts/tend-qa-helper-client.py key --key , --mod command
apps/tend-macos/Scripts/tend-qa-helper-client.py key --key p --mod command --mod shift
apps/tend-macos/Scripts/tend-qa-helper-client.py key --key escape
```

Click coordinates are relative to the captured Tend window and are rejected if
they fall outside that window. `resize` moves and resizes the largest Tend
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

The helper listens on `/tmp/tend-qa-helper-<uid>.sock` with mode `0600`,
rejects peers owned by another user, caps request and text sizes, and always
targets the fixed Tend bundle identifier (`dev.tend.mac`). It cannot capture or
focus an arbitrary application. Because any process running as the same user
can reach this QA bridge, quit the helper when UI automation is finished; it is
intended for local development, not distribution.

## Keyboard navigation

The workbench's **Navigate** and **Editor** menus list the available shortcuts:

| Shortcut | Action |
| --- | --- |
| `⌘P` | Quick Open: search sessions, windows, panes, native shells, work, and agents |
| `⌘⇧P` | Command palette: navigate, create work/shells, refresh, or manage editor tabs |
| `↑` / `↓`, `Tab` / `Shift-Tab` | Select the previous/next enabled palette result |
| `Enter` / `Esc` | Open the selected result / dismiss the palette |
| `⌘1` … `⌘8`, `⌘9` | Activate an editor by position / activate the last editor |
| `Ctrl-Tab` / `Ctrl-Shift-Tab` | Next / previous editor |
| `⌘⌥←` / `⌘⌥→` | Focus the previous / next editor group |
| `⌘\` | Split the active editor to the right |
| `⌘⌥Enter` | Keep the preview editor open (pin) |
| `⌘W` | Close the editor, without terminating its session |
| `⌘⇧1` / `⌘⇧2` / `⌘⇧3` | Open Work Command Center / Ask / Inbox |
| `⌘⇧F` | Focus the sidebar filter |
| `⌘Enter` | Send from the focused pane/work prompt composer |

Quick Open searches names and host/socket context with multiword fuzzy matching.
While the palette is open, `⌘P` and `⌘⇧P` switch between destinations and commands
without closing the search field; typing is focused automatically.
Recent destinations rank first when the query is empty. Nonempty searches rank
by exact title, title prefix/substring, context substring,
then fuzzy match, with recent history only breaking ties. Exact pane titles take
precedence over same-named windows. Editing the query selects the best enabled
result; background refreshes preserve the user's selection. Identical names on
different hosts or tmux sockets remain separate destinations. Opening a result
pins its editor; it does not send terminal input or start another session.
Unavailable commands are shown disabled and skipped by keyboard selection.
IME composition keeps its native key handling, including Korean candidate
selection and composition-confirming Enter. Plain Tab outside the palette keeps
normal focus traversal; macOS Keyboard Navigation controls tabbing to all controls.

Pane/work send results occupy a fixed-size status area immediately left of Send,
so success or a long error never adds a row or shifts the layout. Hover or open
the status to read the complete details. A draft edited during an in-flight send
is preserved when the earlier send completes.

## Reproducible libghostty supply chain

[`apps/tend-macos/Dependencies.lock`](../apps/tend-macos/Dependencies.lock)
pins all non-package-manager source inputs:

- Ghostty release and full Git commit;
- the full Git commit containing the audited host-managed I/O patch set and
  native Swift terminal surface;
- Zig version and both macOS archive checksums.

[`build-libghostty.sh`](../apps/tend-macos/Scripts/build-libghostty.sh) checks
out those exact commits, verifies that the patch set names the same Ghostty
commit, applies it, builds arm64 and x86_64 static archives, combines them into
Tend's local `GhosttyKit.xcframework`, and points the local Swift package at
that artifact. The local compatibility patches are included in the cache key,
and the build rejects archives that omit the required Ghostty C ABI. No
release-hosted libghostty binary is linked into Tend.

The Xcode embed phase builds `tendd` for every architecture in the app target
and also builds the canonical `tend` CLI used for Work control and Fleet
attach. It combines both binaries when producing a universal app, so the
renderer, process-owning daemon, and execution implementation cannot drift to
incompatible architectures.

Build outputs and source checkouts live under `apps/tend-macos/.build` and are
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
  behind tendd's bounded retention window.

`read_session_wait` blocks on the PTY output/exit signal for at most 15
seconds, replacing the former 8–45 ms empty-read loop. TendIPCClient assigns
terminal reads their own serialized lane; keyboard input, attachment changes,
and resize control remain responsive while that lane waits.

## Event-driven refresh and render budget

Tend.app opens compact `fleet_subscribe`, `pipeline_subscribe`, and
`ask_subscribe` streams after its initial coherent load. Host update bursts
are coalesced for 75 ms and capped at four snapshot reads per second. A slow
15-second full reconciliation repairs stream disconnects and lag gaps; it is
not the primary freshness path. Collaboration revisions travel per host, so
the operator inbox and an open Collaborate module re-read only the affected
host rather than polling every mailbox every two seconds. Inbox safety
reconciliation runs at most once per minute even when Inbox is not visible, and equal
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
Bundled tendd**. After explicit confirmation it verifies that the socket and
daemon belong to the current user, stops the existing service, and launches the
version embedded in Tend.app. The confirmation is required because native PTY
sessions owned by the old daemon end during replacement; external tmux sessions
are not terminated.

For a Homebrew-managed daemon, replacement also stops the persistent
`open330/tap/tend` background service and disables the legacy
`dev.open330.tendd` LaunchAgent so neither can immediately reclaim the socket
with an older binary. Re-enable those services only after their installed
version advertises `session_bytes_v1` (`launchctl enable
gui/$(id -u)/dev.open330.tendd` for the legacy label).

## Process ownership and lifecycle

```text
Tend.app
├── Work/session UI
├── GhosttyTerminal (VT parser, Metal renderer, input encoder)
└── TendIPCClient
        │ byte-safe local IPC
        ▼
tendd
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

## Files and artifact previews

The Files activity-bar item opens a read-only file tree. Use **⇧⌘E** to show
Files for the active agent or Work folder, or **⌘O** to choose a local file or
folder. The host picker also browses registered SSH Fleet hosts; remote reads
use the configured SSH target with batch authentication and require Python 3.

The native outline supports arrow-key navigation, folder disclosure, and
type-to-select. Click the workspace folder name (or **⇧⌘G**) for **Go to Folder**: paths
complete after a short debounce, **Tab** completes a suggested directory, and
**Return** opens the path. Relative paths resolve from the current workspace.
Invalid paths leave the workspace unchanged.

A single click keeps focus in the tree and opens a preview tab; another file replaces that preview.
Double-click the file or pin its tab to keep it open. File tabs share the
workbench's split groups, persistence, close/reopen history and **⌘P** search
(recent files plus recursive filename search in the selected local or SSH folder).
The index is rebuilt when Quick Open opens. It scans at most 20,000 entries or
8 seconds, skips symlink directories and `.git`, `.build`, `node_modules`,
`target`, `vendor`, `__pycache__`, `.venv`, and displays up to 200 ranked results.
Incomplete or unreadable folders are reported in the palette; select a narrower
folder to search further. This searches filenames/paths, not file contents.

The visible tree polls locally every 2 seconds and over SSH every 10 seconds,
refreshing at most 32 expanded folders per pass while preserving existing nodes
and selection. Failed reads show an error row and are retried automatically.
Activating a file tab reveals its ancestors and selects its row without taking
keyboard focus. Files outside the current host/folder switch to their parent
folder; hidden files still require Show Hidden Files.

Markdown (`.md`, `.markdown`, `.mdown`, `.mkd`, `.mkdn`) renders headings, tables,
lists, code blocks and links. PNG, JPEG, GIF, WebP, HEIC/HEIF, TIFF, BMP and
ICO/ICNS icons render as images. SVG renders as an inert image with a Source
switch; SVG scripts and external resources do not execute. Images and
PDFs have native previews, and Office documents use macOS Quick Look. Markdown
and HTML offer a Source switch; HTML is a static preview with scripts and
network resources disabled. Relative document links and images resolve on the
same host as the document. Embedded images are limited to the document folder.
Text, Markdown, HTML and PDF previews support **⌘F**. Visible previews poll file
metadata every 2 seconds locally or 10 seconds over SSH, and reread contents
only after a change. Refresh is also available manually.

Preview reads are limited to 16 MB, text display to 2 MB, and each folder listing
to 1,000 entries. Files are never modified by the viewer. Local files can also
be revealed in Finder or opened in their default application.

**⌘W** closes the focused tab, including when a terminal, text view or web
preview has keyboard focus. It keeps an empty workbench open. **⇧⌘W** closes the
window, **⇧⌘T** reopens the last closed tab, and **⌃Tab / ⌃⇧Tab** move between
tabs. Live Watch uses **⌃⌘W** so it does not conflict with Close Window.

## Agent actions and account switching

Wherever an agent is shown — pane header, agent detail, Explore's context
menu, Command Center, the usage popover and the rate-limit badge — the same
actions appear under the same names, the one its state calls for first:

- a rate-limited Claude Code or Codex agent: **Switch Account and Resume…**;
- a blocked or failed agent: **Send "continue"** and **Move to Another
  Account…**;
- any other Claude Code or Codex agent: **Move to Another Account…**.

The switch sheet runs `tend swap … --json` on the agent's own host (local or
Fleet) through tendd's `work_command`, so it uses the CLI's swap logic, daemon
jobs and request keys unchanged. Before starting it shows the `--dry-run` plan
per agent: from which `aas` account to which, and whether the interrupted turn
is carried on. Running and finished switches appear in the status bar with
**Stop After Current** and **Retry Failed**; a switch an automation started
can be followed from the run log. When the local swap service cannot take a
job, the sheet and Settings › Automations say why (`swap_health`).

**When an agent hits its usage limit** is one choice — switch to another
account and continue, wait for the reset and continue, or just tell me — in
the rate-limit popover, the usage popover and Settings › Automations. It is
written as ordinary automation rules (`swap-on-limit`, the
`resume-after-limit` starter), so the CLI and `config.toml` see the same
thing. The policy covers this Mac's agents (the rules run in its tendd); a
capped agent counts as needing the person, and notifies with a **Switch
Account…** action, unless a rule of its own host's daemon will act on it — so
a capped agent on a Fleet host always asks.

Restoring a snapshot can also tell the agents it caught mid-turn to
`continue` once resumed (`mux_snapshot_continue_v1`).

## Explicit requests for a person

Agents use `tend msg attention "..." --action approval` or the
`tend_request_attention` MCP tool for a required approval, choice, or missing
information. These requests do not fabricate a waiting agent state. The app
reads the human incoming queue as well as operator-sent requests from every
registered Fleet host, including hosts with no remaining terminal panes.

They appear in Work Attention, Inbox > Needs Action, and the Dock badge. A
notification opens the exact host/request in Inbox, where Respond records a
human reply through the existing durable collaboration protocol. There is no
notification action that blindly types an approval into the terminal. Reading,
opening or dismissing the banner does not resolve the request. Each Mac keeps
its own notification receipts, while all connected clients see the response
stored on the source host. See [Fleet setup](FLEET.md#human-attention-across-clients).

### Responsiveness and runtime verification

Work commands use an ordered transport per host. Fleet snapshots and background
state reads use separate transports, while terminal input, resize and attachment
control retain their own ordered control path. A slow remote options request
therefore does not hold up another host or terminal input. Workspace projection
sorting/grouping runs off the main actor. At most one projection builds at a
time; notifications arriving during it replace a single pending input. Only the
latest result is published. Menu-bar cards activate Tend before taking keyboard
focus, switch directly between destinations, and close on a repeated click on
the same destination. A transient close on mouse-down suppresses only the same
destination on mouse-up.

Settings → Runtime shows the daemon version/generation and, when the installed
bundle has `Contents/Resources/tend-build.json`, its app build commit. A matching
version number alone does not establish matching code: several local main builds
can report the same release version. The app's embedded `tend` and `tendd` must
also be refreshed when installing a newer main build. CLI-only commands do not
automatically gain a native UI; Advanced settings exposes the daemon's complete
configuration, while dedicated forms remain limited to their supported sections.

Queued IPC work that is cancelled before it starts never reaches tendd. Cancelling
an in-flight operation does not undo an already-sent command. Live pane preview
ANSI parsing runs off the main actor and rejects superseded renders; inactive
previews wake immediately on reactivation instead of waiting for the idle timer.

Fleet snapshot construction indexes panes per host and then resolves only the
matching pane ID's candidates. Matching retains the same socket precedence and
ambiguity rules; the index is rebuilt for each snapshot, so it does not retain
a pane target after a host's topology changes.

Menu-bar glyph bitmaps are cached by glyph, quarter-point size and display
scales, with a maximum of 96 entries. Tint is applied during compositing, so
status changes do not create additional cache entries. Pointer movement away
from every menu-bar band skips replicated-window frame lookup.

Home reuses the work projection built by the model's detached workspace task.
It rebuilds on execution/pipeline changes, limit-policy notifications, and the
15-second reconciliation path so time-based limits do not remain cached.
