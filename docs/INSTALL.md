# Install

This page keeps the detailed install and wiring notes out of the README.

## Requirements

- Rust 1.88+
- tmux 3.x
- Unix-like OS
- One supported agent CLI: Claude Code, OpenAI Codex, Google Gemini CLI, or the
  Google Antigravity CLI (`agy`)

## Try Before Installing

```bash
curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/onboard.sh | sh
```

This path needs none of the requirements above. The fetched shell script is the
entire 20-step fullscreen tour: it uses ANSI terminal controls to preserve the
virtual shell, tmux status line, window/pane layouts, and Muxa watch UI without
downloading a Muxa binary, creating temporary files, changing config, starting
a daemon, or operating a real tmux session. It works in a normal Unix-like
terminal regardless of CPU architecture. Forward tour flags after `sh -s --`:

```bash
curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/onboard.sh | sh -s -- --lang ko
curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/onboard.sh | sh -s -- --print
```

After installing Muxa, `muxa onboard` provides the native Ratatui version of
the same practice scenario. Completing the shell tour returns to the original
terminal and prints the recommended Homebrew command, direct-download link,
and install-guide link.

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
