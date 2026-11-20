#!/usr/bin/env bash
# End-to-end proof of imesh on Linux. Needs Docker and cargo-zigbuild. Runs on an arm64 or x86_64 host.
# Usage: tests/e2e/linux.sh                                  all scenarios against the iroh-mesh-cli example app
#        KEEP=1 tests/e2e/linux.sh                           leave containers running afterwards
#        PREFIX=b PORTS=24 tests/e2e/linux.sh                container name prefix and SSH port block (24 gives 2401-2405)
set -euo pipefail

ROOT=${ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}
CRATE=$ROOT/${CRATE:-crates/iroh-mesh-cli}
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
# Test the example app, built as any app is (iroh_mesh_cli::run plus its own ALPN and methods), when
# the crate has one; a prototype without it uses its plain binary.
if [ -f "$CRATE/examples/app.rs" ]; then BUILD="--example app"; OUT=examples/app; else BUILD=; OUT=imesh; fi
# The CLI under test runs from the bundle, which it finds beside itself. A full bundle has every
# target; BUNDLE_TARGETS="--target <triple> ..." builds a partial one for a faster run.
if [ "$CRATE" = "$ROOT/crates/iroh-mesh-cli" ]; then
	(cd "$ROOT" && cargo x build ${BUILD:+$BUILD} ${BUNDLE_TARGETS:-})
	I=$ROOT/dist/imesh/imesh
	BIN=$ROOT/dist/imesh/targets/$T/imesh
else
	# A prototype has its own CLI, which still reads IMESH_BINARY_<ARCH>.
	(cd "$CRATE" && cargo build -q $BUILD && cargo zigbuild -q --release --target "$T" $BUILD)
	I=$TARGET/debug/$OUT
	BIN=$TARGET/$T/release/$OUT
	export "IMESH_BINARY_$A=$BIN"
fi
export IMESH_SOCKET=$WORK/no-local-daemon.sock IMESH_CONFIG_DIR=$WORK/cfg IMESH_SSH_CONFIG=$WORK/ssh_config IMESH_SSH_DIR=$WORK/dotssh IMESH_LOG=warn

docker build -q -t imesh-test-node "$ROOT/tests/e2e/docker" >/dev/null
ssh-keygen -q -t ed25519 -N '' -f "$WORK/id" -C imesh-e2e
cat >"$WORK/ssh_config" <<EOF
Host localhost
  UserKnownHostsFile $WORK/known_hosts
  StrictHostKeyChecking accept-new
  IdentityFile $WORK/id
  IdentitiesOnly yes
EOF

NODES="n1 n2 n3 n4 n5 n6 n7 n8 n9 n10"
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
# n10 is reached only through a stand-in devtunnel in stage 25.
node n10 lan 10
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
# One context per machine, for commands that must reach that machine's own daemon.
$I ctx add n1 "root@localhost:${PB}01" >/dev/null
$I ctx add n1d "deploy@localhost:${PB}01" >/dev/null
$I ctx add n2 "root@localhost:${PB}02" >/dev/null
$I ctx add n3 "$N3" >/dev/null
$I ctx add n6 "root@localhost:${PB}06" >/dev/null
$I ctx add n7 "root@localhost:${PB}07" >/dev/null
$I ctx add n8 "ops@localhost:${PB}08" >/dev/null

echo "== 1. add --new installs, starts the daemon, creates a cluster, writes a context"
t0=$(date +%s)
out=$($I add root@localhost:${PB}01 --new --relay n0)
[ "$(docker exec $P-n1 systemctl is-active imesh)" = active ] || fail "daemon not active"
grep -q 'current_context = "default"' "$WORK/cfg/config.toml" || fail "no default context"
CODE=$(echo "$out" | awk '$1=="recovery" && $2=="code:"{print $3}')
[ -n "$CODE" ] || fail "add --new printed no recovery code: $out"
pass "add --new took $(($(date +%s) - t0))s, daemon active, context default, recovery code printed"

echo "== 2. add --new again is refused"
$I add root@localhost:${PB}01 --new >/dev/null 2>&1 && fail "second add --new allowed" || pass "second add --new refused"

echo "== 3. add; a user install needs no sudo; a root login gets a system service"
$I add root@localhost:${PB}02 --run-as imeshd >/dev/null
$I add "$N3" >/dev/null
[ "$(docker exec "$P-n3" stat -c '%U %a' /home/ops/.local/share/imesh/imesh.sock)" = "ops 600" ] || fail "n3 socket is not owner-only in the user's home"
[ "$(docker exec "$P-n3" ps -o user= -C imesh | sort -u)" = ops ] || fail "n3 daemon not running as ops"
docker exec "$P-n3" test ! -e /etc/imesh || fail "a user install wrote /etc/imesh"
docker exec "$P-n3" su ops -c 'crontab -l' | grep -qF "@reboot '/home/ops/.local/share/imesh/bin/imesh' service start" || fail "n3 has no @reboot line"
[ "$(docker exec "$P-n2" ps -o user= -C imesh | sort -u)" = imeshd ] || fail "n2 daemon not running as imeshd"
[ "$(docker exec "$P-n1" ps -o user= -C imesh | sort -u)" = imesh ] || fail "a root login should run the daemon as imesh"
pass "n3 runs as ops with no sudo (~/.local, socket 600, @reboot in its crontab); n1 as imesh (root login), n2 as imeshd (--run-as)"

echo "== 4. every machine sees every other machine up"
for i in $(seq 1 30); do
	up=$($I status | awk 'NR>1 && $4=="up"' | wc -l | tr -d ' ')
	[ "$up" = 3 ] && break
	sleep 1
done
[ "$up" = 3 ] || { $I status; fail "not all up"; }
$I status
roles() { $I "$@" status --jq '.[] | .name + "=" + .role' | sort | tr '\n' ' '; }
[ "$(roles)" = "n1=owner n2=member n3=member " ] || fail "roles: $(roles)"
for n in n2 n3; do [ "$(roles -c $n)" = "n1=owner n2=member n3=member " ] || fail "roles on $n: $(roles -c $n)"; done
pass "all 3 up from node1's view; every machine sees n1 as the owner and n2, n3 as members"

echo "== 5. the CLI reaches a user daemon over ssh"
[ "$($I -c n3 dev call ping)" = '"pong"' ] || fail "ops ping"
pass "ops@n3 reaches its own daemon over dial-stdio"

echo "== 7. one machine forwards a call to another"
name=$($I -c n1 dev call machine.inspect -m n3 | awk '/"name"/ && !n++')
echo "$name" | grep -q n3 || fail "forward: $name"
pass "call through n1 served by n3"

echo "== 7b. an app ALPN between two machines, driven by an app API method"
ID2=$($I -c n2 dev call machine.inspect | awk -F'"' '/"id"/ && !n++ {print $4}')
out=$($I -c n1 dev call example.dial "{\"to\":\"$ID2\",\"text\":\"hi n2\"}" 2>&1 || true)
echo "$out" | grep -q '"echo": "hi n2"' || fail "example ALPN: $out"
pass "n1 opened imesh-example/echo/0 to n2"

echo "== 7c. a plain member cannot remove another member"
out=$($I -c n3 rm n2 2>&1) && fail "plain member n3 removed n2: $out"
echo "$out" | grep -q "run this on n1" || fail "the refusal does not name the owner: $out"
[ "$($I -c n1 status --jq '.[] | select(.name == "n2") | .status.state')" = up ] || fail "n2 is not up after the refused rm"
[ "$($I -c n2 dev call machine.inspect | awk -F'"' '/"id"/ && !n++ {print $4}')" = "$ID2" ] || fail "the refused rm reset n2"
pass "n3 rm n2 refused ($(echo "$out" | tail -1)); n2 stays up with the same key"

echo "== 9. a machine goes down; the rest keep working; it comes back up"
state_of() { $I -c "$1" status --jq ".[] | select(.name == \"$2\") | .status.state"; }
docker pause $P-n2 >/dev/null
t0=$(date +%s)
[ "$(state_of n1 n2)" = down ] || { $I -c n1 status -w; fail "n2 not reported down"; }
[ "$(state_of n1 n3)" = up ] || fail "n3 not up while n2 is down"
[ "$(state_of n3 n1)" = up ] || fail "n1 not up in n3's view while n2 is down"
pass "n2 reported down by one status probe after $(($(date +%s) - t0))s; n1 and n3 still see each other up"
docker unpause $P-n2 >/dev/null
t0=$(date +%s)
until [ "$(state_of n1 n2)" = up ]; do
	[ $(($(date +%s) - t0)) -gt 30 ] && fail "n2 not up after resume"
	sleep 1
done
pass "n2 up $(($(date +%s) - t0))s after resume"

echo "== 10. a service restart keeps identity and state"
id_before=$($I -c n2 dev call machine.inspect | awk '/"id"/ && !n++')
docker exec $P-n2 systemctl restart imesh
sleep 1
[ "$($I -c n2 dev call machine.inspect | awk '/"id"/ && !n++')" = "$id_before" ] || fail "identity changed"
[ "$(roles -c n2)" = "n1=owner n2=member n3=member " ] || fail "roster lost: $(roles -c n2)"
pass "same id and roster after restart"

echo "== 11. contexts: a second context for the same cluster"
cp "$WORK/cfg/config.toml" "$WORK/config.before"
$I ctx add remote "root@localhost:${PB}02" "root@localhost:${PB}01" >/dev/null
$I ctx use remote >/dev/null
[ "$($I dev call ping)" = '"pong"' ] || fail "remote context"
$I ctx ls
$I ctx conn 1 >/dev/null
head -1 <<<"$($I ctx conn)" | grep -q "^0  ssh://root@localhost:${PB}01" || fail "ctx conn did not move entry to top"
$I ctx use default >/dev/null
IMESH_CONTEXT=remote $I dev call ping >/dev/null || fail "IMESH_CONTEXT"
pass "ctx use, ctx conn reorder, IMESH_CONTEXT"

echo "== 12. a dead connection falls through to the next one"
python3 - "$WORK/cfg/config.toml" "$PB" <<'EOF'
import sys
p, pb = sys.argv[1], sys.argv[2]
s = open(p).read()
s = s.replace('[[contexts.default.connections]]', f'[[contexts.default.connections]]\nssh = "root@localhost:{pb}99"\n\n[[contexts.default.connections]]', 1)
open(p, "w").write(s)
EOF
[ "$($I dev call ping)" = '"pong"' ] || fail "fallback"
pass "dead ssh entry skipped"
cp "$WORK/config.before" "$WORK/cfg/config.toml"

echo "== 14. rm resets the machine and revokes it"
ID3=$($I -c n3 dev call machine.inspect | awk -F'"' '/"id"/ && !n++ {print $4}')
$I rm n3 >/dev/null
$I status | grep -q '^n3 ' && fail "n3 still listed"
for i in $(seq 1 30); do
	new3=$($I -c n3 dev call machine.inspect 2>/dev/null | awk -F'"' '/"id"/ && !n++ {print $4}' || true)
	[ -n "$new3" ] && [ "$new3" != "$ID3" ] && break
	sleep 0.5
done
[ -n "$new3" ] && [ "$new3" != "$ID3" ] || fail "n3 kept its identity"
$I -c n3 dev call machine.inspect | grep -q '"cluster": null' || fail "n3 kept cluster state"
pass "n3 removed, reset with a new identity, out of the cluster"

echo "== 14b. logout leaves the cluster first, then deletes the data"
$I add "$N3" --name n3p >/dev/null
$I status | grep -q '^n3p ' || fail "n3p did not join"
out=$(docker exec -u ops "$P-n3" /home/ops/.local/share/imesh/bin/imesh logout) || fail "logout failed"
echo "$out" | grep -q "left the cluster; removed by" || fail "logout did not leave the cluster: $out"
for i in $(seq 1 30); do $I status | grep -q '^n3p ' || break; sleep 0.5; done
$I status | grep -q '^n3p ' && fail "the cluster still lists n3p after it logged out"
docker exec "$P-n3" test ! -e /home/ops/.local/share/imesh/secret.key || fail "logout kept the key"
pass "logout removed n3p from the cluster before it deleted its data"

echo "== 15. self-hosted relay, no n0, no direct path between two networks"
relay_ip=$(docker exec "$P-n4" getent hosts "$P-relay" | cut -d" " -f1)
docker exec "$P-n4" timeout 2 bash -c "</dev/tcp/$relay_ip/3340" || fail "relay not reachable"
$I -c relayonly add root@localhost:${PB}04 --new --relay http://$P-relay:3340 >/dev/null
$I ctx use default >/dev/null
$I -c relayonly add root@localhost:${PB}05 >/dev/null
for i in $(seq 1 30); do
	p=$($I -c relayonly status | awk '$1=="n5"{print $4"/"$5}')
	[ "$p" = up/relay ] && break
	sleep 1
done
$I -c relayonly status
[ "$p" = up/relay ] || fail "n5 not up over relay"
pass "n4 and n5 connected only through the self-hosted relay"

echo "== 18. all UDP blocked on one machine: it joins and works through a members-only relay served by another machine"
$I -c udp add root@localhost:${PB}06 --new --serve-relay 0.0.0.0:3340 --relay "http://$P-n6:3340" >/dev/null
$I ctx use default >/dev/null
docker exec -i "$P-n7" nft -f - <<EOF
table inet noudp {
	chain in { type filter hook input priority 0; iif != lo meta l4proto udp drop; }
	chain out { type filter hook output priority 0; oif != lo meta l4proto udp drop; }
}
EOF
$I -c udp add root@localhost:${PB}07 >/dev/null
for i in $(seq 1 30); do
	p=$($I -c udp status | awk '$1=="n7"{print $4"/"$5}')
	[ "$p" = up/relay ] && break
	sleep 1
done
$I -c udp status
[ "$p" = up/relay ] || fail "n7 not up over the relay on n6"
[ "$($I -c n7 status | awk '$1=="n6"{print $4"/"$5}')" = up/relay ] || fail "n6 not up/relay in n7's view"
docker exec "$P-n6" journalctl -u imesh --no-pager | grep -q "relay serving members" || fail "relay not started on n6"
pass "n7 with no UDP reached n6 only through n6's own relay, both ways"

echo "== 19. a server that ran up alone, in its own empty cluster, is reset by add and joins"
docker cp -q "$BIN" "$P-n8:/usr/local/bin/imesh-setup"
docker exec -u ops -w /home/ops "$P-n8" /usr/local/bin/imesh-setup up --relay n0 --service detached >/dev/null
alone=$($I -c n8 dev call machine.inspect)
echo "$alone" | grep -q '"alone": true' || fail "n8 is not alone in its cluster: $alone"
old8=$(echo "$alone" | awk -F'"' '/"id"/ && !n++ {print $4}')
$I add "ops@localhost:${PB}08" --name n8 >/dev/null
for i in $(seq 1 30); do
	s=$($I status | awk '$1=="n8"{print $4}')
	[ "$s" = up ] && break
	sleep 1
done
[ "$s" = up ] || { $I status; fail "n8 not up"; }
[ "$($I -c n8 dev call machine.inspect | awk -F'"' '/"id"/ && !n++ {print $4}')" != "$old8" ] || fail "n8 kept the key of its own cluster"
[ "$($I -c n8 status --jq '.[] | select(.name == "n1") | .role + "/" + .status.state')" = owner/up ] || fail "n8 does not see n1 as an owner that is up"
pass "n8 left its own empty cluster, joined with a new key, and sees the owner"

echo "== 20. review fixes: forwarding, durable writes, safe re-runs, update"
[ "$($I -c n1 dev call whoami -m n2 | awk -F'"' '/"caller"/{print $4}')" = member ] || fail "forward from the local socket"
pass "a call forwarded from the local socket runs as a member"

docker exec -i "$P-n1" nft -f - <<EOF
table inet solo {
	chain in { type filter hook input priority 0; policy drop; iif lo accept; tcp dport 22 accept; }
	chain out { type filter hook output priority 0; policy drop; oif lo accept; tcp sport 22 accept; }
}
EOF
# The member names in a machine's own roster, with no probe.
names_on() { $I -c "$1" dev call cluster.machines 2>/dev/null | python3 -c 'import json,sys; t=sys.stdin.read(); print(" ".join(sorted(r["name"] for r in json.loads(t))) if t.strip() else "", end=" ")' || true; }
$I -c n1 rename n2 n2r >/dev/null
docker exec "$P-n1" pkill -9 -f 'imesh service run'
for i in $(seq 1 40); do
	v=$(names_on n1)
	case " $v" in *" n2r "*) break ;; esac
	sleep 0.5
done
docker exec "$P-n1" nft delete table inet solo
case " $v" in *" n2r "*) ;; *) fail "a rename acknowledged while cut off was lost after kill -9: $v" ;; esac
$I -c n1 rename n2r n2 >/dev/null
for i in $(seq 1 20); do case " $(names_on n2)" in *" n2 "*) break ;; esac; sleep 0.5; done
case " $(names_on n2)" in *" n2 "*) ;; *) fail "the rename back did not reach n2: $(names_on n2)" ;; esac
pass "a roster change signed while cut off survives kill -9 of its only holder"

pid=$(docker exec "$P-n1" systemctl show -p MainPID --value imesh)
env_before=$(docker exec "$P-n1" cat /etc/imesh/env)
$I add "root@localhost:${PB}01" --new --serve-relay 0.0.0.0:3999 >/dev/null 2>&1 && fail "second add --new allowed"
[ "$(docker exec "$P-n1" systemctl show -p MainPID --value imesh)" = "$pid" ] || fail "refused add --new restarted the daemon"
[ "$(docker exec "$P-n1" cat /etc/imesh/env)" = "$env_before" ] || fail "refused add --new changed settings"
pass "a refused add --new leaves the daemon and its settings alone"

env4=$(docker exec "$P-n4" cat /etc/imesh/env)
$I -c relayonly update server "root@localhost:${PB}04" --label role=gateway >/dev/null
docker exec "$P-n4" grep -q "IMESH_RELAYS=.*n0" /etc/imesh/env && fail "update turned on n0"
docker exec "$P-n4" grep -q 'IMESH_LABELS=role=gateway' /etc/imesh/env || fail "update did not add the label"
for i in $(seq 1 30); do
	p=$($I -c relayonly status | awk '$1=="n5"{print $4"/"$5}')
	[ "$p" = up/relay ] && break
	sleep 1
done
[ "$p" = up/relay ] || fail "relay-only pair broke after update"
pass "update server kept the relay settings, added a label, and the relay-only pair stayed up"

old8=$($I -c n8 dev call machine.inspect | awk -F'"' '/"id"/ && !n++ {print $4}')
$I rm n8 >/dev/null
for i in $(seq 1 30); do
	new8=$($I -c n8 dev call machine.inspect 2>/dev/null | awk -F'"' '/"id"/ && !n++ {print $4}' || true)
	[ -n "$new8" ] && [ "$new8" != "$old8" ] && break
	sleep 0.5
done
[ -n "$new8" ] && [ "$new8" != "$old8" ] || fail "rm did not reset n8"
pass "rm reset n8"

$I update server "deploy@localhost:${PB}01" --system --run-as imeshsvc >/dev/null
[ "$(docker exec "$P-n1" ps -o user= -C imesh | sort -u)" = imeshsvc ] || fail "n1 daemon not running as imeshsvc"
[ "$(docker exec "$P-n1" stat -c '%G %a' /run/imesh/imesh.sock)" = "imesh 660" ] || fail "n1 socket is not imesh 660"
[ "$($I -c n1d dev call ping)" = '"pong"' ] || fail "deploy cannot reach the socket through the group"
pass "after update --system --run-as imeshsvc, deploy reaches the socket through the imesh group"

echo "== 21. the owner promotes n2; the admin n2 adds a machine"
$I -c n2 promote n2 >/dev/null 2>&1 && fail "a plain member promoted itself"
$I promote n2 >/dev/null
for i in $(seq 1 20); do [ "$($I -c n2 status --jq '.[] | select(.name == "n2") | .role')" = admin ] && break; sleep 0.5; done
[ "$($I -c n2 status --jq '.[] | select(.name == "n2") | .role')" = admin ] || fail "n2 does not see itself as admin"
$I -c n2 add "$N3" --name n3b >/dev/null
for c in n1 n2 n3; do
	[ "$(roles -c $c)" = "n1=owner n2=admin n3b=member " ] || fail "roles on $c after n2 added n3b: $(roles -c $c)"
done
pass "n2 is an admin and added n3b; n1, n2, and n3b agree on every role"

echo "== 21b. a network split: each side changes the roster, and every machine agrees after it heals"
# n1, the owner, is cut off from everything but the Docker gateway: the CLI still reaches it over
# SSH, but it reaches neither n2, the admin, nor n3b, nor the n0 relay.
gw=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.Gateway}}{{end}}' "$P-n1")
docker exec -i "$P-n1" nft -f - <<EOF
table inet split {
	chain in { type filter hook input priority 0; iif lo accept; ip saddr $gw accept; drop; }
	chain out { type filter hook output priority 0; oif lo accept; ip daddr $gw accept; drop; }
}
EOF
newid() { openssl genpkey -algorithm ed25519 | openssl pkey -pubout -outform DER | tail -c 32 | od -An -tx1 | tr -d ' \n'; }
G1=$(newid) G2=$(newid)
$I -c n1 dev call cluster.add "{\"id\":\"$G1\",\"name\":\"ghost1\"}" >/dev/null
$I -c n2 dev call cluster.add "{\"id\":\"$G2\",\"name\":\"ghost2\"}" >/dev/null
$I -c n1 rename n3b n3left >/dev/null
$I -c n2 rename n3b n3right >/dev/null
[ "$(state_of n2 n1)" = up ] && fail "n2 reaches n1 during the split"
roles -c n1 | grep -q ghost2 && fail "n1 knows ghost2 during the split"
roles -c n2 | grep -q ghost1 && fail "n2 knows ghost1 during the split"
docker exec "$P-n1" nft delete table inet split
# No member is told when the split heals: the retries after the failed hellos, and these status
# probes, bring the sides together.
for i in $(seq 1 90); do
	a=$(roles -c n1) b=$(roles -c n2) c=$(roles -c n3)
	[ "$a" = "$b" ] && [ "$b" = "$c" ] && break
	sleep 1
done
[ "$a" = "$b" ] && [ "$b" = "$c" ] || fail "after the split: n1 [$a], n2 [$b], n3b [$c]"
case $a in *ghost1=member*ghost2=member*) ;; *) fail "a change from one side was lost: $a" ;; esac
case $a in *n3left=member* | *n3right=member*) ;; *) fail "n3b has neither name from the split: $a" ;; esac
healed=$((i))
n3now=$(echo "$a" | tr ' ' '\n' | grep -E '^n3(left|right)=' | cut -d= -f1)
$I -c n1 dev call cluster.remove "{\"id\":\"$G1\"}" >/dev/null
$I -c n1 dev call cluster.remove "{\"id\":\"$G2\"}" >/dev/null
$I -c n1 rename "$n3now" n3b >/dev/null
for c in n1 n2 n3; do
	for i in $(seq 1 30); do [ "$(roles -c $c)" = "n1=owner n2=admin n3b=member " ] && break; sleep 1; done
	[ "$(roles -c $c)" = "n1=owner n2=admin n3b=member " ] || fail "roles on $c after the cleanup: $(roles -c $c)"
done
pass "n1 and n2 each admitted a machine and renamed n3b during a split; ${healed}s after it healed, every machine had both admissions and the same name ($n3now) for n3b"

echo "== 23. a user daemon comes back after a reboot: from its crontab, or as a systemd user service"
id3() { $I -c n3 dev call machine.inspect 2>/dev/null | awk -F'"' '/"id"/ && !n++ {print $4}'; }
up3() { for i in $(seq 1 60); do [ "$(id3)" = "$1" ] && return 0; sleep 1; done; return 1; }
ID3=$(id3)
docker restart "$P-n3" >/dev/null
up3 "$ID3" || fail "the detached daemon did not come back after a reboot"
docker exec "$P-n3" pgrep -u ops -f "service supervise" >/dev/null || fail "no supervisor after the reboot"
pass "after a reboot, the crontab started the detached daemon again with the same key"
docker exec "$P-n3" loginctl enable-linger ops
$I update server "$N3" --service systemd >/dev/null
unit3() { docker exec "$P-n3" sh -c 'grep -q "user@.*imesh.service" /proc/$(pgrep -u ops -x imesh | head -1)/cgroup'; }
unit3 || fail "the daemon is not in the user's imesh.service"
docker exec "$P-n3" pgrep -u ops -f "service supervise" >/dev/null && fail "the supervisor still runs"
docker exec "$P-n3" su ops -c 'crontab -l' 2>/dev/null | grep -q "service start # imesh" && fail "the @reboot line is still there"
docker restart "$P-n3" >/dev/null
up3 "$ID3" || fail "the systemd user service did not come back after a reboot"
unit3 || fail "after the reboot, the daemon is not in the user's imesh.service"
pass "with linger, update --service systemd moved n3 to a user service; it starts at boot with no login"

echo "== 26. a machine removed while it is paused reports removed at its first hello after it comes back"
id_on() { $I -c "$1" dev call machine.inspect | awk -F'"' '/"id"/ && !n++ {print $4}'; }
removed_on() { $I -c "$1" dev call machine.inspect 2>/dev/null | grep -q '"removed": true'; }
$I add "ops@localhost:${PB}08" --name n8b >/dev/null
[ "$(state_of n1 n8b)" = up ] || fail "n8b not up"
docker pause "$P-n8" >/dev/null
t0=$(date +%s)
$I rm n8b >"$WORK/rm8.out" 2>&1 || { cat "$WORK/rm8.out"; fail "rm of the paused n8b failed"; }
took=$(($(date +%s) - t0))
for c in n1 n2 n3; do $I -c $c status | grep -q '^n8b ' && fail "$c still lists n8b"; done
docker unpause "$P-n8" >/dev/null
t0=$(date +%s)
# Machines send nothing when idle, so the removed machine learns at its next hello: a start, a
# change, the 10-minute resync, or a status probe, as here.
$I -c n8 status >/dev/null 2>&1 || true
until removed_on n8; do
	if [ $(($(date +%s) - t0)) -gt 30 ]; then
		$I -c n8 dev call machine.inspect
		docker exec "$P-n8" sh -c 'tail -n 30 /home/ops/.local/share/imesh/*.log' | sed 's/\x1b\[[0-9;]*m//g' | cut -c1-300
		fail "n8b does not report removed 30s after resume"
	fi
	sleep 1
done
pass "rm of the paused n8b took ${took}s; no member lists it; it reports removed $(($(date +%s) - t0))s after resume"

echo "== 27. n2 is removed while n3b is paused; n3b drops n2 at the first hello after it comes back, and refuses it"
ID3=$(id_on n3)
docker pause "$P-n3" >/dev/null
# cluster.remove, not rm: n2 keeps its key, so it can try n3b afterwards.
$I dev call cluster.remove "{\"id\":\"$ID2\"}" >/dev/null
[ "$(roles)" = "n1=owner n3b=member " ] || fail "roles on n1 after the removal: $(roles)"
docker unpause "$P-n3" >/dev/null
lists_n2() { $I -c n3 dev call cluster.machines | grep -q '"name": "n2"'; }
# The first hello may come from a member that changed its roster since, or from this probe.
t0=$(date +%s)
$I -c n3 status >/dev/null
until ! lists_n2; do
	[ $(($(date +%s) - t0)) -gt 30 ] && { $I -c n3 status; fail "n3b still lists n2 30s after it came back"; }
	sleep 1
done
dropped=$(($(date +%s) - t0))
out=$($I -c n2 dev call example.dial "{\"to\":\"$ID3\",\"text\":\"x\"}" 2>&1) && fail "n3b accepted the removed n2: $out"
echo "$out" | grep -q 4031 || fail "n3b refused n2 without the removed code: $out"
$I -c n2 status >/dev/null 2>&1 || true
removed_on n2 || { $I -c n2 dev call machine.inspect; fail "n2 does not report removed"; }
pass "n3b dropped n2 ${dropped}s after it came back and refused it with 4031; n2 reports removed"

echo "== 22. the relay exposed through a dev tunnel (a stand-in devtunnel CLI)"
# The stand-in answers the commands imesh runs and reports a local relay URL once
# `devtunnel host` runs. Microsoft's service is not used.
docker cp -q "$ROOT/tests/e2e/fake/devtunnel" "$P-n6:/usr/local/bin/devtunnel"
$I -c udp update server root@localhost:${PB}06 --serve-relay 127.0.0.1:3340 --relay http://127.0.0.1:3340 --expose-relay devtunnel >/dev/null
FT=/var/lib/imesh/fake-devtunnel
# Keep the previous relay address reachable until both peers learn the new one.
relay_ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$P-n6")
docker exec -d "$P-n6" sh -c "echo \$\$ >/tmp/imesh-relay-migration.pid; exec socat TCP-LISTEN:3340,bind=$relay_ip,reuseaddr,fork TCP:127.0.0.1:3340"
for i in $(seq 1 60); do
	url=$($I -c udp relay ls | awk -F'"' '/"url"/ && !n++ {print $4}')
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
$I -c udp update server root@localhost:${PB}07 --devtunnel devtunnel:fake-tunnel:3340 --relay-only >/dev/null
connector1=$(docker exec "$P-n7" cat $FT/connecting)
docker exec "$P-n7" kill "$connector1"
for i in $(seq 1 30); do
    connector2=$(docker exec "$P-n7" cat $FT/connecting)
    [ "$connector2" != "$connector1" ] && docker exec "$P-n7" kill -0 "$connector2" 2>/dev/null && break
    sleep 1
done
[ "$connector2" != "$connector1" ] || fail "the devtunnel connector was not restarted"
docker exec "$P-n6" cat $FT/create-args | grep -q -- ' -a' && fail "anonymous tunnel requested"
# The old relay stopped when n6 moved its listener to loopback. Both members are
# disconnected during this cutover, so owner n6 sets the new address through its local API.
$I -c n6 relay set "$url" >/dev/null
# Only the owner and admins set the cluster relays. n7 reaches the relay through its own tunnel
# and takes n6's setting at its first hello.
$I -c n7 relay set "$url" >/dev/null 2>&1 && fail "plain member n7 set the cluster relays"
for i in $(seq 1 120); do [ "$($I -c n7 status | awk '$1=="n6"{print $4}')" = up ] && break; sleep 0.5; done
[ "$($I -c n7 status | awk '$1=="n6"{print $4}')" = up ] || {
    docker exec "$P-n6" journalctl -u imesh --no-pager -n 45
    docker exec "$P-n7" journalctl -u imesh --no-pager -n 45
    fail "n7 does not reach n6 with the tunnel URL as the cluster relay"
}
docker exec "$P-n6" sh -c 'kill "$(cat /tmp/imesh-relay-migration.pid)"'
docker exec "$P-n7" systemctl restart imesh
for i in $(seq 1 120); do [ "$($I -c n7 status | awk '$1=="n6"{print $4}')" = up ] && break; sleep 0.5; done
[ "$($I -c n7 status | awk '$1=="n6"{print $4}')" = up ] || fail "n7 needs the removed relay migration bridge"
for i in $(seq 1 120); do [ "$($I -c udp status | awk '$1=="n7"{print $4}')" = up ] && break; sleep 0.5; done
[ "$($I -c udp status | awk '$1=="n7"{print $4}')" = up ] || fail "n6 does not reach n7 after the migration"
pass "n6 exposed its relay through the tunnel ($url), kept the tunnel host running, and n6 and n7 reach each other through the published URL"

echo "== 24. --docker: the daemon in a sandboxed container, on a host with its own Docker (as a NAS)"
# ops is in n9's docker group and has no sudo. n9's Docker runs inside n9, so host networking
# means n9's own LAN address, and bridge networking means Docker's NAT inside n9.
N9="ops@localhost:${PB}09"
D9() { docker exec "$P-n9" docker "$@"; }
id_of() { $I dev call machine.inspect -m "$1" 2>/dev/null | awk -F'"' '/"id"/ && !n++ {print $4}'; }
path_of() { $I status | awk -v n="$1" '$1==n{print $4"/"$5}'; }
# The machine's own view of the cluster: the roster reached it.
sees_n1() { $I dev call cluster.machines -m "$1" 2>/dev/null | grep -q '"name": "n1"'; }
$I add "$N9" --docker --name n9d >/dev/null
[ "$(D9 inspect -f '{{.HostConfig.RestartPolicy.Name}} {{.HostConfig.ReadonlyRootfs}} {{.Config.User}} {{.HostConfig.NetworkMode}} {{.HostConfig.CapDrop}}' imesh)" = "unless-stopped true 65532:65532 host [ALL]" ] ||
	fail "container settings: $(D9 inspect -f '{{.HostConfig.RestartPolicy.Name}} {{.HostConfig.ReadonlyRootfs}} {{.Config.User}} {{.HostConfig.NetworkMode}} {{.HostConfig.CapDrop}}' imesh)"
for i in $(seq 1 40); do sees_n1 n9d && break; sleep 0.5; done
sees_n1 n9d || fail "n9d has no roster with n1"
for i in $(seq 1 30); do [ "$(path_of n9d)" = up/direct ] && break; sleep 1; done
[ "$(path_of n9d)" = up/direct ] || fail "n1 reaches n9d as $(path_of n9d), not up/direct"
idd=$(id_of n9d)
D9 restart imesh >/dev/null
for i in $(seq 1 60); do [ "$(id_of n9d)" = "$idd" ] && break; sleep 1; done
[ "$(id_of n9d)" = "$idd" ] || fail "n9d lost its key after a container restart"
pass "host network: n9d runs read-only as 65532 with no capabilities, has the roster, is up/direct from n1 on the same LAN, and keeps its key over a restart"
$I update server "$N9" --docker --label where=docker >/dev/null
D9 inspect -f '{{range .Config.Env}}{{println .}}{{end}}' imesh | grep -q '^IMESH_LABELS=where=docker$' || fail "update did not set the label"
for i in $(seq 1 60); do [ "$(id_of n9d)" = "$idd" ] && break; sleep 1; done
[ "$(id_of n9d)" = "$idd" ] || fail "update --docker changed the key"
pass "update server --docker replaced the container with the same key and a new label"
$I add "$N9" --docker --docker-name imesh-b --docker-network bridge --name n9b >/dev/null
for i in $(seq 1 40); do sees_n1 n9b && break; sleep 0.5; done
sees_n1 n9b || fail "n9b has no roster with n1"
for i in $(seq 1 30); do [ "$(path_of n9b)" = up/direct ] && break; sleep 1; done
[ "$(path_of n9b)" = up/direct ] || fail "n1 reaches n9b as $(path_of n9b), not up/direct"
pass "bridge network behind Docker's NAT: n9b has the roster, and n1 on the same LAN reaches it up/direct"
$I add "$N9" --docker --docker-name imesh-r --docker-network bridge --relay-only --name n9r >/dev/null
for i in $(seq 1 60); do sees_n1 n9r && break; sleep 0.5; done
sees_n1 n9r || fail "n9r has no roster with n1"
for i in $(seq 1 30); do [ "$(path_of n9r)" = up/relay ] && break; sleep 1; done
[ "$(path_of n9r)" = up/relay ] || fail "n1 reaches the relay-only n9r as $(path_of n9r), not up/relay"
# The container has its own network namespace: every UDP socket there is the daemon's. IPv4
# addresses in /proc are little-endian hex, so 127.x.y.z ends in 7F.
pid=$(D9 inspect -f '{{.State.Pid}}' imesh-r)
open=$(docker exec "$P-n9" sh -c "cat /proc/$pid/net/udp /proc/$pid/net/udp6" | awk '$2 ~ /:/ && !(length($2) == 13 && substr($2, 7, 2) == "7F") {print $2}')
[ -z "$open" ] || fail "relay-only n9r has UDP sockets off loopback: $open"
pass "--relay-only: n9r opens no UDP port but loopback, has the roster, and n1 reaches it through the relay"
$I rm n9d >/dev/null
$I rm n9b >/dev/null
$I rm n9r >/dev/null
$I status | grep -q '^n9[dbr] ' && fail "n9d, n9b, or n9r still listed"
pass "rm removed the containers' machines"

echo "== 25. bundles, JSON output, a tunnel install, SSH aliases, local up and update"
# --json and --jq give the full typed result; strings print raw; an empty result prints nothing.
$I status --json | python3 -c 'import json,sys; r=json.load(sys.stdin); assert r and all(len(m["id"]) == 64 for m in r)' || fail "status --json"
[ "$($I status --jq '.[] | select(.name == "n1") | .name')" = n1 ] || fail "--jq select"
[ -z "$($I status --jq '.[] | select(.name == "nobody") | .name')" ] || fail "an empty filter printed output"
$I status --jq '.[0].status' | grep -q '^{"' || fail "--jq objects are not compact JSON"
$I add "root@localhost:${PB}10" --jq '.[' >/dev/null 2>&1 && fail "a bad --jq expression ran the add"
docker exec "$P-n10" test ! -e /usr/local/bin/imesh || fail "a bad --jq expression changed n10"
pass "--json lists full IDs; --jq selects, prints raw strings and compact objects, nothing for no match; a bad expression stops before a change"

# A stand-in for devtunnel on this computer, first in PATH. The CLI reaches the SSH server of
# n10 only through the stand-in tunnel.
FB=$WORK/fakebin
mkdir -p "$FB" "$WORK/dt" "$WORK/dotssh"
cp "$ROOT/tests/e2e/fake/devtunnel-ssh" "$FB/devtunnel"
export FAKE_DT_PORT=$((PB * 100 + 22)) FAKE_DT_TARGET="localhost:${PB}10" FAKE_DT_STATE=$WORK/dt
DT() { PATH=$FB:$PATH "$@"; }
TUN="devtunnel://box.fake:$FAKE_DT_PORT --ssh-user root --name tbox"
err=$($I add ftp://x 2>&1) && fail "an unknown scheme was accepted"
echo "$err" | grep -q 'unknown target scheme' || fail "scheme error: $err"
echo anonymous >"$WORK/dt/access"
err=$(DT $I add $TUN 2>&1) && fail "an anonymous tunnel was accepted"
echo "$err" | grep -qi anonymous || fail "anonymous error: $err"
rm "$WORK/dt/access"
docker exec "$P-n10" test ! -e /usr/local/bin/imesh || fail "a refused add changed n10"
pass "an anonymous tunnel and an unknown scheme are refused before any change"

# The host key is checked, under the tunnel's alias, never under 127.0.0.1.
cat >>"$WORK/ssh_config" <<EOF
Host 127.0.0.1
  UserKnownHostsFile $WORK/known_hosts
  StrictHostKeyChecking accept-new
  IdentityFile $WORK/id
  IdentitiesOnly yes
EOF
printf 'Host keep-me\n    HostName example.invalid\n' >"$WORK/dotssh/config"
out=$(DT $I add $TUN --json)
echo "$out" | python3 -c 'import json,sys; r=json.load(sys.stdin); assert r["name"] == "tbox" and r["ssh_alias"] == "tbox" and len(r["id"]) == 64, r' || fail "tunnel add: $out"
grep -q "imesh-tunnel-box.fake-$FAKE_DT_PORT" "$WORK/known_hosts" || fail "the host key is not under the tunnel alias"
grep -q '^\[127.0.0.1\]' "$WORK/known_hosts" && fail "a host key was recorded for loopback"
grep -q 'ssh_tunnel = "devtunnel:box.fake:' "$WORK/cfg/config.toml" || fail "the tunnel is not in the context"
for i in $(seq 1 60); do [ "$($I status --jq '.[] | select(.name == "tbox") | .status.state')" = up ] && break; sleep 1; done
[ "$($I status --jq '.[] | select(.name == "tbox") | .status.state')" = up ] || fail "tbox is not up"
pass "devtunnel://box.fake installed through the private tunnel with a checked host key, joined, and is up"

# The bundle went along whole and verified, for onward installs.
KB=/var/lib/imesh/bundle
docker exec "$P-n10" test -d $KB || KB=/root/.local/share/imesh/bundle
for t in $(python3 -c 'import json; print(" ".join(a["path"] + "=" + a["sha256"] for a in json.load(open("'"$ROOT"'/dist/imesh/manifest.json"))["targets"]))'); do
	[ "$(docker exec "$P-n10" sha256sum "$KB/${t%%=*}" | cut -d' ' -f1)" = "${t#*=}" ] || fail "kept bundle: ${t%%=*}"
done
docker exec "$P-n10" test -f "$KB/imesh.zip" || fail "no kept ZIP"
pass "n10 keeps the whole verified bundle, every target and the ZIP, in $KB"

# The managed alias works after the install exited: each SSH session starts its own tunnel client.
head -1 "$WORK/dotssh/config" | grep -qx "Include $WORK/dotssh/imesh/config" || fail "the Include is not first"
tail -2 "$WORK/dotssh/config" | grep -q 'Host keep-me' || fail "the user's entries changed"
grep -q 'ProxyCommand .*ssh-tunnel-proxy devtunnel:box.fake:' "$WORK/dotssh/imesh/config" || fail "no ProxyCommand in the alias"
before=$(wc -l <"$WORK/dt/connects")
[ "$(DT ssh -F "$WORK/dotssh/config" -o UserKnownHostsFile="$WORK/known_hosts" -o BatchMode=yes -o IdentitiesOnly=yes -i "$WORK/id" tbox hostname)" = n10 ] || fail "ssh tbox did not reach n10"
[ "$(wc -l <"$WORK/dt/connects")" -gt "$before" ] || fail "the alias did not start its own tunnel client"
pass "ssh tbox works after the install exited, through its own tunnel client; the user's config keeps its entries"

out=$(DT $I update server $TUN --json)
echo "$out" | python3 -c 'import json,sys; r=json.load(sys.stdin); assert r["name"] == "tbox" and r["ssh_alias"] == "tbox", r' || fail "update: $out"
[ "$(grep -c '^Host tbox' "$WORK/dotssh/imesh/config")" = 1 ] || fail "update duplicated the alias"
pass "update server through the tunnel keeps one alias"

out=$($I rm tbox --json)
echo "$out" | python3 -c 'import json,sys; r=json.load(sys.stdin); assert r["removed"] and r["reset"] and r["ssh_alias"] == "tbox" and r["context_connections"] == 1, r' || fail "rm: $out"
grep -q '^Host tbox' "$WORK/dotssh/imesh/config" && fail "the alias was not removed"
grep -q 'Host keep-me' "$WORK/dotssh/config" || fail "the user's entry was removed"
pass "rm reports removal, reset, context cleanup, and the alias it removed"

# Local up and update, as a user, with the CLI from an extracted bundle.
docker exec "$P-n10" sh -c "rm -rf /home/ops/b; mkdir -p /home/ops/b; cp -r $KB/manifest.json $KB/targets $KB/imesh.zip /home/ops/b/; cp /home/ops/b/targets/$T/imesh /home/ops/b/imesh; chown -R ops:ops /home/ops/b; /usr/local/bin/imesh service uninstall --system"
UP() { docker exec -u ops -w /home/ops "$P-n10" /home/ops/b/imesh "$@"; }
out=$(UP up --service detached --json)
echo "$out" | python3 -c 'import json,sys; r=json.load(sys.stdin); assert r["running"] and r["changed"] == ["cluster"] and r["cluster"] and r["members"] == 1 and r["recovery_code"], r' || fail "up (fresh): $out"
id1=$(echo "$out" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
[ "$(UP up --jq '.changed | length')" = 0 ] || fail "a second up changed something"
UP down >/dev/null
[ "$(UP up --jq .running)" = true ] || fail "up did not start a stopped daemon"
UP up --label role=test >/dev/null
docker exec "$P-n10" grep -q 'IMESH_LABELS=role=test' /home/ops/.config/imesh/daemon.env || fail "up did not apply a new setting"
[ "$(UP update --jq '.changed | length')" = 0 ] || fail "an update with the same bundle changed something"
[ "$(UP update --jq .id)" = "$id1" ] || fail "update changed the key"
UP -c n1 up >/dev/null 2>&1 && fail "up accepted --context"
docker exec "$P-n10" test -f /home/ops/.local/share/imesh/bundle/imesh.zip || fail "up kept no bundle"
pass "imesh up installs, starts a cluster, is idempotent, starts a stopped daemon, applies settings; update with the same bundle changes nothing"

echo "== 28. recover with the code on stdin moves the owner to n3b and removes the old owner"
$I -c n3 recover "$CODE" >/dev/null 2>&1 && fail "recover took the code from argv"
echo "$CODE" | $I -c n3 recover >/dev/null
[ "$(roles -c n3)" = "n3b=owner " ] || fail "roles on n3b after recover: $(roles -c n3)"
$I -c n1 status >/dev/null 2>&1 || true
removed_on n1 || { $I -c n1 dev call machine.inspect; fail "the old owner n1 does not report removed"; }
out=$($I -c n1 promote n3b 2>&1) && fail "the old owner n1 still promotes: $out"
echo "$out" | grep -q "refuse this one as removed" || fail "the old owner n1 refused the promote without saying it is removed: $out"
pass "n3b is the owner; the old owner n1 reports removed and cannot promote ($(echo "$out" | tail -1))"

echo "== 16. footprint"
for n in n1 n4; do
	docker exec "$P-$n" sh -c 'pid=$(systemctl show -p MainPID --value imesh); grep -E "VmRSS|Threads" /proc/$pid/status | tr -s " \t" " " | tr "\n" " "'
	echo
done
ls -l "$BIN" | awk '{printf "binary %.1f MB\n", $5/1048576}'

echo "ALL PASS"
