#!/usr/bin/env bash
# Runs loom and two weftd instances in isolated network namespaces (no root needed)
# and checks that the peers can ping each other over the virtual network.
set -Eeuo pipefail
# Test daemons must not join the public server.
export WEFT_PUBLIC_SERVER=

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
wait_port() {
    for _ in $(seq 100); do
        (exec 3<> "/dev/tcp/$1/$2") 2> /dev/null && return
        sleep 0.1
    done
    return 1
}
start_loom() {
    RUST_LOG=${RUST_LOG:-info} "$BIN/loom" --config "$WORK/loom.toml" >> "$WORK/loom.log" 2>&1 &
    loom_pid=$!
    pids+=("$loom_pid")
    wait_port 10.99.0.1 7443
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
echo "--- join by a one-time invite while disconnected"
invite=$(weft a invite create lan --uses 1 --expires 1h | grep '^weft://')
weft b down
weft b join "$invite"
weft b join "$invite" && exit 1
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
echo "--- peer names through the daemon resolver"
addr_a=$(weft a status | awk -F': *' '/^Address/ {print $2}')
dig_a() { nsenter --net=/run/netns/a dig +short +time=2 +tries=2 @100.100.100.100 "$@"; }
[ "$(dig_a bob.weft)" = "$addr_b" ]
[ "$(dig_a ALICE.weft)" = "$addr_a" ]
[ -z "$(dig_a nobody.weft)" ]
weft a netcheck | grep '^Peer names'
nsenter --net=/run/netns/a ping -q -c 1 -W 2 "$addr_b" > /dev/null
echo "--- kick, ban and unban"
weft a invite list lan
weft b invite create lan && exit 1
weft a kick lan bob
weft b status | grep -q 'No networks'
weft b join lan --password secret
weft a ban lan "$addr_b"
weft a bans lan | grep -q bob
weft b join lan --password secret && exit 1
weft a unban lan bob
weft b join lan --password secret
echo "--- approval, roles and settings"
weft a kick lan bob
weft a approval lan on
weft b join lan --password secret | grep -q 'wait for'
weft a status | grep -q 'requests: 1'
weft a requests lan | grep -q bob
weft a approve lan bob
weft a promote lan bob
sleep 1
weft b status | grep -q '(admin)'
weft b lock lan
weft a status | grep -q 'locked'
weft b password lan --password other && exit 1
weft a password lan --password changed
weft b unlock lan
weft b approval lan off
weft a demote lan bob
weft b lock lan && exit 1
weft a delete lan && exit 1
sleep 3
ping_b 2
weft a delete lan --yes
sleep 1
weft b status | grep -q 'No networks'
echo "--- loom admin"
loom_admin() { "$BIN/loom" --config "$WORK/loom.toml" admin "$@"; }
loom_admin stats
loom_admin devices --online | grep -q alice
loom_admin block alice
weft a status | grep -Eq '^Status: +connected' && sleep 2
weft a status | grep -Eq '^Status: +connected' && exit 1
loom_admin devices --blocked | grep -q alice
loom_admin unblock alice
for _ in $(seq 45); do
    weft a status | grep -Eq '^Status: +connected' && break
    sleep 1
done
weft a status | grep -Eq '^Status: +connected'
echo "--- a second server"
cat > "$WORK/loom2.toml" <<CONF
listen = "10.99.0.1:7444"
data_dir = "$WORK/loom2"
CONF
LINK2=$("$BIN/loom" --config "$WORK/loom2.toml" link --host 10.99.0.1)
RUST_LOG=${RUST_LOG:-info} "$BIN/loom" --config "$WORK/loom2.toml" > "$WORK/loom2.log" 2>&1 &
pids+=($!)
wait_port 10.99.0.1 7444
weft a up "$LINK2"
weft b up "$LINK2"
weft a create games --password secret && exit 1
weft a create games --password secret --server 10.99.0.1:7444
weft b join games --password secret --server 10.99.0.1 && exit 1
weft b join games --password secret --server 10.99.0.1:7444
weft a status
[ "$(weft a status | grep -c '^Server')" = 2 ]
[ "$(weft b status | awk -F': *' '/^Address/ {print $2}' | sort -u)" = "$addr_b" ]
weft a invite create games | grep -q '^weft://10.99.0.1:7444/'
sleep 3
ping_b 2
weft a remove 10.99.0.1:7444
[ "$(weft a status | grep -c '^Server')" = 1 ]
echo "--- a server hosted by the app"
weft a host on --address 10.99.0.2 | tee "$WORK/host.txt"
hosted=$(grep -o 'weft://[^ ]*' "$WORK/host.txt" | head -1)
case "$hosted" in weft://10.99.0.2[:#]*) ;; *) exit 1 ;; esac
weft b up "$hosted"
sleep 1
weft a create home --password secret --server "$hosted"
weft b join home --password secret --server "$hosted"
sleep 3
ping_b 2
weft a host off
weft a status | grep -q 'Your server' && exit 1
echo "--- all checks passed"
if [ -n "${SHOW_LOGS:-}" ]; then
    show_logs
fi
