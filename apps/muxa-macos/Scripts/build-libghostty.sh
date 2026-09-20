#!/bin/bash

set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
APP_DIR=$(cd "$SCRIPT_DIR/.." && pwd)
BUILD_DIR="$APP_DIR/.build"
DEPS_DIR="$BUILD_DIR/dependencies"
TOOLCHAINS_DIR="$BUILD_DIR/toolchains"
GHOSTTY_DIR="$DEPS_DIR/ghostty"
SWIFT_PACKAGE_DIR="$DEPS_DIR/libghostty-spm"
STAMP_FILE="$BUILD_DIR/libghostty.stamp"

# shellcheck source=../Dependencies.lock
source "$APP_DIR/Dependencies.lock"

expected_stamp() {
    local patch_checksum
    patch_checksum=$(
        for patch_file in "$APP_DIR/Patches/libghostty-spm"/*.patch; do
            shasum -a 256 "$patch_file"
        done | shasum -a 256 | awk '{print $1}'
    )
    printf '%s\n' \
        "ghostty=$GHOSTTY_COMMIT" \
        "swift=$GHOSTTY_SWIFT_COMMIT" \
        "zig=$ZIG_VERSION" \
        "muxa_patches=$patch_checksum" \
        "platforms=macos"
}

clone_at_commit() {
    local repository=$1
    local commit=$2
    local destination=$3

    if [ -d "$destination/.git" ] &&
        [ "$(git -C "$destination" rev-parse HEAD 2>/dev/null || true)" = "$commit" ]; then
        return
    fi

    if [ -e "$destination" ]; then
        case "$destination" in
            "$DEPS_DIR"/*) rm -rf "$destination" ;;
            *) echo "refusing to replace unexpected path: $destination" >&2; exit 1 ;;
        esac
    fi

    git clone --filter=blob:none --no-checkout "$repository" "$destination"
    git -C "$destination" fetch --depth 1 origin "$commit"
    git -C "$destination" checkout --detach "$commit"
    test "$(git -C "$destination" rev-parse HEAD)" = "$commit"
}

sha256_file() {
    shasum -a 256 "$1" | awk '{print $1}'
}

install_zig() {
    local machine archive_arch expected archive zig_dir
    machine=$(uname -m)
    case "$machine" in
        arm64)
            archive_arch=aarch64
            expected=$ZIG_ARM64_SHA256
            ;;
        x86_64)
            archive_arch=x86_64
            expected=$ZIG_X86_64_SHA256
            ;;
        *)
            echo "unsupported macOS architecture: $machine" >&2
            exit 1
            ;;
    esac

    zig_dir="$TOOLCHAINS_DIR/zig-$archive_arch-macos-$ZIG_VERSION"
    if [ -x "$zig_dir/zig" ]; then
        printf '%s\n' "$zig_dir"
        return
    fi

    mkdir -p "$TOOLCHAINS_DIR"
    archive="$TOOLCHAINS_DIR/zig-$archive_arch-macos-$ZIG_VERSION.tar.xz"
    curl --fail --location --retry 3 \
        "https://ziglang.org/download/$ZIG_VERSION/zig-$archive_arch-macos-$ZIG_VERSION.tar.xz" \
        --output "$archive"
    if [ "$(sha256_file "$archive")" != "$expected" ]; then
        echo "Zig archive checksum mismatch" >&2
        exit 1
    fi

    tar -xf "$archive" -C "$TOOLCHAINS_DIR"
    test -x "$zig_dir/zig"
    printf '%s\n' "$zig_dir"
}

# The target a Zig host binary links libSystem as on this machine.
zig_host_target() {
    case "$(uname -m)" in
        arm64) printf 'arm64-macos\n' ;;
        *) printf 'x86_64-macos\n' ;;
    esac
}

# Whether Zig can link a host binary against this SDK: the first document of
# libSystem.tbd — libSystem.B itself — has to list the host target.
#
# Searching the whole file is not enough. The documents after the first
# describe the sub-libraries libSystem re-exports, and the macOS 27 SDK names
# arm64-macos in a few of those while libSystem.B offers only arm64e. That
# reads as compatible and then fails to link with every libc symbol undefined.
sdk_links_with_zig() {
    local libsystem="$1/usr/lib/libSystem.tbd"
    [ -f "$libsystem" ] || return 1
    awk '/^targets:/ { found = 1 } found { print } found && /\]/ { exit }' "$libsystem" |
        grep -q "$(zig_host_target)"
}

# Every macOS SDK installed here, oldest first, as "<version> <real path>".
# The name is no guide: MacOSX.sdk is whatever the newest one happens to be.
installed_macos_sdks() {
    local sdk
    for sdk in \
        /Library/Developer/CommandLineTools/SDKs/MacOSX*.sdk \
        "$(xcrun --sdk macosx --show-sdk-path 2>/dev/null || true)"; do
        [ -d "$sdk" ] || continue
        sdk=$(cd "$sdk" && pwd -P)
        printf '%s %s\n' \
            "$(/usr/libexec/PlistBuddy -c 'Print :Version' "$sdk/SDKSettings.plist" 2>/dev/null || echo 9999)" \
            "$sdk"
    done | sort -u | sort -V
}

# A stand-in SDK for Zig: every entry is a symlink into `base` except
# libSystem.tbd, which is copied with plain arm64 added wherever arm64e is
# offered. arm64 and arm64e processes load the same libSystem, so the symbols
# named are exactly the ones the link will find; the SDK merely stopped
# advertising them to arm64. Only Zig's host link reads this — the app itself
# is linked by Xcode against the real SDK.
make_zig_sdk_shim() {
    local base=$1 shim=$2 entry
    case "$shim" in
        "$BUILD_DIR"/*) rm -rf "$shim" ;;
        *) echo "refusing to replace unexpected path: $shim" >&2; exit 1 ;;
    esac
    mkdir -p "$shim/usr/lib"
    for entry in "$base"/*; do
        [ "$(basename "$entry")" = usr ] || ln -s "$entry" "$shim/"
    done
    for entry in "$base"/usr/*; do
        [ "$(basename "$entry")" = lib ] || ln -s "$entry" "$shim/usr/"
    done
    for entry in "$base"/usr/lib/*; do
        [ "$(basename "$entry")" = libSystem.tbd ] || ln -s "$entry" "$shim/usr/lib/"
    done
    perl -0777 -pe '
        s{(targets:\s*\[)([^\]]*)(\])}{
            my ($open, $list, $close) = ($1, $2, $3);
            my @added = grep {
                index($list, $_->[0]) >= 0 && index($list, $_->[1]) < 0
            } (["arm64e-macos", "arm64-macos"], ["arm64e-maccatalyst", "arm64-maccatalyst"]);
            @added ? "$open " . join(", ", map { $_->[1] } @added) . ",$list$close" : "$open$list$close";
        }ge
    ' "$base/usr/lib/libSystem.tbd" > "$shim/usr/lib/libSystem.tbd"
}

# The SDK Zig's build runner links against. An installed SDK that still lists
# the host target is used as it is, oldest first — Zig $ZIG_VERSION predates
# the newest headers too, and its libc++ does not compile against the macOS 27
# SDK's. Xcode 26.4 and later dropped plain arm64 from libSystem.tbd, so on a
# Mac that has only those the oldest one is wrapped in a shim instead.
compatible_zig_macos_sdk() {
    local version sdk oldest=""
    while read -r version sdk; do
        [ -n "$oldest" ] || oldest=$sdk
        if sdk_links_with_zig "$sdk"; then
            printf '%s\n' "$sdk"
            return
        fi
    done < <(installed_macos_sdks)
    if [ -z "$oldest" ]; then
        echo "no macOS SDK was found; install Xcode or the Command Line Tools" >&2
        exit 1
    fi
    sdk="$BUILD_DIR/zig-macos-sdk"
    make_zig_sdk_shim "$oldest" "$sdk"
    if ! sdk_links_with_zig "$sdk"; then
        echo "could not make $oldest linkable for Zig $ZIG_VERSION" >&2
        exit 1
    fi
    echo "libghostty: wrapped $oldest for Zig (its libSystem.tbd lists no $(zig_host_target))" >&2
    printf '%s\n' "$sdk"
}

mkdir -p "$DEPS_DIR"

clone_at_commit "$GHOSTTY_REPOSITORY" "$GHOSTTY_COMMIT" "$GHOSTTY_DIR"
clone_at_commit "$GHOSTTY_SWIFT_REPOSITORY" "$GHOSTTY_SWIFT_COMMIT" "$SWIFT_PACKAGE_DIR"

for patch_file in "$APP_DIR/Patches/libghostty-spm"/*.patch; do
    if git -C "$SWIFT_PACKAGE_DIR" apply --check --reverse "$patch_file" >/dev/null 2>&1; then
        continue
    fi
    git -C "$SWIFT_PACKAGE_DIR" apply --check "$patch_file"
    git -C "$SWIFT_PACKAGE_DIR" apply "$patch_file"
done

test "$(tr -d '[:space:]' < "$SWIFT_PACKAGE_DIR/Ghostty.ref")" = "$GHOSTTY_COMMIT"
test "$(tr -d '[:space:]' < "$SWIFT_PACKAGE_DIR/Ghostty.version")" = "$GHOSTTY_VERSION"

if [ -f "$STAMP_FILE" ] &&
    diff -q <(expected_stamp) "$STAMP_FILE" >/dev/null &&
    [ -d "$SWIFT_PACKAGE_DIR/BinaryTarget/GhosttyKit.xcframework" ] &&
    [ -f "$SWIFT_PACKAGE_DIR/Package.swift" ]; then
    echo "libghostty: pinned XCFramework is ready"
    exit 0
fi

ZIG_DIR=$(install_zig)
export MUXA_REAL_ZIG="$ZIG_DIR/zig"
export MUXA_ZIG_MACOS_SDK="$(compatible_zig_macos_sdk)"
export PATH="$SCRIPT_DIR/toolchain:$ZIG_DIR:$PATH"
test "$(zig version)" = "$ZIG_VERSION"
echo "libghostty: Zig host SDK $MUXA_ZIG_MACOS_SDK"

# A failed build runner can remain in Zig's explicit cache even after the SDK
# selection is fixed. This directory is a generated, app-scoped build cache.
rm -rf "$SWIFT_PACKAGE_DIR/build/cache"

(
    cd "$SWIFT_PACKAGE_DIR"
    ./Script/build.sh \
        --source "$GHOSTTY_DIR" \
        --ref "$GHOSTTY_COMMIT" \
        --platforms macos \
        --skip-tests
)

# Force the Swift wrapper to consume the XCFramework built immediately above,
# never its release-hosted binary target. Pin the one remaining source package.
sed 's/from: "2.1.0"/exact: "2.2.0"/' \
    "$SWIFT_PACKAGE_DIR/Package.local.swift" > "$SWIFT_PACKAGE_DIR/Package.swift"

expected_stamp > "$STAMP_FILE"

echo "libghostty: built $SWIFT_PACKAGE_DIR/BinaryTarget/GhosttyKit.xcframework"
echo "libghostty: Ghostty $GHOSTTY_VERSION ($GHOSTTY_COMMIT)"
