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

Restart running agents afterward to reload global instructions and MCP.
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
