#!/bin/bash

set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
APP_DIR=$(cd "$SCRIPT_DIR/.." && pwd)
CONFIGURATION=${CONFIGURATION:-Release}
SOURCE_APP="$APP_DIR/.build/DerivedData/Build/Products/$CONFIGURATION/MuxaQAHelper.app"
INSTALL_ROOT=${MUXA_QA_INSTALL_DIR:-"$HOME/Applications"}
INSTALLED_APP="$INSTALL_ROOT/MuxaQAHelper.app"

CONFIGURATION="$CONFIGURATION" "$SCRIPT_DIR/build-qa-helper.sh"

if [ -n "${MUXA_QA_CODESIGN_IDENTITY:-}" ]; then
    SIGNING_IDENTITY=$MUXA_QA_CODESIGN_IDENTITY
else
    SIGNING_IDENTITY=$(security find-identity -v -p codesigning \
        | sed -n 's/.*"\(Apple Development:[^"]*\)".*/\1/p' \
        | head -n 1)
fi

if [ -z "$SIGNING_IDENTITY" ]; then
    echo "No Apple Development signing identity is available." >&2
    echo "Set MUXA_QA_CODESIGN_IDENTITY to a stable signing identity and retry." >&2
    exit 1
fi

mkdir -p "$INSTALL_ROOT"
WORK_DIR=$(mktemp -d /tmp/muxa-qa-helper-install.XXXXXX)
STAGED_APP="$WORK_DIR/staged.app"
BACKUP_APP="$WORK_DIR/previous.app"
RESTORE_REQUIRED=0

cleanup() {
    if [ "$RESTORE_REQUIRED" -eq 1 ] && [ -d "$BACKUP_APP" ]; then
        if [ -e "$INSTALLED_APP" ]; then
            mv "$INSTALLED_APP" "$WORK_DIR/failed.app" || true
        fi
        mv "$BACKUP_APP" "$INSTALLED_APP"
    fi
    rm -rf "$WORK_DIR"
}
trap cleanup EXIT

ditto "$SOURCE_APP" "$STAGED_APP"
if ! codesign \
        --force \
        --options runtime \
        --timestamp=none \
        --sign "$SIGNING_IDENTITY" \
        "$STAGED_APP"; then
    echo "Could not use the Apple Development private key." >&2
    echo "Unlock the login keychain in Keychain Access, then run this installer again." >&2
    exit 1
fi
codesign --verify --strict --verbose=2 "$STAGED_APP"

osascript -e 'tell application id "dev.muxa.qa-helper" to quit' 2>/dev/null || true
for _ in $(seq 1 40); do
    pgrep -f '/MuxaQAHelper.app/Contents/MacOS/MuxaQAHelper$' >/dev/null || break
    sleep 0.2
done

if [ -d "$INSTALLED_APP" ]; then
    mv "$INSTALLED_APP" "$BACKUP_APP"
    RESTORE_REQUIRED=1
fi

ditto "$STAGED_APP" "$INSTALLED_APP"
codesign --verify --strict --verbose=2 "$INSTALLED_APP"

open "$INSTALLED_APP"
HELPER_READY=0
for _ in $(seq 1 50); do
    if "$SCRIPT_DIR/muxa-qa-helper-client.py" status >/dev/null 2>&1; then
        HELPER_READY=1
        break
    fi
    sleep 0.1
done
if [ "$HELPER_READY" -ne 1 ]; then
    echo "Muxa QA Helper was installed but its local service did not start." >&2
    exit 1
fi

RESTORE_REQUIRED=0
echo "Installed Muxa QA Helper: $INSTALLED_APP"
echo "Signing identity: $SIGNING_IDENTITY"
