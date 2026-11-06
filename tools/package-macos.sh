#!/bin/bash
# Builds Tailgate.app from an arm64 and an x86_64 binary, signs it, and wraps it
# in a .dmg. Runs on macOS: package-macos.sh ARM64_BIN X86_64_BIN OUTDIR
#
# Signing: with MACOS_SIGN_IDENTITY set (a "Developer ID Application: ..." identity
# in the keychain) the app and .dmg get a real signature with the hardened runtime,
# and with NOTARY_PROFILE also set (a `xcrun notarytool store-credentials` profile)
# the .dmg is notarized and stapled. Without them it's an ad-hoc signature, which
# Gatekeeper asks about once (see READ ME FIRST).
set -euo pipefail
cd "$(dirname "$0")/.."
ARM=$1; X86=$2; OUT=$(mkdir -p "$3" && cd "$3" && pwd)
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
NAME="tailgate-$VERSION-macos-universal"
STAGE=$(mktemp -d); APP="$STAGE/Tailgate.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$STAGE/Licenses"
lipo -create -output "$APP/Contents/MacOS/tailgate" "$ARM" "$X86"
lipo -info "$APP/Contents/MacOS/tailgate"
cp assets/tailgate.icns "$APP/Contents/Resources/tailgate.icns"
sed "s/@VERSION@/$VERSION/g" tools/dist/Info.plist > "$APP/Contents/Info.plist"
plutil -lint "$APP/Contents/Info.plist"
if [ -n "${MACOS_SIGN_IDENTITY:-}" ]; then
  codesign --force --options runtime --timestamp --sign "$MACOS_SIGN_IDENTITY" "$APP"
else
  # Apple Silicon refuses to run a binary with no signature at all.
  codesign --force --deep --sign - "$APP"
fi
codesign --verify --strict --verbose=2 "$APP"
python3 -I tools/third-party-licenses.py "$STAGE/Licenses/THIRD-PARTY-LICENSES.txt" aarch64-apple-darwin x86_64-apple-darwin
cp README.md NOTICE LICENSE-MIT LICENSE-APACHE "$STAGE/Licenses/"
cp -r LICENSES/. "$STAGE/Licenses/"
if [ -n "${MACOS_SIGN_IDENTITY:-}" ] && [ -n "${NOTARY_PROFILE:-}" ]; then
  cp tools/dist/READ-ME-FIRST-macos-notarized.txt "$STAGE/READ ME FIRST.txt"
else
  cp tools/dist/READ-ME-FIRST-macos.txt "$STAGE/READ ME FIRST.txt"
fi
ln -s /Applications "$STAGE/Applications"
rm -f "$OUT/$NAME.dmg"
hdiutil create -volname "Tailgate $VERSION" -srcfolder "$STAGE" -fs HFS+ -format UDZO -ov "$OUT/$NAME.dmg" >/dev/null
if [ -n "${MACOS_SIGN_IDENTITY:-}" ]; then
  codesign --force --timestamp --sign "$MACOS_SIGN_IDENTITY" "$OUT/$NAME.dmg"
  if [ -n "${NOTARY_PROFILE:-}" ]; then
    xcrun notarytool submit "$OUT/$NAME.dmg" --keychain-profile "$NOTARY_PROFILE" ${NOTARY_KEYCHAIN:+--keychain "$NOTARY_KEYCHAIN"} --wait
    xcrun stapler staple "$OUT/$NAME.dmg"
    spctl --assess --type open --context context:primary-signature --verbose=2 "$OUT/$NAME.dmg"
  fi
fi
(cd "$OUT" && shasum -a 256 "$NAME.dmg" > "$NAME.dmg.sha256" && cat "$NAME.dmg.sha256")
rm -rf "$STAGE"
