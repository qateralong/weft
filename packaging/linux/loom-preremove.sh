#!/bin/sh
case "$1" in
    # dpkg, rpm, and pacman, which passes the version.
    remove | purge | 0 | [0-9]*-[0-9]*)
        if [ -d /run/systemd/system ]; then
            systemctl disable --now loom.service || true
        fi
        ;;
esac
exit 0
