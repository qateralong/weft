#!/usr/bin/env bash
# Runs loom, a weftd with a real TUN interface and an echo-mode weftd on one machine,
# then pings the echo peer through Weft. Works on Linux, macOS and Windows (Git Bash).
#   BIN: directory with the binaries (default target/debug)
#   SUDO: prefix for the TUN daemon and its CLI calls, for example "sudo"
set -Eeuo pipefail

BIN=$(cd "${BIN:-target/debug}" && pwd)
SUDO=${SUDO:-}
case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN*) windows=1 exe=.exe ;;
    *) windows=0 exe= ;;
esac
WORK=$(mktemp -d)
native() { if [ "$windows" = 1 ]; then cygpath -w "$1"; else echo "$1"; fi; }
if [ "$windows" = 1 ]; then
    sock_a='\\.\pipe\weft-e2e-a'
    sock_b='\\.\pipe\weft-e2e-b'
else
    sock_a=$WORK/a.sock
    sock_b=$WORK/b.sock
fi

pids=()
show_logs() { for log in loom a b; do echo "--- $log.log"; cat "$WORK/$log.log" 2>/dev/null || true; done; }
cleanup() {
    for pid in "${pids[@]}"; do $SUDO kill "$pid" 2>/dev/null || true; done
    if [ "$windows" = 1 ]; then taskkill //F //IM weftd.exe > /dev/null 2>&1 || true; taskkill //F //IM loom.exe > /dev/null 2>&1 || true; fi
    wait 2>/dev/null || true
}
trap cleanup EXIT
trap show_logs ERR

cat > "$WORK/loom.toml" <<CONF
listen = "127.0.0.1:7443"
data_dir = '$(native "$WORK/loom")'
CONF
LINK=$("$BIN/loom$exe" --config "$(native "$WORK/loom.toml")" link --host 127.0.0.1 | tr -d '\r')
"$BIN/loom$exe" --config "$(native "$WORK/loom.toml")" > "$WORK/loom.log" 2>&1 &
pids+=($!)
$SUDO "$BIN/weftd$exe" --state-dir "$(native "$WORK/a")" --socket "$sock_a" > "$WORK/a.log" 2>&1 &
pids+=($!)
"$BIN/weftd$exe" --state-dir "$(native "$WORK/b")" --socket "$sock_b" --echo > "$WORK/b.log" 2>&1 &
pids+=($!)
sleep 3

weft_a() { $SUDO env LANG=C LC_ALL=C WEFT_SOCKET="$sock_a" "$BIN/weft$exe" "$@" | tr -d '\r'; }
weft_b() { env LANG=C LC_ALL=C WEFT_SOCKET="$sock_b" "$BIN/weft$exe" "$@" | tr -d '\r'; }

weft_a up "$LINK" --nickname alice
weft_b up "$LINK" --nickname bob
weft_a create e2e --password secret
weft_b join e2e --password secret
for _ in $(seq 30); do
    weft_a status | grep -Eq 'bob .*online, (direct|via the server)' && break
    sleep 1
done
weft_a status
addr_b=$(weft_b status | awk -F': *' '/^Address/ {print $2}')

echo "--- ping $addr_b"
if [ "$windows" = 1 ]; then
    ping -n 3 -w 2000 "$addr_b" | tee "$WORK/ping.txt"
    grep -q "TTL=" "$WORK/ping.txt"
else
    ping -c 3 "$addr_b"
fi
echo "--- resolve bob.weft through the system resolver"
resolve() {
    case "$(uname -s)" in
        Linux) getent hosts bob.weft | awk '{print $1}' ;;
        Darwin) dscacheutil -q host -a name bob.weft | awk '/^ip_address/ {print $2}' ;;
        *) powershell.exe -NoProfile -Command "(Resolve-DnsName bob.weft -Type A -DnsOnly -ErrorAction SilentlyContinue).IPAddress" | tr -d '\r' ;;
    esac
}
for _ in $(seq 20); do
    [ "$(resolve | head -1)" = "$addr_b" ] && break
    sleep 1
done
weft_a netcheck | grep '^Peer names'
[ "$(resolve | head -1)" = "$addr_b" ]
echo "--- e2e passed"
if [ -n "${SHOW_LOGS:-}" ]; then
    show_logs
fi
