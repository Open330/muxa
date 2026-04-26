# muxa demo script — regenerate `docs/demo.gif` via:
#
#   vhs docs/demo.tape
#
# Requires charmbracelet/vhs, ttyd, ffmpeg, and a Chromium-based browser.
# docs/demo-setup.sh handles the muxad + tmux bootstrap so this file
# stays focused on the recording.

Output docs/demo.gif

Set Shell bash
Set FontSize 14
Set Width 1200
Set Height 720
Set Theme "TokyoNight"
Set TypingSpeed 45ms
Set PlaybackSpeed 1

# Prelude — invisible setup.
Hide
Type "export PATH=$HOME/.cargo/bin:/tmp/muxa-demo-shim:$PATH"
Enter
Type "export MUXA_SOCKET=/tmp/muxa-demo.sock"
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

# 1. Table view — read the data.
Type "muxa status"
Enter
Sleep 2500ms

# 2. Attach to a real tmux session. The status bar at the TOP renders
#    `muxa status-line --pane #{pane_id}` live — watch the right side.
Type "/usr/bin/tmux -L muxa-demo attach -t main"
Enter
Sleep 2500ms

# 3. Walk a couple of windows so the muxa indicator on the right side of
#    the status bar updates per pane (working / waiting / idle glyphs).
Ctrl+B
Type "n"
Sleep 1800ms
Ctrl+B
Type "n"
Sleep 1800ms

# 4. prefix + s → muxa watch in a popup. This is the production flow:
#    floating dashboard, agents on top, every other pane below.
Ctrl+B
Type "s"
Sleep 4500ms
Type "j"
Sleep 300ms
Type "j"
Sleep 600ms
Type "q"
Sleep 800ms

# 5. Detach from tmux.
Ctrl+B
Type "d"
Sleep 600ms

# Postlude — tear down everything.
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
