#!/bin/sh
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload
    systemctl try-restart weftd.service || true
fi
exit 0
