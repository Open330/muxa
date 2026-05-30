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
#                                   Cursor lands on the user's current pane
#                                   (the claude agent on main:0) thanks to
#                                   `set_initial_pane`, so the next two
#                                   keystrokes already act on a row with
#                                   real prompt + response data.
#   3. p                          — open the preview popup over the table
#   4. j j                        — scroll the assistant response inside
#                                   the popup to show the multi-line body
#   5. f                          — toggle full-screen for long content
#   6. f                          — back to popup
#   7. q                          — close preview, back to picker
#   8. j / k                      — walk the agent list, watch the detail
#                                   line update for each row
#   9. Enter                      — switch to the selected pane
#  10. prefix + d                 — detach

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
Type "export MUXA_CONFIG=/tmp/muxa-demo-config.toml"
Enter
Type "cat > $MUXA_CONFIG <<'EOF'"
Enter
Type "[watch]"
Enter
Type "view = 'pane'"
Enter
Type "sort = ['pane_id']"
Enter
Type "EOF"
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
#    agent-aware rows. Cursor lands on the user's current pane (the
#    claude agent on main:0) via `set_initial_pane`, so the row already
#    under the highlight has rich prompt + response data ready for the
#    preview demo.
Ctrl+B
Type "s"
Sleep 2500ms

# 3. p — open the preview popup. The selected claude row has a real
#    prompt + a multi-paragraph assistant response (seeded via fake
#    transcript), so the popup body is full of content rather than
#    `—` placeholders.
Type "p"
Sleep 2800ms

# 4. j j — scroll the assistant response inside the popup so the
#    viewer sees the body wraps and scrolls as expected.
Type "j"
Sleep 600ms
Type "j"
Sleep 1500ms

# 5. f — toggle full-screen for long content where the popup wraps too
#    aggressively. Footer hint flips to "f popup" so the next press is
#    self-documenting.
Type "f"
Sleep 2000ms

# 6. f — back to popup.
Type "f"
Sleep 1200ms

# 7. q — close the preview, return to the picker.
Type "q"
Sleep 700ms

# 8. Walk a row or two so the detail line updates for each highlight —
#    showcases the `{last_response|last_prompt}` fallback (codex/gemini
#    fall back to last_prompt, claude shows the response).
Type "j"
Sleep 1000ms
Type "j"
Sleep 1000ms
Type "k"
Sleep 800ms

# 9. Enter — switch the client to the selected pane. The popup closes
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
Type "rm -rf /tmp/muxa-demo-shim /tmp/muxa-demo-bashrc /tmp/muxa-demo-transcripts $MUXA_SOCKET"
Enter
Type "rm -f $MUXA_CONFIG"
Enter
Show
