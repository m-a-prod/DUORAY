#!/bin/sh
# Publishes the signed pacman repository for one version to the hub:
#   https://<hub>/arch/$arch/duoray.db (+ packages and signatures),
#   https://<hub>/arch/duoray.asc (the repository key).
# Takes dist/duoray-<v>-1-<arch>.pkg.tar.zst (packages.sh, or a CI release).
# The database is rebuilt each time, so it lists the newest package only.
#
# Usage: packaging/linux/arch-repo.sh <version>
# Needs: repo-add (pacman), gpg with the repository key in
# ~/.config/duoray-release/gnupg, DUORAY_HUB_SSH in ~/.config/duoray-release/env.
set -eu
cd "$(dirname "$0")/../.."
VERSION=${1:?version}
CONF=$HOME/.config/duoray-release/env
[ -f "$CONF" ] && . "$CONF"
HUB=${DUORAY_HUB_SSH:?set DUORAY_HUB_SSH in $CONF}
REMOTE=/var/lib/private/duoray-hub/updates/arch
export GNUPGHOME=${DUORAY_REPO_GNUPG:-$HOME/.config/duoray-release/gnupg}
REPO_KEY=${DUORAY_REPO_KEY:-13F077054DB026DC11D58144254C5F497BDAC7B9}
OUT=dist/arch-repo-$VERSION

set -- dist/duoray-"$VERSION"-1-*.pkg.tar.zst
[ -f "$1" ] || { echo "no Arch packages of $VERSION in dist/: pacman repository unchanged"; exit 0; }
command -v repo-add >/dev/null || { echo "repo-add (pacman) not found: pacman repository unchanged" >&2; exit 0; }

rm -rf "$OUT"
for pkg in "$@"; do
    name=$(basename "$pkg")
    arch=${name%.pkg.tar.zst}
    arch=${arch##*-}
    dir=$OUT/$arch
    mkdir -p "$dir"
    cp "$pkg" "$dir/"
    gpg --batch --yes --local-user "$REPO_KEY" --detach-sign "$dir/$name"
    (cd "$dir" && repo-add --quiet --sign --key "$REPO_KEY" duoray.db.tar.gz "$name")
    ssh "$HUB" "mkdir -p $REMOTE/$arch"
    # Packages and signatures before the database that lists them.
    scp -q "$dir/$name" "$dir/$name.sig" "$HUB:$REMOTE/$arch/"
    scp -q "$dir/duoray.db" "$dir/duoray.db.sig" "$dir/duoray.files" "$dir/duoray.files.sig" "$HUB:$REMOTE/$arch/"
    echo "pacman repository $arch: $name"
done
gpg --armor --export "$REPO_KEY" | ssh "$HUB" "cat > $REMOTE/duoray.asc && chmod -R a+rX $REMOTE"
