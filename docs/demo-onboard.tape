# muxa onboarding demo — shell → tmux → Muxa, one continuous scenario
#
#   vhs docs/demo-onboard.tape   →   docs/demo-onboard.gif
#
# Unlike the fleet recordings, this needs no daemon or tmux fixture. It starts
# at an inert shell, teaches tmux by preserving each virtual layout transition,
# then continues in the same fullscreen client into the Muxa watch workflow.
#
# Arc:
#   0–17s   shell + tmux    — new-session, prefix, windows, panes, detach/attach
#   17–20s  managed keys    — s watch, q peek, D dashboard
#   20–25s  navigate/sort   — j/l and Alt-T over the real state gutter
#   25–31s  inspect/create  — o, ?, n and safe cancel
#   31–38s  collaboration   — m, Backspace, M mailbox, MCP, q

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

# Type the real commands into the virtual shell rather than accepting a
# pre-filled prompt.
Type "tmux new-session -s muxa-onboarding"
Enter
Sleep 900ms
Ctrl+B
Sleep 900ms

# Safe tmux suffix drills preserve every visual result.
Type "w"
Sleep 800ms
Type "c"
Sleep 900ms
Type "%"
Sleep 800ms
Type `"`
Sleep 900ms
Right
Sleep 800ms
Type "z"
Sleep 800ms
Type "z"
Sleep 700ms
Type "["
Sleep 900ms
Type "q"
Sleep 800ms

# d returns to the virtual base shell. Type a real attach command to resume.
Type "d"
Sleep 1200ms
Type "tmux attach -t muxa-onboarding"
Enter
Sleep 900ms

# Step 11 teaches managed bindings; step 12 continues on the same watch surface.
Type "s"
Sleep 1000ms
Type "q"
Sleep 900ms
Type "D"
Sleep 1000ms
Enter
Sleep 1000ms

# Move from muxa › sandbox to muxa › onboarding, then enter its first agent.
Type "j"
Sleep 1100ms
Type "l"
Sleep 1200ms

# State is the left gutter of WORKSPACE › WORK; Alt-T applies the real state sort.
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
