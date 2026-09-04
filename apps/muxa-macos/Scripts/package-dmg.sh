#!/bin/bash
# Build, sign, notarize and package Muxa.app as a DMG that opens on any Mac.
#
# Muxa cannot ship on the Mac App Store: it runs the operator's own binaries
# (tmux, the agent CLIs, ssh), talks to a daemon that outlives it through a
# socket in /tmp, and reads whatever project directory the work is in. The
# App Sandbox forbids all three. So the distribution path is the one every
# other terminal-shaped Mac tool takes — Developer ID, notarized, outside the
# store.
#
# Signing and notarization are optional here on purpose: without credentials
# this still produces a DMG, clearly labelled unsigned, so the packaging path
# stays exercised before the certificates exist. An unsigned DMG will not
# open on anyone else's Mac.
#
# Environment:
#   MUXA_SIGN_IDENTITY   "Developer ID Application: NAME (TEAMID)". Unset to
#                        skip signing and notarization entirely.
#   MUXA_TEAM_ID         Team the notarization is submitted under.
#   MUXA_NOTARY_PROFILE  `notarytool store-credentials` profile name, or
#   NOTARY_KEY_PATH + NOTARY_KEY_ID + NOTARY_ISSUER_ID
#                        App Store Connect API key, which is what CI uses.
#   MUXA_SKIP_NOTARIZE=1 Sign but do not notarize (a local smoke test).
#   MUXA_VERSION         Version to package. Defaults to the workspace
#                        version in Cargo.toml.
#
# Output: dist/Muxa-<version>.dmg

set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
APP_DIR=$(cd "$SCRIPT_DIR/.." && pwd)
REPO_DIR=$(cd "$APP_DIR/../.." && pwd)
DIST_DIR="${MUXA_DIST_DIR:-$APP_DIR/dist}"
BUILT_APP="$APP_DIR/.build/DerivedData/Build/Products/Release/Muxa.app"

log() { printf '\033[1m==>\033[0m %s\n' "$*"; }
fail() { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------- build

# One version for the whole release. The app and the daemon it embeds ship
# together, so the number comes from the workspace rather than from
# project.yml, which would otherwise drift and label a DMG with a version no
# release ever carried.
VERSION="${MUXA_VERSION:-}"
if [ -z "$VERSION" ]; then
    VERSION=$(sed -n '/^\[workspace\.package\]/,/^\[/p' "$REPO_DIR/Cargo.toml" \
        | sed -n 's/^version = "\(.*\)"/\1/p' | head -1)
fi
[ -n "$VERSION" ] || fail "could not determine the version to package"
VERSION="${VERSION#v}"
log "Version $VERSION"

log "Building Muxa.app (Release)"
CONFIGURATION=Release MUXA_MARKETING_VERSION="$VERSION" \
    "$SCRIPT_DIR/build-app.sh" >/dev/null
[ -d "$BUILT_APP" ] || fail "the build produced no app at $BUILT_APP"

BUNDLE_VERSION=$(/usr/libexec/PlistBuddy -c "Print :CFBundleShortVersionString" \
    "$BUILT_APP/Contents/Info.plist" 2>/dev/null || echo "")
if [ "$BUNDLE_VERSION" != "$VERSION" ]; then
    fail "the built app says $BUNDLE_VERSION but this is release $VERSION"
fi

STAGE="$DIST_DIR/stage"
rm -rf "$STAGE" && mkdir -p "$STAGE"
cp -R "$BUILT_APP" "$STAGE/Muxa.app"
APP="$STAGE/Muxa.app"

# ---------------------------------------------------------------- sign

if [ -n "${MUXA_SIGN_IDENTITY:-}" ]; then
    log "Signing with $MUXA_SIGN_IDENTITY"
    # Inside out: every nested executable first, the bundle last. The
    # helpers are what muxa actually runs — an unsigned muxad fails
    # notarization and, worse, would be refused at launch on a clean Mac.
    while IFS= read -r nested; do
        codesign --force --options runtime --timestamp \
            --sign "$MUXA_SIGN_IDENTITY" "$nested"
    done < <(
        find "$APP/Contents" \
            \( -path "*/Helpers/*" -o -path "*/MacOS/*" -o -name "*.dylib" \) \
            -type f -perm -u+x ! -name "Muxa" 2>/dev/null || true
    )
    # Frameworks and bundles, if the app ever grows any.
    while IFS= read -r bundle; do
        codesign --force --options runtime --timestamp \
            --sign "$MUXA_SIGN_IDENTITY" "$bundle"
    done < <(find "$APP/Contents" -name "*.framework" -o -name "*.bundle" 2>/dev/null || true)

    codesign --force --options runtime --timestamp \
        --sign "$MUXA_SIGN_IDENTITY" "$APP"

    log "Verifying the signature"
    codesign --verify --deep --strict --verbose=2 "$APP"
else
    log "No MUXA_SIGN_IDENTITY — packaging UNSIGNED (it will not open on another Mac)"
fi

# ---------------------------------------------------------------- dmg

SUFFIX=""
[ -z "${MUXA_SIGN_IDENTITY:-}" ] && SUFFIX="-unsigned"
DMG="$DIST_DIR/Muxa-$VERSION$SUFFIX.dmg"
rm -f "$DMG"

log "Building $(basename "$DMG")"
ln -sf /Applications "$STAGE/Applications"
hdiutil create \
    -volname "Muxa $VERSION" \
    -srcfolder "$STAGE" \
    -ov -format UDZO \
    -quiet \
    "$DMG"

if [ -n "${MUXA_SIGN_IDENTITY:-}" ]; then
    codesign --force --timestamp --sign "$MUXA_SIGN_IDENTITY" "$DMG"
fi

# ---------------------------------------------------------------- notarize

notarize() {
    local args=(notarytool submit "$DMG" --wait --timeout 30m)
    if [ -n "${MUXA_NOTARY_PROFILE:-}" ]; then
        args+=(--keychain-profile "$MUXA_NOTARY_PROFILE")
    elif [ -n "${NOTARY_KEY_PATH:-}" ]; then
        [ -n "${NOTARY_KEY_ID:-}" ] || fail "NOTARY_KEY_PATH needs NOTARY_KEY_ID"
        [ -n "${NOTARY_ISSUER_ID:-}" ] || fail "NOTARY_KEY_PATH needs NOTARY_ISSUER_ID"
        args+=(--key "$NOTARY_KEY_PATH" --key-id "$NOTARY_KEY_ID" --issuer "$NOTARY_ISSUER_ID")
    else
        log "No notarization credentials — skipping (Gatekeeper will refuse this DMG)"
        return 1
    fi
    [ -n "${MUXA_TEAM_ID:-}" ] && args+=(--team-id "$MUXA_TEAM_ID")
    log "Submitting for notarization; this waits for Apple"
    xcrun "${args[@]}"
}

if [ -n "${MUXA_SIGN_IDENTITY:-}" ] && [ "${MUXA_SKIP_NOTARIZE:-}" != "1" ]; then
    if notarize; then
        log "Stapling the ticket"
        xcrun stapler staple "$DMG"
        xcrun stapler validate "$DMG"
        log "Gatekeeper assessment"
        # The real test: what a stranger's Mac decides about this file.
        spctl -a -vvv -t install "$DMG" || fail "Gatekeeper refused the DMG"
    fi
fi

rm -rf "$STAGE"
log "Done: $DMG"
[ -z "${MUXA_SIGN_IDENTITY:-}" ] && log "This build is unsigned. It is for testing the packaging only."
exit 0
