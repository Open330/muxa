# muxa demo script — regenerate `docs/demo.gif` via:
#
#   vhs docs/demo.tape
#
# Requires charmbracelet/vhs, ttyd, ffmpeg, and a Chromium-based browser
# (VHS uses Chrome for frame capture). docs/demo-setup.sh handles the
# muxad + tmux bootstrap so this file stays focused on the recording.
#
# The visible flow: muxa status (table) → attach to a real tmux session
# with muxa wired into status-right → walk a couple of panes to show the
# glyph update live → drop into muxa watch from inside tmux → detach.

Output docs/demo.gif

Set Shell bash
Set FontSize 14
Set Width 1200
Set Height 720
Set Theme "TokyoNight"
Set TypingSpeed 45ms
Set PlaybackSpeed 1

# Prelude — invisible setup. Spins up a scratch muxad, builds an isolated
# tmux server with three sessions / five panes, seeds three of the panes
# as agents, and configures status-right to render muxa status-line live.
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

Type "# muxa status — every tracked agent at a glance"
Sleep 800ms
Enter
Type "muxa status"
Enter
Sleep 2500ms

Type "# attach to a tmux session — muxa is wired into status-right"
Sleep 800ms
Enter
Type "/usr/bin/tmux -L muxa-demo attach -t main"
Enter
Sleep 2500ms

# Walk a couple of windows so the glyph in status-right updates per pane.
Ctrl+B
Type "n"
Sleep 1800ms
Ctrl+B
Type "n"
Sleep 1800ms
Ctrl+B
Type "p"
Sleep 1500ms

# Drop into the fullscreen TUI from inside tmux.
Type "muxa watch"
Enter
Sleep 4500ms
Type "j"
Sleep 300ms
Type "j"
Sleep 600ms
Type "q"
Sleep 800ms

# Detach from tmux — the recording ends back in the bare shell.
Ctrl+B
Type "d"
Sleep 800ms

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
