#!/usr/bin/env bash
# Point Open330/homebrew-tap at a published muxa release.
#
# The tap-bump workflow does this automatically, but only when the
# TAP_GITHUB_TOKEN secret is configured; without it the job succeeds with
# a "skipping" notice and the formula silently stays behind. This script
# is the same edit done by hand: four checksums pulled from the release
# and written into Formula/muxa.rb.
#
#   scripts/bump-tap.sh v0.8.30
#
# Refuses to run against a draft or a release whose archives are still
# uploading — a formula pointing at a URL that 404s is worse than one
# that is merely stale.

set -euo pipefail

tag="${1:-}"
if [ -z "$tag" ]; then
  echo "usage: $(basename "$0") vX.Y.Z" >&2
  exit 2
fi
case "$tag" in
  v*) ;;
  *) echo "error: tag must start with 'v' (got '$tag')" >&2; exit 2 ;;
esac
version="${tag#v}"

command -v gh >/dev/null || { echo "error: gh is required" >&2; exit 1; }

if [ "$(gh release view "$tag" --repo Open330/muxa --json isDraft --jq .isDraft)" = "true" ]; then
  echo "error: $tag is still a draft — publish it first" >&2
  exit 1
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

echo "==> collecting checksums for $tag"
mkdir "$work/sums"
(cd "$work/sums" && gh release download "$tag" --repo Open330/muxa --pattern '*.sha256')

targets=(
  aarch64-apple-darwin
  x86_64-apple-darwin
  aarch64-unknown-linux-gnu
  x86_64-unknown-linux-gnu
)
sums=()
for target in "${targets[@]}"; do
  file="$work/sums/muxa-$tag-$target.sha256"
  [ -s "$file" ] || { echo "error: missing checksum for $target" >&2; exit 1; }
  # Sidecar format: "<sha256>  <archive name>".
  sums+=("$(awk '{print $1; exit}' "$file")")
done

echo "==> updating Formula/muxa.rb"
gh repo clone Open330/homebrew-tap "$work/tap" -- --depth 1 --quiet
formula="$work/tap/Formula/muxa.rb"

# Rewrite in place: the version string, then the four sha256 literals in
# the order the formula lists them (mac arm, mac x86, linux arm, linux
# x86). Positional rather than pattern-matched per platform because the
# formula's block structure is the only thing that pairs a URL with its
# checksum, and duplicating that mapping here would be a second source of
# truth to drift.
VERSION="$version" S0="${sums[0]}" S1="${sums[1]}" S2="${sums[2]}" S3="${sums[3]}" \
python3 - "$formula" <<'PY'
import os, re, sys

path = sys.argv[1]
text = open(path).read()

text, n = re.subn(r'version "[^"]+"', f'version "{os.environ["VERSION"]}"', text, count=1)
if n != 1:
    sys.exit("error: no version line in the formula")

found = re.findall(r'sha256 "([0-9a-f]{64})"', text)
if len(found) != 4:
    sys.exit(f"error: expected 4 sha256 literals, found {len(found)}")

for old, new in zip(found, [os.environ[f"S{i}"] for i in range(4)]):
    text = text.replace(old, new, 1)

open(path, "w").write(text)
PY

cd "$work/tap"
if git diff --quiet; then
  echo "formula already at $version — nothing to push"
  exit 0
fi
git add Formula/muxa.rb
git commit -q -m "muxa $version"
git push --quiet
echo "==> pushed muxa $version to Open330/homebrew-tap"
