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

compatible_zig_macos_sdk() {
    local candidate libsystem
    for candidate in \
        /Library/Developer/CommandLineTools/SDKs/MacOSX15.4.sdk \
        /Library/Developer/CommandLineTools/SDKs/MacOSX15.sdk \
        "$(xcrun --sdk macosx --show-sdk-path 2>/dev/null || true)"; do
        [ -d "$candidate" ] || continue
        libsystem="$candidate/usr/lib/libSystem.tbd"
        if [ -f "$libsystem" ] && grep -q 'arm64-macos' "$libsystem"; then
            printf '%s\n' "$candidate"
            return
        fi
    done
    echo "no macOS SDK compatible with Zig $ZIG_VERSION was found" >&2
    exit 1
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
