# muxa demo script — regenerate `docs/demo.gif` via:
#
#   vhs docs/demo.tape
#
# Requires charmbracelet/vhs (https://github.com/charmbracelet/vhs) which in
# turn uses ttyd + ffmpeg under the hood.

Output docs/demo.gif

Set Shell bash
Set FontSize 14
Set Width 1200
Set Height 720
Set Theme "Tokyo Night"
Set TypingSpeed 45ms
Set PlaybackSpeed 1

# Prelude: start a fresh daemon on a scratch socket and seed some fake agents.
Hide
Type "export MUXA_SOCKET=/tmp/muxa-demo.sock"
Enter
Type "rm -f $MUXA_SOCKET && muxad &"
Enter
Sleep 500ms
Type "until [ -S $MUXA_SOCKET ]; do sleep 0.1; done"
Enter
Type "clear"
Enter
Show

Type "# seed a few agents"
Sleep 800ms
Enter

Hide
Type "TMUX_PANE=%10 bash -lc 'echo {\"session_id\":\"s-a\",\"prompt\":\"refactor the ipc module to use length-prefixed frames\"} | muxa hook claude --event user_prompt_submit'"
Enter
Type "TMUX_PANE=%11 bash -lc 'echo {\"session_id\":\"s-b\",\"tool_name\":\"shell\"} | muxa hook codex --event permission_request'"
Enter
Type "TMUX_PANE=%12 bash -lc 'echo {\"session_id\":\"s-c\",\"prompt\":\"summarize this PR\"} | muxa hook gemini --event before_agent'"
Enter
Type "TMUX_PANE=%12 bash -lc 'echo {\"session_id\":\"s-c\"}                                | muxa hook gemini --event after_agent'"
Enter
Show

Type "muxa status"
Enter
Sleep 2s

Type "# one-liner for tmux status-right"
Sleep 800ms
Enter
Type "muxa status-line"
Enter
Sleep 1500ms

Type "# live dashboard"
Sleep 800ms
Enter
Type "muxa watch"
Sleep 200ms
Enter
Sleep 3s
Type "q"
Sleep 500ms

Hide
Type "pkill muxad; rm -f $MUXA_SOCKET"
Enter
Show
