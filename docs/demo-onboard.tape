# muxa onboarding demo — "learn it where it appears"
#
#   vhs docs/demo-onboard.tape   →   docs/demo-onboard.gif
#
# Unlike the fleet recordings, this needs no daemon or tmux fixture. The
# dashboard behind each coach dialog is Muxa's deliberately inert onboarding
# mock: it teaches location, state icons, and shortcuts without touching a real
# session.
#
# Arc:
#   0–6s    navigate & sort — j/l and Alt-T over the real session-state gutter
#   6–12s   inspect & help  — o and ? open/close watch-shaped overlays
#   12–15s  create work     — n opens the form and Esc safely closes it
#   15–20s  collaborate     — m, empty Backspace, M mailbox toggle, MCP
#   20–22s  finish          — q, the same key that quits live watch

Output docs/demo-onboard.gif

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

Type "muxa onboard --lang ko"
Enter
Sleep 1800ms

# Start once with Enter; every feature lesson after this uses its live key.
Enter
Sleep 1100ms

# Move from CAL-7088 to CAL-7041, then enter its first child agent.
Type "j"
Sleep 1100ms
Type "l"
Sleep 1200ms

# State is the left gutter of SESSION; Alt-T applies the real state sort.
Alt+t
Sleep 1800ms

# Open and close preview, then do the same with full shortcut help.
Type "o"
Sleep 1400ms
Type "o"
Sleep 900ms
Type "?"
Sleep 1400ms
Type "?"
Sleep 1000ms

# n opens the real-shaped bottom form; Esc closes it safely.
Type "n"
Sleep 1400ms
Escape
Sleep 900ms

# Message composer closes with empty Backspace; M toggles the mailbox.
Type "m"
Sleep 1100ms
Backspace
Sleep 800ms
Type "M"
Sleep 1200ms
Type "M"
Sleep 1000ms

# Agent-side MCP pattern.
Right
Sleep 1200ms

# Finish with q, which also quits the live dashboard.
Type "q"
Sleep 800ms
