#!/bin/sh
# Try Muxa's interactive onboarding without installing Muxa.
#
#   curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/onboard.sh | sh
#
# Forward onboarding flags with `sh -s --`:
#
#   curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/onboard.sh | sh -s -- --lang ko
#   curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/onboard.sh | sh -s -- --print
#
# The latest pre-built CLI is downloaded into a temporary directory, its
# published SHA-256 checksum is verified, and the directory is removed when
# onboarding exits. Nothing is copied to PATH and no config is changed.

set -eu

REPOSITORY="${MUXA_GITHUB_REPOSITORY:-Open330/muxa}"
GITHUB_URL="${MUXA_GITHUB_URL:-https://github.com}"
VERSION="${MUXA_ONBOARD_VERSION:-latest}"
WORK_DIR=""

fail() {
  echo "muxa-onboard: $*" >&2
  exit 1
}

need() {
  command -v "$1" >/dev/null 2>&1 || fail "missing required command: $1"
}

cleanup() {
  if [ -n "$WORK_DIR" ] && [ -d "$WORK_DIR" ]; then
    rm -rf "$WORK_DIR"
  fi
}

trap cleanup 0
trap 'exit 129' 1
trap 'exit 130' 2
trap 'exit 143' 15

need curl
need tar
need uname
need mktemp

case "$(uname -s)" in
  Linux) target_os="unknown-linux-gnu" ;;
  Darwin) target_os="apple-darwin" ;;
  *) fail "unsupported OS: $(uname -s) (supported: Linux and macOS)" ;;
esac

case "$(uname -m)" in
  x86_64 | amd64) target_arch="x86_64" ;;
  arm64 | aarch64) target_arch="aarch64" ;;
  *) fail "unsupported architecture: $(uname -m) (supported: x86_64 and arm64)" ;;
esac

if [ "$VERSION" = "latest" ]; then
  echo "muxa-onboard: finding the latest release..." >&2
  release_url="$(
    curl -fsSL -o /dev/null -w '%{url_effective}' \
      "$GITHUB_URL/$REPOSITORY/releases/latest"
  )" || fail "could not resolve the latest release"
  VERSION="${release_url##*/}"
  [ -n "$VERSION" ] && [ "$VERSION" != "latest" ] \
    || fail "could not determine the latest release tag"
else
  case "$VERSION" in
    v*) ;;
    *) VERSION="v$VERSION" ;;
  esac
fi

target="$target_arch-$target_os"
archive_base="muxa-$VERSION-$target"
archive="$archive_base.tar.gz"
checksum="$archive_base.sha256"
download_url="$GITHUB_URL/$REPOSITORY/releases/download/$VERSION"

WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/muxa-onboard.XXXXXX")" \
  || fail "could not create a temporary directory"

echo "muxa-onboard: downloading Muxa $VERSION for $target..." >&2
curl -fsSL --retry 3 -o "$WORK_DIR/$archive" "$download_url/$archive" \
  || fail "could not download $archive"
curl -fsSL --retry 3 -o "$WORK_DIR/$checksum" "$download_url/$checksum" \
  || fail "could not download $checksum"

if command -v sha256sum >/dev/null 2>&1; then
  (cd "$WORK_DIR" && sha256sum -c "$checksum" >/dev/null) \
    || fail "SHA-256 checksum verification failed"
elif command -v shasum >/dev/null 2>&1; then
  (cd "$WORK_DIR" && shasum -a 256 -c "$checksum" >/dev/null) \
    || fail "SHA-256 checksum verification failed"
else
  fail "SHA-256 verification needs sha256sum (Linux) or shasum (macOS)"
fi

tar -xzf "$WORK_DIR/$archive" -C "$WORK_DIR" \
  || fail "could not extract $archive"
muxa_bin="$WORK_DIR/$archive_base/muxa"
[ -x "$muxa_bin" ] || fail "release archive does not contain an executable muxa CLI"

if ! "$muxa_bin" onboard --help >/dev/null 2>&1; then
  fail "Muxa $VERSION does not include onboard; a newer release is required"
fi

echo "muxa-onboard: starting the temporary tour; nothing will be installed." >&2

# With `curl ... | sh`, the script itself owns stdin. Reconnect the tour to the
# terminal so crossterm can enter interactive mode after curl reaches EOF.
if [ -r /dev/tty ] && [ -t 1 ]; then
  "$muxa_bin" onboard "$@" </dev/tty
else
  "$muxa_bin" onboard "$@"
fi
