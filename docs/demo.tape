# muxa hero demo — "your whole agent fleet on one screen"
#
#   vhs docs/demo.tape   →   docs/demo.gif
#
# This one is about *triage*: sixteen sessions, who needs you, and getting
# there. The collaboration surfaces — mailbox, messaging, ask — have their
# own recording in demo-collab.tape, because cramming both into one GIF
# produced 45 seconds nobody watches to the end of.
#
# Arc:
#   0–7s     the fleet     — every session on one screen, sorted so the
#                            agents that need you float to the top
#   7–13s    the inspector — `|` cycles the split; the wide inspector
#                            carries the selected agent's full detail
#   13–20s   the swarm     — k9s-style clusters with live spinners and an
#                            expandable subagent tree
#   20–25s   teleport      — `muxa attend` jumps to whoever's blocked longest
#
# docs/demo-setup.sh builds the fleet; docs/demo-teardown.sh removes it.

Output docs/demo.gif

Set Shell bash
Set FontFamily "JetBrainsMono Nerd Font Mono"
Set FontSize 14
# 1320x620 at font 14 gives a 130x27 terminal. The width is not
# cosmetic: `muxa watch` only splits out the inspector at >= 120 columns,
# and vhs reserves enough padding that the obvious 1200x760 lands at 117 —
# close enough to look deliberate, wide enough to look broken. Re-measure
# with `tput cols` before changing any of these three numbers.
Set Width 1320
Set Height 620
Set Theme "GitHub Dark"
Set TypingSpeed 45ms
Set PlaybackSpeed 1

# ── Prelude — invisible setup ────────────────────────────────────────────
# demo-setup.sh owns the whole fixture: config, shims, tmux server, muxad,
# and the seeded fleet. Keeping it out of the tape matters because every
# line here is one vhs has to type, at typing speed, on every render.
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
# No `exec` — the postlude needs this shell back after the client detaches.
Type "$MUXA_DEMO_TMUX -u -L muxa-demo attach -t ctl:0"
Enter
Sleep 2s
Set TypingSpeed 45ms
Show

# ── 1. The fleet (0–7s) ──────────────────────────────────────────────────
# Sixteen sessions, one screen. The sort floats needs-attention rows —
# error, rate-limited, choice, input — above the ones that are just busy,
# so the top of the list is always the work queue.
Type "muxa watch"
Enter
Sleep 3500ms
# `gg` rather than a bare `Down`: the cursor opens on whatever session the
# TUI is running in, so every later move would be relative to a position
# that shifts with the fixture. From the top the walk is deterministic —
# and the top is where the attention rows are.
Type "gg"
Sleep 1200ms
Down@700ms 3
Sleep 1800ms

# ── 2. The inspector (7–13s) ─────────────────────────────────────────────
# `|` cycles the list/inspector split 50/50 → 70/30 → 30/70. The wide
# inspector is where a row stops being a summary line: full prompt, full
# last response, state and duration.
Type "|"
Sleep 1500ms
Type "|"
Sleep 3s
Down@700ms 2
Sleep 2s
Type "|"
Sleep 800ms
Type "q"
Sleep 500ms

# ── 3. The swarm (13–20s) ────────────────────────────────────────────────
# One cluster per session, animated spinners on the working agents, and an
# expandable tree under the agents that spawned Task subagents.
Type "muxa watch --view swarm"
Enter
Sleep 3500ms
Type "gg"
Sleep 1s
# Nine rows down from the top of the state sort lands on `main`, the agent
# carrying three live Task children. Walking there rather than jumping is
# the point: the clusters scrolling past *are* the fleet.
Down@350ms 9
Sleep 2500ms
Type "q"
Sleep 500ms

# ── 4. Teleport (20–25s) ─────────────────────────────────────────────────
# Don't hunt across sixteen sessions — jump the tmux client straight to
# whichever agent has been blocked the longest.
Type "muxa attend"
Enter
Sleep 4s

# ── Postlude — tear everything down ──────────────────────────────────────
Hide
Set TypingSpeed 1ms
Ctrl+B
Type "d"
Sleep 1s
Type "bash docs/demo-teardown.sh"
Enter
Sleep 3s
Show
