#!/bin/sh
# Installs DUORAY system-wide on Linux:
#   /usr/lib/duoray/{duoray,xray,geoip.dat,geosite.dat}, /usr/bin/duoray,
#   /usr/libexec/duoray/duoray-helper (+ its systemd/OpenRC/runit service),
#   desktop entry and icon.
# Usage: packaging/linux/install.sh [--no-build]
# Run as your normal user: it builds with cargo, then asks for sudo once.
set -eu
cd "$(dirname "$0")/../.."
ROOT=$(pwd)

# The newest stable xray-core (XRAY_VERSION=vX.Y.Z pins one), from GitHub's
# releases/latest redirect.
XRAY_VERSION=${XRAY_VERSION:-$(curl -fsSI --retry 3 --connect-timeout 20 https://github.com/XTLS/Xray-core/releases/latest 2>/dev/null |
    sed -n 's|^[Ll]ocation: .*/tag/\([^[:space:]]*\).*|\1|p')}
LIB=/usr/lib/duoray
LIBEXEC=/usr/libexec/duoray

if [ "${1:-}" != "--root-stage" ]; then
    if [ "$(id -u)" -eq 0 ]; then
        echo "Запустите от своего пользователя (не root): скрипт сам попросит sudo." >&2
        exit 1
    fi
    if [ "${1:-}" != "--no-build" ]; then
        command -v cargo >/dev/null 2>&1 || { echo "Нужен Rust: https://rustup.rs" >&2; exit 1; }
        cargo build --release -p duoray-gui -p duoray-helper
    fi
    for f in target/release/duoray target/release/duoray-helper; do
        [ -x "$f" ] || { echo "Нет $f: соберите без --no-build" >&2; exit 1; }
    done
    # sudo sets SUDO_UID: the helper will serve this user.
    exec sudo sh "$ROOT/packaging/linux/install.sh" --root-stage
fi

# ── root stage ──────────────────────────────────────────────────────────────
[ "$(id -u)" -eq 0 ] || { echo "root stage needs root" >&2; exit 1; }

install -d -m 755 "$LIB" "$LIBEXEC"
# The helper may be running: replace the file, do not write into it.
install -m 755 target/release/duoray-helper "$LIBEXEC/duoray-helper.new"
mv -f "$LIBEXEC/duoray-helper.new" "$LIBEXEC/duoray-helper"
install -m 755 target/release/duoray "$LIB/duoray.new"
mv -f "$LIB/duoray.new" "$LIB/duoray"
ln -sfn "$LIB/duoray" /usr/bin/duoray
command -v restorecon >/dev/null 2>&1 && restorecon -RF "$LIB" "$LIBEXEC" || true

# xray: the official build, checked against its published SHA-256.
if [ ! -x "$LIB/xray" ] || ! "$LIB/xray" version 2>/dev/null | grep -q "Xray ${XRAY_VERSION#v} "; then
    case "$(uname -m)" in
        x86_64) ZIP=Xray-linux-64.zip ;;
        aarch64|arm64) ZIP=Xray-linux-arm64-v8a.zip ;;
        *) ZIP= ;;
    esac
    if [ -n "$ZIP" ]; then
        tmp=$(mktemp -d)
        trap 'rm -rf "$tmp"' EXIT
        echo "Скачиваю xray $XRAY_VERSION…"
        SHA=$(curl -fsSL --retry 3 --connect-timeout 20 "https://github.com/XTLS/Xray-core/releases/download/$XRAY_VERSION/$ZIP.dgst" | sed -n 's/^SHA2-256= *//p')
        curl -fsSL --retry 3 --retry-delay 5 --connect-timeout 20 --max-time 600 -o "$tmp/xray.zip" "https://github.com/XTLS/Xray-core/releases/download/$XRAY_VERSION/$ZIP"
        echo "$SHA  $tmp/xray.zip" | sha256sum -c - >/dev/null
        if command -v unzip >/dev/null 2>&1; then
            unzip -o -q "$tmp/xray.zip" xray geoip.dat geosite.dat LICENSE -d "$tmp/x"
        else
            python3 -c "import sys,zipfile; zipfile.ZipFile(sys.argv[1]).extractall(sys.argv[2])" "$tmp/xray.zip" "$tmp/x"
        fi
        install -m 755 "$tmp/x/xray" "$LIB/xray"
        install -m 644 "$tmp/x/geoip.dat" "$tmp/x/geosite.dat" "$LIB/"
        install -m 644 "$tmp/x/LICENSE" "$LIB/LICENSE-xray.txt"
    elif ! command -v xray >/dev/null 2>&1; then
        echo "Нет готового xray для $(uname -m): установите xray из репозитория дистрибутива." >&2
    fi
fi

install -Dm644 packaging/linux/duoray.desktop /usr/share/applications/duoray.desktop
install -Dm644 crates/duoray-gui/assets/duoray-256.png /usr/share/icons/hicolor/256x256/apps/duoray.png
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database -q /usr/share/applications || true
command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q -t /usr/share/icons/hicolor || true

# The helper service, for the user who ran the script: connecting then needs no password.
SUDO_UID="${SUDO_UID:?}" "$LIB/duoray" --install-helper

echo "Готово: DUORAY в меню приложений или команда duoray."
