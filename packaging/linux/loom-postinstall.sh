#!/bin/sh
if command -v systemd-sysusers > /dev/null 2>&1; then
    systemd-sysusers loom.conf || true
elif ! getent passwd loom > /dev/null; then
    useradd --system --home-dir /var/lib/loom --shell /usr/sbin/nologin loom || true
fi
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload
    systemctl try-restart loom.service || true
fi
echo "Edit /etc/loom/loom.toml, then run: systemctl enable --now loom"
exit 0
