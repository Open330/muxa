#!/bin/bash

set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
APP_DIR=$(cd "$SCRIPT_DIR/.." && pwd)
REPO_DIR=$(cd "$APP_DIR/../.." && pwd)
DERIVED_DATA="$APP_DIR/.build/DerivedData"
CONFIGURATION=${CONFIGURATION:-Debug}

# xcodebuild needs a full Xcode. A Mac whose `xcode-select` still points at
# the Command Line Tools has one installed all the same, and repointing it
# takes sudo — so use the installed Xcode for this build rather than failing
# on a setting the build does not need changed.
if [ -z "${DEVELOPER_DIR:-}" ] && ! xcodebuild -version >/dev/null 2>&1; then
    for xcode in /Applications/Xcode.app /Applications/Xcode-beta.app; do
        if [ -x "$xcode/Contents/Developer/usr/bin/xcodebuild" ]; then
            export DEVELOPER_DIR="$xcode/Contents/Developer"
            echo "Using $xcode (xcode-select points at the Command Line Tools)"
            break
        fi
    done
fi

"$SCRIPT_DIR/build-libghostty.sh"

(
    cd "$APP_DIR"
    xcodegen generate
)

# The version the release carries lives in Cargo.toml, not in project.yml —
# muxa the daemon and Muxa the app ship together and must not disagree about
# which release a user is running. Callers that know the version pass it in.
APP_VERSION="${MUXA_MARKETING_VERSION:-}"
if [ -z "$APP_VERSION" ]; then
    APP_VERSION=$(sed -n '/^\[workspace\.package\]/,/^\[/p' "$REPO_DIR/Cargo.toml" \
        | sed -n 's/^version = "\(.*\)"/\1/p' | head -1)
fi
[ -n "$APP_VERSION" ] || { echo "could not determine the app version from Cargo.toml" >&2; exit 1; }

xcodebuild \
    -project "$APP_DIR/Muxa.xcodeproj" \
    -scheme Muxa \
    -configuration "$CONFIGURATION" \
    -derivedDataPath "$DERIVED_DATA" \
    -destination 'platform=macOS' \
    CODE_SIGNING_ALLOWED=NO \
    MARKETING_VERSION="${APP_VERSION#v}" \
    build

APP_PATH="$DERIVED_DATA/Build/Products/$CONFIGURATION/Muxa.app"
test -d "$APP_PATH"
echo "Muxa for Mac: $APP_PATH"

if [ "${1:-}" = "--open" ]; then
    open "$APP_PATH"
fi
