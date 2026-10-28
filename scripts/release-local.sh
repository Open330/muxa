#!/usr/bin/env bash
# Build and attach muxa release archives from a local machine.
#
# The same archives the release workflow makes, for when it is not run:
# build on each machine that can (Linux targets here, Apple targets on a
# Mac), then publish once every target is attached.
#
#   scripts/release-local.sh build vX.Y.Z [TARGET...]  # build, package, attach to the draft
#   scripts/release-local.sh publish vX.Y.Z            # notes from CHANGELOG, then publish
#
# The tag must already be pushed; it is built in its own worktree under
# target/release-local, so this runs from any checkout. TARGET defaults to
# this host's. Linux targets build with cargo-zigbuild (from PATH, or
# through `uvx --from cargo-zigbuild`) against glibc 2.17. Archives match
# the workflow's layout and "<sha256>  <file>" sidecars, so
# scripts/bump-tap.sh reads them as is.
# The macOS DMG is not built here (apps/muxa-macos/Scripts/package-dmg.sh).

set -euo pipefail

repo=Open330/muxa
targets_all=(
  x86_64-unknown-linux-gnu
  aarch64-unknown-linux-gnu
  x86_64-apple-darwin
  aarch64-apple-darwin
)

die() { echo "error: $*" >&2; exit 1; }
usage() { sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }

command -v gh >/dev/null || die "gh is required"
cd "$(git rev-parse --show-toplevel)"

action=${1:-}
tag=${2:-}
[ -n "$action" ] && [ -n "$tag" ] || usage
[[ "$tag" == v* ]] || die "tag must start with 'v' (got '$tag')"
version=${tag#v}
shift 2

git fetch --quiet origin "refs/tags/$tag:refs/tags/$tag" || die "$tag is not pushed to origin"
tagged_version=$(git show "$tag:Cargo.toml" \
  | awk -F'"' '/^\[workspace.package\]/{p=1} p && /^version/{print $2; exit}')
[ "$tagged_version" = "$version" ] || die "$tag's Cargo.toml says $tagged_version"

notes() {
  git show "$tag:CHANGELOG.md" | awk -v want="## [$version]" '
    index($0, want) == 1 { inside = 1; next }
    # Consume the full input: exiting early can SIGPIPE git show under pipefail.
    inside && /^## \[/    { inside = 0 }
    inside                { print }
  '
}
[ -n "$(notes)" ] || die "$tag's CHANGELOG.md has no '## [$version]' section"

out="$PWD/target/release-local"
glibc=2.17
src="$out/src-$tag"

build_one() {
  local target=$1 host
  host=$(rustc -vV | awk '/^host:/{print $2}')
  (
    cd "$src"
    if [[ "$target" == *-apple-* ]]; then
      # A Mac builds both Apple targets natively (rustup target add …).
      cargo build --release --locked --target "$target" -p muxa-cli -p muxad
    # Linux always goes through zig, even for the host, pinned to an old
    # glibc: a native build would require the build host's glibc, and zig's
    # own default is newer than what earlier releases supported.
    elif command -v cargo-zigbuild >/dev/null; then
      cargo zigbuild --release --locked --target "$target.$glibc" -p muxa-cli -p muxad
    elif command -v uvx >/dev/null; then
      uvx --from cargo-zigbuild cargo-zigbuild zigbuild --release --locked \
        --target "$target.$glibc" -p muxa-cli -p muxad
    else
      die "cross-building $target needs cargo-zigbuild or uv"
    fi
  )

  local name="muxa-$tag-$target"
  local stage="$out/$name"
  rm -rf "$stage" && mkdir -p "$stage"
  # The release profile strips symbols, as the workflow's builds do.
  cp "$src/target/$target/release/muxa" "$src/target/$target/release/muxad" "$stage/"
  if [ "$target" = "$host" ]; then
    [ "$("$stage/muxa" --version)" = "muxa $version" ] || die "$target binary does not report $version"
  fi
  (cd "$src" && cp LICENSE-MIT LICENSE-APACHE README.md CHANGELOG.md \
    examples/muxad.service examples/muxa.tmux.conf examples/claude-settings.json "$stage/")
  (cd "$out" && tar czf "$name.tar.gz" "$name")
  if command -v sha256sum >/dev/null; then
    (cd "$out" && sha256sum "$name.tar.gz" > "$name.sha256")
  else
    (cd "$out" && shasum -a 256 "$name.tar.gz" > "$name.sha256")
  fi
  echo "==> attaching $name"
  gh release upload "$tag" --repo "$repo" --clobber "$out/$name.tar.gz" "$out/$name.sha256"
}

case "$action" in
  build)
    mkdir -p "$out"
    if [ -d "$src" ]; then
      git -C "$src" checkout --quiet --detach "$tag"
    else
      git worktree add --quiet --detach "$src" "$tag"
    fi
    if ! gh release view "$tag" --repo "$repo" >/dev/null 2>&1; then
      echo "==> creating draft release $tag"
      gh release create "$tag" --repo "$repo" --verify-tag --draft --title "$tag" \
        --notes "Release notes are populated from CHANGELOG.md once every archive is attached."
    fi
    [ $# -gt 0 ] || set -- "$(rustc -vV | awk '/^host:/{print $2}')"
    for target in "$@"; do build_one "$target"; done
    ;;
  publish)
    assets=$(gh release view "$tag" --repo "$repo" --json assets --jq '.assets[].name')
    for target in "${targets_all[@]}"; do
      for ext in tar.gz sha256; do
        grep -qxF "muxa-$tag-$target.$ext" <<<"$assets" \
          || die "muxa-$tag-$target.$ext is not attached; build it first"
      done
    done
    file=$(mktemp)
    trap 'rm -f "$file"' EXIT
    notes > "$file"
    gh release edit "$tag" --repo "$repo" --draft=false --latest --notes-file "$file"
    echo "==> published $tag; point the tap at it with scripts/bump-tap.sh $tag"
    ;;
  *) usage ;;
esac
