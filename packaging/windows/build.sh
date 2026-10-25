#!/bin/sh
# Builds dist/DUORAY-Setup-<version>-<arch>.exe on macOS/Linux.
# Usage: build.sh [x64|x86]   (default x64)
# Needs: rustup targets x86_64/i686-pc-windows-msvc, cargo-xwin, llvm-tools
# (llvm-lib/lld-link links in ~/.local/duoray-xtools), makensis.
set -eu
cd "$(dirname "$0")/../.."
ARCH=${1:-x64}
# The newest stable xray-core (wintun and the geo databases come in its
# archive); XRAY_VERSION=vX.Y.Z pins one.
. packaging/latest.sh
XRAY_VERSION=${XRAY_VERSION:-$(latest_tag XTLS/Xray-core)}
case "$ARCH" in
    x64) TARGET=x86_64-pc-windows-msvc; XRAY_ZIP=Xray-windows-64.zip
         ;;
    x86) TARGET=i686-pc-windows-msvc; XRAY_ZIP=Xray-windows-32.zip
         ;;
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
# A stage left by an earlier build may hold an older xray.
if [ ! -f "$stage/xray.exe" ] || [ "$(cat "$stage/xray.version" 2>/dev/null)" != "$XRAY_VERSION" ]; then
    XRAY_SHA256=$(xray_sha256 "$XRAY_VERSION" "$XRAY_ZIP")
    tmp=$(mktemp -d)
    curl -fsSL --retry 3 --retry-delay 5 --connect-timeout 20 --max-time 600 -o "$tmp/xray.zip" "https://github.com/XTLS/Xray-core/releases/download/$XRAY_VERSION/$XRAY_ZIP"
    echo "$XRAY_SHA256  $tmp/xray.zip" | shasum -a 256 -c -
    # wintun's license was LICENSE-wintun.txt, newer archives call it LICENSE-Wintun.
    unzip -o -q "$tmp/xray.zip" xray.exe geoip.dat geosite.dat wintun.dll LICENSE 'LICENSE-[Ww]intun*' -d "$stage"
    mv "$stage/LICENSE" "$stage/LICENSE-xray.txt"
    for f in "$stage"/LICENSE-[Ww]intun*; do
        [ "$f" = "$stage/LICENSE-wintun.txt" ] || mv "$f" "$stage/LICENSE-wintun.txt"
    done
    echo "$XRAY_VERSION" > "$stage/xray.version"
    rm -rf "$tmp"
fi
cp "target/$TARGET/release/duoray.exe" "target/$TARGET/release/duoray-helper.exe" "$stage/"
cp LICENSE "$stage/LICENSE.txt"
cp LICENSE-EXCEPTION.md THIRD-PARTY-NOTICES.md THIRD-PARTY-CRATES.txt "$stage/"
cp crates/duoray-gui/assets/fonts/OFL-NotoColorEmoji.txt "$stage/"
(cd packaging/windows && makensis -V2 -DVERSION="$VERSION" -DARCH="$ARCH" duoray.nsi)
ls -la dist/
