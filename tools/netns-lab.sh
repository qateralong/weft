#!/usr/bin/env bash
# Runs loom and two weftd instances in isolated network namespaces (no root needed)
# and checks that the peers can ping each other over the virtual network.
set -Eeuo pipefail

BIN=$(realpath "${BIN:-target/debug}")
SELF=$(realpath "$0")

if [ "${WEFT_LAB_INNER:-}" != 1 ]; then
    WORK=$(mktemp -d)
    trap 'rm -rf "$WORK"' EXIT
    WEFT_LAB_INNER=1 WORK="$WORK" BIN="$BIN" unshare --user --map-root-user --net --mount "$SELF" "$@"
    exit
fi

mount -t tmpfs tmpfs /run
mkdir -p /run/netns
ip link set lo up
ip link add br0 type bridge
ip addr add 10.99.0.1/24 dev br0
ip link set br0 up

i=2
for n in a b; do
    ip netns add "$n"
    ip link add "veth-$n" type veth peer name eth0 netns "$n"
    ip link set "veth-$n" master br0 up
    ip -n "$n" addr add "10.99.0.$i/24" dev eth0
    ip -n "$n" link set eth0 up
    ip -n "$n" link set lo up
    i=$((i + 1))
done

pids=()
show_logs() { for log in loom a b; do echo "--- $log.log"; cat "$WORK/$log.log" 2>/dev/null || true; done; }
cleanup() { kill "${pids[@]}" 2>/dev/null || true; wait 2>/dev/null || true; }
trap cleanup EXIT
trap show_logs ERR

cat > "$WORK/loom.toml" <<CONF
listen = "10.99.0.1:7443"
data_dir = "$WORK/loom"
CONF
LINK=$("$BIN/loom" --config "$WORK/loom.toml" link --host 10.99.0.1)
start_loom() {
    RUST_LOG=${RUST_LOG:-info} "$BIN/loom" --config "$WORK/loom.toml" >> "$WORK/loom.log" 2>&1 &
    loom_pid=$!
    pids+=("$loom_pid")
}
start_loom

for n in a b; do
    RUST_LOG=${RUST_LOG:-info} nsenter --net="/run/netns/$n" \
        "$BIN/weftd" --state-dir "$WORK/$n" --socket "$WORK/$n.sock" > "$WORK/$n.log" 2>&1 &
    pids+=($!)
done
sleep 1

weft() { local n=$1; shift; LANG=C LC_ALL=C WEFT_SOCKET="$WORK/$n.sock" "$BIN/weft" "$@"; }

weft a up "$LINK" --nickname alice
weft b up "$LINK" --nickname bob
weft a create lan --password secret
weft b join wrong-name --password secret || true
weft b join lan --password wrong || true
weft b join lan --password secret
sleep 3

echo "--- status a"
weft a status
echo "--- status b"
weft b status

addr_b=$(weft b status | awk -F': *' '/^Address/ {print $2}')
ping_b() { nsenter --net=/run/netns/a ping -q -c "$1" -W 2 "${@:2}" "$addr_b"; }

echo "--- ping $addr_b from a"
ping_b 3
echo "--- 1200-byte packets without fragmentation"
ping_b 2 -s 1200 -M do
echo "--- server down, peers keep talking"
kill "$loom_pid"
wait "$loom_pid" 2>/dev/null || true
sleep 1
ping_b 2
echo "--- server back, clients reconnect"
start_loom
for _ in $(seq 20); do
    weft a status | grep -Eq '^Status: +connected' && break
    sleep 1
done
weft a status | grep '^Status'
ping_b 2
echo "--- all checks passed"
if [ -n "${SHOW_LOGS:-}" ]; then
    show_logs
fi
