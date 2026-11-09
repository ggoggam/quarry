#!/usr/bin/env bash
# Wrap the quarry binary in a macOS .app bundle.
#
# Usage: script/bundle-macos.sh [debug|release]   (default: release)
#
# QUARRY_BINARY=path skips the build and bundles that binary instead (the release
# workflow passes a universal binary made with lipo). The bundle lands next to
# the profile's binary as target/<profile>/Quarry.app, or in QUARRY_BUNDLE_DIR.
set -euo pipefail

profile="${1:-release}"
root="$(cd "$(dirname "$0")/.." && pwd)"

binary="${QUARRY_BINARY:-}"
if [[ -z "$binary" ]]; then
  if [[ "$profile" == release ]]; then
    cargo build --release --locked -p quarry-app --manifest-path "$root/Cargo.toml"
  else
    cargo build --locked -p quarry-app --manifest-path "$root/Cargo.toml"
  fi
  binary="$root/target/$profile/quarry"
fi

version="$(sed -n '/^\[workspace.package\]/,/^\[/ s/^version = "\(.*\)"/\1/p' "$root/Cargo.toml")"
out_dir="${QUARRY_BUNDLE_DIR:-$root/target/$profile}"
app="$out_dir/Quarry.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$binary" "$app/Contents/MacOS/quarry"
cp "$root/assets/icon/quarry.icns" "$app/Contents/Resources/quarry.icns"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Quarry</string>
  <key>CFBundleDisplayName</key><string>Quarry</string>
  <key>CFBundleIdentifier</key><string>dev.ggoggam.quarry</string>
  <key>CFBundleExecutable</key><string>quarry</string>
  <key>CFBundleIconFile</key><string>quarry</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSHumanReadableCopyright</key><string>Apache-2.0</string>
</dict>
</plist>
PLIST
echo "$app"
