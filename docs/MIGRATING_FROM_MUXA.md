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
   (a `CLAUDE_CONFIG_DIR` other than `~/.claude`), and `-v` to see each changed line.
3. Apply it:

   ```bash
   python3 scripts/migrate-from-muxa.py --apply
   ```

4. Run `tend init` to install the `tendd` service, open a new shell, and
   restart your agents so their hooks and MCP server call `tend`.
5. Remove Muxa once Tend works: `brew uninstall muxa` (or
   `cargo uninstall muxa-cli muxad`) and move `Muxa.app` to the Trash.

Run the script on every host that ran Muxa. Without Python, copy the script
to a host that has it, or follow the table above by hand.

## What the script does

- Stops `muxad` and its launchd job, systemd unit or brew service, and removes the old unit files.
- Moves Muxa's data, config and cache directories to `tend`. If a `tend`
  directory already exists, it leaves both in place and tells you.
- Renames Muxa's own files in full: `config.toml`, user agent definitions,
  and the agent integration bundle (`bootstrap.md`, `manifest.json`, the collaboration skill).
- In other tools' config it changes only what Muxa wrote: hook commands,
  the MCP server entry, managed blocks, `MUXA_*` variables, `@muxa_*`
  options and Muxa's own paths. A repository checked out at `~/code/muxa`
  keeps its path.
- Relinks the `tend-collaboration` skill for Claude Code and Codex.
- Copies `@muxa_*` options on running tmux sessions, windows and panes to
  `@tend_*`, so Work that is already running keeps its identity.
- macOS: copies app preferences from `dev.muxa.mac` to `dev.tend.mac`, and
  Ask provider API keys from the `dev.muxa.mac.ask-provider` keychain
  service to `dev.tend.mac.ask-provider`. macOS asks once per key to allow the read.

Every edited file is first copied to `<file>.muxa-migrate-<timestamp>`.
Running the script again after a successful run finds nothing to do.

`--only-files` skips services, live tmux, preferences and the keychain.
`--skip-tmux` leaves live tmux sessions alone.
