#!/bin/sh
# rpm passes the number of versions left (0: removal), deb "remove"/"purge"
# ("upgrade" otherwise). On removal take the helper service down too: the app
# created it, the package does not track it.
case "${1:-}" in
    0|remove|purge) /usr/lib/duoray/duoray --uninstall-helper >/dev/null 2>&1 || true ;;
esac
exit 0
