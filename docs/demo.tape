# muxa demo script — regenerate `docs/demo.gif` via:
#
#   vhs docs/demo.tape
#
# Requires charmbracelet/vhs, ttyd, ffmpeg, and a Chromium-based browser.
# docs/demo-setup.sh handles the muxad + tmux bootstrap so this file
# stays focused on the recording.
#
# Visible flow:
#   1. muxa status            — table, run from a bare shell
#   2. tmux attach -t main    — drop into a real tmux session
#   3. prefix + s             — muxa watch popup (the switcher)
#   4. muxa status-line       — one-liner from inside the tmux pane
#   5. prefix + n             — walk a window, see status-right update
#   6. prefix + d             — detach

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

# 1. muxa status — read the data.
Type "muxa status"
Enter
Sleep 2500ms

# 2. Drop into a real tmux session. Top status bar already renders the
#    muxa indicator on the right.
Type "/usr/bin/tmux -L muxa-demo attach -t main:0"
Enter
Sleep 2500ms

# 3. prefix + s — muxa watch popup. The "switch" experience.
Ctrl+B
Type "s"
Sleep 4000ms
Type "j"
Sleep 300ms
Type "j"
Sleep 600ms
Type "q"
Sleep 800ms

# 4. muxa status-line — invoked from inside the attached tmux pane so
#    the viewer sees the same one-liner that tmux is feeding into
#    status-right.
Type "muxa status-line"
Enter
Sleep 2200ms

# 5. Walk a window so the muxa indicator on the status bar updates from
#    `working` → `waiting_input` → `idle`.
Ctrl+B
Type "n"
Sleep 1800ms

# 6. Detach.
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
Type "rm -rf /tmp/muxa-demo-shim /tmp/muxa-demo-bashrc $MUXA_SOCKET"
Enter
Show
