#!/bin/sh
case "$1" in
    remove | purge | 0)
        if [ -d /run/systemd/system ]; then
            systemctl disable --now weftd.service || true
        fi
        ;;
esac
exit 0
