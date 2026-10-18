#!/usr/bin/env bash
# Records the diri.sh feature clips from the real window and encodes them as
# the animated WebP files the home page shows (website/assets/features).
#
#   scripts/site-clips.sh            # record every clip, then encode
#   scripts/site-clips.sh <frames>   # encode frames recorded earlier
#
# Recording is the ignored `render_site_clips` test (see
# crates/diri-app/src/root/site_clips.rs); DIRI_CLIP=<name> limits it to one
# clip. Needs img2webp (brew install webp) and ffmpeg.
set -euo pipefail

cd "$(dirname "$0")/.."
out=../website/assets/features

frames=${1:-}
if [[ -z $frames ]]; then
  frames=$(mktemp -d)
  DIRI_VISUAL_OUTPUT=$frames cargo test -p diri-app --bin diri -- \
    --ignored render_site_clips
fi

command -v img2webp >/dev/null || { echo "img2webp not found: brew install webp" >&2; exit 1; }
mkdir -p "$out"
# The page zooms into the part of some clips that matters: width height x y,
# in recorded (2x) pixels. Others show the whole window.
crop_for() {
  case $1 in
    schedules) echo "1280 900 0 0" ;;       # the clocked rows and the run
    voice) echo "1280 900 516 230" ;;       # the spoken summary and the answer
    computer) echo "1756 1140 510 14" ;;    # inside the terminal pane, down to its last row
  esac
}

# The dictation pill drawn over the frames a clip's listen.txt names: a
# capsule at x y (recorded pixels, before the crop) with a moving waveform.
pill_at() {
  case $1 in
    voice) echo "941 960" ;;                # under the prompt, left of the fade
  esac
}
pill=$(mktemp -d)/pill.png
ffmpeg -loglevel error -f lavfi -i color=c=black:s=200x60 -frames:v 1 -vf "format=rgba,\
geq=r='if(gt(hypot(max(0,abs(X-100)-70),Y-30),28),64,10)':\
g='if(gt(hypot(max(0,abs(X-100)-70),Y-30),28),61,9)':\
b='if(gt(hypot(max(0,abs(X-100)-70),Y-30),28),80,15)':\
a='clip(255*(30.5-hypot(max(0,abs(X-100)-70),Y-30)),0,255)'" "$pill"
RANDOM=7
waveform() { # drawbox filters for 11 bars centred in the pill at $1 $2
  local x=$(($1 + 37)) cy=$(($2 + 30)) out="" i h
  for i in 0 1 2 3 4 5 6 7 8 9 10; do
    h=$((6 + RANDOM % (8 + 26 * (5 - (i > 5 ? i - 5 : 5 - i)) / 5)))
    out+=",drawbox=x=$((x + i * 12)):y=$((cy - h / 2)):w=6:h=$h:color=white@0.92:t=fill"
  done
  echo "$out"
}

for dir in "$frames"/*/; do
  name=$(basename "$dir")
  [[ -f $dir/frames.txt ]] || continue
  read -r w h x y <<<"$(crop_for "$name")" || true
  src=$dir
  if [[ -n ${w:-} ]]; then
    src=$(mktemp -d)
    # sips centres any crop whose offset is 0,0; ffmpeg crops exactly.
    while read -r frame _; do
      if grep -qx "$frame" "$dir/listen.txt" 2>/dev/null; then
        read -r px py <<<"$(pill_at "$name")"
        ffmpeg -loglevel error -i "$dir/$frame" -i "$pill" -filter_complex \
          "[0][1]overlay=$px:$py$(waveform "$px" "$py"),crop=$w:$h:$x:$y" "$src/$frame"
      else
        ffmpeg -loglevel error -i "$dir/$frame" -vf "crop=$w:$h:$x:$y" "$src/$frame"
      fi
    done <"$dir/frames.txt"
  fi
  # Lossless at the recorded 2x size: terminal text smears under lossy
  # compression and any resampling.
  args=(-loop 0 -min_size)
  while read -r frame hold; do
    args+=(-d "$hold" -lossless -m 6 "$src/$frame")
  done <"$dir/frames.txt"
  img2webp "${args[@]}" -o "$out/$name.webp"
  [[ $src == "$dir" ]] || rm -rf "$src"
  unset w h x y
  printf '%-10s %s\n' "$name" "$(du -h "$out/$name.webp" | cut -f1)"
done
