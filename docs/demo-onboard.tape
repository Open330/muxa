# muxa onboarding demo — "learn it where it appears"
#
#   vhs docs/demo-onboard.tape   →   docs/demo-onboard.gif
#
# Unlike the fleet recordings, this needs no daemon or tmux fixture. The
# dashboard behind each coach dialog is Muxa's deliberately inert onboarding
# mock: it teaches location, state icons, and commands without touching a real
# session.
#
# Arc:
#   0–7s    map the UI       — work/session, agent/pane, canonical state icons
#   7–12s   inspect & keys   — pane preview and contextual footer shortcuts
#   12–18s  type the command — real `muxa work start`, validated but not run
#   18–23s  collaborate/MCP — message/mailbox and compact agent-side tools
#   23–25s  launch           — type `muxa watch` to finish

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

Type "muxa onboard"
Enter
Sleep 1800ms

# session = work, pane = agent
Enter
Sleep 1400ms
Enter
Sleep 1400ms
Enter
Sleep 1400ms

# The state lesson now shows the exact ● / ▶ / ○ / ■ visual language.
Enter
Sleep 1800ms

# Inspector and contextual footer.
Enter
Sleep 1400ms
Enter
Sleep 1400ms

# A real command is typed, but onboarding only validates it.
Type "muxa work start CAL-7041 --agent codex"
Sleep 800ms
Enter
Sleep 1800ms
Enter
Sleep 1000ms

# Message and mailbox are still hands-on shortcut previews.
Type "m"
Sleep 1300ms
Type "M"
Sleep 1300ms
Enter
Sleep 1500ms

# Agent-side MCP pattern.
Enter
Sleep 1400ms

# Finish by recalling the command that opens the real dashboard.
Type "muxa watch"
Sleep 700ms
Enter
Sleep 1400ms
Enter
Sleep 800ms
