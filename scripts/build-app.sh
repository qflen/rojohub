#!/bin/bash
# Builds dist/RojoHub.app and dist/RojoHub-mac.zip from this checkout.
#
#   scripts/build-app.sh                              cargo build --release for this Mac, then assemble
#   scripts/build-app.sh --bin PATH [--bin PATH]      use prebuilt binaries; two (arm64 and x64) are merged with lipo
#   scripts/build-app.sh ... --compare PATH           also diff the result against an installed app
#
# The app is: packaging/launcher.sh as Contents/MacOS/RojoHub (it installs the CLI shim, the
# Studio plugins and the login service on first launch), the rojohub binary as
# Contents/Resources/bin/rojohub (a different folder, because the default file system is
# case-insensitive and "RojoHub" and "rojohub" would be one file), packaging/Info.plist.in
# and the icon. Nothing else is bundled. The app is not signed or
# notarized: users clear the quarantine flag once, as the README says.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"

BINS=()
COMPARE=""
while [ $# -gt 0 ]; do
  case "$1" in
    --bin) BINS+=("${2:?--bin needs a path}"); shift 2 ;;
    --compare) COMPARE="${2:?--compare needs a path to an app}"; shift 2 ;;
    *) echo "unknown argument $1"; exit 1 ;;
  esac
done

[ "$(uname -s)" = "Darwin" ] || { echo "build on macOS (ditto, plutil, lipo)"; exit 1; }
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
[ -n "$VERSION" ] || { echo "no version in Cargo.toml"; exit 1; }

DIST="$ROOT/dist"
APP="$DIST/RojoHub.app"
mkdir -p "$DIST"

echo "== RojoHub $VERSION"
if [ ${#BINS[@]} -eq 0 ]; then
  echo "== cargo build --release"
  cargo build --release
  BINS=("target/release/rojohub")
fi
if [ ${#BINS[@]} -gt 1 ]; then
  echo "== lipo ${BINS[*]}"
  lipo -create -output "$DIST/rojohub-universal" "${BINS[@]}"
  BIN="$DIST/rojohub-universal"
else
  BIN="${BINS[0]}"
fi
lipo -archs "$BIN" | sed 's/^/  archs: /'

echo "== Assemble"
rm -rf "$APP" && mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources/bin"
sed "s/__VERSION__/$VERSION/g" packaging/Info.plist.in > "$APP/Contents/Info.plist"
plutil -lint "$APP/Contents/Info.plist" >/dev/null
install -m 755 packaging/launcher.sh "$APP/Contents/MacOS/RojoHub"
install -m 755 "$BIN" "$APP/Contents/Resources/bin/rojohub"
cp packaging/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
xattr -cr "$APP" 2>/dev/null || true

echo "== Smoke test"
got="$("$APP/Contents/Resources/bin/rojohub" version)"
[ "$got" = "$VERSION" ] || { echo "  !! bundled binary reports $got, expected $VERSION"; exit 1; }
echo "  rojohub $got ok"
bash -n "$APP/Contents/MacOS/RojoHub" && echo "  launcher syntax ok"

echo "== Zip"
rm -f "$DIST/RojoHub-mac.zip"
(cd "$DIST" && ditto -c -k --sequesterRsrc --keepParent RojoHub.app RojoHub-mac.zip)
echo "  $DIST/RojoHub-mac.zip ($(du -h "$DIST/RojoHub-mac.zip" | cut -f1))"

if [ -n "$COMPARE" ]; then
  echo "== Compare with $COMPARE"
  if diff -rq "$COMPARE" "$APP"; then echo "  identical"; else echo "  (differences above)"; fi
fi
