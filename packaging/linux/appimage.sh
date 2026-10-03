#!/bin/sh
# Builds dist/DUORAY-<version>-x86_64.AppImage: one file that runs on any
# x86_64 distribution with glibc 2.28+ (Ubuntu 20.04, Debian 10, Fedora, Arch…).
# Bundles the official xray with geoip.dat/geosite.dat. On first connect the
# app installs its helper service (password prompt via polkit).
# Needs: rustup, zig, cargo-zigbuild (cargo install cargo-zigbuild), curl.
set -eu
cd "$(dirname "$0")/../.."

XRAY_VERSION=v26.3.27
XRAY_ZIP=Xray-linux-64.zip
XRAY_SHA256=23cd9af937744d97776ee35ecad4972cf4b2109d1e0fe6be9930467608f7c8ae
# Old glibc on purpose: binaries built against it run on newer ones.
TARGET=x86_64-unknown-linux-gnu
GLIBC=2.28

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' crates/duoray-gui/Cargo.toml | head -1)
CACHE=packaging/linux/cache
APPDIR=packaging/linux/AppDir
OUT=dist/DUORAY-$VERSION-x86_64.AppImage

cargo zigbuild --release --target "$TARGET.$GLIBC" -p duoray-gui -p duoray-helper
BIN=target/$TARGET/release

mkdir -p "$CACHE" dist
if [ ! -f "$CACHE/$XRAY_ZIP" ] || ! echo "$XRAY_SHA256  $CACHE/$XRAY_ZIP" | sha256sum -c - >/dev/null 2>&1; then
    curl -fsSL -o "$CACHE/$XRAY_ZIP" "https://github.com/XTLS/Xray-core/releases/download/$XRAY_VERSION/$XRAY_ZIP"
    echo "$XRAY_SHA256  $CACHE/$XRAY_ZIP" | sha256sum -c -
fi
TOOL=$CACHE/appimagetool-x86_64.AppImage
if [ ! -x "$TOOL" ]; then
    curl -fsSL -o "$TOOL" https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage
    chmod +x "$TOOL"
fi

rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" "$APPDIR/usr/share/icons/hicolor/256x256/apps"
install -m 755 "$BIN/duoray" "$BIN/duoray-helper" "$APPDIR/usr/bin/"
# xray and its databases next to duoray: that is where the app looks first.
python3 -c "import sys,zipfile; zipfile.ZipFile(sys.argv[1]).extractall(sys.argv[2])" "$CACHE/$XRAY_ZIP" "$CACHE/xray"
install -m 755 "$CACHE/xray/xray" "$APPDIR/usr/bin/xray"
install -m 644 "$CACHE/xray/geoip.dat" "$CACHE/xray/geosite.dat" "$APPDIR/usr/bin/"
install -m 644 "$CACHE/xray/LICENSE" "$APPDIR/usr/bin/LICENSE-xray.txt"

install -m 644 packaging/linux/duoray.desktop "$APPDIR/duoray.desktop"
install -m 644 packaging/linux/duoray.desktop "$APPDIR/usr/share/applications/duoray.desktop"
install -m 644 crates/duoray-gui/assets/duoray-256.png "$APPDIR/duoray.png"
install -m 644 crates/duoray-gui/assets/duoray-256.png "$APPDIR/usr/share/icons/hicolor/256x256/apps/duoray.png"
ln -sf duoray.png "$APPDIR/.DirIcon"
cat > "$APPDIR/AppRun" <<'EOF'
#!/bin/sh
HERE=$(dirname "$(readlink -f "$0")")
exec "$HERE/usr/bin/duoray" "$@"
EOF
chmod 755 "$APPDIR/AppRun"

rm -f "$OUT"
# Extract-and-run: building does not need FUSE.
ARCH=x86_64 APPIMAGE_EXTRACT_AND_RUN=1 "$TOOL" --no-appstream "$APPDIR" "$OUT"
ls -la "$OUT"
