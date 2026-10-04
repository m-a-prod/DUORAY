#!/bin/sh
# rpm / deb post-install: menu and icon caches; after an upgrade, restart the
# helper so it runs the new binary. The app installs the helper service on
# first connect (polkit asks for the password once).
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database -q /usr/share/applications || true
command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q -t /usr/share/icons/hicolor || true
if [ -d /run/systemd/system ]; then
    systemctl try-restart duoray-helper.service >/dev/null 2>&1 || true
fi
exit 0
