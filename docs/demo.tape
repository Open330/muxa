# muxa demo script — regenerate `docs/demo.gif` via:
#
#   vhs docs/demo.tape
#
# Requires charmbracelet/vhs, ttyd, ffmpeg, and a Chromium-based browser
# (VHS uses Chrome for frame capture).

Output docs/demo.gif

Set Shell bash
Set FontSize 14
Set Width 1200
Set Height 720
Set Theme "TokyoNight"
Set TypingSpeed 45ms
Set PlaybackSpeed 1

# Prelude — start a scratch daemon and seed it. Nothing here is visible in
# the final GIF.
Hide
Type "export MUXA_SOCKET=/tmp/muxa-demo.sock"
Enter
Type "rm -f $MUXA_SOCKET"
Enter
Type "muxad &"
Enter
Sleep 500ms
Type "until [ -S $MUXA_SOCKET ]; do sleep 0.1; done"
Enter
Type "bash docs/demo-seed.sh"
Enter
Sleep 200ms
Type "clear"
Enter
Show

Type "# a live snapshot across four agent CLIs"
Sleep 600ms
Enter
Type "muxa status"
Enter
Sleep 2500ms

Type "# one-liner for tmux status-right"
Sleep 600ms
Enter
Type "muxa status-line"
Enter
Sleep 1500ms

Type "# live dashboard — q to quit"
Sleep 600ms
Enter
Type "muxa watch"
Sleep 200ms
Enter
Sleep 3000ms
Type "q"
Sleep 800ms

# Postlude — tear down the scratch daemon.
Hide
Type "kill %1 2>/dev/null; wait 2>/dev/null; rm -f $MUXA_SOCKET"
Enter
Show
