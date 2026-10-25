# Global agent integration

`muxa init` installs a shared collaboration skill, global entry points, and
user-scoped MCP registrations for detected Codex and Claude Code installations.
The `standard` and `full` presets include all three components. Other hosts keep
their existing hook integrations; their global instruction/skill adapters are
not installed by these components yet.

```bash
muxa init --component agent-instructions,agent-skills,agent-mcp --dry-run
muxa init --component agent-instructions,agent-skills,agent-mcp --yes

# Fresh installations also need hooks, collaboration, and the daemon.
muxa init --preset standard --yes
```

After replacing the Muxa binary, run `muxa reload mcp` to request an MCP refresh
from the existing Codex app-server. This uses `CODEX_HOME` (or `--codex-home DIR`)
and refreshes all configured MCP servers and loaded threads on that profile.
Without an explicit profile or `CODEX_HOME`, it requires `--codex-home` or `--all`
instead of guessing `~/.codex`. The profile's existing control socket is selected
explicitly, and socket aliases are deduplicated.
It reports `queued` after API acknowledgement, not that every connection has
already changed. Verify updated tools when the host next refreshes the thread.
Global agent instructions/skills can still require restarting the agent.
Other agent clients without a supported hot-reload API need manual reconnect.
MCP requires a reachable muxad. These three components do not enable the
mailbox by themselves: select `collaboration` or the standard preset as well.

## Canonical bundle

The binary embeds versioned assets from `crates/muxa-cli/assets/agent-integration/`.
Init installs them beside the resolved muxa config file, including a location
selected with `--config`:

```text
<muxa-config-directory>/
├── config.toml
└── agent-integration/
    ├── bootstrap.md
    ├── manifest.json
    └── skills/muxa-collaboration/
        ├── SKILL.md
        └── references/workflows.md
```

The default directory is `~/Library/Application Support/muxa` on macOS and
`$XDG_CONFIG_HOME/muxa` or `~/.config/muxa` on Linux. Paths containing spaces work.

| Component | Codex | Claude Code |
| --- | --- | --- |
| `agent-instructions` | Managed block in active global `AGENTS.md` or `AGENTS.override.md` | Managed block in global `CLAUDE.md` |
| `agent-skills` | `~/.agents/skills/muxa-collaboration` symlink | `~/.claude/skills/muxa-collaboration` symlink |
| `agent-mcp` | Merge `[mcp_servers.muxa]` in `config.toml` | Merge user `mcpServers.muxa` in `~/.claude.json` |

`CODEX_HOME` relocates Codex config and instructions; its shared personal skill
directory remains `~/.agents/skills`. `CLAUDE_CONFIG_DIR` relocates Claude's home,
including `.claude.json` inside that custom directory.

If you run a provider from more than one home (separate accounts or profiles,
however you switch between them), run init once per home with that home's own
variable, so every session finds the server:

```sh
for home in <each Claude home>; do CLAUDE_CONFIG_DIR=$home muxa init --component agent-mcp; done
```

`muxa doctor` checks the home of the session it runs in: an active
`CLAUDE_CONFIG_DIR` or `CODEX_HOME` whose config has no muxa server is reported
with the command that fixes it. Writes to `.claude.json` take Claude Code's own
`.claude.json.lock` so a running session cannot interleave with them, and keep
the file in the style it was written in.

The global block tells the agent when to read the canonical `bootstrap.md`.
It does not replace the whole instruction file or rely on Markdown links being
automatically imported. Existing dotfile symlinks remain links; edits go to their
resolved targets. A nonempty Codex override takes precedence. Re-running init
moves Muxa's entry point when that changes.

## Collaboration and preferences

The skill teaches Workspace/session, Work Run/window, and Agent/pane bindings;
same-window peer discovery; review and delegated implementation contracts;
durable request IDs; inbox/reply handling; and verified integration. Pane layout
does not isolate files: concurrent writers need disjoint paths or worktrees.

MCP supplies live identity and capabilities. `muxa_guide` supplies configured
launch preferences and `muxa_collaboration_guide` supplies the runtime contract.
Existing explicit authorization remains valid within its scope. A review alone
does not authorize editing or spawning a bypass-permission peer.

Keep preferences in `[mcp.guide]` in `config.toml`. The bundle is versioned
application content; `[message.skills]` remains a separate registry of outgoing
prompt templates, not agent `SKILL.md` files.

## Registration, updates, and removal

Init preserves other servers and settings. Recognized existing Muxa servers keep
their command, arguments, environment, and options. Codex receives any missing
`env_vars`: `RMUX`, `RMUX_PANE`, `TMUX`, `TMUX_PANE`, `MUXA_SOCKET`.
New registrations use `muxa` on PATH, the selected config, and pin a non-default
socket. Default sockets remain environment-routed. Existing custom registrations
keep their routing; change them explicitly when switching configs or sockets.

Ownership and previous MCP entries are recorded in the private `manifest.json`.
Files are backed up before edits; re-running does not duplicate blocks or links.
User-owned name collisions and locally modified assets/registrations are
preserved and reported. Changes made to a config after planning stop apply.

```bash
muxa doctor
muxa init --component agent-skills --uninstall
muxa init --component agent-instructions,agent-skills,agent-mcp --uninstall
```

Doctor checks recorded assets, links, global override shadowing, and MCP drift.
It checks configuration rather than launching agents or proving their sessions
reloaded it. After restarting, use `codex mcp list` / `claude mcp list` to confirm
the client sees the server.

Uninstall removes owned links and unchanged assets, strips only Muxa's global
blocks, and restores pre-existing MCP entries if the installed entry is unchanged.
Modified entries remain with a warning. Component-specific uninstall preserves
other components. Empty instruction files, bundle directories, the ownership
manifest, and backups may remain; no directories are recursively removed.

## Scoped reload

```bash
muxa reload mcp --dry-run --json
muxa reload mcp
muxa reload mcp --codex-home /path/to/codex-profile --json
muxa reload mcp --all --dry-run
muxa reload mux --yes
```

Bare `muxa reload` retains its existing snapshot/restart/restore behavior.
`reload mux` names that operation explicitly. Neither is needed for MCP refresh.
The MCP operation uses a WebSocket connection over the existing Unix control
socket of an already-running host, then the official `config/mcpServer/reload` API. It does
not start another app-server or restart the multiplexer.
This API has no individual MCP-server filter: the command prints the real scope.

`--all` selects distinct profiles owning stale shared Codex MCP processes.
Unidentified profiles and other agent owners are listed for manual reconnect;
it does not guess a profile or inject interactive menu commands.
States are `planned` (dry run), `queued` (API accepted), `reloaded` (verified Muxa connections), `unsupported`, `rejected`,
`unavailable` (no request submitted), or `unknown` (submission may have occurred
without acknowledgement). Each host has a bounded `--timeout-secs` (default 15,
maximum 60). An uncertain request is never automatically retried or escalated to
a broader restart. Transport/API failures return nonzero after reporting all
selected hosts, so a partially successful run retains its per-profile evidence.

Protocol reference: [OpenAI App Server documentation](https://learn.chatgpt.com/docs/app-server).


When the registered local Muxa binary changes, `reload mcp` records its SHA-256
under the reserved `mcp_servers.muxa.env.MUXA_MCP_BINARY_REVISION` key. Codex can
reuse live clients when their configuration is unchanged; this revision makes
the new binary a different client configuration. It updates only an existing
local `muxa mcp` registration, not remote/custom transports or disabled entries.
The official config writer receives the exact user-layer version to reject
concurrent changes, and a private config backup is made before the write (the latest three are retained).
An identical binary revision produces no config write. Unrelated user settings
and integration ownership are preserved. `--dry-run` reports
`binary_revision_update` without writing configuration.

API acceptance is `queued`. The command then checks each loaded Muxa connection's
server version and compact-report capability through `mcpServerStatus/list`.
`reloaded` means all observed enabled Muxa connections report the installed
revision. Exit 2 means accepted but still pending/unverified; exit 1 is a failure
or an owner requiring manual reconnect. Do not repeat a pending reload in a loop.
The old-process counters are diagnostic: captured runtimes can retain obsolete
processes even after fresh connections are available. These checks cover Muxa
connections, not the readiness of other configured MCP servers.

The command never restarts app-server or agents, and never starts model turns
solely to refresh tools. For clients without the API or custom entries that
cannot be revised, use their reconnect action. As a last resort after finishing
active work, the existing
`CODEX_HOME=/path/to/profile codex app-server daemon restart` reconnects a whole
Codex host and disconnects its clients; Muxa does not escalate to it automatically.
