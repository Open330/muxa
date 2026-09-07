# Install

This page keeps the detailed install and wiring notes out of the README.

## Requirements

- Rust 1.89+
- tmux 3.x
- Unix-like OS
- One supported agent CLI: Claude Code, OpenAI Codex, Google Gemini CLI, or the
  Google Antigravity CLI (`agy`)

## Try Before Installing

```bash
curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/onboard.sh | sh
```

This path installs nothing. The script fetches the release archive for your
platform into a temporary directory, verifies its published SHA-256, runs the
real `muxa onboard`, and deletes it on exit — no persistent daemon, config, or
PATH entry. The live tour uses tmux plus an isolated daemon and mailbox while it
runs, then removes the entire sandbox. Downloading through the launcher needs
network access, a supported release platform, a checksum tool, and `tar`; the
interactive tour also needs tmux. It fails clearly when it cannot download and
verify the release. Forward tour flags after `sh -s --`:

```bash
curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/onboard.sh | sh -s -- --lang ko
curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/onboard.sh | sh -s -- --print
```

After installing Muxa, run `muxa onboard` directly for the same sixteen-step
live tour, or `muxa onboard --print` when tmux is unavailable.

## One-Shot Install

```bash
curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/install.sh | sh
```

The script builds and installs `muxa` and `muxad`, then runs `muxa init`
to wire tmux, agent hooks, optional systemd, and the optional dashboard.
Forward flags to the wizard with `sh -s --`:

```bash
curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/install.sh | sh -s -- --preset standard --yes
curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/install.sh | sh -s -- --dry-run
```

## From Source

```bash
git clone https://github.com/Open330/muxa.git
cd muxa
cargo install --path crates/muxad --locked
cargo install --path crates/muxa-cli --locked
muxa init
```

`cargo install` writes to `~/.cargo/bin`; make sure that directory is on
your `PATH`.

## Homebrew

```bash
brew install open330/tap/muxa
muxa init
```

Installs pre-built `muxa` and `muxad` binaries from
[Open330/homebrew-tap](https://github.com/Open330/homebrew-tap)
(macOS arm64/x86_64 and Linux arm64/x86_64). The formula ships a
`brew services`-compatible service for `muxad`:

```bash
brew services start muxa   # or run `muxad` yourself / via muxa init
```

The formula tracks the latest release automatically (the `tap-bump`
workflow rewrites it whenever a release is published).


### Muxa for Mac

The Mac app is a separate cask, because the tap already has a formula named
`muxa` and one name cannot mean both:

```bash
brew install --cask open330/tap/muxa-app
```

That installs the notarized `Muxa.app` into `/Applications`. The app carries
its own `muxa` and `muxad` in `Contents/Helpers`, so it works without the
formula; install both if you also want `muxa` on your `PATH`.

Muxa.app has no built-in updater, which makes Homebrew its update path:
`brew upgrade` moves an installed app to the newest release. Downloading the
DMG from the release page works too, but then updating is on you.

`brew uninstall --cask muxa-app` removes the app. `--zap` additionally
removes its preferences and caches; it deliberately leaves
`~/Library/Application Support/muxa` alone, since `config.toml` there belongs
to the daemon and the CLI as much as to the app.

## Pre-Built Binaries

Download an archive from the
[Releases page](https://github.com/Open330/muxa/releases), put `muxa` and
`muxad` on `PATH`, then run:

```bash
muxa init
```

Current release targets:

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`

## `muxa init`

`muxa init` is the recommended wiring path. It detects available tools,
offers components, previews file edits, and can uninstall its own managed
blocks later.

Common commands:

| Goal | Command |
| --- | --- |
| Interactive wizard | `muxa init` |
| Headless install | `muxa init --preset standard --yes` |
| Preview only | `muxa init --dry-run` |
| Reverse install | `muxa init --uninstall` |
| One component | `muxa init --component tmux-popup --yes` |
| Preset minus one piece | `muxa init --preset standard --no muxad-systemd --yes` |

Managed tmux edits are wrapped in marker blocks like:

```text
# >>> muxa managed (tmux-popup) >>>
...
# <<< muxa managed (tmux-popup) <<<
```

JSON/TOML agent config edits use command-prefix matching so uninstall can
remove muxa hook entries without deleting user hooks. Files are backed up
to `<file>.muxa-backup-<unix_ts>` before writes.

The `standard` and `full` presets also install global entry points, a shared
symlinked collaboration skill, and MCP registrations for detected Codex and
Claude Code installations. Existing installs can add them with
`muxa init --component agent-instructions,agent-skills,agent-mcp`.
See [Global agent integration](AGENT_INTEGRATION.md) for paths, ownership,
updates, and component-specific uninstall. Restart agents after installing.

## Daemon Lifecycle

The CLI resolves `--socket`, `MUXA_SOCKET`, config, and the XDG default in
that order, so normal lifecycle commands do not need a socket argument:

```bash
muxa daemon start
muxa daemon status
muxa daemon restart
muxa daemon stop
```

## Manual Daemon Start

Foreground:

```bash
muxad
```

Background shell:

```bash
muxad &
```

systemd user service:

```bash
mkdir -p ~/.local/share/systemd/user
cp examples/muxad.service ~/.local/share/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now muxad.service
```

## Manual tmux Wiring

Minimal status line:

```tmux
set -g status-interval 2
set -g status-right "#(muxa status-line --pane #{pane_id}) | %H:%M"
```

Optional popups:

```tmux
bind-key s display-popup -B -E -w 100% -h 100% -x 0 -y 0 "muxa watch"
bind-key S display-popup -B -E -w 100% -h 100% -x 0 -y 0 "muxa watch --fleet"
bind-key D display-popup -E -w 95% -h 90% "muxa dashboard"
```

Use `prefix+s` for the local watch and `prefix+S` for the physical-host Fleet.
`prefix+D` opens the dashboard.

Reload:

```bash
tmux source-file ~/.tmux.conf
```

## Manual Agent Wiring

`muxa init` is safer than hand-editing. When wiring manually, append muxa
hook commands without overwriting existing user hooks.

| Agent | Config |
| --- | --- |
| Claude Code | Merge `examples/claude-settings.json` into `~/.claude/settings.json`. |
| OpenAI Codex | Add the `[[hooks.*]]` blocks documented in `crates/muxa/src/adapters/codex.rs`. |
| Google Gemini CLI | Merge the hooks from `crates/muxa/src/adapters/gemini.rs` into `~/.gemini/settings.json`. |
| Google Antigravity CLI | Add the `muxa` block from `crates/muxa/src/adapters/antigravity.rs` to `~/.gemini/config/hooks.json`. Note this is agy's own `hooks.json`, **not** the Gemini CLI's `settings.json`. |

## Verify

```bash
muxa status
muxa status-line --pane "$TMUX_PANE"
muxa watch
```

## Rollback

```bash
muxa init --uninstall
pkill muxad
tmux source-file ~/.tmux.conf
```

If you need a manual rollback, restore the `.muxa-backup-<unix_ts>` files.
