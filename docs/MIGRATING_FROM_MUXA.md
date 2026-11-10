# Migrating from Muxa

Muxa is now Tend. Tend reads none of Muxa's names, so an existing install has
to be moved over once per host. `scripts/migrate-from-muxa.py` does that.

## What changed

| Muxa | Tend |
| --- | --- |
| `muxa`, `muxad` | `tend`, `tendd` |
| `brew install open330/tap/muxa` | `brew install open330/tap/tend` |
| `Muxa.app` (`dev.muxa.mac`) | `Tend.app` (`dev.tend.mac`) |
| `MUXA_SOCKET`, `MUXA_CONFIG`, … | `TEND_SOCKET`, `TEND_CONFIG`, … |
| `~/Library/Application Support/muxa` (macOS), `~/.local/share/muxa`, `~/.config/muxa` (Linux) | the same paths with `tend` |
| `/tmp/muxa-<uid>.sock` | `/tmp/tend-<uid>.sock` |
| `muxa hook …` in Claude Code, Codex, Gemini, Antigravity and opencode | `tend hook …` |
| MCP server `muxa`, tools `muxa_*` | MCP server `tend`, tools `tend_*` |
| skill `muxa-collaboration` | skill `tend-collaboration` |
| tmux options `@muxa_*` | `@tend_*` |
| peer messages `[muxa:req_…]` | `[tend:req_…]` |
| launchd `dev.open330.muxad`, systemd `muxad.service` | `dev.open330.tendd`, `tendd.service` |

Peers only talk to peers on the same name. Upgrade every host in a fleet
before relying on peer requests again.

## Steps

1. Install Tend: `brew install open330/tap/tend` (and `brew install --cask open330/tap/tend-app` for the Mac app).
2. Look at the plan. Nothing changes yet:

   ```bash
   python3 scripts/migrate-from-muxa.py
   ```

   Add `--claude-home DIR` for each extra Claude Code config directory
   (a `CLAUDE_CONFIG_DIR` other than `~/.claude`), `--codex-home DIR` for each
   extra Codex one (a `CODEX_HOME` other than `~/.codex`), and `-v` to see each changed line.
3. Apply it:

   ```bash
   python3 scripts/migrate-from-muxa.py --apply
   ```

4. Run `tend init` to install the `tendd` service, open a new shell, and
   restart your agents so their hooks and MCP server call `tend`.
5. Remove Muxa once Tend works: `brew uninstall muxa` (or
   `cargo uninstall muxa-cli muxad`) and move `Muxa.app` to the Trash.

Run the script on every host that ran Muxa. It needs Python 3.8+, and 3.11+
to edit TOML (Tend's `config.toml`, Codex's `config.toml`): with an older
Python, such as the one macOS ships, those two files are reported for a manual
edit instead. `brew install python` gives a newer one.

## What the script does

- Stops `muxad` and its launchd job, systemd unit or brew service, and removes the old unit files.
- Moves Muxa's data, config and cache directories to `tend`. If a `tend`
  directory already exists, it leaves both in place and tells you.
- Renames the agent integration bundle (`bootstrap.md`, the collaboration
  skill) in full. Renames the fleet key `muxa_path` to `tend_path`. In `config.toml`, user agent definitions and every other
  tool's config it changes only what Muxa wrote: hook commands (including an
  absolute `/opt/bin/muxa`), the MCP server entry, managed blocks, `MUXA_*`
  variables, `@muxa_*` options and paths into the directories it moved.
  Your own values keep their spelling: a `cwd` or repository at
  `~/code/muxa`, a URL, a project trust entry.
- Leaves both entries in place and tells you when Tend's already exists:
  an MCP server named `tend`, `[mcp_servers.tend]` in Codex, `tend.ts`, a
  `tend-collaboration` link, or a Tend key in the keychain.
- Edits `.claude.json` under Claude Code's own lock, edits the file a
  symlinked dotfile points to (keeping the link), and writes a TOML file
  only after checking that the result still parses.
- Relinks the `tend-collaboration` skill for Claude Code and Codex.
- Copies `@muxa_*` options on running tmux sessions, windows and panes to
  `@tend_*`, so Work that is already running keeps its identity.
- macOS: copies app preferences from `dev.muxa.mac` to `dev.tend.mac`, and
  Ask provider API keys from the `dev.muxa.mac.ask-provider` keychain
  service to `dev.tend.mac.ask-provider`. The key is handed to `security`
  on standard input, never on a command line. macOS asks once per key to
  allow the read.

Your own launchd jobs, systemd units and crontab entries that run muxa are
listed at the end for you to update; the script does not edit them.

Every edited file is first copied to `<file>.muxa-migrate-<timestamp>`.
Running the script again after a successful run finds nothing to do.

`--only-files` skips services, live tmux, preferences and the keychain.
`--skip-tmux` leaves live tmux sessions alone.
