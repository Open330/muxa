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

## Interrupted peers and usage caps

The shared skill's **Peer interruption recovery** section and the MCP guide's
`workflows.peer_interruption` define coordinator recovery. A peer's quota/error
state and a durable reply are distinct: upgraded daemons return `peer_interrupted`
on known error/stopped recipients without fabricating a terminal reply. Use waits of at most 60 seconds plus a chosen overall budget,
then check fresh identity-matched recipient state. Do not use the request's
snapshot of `to.state` or usage percentage alone to declare failure.

Record the observed blocker once and use authorized recovery. Ask the operator
only when a real decision is needed, preserving a single linked human request.
Prevent overlapping execution before reassignment: claimed work may resume after
a reset and cannot be cancelled through the queued-request cancellation tool.
Do not impersonate the unavailable recipient or fabricate its terminal reply.
A coordinator ending its own incoming attempt returns `blocked` with evidence.

The daemon reconciles exact agent kind/session/pane/socket identity on mailbox
changes, agent transitions and a 30-second safety scan, including startup and
when terminal wake injection is disabled. It records an interruption on the
original request; local and Fleet waits return promptly with that metadata.
Unknown/missing identities are not assumed stopped. Remote hosts need the
updated daemon; old hosts continue to use bounded policy checks.

If the coordinator is unavailable, or does not acknowledge via a request update
within two minutes, one human choice request is persisted in the same transaction
as its link on the original request. Restarts do not duplicate it. Existing linked
human questions are reused. The service is explicitly identified as Muxa recovery;
it does not impersonate a peer. Human answers are copied to interruption.decision
for the coordinator; they do not execute any action or complete the original work.
Healthy working/idle state or an actual terminal reply clears the interruption and
withdraws an unanswered daemon-generated action. A reset time alone never clears it.

muxa.app shows reason/reset/original request/action identity and decision, with
editable reply drafts for wait/recheck, safe handoff and stopping an attempt. Its
Reply button records only the decision. Dashboard detail shows the same structured
recovery metadata; decision and progress bodies stay redacted without detail auth.
Installed agents should reread the skill or reconnect MCP for the updated contract.
