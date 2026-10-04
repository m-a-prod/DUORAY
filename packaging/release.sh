#!/bin/sh
# Publishes a DUORAY release to the update hub.
#
#   packaging/release.sh [--no-build | --from-github <tag>] ["Что нового: ..."]
#
# --from-github takes the builds from that GitHub release (made by
# .github/workflows/release.yml, all platforms) instead of building here.
#
# Builds what this machine can build (Windows x64/x86, Linux x86_64 AppImage),
# then publishes every build of this version found in dist/, including ones
# made elsewhere and copied in (macOS, aarch64), named like:
#   DUORAY-Setup-<v>-x64.exe | DUORAY-Setup-<v>-x86.exe
#   DUORAY-<v>-x86_64.AppImage | DUORAY-<v>-aarch64.AppImage
#   DUORAY-<v>-macos-aarch64.dmg | DUORAY-<v>-macos-x86_64.dmg
# The manifest is signed with the Ed25519 key that the app trusts
# (crates/duoray-gui/src/update.rs); the key never leaves this machine.
set -eu
cd "$(dirname "$0")/.."

BUILD=1
FROM_TAG=
if [ "${1:-}" = "--no-build" ]; then BUILD=0; shift; fi
if [ "${1:-}" = "--from-github" ]; then BUILD=0; FROM_TAG=${2:?tag}; shift 2; fi
NOTES=${1:-}
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' crates/duoray-gui/Cargo.toml | head -1)
BUILD_ID=$(git rev-parse --short=12 HEAD)
# The build id in the binaries must be a real commit: updates compare it.
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
    echo "uncommitted changes: commit first, the release build id is the commit" >&2
    exit 1
fi
# Local settings, kept out of the repository: DUORAY_HUB_SSH=<user@host of the hub>.
CONF=$HOME/.config/duoray-release/env
[ -f "$CONF" ] && . "$CONF"
KEY=${DUORAY_SIGNING_KEY:-$HOME/.config/duoray-release/update-signing.pem}
HUB=${DUORAY_HUB_SSH:?set DUORAY_HUB_SSH (user@host of the update hub) in $CONF}
REMOTE=/var/lib/private/duoray-hub/updates
[ -f "$KEY" ] || { echo "signing key not found: $KEY" >&2; exit 1; }

if [ -n "$FROM_TAG" ]; then
    [ "$FROM_TAG" = "v$VERSION" ] || { echo "tag $FROM_TAG does not match version $VERSION" >&2; exit 1; }
    [ "$(git rev-parse --short=12 "$FROM_TAG^{commit}")" = "$BUILD_ID" ] || {
        echo "check out $FROM_TAG first: the manifest names the commit the builds came from" >&2; exit 1; }
    mkdir -p dist
    gh release download "$FROM_TAG" -R m-a-prod/DUORAY -D dist --clobber
fi
if [ "$BUILD" = 1 ]; then
    packaging/windows/build.sh x64
    packaging/windows/build.sh x86
    packaging/linux/appimage.sh
    packaging/linux/packages.sh
fi

OUT=dist/release-$VERSION
rm -rf "$OUT" && mkdir -p "$OUT"
python3 - "$VERSION" "$NOTES" "$OUT" "$BUILD_ID" <<'EOF'
import hashlib, json, os, shutil, sys, time
version, notes, out, build = sys.argv[1:5]
names = {
    f"DUORAY-Setup-{version}-x64.exe": "windows-x86_64-setup",
    f"DUORAY-Setup-{version}-x86.exe": "windows-x86-setup",
    f"DUORAY-{version}-x86_64.AppImage": "linux-x86_64-appimage",
    f"DUORAY-{version}-aarch64.AppImage": "linux-aarch64-appimage",
    f"DUORAY-{version}-macos-aarch64.dmg": "macos-aarch64-dmg",
    f"DUORAY-{version}-macos-x86_64.dmg": "macos-x86_64-dmg",
}
assets = {}
for name, key in names.items():
    path = os.path.join("dist", name)
    if not os.path.exists(path):
        continue
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    assets[key] = {"file": name, "sha256": h.hexdigest(), "size": os.path.getsize(path)}
    shutil.copy(path, os.path.join(out, name))
if not assets:
    sys.exit(f"no builds of {version} in dist/")
manifest = {
    "version": version,
    "build": build,
    "notes": notes,
    "published": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    "page": os.environ.get("DUORAY_RELEASE_PAGE", ""),
    "assets": assets,
}
with open(os.path.join(out, "manifest.json"), "w") as f:
    json.dump(manifest, f, ensure_ascii=False, indent=2)
for key, a in assets.items():
    print(f"  {key:<24} {a['file']}  {a['size'] // 1024 // 1024} MB")
# Packages for package managers: plain downloads (AUR uses the tarballs),
# not in the manifest - those installs update through their package manager.
for name in sorted(os.listdir("dist")):
    if version in name and name.endswith((".rpm", ".deb", ".tar.gz")):
        shutil.copy(os.path.join("dist", name), os.path.join(out, name))
        print(f"  {'(download)':<24} {name}")
EOF

openssl pkeyutl -sign -inkey "$KEY" -rawin -in "$OUT/manifest.json" | xxd -p | tr -d '\n' > "$OUT/manifest.sig"
# Check the signature against the public key before anything is published.
xxd -r -p "$OUT/manifest.sig" > "$OUT/.sig.bin"
openssl pkey -in "$KEY" -pubout > "$OUT/.pub.pem"
openssl pkeyutl -verify -pubin -inkey "$OUT/.pub.pem" -rawin -in "$OUT/manifest.json" -sigfile "$OUT/.sig.bin" >/dev/null
rm -f "$OUT/.sig.bin" "$OUT/.pub.pem"

# Files first, the manifest last: clients never see a manifest whose files
# are still uploading.
ssh "$HUB" "mkdir -p $REMOTE/files"
for f in "$OUT"/DUORAY-* "$OUT"/duoray*; do
    [ -f "$f" ] && scp -q "$f" "$HUB:$REMOTE/files/"
done
scp -q "$OUT/manifest.json" "$HUB:$REMOTE/manifest.json.new"
scp -q "$OUT/manifest.sig" "$HUB:$REMOTE/manifest.sig.new"
ssh "$HUB" "cd $REMOTE && chmod 644 files/* manifest.*.new && mv manifest.sig.new manifest.sig && mv manifest.json.new manifest.json"
echo "published $VERSION ($BUILD_ID)"
