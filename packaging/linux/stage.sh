#!/bin/sh
# Builds DUORAY for Linux and lays it out as installed (the same paths as
# install.sh), under packaging/linux/root-<arch>/. The packages (rpm, deb, the
# tarball AUR uses) are made from this tree by packages.sh.
#
# Usage: packaging/linux/stage.sh [x86_64|aarch64]   (default: this machine)
# Needs: rustup target for <arch>-unknown-linux-gnu, zig, cargo-zigbuild, curl.
set -eu
cd "$(dirname "$0")/../.."

ARCH=${1:-$(uname -m)}
XRAY_VERSION=v26.3.27
case "$ARCH" in
    x86_64) XRAY_ZIP=Xray-linux-64.zip; XRAY_SHA256=23cd9af937744d97776ee35ecad4972cf4b2109d1e0fe6be9930467608f7c8ae ;;
    aarch64|arm64) ARCH=aarch64; XRAY_ZIP=Xray-linux-arm64-v8a.zip; XRAY_SHA256=4d30283ae614e3057f730f67cd088a42be6fdf91f8639d82cb69e48cde80413c ;;
    *) echo "unsupported arch: $ARCH (x86_64|aarch64)" >&2; exit 1 ;;
esac
# Old glibc on purpose: binaries built against it run on any newer one.
TARGET=$ARCH-unknown-linux-gnu
GLIBC=2.28
CACHE=packaging/linux/cache
ROOT=packaging/linux/root-$ARCH

cargo zigbuild --release --target "$TARGET.$GLIBC" -p duoray-gui -p duoray-helper
BIN=target/$TARGET/release

mkdir -p "$CACHE"
if [ ! -f "$CACHE/$XRAY_ZIP" ] || ! echo "$XRAY_SHA256  $CACHE/$XRAY_ZIP" | sha256sum -c - >/dev/null 2>&1; then
    curl -fsSL -o "$CACHE/$XRAY_ZIP" "https://github.com/XTLS/Xray-core/releases/download/$XRAY_VERSION/$XRAY_ZIP"
    echo "$XRAY_SHA256  $CACHE/$XRAY_ZIP" | sha256sum -c -
fi
rm -rf "$CACHE/xray-$ARCH"
python3 -c "import sys,zipfile; zipfile.ZipFile(sys.argv[1]).extractall(sys.argv[2])" "$CACHE/$XRAY_ZIP" "$CACHE/xray-$ARCH"

rm -rf "$ROOT"
install -Dm755 "$BIN/duoray" "$ROOT/usr/lib/duoray/duoray"
install -Dm755 "$BIN/duoray-helper" "$ROOT/usr/libexec/duoray/duoray-helper"
# xray and its databases next to duoray: that is where the app looks first.
install -Dm755 "$CACHE/xray-$ARCH/xray" "$ROOT/usr/lib/duoray/xray"
install -Dm644 -t "$ROOT/usr/lib/duoray" "$CACHE/xray-$ARCH/geoip.dat" "$CACHE/xray-$ARCH/geosite.dat"
install -Dm644 "$CACHE/xray-$ARCH/LICENSE" "$ROOT/usr/share/licenses/duoray/LICENSE-xray.txt"
install -d "$ROOT/usr/bin"
ln -sfn ../lib/duoray/duoray "$ROOT/usr/bin/duoray"
install -Dm644 packaging/linux/duoray.desktop "$ROOT/usr/share/applications/duoray.desktop"
install -Dm644 crates/duoray-gui/assets/duoray-256.png "$ROOT/usr/share/icons/hicolor/256x256/apps/duoray.png"
install -Dm644 -t "$ROOT/usr/share/licenses/duoray" LICENSE LICENSE-EXCEPTION.md THIRD-PARTY-NOTICES.md THIRD-PARTY-CRATES.txt
echo "staged $ROOT"
