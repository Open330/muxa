#!/bin/bash

# Builds muxa-afm — the bridge muxad's `apple` Ask engine spawns — and puts
# it in Contents/Helpers beside muxad, which is where the daemon looks first.
#
# It is compiled here rather than as an Xcode target because it is one file
# with no dependencies, and because it must keep the app's macOS 13 floor:
# Foundation Models is weak-linked and every use sits behind `#available`,
# so the same binary runs on a Mac that has never heard of it and says so.
#
# Usage: embed-ask-helper.sh [output-path]
#   With no argument (the Xcode build phase) the output is the app bundle's
#   Contents/Helpers/muxa-afm. Pass a path to build the helper on its own.

set -euo pipefail

APP_DIR=$(cd "$(dirname "$0")/.." && pwd)
SOURCE="$APP_DIR/AskHelperSources/MuxaAFM.swift"

if [ -n "${1:-}" ]; then
    OUTPUT=$1
elif [ -n "${TARGET_BUILD_DIR:-}" ] && [ -n "${CONTENTS_FOLDER_PATH:-}" ]; then
    OUTPUT="$TARGET_BUILD_DIR/$CONTENTS_FOLDER_PATH/Helpers/muxa-afm"
else
    echo "usage: $0 <output-path> (or run as an Xcode build phase)" >&2
    exit 1
fi

DEPLOYMENT_TARGET=${MACOSX_DEPLOYMENT_TARGET:-13.0}
SDK_PATH=${SDKROOT:-$(xcrun --sdk macosx --show-sdk-path)}
WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/muxa-afm.XXXXXX")
trap 'rm -rf "$WORK_DIR"' EXIT

IFS=' ' read -r -a architectures <<<"${ARCHS:-$(uname -m)}"
slices=()
for architecture in "${architectures[@]}"; do
    case "$architecture" in
        arm64 | x86_64) ;;
        *) echo "unsupported muxa-afm architecture: $architecture" >&2; exit 1 ;;
    esac
    slice="$WORK_DIR/muxa-afm-$architecture"
    xcrun --sdk macosx swiftc \
        -parse-as-library \
        -swift-version 6 \
        -O \
        -target "$architecture-apple-macos$DEPLOYMENT_TARGET" \
        -sdk "$SDK_PATH" \
        "$SOURCE" \
        -o "$slice"
    slices+=("$slice")
done

mkdir -p "$(dirname "$OUTPUT")"
if [ "${#slices[@]}" -eq 1 ]; then
    cp "${slices[0]}" "$OUTPUT"
else
    lipo -create "${slices[@]}" -output "$OUTPUT"
fi
chmod 755 "$OUTPUT"

if [ "${CODE_SIGNING_ALLOWED:-NO}" = "YES" ] &&
    [ -n "${EXPANDED_CODE_SIGN_IDENTITY:-}" ]; then
    /usr/bin/codesign --force --sign "$EXPANDED_CODE_SIGN_IDENTITY" "$OUTPUT"
fi
