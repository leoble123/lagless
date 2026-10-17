#!/usr/bin/env bash
# Build Lagless.app, Lagless.dmg and Lagless-macos.zip (the auto-update payload) on a Mac.
#   ./packaging/macos/package.sh            # universal (Apple Silicon + Intel)
# Set LAGLESS_UPDATE_REPO=owner/repo to enable auto-update from that repo's GitHub Releases.
set -euo pipefail
cd "$(dirname "$0")/../.."
VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
OUT=dist
rm -rf "$OUT/Lagless.app" && mkdir -p "$OUT"

rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin

APP="$OUT/Lagless.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
lipo -create -output "$APP/Contents/MacOS/lagless" \
  target/aarch64-apple-darwin/release/lagless target/x86_64-apple-darwin/release/lagless
sed "s/__VERSION__/$VERSION/g" packaging/macos/Info.plist > "$APP/Contents/Info.plist"

ICONSET=$(mktemp -d)/AppIcon.iconset && mkdir -p "$ICONSET"
for s in 16 32 128 256 512; do
  sips -z $s $s assets/icon-1024.png --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
  sips -z $((s*2)) $((s*2)) assets/icon-1024.png --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"

# Ad-hoc signature (required on Apple Silicon). For no Gatekeeper warning at all you need a
# Developer ID certificate + notarization; set MACOS_SIGN_IDENTITY to use one.
if [ -n "${MACOS_SIGN_IDENTITY:-}" ]; then
  codesign --force --deep --options runtime --timestamp -s "$MACOS_SIGN_IDENTITY" "$APP"
else
  codesign --force --deep -s - "$APP"
fi

# Auto-update payload.
rm -f "$OUT/Lagless-macos.zip"
ditto -c -k --norsrc --keepParent "$APP" "$OUT/Lagless-macos.zip"

# Drag-to-Applications disk image.
STAGE=$(mktemp -d)
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
rm -f "$OUT/Lagless.dmg"
hdiutil create -volname "Lagless" -srcfolder "$STAGE" -ov -format UDZO "$OUT/Lagless.dmg" >/dev/null

(cd "$OUT" && for f in Lagless.dmg Lagless-macos.zip; do shasum -a 256 "$f" > "$f.sha256"; done)
echo "Built $OUT/Lagless.app, $OUT/Lagless.dmg, $OUT/Lagless-macos.zip (v$VERSION)"
