# muxa tmux onboarding demo — prefix checkpoint + safe suffix muscle memory
#
#   vhs docs/demo-tmux-onboard.tape   →   docs/demo-tmux-onboard.gif
#
# The mock first verifies one detected prefix-only press. It then uses suffix
# keys with a virtually armed prefix, so no live tmux binding is executed.
#
# Arc:
#   0–4s    prefix, hierarchy     — Ctrl-b, w
#   4–9s    pane layout          — %, ", Right
#   9–14s   zoom and copy mode   — z/z, [/q
#   14–18s  detach semantics     — d
#   18–24s  Muxa surfaces         — s watch, q peek, D dashboard

Output docs/demo-tmux-onboard.gif

Set Shell bash
Set FontFamily "JetBrainsMono Nerd Font Mono"
Set FontSize 14
Set Width 1320
Set Height 720
Set Theme "GitHub Dark"
Set TypingSpeed 42ms
Set PlaybackSpeed 1

Hide
Type "export PATH=$PWD/target/debug:$HOME/.cargo/bin:$PATH"
Enter
Type "export LANG=C.UTF-8"
Enter
Type "export LC_ALL=C.UTF-8"
Enter
Type "clear"
Enter
Show

Type "muxa onboard --tmux --lang ko"
Enter
Sleep 1800ms

# Start once, then press only the detected prefix and wait for confirmation.
Enter
Sleep 1000ms
Ctrl+B
Sleep 1200ms

# Every feature lesson after the checkpoint uses the real suffix key.
Type "w"
Sleep 1100ms
Type "c"
Sleep 1200ms

# Split the inert pane mock left/right and then top/bottom.
Type "%"
Sleep 1100ms
Type `"`
Sleep 1200ms
Right
Sleep 1100ms

# Zoom is a toggle, so practise both directions before moving on.
Type "z"
Sleep 1100ms
Type "z"
Sleep 1000ms

# Enter and leave the mock copy-mode key table.
Type "["
Sleep 1200ms
Type "q"
Sleep 1100ms

# Detach is simulated; no real tmux client is detached.
Type "d"
Sleep 1200ms

# Muxa adds watch, peek, and dashboard to the same prefix table.
Type "s"
Sleep 1300ms
Type "q"
Sleep 1300ms
Type "D"
Sleep 1500ms

# Confirm the visible dashboard, then finish the final summary.
Enter
Sleep 900ms
Enter
Sleep 800ms
