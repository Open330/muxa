# Agent CLI candidates (survey, 2026-11-12)

Status: proposal. Nothing here is implemented. It records which agent CLIs
tend should integrate next after the Gemini CLI removal and the opencode
rewrite, judged by the [Direction](../DIRECTION.md) rule: support is claimed
only at the depth where the provider gives deterministic signals (hook or
plugin events with a session id, provider session records, or a local API).

Sources are the projects' own docs and release pages as of the survey date;
see the links per tool. Nothing was run against a live binary.

| Tool | What it is | Deterministic signals | Activity | Adapter cost | Recommendation |
| --- | --- | --- | --- | --- | --- |
| **GitHub Copilot CLI** | GitHub's terminal coding agent | Official hooks in `~/.copilot/hooks/*.json` (`$COPILOT_HOME`), JSON on stdin with `sessionId`, `cwd`; events `sessionStart`, `userPromptSubmitted` (`prompt`), `preToolUse`/`postToolUse` (`toolName`), `notification`, `agentStop` (`transcriptPath`), `sessionEnd`, `errorOccurred`. Session records in `~/.copilot/session-state/<id>/events.jsonl` | v1.0.x, frequent releases, documented hook reference | Small–medium: hook writer + normalizer, but a new `AgentKind` touches every exhaustive match, MCP enums and the Mac marks | **Next.** Best-documented hook contract after Claude/Codex |
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

## Sources

- Copilot CLI hooks: https://docs.github.com/en/copilot/reference/hooks-reference
- Copilot CLI releases: https://github.com/github/copilot-cli/releases
- Cursor hooks: https://cursor.com/docs/agent/hooks
- Cursor CLI on Linux not firing hooks: https://forum.cursor.com/t/cursor-agent-cli-never-invokes-hooks-json-on-linux-2026-08-11-same-version-works-on-macos/168326
- T3 Code: https://github.com/pingdotgg/t3code, `docs/user/outside-agents.md`, `docs/internals/overview.md`
- Kiro hooks: https://kiro.dev/docs/cli/v3/hooks-migration/
- Amp plugin API: https://ampcode.com/docs/plugin-api; setup: https://ampcode.com/docs/customize/plugins
