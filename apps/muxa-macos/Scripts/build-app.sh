#!/bin/bash

set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
APP_DIR=$(cd "$SCRIPT_DIR/.." && pwd)
DERIVED_DATA="$APP_DIR/.build/DerivedData"
CONFIGURATION=${CONFIGURATION:-Debug}

"$SCRIPT_DIR/build-libghostty.sh"

(
    cd "$APP_DIR"
    xcodegen generate
)

xcodebuild \
    -project "$APP_DIR/Muxa.xcodeproj" \
    -scheme Muxa \
    -configuration "$CONFIGURATION" \
    -derivedDataPath "$DERIVED_DATA" \
    -destination 'platform=macOS' \
    CODE_SIGNING_ALLOWED=NO \
    build

APP_PATH="$DERIVED_DATA/Build/Products/$CONFIGURATION/Muxa.app"
test -d "$APP_PATH"
echo "Muxa for Mac: $APP_PATH"

if [ "${1:-}" = "--open" ]; then
    open "$APP_PATH"
fi
