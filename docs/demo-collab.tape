# muxa collaboration demo — "talk to the fleet without leaving the list"
#
#   vhs docs/demo-collab.tape   →   docs/demo-collab.gif
#
# The companion to demo.tape. That one is about finding the agent that
# needs you; this one is about saying something once you have.
#
# Arc:
#   0–7s     the mailbox   — `b`: durable request/reply between agents,
#                            incoming and sent, with the full body
#   7–15s    messaging     — `m`: address whatever row the cursor is on,
#                            pick the contract, send
#   15–27s   ask           — `a`: a headless question to claude or codex,
#                            answered without opening a session, then `A`
#                            to browse the thread
#
# The agent CLIs are stubbed for this recording — see the shims in
# docs/demo-setup.sh. A real `a` costs money and answers differently every
# time, neither of which a recording can use.

Output docs/demo-collab.gif

Set Shell bash
Set FontFamily "JetBrainsMono Nerd Font Mono"
Set FontSize 14
# 1320x720 at font 14 gives a 130x32 terminal — taller than the hero tape
# because the ask panel stacks a list and a detail pane inside the list
# box, and a short terminal cuts the answer down to two lines.
#
# The width is not cosmetic either: `muxa watch` only splits out the
# inspector at >= 120 columns, and vhs reserves enough padding that the
# obvious 1200x760 lands at 117 — close enough to look deliberate, wide
# enough to look broken. Re-measure with `tput cols` before changing any
# of these three numbers.
Set Width 1320
Set Height 720
Set Theme "GitHub Dark"
Set TypingSpeed 45ms
Set PlaybackSpeed 1

# ── Prelude — invisible setup ────────────────────────────────────────────
Hide
Set TypingSpeed 1ms
Type "export MUXA_DEMO_TMUX=$(command -v tmux)"
Enter
Type "export PATH=$HOME/.cargo/bin:$PATH"
Enter
Type "export LANG=C.UTF-8"
Enter
Type "export LC_ALL=C.UTF-8"
Enter
Type "bash docs/demo-setup.sh >/dev/null 2>&1"
Enter
Sleep 12s
Type "clear"
Enter
Type "$MUXA_DEMO_TMUX -u -L muxa-demo attach -t ctl:0"
Enter
Sleep 2s
Type "muxa watch"
Enter
Sleep 3s
Set TypingSpeed 45ms
Show

# ── 1. The mailbox (0–7s) ────────────────────────────────────────────────
# Agents leave each other durable requests: a kind, a work mode, a body,
# and a reply that has to come back. `b` is that mailbox — three queued
# from other agents, one this session sent out.
Type "b"
Sleep 2500ms
Down@800ms 2
Sleep 2500ms
Escape
Sleep 800ms

# ── 2. Messaging the cursor row (7–15s) ──────────────────────────────────
# `m` addresses whichever session the cursor is on — not the window muxa
# watch happens to be running in. Tab picks the contract kind, Ctrl-E the
# work mode, and the agent gets a request it must answer.
# `gg` first: the cursor is wherever the mailbox left it, and the row this
# scene wants is a specific one — `api`, whose codex reviewer sent the
# queued request the viewer just read. Answering its author closes the loop
# the mailbox scene opened.
Type "gg"
Sleep 800ms
Down@500ms 4
Sleep 800ms
Type "m"
Sleep 1500ms
Type "split the reap error — an unreachable tmux server isn't a closed pane. Go ahead."
Sleep 2s
Enter
Sleep 2s

# ── 3. Ask (15–27s) ──────────────────────────────────────────────────────
# Not every question needs a session. `a` runs it headless against claude
# or codex — Tab switches which — and the answer lands in a browsable
# thread rather than a scrollback you'll lose.
Type "a"
Sleep 1200ms
Type "which agents are working right now, and which need me?"
Sleep 1500ms
Enter
# Submitting opens the history panel by itself, landing on the question
# just asked — so this beat shows the query in flight, not a dead pause.
Sleep 2500ms
# Back to the list while it runs. The panel takes its snapshot when it
# opens and does not poll, so sitting on it would show "…waiting" forever
# no matter how long the sleep is. Leaving and coming back is both what a
# user actually does and what re-fetches.
Escape
Sleep 2s
# `A` browses the thread: every answer kept, `|` grows the detail pane so
# a long answer is readable, Tab filters by agent.
Type "A"
Sleep 2s
Type "|"
Sleep 4s
Tab
Sleep 2500ms
Escape
Sleep 800ms

# ── Postlude — tear everything down ──────────────────────────────────────
Hide
Set TypingSpeed 1ms
Type "q"
Sleep 500ms
Ctrl+B
Type "d"
Sleep 1s
Type "bash docs/demo-teardown.sh"
Enter
Sleep 3s
Show
