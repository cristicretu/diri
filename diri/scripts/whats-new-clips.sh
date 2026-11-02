#!/usr/bin/env bash
# Records the What's New clips from the real window and encodes them as the
# animated WebP files the app embeds (crates/diri-app/assets/whats-new). The
# dark recordings also go to the website's What's New page
# (website/assets/whats-new), which matches the site's dark palette.
#
#   scripts/whats-new-clips.sh            # record every clip, then encode
#   scripts/whats-new-clips.sh <frames>   # encode frames recorded earlier
#
# Recording is the ignored `render_whats_new_clips` test (see
# crates/diri-app/src/root/whats_new_clips.rs); DIRI_CLIP=<name> limits it to
# one clip. Needs img2webp (brew install webp) and macOS `sips`.
set -euo pipefail

cd "$(dirname "$0")/.."
out=crates/diri-app/assets/whats-new
# The sheet shows clips at 720x450 points; encode for a 2x display.
width=1440

frames=${1:-}
if [[ -z $frames ]]; then
  frames=$(mktemp -d)
  DIRI_VISUAL_OUTPUT=$frames cargo test -p diri-app --bin diri -- \
    --ignored render_whats_new_clips
fi

command -v img2webp >/dev/null || { echo "img2webp not found: brew install webp" >&2; exit 1; }
mkdir -p "$out"
for dir in "$frames"/*/; do
  name=$(basename "$dir")
  [[ -f $dir/frames.txt ]] || continue
  # Gesture clips carry their pointer and keys separately (overlay.txt).
  if [[ -f $dir/overlay.txt && ! -f $dir/.overlaid ]]; then
    python3 scripts/whats-new-overlay.py "$dir"
    touch "$dir/.overlaid"
  fi
  scaled=$(mktemp -d)
  args=(-loop 0 -min_size)
  while read -r frame hold; do
    sips -s format png --resampleWidth "$width" "$dir/$frame" --out "$scaled/$frame" >/dev/null
    args+=(-d "$hold" -lossless -m 6 "$scaled/$frame")
  done <"$dir/frames.txt"
  img2webp "${args[@]}" -o "$out/$name.webp"
  rm -rf "$scaled"
  printf '%-14s %s\n' "$name" "$(du -h "$out/$name.webp" | cut -f1)"
  if [[ $name == *-dark ]]; then
    cp "$out/$name.webp" "../website/assets/whats-new/${name%-dark}.webp"
  fi
done
