#!/bin/sh
if command -v systemd-sysusers > /dev/null 2>&1; then
    systemd-sysusers weft.conf || true
elif ! getent group weft > /dev/null; then
    groupadd --system weft || true
fi
if [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != root ]; then
    usermod -aG weft "$SUDO_USER" || true
fi
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload
    systemctl enable weftd.service
    systemctl restart weftd.service || true
fi
exit 0
