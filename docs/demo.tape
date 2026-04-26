# muxa demo script — regenerate `docs/demo.gif` via:
#
#   vhs docs/demo.tape
#
# Requires charmbracelet/vhs, ttyd, ffmpeg, and a Chromium-based browser
# (VHS uses Chrome for frame capture). docs/demo-setup.sh handles the
# tmux/muxad bootstrap so this file stays focused on the recording.

Output docs/demo.gif

Set Shell bash
Set FontSize 14
Set Width 1200
Set Height 720
Set Theme "TokyoNight"
Set TypingSpeed 45ms
Set PlaybackSpeed 1

# Prelude — invisible setup. Spins up a scratch muxad on its own socket,
# stands up an isolated tmux server, seeds three agent rows, and prepends
# a `tmux` PATH shim so muxa's children see the demo server.
Hide
Type "export MUXA_SOCKET=/tmp/muxa-demo.sock"
Enter
Type "export PATH=/tmp/muxa-demo-shim:$PATH"
Enter
Type "rm -f $MUXA_SOCKET"
Enter
Type "muxad >/dev/null 2>&1 &"
Enter
Sleep 500ms
Type "until [ -S $MUXA_SOCKET ]; do sleep 0.1; done"
Enter
Type "bash docs/demo-setup.sh >/dev/null"
Enter
Sleep 300ms
Type "clear"
Enter
Show

Type "# muxa watch — agents + every other tmux pane in one view"
Sleep 800ms
Enter
Type "muxa status"
Enter
Sleep 2500ms

Type "# fullscreen dashboard, agent-aware, attaches on ⏎"
Sleep 800ms
Enter
Type "muxa watch"
Sleep 200ms
Enter
Sleep 4500ms

Type "j"
Sleep 300ms
Type "j"
Sleep 300ms
Type "j"
Sleep 300ms
Type "k"
Sleep 800ms
Type "q"
Sleep 700ms

# Postlude — tear down everything we created.
Hide
Type "/usr/bin/tmux -L muxa-demo kill-server 2>/dev/null"
Enter
Type "kill %1 2>/dev/null"
Enter
Type "wait 2>/dev/null"
Enter
Type "rm -rf /tmp/muxa-demo-shim $MUXA_SOCKET"
Enter
Show
