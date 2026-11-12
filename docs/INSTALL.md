# Install

This page keeps the detailed install and wiring notes out of the README.

## Requirements

- Rust 1.89+ (source builds only)
- tmux 3.x, or [rmux](https://rmux.io) 0.10+
- Linux or macOS; Windows natively through rmux (see [WINDOWS.md](WINDOWS.md))
  or inside WSL2
- One supported agent CLI: Claude Code, OpenAI Codex, the Google Antigravity
  CLI (`agy`), or opencode

## Try Before Installing

```bash
curl -fsSL https://raw.githubusercontent.com/Open330/tend/main/scripts/onboard.sh | sh
```

This path installs nothing. The script fetches the release archive for your
platform into a temporary directory, verifies its published SHA-256, runs the
real `tend onboard`, and deletes it on exit — no persistent daemon, config, or
PATH entry. The live tour uses tmux plus an isolated daemon and mailbox while it
runs, then removes the entire sandbox. Downloading through the launcher needs
network access, a supported release platform, a checksum tool, and `tar`; the
interactive tour also needs tmux. It fails clearly when it cannot download and
verify the release. Forward tour flags after `sh -s --`:

```bash
curl -fsSL https://raw.githubusercontent.com/Open330/tend/main/scripts/onboard.sh | sh -s -- --lang ko
curl -fsSL https://raw.githubusercontent.com/Open330/tend/main/scripts/onboard.sh | sh -s -- --print
```

After installing Tend, run `tend onboard` directly for the same sixteen-step
live tour, or `tend onboard --print` when tmux is unavailable.

## One-Shot Install

```bash
curl -fsSL https://raw.githubusercontent.com/Open330/tend/main/scripts/install.sh | sh
```

The script builds and installs `tend` and `tendd`, then runs `tend init`
to wire tmux, agent hooks, optional systemd, and the optional dashboard.
Forward flags to the wizard with `sh -s --`:

```bash
curl -fsSL https://raw.githubusercontent.com/Open330/tend/main/scripts/install.sh | sh -s -- --preset standard --yes
curl -fsSL https://raw.githubusercontent.com/Open330/tend/main/scripts/install.sh | sh -s -- --dry-run
```

## From Source

```bash
git clone https://github.com/Open330/tend.git
cd tend
cargo install --path crates/tendd --locked
cargo install --path crates/tend-cli --locked
tend init
```

`cargo install` writes to `~/.cargo/bin`; make sure that directory is on
your `PATH`.

## Homebrew

```bash
brew install open330/tap/tend
tend init
```

Installs pre-built `tend` and `tendd` binaries from
[Open330/homebrew-tap](https://github.com/Open330/homebrew-tap)
(macOS arm64/x86_64 and Linux arm64/x86_64). The formula ships a
`brew services`-compatible service for `tendd`:

```bash
brew services start tend   # or run `tendd` yourself / via tend init
```

The formula tracks the latest release automatically (the `tap-bump`
workflow rewrites it whenever a release is published).


### Tend for Mac

The Mac app is a separate cask, because the tap already has a formula named
`tend` and one name cannot mean both:

```bash
brew install --cask open330/tap/tend-app
```

That installs the notarized `Tend.app` into `/Applications`. The app carries
its own `tend` and `tendd` in `Contents/Helpers`, so it works without the
formula; install both if you also want `tend` on your `PATH`.

Tend.app has no built-in updater, which makes Homebrew its update path:
`brew update` followed by `brew upgrade --cask open330/tap/tend-app` moves
an installed app to the newest release. Downloading the
DMG from the release page works too, but then updating is on you.

`brew uninstall --cask tend-app` removes the app. `--zap` additionally
removes its preferences and caches; it deliberately leaves
`~/Library/Application Support/tend` alone, since `config.toml` there belongs
to the daemon and the CLI as much as to the app.

## Pre-Built Binaries

Download an archive from the
[Releases page](https://github.com/Open330/tend/releases), put `tend` and
`tendd` on `PATH`, then run:

```bash
tend init
```

Current release targets:

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`

## `tend init`

`tend init` is the recommended wiring path. It detects available tools,
offers components, previews file edits, and can uninstall its own managed
blocks later.

Common commands:

| Goal | Command |
| --- | --- |
| Interactive wizard | `tend init` |
| Headless install | `tend init --preset standard --yes` |
| Preview only | `tend init --dry-run` |
| Reverse install | `tend init --uninstall` |
| One component | `tend init --component tmux-popup --yes` |
| Preset minus one piece | `tend init --preset standard --no tendd-systemd --yes` |

Managed tmux edits are wrapped in marker blocks like:

```text
# >>> tend managed (tmux-popup) >>>
...
# <<< tend managed (tmux-popup) <<<
```

JSON/TOML agent config edits use command-prefix matching so uninstall can
remove tend hook entries without deleting user hooks. Files are backed up
to `<file>.tend-backup-<unix_ts>` before writes.

The `standard` and `full` presets also install global entry points, a shared
symlinked collaboration skill, and MCP registrations for detected Codex and
Claude Code installations. Existing installs can add them with
`tend init --component agent-instructions,agent-skills,agent-mcp`.
See [Global agent integration](AGENT_INTEGRATION.md) for paths, ownership,
updates, and component-specific uninstall. Restart agents after installing.

## Daemon Lifecycle

The CLI resolves `--socket`, `TEND_SOCKET`, config, and the XDG default in
that order, so normal lifecycle commands do not need a socket argument:

```bash
tend daemon start
tend daemon status
tend daemon restart
tend daemon stop
```

## Manual Daemon Start

Foreground:

```bash
tendd
```

Background shell:

```bash
tendd &
```

systemd user service:

```bash
mkdir -p ~/.local/share/systemd/user
cp examples/tendd.service ~/.local/share/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now tendd.service
```

## Manual tmux Wiring

Minimal status line:

```tmux
set -g status-interval 2
set -g status-right "#(tend status-line --pane #{pane_id}) | %H:%M"
```

Optional popups:

```tmux
bind-key s display-popup -B -E -w 100% -h 100% -x 0 -y 0 "tend watch"
bind-key S display-popup -B -E -w 100% -h 100% -x 0 -y 0 "tend watch --fleet"
bind-key D display-popup -E -w 95% -h 90% "tend dashboard"
```

Use `prefix+s` for the local watch and `prefix+S` for the physical-host Fleet.
`prefix+D` opens the dashboard.

Reload:

```bash
tmux source-file ~/.tmux.conf
```

## Manual Agent Wiring

`tend init` is safer than hand-editing. When wiring manually, append tend
hook commands without overwriting existing user hooks.

| Agent | Config |
| --- | --- |
| Claude Code | Merge `examples/claude-settings.json` into `~/.claude/settings.json`. |
| OpenAI Codex | Prefer `tend init` for compatible hooks. The daemon also reads local session-rollout logs every two seconds; hook support alone is not required for lifecycle recovery. |
| Google Antigravity CLI | Add the `tend` block from `crates/tend/src/adapters/antigravity.rs` to `~/.gemini/config/hooks.json`. Note this is agy's own `hooks.json`, **not** `~/.gemini/settings.json`. |
| opencode | Use `tend init --component opencode-hooks`; the plugin body lives in `crates/tend-cli/src/init/files/opencode.rs`. See [OPENCODE.md](OPENCODE.md). |

## Verify

```bash
tend status
tend status-line --pane "$TMUX_PANE"
tend watch
```

## Upgrading

`tend upgrade` updates in place for every channel (source checkout, Homebrew,
release binary) and re-execs tendd. Around the swap it also:

1. **Snapshots the workspace first.** The path is printed as
   `pre-upgrade snapshot: …/snapshots/<id>`.
2. **Reports what still runs the old build.** Hooks pick up the new `tend` on
   their next event. The `tend mcp` server each agent launched keeps running
   the old code until its owner restarts. `tend upgrade` (and later
   `tend doctor`) lists those servers by owner and gives the exact command for
   each:
   - **Codex shared app-server** (Codex 0.160+ runs one for all threads): when
     its sessions are idle, run `CODEX_HOME=<home> codex app-server daemon restart`.
   - **Agents in panes:** restart each agent; it resumes its conversation. To
     restart every pane at once, run `tend reload` from outside the
     multiplexer. It snapshots, restarts the server, and relaunches each agent
     with its resume command.

   Do not kill a `tend mcp` to refresh it. The agent does not respawn it, and
   its tend tools fail until the agent restarts.
3. **Lets you restore the workspace.** If a restart loses panes, rebuild the
   pre-upgrade workspace:

   ```bash
   tend restore ~/.local/share/tend/snapshots/<id>         # print the plan
   tend restore ~/.local/share/tend/snapshots/<id> --run   # recreate what is missing
   ```

   Existing sessions, windows and panes are kept. Only missing ones are
   created, so it is safe to rerun.

## Restore After Reboot

tendd snapshots the workspace on an interval. To have the newest snapshot
rebuilt when you log in after a reboot:

```bash
tend init --component restore-at-login
```

It registers `tend restore --boot` with this host's per-user scheduler:

| Host | Registration | Output |
| --- | --- | --- |
| Linux | systemd user unit `tend-restore.service` (`default.target`) | `journalctl --user -u tend-restore` |
| macOS | `LaunchAgent` `dev.open330.tend-restore`, loaded at the next login | `/tmp/tend-restore.log` |
| Windows | Task Scheduler task `tend-restore`, at logon | `%TEMP%\tend-restore.log` |

`restore --boot` rebuilds only into a server that has no sessions — starting
it if it is down — and does nothing when the server already has sessions, so
a login that kept its server is untouched. Agents come back on their own
conversations, as with `tend restore --run`. The hook records the `tend`
binary and `PATH` of the shell that installed it; rerun the component after
moving either. On Linux, a user manager runs only while you are logged in
unless lingering is enabled (`loginctl enable-linger`). On Windows the task
runs at the installing terminal's level, so install it from the kind of
terminal (elevated or not) you attach from. `tend init --uninstall
--component restore-at-login` removes it.

## Rollback

```bash
tend init --uninstall
pkill tendd
tmux source-file ~/.tmux.conf
```

If you need a manual rollback, restore the `.tend-backup-<unix_ts>` files.

`tend init` also installs five disabled automation rules for capacity retries,
usage-limit recovery, input-wait notices, persistent-error notices and selected Yes/Proceed confirmations. Enable
individual rules with `tend automation enable <name>`; existing same-name rules
are preserved. Binary-only upgrades add missing rules when tendd next starts.
See [Automation](AUTOMATION.md).

### Peek in linked view sessions

`prefix + q` launches `tend peek --popup` with the pressing client's session and
window IDs. A linked window's panes may retain their original session environment;
peek must not use that environment to decide which screen to draw on or where to
jump. The popup launcher pins the client, and pane jumps retain the same view.
After upgrading, `tend init --component tmux-peek --yes` updates an older binding.

The isolated regression uses only shell panes on a private rmux socket:

```sh
python3 scripts/rmux-peek-view-check.py
```
