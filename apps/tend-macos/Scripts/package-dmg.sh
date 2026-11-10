#!/bin/bash
# Build, sign, notarize and package Tend.app as a DMG that opens on any Mac.
#
# Tend cannot ship on the Mac App Store: it runs the operator's own binaries
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
#   TEND_SIGN_IDENTITY   "Developer ID Application: NAME (TEAMID)". Unset to
#                        skip signing and notarization entirely.
#   TEND_TEAM_ID         Team the notarization is submitted under.
#   TEND_NOTARY_PROFILE  `notarytool store-credentials` profile name, or
#   NOTARY_KEY_PATH + NOTARY_KEY_ID + NOTARY_ISSUER_ID
#                        App Store Connect API key, which is what CI uses.
#   TEND_SKIP_NOTARIZE=1 Sign but do not notarize (a local smoke test).
#   TEND_VERSION         Version to package. Defaults to the workspace
#                        version in Cargo.toml.
#
# Output: dist/Tend-<version>.dmg

set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
APP_DIR=$(cd "$SCRIPT_DIR/.." && pwd)
REPO_DIR=$(cd "$APP_DIR/../.." && pwd)
DIST_DIR="${TEND_DIST_DIR:-$APP_DIR/dist}"
BUILT_APP="$APP_DIR/.build/DerivedData/Build/Products/Release/Tend.app"

log() { printf '\033[1m==>\033[0m %s\n' "$*"; }
fail() { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------- build

# One version for the whole release. The app and the daemon it embeds ship
# together, so the number comes from the workspace rather than from
# project.yml, which would otherwise drift and label a DMG with a version no
# release ever carried.
VERSION="${TEND_VERSION:-}"
if [ -z "$VERSION" ]; then
    VERSION=$(sed -n '/^\[workspace\.package\]/,/^\[/p' "$REPO_DIR/Cargo.toml" \
        | sed -n 's/^version = "\(.*\)"/\1/p' | head -1)
fi
[ -n "$VERSION" ] || fail "could not determine the version to package"
VERSION="${VERSION#v}"
log "Version $VERSION"

log "Building Tend.app (Release)"
BUILD_LOG="$(mktemp -t tend-build)"
if ! CONFIGURATION=Release TEND_MARKETING_VERSION="$VERSION" \
    "$SCRIPT_DIR/build-app.sh" >"$BUILD_LOG" 2>&1; then
    # Quiet on success, but a failed build must show why.
    grep -E 'error:|warning: .*(unavailable|deprecated)|BUILD FAILED' "$BUILD_LOG" | sort -u | head -80 >&2 || true
    tail -40 "$BUILD_LOG" >&2
    fail "the app build failed; full log at $BUILD_LOG"
fi
[ -d "$BUILT_APP" ] || fail "the build produced no app at $BUILT_APP"

BUNDLE_VERSION=$(/usr/libexec/PlistBuddy -c "Print :CFBundleShortVersionString" \
    "$BUILT_APP/Contents/Info.plist" 2>/dev/null || echo "")
if [ "$BUNDLE_VERSION" != "$VERSION" ]; then
    fail "the built app says $BUNDLE_VERSION but this is release $VERSION"
fi

STAGE="$DIST_DIR/stage"
rm -rf "$STAGE" && mkdir -p "$STAGE"
cp -R "$BUILT_APP" "$STAGE/Tend.app"
APP="$STAGE/Tend.app"

# ---------------------------------------------------------------- sign

if [ -n "${TEND_SIGN_IDENTITY:-}" ]; then
    log "Signing with $TEND_SIGN_IDENTITY"
    # Inside out: every nested executable first, the bundle last. The
    # helpers are what tend actually runs — an unsigned tendd fails
    # notarization and, worse, would be refused at launch on a clean Mac.
    while IFS= read -r nested; do
        codesign --force --options runtime --timestamp \
            --sign "$TEND_SIGN_IDENTITY" "$nested"
    done < <(
        find "$APP/Contents" \
            \( -path "*/Helpers/*" -o -path "*/MacOS/*" -o -name "*.dylib" \) \
            -type f -perm -u+x ! -name "Tend" 2>/dev/null || true
    )
    # Frameworks and bundles, if the app ever grows any.
    while IFS= read -r bundle; do
        codesign --force --options runtime --timestamp \
            --sign "$TEND_SIGN_IDENTITY" "$bundle"
    done < <(find "$APP/Contents" -name "*.framework" -o -name "*.bundle" 2>/dev/null || true)

    codesign --force --options runtime --timestamp \
        --sign "$TEND_SIGN_IDENTITY" "$APP"

    log "Verifying the signature"
    codesign --verify --deep --strict --verbose=2 "$APP"
else
    log "No TEND_SIGN_IDENTITY — packaging UNSIGNED (it will not open on another Mac)"
fi

# ---------------------------------------------------------------- dmg

SUFFIX=""
[ -z "${TEND_SIGN_IDENTITY:-}" ] && SUFFIX="-unsigned"
DMG="$DIST_DIR/Tend-$VERSION$SUFFIX.dmg"
rm -f "$DMG"

ln -sf /Applications "$STAGE/Applications"

build_dmg() {
    log "Building $(basename "$DMG")"
    rm -f "$DMG"
    hdiutil create \
        -volname "Tend $VERSION" \
        -srcfolder "$STAGE" \
        -ov -format UDZO \
        -quiet \
        "$DMG"
    if [ -n "${TEND_SIGN_IDENTITY:-}" ]; then
        codesign --force --timestamp --sign "$TEND_SIGN_IDENTITY" "$DMG"
    fi
}

build_dmg

# ---------------------------------------------------------------- notarize

notarize() {
    local args=(notarytool submit "$1" --wait --timeout 30m)
    if [ -n "${TEND_NOTARY_PROFILE:-}" ]; then
        args+=(--keychain-profile "$TEND_NOTARY_PROFILE")
    elif [ -n "${NOTARY_KEY_PATH:-}" ]; then
        [ -n "${NOTARY_KEY_ID:-}" ] || fail "NOTARY_KEY_PATH needs NOTARY_KEY_ID"
        [ -n "${NOTARY_ISSUER_ID:-}" ] || fail "NOTARY_KEY_PATH needs NOTARY_ISSUER_ID"
        args+=(--key "$NOTARY_KEY_PATH" --key-id "$NOTARY_KEY_ID" --issuer "$NOTARY_ISSUER_ID")
    else
        fail "notarize called without credentials"
    fi
    [ -n "${TEND_TEAM_ID:-}" ] && args+=(--team-id "$TEND_TEAM_ID")
    log "Submitting for notarization; this waits for Apple"
    xcrun "${args[@]}"
}

# Stapling fetches the ticket from Apple's CloudKit endpoint, which drops
# connections now and then (v0.9.8 failed once with NSURLErrorDomain -1005
# after both notarizations were accepted). Retry instead of failing the
# release; a ticket that genuinely does not exist still fails every attempt.
staple() {
    local attempt
    for attempt in 1 2 3 4 5; do
        if xcrun stapler staple "$1"; then
            xcrun stapler validate "$1"
            return 0
        fi
        [ "$attempt" -lt 5 ] || break
        log "Stapling $(basename "$1") failed (attempt $attempt); retrying"
        sleep $((attempt * 10))
    done
    fail "could not staple $(basename "$1")"
}

have_notary_credentials() {
    [ -n "${TEND_NOTARY_PROFILE:-}" ] || [ -n "${NOTARY_KEY_PATH:-}" ]
}

if [ -n "${TEND_SIGN_IDENTITY:-}" ] && [ "${TEND_SKIP_NOTARIZE:-}" != "1" ]; then
    if ! have_notary_credentials; then
        # A developer packaging run without credentials: say so plainly.
        log "No notarization credentials — skipping (Gatekeeper will refuse this DMG)"
    else
        # With credentials, a failed submission must fail the release rather
        # than leave a signed but unnotarized DMG under the release name.
        notarize "$DMG" || fail "notarization of the DMG failed"
        # Homebrew and anyone who drags the app out copy only Tend.app, so a
        # ticket stapled to the DMG alone stays behind. Without its own
        # ticket the app depends on an online notarization check at first
        # launch, and when that check fails macOS says it "could not verify"
        # the app. The first submission registered the app's code hash, so
        # staple the app, rebuild the DMG around it, and notarize that.
        log "Stapling the ticket to Tend.app"
        staple "$APP"
        build_dmg
        notarize "$DMG" || fail "the rebuilt DMG failed notarization"
        log "Stapling the ticket to the DMG"
        staple "$DMG"
        log "Gatekeeper assessment"
        # The real test: what a stranger's Mac decides about these files.
        spctl -a -vvv -t install "$DMG" || fail "Gatekeeper refused the DMG"
        spctl -a -vvv -t exec "$APP" || fail "Gatekeeper refused Tend.app"
    fi
fi

rm -rf "$STAGE"
log "Done: $DMG"
[ -z "${TEND_SIGN_IDENTITY:-}" ] && log "This build is unsigned. It is for testing the packaging only."
exit 0
