#!/usr/bin/env bash
# Sign Quarry.app, package it as a .dmg, and notarize and staple the .dmg.
#
# Usage: script/sign-macos.sh path/to/Quarry.app path/to/output.dmg
#
# With MACOS_SIGNING_IDENTITY set (a "Developer ID Application: ..." identity in the
# keychain), the app is signed with the hardened runtime for distribution. Without it
# the app gets an ad-hoc signature, which runs on the machine that built it but is
# stopped by Gatekeeper when downloaded.
#
# Notarization runs when the App Store Connect API key is in ASC_KEY_ID, ASC_ISSUER_ID
# and ASC_KEY_PATH (a .p8 file) and the app was Developer ID signed.
set -euo pipefail

app="$1"
dmg="$2"
root="$(cd "$(dirname "$0")/.." && pwd)"
identity="${MACOS_SIGNING_IDENTITY:-}"

if [[ -n "$identity" ]]; then
  codesign --force --options runtime --timestamp \
    --entitlements "$root/script/macos.entitlements" \
    --sign "$identity" "$app"
else
  echo "MACOS_SIGNING_IDENTITY is not set: signing ad hoc. Gatekeeper will block downloads." >&2
  codesign --force --sign - "$app"
fi
codesign --verify --strict --verbose=2 "$app"

staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT
cp -R "$app" "$staging/"
ln -s /Applications "$staging/Applications"
rm -f "$dmg"
hdiutil create -volname Quarry -srcfolder "$staging" -fs HFS+ -format UDZO "$dmg"

if [[ -n "$identity" ]]; then
  codesign --force --timestamp --sign "$identity" "$dmg"
  if [[ -n "${ASC_KEY_ID:-}" && -n "${ASC_ISSUER_ID:-}" && -n "${ASC_KEY_PATH:-}" ]]; then
    xcrun notarytool submit "$dmg" --wait \
      --key "$ASC_KEY_PATH" --key-id "$ASC_KEY_ID" --issuer "$ASC_ISSUER_ID"
    xcrun stapler staple "$dmg"
    spctl --assess --type open --context context:primary-signature --verbose=2 "$dmg"
  else
    echo "No App Store Connect key: the .dmg is signed but not notarized." >&2
  fi
fi
echo "$dmg"
