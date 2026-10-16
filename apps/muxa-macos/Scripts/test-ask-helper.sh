#!/bin/bash
set -euo pipefail
APP_DIR=$(cd "$(dirname "$0")/.." && pwd)
WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/muxa-afm-tests.XXXXXX")
trap 'rm -rf "$WORK_DIR"' EXIT
xcrun --sdk macosx swiftc -parse-as-library -swift-version 6 \
    -D MUXA_AFM_TESTING \
    "$APP_DIR/AskHelperSources/MuxaAFM.swift" \
    "$APP_DIR/AskHelperTests/WorkspaceTests.swift" \
    -o "$WORK_DIR/tests"
"$WORK_DIR/tests"
