# Quarry icon

An open pit cut into a slab of ground in two terraces. The terraces are the rows and layers of a
table being dug out, and the whole mark is one flat silhouette with a notch, so it sits beside
Hangar (a dome with a door) and Sokuri (a bowl with a handle). Like theirs, it is one ink on paper,
its convex corners take a fillet of 1 dp of the 72 dp app icon square, and its inner corners stay
sharp.

The canvas follows the macOS icon grid: a 824 px rounded square (corner radius 185 px) inset 100 px
on a 1024 px canvas, with no drop shadow, since the other two icons are flat. Every edge of the mark
sits on a 32 px grid, so the terraces land on whole pixels at 32 px and 16 px.

| Role  | Hex       | Tailwind / shadcn |
| ----- | --------- | ----------------- |
| Ink   | `#171717` | neutral-900       |
| Paper | `#FAFAFA` | neutral-50        |

`quarry-dark.svg` swaps the two.

## Files

- `quarry.svg`: the master, written by hand. Change the icon here.
- `quarry-dark.svg`: the same mark, light on dark.
- `png/quarry-{16,32,64,128,256,512,1024}.png`: renders of `quarry.svg`.
- `quarry.icns`: the macOS icon, built from those PNGs.

## Regenerating

```sh
assets/icon/build.sh
```

The script needs ImageMagick 7 (`magick`, which renders SVG itself, so `rsvg-convert` isn't
needed) and macOS's `iconutil`. It draws the SVG once at 2048 px, scales that down to each size, then
packs the 1x and 2x sizes from 16 px to 512 px into `quarry.icns`.
