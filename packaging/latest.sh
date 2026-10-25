# Sourced by the build scripts: the newest stable releases of what we bundle
# or build with, and the checksums their authors publish next to them.
# The checksums come from the same release page as the archives: they catch
# a broken download, not a compromised release.

# latest_tag OWNER/REPO: the tag of the newest stable (non-pre-release)
# release, from GitHub's releases/latest redirect (no API rate limit).
latest_tag() {
    tag=$(curl -fsSI --retry 3 --retry-delay 5 --connect-timeout 20 "https://github.com/$1/releases/latest" |
        sed -n 's|^[Ll]ocation: .*/tag/\([^[:space:]]*\).*|\1|p')
    [ -n "$tag" ] || { echo "cannot find the latest release of $1" >&2; return 1; }
    echo "$tag"
}

# xray_sha256 TAG ZIP: SHA-256 of an Xray-core archive, from its .dgst file.
xray_sha256() {
    sha=$(curl -fsSL --retry 3 --retry-delay 5 --connect-timeout 20 "https://github.com/XTLS/Xray-core/releases/download/$1/$2.dgst" |
        sed -n 's/^SHA2-256= *//p')
    [ -n "$sha" ] || { echo "no SHA-256 published for xray $1 $2" >&2; return 1; }
    echo "$sha"
}
