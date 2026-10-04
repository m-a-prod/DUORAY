#!/bin/sh
# Builds dist/DUORAY-<version>-macos-<arch>.dmg with DUORAY.app inside
# (duoray, duoray-helper and the official xray in Contents/MacOS).
# Runs on macOS (the GitHub Actions macos runners). The app is not signed or
# notarized: on first start the user opens it with right click → Open.
#
# Usage: packaging/macos/build.sh [aarch64|x86_64]   (default: this machine)
set -eu
cd "$(dirname "$0")/../.."

ARCH=${1:-$(uname -m)}
[ "$ARCH" = arm64 ] && ARCH=aarch64
XRAY_VERSION=v26.3.27
case "$ARCH" in
    aarch64) XRAY_ZIP=Xray-macos-arm64-v8a.zip; XRAY_SHA256=2e93a67e8aa1936ecefb307e120830fcbd4c643ab9b1c46a2d0838d5f8409eaf ;;
    x86_64) XRAY_ZIP=Xray-macos-64.zip; XRAY_SHA256=f5b0471d3459eff1b82e48af0aeac186abcc3298210070afbbbd8437a4e8b203 ;;
    *) echo "unsupported arch: $ARCH (aarch64|x86_64)" >&2; exit 1 ;;
esac
TARGET=$ARCH-apple-darwin
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' crates/duoray-gui/Cargo.toml | head -1)
WORK=packaging/macos/work-$ARCH
APP=$WORK/DUORAY.app
OUT=dist/DUORAY-$VERSION-macos-$ARCH.dmg

# Match the oldest macOS we claim in Info.plist.
export MACOSX_DEPLOYMENT_TARGET=11.0
cargo build --release --target "$TARGET" -p duoray-gui -p duoray-helper
BIN=target/$TARGET/release

rm -rf "$WORK"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" dist
curl -fsSL -o "$WORK/xray.zip" "https://github.com/XTLS/Xray-core/releases/download/$XRAY_VERSION/$XRAY_ZIP"
echo "$XRAY_SHA256  $WORK/xray.zip" | shasum -a 256 -c -
unzip -q "$WORK/xray.zip" -d "$WORK/xray"

install -m 755 "$BIN/duoray" "$BIN/duoray-helper" "$WORK/xray/xray" "$APP/Contents/MacOS/"
install -m 644 "$WORK/xray/geoip.dat" "$WORK/xray/geosite.dat" "$APP/Contents/MacOS/"
install -m 644 "$WORK/xray/LICENSE" "$APP/Contents/Resources/LICENSE-xray.txt"
install -m 644 LICENSE LICENSE-EXCEPTION.md THIRD-PARTY-NOTICES.md THIRD-PARTY-CRATES.txt "$APP/Contents/Resources/"

# Icon: an iconset from the 256 px PNG.
ICONSET=$WORK/duoray.iconset
mkdir -p "$ICONSET"
for s in 16 32 64 128 256 512; do
    sips -z $s $s crates/duoray-gui/assets/duoray-256.png --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
done
for s in 16 32 128 256; do
    cp "$ICONSET/icon_$((s * 2))x$((s * 2)).png" "$ICONSET/icon_${s}x${s}@2x.png"
done
rm -f "$ICONSET/icon_64x64.png"
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/duoray.icns"

cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>DUORAY</string>
    <key>CFBundleDisplayName</key><string>DUORAY</string>
    <key>CFBundleIdentifier</key><string>space.dualizm.duoray</string>
    <key>CFBundleVersion</key><string>$VERSION</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundleExecutable</key><string>duoray</string>
    <key>CFBundleIconFile</key><string>duoray</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSHumanReadableCopyright</key><string>© 2026 DUALIZM. GNU GPL v3.</string>
</dict>
</plist>
EOF

# Ad-hoc signature: unsigned arm64 binaries do not run at all on Apple
# silicon. It is not a Developer ID, so Gatekeeper still asks once.
codesign --force --deep --sign - "$APP"

# The disk image: the app plus a link to /Applications to drag it onto.
mkdir -p "$WORK/dmg"
cp -R "$APP" "$WORK/dmg/"
ln -s /Applications "$WORK/dmg/Applications"
rm -f "$OUT"
hdiutil create -volname DUORAY -srcfolder "$WORK/dmg" -ov -format UDZO "$OUT" >/dev/null
ls -la "$OUT"
