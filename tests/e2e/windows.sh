#!/usr/bin/env bash
# Windows proof for an imesh prototype: a Windows machine joins a Docker cluster with tickets only,
# then a Linux machine that can reach nothing but a dev tunnel joins through a relay the Windows
# machine serves.
#
# Needs: Docker, cargo-zigbuild, mingw, and a way to type the commands a person at the Windows
# console would type. imesh itself uses neither:
# - WIN_VIA=ssh (default): `ssh $WIN`.
# - WIN_VIA=vmrun: VMware Tools, through tests/e2e/vmrun-agent.sh running in a terminal (VMQ is its
#   queue directory). Needs no network path to the VM, and no dev tunnel.
# W5 needs DEVTUNNEL_ID and a logged-in devtunnel CLI for the Windows user and Linux daemon users.
# SKIP_W5=1 skips it.
# Usage: tests/e2e/windows.sh, or CRATE=prototypes/imesh-records tests/e2e/windows.sh
set -euo pipefail

ROOT=${ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}
CRATE=$ROOT/${CRATE:-crates/imesh-cli}
# The cargo target directory: the workspace's for crates/*, the crate's own for a prototype.
TARGET=$(cd "$CRATE" && cargo metadata -q --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
WIN=${WIN:-pswin01}
P=imesh-win
PB=${PORTS:-27}
WORK=${WORK:-$(mktemp -d /tmp/imesh-win.XXXXXX)}
pass() { printf 'PASS %s\n' "$*"; }
fail() { printf 'FAIL %s\n' "$*"; exit 1; }

WIN_VIA=${WIN_VIA:-ssh}
VMQ=${VMQ:-/tmp/imesh-vmq}
GDIR='C:\Users\Public\imesh-test'
# Every Windows script starts with $src, the copied exe, and $relayArgs, the W2 relay flags.
if [ -n "${SKIP_W5:-}" ]; then
    RELAY_ARGS='@()'
else
    [[ "${DEVTUNNEL_ID:-}" =~ ^[a-zA-Z0-9][a-zA-Z0-9.-]*$ ]] || fail "W5 needs a valid DEVTUNNEL_ID"
    RELAY_ARGS="@('--serve-relay','127.0.0.1:3340','--expose-relay','devtunnel:$DEVTUNNEL_ID')"
fi
if [ "$WIN_VIA" = vmrun ]; then SRC="'$GDIR\imesh.exe'"; else SRC="(Join-Path \$env:USERPROFILE 'imesh.exe')"; fi
prelude() { printf '$src = %s\n$relayArgs = %s\n' "$SRC" "$RELAY_ARGS"; }

# One vmrun job through the agent: one argument per line. Prints the job output.
vmjob() {
	[ -e "$VMQ/ready" ] || fail "no vmrun agent: run tests/e2e/vmrun-agent.sh $VMQ in a terminal"
	local id="$VMQ/$(date +%s%N)-$$"
	printf '%s\n' "$@" >"$id.tmp" && mv "$id.tmp" "$id.job"
	for _ in $(seq 1 "${WINPS_TIMEOUT:-240}"); do [ -e "$id.rc" ] && break; sleep 1; done
	[ -e "$id.rc" ] || fail "vmrun job timed out: $1"
	local rc; rc=$(cat "$id.rc")
	cat "$id.log"; rm -f "$id.rc" "$id.log"
	return "$rc"
}

# Runs PowerShell on the Windows machine. The script comes on stdin.
winps() {
	if [ "$WIN_VIA" = vmrun ]; then
		local n=$RANDOM
		{ prelude; cat; } >"$WORK/ps-$n.ps1"
		vmjob copyFileFromHostToGuest "$WORK/ps-$n.ps1" "$GDIR\ps-$n.ps1" >/dev/null
		vmjob runProgramInGuest 'C:\Windows\System32\cmd.exe' \
			"/c powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File $GDIR\ps-$n.ps1 > $GDIR\ps-$n.out 2>&1" >/dev/null || true
		vmjob copyFileFromGuestToHost "$GDIR\ps-$n.out" "$WORK/ps-$n.out" >/dev/null
		cat "$WORK/ps-$n.out"
	else
		local enc
		enc=$({ prelude; cat; } | iconv -f UTF-8 -t UTF-16LE | base64 | tr -d '\n')
		timeout "${WINPS_TIMEOUT:-240}" ssh -o BatchMode=yes "$WIN" "powershell -NoProfile -NonInteractive -EncodedCommand $enc" 2>&1 | grep -v 'CLIXML\|<Objs'
	fi
}

# Copies the Windows exe to $src.
copy_exe() {
	local exe=$TARGET/x86_64-pc-windows-gnu/release/$OUT.exe
	if [ "$WIN_VIA" = vmrun ]; then
		vmjob runProgramInGuest 'C:\Windows\System32\cmd.exe' "/c mkdir $GDIR" >/dev/null || true
		vmjob copyFileFromHostToGuest "$exe" "$GDIR\imesh.exe" >/dev/null
	else
		timeout 300 scp -q -o BatchMode=yes "$exe" "$WIN:imesh.exe"
	fi
}

echo "work dir $WORK"
# Test the example app, built as any app is (imesh::main plus its own tables and methods), when
# the crate has one; a prototype without it uses its plain binary.
if [ -f "$CRATE/examples/app.rs" ]; then BUILD="--example app"; OUT=examples/app; else BUILD=; OUT=imesh; fi
(cd "$CRATE" && cargo build -q $BUILD && cargo zigbuild -q --release --target aarch64-unknown-linux-musl $BUILD &&
	cargo build -q --release --target x86_64-pc-windows-gnu $BUILD)
I=$TARGET/debug/$OUT
export IMESH_CONFIG_DIR=$WORK/cfg IMESH_SSH_CONFIG=$WORK/ssh_config IMESH_LOG=warn
export IMESH_BINARY_AARCH64=$TARGET/aarch64-unknown-linux-musl/release/$OUT

ssh-keygen -q -t ed25519 -N '' -f "$WORK/id" -C imesh-win
cat >"$WORK/ssh_config" <<EOF
Host localhost
  UserKnownHostsFile $WORK/known_hosts
  StrictHostKeyChecking accept-new
  IdentityFile $WORK/id
  IdentitiesOnly yes
EOF

# KEEP=1 keeps the containers after a run; a new run always starts clean.
cleanup() {
	[ -n "${KEEP:-}" ] && [ "${1:-}" != start ] && return
	for n in n1 n2 n3; do docker rm -f "$P-$n" >/dev/null 2>&1 || true; done
	docker network rm "$P-lan" >/dev/null 2>&1 || true
}
trap cleanup EXIT
cleanup start
docker network create "$P-lan" >/dev/null
for n in 1 2 3; do
	docker run -d --name "$P-n$n" --hostname "lin$n" --network "$P-lan" --privileged --cgroupns=host \
		-v /sys/fs/cgroup:/sys/fs/cgroup:rw -p "${PB}0$n:22" imesh-test-node >/dev/null
done
sleep 3
for n in 1 2 3; do docker exec -i "$P-n$n" sh -c 'cat > /root/.ssh/authorized_keys' <"$WORK/id.pub"; done

echo "== W1. a Linux cluster over SSH"
$I machine init "root@localhost:${PB}01" >/dev/null
$I machine add "root@localhost:${PB}02" >/dev/null
pass "lin1 and lin2"

echo "== W2. the Windows machine installs itself, with no admin and no SSH in imesh"
copy_exe
out=$(winps <<'EOF'
$dir = Join-Path $env:LOCALAPPDATA 'imesh'
Stop-ScheduledTask -TaskName imesh -ErrorAction SilentlyContinue
Get-Process imesh -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq (Join-Path $dir 'bin\imesh.exe') } | Stop-Process -Force
Start-Sleep 1
# Keep the machine key, so the labeled dev tunnel is reused. Remove the rest.
Get-ChildItem $dir -Exclude secret.key -ErrorAction SilentlyContinue | Remove-Item -Recurse -Force
& $src daemon install --label os=windows @relayArgs
Start-Sleep 3
& (Join-Path $dir 'bin\imesh.exe') machine id
EOF
)
echo "$out" | grep -q 'imesh daemon is running' || fail "install: $out"
ticket=$(echo "$out" | grep -o 'endpoint[a-z0-9]*' | tail -1)
[ -n "$ticket" ] || fail "no ticket: $out"
pass "daemon installed as a scheduled task, ticket printed"

echo "== W3. admit by ticket on this computer, join on Windows"
jt=$($I machine add "$ticket" --name win01 | grep -o 'imeshjoin[A-Za-z0-9_-]*')
out=$(winps <<EOF
& (Join-Path \$env:LOCALAPPDATA 'imesh\bin\imesh.exe') machine join $jt
EOF
)
echo "$out" | grep -q 'joined cluster' || fail "join: $out"
for i in $(seq 1 30); do
	s=$($I machine ls | awk '$1=="win01"{print $3"/"$4}')
	case $s in up/*) break ;; esac
	sleep 1
done
$I machine ls
case $s in up/*) ;; *) fail "win01 not up: $s" ;; esac
pass "win01 joined with tickets only, path ${s#up/}"

echo "== W4. data both ways, forwarding, app ALPN, CLI over iroh"
WID=$($I call machine.inspect -m win01 | awk -F'"' '/"id"/{print $4; exit}')
$I call kv.put '{"key":"to-win","value":"from-linux"}' >/dev/null
for i in $(seq 1 20); do [ "$($I call kv.get '{"key":"to-win"}' -m win01)" = '"from-linux"' ] && break; sleep 0.5; done
[ "$($I call kv.get '{"key":"to-win"}' -m win01)" = '"from-linux"' ] || fail "Linux write not on Windows"
winps <<'EOF' >/dev/null
& (Join-Path $env:LOCALAPPDATA 'imesh\bin\imesh.exe') call kv.put '{\"key\":\"to-lin\",\"value\":\"from-windows\"}'
EOF
for i in $(seq 1 20); do [ "$($I call kv.get '{"key":"to-lin"}' -m lin2)" = '"from-windows"' ] && break; sleep 0.5; done
[ "$($I call kv.get '{"key":"to-lin"}' -m lin2)" = '"from-windows"' ] || fail "Windows write not on Linux"
$I call example.dial "{\"to\":\"$WID\",\"text\":\"hi\"}" | grep -q '"echo": "hi"' || fail "app ALPN to Windows"
[ "$($I --connect "iroh://$WID" call ping)" = '"pong"' ] || fail "CLI over iroh to Windows"
pass "sync both ways, forwarded calls, app ALPN, CLI straight to win01"

echo "== W5. Linux joins through the authenticated devtunnel CLI and win01 relay"
if [ -n "${SKIP_W5:-}" ]; then
	echo "SKIP W5 (SKIP_W5 is set)"
else
for i in $(seq 1 60); do
	url=$($I relay ls | awk -F'"' '/"url"/{print $4; exit}')
	[ -n "$url" ] && break
	sleep 2
done
[ -n "$url" ] || fail "win01 did not publish a relay URL"
[ -n "${DEVTUNNEL_ID:-}" ] || fail "W5 needs DEVTUNNEL_ID and the devtunnel CLI logged in as imesh in every Linux container; use SKIP_W5=1 otherwise"
# Private forwarding needs the CLI on each peer, with an existing authorized login.
# Use relay-only to prove no direct UDP path, without blocking the CLI's login endpoints.
for n in 1 2; do
    $I machine update "root@localhost:${PB}0$n" --devtunnel "devtunnel:$DEVTUNNEL_ID:3340" --no-n0 --relay-only >/dev/null
done
$I relay set "$url" >/dev/null
sleep 20
$I machine add "root@localhost:${PB}03" --no-n0 --relay-only --devtunnel "devtunnel:$DEVTUNNEL_ID:3340" >/dev/null
for i in $(seq 1 40); do
	s=$($I --connect "root@localhost:${PB}03" machine ls 2>/dev/null | awk '$1=="win01"{print $3"/"$4}' || true)
	[ "$s" = up/relay ] && break
	sleep 1
done
[ "$s" = up/relay ] || fail "lin3 does not see win01 over the relay: $s"
$I call kv.put '{"key":"tunnel","value":"ok"}' >/dev/null
for i in $(seq 1 20); do [ "$($I --connect "root@localhost:${PB}03" call kv.get '{"key":"tunnel"}')" = '"ok"' ] && break; sleep 0.5; done
[ "$($I --connect "root@localhost:${PB}03" call kv.get '{"key":"tunnel"}')" = '"ok"' ] || fail "lin3 did not sync"
$I --connect "root@localhost:${PB}03" machine ls
pass "lin3 joined and synced through $url"
fi

echo "== W6. Windows footprint"
winps <<'EOF'
$p = Get-Process imesh | Where-Object { $_.Path -eq (Join-Path $env:LOCALAPPDATA 'imesh\bin\imesh.exe') }
"RSS {0:N1} MB, threads {1}" -f ($p.WorkingSet64 / 1MB), $p.Threads.Count
EOF
ls -l "$TARGET/x86_64-pc-windows-gnu/release/$OUT.exe" | awk '{printf "exe %.1f MB\n", $5/1048576}'

echo "ALL PASS"
