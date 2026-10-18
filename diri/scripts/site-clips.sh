#!/usr/bin/env bash
# Records the diri.sh feature clips from the real window and encodes them as
# the animated WebP files the home page shows (website/assets/features).
#
#   scripts/site-clips.sh            # record every clip, then encode
#   scripts/site-clips.sh <frames>   # encode frames recorded earlier
#
# Recording is the ignored `render_site_clips` test (see
# crates/diri-app/src/root/site_clips.rs); DIRI_CLIP=<name> limits it to one
# clip. Needs img2webp (brew install webp) and macOS `sips`.
set -euo pipefail

cd "$(dirname "$0")/.."
out=../website/assets/features
# The page shows clips at up to 1200 CSS pixels; this is sharp at that size
# on a 2x display without shipping the full 2400-pixel recording.
width=1800

frames=${1:-}
if [[ -z $frames ]]; then
  frames=$(mktemp -d)
  DIRI_VISUAL_OUTPUT=$frames cargo test -p diri-app --bin diri -- \
    --ignored render_site_clips
fi

command -v img2webp >/dev/null || { echo "img2webp not found: brew install webp" >&2; exit 1; }
mkdir -p "$out"
for dir in "$frames"/*/; do
  name=$(basename "$dir")
  [[ -f $dir/frames.txt ]] || continue
  scaled=$(mktemp -d)
  args=(-loop 0 -min_size)
  while read -r frame hold; do
    sips -s format png --resampleWidth "$width" "$dir/$frame" --out "$scaled/$frame" >/dev/null
    args+=(-d "$hold" -lossy -q 88 -m 6 "$scaled/$frame")
  done <"$dir/frames.txt"
  img2webp "${args[@]}" -o "$out/$name.webp"
  rm -rf "$scaled"
  printf '%-10s %s\n' "$name" "$(du -h "$out/$name.webp" | cut -f1)"
done
