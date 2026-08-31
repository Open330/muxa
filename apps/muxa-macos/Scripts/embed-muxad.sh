#!/bin/bash

set -euo pipefail

APP_DIR=$(cd "$(dirname "$0")/.." && pwd)
REPO_DIR=$(cd "$APP_DIR/../.." && pwd)
HELPERS_DIR="$TARGET_BUILD_DIR/$CONTENTS_FOLDER_PATH/Helpers"
DAEMON_PATH="$HELPERS_DIR/muxad"
CLI_PATH="$HELPERS_DIR/muxa"

CARGO_BIN=$(command -v cargo || true)
if [ -z "$CARGO_BIN" ] && [ -x "${HOME:-}/.cargo/bin/cargo" ]; then
    CARGO_BIN="${HOME}/.cargo/bin/cargo"
fi
if [ -z "$CARGO_BIN" ]; then
    echo "cargo was not found; install Rust 1.88+ before building Muxa.app" >&2
    exit 1
fi
RUSTUP_BIN=$(command -v rustup || true)
if [ -z "$RUSTUP_BIN" ] && [ -x "${HOME:-}/.cargo/bin/rustup" ]; then
    RUSTUP_BIN="${HOME}/.cargo/bin/rustup"
fi

IFS=' ' read -r -a architectures <<<"${ARCHS:-$(uname -m)}"
daemon_binaries=()
cli_binaries=()
for architecture in "${architectures[@]}"; do
    case "$architecture" in
        arm64) rust_target=aarch64-apple-darwin ;;
        x86_64) rust_target=x86_64-apple-darwin ;;
        *) echo "unsupported muxad architecture: $architecture" >&2; exit 1 ;;
    esac
    if [ -n "$RUSTUP_BIN" ] &&
        ! "$RUSTUP_BIN" target list --installed | grep -qx "$rust_target"; then
        "$RUSTUP_BIN" target add "$rust_target"
    fi
    "$CARGO_BIN" build \
        --manifest-path "$REPO_DIR/Cargo.toml" \
        --release \
        --target "$rust_target" \
        -p muxad \
        -p muxa-cli
    daemon_binaries+=("$REPO_DIR/target/$rust_target/release/muxad")
    cli_binaries+=("$REPO_DIR/target/$rust_target/release/muxa")
done

mkdir -p "$HELPERS_DIR"
combine_binary() {
    output=$1
    shift
    if [ "$#" -eq 1 ]; then
        cp "$1" "$output"
    else
        lipo -create "$@" -output "$output"
    fi
    chmod 755 "$output"
}

combine_binary "$DAEMON_PATH" "${daemon_binaries[@]}"
combine_binary "$CLI_PATH" "${cli_binaries[@]}"

if [ "${CODE_SIGNING_ALLOWED:-NO}" = "YES" ] &&
    [ -n "${EXPANDED_CODE_SIGN_IDENTITY:-}" ]; then
    /usr/bin/codesign --force --sign "$EXPANDED_CODE_SIGN_IDENTITY" "$DAEMON_PATH"
    /usr/bin/codesign --force --sign "$EXPANDED_CODE_SIGN_IDENTITY" "$CLI_PATH"
fi
