# muxa demo — "which of your agents needs you right now?"
#
#   vhs docs/demo.tape   →   docs/demo.gif
#
# Story arc (see docs/demo-README.md):
#   0–3s   overwhelm     — muxa status: claude working, codex blocked, gemini waiting
#   3–7s   the accountant — muxa stats: where the day actually went
#   7–12s  the god view  — prefix+s → muxa watch, all agents one screen, with the
#                          real prompt/response in the preview
#   12–17s the finale    — `muxa attend` teleports to the longest-blocked agent
#                          (codex, mid shell-approval) — the strongest note to end on
#
# Depends on docs/demo-setup.sh painting the codex/gemini panes with
# docs/demo-paint.sh instead of bare `cat`, so attend/attach land on a
# believable agent frame rather than an empty shell.

Output docs/demo.gif

Set Shell bash
Set FontFamily "JetBrainsMono Nerd Font Mono"
Set FontSize 15
Set Width 1200
Set Height 720
Set Theme "GitHub Dark"
Set TypingSpeed 45ms
Set PlaybackSpeed 1

# ── Prelude — invisible setup (same isolation as the old tape) ───────────
Hide
# Resolve the real tmux BEFORE the shim shadows it on PATH, so attach/detach
# work whether tmux lives at /usr/bin (Docker/CI) or Homebrew (mac).
Type "export MUXA_DEMO_TMUX=$(command -v tmux)"
Enter
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
Type "sort = ['state']"
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
Type "exec $MUXA_DEMO_TMUX -u -L muxa-demo attach -t main:0"
Enter
Sleep 1500ms
Show

# All shell commands run FIRST, from the main:0 bash pane, so nothing is
# ever typed into a non-shell (painted) pane. `muxa attend` teleports us
# away for the finale, so it must come last.

# ── 1. Overwhelm (0–3s) ──────────────────────────────────────────────────
# status-right at the top already shows this pane's agent glyph. The table
# makes the fleet legible: one working, one blocked on approval, one waiting.
Type "muxa status"
Enter
Sleep 3000ms

# ── 2. The accountant (3–7s) ─────────────────────────────────────────────
# muxa also quantifies the day: how long agents worked, how long they sat
# blocked on you, how long you actually spent driving.
Type "clear"
Enter
Type "muxa stats --since today --limit 4"
Enter
Sleep 3200ms

# ── 3. The god view (7–12s) ──────────────────────────────────────────────
# prefix + s pops muxa watch. sort=['state'] floats blocked/error agents to
# the top; the status gutter shows multi/single/bare sessions at a glance.
Type "clear"
Enter
Ctrl+B
Type "s"
Sleep 2200ms
# p — preview the highlighted agent's real prompt + response body
# (default_content=prompt_response), then q q back out to the shell.
Type "p"
Sleep 2600ms
Type "q"
Sleep 400ms
Type "q"
Sleep 600ms

# ── 4. The finale — teleport to whoever needs you (12–17s) ───────────────
# Don't hunt across windows. One command jumps the tmux client to whichever
# agent has been blocked the longest — here, codex on the shell approval.
# demo-paint.sh has painted a real-looking approval prompt, so the teleport
# lands somewhere that reads as genuine. Strongest note to end on.
Type "muxa attend"
Enter
Sleep 700ms
Sleep 3200ms
Ctrl+B
Type "d"
Sleep 600ms

# ── Postlude — tear everything down ──────────────────────────────────────
Hide
Type "$MUXA_DEMO_TMUX -u -L muxa-demo kill-server 2>/dev/null"
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
