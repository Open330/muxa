# muxa demo — "your whole agent fleet on one screen"
#
#   vhs docs/demo.tape   →   docs/demo.gif
#
# muxa watch is the hero. Arc:
#   0–6s    the fleet      — `muxa watch`: every session on one screen, working
#                            agents spinning, attention floated to the top
#   6–13s   the swarm      — `muxa watch --view swarm`: k9s-style clusters with
#                            live dot spinners and an expandable subagent tree
#   13–17s  teleport       — `muxa attend` jumps to whoever's blocked longest
#
# Depends on docs/demo-setup.sh standing up a rich isolated fleet (many
# sessions, varied states, in-flight Task subagents) and docs/demo-paint.sh
# painting the codex approval pane attend lands on.

Output docs/demo.gif

Set Shell bash
Set FontFamily "JetBrainsMono Nerd Font Mono"
Set FontSize 15
Set Width 1200
Set Height 720
Set Theme "GitHub Dark"
Set TypingSpeed 45ms
Set PlaybackSpeed 1

# ── Prelude — invisible setup (isolated demo daemon + tmux) ──────────────
Hide
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
Type "theme = 'oh-my-muxa'"
Enter
Type "[discovery]"
Enter
Type "enabled = false"
Enter
Type "[reconciler]"
Enter
Type "enabled = false"
Enter
Type "[state]"
Enter
Type "enabled = false"
Enter
Type "[history]"
Enter
Type "enabled = false"
Enter
Type "[activity]"
Enter
Type "enabled = false"
Enter
Type "[watch]"
Enter
Type "theme = 'oh-my-muxa'"
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
# Scope muxad's tmux scan to the demo server only, so a real tmux fleet on the
# same host can't leak into the recording. Resolve the socket path from a
# throwaway server, then let demo-setup recreate `-L muxa-demo` at that path.
Type "$MUXA_DEMO_TMUX -L muxa-demo new-session -d -s _probe cat"
Enter
Type "export MUXA_TMUX_SOCKET=$($MUXA_DEMO_TMUX -L muxa-demo display-message -p '#{socket_path}')"
Enter
Type "$MUXA_DEMO_TMUX -L muxa-demo kill-server"
Enter
Type "muxad --config $MUXA_CONFIG >/dev/null 2>&1 &"
Enter
Sleep 500ms
Type "until [ -S $MUXA_SOCKET ]; do sleep 0.1; done"
Enter
Type "bash docs/demo-setup.sh >/dev/null"
Enter
Sleep 400ms
Type "clear"
Enter
Type "exec $MUXA_DEMO_TMUX -u -L muxa-demo attach -t main:0"
Enter
Sleep 1500ms
Show

# ── 1. The fleet (0–6s) ──────────────────────────────────────────────────
# muxa watch: the whole fleet on one screen. Working agents spin; sort=state
# floats the agents that need you (input/choice) to the top; the gutter shows
# each session's state at a glance.
Type "muxa watch"
Enter
Sleep 3800ms
Down@600ms 2
Sleep 2400ms
Type "q"
Sleep 500ms

# ── 2. The swarm (6–13s) ─────────────────────────────────────────────────
# The k9s-style swarm console: one cluster per session, animated dot spinners
# on the working agents, and an expandable subagent tree under the agents that
# spawned Task children.
Type "muxa watch --view swarm"
Enter
Sleep 5500ms
Down@700ms 2
Sleep 1600ms
Type "q"
Sleep 500ms

# ── 3. Teleport (13–17s) ─────────────────────────────────────────────────
# Don't hunt across windows — jump the tmux client straight to whichever agent
# has been blocked the longest.
Type "muxa attend"
Enter
Sleep 3400ms
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
