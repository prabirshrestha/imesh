#!/bin/sh
# Installs or updates the imesh daemon. Runs as root; the daemon runs as IMESH_RUN_USER.
# Inputs: IMESH_SRC (uploaded binary or empty), IMESH_SHA256 (its checksum), IMESH_USER (the SSH user),
# IMESH_RUN_USER and IMESH_RUN_GROUP (default: the SSH user and its primary group),
# IMESH_ENV (daemon settings for /etc/imesh/env, one VAR=value per line).
set -eu

[ "$(uname -s)" = Linux ] || { echo "imesh: only Linux is supported" >&2; exit 1; }
[ -d /run/systemd/system ] || { echo "imesh: systemd is required" >&2; exit 1; }

if [ -n "${IMESH_SRC:-}" ]; then
	echo "$IMESH_SHA256  $IMESH_SRC" | sha256sum -c --quiet - || { echo "imesh: checksum mismatch" >&2; rm -f "$IMESH_SRC"; exit 1; }
	install -m 0755 "$IMESH_SRC" /usr/local/bin/imesh.new
	mv -f /usr/local/bin/imesh.new /usr/local/bin/imesh
	rm -f "$IMESH_SRC"
fi

mkdir -p /etc/imesh
# May hold relay tokens.
(umask 077 && printf '%s\n' "${IMESH_ENV:-}" >/etc/imesh/env)

run_user=${IMESH_RUN_USER:-$IMESH_USER}
if ! getent passwd "$run_user" >/dev/null; then
	group=${IMESH_RUN_GROUP:-$run_user}
	getent group "$group" >/dev/null || groupadd --system "$group"
	useradd --system --gid "$group" --home-dir /var/lib/imesh --no-create-home --shell /usr/sbin/nologin "$run_user"
fi
run_group=${IMESH_RUN_GROUP:-$(id -gn "$run_user")}
getent group "$run_group" >/dev/null || groupadd --system "$run_group"
# The SSH user reaches the socket through the daemon's group.
if [ "$IMESH_USER" != root ] && [ "$IMESH_USER" != "$run_user" ]; then
	usermod -aG "$run_group" "$IMESH_USER"
fi

cat >/etc/systemd/system/imesh.service <<EOF
[Unit]
Description=imesh machine daemon
Wants=network-online.target
After=network-online.target

[Service]
Type=notify
User=$run_user
Group=$run_group
EnvironmentFile=-/etc/imesh/env
ExecStart=/usr/local/bin/imesh daemon
Restart=always
RestartSec=2
RuntimeDirectory=imesh
RuntimeDirectoryMode=0750
StateDirectory=imesh
StateDirectoryMode=0700
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=read-only
PrivateTmp=yes

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable imesh.service >/dev/null 2>&1
systemctl restart imesh.service

i=0
while [ ! -S /run/imesh/imesh.sock ]; do
	i=$((i + 1))
	[ $i -gt 100 ] && { journalctl -u imesh --no-pager -n 30 >&2; exit 1; }
	sleep 0.1
done
echo installed
