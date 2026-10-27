#!/usr/bin/env bash
# End-to-end proof of `imesh invite` over plain iroh: a machine that runs `imesh up --join <code>`
# joins the cluster of the inviting machine on the same network. Needs Docker and cargo-zigbuild.
# Usage: tests/e2e/pair.sh                       KEEP=1 leaves the containers running
#        PREFIX=p PORTS=32 tests/e2e/pair.sh     container name prefix and SSH port block
set -euo pipefail

ROOT=${ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}
P=imesh-e2e-${PREFIX:-p}
PB=${PORTS:-32}
WORK=${WORK:-$(mktemp -d /tmp/imesh-e2e.XXXXXX)}
ARCH=$(docker info --format '{{.Architecture}}')
case $ARCH in aarch64 | arm64) T=aarch64-unknown-linux-musl ;; *) T=x86_64-unknown-linux-musl ;; esac

pass() { printf 'PASS %s\n' "$*"; }
fail() { printf 'FAIL %s\n' "$*"; exit 1; }

echo "work dir $WORK"
(cd "$ROOT" && cargo x build --example app ${BUNDLE_TARGETS:-})
I=$ROOT/dist/imesh/imesh
BIN=$ROOT/dist/imesh/targets/$T/imesh
export IMESH_SOCKET=$WORK/no-local-daemon.sock IMESH_CONFIG_DIR=$WORK/cfg IMESH_SSH_CONFIG=$WORK/ssh_config IMESH_SSH_DIR=$WORK/dotssh IMESH_LOG=warn

docker build -q -t imesh-test-node "$ROOT/tests/e2e/docker" >/dev/null
ssh-keygen -q -t ed25519 -N '' -f "$WORK/id" -C imesh-e2e
cat >"$WORK/ssh_config" <<EOC
Host localhost
  UserKnownHostsFile $WORK/known_hosts
  StrictHostKeyChecking accept-new
  IdentityFile $WORK/id
  IdentitiesOnly yes
EOC

NODES="n1 n2"
cleanup() {
	[ -n "${KEEP:-}" ] && [ "${1:-}" != start ] && return
	[ -n "${INVITE:-}" ] && kill "$INVITE" 2>/dev/null || true
	for n in $NODES; do docker rm -f "$P-$n" >/dev/null 2>&1 || true; done
	docker network rm "$P-lan" >/dev/null 2>&1 || true
}
trap cleanup EXIT
cleanup start
docker network create "$P-lan" >/dev/null
node() { # name ssh-port
	docker run -d --name "$P-$1" --hostname "$1" --network "$P-lan" --privileged --cgroupns=host \
		-v /sys/fs/cgroup:/sys/fs/cgroup:rw -p "${PB}$2:22" imesh-test-node >/dev/null
}
node n1 01
node n2 02
sleep 3
for n in $NODES; do
	docker exec -i "$P-$n" sh -c "cat > /root/.ssh/authorized_keys; for u in deploy ops; do cp /root/.ssh/authorized_keys /home/\$u/.ssh/; chown -R \$u:\$u /home/\$u/.ssh; done" <"$WORK/id.pub"
done
$I ctx add n1 "root@localhost:${PB}01" >/dev/null

echo "== 1. n1 starts a cluster"
$I add "root@localhost:${PB}01" --new >/dev/null
pass "n1 is in a cluster"

echo "== 2. invite prints an iroh pairing code"
$I -c n1 invite n2 --yes >"$WORK/invite.out" 2>"$WORK/invite.err" &
INVITE=$!
for i in $(seq 1 60); do
	code=$(grep -o 'imeshpair1-ir-[a-z0-9]*' "$WORK/invite.err" | head -1 || true)
	[ -n "$code" ] && break
	sleep 1
done
[ -n "$code" ] || { cat "$WORK/invite.err"; fail "no pairing code"; }
pass "code $code"

echo "== 3. a bad code is refused"
docker cp -q "$BIN" "$P-n2:/usr/local/bin/imesh-setup"
UP() { docker exec -u ops -w /home/ops "$P-n2" /usr/local/bin/imesh-setup "$@"; }
UP up --join imeshpair1-ir-abc --service detached >/dev/null 2>&1 && fail "a bad code was accepted"
pass "bad code refused"

echo "== 4. n2 joins with the code"
UP up --join "$code" --service detached || { cat "$WORK/invite.err"; fail "join failed"; }
wait "$INVITE" || { cat "$WORK/invite.err"; fail "invite failed"; }
INVITE=
grep -q "n2 .* is in cluster" "$WORK/invite.out" || fail "invite printed no result: $(cat "$WORK/invite.out")"
pass "invite and join both report success"

echo "== 5. n2 is a member and kv syncs both ways"
for i in $(seq 1 30); do
	s=$($I -c n1 status | awk '$1=="n2"{print $3}')
	[ "$s" = up ] && break
	sleep 1
done
[ "$s" = up ] || { $I -c n1 status; fail "n2 not up in n1's status"; }
$I -c n1 kv set from1 a >/dev/null
for i in $(seq 1 20); do [ "$(UP kv get from1 2>/dev/null)" = a ] && break; sleep 0.5; done
[ "$(UP kv get from1)" = a ] || fail "n1 -> n2 kv not synced"
UP kv set from2 b >/dev/null
for i in $(seq 1 20); do [ "$($I -c n1 kv get from2 2>/dev/null)" = b ] && break; sleep 0.5; done
[ "$($I -c n1 kv get from2)" = b ] || fail "n2 -> n1 kv not synced"
pass "n2 up in n1's status; kv synced both ways"

echo "ALL PASS"
