# muxa demo script — regenerate `docs/demo.gif` via:
#
#   vhs docs/demo.tape
#
# Requires charmbracelet/vhs, ttyd, ffmpeg, and a Chromium-based browser.
# docs/demo-setup.sh handles the muxad + tmux bootstrap so this file
# stays focused on the recording.
#
# Visible flow (the recording starts already attached to tmux):
#   1. muxa status                — table, run from inside the tmux pane
#   2. prefix + s                 — muxa watch popup (the switcher)
#   3. j/j                        — walk the agent list
#   4. p                          — open the preview popup over the table
#   5. f                          — toggle full-screen for long content
#   6. f                          — back to popup
#   7. q                          — close preview, back to picker
#   8. Enter                      — switch to the selected pane
#   9. prefix + d                 — detach

Output docs/demo.gif

Set Shell bash
Set FontSize 14
Set Width 1200
Set Height 720
Set Theme "TokyoNight"
Set TypingSpeed 45ms
Set PlaybackSpeed 1

# Prelude — invisible setup. We attach to the demo tmux server inside
# this same shell so the recording starts already inside tmux.
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
# Attach in-place so the recording opens already inside the tmux session.
# `exec` replaces the bash, so vhs records the tmux UI directly.
Type "exec /usr/bin/tmux -L muxa-demo attach -t main:0"
Enter
Sleep 1500ms
Show

# 1. muxa status — read the data, from inside the tmux pane. The
#    status-right at the top already reflects this pane's agent state,
#    so the viewer sees both surfaces at once.
Type "muxa status"
Enter
Sleep 2800ms

# 2. prefix + s — pop the muxa watch picker. This is the "switch"
#    experience: same gesture as tmux's stock prefix + s, but with
#    agent-aware rows.
Ctrl+B
Type "s"
Sleep 2500ms

# 3. Walk down the agent list so the highlight + detail line both move.
Type "j"
Sleep 700ms
Type "j"
Sleep 1200ms

# 4. p — open the preview popup over the table. Full prompt + last
#    response in a centred 80% × 70% box; the surrounding rows stay
#    visible behind it so you don't lose your place.
Type "p"
Sleep 3000ms

# 5. f — toggle full-screen for long content where the popup wraps too
#    aggressively. Footer hint flips to "f popup" so the next press is
#    self-documenting.
Type "f"
Sleep 2200ms

# 6. f — back to popup.
Type "f"
Sleep 1500ms

# 7. q — close the preview, return to the picker.
Type "q"
Sleep 800ms

# 8. Enter — switch the client to the selected pane. The popup closes
#    and tmux pulls us into the chosen window.
Enter
Sleep 1800ms

# 9. Detach — clean exit out of the demo server.
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
