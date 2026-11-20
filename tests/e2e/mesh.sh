#!/usr/bin/env bash
# The devtunnel mesh with three machines (A, B, C) in Docker, through the stand-in devtunnel CLI
# (tests/e2e/fake/devtunnel) and a shared volume for its tunnel map. No Microsoft service is used.
# Usage: tests/e2e/mesh.sh                       KEEP=1 leaves the containers running
#        PREFIX=m PORTS=26 tests/e2e/mesh.sh     container name prefix and SSH port block (2601-2603)
# The machines are user installs (ops@): the system unit has ProtectSystem=strict and cannot
# write the shared volume.
set -euo pipefail

ROOT=${ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}
P=imesh-mesh-${PREFIX:-m}
PB=${PORTS:-26}
WORK=${WORK:-$(mktemp -d /tmp/imesh-mesh.XXXXXX)}
ARCH=$(docker info --format '{{.Architecture}}')
case $ARCH in aarch64 | arm64) T=aarch64-unknown-linux-musl ;; *) T=x86_64-unknown-linux-musl ;; esac

pass() { printf 'PASS %s\n' "$*"; }
fail() { printf 'FAIL %s\n' "$*"; exit 1; }

echo "work dir $WORK"
(cd "$ROOT" && cargo x build --example app ${BUNDLE_TARGETS:-})
I=$ROOT/dist/imesh/imesh
export IMESH_SOCKET=$WORK/no-local-daemon.sock IMESH_CONFIG_DIR=$WORK/cfg IMESH_SSH_CONFIG=$WORK/ssh_config IMESH_SSH_DIR=$WORK/dotssh IMESH_LOG=warn

docker build -q -t imesh-test-node "$ROOT/tests/e2e/docker" >/dev/null
ssh-keygen -q -t ed25519 -N '' -f "$WORK/id" -C imesh-e2e
cat >"$WORK/ssh_config" <<CFG
Host localhost
  UserKnownHostsFile $WORK/known_hosts
  StrictHostKeyChecking accept-new
  IdentityFile $WORK/id
  IdentitiesOnly yes
CFG

NODES="a b c"
cleanup() {
	[ -n "${KEEP:-}" ] && [ "${1:-}" != start ] && return
	for n in $NODES; do docker rm -f "$P-$n" >/dev/null 2>&1 || true; done
	docker volume rm "$P-dt" >/dev/null 2>&1 || true
	docker network rm "$P-lan" >/dev/null 2>&1 || true
}
trap cleanup EXIT
cleanup start
docker network create "$P-lan" >/dev/null
docker volume create "$P-dt" >/dev/null
i=0
for n in $NODES; do
	i=$((i + 1))
	docker run -d --name "$P-$n" --hostname "$n" --network "$P-lan" --privileged --cgroupns=host \
		-v /sys/fs/cgroup:/sys/fs/cgroup:rw -v "$P-dt:/dt" -p "${PB}0$i:22" imesh-test-node >/dev/null
done
sleep 3
docker exec "$P-a" chmod 1777 /dt
for n in $NODES; do
	# No direct UDP between machines: every iroh path goes through a mesh relay.
	docker exec -i "$P-$n" nft -f - <<NFT
table inet mesh {
	chain input { type filter hook input priority 0; iif != lo meta l4proto udp drop; }
}
NFT
	# In /usr/bin: after a reboot, the detached daemon starts from cron, whose PATH has no
	# /usr/local/bin.
	docker cp -q "$ROOT/tests/e2e/fake/devtunnel" "$P-$n:/usr/bin/devtunnel"
	docker exec -i "$P-$n" sh -c "cat > /home/ops/.ssh/authorized_keys; chown ops:ops /home/ops/.ssh/authorized_keys" <"$WORK/id.pub"
done
A=ops@localhost:${PB}01 B=ops@localhost:${PB}02 C=ops@localhost:${PB}03
# One SSH context per machine: `ON $A <command>` runs the command on that machine's daemon.
ctx_of() { case "$1" in "$A") echo a ;; "$B") echo b ;; "$C") echo c ;; *) echo "$1" ;; esac; }
ON() { $I -c "$(ctx_of "$1")" "${@:2}"; }
id_of() { ON "$1" dev call machine.inspect | awk -F'"' '/"id"/ && !n++ {print $4}'; }
tunnel_of() { docker exec "$P-$1" sh -c 'for d in /dt/t/*/; do [ "$(cat $d/host 2>/dev/null)" = '"$1"' ] && basename $d; done; true'; }
# reaches <machine> <name> [seconds]: the machine's status probe gets an answer from member <name>.
reaches() {
	for _ in $(seq 1 "${3:-60}"); do
		[ "$(ON "$1" status 2>/dev/null | awk -v n="$2" '$1==n{print $4}')" = up ] && return 0
		sleep 1
	done
	return 1
}
logs() { for n in $NODES; do echo "--- $n"; docker exec "$P-$n" sh -c 'tail -n 15 /home/ops/.local/share/imesh/*.log 2>/dev/null | sed "s/\x1b\[[0-9;]*m//g" | cut -c1-300' || true; done; }

echo "== 1. A founds the cluster; B and C join; all use --relay devtunnel on one mesh port"
# add over SSH: the invite flow needs the real service to host a pairing tunnel. With one port,
# each machine's own relay holds it, so every forward listens on another local port.
$I add "$A" --new --name a --relay devtunnel --mesh-port 3400 >/dev/null
$I add "$B" --name b --relay devtunnel --mesh-port 3400 >/dev/null
$I add "$C" --name c --relay devtunnel --mesh-port 3400 >/dev/null
for n in a b c; do
	case $n in a) h=$A ;; b) h=$B ;; c) h=$C ;; esac
	$I ctx add "$n" "$h" >/dev/null
done
$I ctx use default >/dev/null
for _ in $(seq 1 90); do
	up=$($I status | awk 'NR>1 && $4=="up"' | wc -l | tr -d ' ')
	[ "$up" = 3 ] && break
	sleep 1
done
$I status
[ "$up" = 3 ] || { logs; fail "not all 3 up"; }
$I status | awk 'NR>1 && $5=="direct"' | grep -q . && fail "a direct path exists"
pass "a, b, c are up, through relays, with one mesh port"

echo "== 2. each machine hosts one tunnel with a readable name and forwards each peer"
for n in $NODES; do
	id=$(id_of "ops@localhost:${PB}0$(( $(printf '%d' "'$n") - 96 ))")
	tun=$(tunnel_of "$n")
	[ "$(echo "$tun" | wc -w | tr -d ' ')" = 1 ] || fail "$n hosts tunnels: $tun"
	echo "$tun" | grep -Eq "^imesh-$n-[0-9a-f]{10}$" || fail "$n tunnel name $tun"
	[ "${id:0:10}" = "${tun##*-}" ] || fail "$n tunnel $tun does not end in the machine id $id"
	eval "T_$n=$tun"
done
for n in $NODES; do
	for _ in $(seq 1 30); do
		f=$(docker exec "$P-$n" sh -c 'pgrep -u ops -f "^/bin/sh /usr/bin/devtunnel connect" | wc -l')
		[ "$f" = 2 ] && break
		sleep 1
	done
	[ "$f" = 2 ] || { docker exec "$P-$n" ps -ef; fail "$n runs $f forwards, not 2"; }
done
docker exec "$P-a" grep -qx "b connect $T_c" /dt/log || { docker exec "$P-a" cat /dt/log; fail "b did not connect to c's tunnel"; }
pass "tunnels $T_a $T_b $T_c; 2 forwards each; b connected to $T_c"

echo "== 3. b and c reach each other"
reaches "$B" c || fail "b to c"
reaches "$C" b || fail "c to b"
pass "b to c and c to b"

echo "== 4. a stops; b and c still reach each other; a reaches both when it comes back"
docker stop "$P-a" >/dev/null
reaches "$B" c || { logs; fail "b to c with a down"; }
reaches "$C" b || { logs; fail "c to b with a down"; }
[ "$(ON "$B" status | awk '$1=="a"{print $4}')" = down ] || fail "b does not report a down"
pass "b and c reach each other with a stopped, and report a down"
docker start "$P-a" >/dev/null
t0=$(date +%s)
reaches "$A" b 180 && reaches "$A" c 30 || { logs; fail "a does not reach b and c after it started"; }
pass "a reaches b and c $(($(date +%s) - t0))s after it started"

echo "== 5. a stuck forward is stopped and started again by the daemon"
fwd=$(docker exec "$P-b" cat "/tmp/fake-devtunnel/forwarder-$T_c")
conn=$(docker exec "$P-b" cat "/tmp/fake-devtunnel/connecting-$T_c")
docker exec "$P-b" kill "$fwd"
docker exec "$P-b" kill -0 "$conn" || fail "the stuck connect is not running"
t0=$(date +%s)
for _ in $(seq 1 150); do
	conn2=$(docker exec "$P-b" cat "/tmp/fake-devtunnel/connecting-$T_c")
	[ "$conn2" != "$conn" ] && docker exec "$P-b" kill -0 "$conn2" 2>/dev/null && break
	sleep 1
done
[ "$conn2" != "$conn" ] || { logs; fail "b did not restart its stuck forward to c within 150s"; }
docker exec "$P-b" kill -0 "$conn" 2>/dev/null && fail "the stuck connect still runs"
reaches "$B" c || fail "b to c after the restart"
pass "b stopped and restarted its stuck forward to c after $(($(date +%s) - t0))s; b reaches c"

echo "== 6. a stuck tunnel host is stopped and started again"
host=$(docker exec "$P-c" cat /tmp/fake-devtunnel/hosting)
docker exec "$P-c" kill -STOP "$host"
t0=$(date +%s)
for _ in $(seq 1 150); do
	host2=$(docker exec "$P-c" cat /tmp/fake-devtunnel/hosting)
	[ "$host2" != "$host" ] && docker exec "$P-c" kill -0 "$host2" 2>/dev/null && break
	sleep 1
done
[ "$host2" != "$host" ] || { logs; fail "c did not restart its stuck host within 150s"; }
reaches "$B" c || fail "b to c after the host restart"
pass "c stopped and restarted its stuck host after $(($(date +%s) - t0))s; b reaches c"

echo "== 7. c leaves with logout; its tunnel is deleted"
# The owner cannot leave, so a plain member does.
docker exec -u ops -w /home/ops "$P-c" /home/ops/.local/share/imesh/bin/imesh logout
docker exec "$P-a" test ! -d "/dt/t/$T_c" || fail "c's tunnel $T_c is still in the map"
for m in "$A" "$B"; do
	for _ in $(seq 1 30); do ON "$m" status | awk 'NR>1{print $1}' | grep -qx c || break; sleep 1; done
	ON "$m" status | awk 'NR>1{print $1}' | grep -qx c && fail "$m still lists c"
done
reaches "$A" b || fail "a to b after c left"
reaches "$B" a || fail "b to a after c left"
for n in a b; do
	f=$(docker exec "$P-$n" sh -c 'pgrep -u ops -f "^/bin/sh /usr/bin/devtunnel connect" | wc -l')
	[ "$f" = 1 ] || fail "$n runs $f forwards after c left, not 1"
done
pass "c's tunnel is gone; a and b do not list c, run 1 forward each, and reach each other"

echo "== 8. b moves its relay to another mesh port; its tunnel follows and a reaches b"
$I update server "$B" --mesh-port 3500 >/dev/null
for _ in $(seq 1 60); do
	[ "$(docker exec "$P-a" cat "/dt/t/$T_b/port" 2>/dev/null)" = 3500 ] && break
	sleep 1
done
[ "$(docker exec "$P-a" cat "/dt/t/$T_b/port" 2>/dev/null)" = 3500 ] || { logs; fail "b's tunnel does not have port 3500"; }
reaches "$A" b 120 || { logs; fail "a to b after b moved its port"; }
reaches "$B" a || fail "b to a after b moved its port"
pass "b's tunnel has port 3500; a and b reach each other"

echo "ALL PASS"
