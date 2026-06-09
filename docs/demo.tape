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
#   2. muxa stats                 — duration summary with WORK/WAIT/HUMAN/THINK
#   3. muxa activity              — raw human interaction intervals
#   4. prefix + s                 — muxa watch session dashboard
#                                   Cursor lands on the current session
#                                   thanks to `set_initial_pane`, so the
#                                   preview acts on a row with real prompt
#                                   + response data.
#   5. p                          — open the preview popup over the table
#   6. j j                        — scroll the assistant response inside
#                                   the popup to show the multi-line body
#   7. f                          — toggle full-screen for long content
#   8. f                          — back to popup
#   9. q                          — close preview, back to picker
#  10. j / k                      — walk the session list, watch the detail
#                                   line update for each row
#  11. Enter                      — switch to the selected pane
#  12. prefix + d                 — detach

Output docs/demo.gif

Set Shell bash
Set FontFamily "JetBrainsMono Nerd Font Mono"
Set FontSize 15
Set Width 1200
Set Height 720
Set Theme "GitHub Dark"
Set TypingSpeed 45ms
Set PlaybackSpeed 1

# Prelude — invisible setup. We attach to the demo tmux server inside
# this same shell so the recording starts already inside tmux.
Hide
Type "export PATH=$HOME/.cargo/bin:/tmp/muxa-demo-shim:$PATH"
Enter
Type "export LANG=C.UTF-8"
Enter
Type "export LC_ALL=C.UTF-8"
Enter
Type "export MUXA_SOCKET=/tmp/muxa-demo.sock"
Enter
Type "export MUXA_CONFIG=/tmp/muxa-demo-config.toml"
Enter
Type "export XDG_DATA_HOME=/tmp/muxa-demo-data"
Enter
Type "cat > $MUXA_CONFIG <<'EOF'"
Enter
Type "[ui]"
Enter
Type "theme = 'high-contrast'"
Enter
Type "[watch]"
Enter
Type "theme = 'high-contrast'"
Enter
Type "view = 'session'"
Enter
Type "sort = ['session']"
Enter
Type "[watch.preview]"
Enter
Type "default_content = 'prompt_response'"
Enter
Type "EOF"
Enter
Type "rm -rf $XDG_DATA_HOME"
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
Type "exec /usr/bin/tmux -u -L muxa-demo attach -t main:0"
Enter
Sleep 1500ms
Show

# 1. muxa status — read the data, from inside the tmux pane. The
#    status-right at the top already reflects this pane's agent state,
#    so the viewer sees both surfaces at once.
Type "muxa status"
Enter
Sleep 2200ms

# 2. muxa stats — show that muxa is more than live status. The demo seed
#    writes activity.ndjson intervals, so WORK / WAIT / HUMAN / THINK are
#    already populated instead of waiting for a long recording.
Type "clear"
Enter
Type "muxa stats --since today --limit 4"
Enter
Sleep 3200ms

# 3. muxa activity — raw ledger view for debugging stats/report inputs.
Type "clear"
Enter
Type "muxa activity --since today --type human --limit 4"
Enter
Sleep 2600ms

Type "clear"
Enter

# 4. prefix + s — pop the muxa watch session dashboard. This is the
#    "switch" experience: same gesture as tmux's stock prefix + s, but
#    with agent-aware session rows. Cursor lands on the current session
#    via `set_initial_pane`, and the status gutter shows multi-agent,
#    single-agent, and bare sessions without moving the session names.
Ctrl+B
Type "s"
Sleep 2200ms

# 5. p — open the preview popup. The selected claude row has a real
#    prompt + a multi-paragraph assistant response (seeded via fake
#    transcript), so the popup body is full of content rather than
#    `—` placeholders.
Type "p"
Sleep 2400ms

# 6. j j — scroll the assistant response inside the popup so the
#    viewer sees the body wraps and scrolls as expected.
Type "j"
Sleep 600ms
Type "j"
Sleep 1200ms

# 7. f — toggle full-screen for long content where the popup wraps too
#    aggressively. Footer hint flips to "f popup" so the next press is
#    self-documenting.
Type "f"
Sleep 1700ms

# 8. f — back to popup.
Type "f"
Sleep 1000ms

# 9. q — close the preview, return to the picker.
Type "q"
Sleep 700ms

# 10. Walk a row or two so the detail line updates for each highlight —
#     showcases the `{last_response|last_prompt}` fallback while keeping
#     the compact session status gutter aligned.
Type "j"
Sleep 1000ms
Type "k"
Sleep 800ms

# 11. Enter — switch the client to the selected pane. The popup closes
#    and tmux pulls us into the chosen window.
Enter
Sleep 1500ms

# 12. Detach — clean exit out of the demo server.
Ctrl+B
Type "d"
Sleep 600ms

# Postlude — tear down everything.
Hide
Type "/usr/bin/tmux -u -L muxa-demo kill-server 2>/dev/null"
Enter
Type "kill %1 2>/dev/null"
Enter
Type "wait 2>/dev/null"
Enter
Type "rm -rf /tmp/muxa-demo-shim /tmp/muxa-demo-bashrc /tmp/muxa-demo-transcripts $MUXA_SOCKET"
Enter
Type "rm -rf $XDG_DATA_HOME"
Enter
Type "rm -f $MUXA_CONFIG"
Enter
Show
