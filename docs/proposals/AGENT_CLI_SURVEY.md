# Agent CLI candidates (survey, 2026-11-12)

Status: proposal. Nothing here is implemented. It records which agent CLIs
tend should integrate next after the Gemini CLI removal and the opencode
rewrite, judged by the [Direction](../DIRECTION.md) rule: support is claimed
only at the depth where the provider gives deterministic signals (hook or
plugin events with a session id, provider session records, or a local API).

Sources are the projects' own docs and release pages as of the survey date;
see the links per tool. The survey itself ran no binaries; Copilot CLI was
later checked on a real install (section below).

| Tool | What it is | Deterministic signals | Activity | Adapter cost | Recommendation |
| --- | --- | --- | --- | --- | --- |
| **GitHub Copilot CLI** | GitHub's terminal coding agent | Official hooks in `~/.copilot/hooks/*.json` (`$COPILOT_HOME`), JSON on stdin with `sessionId`, `cwd`; events `sessionStart`, `userPromptSubmitted` (`prompt`), `preToolUse`/`postToolUse` (`toolName`), `notification`, `agentStop` (`transcriptPath`), `sessionEnd`, `errorOccurred`. Session records in `~/.copilot/session-state/<id>/events.jsonl` | v1.0.x, frequent releases, documented hook reference | Small–medium: hook writer + normalizer, but a new `AgentKind` touches every exhaustive match, MCP enums and the Mac marks | **Next.** Best-documented hook contract after Claude/Codex. Checked on 1.0.110: hooks alone miss Esc, so it needs a session-log reader too (below) |
| **Cursor CLI** (`agent` / `cursor-agent`) | Cursor's agent in the terminal | `~/.cursor/hooks.json`; payloads carry `conversation_id`, `generation_id`, `transcript_path`, `hook_event_name`; `stop` has `status` (completed/aborted/error) | Closed source, dated releases; which events fire in the CLI varies by version (a Linux build was reported firing none) | Small–medium | **After Copilot**, labelled beta, each event verified on a real build before it is claimed |
| **T3 Code** | A local server plus desktop/web/mobile GUI that drives other agents (Codex app-server, Claude Agent SDK, opencode SDK, …) | No pane-level hooks. Exposes an OAuth-gated MCP endpoint, a `t3 thread start/send/show/list` CLI, and an internal event-sourced SQLite store (not a contract) | Very early (0.1.x), nightly builds | Not a pane adapter at all; a bridge would be medium–large | **No pane adapter.** Revisit as a Fleet-style bridge once its thread API stabilises |
| Kiro CLI | AWS's agent CLI | V3 hooks in `~/.kiro/hooks/` with `session_id`, `hook_event_name`, `cwd` | Mid-migration to V3; payload docs incomplete | Small–medium | Later |
| Amp | Sourcegraph's agent | Documented in-process TypeScript plugin API (`~/.config/amp/plugins/`, run with Bun): `session.start` and `tool.call` carry `thread.id`; `tool.result` has `status`; `agent.start` / `agent.end` frame each turn | Active; core plugin API documented, only the `experimental` namespace is marked unstable | Medium (same shape as the opencode plugin bridge) | **Later, strong candidate.** The signals are deterministic; it ranks behind Copilot only because a plugin bridge needs in-process code and more state than a hooks file |

Not surveyed in depth: Goose, Qwen Code, Crush, Kimi CLI, Factory droid. They
keep their screen-detection manifests where tend already ships one.

## Notes

- **Copilot reads repository `.claude/settings.json` hooks.** tend writes
  Claude hooks at user level (`~/.claude/settings.json`), so this should not
  double-report, but an adapter must still tell the providers apart rather
  than trust which hook command fired.
- **Copilot's PascalCase events** switch payloads to a snake_case
  "VS Code compatible" shape (`session_id`, `hook_event_name`). Writing the
  camelCase events keeps one documented shape.
- **T3-launched agents.** If T3 Code runs Claude or Codex with the user's own
  settings, their tend hooks or Codex rollouts may fire without a pane. tend
  already treats paneless rows as such; verify on a real install before
  claiming anything about T3.

## Copilot CLI checked on the real binary (2026-11-13)

GitHub Copilot CLI 1.0.110 (`npm i @github/copilot`) was run on macOS with an
isolated `COPILOT_HOME`, a user hooks file that only logged each event, and a
private tmux socket. It ran three interactive turns: a read-only shell call, a
shell call behind a permission prompt, and Esc on that prompt.

What the hooks deliver (`~/.copilot/hooks/*.json`, `{"version":1,"hooks":{…}}`,
camelCase events, JSON on stdin):

| Event | Payload (beyond `sessionId`, `timestamp`, `cwd`) | Use |
| --- | --- | --- |
| `sessionStart` | `source` (`new`), `initialPrompt` | Started |
| `userPromptSubmitted` | `prompt` | working |
| `preToolUse` | `toolCallId`, `toolName` (`bash`), `toolArgs.command` | tool start |
| `permissionRequest` | `toolName`, `toolInput`, `permissionSuggestions` | **not** a wait: it also fires for calls auto-approved 30 ms later |
| `notification` | `notification_type: "permission_prompt"`, `message`, `title` | the real "waiting on you" signal |
| `postToolUse` | `toolCallId`, `toolName`, `toolArgs`, result | tool end |
| `agentStop` | `transcriptPath` | turn end |

Every hook process inherited `TMUX_PANE`, so pane binding works like Claude
Code's.

**The gap: Esc fires no hook** (observed on 1.0.110, Esc on a permission
prompt). Cancelling the prompt (and so the turn) produced no `postToolUse`, no `agentStop` and no `notification`, even
75 s later. A hooks-only adapter would leave the row waiting (or working)
until the next prompt. The cancel is recorded deterministically in Copilot's
own session log, `$COPILOT_HOME/session-state/<sessionId>/events.jsonl`:

```
permission.completed {"result": {"kind": "cancelled", "reason": "Session aborted"}, "toolCallId": …}
assistant.turn_end   {"turnId": "0"}
abort                {"reason": "user_initiated"}
```

The same log also has `assistant.turn_start` / `assistant.turn_end`,
`tool.execution_start` / `tool.execution_complete`, `permission.requested`
and per-turn `session.usage_record`.

**So a correct adapter is hooks plus a session-log reader**, the shape tend
already uses for Codex (hooks plus rollout reading in the reconciler):
hooks for live transitions and the pane, and `events.jsonl` (`abort`,
`assistant.turn_end`) to settle a turn the hooks never close. It was not
built in wave 3, for these reasons:

- The reader belongs in the reconciler, which another stream owns.
- A new `AgentKind` touches about 40 files, including the MCP enums and the
  Mac app.
- Shipping the hooks half alone would add a provider whose rows stick after
  an Esc on a permission prompt (the path tested on 1.0.110; other Esc paths
  were not tried).

Notes for whoever builds it:

- Use `notification` (`permission_prompt`), not `permissionRequest`, for
  waiting input.
- Copilot also runs a repository's `.claude/settings.json` hooks under Claude
  event names, so a repo that installs tend's Claude hooks at project level
  would report a Copilot session as Claude. The adapter has to tell them
  apart, and the discriminator must be verified on that compatibility path:
  the reference documents snake_case `session_id` / `hook_event_name` for
  the PascalCase Claude-compatible events, and the camelCase fields seen in
  this run (`sessionId`, `hookName` on `permissionRequest`) were only
  observed on 1.0.110's native events, not promised.
- The installer writes `$COPILOT_HOME/hooks/tend.json` (default
  `~/.copilot`). Copilot asks for folder trust before loading repo hooks;
  user hooks load without it.

## Sources

- Copilot CLI hooks: https://docs.github.com/en/copilot/reference/hooks-reference
- Copilot CLI releases: https://github.com/github/copilot-cli/releases
- Cursor hooks: https://cursor.com/docs/agent/hooks
- Cursor CLI on Linux not firing hooks: https://forum.cursor.com/t/cursor-agent-cli-never-invokes-hooks-json-on-linux-2026-08-11-same-version-works-on-macos/168326
- T3 Code: https://github.com/pingdotgg/t3code, `docs/user/outside-agents.md`, `docs/internals/overview.md`
- Kiro hooks: https://kiro.dev/docs/cli/v3/hooks-migration/
- Amp plugin API: https://ampcode.com/docs/plugin-api; setup: https://ampcode.com/docs/customize/plugins
