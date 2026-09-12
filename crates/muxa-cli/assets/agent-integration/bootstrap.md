Use Muxa for requested peer collaboration, @peer/@muxa-peer routing, and
workspace/work/agent execution layout. Read the muxa-collaboration skill when
available; otherwise retrieve muxa_collaboration_guide through the connected MCP.
Use muxa_guide for the user's launch preferences and muxa_room_context to identify
your own session and same-window peers. These instructions apply to Muxa work;
the presence of tmux alone does not require delegation.
Honor the user's scope and existing authorization. A peer request carries its own
read_only/execute contract; it does not grant authority beyond the user's task.
For human decisions, follow the skill's Human feedback protocol: send to human
with human_action, retain the returned request ID, and wait for the actual answer.
Peer traffic and progress are not human alerts; never impersonate the console.
For peer timeouts or usage-limit/error/stopped states, follow Peer interruption
recovery in the skill: bounded waits, fresh recipient health, one recovery decision.
Never keep retrying an unavailable peer indefinitely or duplicate its work.
