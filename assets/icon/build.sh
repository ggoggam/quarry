#!/bin/sh
# Renders quarry.svg to png/quarry-<size>.png and builds quarry.icns.
# Needs ImageMagick 7 (`magick`) and, for the .icns, macOS's `iconutil`.
set -eu
cd "$(dirname "$0")"

mkdir -p png
# Draw once at 2048 px, then scale each size down from that, so small sizes are antialiased evenly.
master=$(mktemp -d)/quarry-2048.png
magick -background none -density 192 quarry.svg "$master"
for size in 16 32 64 128 256 512 1024; do
  magick "$master" -filter Lanczos -resize "${size}x${size}" -strip "PNG32:png/quarry-$size.png"
done

if command -v iconutil >/dev/null 2>&1; then
  iconset=$(mktemp -d)/quarry.iconset
  mkdir "$iconset"
  for size in 16 32 128 256 512; do
    cp "png/quarry-$size.png" "$iconset/icon_${size}x${size}.png"
    cp "png/quarry-$((size * 2)).png" "$iconset/icon_${size}x${size}@2x.png"
  done
  iconutil -c icns -o quarry.icns "$iconset"
fi
echo "Wrote png/quarry-{16..1024}.png and quarry.icns"
