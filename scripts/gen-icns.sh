#!/bin/sh
# Regenerates docs/assets/brand/Clusia.icns from docs/assets/brand/clusia-app-icon.svg: every
# size from 16 to 1024 pixels (the 512 point entry at @2x). Needs rsvg-convert (brew install
# librsvg) and iconutil (part of macOS). An output path may be given as the only argument.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
svg="$root/docs/assets/brand/clusia-app-icon.svg"
out="${1:-$root/docs/assets/brand/Clusia.icns}"
if ! command -v rsvg-convert >/dev/null 2>&1; then
  echo "gen-icns.sh: rsvg-convert is not installed (brew install librsvg)" >&2
  exit 1
fi
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
set="$work/Clusia.iconset"
mkdir "$set"
for size in 16 32 128 256 512; do
  double=$((size * 2))
  rsvg-convert -w "$size" -h "$size" "$svg" -o "$set/icon_${size}x${size}.png"
  rsvg-convert -w "$double" -h "$double" "$svg" -o "$set/icon_${size}x${size}@2x.png"
done
iconutil -c icns "$set" -o "$out"
echo "wrote $out"
