# Human attention in collaboration mailboxes

Agent instructions ship through MCP initialization, `muxa_collaboration_guide`,
and the managed `muxa-collaboration` skill. Install/update the file-based entry
points with `muxa init --component agent-instructions,agent-skills --yes
--start-daemon=false`. Existing agents must reread the skill or reconnect MCP
to acquire updated guidance; changing a file does not replace their context.

Collaboration requests addressed to an agent are internal traffic. The sender
being a console, a human initiator, or words such as "approval" in a message
do not make that request an operator notification.

When an operator decision is required, send a separate request to `human`:

```sh
muxa msg send human 'Choose A or B. Recommend A because it preserves compatibility.' \
  --human-action choice --parent req_parent --json
```

The equivalent MCP `muxa_send_message` arguments are `target: "human"`,
`human_action: "choice"`, `body`, and optionally `parent_request_id`.
Actions are `approval`, `choice`, and `information`. Explain the decision,
why it blocks progress, the options, and your recommendation in the body.
Do not escalate ordinary peer review, progress, retries, or a pending reply.

Only a participant in the parent request may escalate it to the operator.
The parent thread is retained. Human replies return to the requesting agent's
normal reply mailbox. No pane is woken to deliver a request to a human.

`muxa watch` opens its mailbox on **Need you**. `Tab` switches to agent
traffic/history and sent messages. `e` answers a human request as the operator;
claiming an agent inbox is unnecessary. Muxa.app uses the same default tab.
Completed, cancelled, expired, failed, declined, or blocked requests no longer
count as awaiting a response. Legacy console-targeted requests are classified
by recipient, `expects_reply`, kind and status; no message text is scanned.

The dashboard graph preserves request/reply directions and shows **need you**
counts on edges. Counts apply to loaded history (which may be paginated).
Muxa.app also shows a chronological participant graph for the latest 20 loaded
requests: solid request arrows, dashed return arrows and orange human waits.

With the existing `[notifier] enabled = true, backend = "libnotify"` setting,
new human requests enter a separate desktop notification queue. Successful
delivery is recorded durably; polling, progress updates and daemon restarts do
not send the same request again. Failed delivery is retried. Resolving a request
removes it from the queue. This is a notification receipt, not an approval.
There is a small post/receipt crash window; desktop delivery cannot be exactly
once across a crash. Ordinary runtime error/permission notifications keep their
existing policy: a pending peer request alone does not prove an input prompt
is safe to suppress.

Linux builds validate the Rust and dashboard changes. SwiftUI rendering must
be built and visually verified on macOS before publishing a Muxa.app bundle.
