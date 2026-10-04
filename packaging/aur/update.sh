#!/bin/sh
# Fills packaging/aur/duoray-bin/ for a version whose tarballs are on the hub
# (packaging/release.sh puts them there), writes .SRCINFO and, with --push,
# commits and pushes to the AUR (needs an AUR account with your SSH key).
#
#   packaging/aur/update.sh [--push] [<version>]
set -eu
cd "$(dirname "$0")/../.."
PUSH=0
if [ "${1:-}" = "--push" ]; then PUSH=1; shift; fi
VERSION=${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' crates/duoray-gui/Cargo.toml | head -1)}
# DUORAY_FILES overrides where the tarballs come from (file://… for tests).
HUB=${DUORAY_FILES:-https://duoray.dualizm.space/v1/update/files}
OUT=packaging/aur/build
rm -rf "$OUT" && mkdir -p "$OUT"

sum() {
    curl -fsSL -o "$OUT/DUORAY-$VERSION-linux-$1.tar.gz" "$HUB/DUORAY-$VERSION-linux-$1.tar.gz" &&
        sha256sum "$OUT/DUORAY-$VERSION-linux-$1.tar.gz" | cut -d' ' -f1
}
SHA_X86_64=$(sum x86_64)
# aarch64 builds come from CI; without one the package stays x86_64-only.
SHA_AARCH64=$(sum aarch64 2>/dev/null || true)
sed -e "s/@VERSION@/$VERSION/" -e "s/@SHA_X86_64@/$SHA_X86_64/" -e "s/@SHA_AARCH64@/$SHA_AARCH64/" \
    packaging/aur/duoray-bin/PKGBUILD > "$OUT/PKGBUILD"
if [ -z "$SHA_AARCH64" ]; then
    sed -i -e "s/^arch=.*/arch=('x86_64')/" -e '/aarch64/d' "$OUT/PKGBUILD"
fi
cp packaging/aur/duoray-bin/duoray.install "$OUT/"
(cd "$OUT" && makepkg --printsrcinfo > .SRCINFO)
echo "AUR package for $VERSION in $OUT"

if [ "$PUSH" = 1 ]; then
    AUR=packaging/aur/repo
    [ -d "$AUR/.git" ] || git clone ssh://aur@aur.archlinux.org/duoray-bin.git "$AUR"
    cp "$OUT/PKGBUILD" "$OUT/duoray.install" "$OUT/.SRCINFO" "$AUR/"
    (cd "$AUR" && git add PKGBUILD duoray.install .SRCINFO && git commit -m "Update to $VERSION" && git push)
fi
