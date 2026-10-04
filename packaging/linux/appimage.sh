#!/bin/sh
# Builds dist/DUORAY-<version>-<arch>.AppImage: one file that runs on any
# distribution with glibc 2.28+ (Ubuntu 20.04, Debian 10, Fedora, Arch…).
# Bundles the official xray with geoip.dat/geosite.dat. On first connect the
# app installs its helper service (password prompt via polkit).
#
# Usage: packaging/linux/appimage.sh [x86_64|aarch64]   (default: this machine)
# Needs: what stage.sh needs, plus curl.
set -eu
cd "$(dirname "$0")/../.."

ARCH=${1:-$(uname -m)}
[ "$ARCH" = arm64 ] && ARCH=aarch64
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' crates/duoray-gui/Cargo.toml | head -1)
CACHE=packaging/linux/cache
APPDIR=packaging/linux/AppDir-$ARCH
OUT=dist/DUORAY-$VERSION-$ARCH.AppImage

packaging/linux/stage.sh "$ARCH"
ROOT=packaging/linux/root-$ARCH

# appimagetool runs on the build machine; it packs for $ARCH by itself.
HOST=$(uname -m)
TOOL=$CACHE/appimagetool-$HOST.AppImage
if [ ! -x "$TOOL" ]; then
    curl -fsSL -o "$TOOL" "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-$HOST.AppImage"
    chmod +x "$TOOL"
fi

rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share"
# xray and its databases next to duoray: that is where the app looks first.
cp -a "$ROOT/usr/lib/duoray/." "$APPDIR/usr/bin/"
install -m 755 "$ROOT/usr/libexec/duoray/duoray-helper" "$APPDIR/usr/bin/"
cp -a "$ROOT/usr/share/applications" "$ROOT/usr/share/icons" "$APPDIR/usr/share/"
install -Dm644 -t "$APPDIR/usr/share/doc/duoray" "$ROOT"/usr/share/licenses/duoray/*
install -m 644 packaging/linux/duoray.desktop "$APPDIR/duoray.desktop"
install -m 644 crates/duoray-gui/assets/duoray-256.png "$APPDIR/duoray.png"
ln -sf duoray.png "$APPDIR/.DirIcon"
cat > "$APPDIR/AppRun" <<'EOF'
#!/bin/sh
HERE=$(dirname "$(readlink -f "$0")")
exec "$HERE/usr/bin/duoray" "$@"
EOF
chmod 755 "$APPDIR/AppRun"

mkdir -p dist
rm -f "$OUT"
# Extract-and-run: building does not need FUSE.
ARCH=$ARCH APPIMAGE_EXTRACT_AND_RUN=1 "$TOOL" --no-appstream "$APPDIR" "$OUT"
ls -la "$OUT"
