#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
SOURCE_SVG="${1:-$REPO_ROOT/assets/icon.svg}"
APPICON_DIR="$REPO_ROOT/apps/muxa-macos/Resources/Assets.xcassets/AppIcon.appiconset"

if ! command -v rsvg-convert >/dev/null 2>&1; then
  echo "rsvg-convert is required (brew install librsvg)" >&2
  exit 1
fi

if [[ ! -f "$SOURCE_SVG" ]]; then
  echo "Muxa icon source not found: $SOURCE_SVG" >&2
  exit 1
fi

mkdir -p "$APPICON_DIR"

while read -r pixels filename; do
  rsvg-convert \
    --width "$pixels" \
    --height "$pixels" \
    "$SOURCE_SVG" \
    --output "$APPICON_DIR/$filename"
done <<'SIZES'
16 AppIcon-16.png
32 AppIcon-16@2x.png
32 AppIcon-32.png
64 AppIcon-32@2x.png
128 AppIcon-128.png
256 AppIcon-128@2x.png
256 AppIcon-256.png
512 AppIcon-256@2x.png
512 AppIcon-512.png
1024 AppIcon-512@2x.png
SIZES

echo "Generated Muxa AppIcon set from $SOURCE_SVG"
