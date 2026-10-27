#!/usr/bin/env bash
# End-to-end proof of imesh on Linux. Needs Docker and cargo-zigbuild. Runs on an arm64 or x86_64 host.
# Usage: tests/e2e/linux.sh                                  all scenarios against the imesh-cli example app
#        CRATE=prototypes/imesh-docs tests/e2e/linux.sh      a prototype with the same CLI
#        KEEP=1 tests/e2e/linux.sh                           leave containers running afterwards
#        PREFIX=b PORTS=24 tests/e2e/linux.sh                container name prefix and SSH port block (24 gives 2401-2405)
set -euo pipefail

ROOT=${ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}
CRATE=$ROOT/${CRATE:-crates/imesh-cli}
# The cargo target directory: the workspace's for crates/*, the crate's own for a prototype.
TARGET=$(cd "$CRATE" && cargo metadata -q --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
P=imesh-e2e-${PREFIX:-a}
PB=${PORTS:-23}
WORK=${WORK:-$(mktemp -d /tmp/imesh-e2e.XXXXXX)}
ARCH=$(docker info --format '{{.Architecture}}')
case $ARCH in aarch64 | arm64) T=aarch64-unknown-linux-musl A=AARCH64 ;; *) T=x86_64-unknown-linux-musl A=X86_64 ;; esac

pass() { printf 'PASS %s\n' "$*"; }
fail() { printf 'FAIL %s\n' "$*"; exit 1; }

echo "work dir $WORK"
# Test the example app, built as any app is (imesh::main plus its own tables and methods), when
# the crate has one; a prototype without it uses its plain binary.
if [ -f "$CRATE/examples/app.rs" ]; then BUILD="--example app"; OUT=examples/app; else BUILD=; OUT=imesh; fi
(cd "$CRATE" && cargo build -q $BUILD && cargo zigbuild -q --release --target "$T" $BUILD)
I=$TARGET/debug/$OUT
export IMESH_CONFIG_DIR=$WORK/cfg IMESH_SSH_CONFIG=$WORK/ssh_config IMESH_LOG=warn
export "IMESH_BINARY_$A=$TARGET/$T/release/$OUT"

docker build -q -t imesh-test-node "$ROOT/tests/e2e/docker" >/dev/null
ssh-keygen -q -t ed25519 -N '' -f "$WORK/id" -C imesh-e2e
cat >"$WORK/ssh_config" <<EOF
Host localhost
  UserKnownHostsFile $WORK/known_hosts
  StrictHostKeyChecking accept-new
  IdentityFile $WORK/id
  IdentitiesOnly yes
EOF

NODES="n1 n2 n3 n4 n5 n6 n7 n8 n9"
# n9 runs its own Docker, as a NAS does; imesh runs in containers there.
# KEEP=1 keeps the containers after a run; a new run always starts clean.
cleanup() {
	[ -n "${KEEP:-}" ] && [ "${1:-}" != start ] && return
	for n in $NODES relay outsider; do docker rm -f "$P-$n" >/dev/null 2>&1 || true; done
	docker volume rm "$P-n9-docker" >/dev/null 2>&1 || true
	for net in lan a b; do docker network rm "$P-$net" >/dev/null 2>&1 || true; done
}
trap cleanup EXIT
cleanup start
for net in lan a b; do docker network create "$P-$net" >/dev/null; done

node() { # name network ssh-port
	docker run -d --name "$P-$1" --hostname "$1" --network "$P-$2" --privileged --cgroupns=host \
		-v /sys/fs/cgroup:/sys/fs/cgroup:rw -p "${PB}$3:22" imesh-test-node >/dev/null
}
node n1 lan 01
node n2 lan 02
node n3 lan 03
node n4 a 04
node n5 b 05
node n6 lan 06
node n7 lan 07
node n8 lan 08
docker run -d --name "$P-n9" --hostname n9 --network "$P-lan" --privileged --cgroupns=host \
	-v /sys/fs/cgroup:/sys/fs/cgroup:rw -v "$P-n9-docker:/var/lib/docker" -p "${PB}09:22" imesh-test-node >/dev/null
docker run -d --name $P-relay --network $P-a n0computer/iroh-relay:latest --dev >/dev/null
docker network connect $P-b $P-relay
sleep 3
docker exec "$P-n9" systemctl start docker
for n in $NODES; do
	docker exec -i "$P-$n" sh -c "cat > /root/.ssh/authorized_keys; for u in deploy ops; do cp /root/.ssh/authorized_keys /home/\$u/.ssh/; chown -R \$u:\$u /home/\$u/.ssh; done" <"$WORK/id.pub"
done
# n3 runs its daemon for `ops`, a user with no sudo.
N3="ops@localhost:${PB}03"

echo "== 1. machine init installs, starts the daemon, creates a cluster, writes a context"
t0=$(date +%s)
$I machine init root@localhost:${PB}01 >/dev/null
[ "$(docker exec $P-n1 systemctl is-active imesh)" = active ] || fail "daemon not active"
grep -q 'current_context = "default"' "$WORK/cfg/config.toml" || fail "no default context"
pass "init took $(($(date +%s) - t0))s, daemon active, context default"

echo "== 2. init again is refused"
$I machine init root@localhost:${PB}01 >/dev/null 2>&1 && fail "second init allowed" || pass "second init refused"

echo "== 3. machine add; a user install needs no sudo; a root login gets a system service"
$I machine add root@localhost:${PB}02 --run-as imeshd >/dev/null
$I machine add "$N3" >/dev/null
[ "$(docker exec "$P-n3" stat -c '%U %a' /home/ops/.local/share/imesh/imesh.sock)" = "ops 600" ] || fail "n3 socket is not owner-only in the user's home"
[ "$(docker exec "$P-n3" ps -o user= -C imesh | sort -u)" = ops ] || fail "n3 daemon not running as ops"
docker exec "$P-n3" test ! -e /etc/imesh || fail "a user install wrote /etc/imesh"
docker exec "$P-n3" su ops -c 'crontab -l' | grep -qF "@reboot '/home/ops/.local/bin/imesh' daemon start" || fail "n3 has no @reboot line"
[ "$(docker exec "$P-n2" ps -o user= -C imesh | sort -u)" = imeshd ] || fail "n2 daemon not running as imeshd"
[ "$(docker exec "$P-n1" ps -o user= -C imesh | sort -u)" = imesh ] || fail "a root login should run the daemon as imesh"
pass "n3 runs as ops with no sudo (~/.local, socket 600, @reboot in its crontab); n1 as imesh (root login), n2 as imeshd (--run-as)"

echo "== 4. every machine sees every other machine up"
for i in $(seq 1 30); do
	up=$($I machine ls | awk 'NR>1 && $3=="up"' | wc -l | tr -d ' ')
	[ "$up" = 3 ] && break
	sleep 1
done
[ "$up" = 3 ] || { $I machine ls; fail "not all up"; }
$I machine ls
pass "all 3 up from node1's view"

echo "== 5. the CLI reaches a user daemon over ssh"
[ "$($I --connect "$N3" call ping)" = '"pong"' ] || fail "ops ping"
pass "ops@n3 reaches its own daemon over dial-stdio"

echo "== 6. the CLI reaches the cluster over iroh (relay from this host into Docker)"
ID1=$(awk -F'"' '/^iroh = /{print $2; exit}' "$WORK/cfg/config.toml")
[ "$($I --connect "iroh://$ID1" call ping)" = '"pong"' ] || fail "iroh ping"
pass "iroh://$ID1 ping"

echo "== 7. one machine forwards a call to another"
name=$($I --connect root@localhost:${PB}01 call machine.inspect -m n3 | grep -m1 '"name"')
echo "$name" | grep -q n3 || fail "forward: $name"
pass "call through n1 served by n3"

echo "== 7b. an app ALPN between two machines, driven by an app API method"
ID2=$($I --connect root@localhost:${PB}02 call machine.inspect | awk -F'"' '/"id"/{print $4; exit}')
$I --connect root@localhost:${PB}01 call example.dial "{\"to\":\"$ID2\",\"text\":\"hi n2\"}" | grep -q '"echo": "hi n2"' || fail "example ALPN"
pass "n1 opened imesh-example/echo/0 to n2"

echo "== 8. a write on one machine reaches the others"
$I --connect root@localhost:${PB}01 call kv.put '{"key":"k","value":"v1"}' >/dev/null
for i in $(seq 1 20); do [ "$($I --connect "$N3" call kv.get '{"key":"k"}')" = '"v1"' ] && break; sleep 0.5; done
[ "$($I --connect "$N3" call kv.get '{"key":"k"}')" = '"v1"' ] || fail "kv not synced"
pass "kv written on n1, read on n3"

echo "== 9. a machine goes down; the rest keep working; it catches up"
docker pause $P-n2 >/dev/null
$I --connect root@localhost:${PB}01 call kv.put '{"key":"k2","value":"while-down"}' >/dev/null
for i in $(seq 1 20); do [ "$($I --connect "$N3" call kv.get '{"key":"k2"}')" = '"while-down"' ] && break; sleep 0.5; done
[ "$($I --connect "$N3" call kv.get '{"key":"k2"}')" = '"while-down"' ] || fail "n3 did not get the write made while n2 was down"
for i in $(seq 1 90); do
	s=$($I --connect root@localhost:${PB}01 machine ls | awk '$1=="n2"{print $3}')
	[ "$s" = down ] && break
	sleep 1
done
[ "$s" = down ] || fail "n2 not reported down"
pass "n2 reported down after ${i}s; n1 and n3 still write and read"
docker unpause $P-n2 >/dev/null
t0=$(date +%s)
until [ "$($I --connect root@localhost:${PB}02 call kv.get '{"key":"k2"}' 2>/dev/null)" = '"while-down"' ]; do
	[ $(($(date +%s) - t0)) -gt 60 ] && fail "n2 did not catch up"
	sleep 1
done
pass "n2 caught up $(($(date +%s) - t0))s after resume"

echo "== 9b. a network split: both sides keep writing, then converge"
# n1 keeps only its SSH server, which the test drives. It loses n2, n3, and every relay.
docker exec -i $P-n1 nft -f - <<EOF
table inet split {
	chain in { type filter hook input priority 0; policy drop; iif lo accept; tcp dport 22 accept; }
	chain out { type filter hook output priority 0; policy drop; oif lo accept; tcp sport 22 accept; }
}
EOF
$I --connect root@localhost:${PB}01 call kv.put '{"key":"split","value":"n1-side"}' >/dev/null
sleep 1
$I --connect root@localhost:${PB}02 call kv.put '{"key":"split","value":"n2-side-later"}' >/dev/null
$I --connect root@localhost:${PB}02 call kv.put '{"key":"only-n2-side","value":1}' >/dev/null
$I --connect root@localhost:${PB}01 call kv.put '{"key":"only-n1-side","value":1}' >/dev/null
[ "$($I --connect root@localhost:${PB}01 call kv.get '{"key":"split"}')" = '"n1-side"' ] || fail "n1 lost its own write during split"
for i in $(seq 1 20); do [ "$($I --connect "$N3" call kv.get '{"key":"split"}')" = '"n2-side-later"' ] && break; sleep 0.5; done
[ "$($I --connect "$N3" call kv.get '{"key":"split"}')" = '"n2-side-later"' ] || fail "n3 missed n2 write during split"
pass "both sides accepted writes during the split"
for i in $(seq 1 60); do
	d=$($I --connect root@localhost:${PB}01 machine ls | awk '$3=="down"' | wc -l | tr -d ' ')
	[ "$d" = 2 ] && break
	sleep 1
done
[ "$d" = 2 ] || fail "n1 did not mark n2 and n3 down"
sleep 30
pass "n1 marked both peers down after ${i}s and stayed cut off 30s more"
docker exec $P-n1 nft delete table inet split
t0=$(date +%s)
for n in root@localhost:${PB}01 root@localhost:${PB}02 "$N3"; do
	until [ "$($I --connect "$n" call kv.get '{"key":"split"}' 2>/dev/null)" = '"n2-side-later"' ] &&
		[ "$($I --connect "$n" call kv.get '{"key":"only-n1-side"}' 2>/dev/null)" = 1 ] &&
		[ "$($I --connect "$n" call kv.get '{"key":"only-n2-side"}' 2>/dev/null)" = 1 ]; do
		[ $(($(date +%s) - t0)) -gt 90 ] && fail "no convergence on $n"
		sleep 1
	done
done
pass "all three converged $(($(date +%s) - t0))s after heal: later write won, both sides' keys kept"

echo "== 10. a daemon restart keeps identity and state"
id_before=$($I --connect root@localhost:${PB}02 call machine.inspect | grep -m1 '"id"')
docker exec $P-n2 systemctl restart imesh
sleep 1
[ "$($I --connect root@localhost:${PB}02 call machine.inspect | grep -m1 '"id"')" = "$id_before" ] || fail "identity changed"
[ "$($I --connect root@localhost:${PB}02 call kv.get '{"key":"k"}')" = '"v1"' ] || fail "state lost"
pass "same id and state after restart"

echo "== 11. contexts: a second context for the same cluster, iroh first"
cp "$WORK/cfg/config.toml" "$WORK/config.before"
$I ctx add remote "iroh://$ID1" "root@localhost:${PB}01" >/dev/null
$I ctx use remote >/dev/null
[ "$($I call ping)" = '"pong"' ] || fail "remote context"
$I ctx ls
$I ctx use default >/dev/null
$I ctx conn 1 >/dev/null
head -3 <<<"$($I ctx conn)" | grep -q '^0  iroh://' || fail "ctx conn did not move entry to top"
IMESH_CONTEXT=remote $I call ping >/dev/null || fail "IMESH_CONTEXT"
pass "ctx use, ctx conn reorder, IMESH_CONTEXT"

echo "== 12. a dead connection falls through to the next one"
python3 - "$WORK/cfg/config.toml" "$PB" <<'EOF'
import sys
p, pb = sys.argv[1], sys.argv[2]
s = open(p).read()
s = s.replace('[[contexts.default.connections]]', f'[[contexts.default.connections]]\nssh = "root@localhost:{pb}99"\n\n[[contexts.default.connections]]', 1)
open(p, "w").write(s)
EOF
[ "$($I call ping)" = '"pong"' ] || fail "fallback"
pass "dead ssh entry skipped"
cp "$WORK/config.before" "$WORK/cfg/config.toml"

echo "== 13. a stranger cannot open the API or the sync ALPN"
out=$(IMESH_CONFIG_DIR=$WORK/stranger $I --connect "iroh://$ID1" call ping 2>&1 || true)
echo "$out" | grep -qi 'rejected\|403\|not a member' || fail "stranger not rejected: $out"
pass "unknown CLI key rejected"

echo "== 14. machine rm resets the machine and revokes it"
ID3=$($I --connect "$N3" call machine.inspect | awk -F'"' '/"id"/{print $4; exit}')
$I machine rm n3 >/dev/null
$I machine ls | grep -q '^n3 ' && fail "n3 still listed"
for i in $(seq 1 30); do
	new3=$($I --connect "$N3" call machine.inspect 2>/dev/null | awk -F'"' '/"id"/{print $4; exit}' || true)
	[ -n "$new3" ] && [ "$new3" != "$ID3" ] && break
	sleep 0.5
done
[ -n "$new3" ] && [ "$new3" != "$ID3" ] || fail "n3 kept its identity"
$I --connect "$N3" call machine.inspect | grep -q '"cluster": null' || fail "n3 kept cluster state"
pass "n3 removed, reset with a new identity, out of the cluster"

echo "== 15. self-hosted relay, no n0, no direct path between two networks"
relay_ip=$(docker exec "$P-n4" getent hosts "$P-relay" | cut -d" " -f1)
docker exec "$P-n4" timeout 2 bash -c "</dev/tcp/$relay_ip/3340" || fail "relay not reachable"
$I -c relayonly machine init root@localhost:${PB}04 --relay http://$P-relay:3340 --no-n0 >/dev/null
$I ctx use default >/dev/null
$I -c relayonly machine add root@localhost:${PB}05 >/dev/null
for i in $(seq 1 30); do
	p=$($I -c relayonly machine ls | awk '$1=="n5"{print $3"/"$4}')
	[ "$p" = up/relay ] && break
	sleep 1
done
$I -c relayonly machine ls
[ "$p" = up/relay ] || fail "n5 not up over relay"
pass "n4 and n5 connected only through the self-hosted relay"

echo "== 17. app data at scale: 1000 keys of 1 KiB written on one machine"
now() { python3 -c 'import time;print(time.time())'; }
t0=$(now)
$I --connect root@localhost:${PB}01 call kv.fill '{"prefix":"bulk","count":1000,"bytes":1024}' >/dev/null
t1=$(now)
until [ "$($I --connect root@localhost:${PB}02 call kv.count '{"prefix":"bulk"}')" = 1000 ]; do
	[ "$(python3 -c "print(int($(now) - $t0))")" -gt 120 ] && fail "bulk did not reach n2"
	sleep 0.2
done
t2=$(now)
pass "$(python3 -c "print(f'write {$t1-$t0:.1f}s, all on n2 after {$t2-$t0:.1f}s')")"

echo "== 18. all UDP blocked on one machine: it joins and works through a members-only relay served by another machine"
$I -c udp machine init root@localhost:${PB}06 --serve-relay 0.0.0.0:3340 --relay "http://$P-n6:3340" --no-n0 >/dev/null
$I ctx use default >/dev/null
docker exec -i "$P-n7" nft -f - <<EOF
table inet noudp {
	chain in { type filter hook input priority 0; iif != lo meta l4proto udp drop; }
	chain out { type filter hook output priority 0; oif != lo meta l4proto udp drop; }
}
EOF
$I -c udp machine add root@localhost:${PB}07 >/dev/null
for i in $(seq 1 30); do
	p=$($I -c udp machine ls | awk '$1=="n7"{print $3"/"$4}')
	[ "$p" = up/relay ] && break
	sleep 1
done
$I -c udp machine ls
[ "$p" = up/relay ] || fail "n7 not up over the relay on n6"
$I -c udp call kv.put '{"key":"udp","value":"over-tcp"}' >/dev/null
for i in $(seq 1 20); do [ "$($I --connect root@localhost:${PB}07 call kv.get '{"key":"udp"}')" = '"over-tcp"' ] && break; sleep 0.5; done
[ "$($I --connect root@localhost:${PB}07 call kv.get '{"key":"udp"}')" = '"over-tcp"' ] || fail "no sync over the relay"
docker exec "$P-n6" journalctl -u imesh --no-pager | grep -q "relay serving members" || fail "relay not started on n6"
pass "n7 with no UDP reached n6 only through n6's own relay, and synced"

echo "== 19. no SSH: a machine installs itself, prints a ticket, and joins with the ticket that add prints"
docker cp -q "$TARGET/$T/release/$OUT" "$P-n8:/usr/local/bin/imesh-setup"
docker exec "$P-n8" /usr/local/bin/imesh-setup daemon install >/dev/null
ticket=$(docker exec "$P-n8" imesh machine id)
case $ticket in endpoint*) ;; *) fail "machine id printed $ticket" ;; esac
jt=$($I machine add "$ticket" --name n8 | grep -o 'imeshjoin[A-Za-z0-9_-]*')
[ -n "$jt" ] || fail "add printed no join ticket"
docker exec "$P-n8" imesh machine join "$jt" >/dev/null
for i in $(seq 1 30); do
	s=$($I machine ls | awk '$1=="n8"{print $3}')
	[ "$s" = up ] && break
	sleep 1
done
[ "$s" = up ] || { $I machine ls; fail "n8 not up"; }
$I call kv.put '{"key":"n8","value":"ticket"}' >/dev/null
for i in $(seq 1 20); do [ "$(docker exec "$P-n8" imesh call kv.get '{"key":"n8"}')" = '"ticket"' ] && break; sleep 0.5; done
[ "$(docker exec "$P-n8" imesh call kv.get '{"key":"n8"}')" = '"ticket"' ] || fail "n8 did not sync"
pass "n8 joined with tickets only, no SSH from the CLI, and synced"

echo "== 20. review fixes: iroh forwarding, key revocation, durable writes, safe re-runs, update"
ID1=$($I --connect root@localhost:${PB}01 call machine.inspect | awk -F'"' '/"id"/{print $4; exit}')
$I --connect "iroh://$ID1" call machine.inspect -m n2 | grep -q '"n2"' || fail "forward over the CLI's iroh connection"
pass "a call over the CLI's iroh connection is forwarded to another machine"
out=$($I --connect "iroh://$ID1" call example.note '{"text":"x"}' -m n2 2>&1 || true)
case $out in *"CLI keys may not write notes"*) ;; *) fail "a forwarded call ran with more rights than the CLI key: $out" ;; esac
[ "$($I --connect root@localhost:${PB}01 call whoami -m n2 | awk -F'"' '/"caller"/{print $4}')" = member ] || fail "forward from the local socket"
pass "a forwarded call runs as the CLI key, not as the forwarding member"

K2=$(IMESH_CONFIG_DIR=$WORK/second $I key)
$I client add "$K2" --name second >/dev/null
[ "$(IMESH_CONFIG_DIR=$WORK/second $I --connect "iroh://$ID1" call ping)" = '"pong"' ] || fail "second key not authorized"
$I client rm "$K2" >/dev/null
if IMESH_CONFIG_DIR=$WORK/second $I --connect "iroh://$ID1" call ping >/dev/null 2>&1; then fail "revoked key still works"; fi
pass "a second CLI key works after client add, and fails after client rm"

docker exec -i "$P-n1" nft -f - <<EOF
table inet solo {
	chain in { type filter hook input priority 0; policy drop; iif lo accept; tcp dport 22 accept; }
	chain out { type filter hook output priority 0; policy drop; oif lo accept; tcp sport 22 accept; }
}
EOF
$I --connect root@localhost:${PB}01 call kv.put '{"key":"durable","value":"acked"}' >/dev/null
docker exec "$P-n1" pkill -9 -f 'imesh daemon'
for i in $(seq 1 40); do
	v=$($I --connect root@localhost:${PB}01 call kv.get '{"key":"durable"}' 2>/dev/null || true)
	[ -n "$v" ] && break
	sleep 0.5
done
docker exec "$P-n1" nft delete table inet solo
[ "$v" = '"acked"' ] || fail "acknowledged write lost after kill -9: $v"
pass "a write acknowledged while cut off survives kill -9 of its only holder"

pid=$(docker exec "$P-n1" systemctl show -p MainPID --value imesh)
env_before=$(docker exec "$P-n1" cat /etc/imesh/env)
$I machine init "root@localhost:${PB}01" --serve-relay 0.0.0.0:3999 >/dev/null 2>&1 && fail "second init allowed"
[ "$(docker exec "$P-n1" systemctl show -p MainPID --value imesh)" = "$pid" ] || fail "refused init restarted the daemon"
[ "$(docker exec "$P-n1" cat /etc/imesh/env)" = "$env_before" ] || fail "refused init changed settings"
pass "a refused init leaves the daemon and its settings alone"

env4=$(docker exec "$P-n4" cat /etc/imesh/env)
$I -c relayonly machine update "root@localhost:${PB}04" --label role=gateway >/dev/null
docker exec "$P-n4" grep -q IMESH_NO_N0 /etc/imesh/env || fail "update dropped --no-n0"
docker exec "$P-n4" grep -q 'IMESH_LABELS=role=gateway' /etc/imesh/env || fail "update did not add the label"
for i in $(seq 1 30); do
	p=$($I -c relayonly machine ls | awk '$1=="n5"{print $3"/"$4}')
	[ "$p" = up/relay ] && break
	sleep 1
done
[ "$p" = up/relay ] || fail "relay-only pair broke after update"
pass "machine update kept the relay settings, added a label, and the relay-only pair stayed up"

old8=$(docker exec "$P-n8" imesh call machine.inspect | awk -F'"' '/"id"/{print $4; exit}')
$I --connect "iroh://$ID1" machine rm n8 >/dev/null
for i in $(seq 1 30); do
	new8=$(docker exec "$P-n8" imesh call machine.inspect 2>/dev/null | awk -F'"' '/"id"/{print $4; exit}' || true)
	[ -n "$new8" ] && [ "$new8" != "$old8" ] && break
	sleep 0.5
done
[ -n "$new8" ] && [ "$new8" != "$old8" ] || fail "rm over iroh did not reset n8"
pass "machine rm over the CLI's iroh connection reset n8"

$I machine update "deploy@localhost:${PB}01" --system --run-as imeshsvc >/dev/null
[ "$(docker exec "$P-n1" ps -o user= -C imesh | sort -u)" = imeshsvc ] || fail "n1 daemon not running as imeshsvc"
[ "$(docker exec "$P-n1" stat -c '%G %a' /run/imesh/imesh.sock)" = "imesh 660" ] || fail "n1 socket is not imesh 660"
[ "$($I --connect deploy@localhost:${PB}01 call ping)" = '"pong"' ] || fail "deploy cannot reach the socket through the group"
pass "after update --system --run-as imeshsvc, deploy reaches the socket through the imesh group"

echo "== 21. schema changes: new tables and columns reach every machine, also one that was away"
N1="--connect root@localhost:${PB}01"
N2="--connect root@localhost:${PB}02"
cat >"$WORK/v1.sql" <<'SQL'
CREATE TABLE app_notes (id TEXT NOT NULL PRIMARY KEY, body TEXT NOT NULL DEFAULT '');
SQL
$I $N1 schema apply "$WORK/v1.sql" >/dev/null
for i in $(seq 1 40); do $I $N2 sql "SELECT count(*) FROM app_notes" >/dev/null 2>&1 && break; sleep 0.5; done
$I $N2 sql "SELECT count(*) FROM app_notes" >/dev/null || fail "n2 did not get table app_notes"
$I $N2 sql --exec "INSERT INTO app_notes (id, body) VALUES ('a', 'from n2')" >/dev/null
for i in $(seq 1 40); do [ "$($I $N1 sql "SELECT body FROM app_notes WHERE id = 'a'" | tail -1)" = "from n2" ] && break; sleep 0.5; done
[ "$($I $N1 sql "SELECT body FROM app_notes WHERE id = 'a'" | tail -1)" = "from n2" ] || fail "row from n2 not on n1"
pass "a new table reached every machine, and rows flow both ways"

docker pause "$P-n2" >/dev/null
cat >"$WORK/v2.sql" <<'SQL'
CREATE TABLE app_notes (id TEXT NOT NULL PRIMARY KEY, body TEXT NOT NULL DEFAULT '', tags TEXT);
CREATE TABLE app_tags (name TEXT NOT NULL PRIMARY KEY, color TEXT NOT NULL DEFAULT '');
SQL
$I $N1 schema apply "$WORK/v2.sql" >/dev/null
$I $N1 sql --exec "INSERT INTO app_notes (id, body, tags) VALUES ('b', 'while n2 was away', 'x,y')" >/dev/null
$I $N1 sql --exec "INSERT INTO app_tags (name, color) VALUES ('x', 'red')" >/dev/null
$I $N1 sql --exec "UPDATE app_notes SET tags = 'late' WHERE id = 'a'" >/dev/null
sleep 3
docker unpause "$P-n2" >/dev/null
got() { $I $N2 sql "SELECT (SELECT tags FROM app_notes WHERE id = 'b') || '|' || (SELECT tags FROM app_notes WHERE id = 'a') || '|' || (SELECT color FROM app_tags WHERE name = 'x')" 2>&1 | tail -1; }
start=$(date +%s)
for i in $(seq 1 120); do [ "$(got)" = "x,y|late|red" ] && break; sleep 0.5; done
[ "$(got)" = "x,y|late|red" ] || { $I $N2 schema ls; fail "n2 after it came back: $(got)"; }
pass "n2 was paused while n1 added a column and a table and wrote to both; n2 has all of it $(($(date +%s) - start))s after resume"

cat >"$WORK/v3.sql" <<'SQL'
CREATE TABLE app_notes (id TEXT NOT NULL PRIMARY KEY, body TEXT NOT NULL DEFAULT '');
SQL
if $I $N1 schema apply "$WORK/v3.sql" >"$WORK/v3.out" 2>&1; then fail "dropping a column was accepted"; fi
cat >"$WORK/v4.sql" <<'SQL'
CREATE TABLE app_notes (id TEXT NOT NULL PRIMARY KEY, body TEXT NOT NULL DEFAULT '', tags INTEGER);
SQL
if $I $N1 schema apply "$WORK/v4.sql" >"$WORK/v4.out" 2>&1; then fail "changing a column type was accepted"; fi
if $I $N1 sql --exec "DROP TABLE app_notes" >/dev/null 2>&1; then fail "sql --exec dropped a table"; fi
$I $N1 schema ls | grep -q "tags TEXT" || fail "a refused schema replaced the cluster schema"
[ "$($I $N2 sql "SELECT tags FROM app_notes WHERE id = 'b'" | tail -1)" = "x,y" ] || fail "n2 lost column tags"
for i in $(seq 1 20); do $I $N1 schema ls | grep -q '"n2": "ok"' && break; sleep 0.5; done
$I $N1 schema ls | grep -q '"n2": "ok"' || { $I $N1 schema ls; fail "rows do not pass between n1 and n2 after the refused changes"; }
pass "dropping or changing a column is refused where it is applied, and reaches no one: $(head -c 120 "$WORK/v3.out")"

echo "== 21b. a schema conflict: during a split, two machines add different columns to one table"
docker exec -i $P-n1 nft -f - <<EOF
table inet split {
	chain in { type filter hook input priority 0; policy drop; iif lo accept; tcp dport 22 accept; }
	chain out { type filter hook output priority 0; policy drop; oif lo accept; tcp sport 22 accept; }
}
EOF
base="CREATE TABLE app_notes (id TEXT NOT NULL PRIMARY KEY, body TEXT NOT NULL DEFAULT '', tags TEXT"
echo "$base, colour_a TEXT);" >"$WORK/v5a.sql"
echo "$base, colour_b TEXT);" >"$WORK/v5b.sql"
$I $N1 schema apply "$WORK/v5a.sql" >/dev/null
$I $N2 schema apply "$WORK/v5b.sql" >/dev/null
$I $N1 sql --exec "INSERT INTO app_notes (id, colour_a) VALUES ('c', 'red')" >/dev/null
$I $N2 sql --exec "INSERT INTO app_notes (id, colour_b) VALUES ('d', 'blue')" >/dev/null
$I $N1 call kv.put '{"key":"during-conflict","value":"n1"}' >/dev/null
docker exec $P-n1 nft delete table inet split
for i in $(seq 1 40); do $I $N1 schema ls | grep -q '"n2": "schema conflict' && break; sleep 0.5; done
$I $N1 schema ls | grep -q '"n2": "schema conflict' || { $I $N1 schema ls; fail "n1 does not report the conflict with n2"; }
sleep 5
[ "$($I $N2 sql "SELECT count(*) FROM app_notes WHERE id = 'c'" | tail -1)" = 0 ] || fail "a row passed a link with a schema conflict"
pass "after the heal, the link reports the conflict and carries no rows"
echo "$base, colour_a TEXT, colour_b TEXT);" >"$WORK/v6.sql"
$I $N1 schema apply "$WORK/v6.sql" >/dev/null
both() { $I $1 sql "SELECT (SELECT colour_a FROM app_notes WHERE id = 'c') || '|' || (SELECT colour_b FROM app_notes WHERE id = 'd')" 2>/dev/null | tail -1; }
for i in $(seq 1 60); do [ "$(both "$N2")" = "red|blue" ] && [ "$(both "$N1")" = "red|blue" ] && break; sleep 0.5; done
[ "$(both "$N2")" = "red|blue" ] && [ "$(both "$N1")" = "red|blue" ] || { $I $N1 schema ls; fail "after the fix: n1 $(both "$N1"), n2 $(both "$N2")"; }
[ "$($I $N2 call kv.get '{"key":"during-conflict"}')" = '"n1"' ] || fail "kv write from the conflict did not arrive"
$I $N1 schema ls | grep -q '"n2": "ok"' || fail "link not ok after the fix"
pass "a schema with both columns, applied on one machine, ends the conflict; rows from both sides arrive $((i / 2))s later"

echo "== 21c. a compiled table grown at run time survives a restart; a reset machine rejoins"
echo "CREATE TABLE example_notes (who TEXT NOT NULL PRIMARY KEY, text TEXT NOT NULL DEFAULT '', mood TEXT);" >"$WORK/grow.sql"
$I $N1 schema apply "$WORK/grow.sql" >/dev/null
for i in $(seq 1 40); do $I $N2 sql "SELECT mood FROM example_notes" >/dev/null 2>&1 && break; sleep 0.5; done
docker exec $P-n2 systemctl restart imesh
for i in $(seq 1 40); do [ "$($I $N2 call ping 2>/dev/null)" = '"pong"' ] && break; sleep 0.5; done
[ "$($I $N2 call ping)" = '"pong"' ] || { docker exec $P-n2 journalctl -u imesh --no-pager -n 20; fail "n2 did not start again after a compiled table grew"; }
$I $N2 sql "SELECT mood FROM example_notes" >/dev/null || fail "n2 lost column mood after the restart"
pass "n2 restarted with the grown compiled table"
$I machine add "$N3" --name n3b >/dev/null
$I --connect "$N3" call kv.put '{"key":"from-n3b","value":1}' >/dev/null
for i in $(seq 1 40); do [ "$($I $N1 call kv.get '{"key":"from-n3b"}')" = 1 ] && break; sleep 0.5; done
[ "$($I $N1 call kv.get '{"key":"from-n3b"}')" = 1 ] || fail "n3 did not sync after it was reset and added again"
[ "$($I --connect "$N3" call kv.get '{"key":"during-conflict"}')" = '"n1"' ] || fail "n3 did not get the older rows"
pass "n3, reset in stage 14, joined again with a new key and syncs both ways"

echo "== 23. a user daemon comes back after a reboot: from its crontab, or as a systemd user service"
id3() { $I --connect "$N3" call machine.inspect 2>/dev/null | awk -F'"' '/"id"/{print $4; exit}'; }
up3() { for i in $(seq 1 60); do [ "$(id3)" = "$1" ] && return 0; sleep 1; done; return 1; }
ID3=$(id3)
docker restart "$P-n3" >/dev/null
up3 "$ID3" || fail "the detached daemon did not come back after a reboot"
docker exec "$P-n3" pgrep -u ops -f "daemon supervise" >/dev/null || fail "no supervisor after the reboot"
pass "after a reboot, the crontab started the detached daemon again with the same key"
docker exec "$P-n3" loginctl enable-linger ops
$I machine update "$N3" --service systemd >/dev/null
unit3() { docker exec "$P-n3" sh -c 'grep -q "user@.*imesh.service" /proc/$(pgrep -u ops -x imesh | head -1)/cgroup'; }
unit3 || fail "the daemon is not in the user's imesh.service"
docker exec "$P-n3" pgrep -u ops -f "daemon supervise" >/dev/null && fail "the supervisor still runs"
docker exec "$P-n3" su ops -c 'crontab -l' 2>/dev/null | grep -q "daemon start # imesh" && fail "the @reboot line is still there"
docker restart "$P-n3" >/dev/null
up3 "$ID3" || fail "the systemd user service did not come back after a reboot"
unit3 || fail "after the reboot, the daemon is not in the user's imesh.service"
pass "with linger, update --service systemd moved n3 to a user service; it starts at boot with no login"

echo "== 22. the relay exposed through a dev tunnel (a stand-in devtunnel CLI)"
# The stand-in answers the commands imesh runs and reports a local relay URL once
# `devtunnel host` runs. Microsoft's service is not used.
docker cp -q "$ROOT/tests/e2e/fake/devtunnel" "$P-n6:/usr/local/bin/devtunnel"
$I -c udp machine update root@localhost:${PB}06 --serve-relay 127.0.0.1:3340 --relay http://127.0.0.1:3340 --expose-relay devtunnel >/dev/null
FT=/var/lib/imesh/fake-devtunnel
for i in $(seq 1 60); do
	url=$($I -c udp relay ls | awk -F'"' '/"url"/{print $4; exit}')
	[ "$url" = "http://127.0.0.1:3340/" ] && break
	sleep 1
done
[ "$url" = "http://127.0.0.1:3340/" ] || { docker exec "$P-n6" journalctl -u imesh --no-pager -n 20; fail "n6 did not publish a relay URL"; }
docker exec "$P-n6" cat $FT/create-args | grep -q -- "-l imesh -l imesh-" || fail "tunnel created without the imesh labels"
host1=$(docker exec "$P-n6" cat $FT/hosting)
docker exec "$P-n6" kill "$host1"
for i in $(seq 1 30); do
	host2=$(docker exec "$P-n6" cat $FT/hosting 2>/dev/null)
	[ -n "$host2" ] && [ "$host2" != "$host1" ] && docker exec "$P-n6" kill -0 "$host2" 2>/dev/null && break
	sleep 1
done
[ -n "$host2" ] && [ "$host2" != "$host1" ] || fail "the devtunnel host was not started again after it stopped"
docker cp -q "$ROOT/tests/e2e/fake/devtunnel" "$P-n7:/usr/local/bin/devtunnel"
relay_ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$P-n6")
docker exec "$P-n7" sh -c "echo '$relay_ip fake-relay' >> /etc/hosts"
$I -c udp machine update root@localhost:${PB}07 --devtunnel devtunnel:fake-tunnel:3340 --no-n0 --relay-only >/dev/null
connector1=$(docker exec "$P-n7" cat $FT/connecting)
docker exec "$P-n7" kill "$connector1"
for i in $(seq 1 30); do
    connector2=$(docker exec "$P-n7" cat $FT/connecting)
    [ "$connector2" != "$connector1" ] && docker exec "$P-n7" kill -0 "$connector2" 2>/dev/null && break
    sleep 1
done
[ "$connector2" != "$connector1" ] || fail "the devtunnel connector was not restarted"
docker exec "$P-n6" cat $FT/create-args | grep -q -- ' -a' && fail "anonymous tunnel requested"
$I --connect root@localhost:${PB}06 relay set "$url" >/dev/null
$I -c udp call kv.put '{"key":"via-tunnel-url","value":1}' >/dev/null
for i in $(seq 1 40); do [ "$($I --connect root@localhost:${PB}07 call kv.get '{"key":"via-tunnel-url"}')" = 1 ] && break; sleep 0.5; done
[ "$($I --connect root@localhost:${PB}07 call kv.get '{"key":"via-tunnel-url"}')" = 1 ] || fail "n7 did not sync with the tunnel URL as the cluster relay"
pass "n6 exposed its relay through the tunnel ($url), kept the tunnel host running, and n7 syncs through the published URL"

echo "== 24. --docker: the daemon in a sandboxed container, on a host with its own Docker (as a NAS)"
# ops is in n9's docker group and has no sudo. n9's Docker runs inside n9, so host networking
# means n9's own LAN address, and bridge networking means Docker's NAT inside n9.
N9="ops@localhost:${PB}09"
D9() { docker exec "$P-n9" docker "$@"; }
id_of() { $I call machine.inspect -m "$1" 2>/dev/null | awk -F'"' '/"id"/{print $4; exit}'; }
path_of() { $I machine ls | awk -v n="$1" '$1==n{print $3"/"$4}'; }
$I machine add "$N9" --docker --name n9d >/dev/null
[ "$(D9 inspect -f '{{.HostConfig.RestartPolicy.Name}} {{.HostConfig.ReadonlyRootfs}} {{.Config.User}} {{.HostConfig.NetworkMode}} {{.HostConfig.CapDrop}}' imesh)" = "unless-stopped true 65532:65532 host [ALL]" ] ||
	fail "container settings: $(D9 inspect -f '{{.HostConfig.RestartPolicy.Name}} {{.HostConfig.ReadonlyRootfs}} {{.Config.User}} {{.HostConfig.NetworkMode}} {{.HostConfig.CapDrop}}' imesh)"
$I call kv.put '{"key":"to-docker","value":1}' >/dev/null
for i in $(seq 1 40); do [ "$($I call kv.get '{"key":"to-docker"}' -m n9d 2>/dev/null)" = 1 ] && break; sleep 0.5; done
[ "$($I call kv.get '{"key":"to-docker"}' -m n9d)" = 1 ] || fail "n9d did not sync"
for i in $(seq 1 30); do [ "$(path_of n9d)" = up/direct ] && break; sleep 1; done
[ "$(path_of n9d)" = up/direct ] || fail "n1 reaches n9d as $(path_of n9d), not up/direct"
idd=$(id_of n9d)
D9 restart imesh >/dev/null
for i in $(seq 1 60); do [ "$(id_of n9d)" = "$idd" ] && break; sleep 1; done
[ "$(id_of n9d)" = "$idd" ] || fail "n9d lost its key after a container restart"
pass "host network: n9d runs read-only as 65532 with no capabilities, syncs, is up/direct from n1 on the same LAN, and keeps its key over a restart"
$I machine update "$N9" --docker --label where=docker >/dev/null
D9 inspect -f '{{range .Config.Env}}{{println .}}{{end}}' imesh | grep -q '^IMESH_LABELS=where=docker$' || fail "update did not set the label"
for i in $(seq 1 60); do [ "$(id_of n9d)" = "$idd" ] && break; sleep 1; done
[ "$(id_of n9d)" = "$idd" ] || fail "update --docker changed the key"
pass "machine update --docker replaced the container with the same key and a new label"
$I machine add "$N9" --docker --docker-name imesh-b --docker-network bridge --name n9b >/dev/null
$I call kv.put '{"key":"to-bridge","value":1}' >/dev/null
for i in $(seq 1 40); do [ "$($I call kv.get '{"key":"to-bridge"}' -m n9b 2>/dev/null)" = 1 ] && break; sleep 0.5; done
[ "$($I call kv.get '{"key":"to-bridge"}' -m n9b)" = 1 ] || fail "n9b did not sync"
for i in $(seq 1 30); do [ "$(path_of n9b)" = up/direct ] && break; sleep 1; done
[ "$(path_of n9b)" = up/direct ] || fail "n1 reaches n9b as $(path_of n9b), not up/direct"
pass "bridge network behind Docker's NAT: n9b syncs, and n1 on the same LAN reaches it up/direct"
$I machine add "$N9" --docker --docker-name imesh-r --docker-network bridge --relay-only --name n9r >/dev/null
$I call kv.put '{"key":"to-relay-only","value":1}' >/dev/null
for i in $(seq 1 60); do [ "$($I call kv.get '{"key":"to-relay-only"}' -m n9r 2>/dev/null)" = 1 ] && break; sleep 0.5; done
[ "$($I call kv.get '{"key":"to-relay-only"}' -m n9r)" = 1 ] || fail "n9r did not sync"
[ "$(path_of n9r)" = up/relay ] || fail "n1 reaches the relay-only n9r as $(path_of n9r), not up/relay"
# The container has its own network namespace: every UDP socket there is the daemon's. IPv4
# addresses in /proc are little-endian hex, so 127.x.y.z ends in 7F.
pid=$(D9 inspect -f '{{.State.Pid}}' imesh-r)
open=$(docker exec "$P-n9" sh -c "cat /proc/$pid/net/udp /proc/$pid/net/udp6" | awk '$2 ~ /:/ && !(length($2) == 13 && substr($2, 7, 2) == "7F") {print $2}')
[ -z "$open" ] || fail "relay-only n9r has UDP sockets off loopback: $open"
pass "--relay-only: n9r opens no UDP port but loopback, syncs, and n1 reaches it through the relay"
$I machine rm n9d >/dev/null
$I machine rm n9b >/dev/null
$I machine rm n9r >/dev/null
$I machine ls | grep -q '^n9[dbr] ' && fail "n9d, n9b, or n9r still listed"
pass "machine rm removed the containers' machines"

echo "== 16. footprint"
for n in n1 n4; do
	docker exec "$P-$n" sh -c 'pid=$(systemctl show -p MainPID --value imesh); grep -E "VmRSS|Threads" /proc/$pid/status | tr -s " \t" " " | tr "\n" " "'
	echo
done
ls -l "$TARGET/$T/release/$OUT" | awk '{printf "binary %.1f MB\n", $5/1048576}'

echo "ALL PASS"
