# Muxa demo GIFs — how to regenerate

The GIFs are checked into the repo rather than
generated at build time. Re-record whenever you change the visible flow:
new keybinds, layout shifts, a renamed subcommand, a new panel.

## Files

| File                      | Role                                                                                     |
| ------------------------- | ---------------------------------------------------------------------------------------- |
| `docs/demo.tape`          | [VHS](https://github.com/charmbracelet/vhs) script for the hero GIF.                      |
| `docs/demo-collab.tape`   | VHS script for the collaboration GIF.                                                     |
| `docs/demo-onboard.tape`  | Standalone fullscreen onboarding walkthrough; no fixture or daemon needed.                |
| `docs/demo-setup.sh`      | Builds the whole fixture: config, PATH shims, tmux server, `muxad`, the seeded fleet, the mailbox, and the ask history. |
| `docs/demo-paint.sh`      | Emits one agent's screen — `demo-paint.sh <agent> <state> <prompt> [tool]...` — so panes hold a believable frame instead of a bare `cat`. |
| `docs/demo-teardown.sh`   | Removes all of it. Safe to run at any time, including after a half-finished render.        |
| `docs/demo-optimize.sh`   | Rebuilds a rendered GIF on a 64-colour palette. Run it after `vhs`, before committing.     |
| `docs/demo.gif`           | Hero output. 1320 × 620, ~1.6 MB after optimizing.                                          |
| `docs/demo-collab.gif`    | Collaboration output. 1320 × 720, ~1.5 MB after optimizing.                                 |
| `docs/demo-onboard.gif`   | Korean onboarding output. 1320 × 720, ~806 KB after optimizing.                           |

## What the recordings cover

The two fleet recordings are split because one GIF covering everything ran
45 seconds, which nobody watches to the end of. Onboarding is separate because
it is a self-contained lesson and deliberately needs no live fixture.

**`demo.tape` — triage.** Sixteen sessions on one screen with the state
sort floating the ones that need you; `|` cycling the list/inspector
split, with the inspector carrying the selected agent's live screen; the
k9s-style swarm view with its subagent trees; `muxa attend` teleporting
the tmux client to the agent blocked longest.

**`demo-collab.tape` — talking to the fleet.** `b` for the durable
request/reply mailbox; `m` addressing whatever row the cursor is on; `a`
asking claude a headless question and `A` browsing the answers.

**`demo-onboard.tape` — learning the model.** A safe replica of watch's
session-state gutter, columns, inspector, overlays, and footer. The tape
uses the Korean UI and advances only with real watch keys; there is no shell
command transcription exercise. It does not use `demo-setup.sh` because
onboarding must not touch real sessions.

## The fixture

`demo-setup.sh` is the whole story. It stands up a tmux server labelled
`muxa-demo`, seeds ~18 agents across every state muxa can render
(working, waiting on input, waiting on a choice, error, rate-limited,
idle) plus live Task subagents, then feeds the mailbox through the real
`muxa msg` path. Nothing in the recording is a mock-up of muxa itself —
only the agents are fixtures.

Four details in there are load-bearing, and each one cost a take to find:

* **The daemon and its paths are hardcoded, not inherited.** The script
  starts a `muxad` and feeds it a fabricated fleet. If it honoured a
  `MUXA_SOCKET` from your shell it would inject that fleet into your real
  registry.
* **`MUXA_TMUX_SOCKET` is pinned before any session is created.** Panes
  inherit their environment at creation time, and a pane without the pin
  enumerates every tmux server on the host. The first take showed 59
  sessions, most of them the author's.
* **Every seeding call claims the demo tmux server explicitly.** A hook
  client stamps events with `$TMUX`; run the script from inside your own
  tmux and every event is silently dropped as out-of-scope.
* **`ctl` is the operator's session, and the recording types there.** The
  inspector renders the selected pane's live screen, so running the TUI
  from a pane the cursor might land on makes muxa draw a mirror of
  itself, which reads as a bug.

`claude` and `codex` are shimmed onto `PATH` for the ask feature. A real
`a` bills the user, takes an unpredictable 10-40 s, and answers
differently on every render.

## Recording

```bash
cd /path/to/muxa
cargo build -p muxa-cli  # demo-onboard.tape prefers target/debug/muxa
vhs docs/demo.tape
vhs docs/demo-collab.tape
vhs docs/demo-onboard.tape
docs/demo-optimize.sh docs/demo.gif docs/demo-collab.gif docs/demo-onboard.gif
```

The optimize pass is not optional housekeeping — VHS emits a 256-colour
GIF and these recordings use maybe forty of them, so rebuilding on a
64-colour palette takes ~21% off each file with no visible difference on
terminal content. Skip it and you commit a fifth of a megabyte of unused
palette to a README people load on phones.

VHS spins up `ttyd`, points headless Chrome at it, records the rendered
terminal, and encodes the GIF. The fleet tapes run `demo-setup.sh` in their
prelude and `demo-teardown.sh` in their postlude, so no state is left behind —
and setup tears down before it builds, so an interrupted render does not poison
the next one. `demo-onboard.tape` runs the inert onboarding mock directly and
does not need teardown.

You need [JetBrains Mono Nerd Font](https://github.com/ryanoasis/nerd-fonts)
installed. Without it the terminal falls back to a font with different
metrics: glyphs render at roughly double width, the column count
collapses, and `muxa watch` silently drops the inspector.

If Chrome cannot create a user namespace — containers, restricted
runners — it bails with:

```
could not launch browser: Failed to move to new namespace:
PID namespaces supported, Network namespace supported, but failed:
errno = Operation not permitted
```

Set `VHS_NO_SANDBOX=true` and record again.

## Tweaking the tapes

* **Terminal size is not cosmetic.** `muxa watch` only splits out the
  inspector at ≥ 120 columns, and VHS reserves enough padding that the
  obvious `1200 × 760` at font 14 lands at **117** — wide enough to look
  deliberate, narrow enough to look broken. Verify with a throwaway tape
  that runs `tput cols` before trusting new numbers.
* **Move the cursor from `gg`, never relatively.** The cursor opens on
  whichever session the TUI is running in, so a bare `Down` is measured
  from a position that shifts with the fixture.
* **Submitting an ask opens the history panel by itself**, showing the
  question in flight. The panel snapshots on open and does not poll, so
  the answer only appears if you close and reopen it — which is what the
  tape does, and what a user does anyway.
* **Pacing**: `Sleep` is in milliseconds. ~1500 ms after a key gives the
  viewer time to read; ~500 ms is right between consecutive keystrokes.
* **`Set Framerate` is not a size lever.** VHS 0.11 emitted 25 fps for
  these tapes whether or not it was set; asking for 24 changed the file
  by 0.02%. Palette size is where the bytes are — see the optimize step.
* **State glyphs render as dashes**: make sure the tape exports a UTF-8
  locale and the demo tmux server starts with `tmux -u`; otherwise tmux
  degrades `●` / `▶` / `○` before VHS ever sees them.

## Troubleshooting

| Symptom                                                    | Cause                                                                    | Fix                                                                 |
| ---------------------------------------------------------- | ------------------------------------------------------------------------ | ------------------------------------------------------------------- |
| `Failed to move to new namespace: Operation not permitted` | Chrome's sandbox can't run here.                                         | `VHS_NO_SANDBOX=true vhs docs/demo.tape`                            |
| Characters double-width, no inspector                      | JetBrains Mono Nerd Font is missing and the terminal fell back.          | Install the font, `fc-cache -f`.                                    |
| "daemon not reachable" flashes mid-recording               | A `muxad` from an earlier run is still holding the demo socket.          | `docs/demo-teardown.sh` — it sweeps by config path, not by name.    |
| Sessions from your own machine appear in the list          | `MUXA_TMUX_SOCKET` was not inherited by the demo panes.                  | Check it is exported before `new-session`, not after.               |
| Agent rows are empty                                       | Seeded events were dropped as out-of-scope, usually the `$TMUX` stamp.   | Re-run setup without `>/dev/null` and read the output.              |
| `parser: N error(s)` from VHS                              | A typo in the tape — often an absolute path treated as a command.        | `vhs validate docs/demo.tape` and read the highlighted spans.       |
