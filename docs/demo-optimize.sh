#!/usr/bin/env bash
# Shrink a demo GIF by rebuilding it on a smaller palette.
#
#   docs/demo-optimize.sh docs/demo.gif [more.gif ...]
#
# Run this after `vhs`, before committing. It rewrites in place and is
# idempotent — a second pass on an already-optimized file finds nothing
# left to take.
#
# Why 64 colors is free here: these recordings are a terminal UI, which
# draws from a fixed theme palette plus a handful of text colors. VHS
# emits a 256-colour GIF anyway, so most of that table is unused, and
# quantizing to 64 is visually lossless on this content while cutting
# roughly a quarter of the bytes. It would not be free on a photo.
#
# `Set Framerate` is not the lever it looks like: VHS 0.11 emitted 25 fps
# for these tapes whether or not it was set, so lowering it changed the
# file by 0.02%. Measure before believing the next such idea, this one
# included.

set -euo pipefail

command -v ffmpeg >/dev/null || { echo "error: ffmpeg is required" >&2; exit 1; }
[ "$#" -gt 0 ] || { echo "usage: $(basename "$0") <gif>..." >&2; exit 2; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

for gif in "$@"; do
  [ -f "$gif" ] || { echo "error: no such file: $gif" >&2; exit 1; }
  before=$(wc -c < "$gif")

  # `stats_mode=diff` weights the palette toward the pixels that actually
  # change between frames, which on a mostly-static TUI is the text rather
  # than the background.
  ffmpeg -v error -i "$gif" -vf "palettegen=max_colors=64:stats_mode=diff" -y "$work/pal.png"
  # bayer dithering rather than the default floyd_steinberg: error
  # diffusion smears noise across flat background, and on a terminal that
  # both looks wrong and costs bytes, because every frame then differs
  # everywhere.
  ffmpeg -v error -i "$gif" -i "$work/pal.png" \
    -lavfi "paletteuse=dither=bayer:bayer_scale=5:diff_mode=rectangle" -y "$work/out.gif"

  after=$(wc -c < "$work/out.gif")
  if [ "$after" -lt "$before" ]; then
    mv "$work/out.gif" "$gif"
    echo "$gif: $((before / 1024))K → $((after / 1024))K ($(( (before - after) * 100 / before ))% smaller)"
  else
    echo "$gif: already at $((before / 1024))K — left alone"
  fi
done
