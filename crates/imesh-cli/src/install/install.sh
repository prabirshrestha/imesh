#!/bin/sh
# Installs or updates the imesh system daemon (`imesh daemon install --system`). Runs as root; the
# daemon runs as IMESH_RUN_USER.
# Inputs: IMESH_SRC (the binary to install), IMESH_SHA256 (its checksum), IMESH_USER (the user who
# asked; gets access to the socket), IMESH_RUN_USER (created as a system user if missing),
# IMESH_RUN_GROUP (the socket's group), IMESH_ENV_B64 (base64 of the daemon settings
# for /etc/imesh/env, one VAR=value per line).
set -eu

[ "$(uname -s)" = Linux ] || { echo "imesh: only Linux is supported" >&2; exit 1; }
[ -d /run/systemd/system ] || { echo "imesh: systemd is required" >&2; exit 1; }

echo "$IMESH_SHA256  $IMESH_SRC" | sha256sum -c --quiet - || { echo "imesh: checksum mismatch" >&2; exit 1; }
if ! cmp -s "$IMESH_SRC" /usr/local/bin/imesh; then
	install -m 0755 "$IMESH_SRC" /usr/local/bin/imesh.new
	mv -f /usr/local/bin/imesh.new /usr/local/bin/imesh
fi

mkdir -p /etc/imesh
# May hold relay tokens. The settings arrive as base64, so no value is ever shell source.
(umask 077 && printf '%s' "${IMESH_ENV_B64:-}" | base64 -d >/etc/imesh/env)

# A group of its own, so the socket is never shared through a common primary group.
run_group=$IMESH_RUN_GROUP
run_user=$IMESH_RUN_USER
getent group "$run_group" >/dev/null || groupadd --system "$run_group"
if ! getent passwd "$run_user" >/dev/null; then
	useradd --system --gid "$run_group" --home-dir /var/lib/imesh --no-create-home --shell /usr/sbin/nologin "$run_user"
fi
# The user who asked reaches the socket through the group.
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
Environment=IMESH_DATA_DIR=/var/lib/imesh IMESH_SOCKET=/run/imesh/imesh.sock IMESH_SOCKET_GROUP=$run_group
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
echo "imesh system daemon running as $run_user, socket group $run_group"
