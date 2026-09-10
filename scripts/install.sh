#!/usr/bin/env bash
# muxa one-shot installer.
#
#   curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/install.sh | sh
#
# Or with arguments forwarded to `muxa init`:
#
#   curl -fsSL https://… | sh -s -- --preset standard --yes
#   curl -fsSL https://… | sh -s -- --dry-run
#
# Tier 1 of muxa's three-tier install model:
#   - Tier 1 (this script): builds + installs binaries, then hands off
#     to `muxa init` which does the actual wiring.
#   - Tier 2 (interactive):  cargo install + `muxa init`.
#   - Tier 3 (automation):   the underlying `muxa init` flags
#     (--preset, --yes, --component, --dry-run).
#
# All installation logic — backups, marker blocks, hook merging — lives
# in `muxa init`, not here. Keeps this script short, auditable, and
# safe to pipe into `sh`.

# POSIX-clean: `pipefail` is a bash/zsh-ism that dash (`/bin/sh` on Debian/
# Ubuntu) rejects with "set: Illegal option -o pipefail", which would abort
# the documented `curl … | sh` one-liner before it ran. `set -eu` is enough
# here — there is no pipeline whose partial failure we need to catch.
set -eu

REPO_URL="${MUXA_REPO_URL:-https://github.com/Open330/muxa.git}"
REPO_REF="${MUXA_REPO_REF:-main}"

need() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "muxa-install: missing dependency: $1" >&2
    case "$1" in
      cargo) echo "  install rustup: https://rustup.rs" >&2 ;;
      git)   echo "  install git via your package manager" >&2 ;;
    esac
    exit 1
  }
}

# Git Bash, MSYS2 and Cygwin all report a Windows-family `uname -s` while
# providing a POSIX shell, so this script runs happily and then spends several
# minutes compiling before dying inside `nix`, which has no Windows target.
# Answer the question up front instead: muxa has no Windows host, and WSL is
# not a workaround there but the actual supported environment.
case "$(uname -s 2>/dev/null)" in
  MINGW* | MSYS* | CYGWIN* | Windows_NT)
    cat >&2 <<'WINDOWS_NOTICE'
muxa-install: this is a Windows shell, and muxa has no Windows host.

muxa observes agents running in tmux panes, and tmux does not run natively on
Windows. Install inside WSL2, where muxa is the complete product:

  wsl --install                 # once, if you have no distribution yet
  wsl                           # then, inside it:
  curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/install.sh | sh

Windows Terminal attaches to that distribution like any other shell, so
`muxa watch` runs in a Windows Terminal tab with nothing bridging in between.

Clone into the WSL filesystem (~/) rather than /mnt/c — the 9p mount makes
cargo builds several times slower.

See docs/WINDOWS.md for why native support is not planned.
WINDOWS_NOTICE
    exit 1
    ;;
esac

need cargo
need git

echo "muxa-install: cloning $REPO_URL ($REPO_REF)..."
TMPDIR="$(mktemp -d -t muxa-install.XXXXXX)"
trap 'rm -rf "$TMPDIR"' EXIT

git clone --depth 1 --branch "$REPO_REF" "$REPO_URL" "$TMPDIR" >/dev/null 2>&1 || {
  # Fallback for forks where the default branch isn't named REPO_REF.
  git clone --depth 1 "$REPO_URL" "$TMPDIR"
}

echo "muxa-install: building muxad..."
cargo install --quiet --path "$TMPDIR/crates/muxad" --locked

echo "muxa-install: building muxa CLI..."
cargo install --quiet --path "$TMPDIR/crates/muxa-cli" --locked

# Make sure muxa is on PATH for the exec below — fresh `cargo install`
# adds it but a brand-new shell may not have rebuilt PATH yet.
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"

if ! command -v muxa >/dev/null 2>&1; then
  echo "muxa-install: muxa was built but is not on PATH." >&2
  echo "  Add ${CARGO_HOME:-$HOME/.cargo}/bin to your PATH, then run \`muxa init\`." >&2
  exit 0
fi

# Hand off to the wizard. All the file-edit, hook-merge, systemd, and
# verify logic lives there. Forward any extra args the user piped in.
exec muxa init "$@"
