#!/bin/sh
# Run the real Muxa onboarding without installing Muxa.
#
#   curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/onboard.sh | sh
#
# Forward flags with `sh -s --`:
#
#   curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/onboard.sh | sh -s -- --lang ko
#   curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/onboard.sh | sh -s -- --print
#
# This script fetches the release binary for the host into a temporary
# directory, verifies its published SHA-256, runs `muxa onboard`, and deletes
# the temporary copy on exit. It installs no daemon, config, or PATH entry; the
# live tour's isolated tmux server and sandbox are also removed when it exits.

set -eu

language=auto
print_only=0
no_quiz=0

usage() {
  printf '%s\n' \
    'Usage: onboard.sh [--lang auto|en|ko] [--print] [--no-quiz]' \
    '' \
    'Downloads a checksum-verified release binary to a temporary directory,' \
    'runs the real muxa onboard, then deletes the binary. Nothing is installed.' \
    '' \
    'Options:' \
    '  --lang auto|en|ko  Display language (default: detect from locale)' \
    '  --print            Print the complete guide instead of opening the live tour' \
    '  --no-quiz          Offer the live tour skip key from the first step' \
    '  -h, --help         Show this help'
}

fail() {
  printf 'muxa-onboard: %s\n' "$*" >&2
  exit 1
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --lang)
      [ "$#" -ge 2 ] || fail '--lang needs auto, en, or ko'
      language=$2
      shift 2
      ;;
    --lang=*) language=${1#--lang=}; shift ;;
    --print) print_only=1; shift ;;
    --no-quiz) no_quiz=1; shift ;;
    --tmux) shift ;; # Compatibility: tmux is always part of this tour.
    -h | --help) usage; exit 0 ;;
    --) shift; break ;;
    *) fail "unknown option: $1" ;;
  esac
done

[ "$#" -eq 0 ] || fail "unexpected argument: $1"

case "$language" in
  auto | en | ko) ;;
  *) fail "unsupported language: $language (expected auto, en, or ko)" ;;
esac

release_repo=${MUXA_ONBOARD_REPO:-Open330/muxa}
download_dir=

detect_release_target() {
  case "$(uname -s 2>/dev/null || printf unknown)" in
    Darwin) target_os=apple-darwin ;;
    Linux) target_os=unknown-linux-gnu ;;
    *) return 1 ;;
  esac
  case "$(uname -m 2>/dev/null || printf unknown)" in
    x86_64 | amd64) target_arch=x86_64 ;;
    arm64 | aarch64) target_arch=aarch64 ;;
    *) return 1 ;;
  esac
  printf '%s-%s' "$target_arch" "$target_os"
}

fetch_url() { # url [dest]; prints to stdout when no dest is given
  if command -v curl >/dev/null 2>&1; then
    if [ "$#" -ge 2 ]; then curl -fsSL "$1" -o "$2"; else curl -fsSL "$1"; fi
  elif command -v wget >/dev/null 2>&1; then
    if [ "$#" -ge 2 ]; then wget -qO "$2" "$1"; else wget -qO- "$1"; fi
  else
    return 1
  fi
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  else
    return 1
  fi
}

discard_download() {
  [ -n "$download_dir" ] || return 0
  rm -rf "$download_dir"
  download_dir=
  trap - EXIT INT TERM HUP
}

run_release_onboarding() {
  command -v tar >/dev/null 2>&1 || return 1
  command -v mktemp >/dev/null 2>&1 || return 1
  target=$(detect_release_target) || return 1

  version=${MUXA_ONBOARD_VERSION:-}
  if [ -z "$version" ]; then
    version=$(fetch_url "https://api.github.com/repos/$release_repo/releases/latest" 2>/dev/null |
      sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1) || return 1
  fi
  [ -n "$version" ] || return 1

  download_dir=$(mktemp -d 2>/dev/null) || { download_dir=; return 1; }
  trap 'rm -rf "$download_dir"' EXIT INT TERM HUP

  release=muxa-$version-$target
  base=https://github.com/$release_repo/releases/download/$version

  printf 'muxa-onboard: fetching muxa %s (%s) — runs from a temp dir, nothing is installed\n' \
    "$version" "$target" >&2
  fetch_url "$base/$release.tar.gz" "$download_dir/$release.tar.gz" || return 1
  fetch_url "$base/$release.sha256" "$download_dir/$release.sha256" || return 1

  published=$(sed -n 's/^\([0-9a-fA-F]\{64\}\).*/\1/p' "$download_dir/$release.sha256" | head -1)
  actual=$(sha256_of "$download_dir/$release.tar.gz") || return 1
  if [ -z "$published" ] || [ "$published" != "$actual" ]; then
    printf 'muxa-onboard: checksum mismatch for %s; refusing to run it\n' "$release.tar.gz" >&2
    return 1
  fi

  tar -xzf "$download_dir/$release.tar.gz" -C "$download_dir" || return 1
  muxa_bin=$download_dir/$release/muxa
  [ -x "$muxa_bin" ] || muxa_bin=$(find "$download_dir" -type f -name muxa 2>/dev/null | head -1)
  [ -n "$muxa_bin" ] && [ -f "$muxa_bin" ] || return 1
  chmod +x "$muxa_bin" 2>/dev/null || :

  # No `--tour`: this runs whichever release is published, and every release
  # up to and including v0.8.36 predates the flag and exits on it — pinning
  # `--tour live` breaks the no-install path for every currently published
  # build. Once the live tour ships it is the only tour and the default, so
  # the flag would say nothing anyway.
  set -- onboard --lang "$language"
  [ "$print_only" -eq 1 ] && set -- "$@" --print
  [ "$no_quiz" -eq 1 ] && set -- "$@" --no-quiz

  # This script is usually piped into `sh`, so stdin is the pipe, not the
  # terminal from which the live tour must read commands.
  status=0
  if ( : </dev/tty ) 2>/dev/null; then
    "$muxa_bin" "$@" </dev/tty || status=$?
  else
    "$muxa_bin" "$@" || status=$?
  fi
  discard_download
  exit "$status"
}

if ! run_release_onboarding; then
  discard_download
  fail 'could not download and verify a release for this host; see https://github.com/Open330/muxa/releases/latest'
fi
