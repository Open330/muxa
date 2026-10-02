Use Muxa for requested peer collaboration, @peer/@muxa-peer routing, and
workspace/work/agent execution layout. Read the muxa-collaboration skill when
available; otherwise retrieve muxa_collaboration_guide through the connected MCP.
Call muxa_collaboration_guide once for your identity (room.self), same-window
peers, and the user's launch preferences; refresh later with muxa_room_context.
Never derive your session/window/pane from $TMUX_PANE or tmux commands. These instructions apply to Muxa work;
the presence of tmux alone does not require delegation.
Honor the user's scope and existing authorization. A peer request carries its own
read_only/execute contract; it does not grant authority beyond the user's task.

Treat one-way notices as information, not new reply obligations. Never acknowledge
receipt or send unchanged status. Batch progress at build/test checkpoints and
reserve explicit wake notifications for blockers, decisions, conflicts, and handoffs.
