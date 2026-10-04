#!/bin/sh
# Builds the Linux packages for one architecture into dist/:
#   duoray-<v>-1.<arch>.rpm, duoray_<v>-1_<debarch>.deb,
#   DUORAY-<v>-linux-<arch>.tar.gz (the installed tree; the AUR package uses it).
# Usage: packaging/linux/packages.sh [x86_64|aarch64]
set -eu
cd "$(dirname "$0")/../.."

ARCH=${1:-$(uname -m)}
[ "$ARCH" = arm64 ] && ARCH=aarch64
case "$ARCH" in
    x86_64) PKG_ARCH=amd64 ;;
    aarch64) PKG_ARCH=arm64 ;;
    *) echo "unsupported arch: $ARCH" >&2; exit 1 ;;
esac
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' crates/duoray-gui/Cargo.toml | head -1)
CACHE=packaging/linux/cache

packaging/linux/stage.sh "$ARCH"
ROOT=$(pwd)/packaging/linux/root-$ARCH

# nfpm: pinned release, checked against its published SHA-256.
NFPM_VERSION=2.47.0
case "$(uname -m)" in
    x86_64) NFPM_TAR=nfpm_${NFPM_VERSION}_Linux_x86_64.tar.gz; NFPM_SHA=0660ca602b2d2d2ae4781a06c692b3eeb9d437ffea05b831d76e41f4a3188783 ;;
    aarch64|arm64) NFPM_TAR=nfpm_${NFPM_VERSION}_Linux_arm64.tar.gz; NFPM_SHA=1c0f5f2999b9a974bfb04fdb0cc3306096de530ac5dbb25d739cc5f5219c919c ;;
esac
NFPM=$CACHE/nfpm-$NFPM_VERSION/nfpm
if [ ! -x "$NFPM" ]; then
    mkdir -p "$CACHE/nfpm-$NFPM_VERSION"
    curl -fsSL -o "$CACHE/$NFPM_TAR" "https://github.com/goreleaser/nfpm/releases/download/v$NFPM_VERSION/$NFPM_TAR"
    echo "$NFPM_SHA  $CACHE/$NFPM_TAR" | sha256sum -c -
    tar -xzf "$CACHE/$NFPM_TAR" -C "$CACHE/nfpm-$NFPM_VERSION" nfpm
fi

mkdir -p dist
# nfpm expands variables only in some fields, so fill the paths in here.
CONFIG=$CACHE/nfpm-$ARCH.yaml
sed -e "s|\${ROOT}|$ROOT|g" -e "s|\${VERSION}|$VERSION|g" -e "s|\${PKG_ARCH}|$PKG_ARCH|g" \
    packaging/linux/pkg/nfpm.yaml > "$CONFIG"
for fmt in rpm deb; do
    "$NFPM" package --config "$CONFIG" --packager "$fmt" --target dist/
done
tar -C "$ROOT" --owner=0 --group=0 -czf "dist/DUORAY-$VERSION-linux-$ARCH.tar.gz" usr
ls -la dist/ | grep -E "\.rpm|\.deb|linux-$ARCH\.tar\.gz"
