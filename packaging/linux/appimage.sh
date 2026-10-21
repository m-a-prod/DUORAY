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
    curl -fsSL --retry 3 --retry-delay 5 --connect-timeout 20 --max-time 600 -o "$TOOL" "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-$HOST.AppImage"
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

# winit loads libxkbcommon-x11 at run time and panicked at start where the
# system has none ("Library libxkbcommon-x11.so could not be loaded", the
# AppImage catalog test). Bundle it with libxcb-xkb, which it needs; AppRun
# uses them only when the system lacks its own, so a working system keeps
# its libraries. Built natively per arch, so the host's copies match $ARCH.
mkdir -p "$APPDIR/usr/lib/fallback"
# ldconfig tags each library with its ABI; skip 32-bit and foreign copies.
case $ARCH in x86_64) ABI=x86-64 ;; aarch64) ABI=AArch64 ;; *) ABI=$ARCH ;; esac
for lib in libxkbcommon-x11.so.0 libxcb-xkb.so.1; do
    src=$(/sbin/ldconfig -p | awk -v l="$lib" -v a="$ABI" '$1 == l && index($0, a) { print $NF; exit }')
    [ -n "$src" ] || { echo "appimage.sh: $lib not found on the build machine" >&2; exit 1; }
    cp -L "$src" "$APPDIR/usr/lib/fallback/$lib"
done

cat > "$APPDIR/AppRun" <<'EOF'
#!/bin/sh
HERE=$(dirname "$(readlink -f "$0")")
# Bundled X11 keyboard libraries, only where the system has none (appimage.sh).
LDCONFIG=$(command -v ldconfig || echo /sbin/ldconfig)
if ! "$LDCONFIG" -p 2>/dev/null | grep -q 'libxkbcommon-x11\.so\.0 '; then
    export LD_LIBRARY_PATH="$HERE/usr/lib/fallback${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
fi
exec "$HERE/usr/bin/duoray" "$@"
EOF
chmod 755 "$APPDIR/AppRun"

mkdir -p dist
rm -f "$OUT"
# Extract-and-run: building does not need FUSE.
ARCH=$ARCH APPIMAGE_EXTRACT_AND_RUN=1 "$TOOL" --no-appstream "$APPDIR" "$OUT"
ls -la "$OUT"
