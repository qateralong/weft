# Installs or updates the Weft server (loom) on a Linux machine with systemd.
# Runs as root; $1 is the address clients use, $2 overrides where the binary comes from. Prints "STEP name" while working,
# "ERROR code" on failure, and at the end "LINK weft://...", "PANEL https://..." and "PANELPASS password" (a new one each time).
set -eu
HOST="$1"
step() { echo "STEP $1"; }
fail() { echo "ERROR $1"; exit 3; }

step check
[ "$(uname -s)" = Linux ] || fail unsupported-os
case "$(uname -m)" in
    x86_64 | amd64) target=x86_64-unknown-linux-musl ;;
    aarch64 | arm64) target=aarch64-unknown-linux-musl ;;
    *) fail unsupported-arch ;;
esac
command -v systemctl > /dev/null || fail no-systemd

step download
url="${2:-https://github.com/qateralong/weft/releases/latest/download/loom-$target}"
tmp=$(mktemp)
if command -v curl > /dev/null; then
    curl -fsSL --retry 3 "$url" -o "$tmp" || fail download
elif command -v wget > /dev/null; then
    wget -qO "$tmp" "$url" || fail download
else
    fail no-downloader
fi
install -m 755 "$tmp" /usr/local/bin/loom
rm -f "$tmp"

step configure
id loom > /dev/null 2>&1 || useradd --system --home-dir /var/lib/loom --shell /usr/sbin/nologin loom
mkdir -p /etc/loom
if [ ! -f /etc/loom/loom.toml ]; then
    busy() { ss -Hltnu 2> /dev/null | awk '{print $5}' | grep -qE "[:.]$1\$"; }
    port=""
    for candidate in 443 8443 7443 9443 10443; do
        if ! busy "$candidate"; then
            port=$candidate
            break
        fi
    done
    [ -n "$port" ] || fail no-port
    cat > /etc/loom/loom.toml << EOF
# Weft server, installed by the Weft app. Restart after changes: systemctl restart loom
listen = "0.0.0.0:$port"
public_host = "$HOST"
data_dir = "/var/lib/loom"
EOF
fi
port=$(sed -n 's/^listen = ".*:\([0-9]*\)"/\1/p' /etc/loom/loom.toml)
cat > /etc/systemd/system/loom.service << 'EOF'
[Unit]
Description=Weft coordination server
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/loom --config /etc/loom/loom.toml
User=loom
Group=loom
StateDirectory=loom
StateDirectoryMode=0700
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
Restart=on-failure

[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload
systemctl enable loom > /dev/null 2>&1
systemctl restart loom

step firewall
if command -v ufw > /dev/null && ufw status 2> /dev/null | grep -q "Status: active"; then
    ufw allow "$port/tcp" > /dev/null
    ufw allow "$port/udp" > /dev/null
fi
if command -v firewall-cmd > /dev/null && firewall-cmd --state > /dev/null 2>&1; then
    firewall-cmd --permanent --add-port="$port/tcp" --add-port="$port/udp" > /dev/null
    firewall-cmd --reload > /dev/null
fi

step start
for _ in $(seq 30); do
    if systemctl is-active --quiet loom && ss -Hltn | awk '{print $4}' | grep -qE "[:.]$port\$"; then
        break
    fi
    sleep 1
done
if ! systemctl is-active --quiet loom; then
    journalctl -u loom -n 20 --no-pager || true
    fail not-started
fi
loom_cmd() { runuser -u loom -- /usr/local/bin/loom --config /etc/loom/loom.toml "$@"; }
echo "LINK $(loom_cmd link)"
if password=$(loom_cmd panel password --generate 2> /dev/null); then
    echo "PANEL $(loom_cmd panel url)"
    echo "PANELPASS $password"
fi
