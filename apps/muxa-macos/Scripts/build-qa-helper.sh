#!/bin/bash

set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
APP_DIR=$(cd "$SCRIPT_DIR/.." && pwd)
DERIVED_DATA="$APP_DIR/.build/DerivedData"
CONFIGURATION=${CONFIGURATION:-Debug}

(
    cd "$APP_DIR"
    xcodegen generate
)

xcodebuild \
    -project "$APP_DIR/Muxa.xcodeproj" \
    -scheme MuxaQAHelper \
    -configuration "$CONFIGURATION" \
    -derivedDataPath "$DERIVED_DATA" \
    -destination 'platform=macOS' \
    CODE_SIGNING_ALLOWED=NO \
    build

HELPER_PATH="$DERIVED_DATA/Build/Products/$CONFIGURATION/MuxaQAHelper.app"
test -d "$HELPER_PATH"
echo "Muxa QA Helper: $HELPER_PATH"

if [ "${1:-}" = "--open" ]; then
    open "$HELPER_PATH"
fi
