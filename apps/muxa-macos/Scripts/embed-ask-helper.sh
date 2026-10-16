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
HELPER_DEVELOPER_DIR=${DEVELOPER_DIR:-}
WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/muxa-afm.XXXXXX")
trap 'rm -rf "$WORK_DIR"' EXIT

# Foundation Models arrived with the macOS 26 SDK. The source compiles
# without it (`#if canImport`), which is exactly the trap: an app built by an
# older Xcode would ship a provider that can only ever answer "built without
# the Foundation Models SDK". The app may need that older Xcode for its own
# reasons, so the helper looks for a newer one of its own — CI images keep
# several side by side — and a Release build with none is an error, not a
# helper that quietly does nothing.
has_foundation_models() {
    [ -d "$1/System/Library/Frameworks/FoundationModels.framework" ]
}

if ! has_foundation_models "$SDK_PATH"; then
    while IFS= read -r xcode; do
        candidate="$xcode/Contents/Developer"
        candidate_sdk=$(DEVELOPER_DIR="$candidate" xcrun --sdk macosx --show-sdk-path 2>/dev/null || true)
        if [ -n "$candidate_sdk" ] && has_foundation_models "$candidate_sdk"; then
            echo "muxa-afm: building with $xcode, whose SDK has Foundation Models"
            HELPER_DEVELOPER_DIR=$candidate
            SDK_PATH=$candidate_sdk
            break
        fi
    done < <(find "${MUXA_AFM_XCODE_SEARCH:-/Applications}" -maxdepth 1 -name 'Xcode*.app' 2>/dev/null | sort -rV)
fi

if ! has_foundation_models "$SDK_PATH"; then
    message="no installed Xcode has the Foundation Models SDK (macOS 26 SDK, Xcode 26 or later)"
    if [ "${CONFIGURATION:-Debug}" = "Release" ]; then
        echo "error: muxa-afm: $message; the apple Ask provider would ship unable to answer" >&2
        exit 1
    fi
    echo "warning: muxa-afm: $message; this build's apple Ask provider will report itself unavailable" >&2
fi

# The helper's compiler may come from a different Xcode than the one running
# this build phase, whose SDKROOT and TOOLCHAINS would otherwise follow it in.
helper_swiftc() {
    if [ -n "$HELPER_DEVELOPER_DIR" ]; then
        env -u SDKROOT -u TOOLCHAINS DEVELOPER_DIR="$HELPER_DEVELOPER_DIR" xcrun --sdk macosx swiftc "$@"
    else
        env -u SDKROOT -u TOOLCHAINS xcrun --sdk macosx swiftc "$@"
    fi
}

IFS=' ' read -r -a architectures <<<"${ARCHS:-$(uname -m)}"
slices=()
for architecture in "${architectures[@]}"; do
    case "$architecture" in
        arm64 | x86_64) ;;
        *) echo "unsupported muxa-afm architecture: $architecture" >&2; exit 1 ;;
    esac
    slice="$WORK_DIR/muxa-afm-$architecture"
    helper_swiftc \
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
