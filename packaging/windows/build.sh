#!/bin/sh
# Builds dist/DUORAY-Setup-<version>-<arch>.exe on macOS/Linux.
# Usage: build.sh [x64|x86]   (default x64)
# Needs: rustup targets x86_64/i686-pc-windows-msvc, cargo-xwin, llvm-tools
# (llvm-lib/lld-link links in ~/.local/duoray-xtools), makensis.
set -eu
cd "$(dirname "$0")/../.."
ARCH=${1:-x64}
XRAY_VERSION=v26.3.27
case "$ARCH" in
    x64) TARGET=x86_64-pc-windows-msvc; XRAY_ZIP=Xray-windows-64.zip
         XRAY_SHA256=d004c39288ce9ada487c6f398c7c545f7d749e44bdfdd59dbc9f865afba4e1ad ;;
    x86) TARGET=i686-pc-windows-msvc; XRAY_ZIP=Xray-windows-32.zip
         XRAY_SHA256=956a5ec00bce747c7936dc4ff7ac570df1c8030b0a4a8640f843488365084db3 ;;
    *) echo "unknown arch $ARCH (x64|x86)" >&2; exit 1 ;;
esac
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' crates/duoray-gui/Cargo.toml | head -1)
export PATH="$HOME/.local/duoray-xtools:$HOME/.cargo/bin:$PATH"
# The MSVC CRT/SDK cache holds one architecture; keep one cache per arch.
case "$ARCH" in
    x64) export XWIN_ARCH=x86_64 ;;
    x86) export XWIN_ARCH=x86; export XWIN_CACHE_DIR="$HOME/Library/Caches/cargo-xwin-x86" ;;
esac

cargo xwin build --release --target "$TARGET" -p duoray-gui -p duoray-helper

stage=packaging/windows/stage-$ARCH
mkdir -p "$stage" dist
if [ ! -f "$stage/xray.exe" ]; then
    tmp=$(mktemp -d)
    curl -sSL -o "$tmp/xray.zip" "https://github.com/XTLS/Xray-core/releases/download/$XRAY_VERSION/$XRAY_ZIP"
    echo "$XRAY_SHA256  $tmp/xray.zip" | shasum -a 256 -c -
    unzip -o -q "$tmp/xray.zip" xray.exe geoip.dat geosite.dat wintun.dll LICENSE LICENSE-wintun.txt -d "$stage"
    mv "$stage/LICENSE" "$stage/LICENSE-xray.txt"
    rm -rf "$tmp"
fi
cp "target/$TARGET/release/duoray.exe" "target/$TARGET/release/duoray-helper.exe" "$stage/"
cp LICENSE "$stage/LICENSE.txt"
cp LICENSE-EXCEPTION.md THIRD-PARTY-NOTICES.md THIRD-PARTY-CRATES.txt "$stage/"
(cd packaging/windows && makensis -V2 -DVERSION="$VERSION" -DARCH="$ARCH" duoray.nsi)
ls -la dist/
