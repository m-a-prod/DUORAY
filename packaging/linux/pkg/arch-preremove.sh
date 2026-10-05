#!/bin/sh
# pacman pre_remove runs only on removal (upgrades use pre_upgrade): take the
# helper service down too, the app created it.
/usr/lib/duoray/duoray --uninstall-helper >/dev/null 2>&1 || true
exit 0
