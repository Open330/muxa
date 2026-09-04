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

# The version the release carries lives in Cargo.toml, not in project.yml —
# muxa the daemon and Muxa the app ship together and must not disagree about
# which release a user is running. Callers that know the version pass it in.
VERSION_SETTING=()
if [ -n "${MUXA_MARKETING_VERSION:-}" ]; then
    VERSION_SETTING=(MARKETING_VERSION="$MUXA_MARKETING_VERSION")
fi

xcodebuild \
    -project "$APP_DIR/Muxa.xcodeproj" \
    -scheme Muxa \
    -configuration "$CONFIGURATION" \
    -derivedDataPath "$DERIVED_DATA" \
    -destination 'platform=macOS' \
    CODE_SIGNING_ALLOWED=NO \
    ${VERSION_SETTING[@]+"${VERSION_SETTING[@]}"} \
    build

APP_PATH="$DERIVED_DATA/Build/Products/$CONFIGURATION/Muxa.app"
test -d "$APP_PATH"
echo "Muxa for Mac: $APP_PATH"

if [ "${1:-}" = "--open" ]; then
    open "$APP_PATH"
fi
