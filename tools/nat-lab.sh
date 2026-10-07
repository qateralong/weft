#!/usr/bin/env bash
# Runs loom and two weftd instances behind separate Linux NAT routers in unprivileged namespaces.
#   NAT_A, NAT_B: cone (masquerade) or symmetric (masquerade random)
#   BLOCK_LOOM_UDP_A=1: router A drops UDP to loom, so A must use the TCP relay
#   EXPECT: direct or relay
set -Eeuo pipefail

BIN=$(realpath "${BIN:-target/debug}")
SELF=$(realpath "$0")
NAT_A=${NAT_A:-cone}
NAT_B=${NAT_B:-cone}
EXPECT=${EXPECT:-direct}

if [ "${WEFT_LAB_INNER:-}" != 1 ]; then
    WORK=$(mktemp -d)
    trap 'rm -rf "$WORK"' EXIT
    WEFT_LAB_INNER=1 WORK="$WORK" BIN="$BIN" unshare --user --map-root-user --net --mount "$SELF" "$@"
    exit
fi

pids=()
show_logs() { for log in loom a b; do echo "--- $log.log"; cat "$WORK/$log.log" 2>/dev/null || true; done; }
cleanup() { kill "${pids[@]}" 2>/dev/null || true; wait 2>/dev/null || true; }
trap cleanup EXIT
trap show_logs ERR

mount -t tmpfs tmpfs /run
mkdir -p /run/netns
ip link set lo up
ip link add br0 type bridge
ip addr add 10.99.0.1/24 dev br0
ip link set br0 up

site() {
    local name=$1 n=$2 mode=$3 block=$4
    ip netns add "r$name"
    ip netns add "$name"
    ip link add "veth-r$name" type veth peer name wan netns "r$name"
    ip link set "veth-r$name" master br0 up
    ip -n "r$name" addr add "10.99.0.$((n + 1))/24" dev wan
    ip -n "r$name" link set wan up
    ip -n "r$name" link set lo up
    ip -n "r$name" link add lan type veth peer name eth0 netns "$name"
    ip -n "r$name" addr add "192.168.$n.1/24" dev lan
    ip -n "r$name" link set lan up
    ip -n "$name" addr add "192.168.$n.10/24" dev eth0
    ip -n "$name" link set eth0 up
    ip -n "$name" link set lo up
    ip -n "$name" route add default via "192.168.$n.1"
    nsenter --net="/run/netns/r$name" sh -c 'echo 1 > /proc/sys/net/ipv4/ip_forward'
    nsenter --net="/run/netns/r$name" tc qdisc add dev wan root netem delay "${DELAY:-10ms}"
    tc qdisc add dev "veth-r$name" root netem delay "${DELAY:-10ms}"
    local flags=""
    [ "$mode" = symmetric ] && flags="random"
    local drop=""
    [ "$block" = 1 ] && drop='ip daddr 10.99.0.1 udp dport 7443 drop;'
    nsenter --net="/run/netns/r$name" nft -f - <<NFT
table ip nat {
    chain post { type nat hook postrouting priority 100; oifname "wan" masquerade $flags; }
}
table ip filter {
    chain forward { type filter hook forward priority 0; $drop }
}
NFT
}

site a 1 "$NAT_A" "${BLOCK_LOOM_UDP_A:-0}"
site b 2 "$NAT_B" 0

cat > "$WORK/loom.toml" <<CONF
listen = "10.99.0.1:7443"
data_dir = "$WORK/loom"
CONF
LINK=$("$BIN/loom" --config "$WORK/loom.toml" link --host 10.99.0.1)
RUST_LOG=${RUST_LOG:-info} "$BIN/loom" --config "$WORK/loom.toml" > "$WORK/loom.log" 2>&1 &
pids+=($!)

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
weft b join lan --password secret

addr_b=$(weft b status | awk -F': *' '/^Address/ {print $2}')
case "$EXPECT" in
    direct) pattern='online, direct' ;;
    relay) pattern='online, via the server' ;;
esac
for _ in $(seq 30); do
    weft a status | grep -q "$pattern" && weft b status | grep -q "${pattern}" && break
    sleep 1
done
echo "--- NAT_A=$NAT_A NAT_B=$NAT_B BLOCK_LOOM_UDP_A=${BLOCK_LOOM_UDP_A:-0}"
weft a status | grep -E 'bob'
weft b status | grep -E 'alice'
weft a status | grep -q "$pattern"
weft b status | grep -q "$pattern"
nsenter --net=/run/netns/a ping -q -c 3 -W 2 "$addr_b"
echo "--- LAN game discovery from a to b"
addr_a=$(weft a status | awk -F': *' '/^Address/ {print $2}')
nsenter --net=/run/netns/b python3 "$(dirname "$SELF")/lan-probe.py" listen 10 > "$WORK/probe.txt" &
probe=$!
sleep 1
nsenter --net=/run/netns/a python3 "$(dirname "$SELF")/lan-probe.py" send 6
wait "$probe"
sort -u "$WORK/probe.txt"
grep -q "^$addr_a broadcast$" "$WORK/probe.txt"
grep -q "^$addr_a multicast$" "$WORK/probe.txt"
echo "--- passed: $EXPECT"
if [ -n "${SHOW_LOGS:-}" ]; then
    show_logs
fi
