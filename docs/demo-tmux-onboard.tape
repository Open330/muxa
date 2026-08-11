# muxa tmux onboarding demo — safe suffix-key muscle memory
#
#   vhs docs/demo-tmux-onboard.tape   →   docs/demo-tmux-onboard.gif
#
# The mock detects and displays the configured tmux prefix but never sends it.
# Every typed character below is only the suffix that follows a virtually
# armed prefix, so this recording cannot mutate a live tmux client.
#
# Arc:
#   0–4s    hierarchy and windows — w, c
#   4–9s    pane layout          — %, ", Right
#   9–14s   zoom and copy mode   — z/z, [/q
#   14–18s  detach semantics     — d
#   18–22s  Muxa prefix table    — s/q/D

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

# Start once with Enter; every lesson after this uses the real suffix key.
Enter
Sleep 1000ms
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
Sleep 900ms
Type "q"
Sleep 900ms
Type "D"
Sleep 1300ms

Enter
Sleep 800ms
