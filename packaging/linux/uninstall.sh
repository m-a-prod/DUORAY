#!/bin/sh
# Removes what packaging/linux/install.sh installed. User data
# (~/.local/share/duoray, ~/.config/duoray) is kept.
set -eu
if [ "$(id -u)" -ne 0 ]; then
    exec sudo sh "$0" "$@"
fi
if [ -x /usr/lib/duoray/duoray ]; then
    /usr/lib/duoray/duoray --uninstall-helper || true
fi
rm -rf /usr/lib/duoray /usr/libexec/duoray
rm -f /usr/bin/duoray /usr/share/applications/duoray.desktop /usr/share/icons/hicolor/256x256/apps/duoray.png
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database -q /usr/share/applications || true
echo "DUORAY удалён."
