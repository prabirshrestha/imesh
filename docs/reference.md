# imesh reference

This page describes the `imesh` command ([`crates/imesh-cli`](../crates/imesh-cli)) and the machine it runs ([`crates/imesh`](../crates/imesh)). The variables and files are the command's; the library reads none of them. [design.md](design.md) explains the reasons behind it.

## Commands

| Command | Description |
|---|---|
| `imesh machine init <user@host[:port]>` | Installs or updates the daemon over SSH, creates a cluster on it, authorizes this CLI's key, and adds a context that becomes current. Refuses, without changing anything, if the host is already in a cluster. |
| `imesh machine add <user@host[:port]>` | Installs the daemon over SSH and adds the host to the cluster of the entry machine. Refuses, without changing anything, if the host is already in a cluster. |
| `imesh machine add <ticket> --name <name>` | Admits a machine that installed itself, by the ticket from its `machine id`, and prints its join ticket. No SSH. |
| `imesh machine update <user@host[:port]>` | Uploads the current Linux binary and applies settings over SSH. Settings you do not pass stay as they are. |
| `imesh machine id` | Prints this machine's ticket. Talks to the local daemon. |
| `imesh machine join <join ticket>` | Joins the cluster named in the join ticket. Talks to the local daemon. |
| `imesh machine ls [-w]` | Lists members with state, path, RTT, OS, architecture, and version, as the entry machine sees them. `-w` adds the full ID, last contact, and the last link error. |
| `imesh machine rm <name or id> [--force]` | Resets the machine (best effort), marks it removed, and removes its connections from the context. Fails if you are connected through that machine, or if it serves a relay in the cluster relay setting, unless `--force`. |
| `imesh relay ls` | Shows the cluster relay setting, the relays that machines serve, and the relays the entry machine uses. |
| `imesh relay set [url...] [--token <token>]` | Makes every machine use exactly these relays. No URLs clears the setting. |
| `imesh schema apply <file>` | Applies the `CREATE TABLE` and `CREATE INDEX` statements in the file on the entry machine. Members take the new tables over their links before any row that uses them. A table that exists may only grow. |
| `imesh schema ls` | Prints the entry machine's tables, and for each member whether rows pass: `ok`, or why not, for example `schema conflict: ...`. |
| `imesh sql "<select>" [-p <param>]...` | Runs one read-only statement on the entry machine's copy of the database and prints the rows, tab-separated with a header. Each `-p` binds `?1`, `?2`, ... as a JSON value, or as a string if it is not JSON. |
| `imesh sql --exec "<statement>" [-p <param>]...` | Runs one `INSERT`, `UPDATE`, `DELETE`, or `REPLACE` in a transaction, sends the change to every machine, and prints the number of rows changed. |
| `imesh client ls` | Lists the CLI keys that may use the API over iroh. |
| `imesh client add <key> [--name <name>]` | Authorizes a CLI key. |
| `imesh client rm <key>` | Revokes a CLI key and closes its open connections. |
| `imesh key` | Prints this CLI's public key. |
| `imesh ctx ls` | Lists contexts and marks the current one. |
| `imesh ctx use <name>` | Makes a context current. |
| `imesh ctx conn [n]` | Lists the connections of the context. With `n`, moves connection `n` to the top first. |
| `imesh ctx add <name> <connection>...` | Adds connections to a context and creates it if needed. |
| `imesh call <method> [json] [-m <name or id>]` | Calls an API method. With `-m`, the entry machine sends the call on to that machine. |
| `imesh daemon` | Runs the daemon. The service runs this. A user daemon reads its settings from `~/.config/imesh/daemon.env` first; variables already set win. |
| `imesh daemon install [options]` | Installs this binary as the daemon and starts it. Linux: for the current user with no root or sudo, as a systemd user service or a detached process (`--service auto\|systemd\|detached`). `--system`, or running as root, installs a system service instead (`--run-as`, `--run-as-group`). Windows: a scheduled task for the current user at logon, no admin; with `--system`, from an elevated shell, a Windows service that starts at boot, before anyone logs in. Takes the relay options and `--label`. |
| `imesh daemon uninstall [--purge] [--system]` | Stops the daemon and removes the service and the binary. `--purge` also removes the key, the database, and settings. `--system` (or root on Linux) removes the system service. |
| `imesh daemon status [--socket <path>]` | Prints `{"running": <bool>, "cluster": <id or null>}` for the local daemon. |
| `imesh daemon start` | Starts this user's detached daemon unless it runs. The crontab line runs this at boot. |
| `imesh daemon stop` | Stops this user's detached daemon. |
| `imesh dial-stdio` | Hidden. Bridges stdin and stdout to the local API. The CLI runs it over SSH. |

### Options for `machine init`, `machine add`, and `machine update`

| Option | Description |
|---|---|
| `-i, --ssh-key <path>` | SSH private key. When you omit it, `ssh` uses the agent and `~/.ssh/config`. |
| `--name <name>` | Machine name. The default is the remote hostname. Required when adding by ticket. |
| `--system` | Install a system service instead of one for the SSH user. A root login always gets one. Any other SSH user then needs passwordless sudo. |
| `--service auto\|systemd\|detached` | For a user install: a systemd user service with linger, or a detached process that the user's crontab starts at boot. `auto` takes systemd when the user may turn on linger. |
| `--docker` | Run the daemon in a Docker container on the host instead of installing it: for hosts where installing is hard, such as a NAS. The SSH user must be able to run `docker`, or `sudo -n docker`. The CLI builds the image there from the binary on `alpine:3`, and runs it with `--restart unless-stopped`, a read-only root file system, user 65532, no capabilities, and `no-new-privileges`. The key and database are in the volume `<name>-data`, so `update` keeps the machine. |
| `--docker-name <name>` | Container name. The default is `imesh`. |
| `--docker-network host\|bridge` | `host` (the default) gives iroh the host's addresses. `bridge` puts the container behind Docker's NAT on the host, which isolates its network more. Both reach LAN peers directly in the e2e test (stage 24); on other NATs, bridge may need a relay more often. |
| `--docker-volume <src:dst[:ro]>` | Also mount this into the container. Repeat for more. |
| `--run-as <user>` | System service: run the daemon as this user. The default is the SSH user, or `imesh` when the SSH user is root. The install creates the user as a system user if it does not exist. `update` keeps the current user unless you pass this. |
| `--run-as-group <group>` | System service: the socket's group. The default is `imesh`. The install adds the SSH user to it. |
| `--label <key=value>` | Machine label. Repeat for more. |
| `--relay <url>` | Relay to use instead of the n0 relays. Repeat for more relays. |
| `--relay-token <token>` | Bearer token for the `--relay` relays. |
| `--relay-ca <path>` | PEM file of extra CA certificates to trust for relay TLS. A path on the machine. |
| `--no-n0` | Turn off the n0 relays and n0 address lookup. |
| `--pkarr <url>` | Also publish and resolve addresses through this pkarr server (a self-hosted `iroh-dns-server`) over HTTP. |
| `--mdns` | Also find machines on the same LAN with mDNS. |
| `--relay-only` | Open no UDP port: every connection goes out to a relay over HTTPS, and nothing listens for iroh on the machine. Slower, and all traffic uses relay bandwidth; use your own relay (`--relay`). Not with `--mdns`. |
| `--serve-relay <addr>` | Run a members-only iroh relay in this daemon on this TCP address, for example `0.0.0.0:3340`. |
| `--serve-relay-token <token>` | Also require this token on the served relay. |
| `--serve-relay-cert <path>`, `--serve-relay-key <path>` | Serve the relay over HTTPS with this certificate and key. Paths on the machine. |
| `--devtunnel devtunnel:<id>:<port>` | Forward a private relay with the logged-in CLI. Requires a free local port, and replaces `--relay`. |
| `--expose-relay devtunnel[:<id>]` | Publish the loopback relay through a private dev tunnel, found or created by the labels `imesh` and `imesh-<first 10 hex of the machine id>`, or with this tunnel ID. |
| `--proxy <url>` | HTTP proxy for relay traffic, written to the daemon's settings as `HTTPS_PROXY`. |
| `--binary <path>` | The Linux `imesh` binary to upload. The default is `$IMESH_BINARY_<ARCH>`, for example `IMESH_BINARY_AARCH64` or `IMESH_BINARY_X86_64`. |

For `add`, `--relay` and `--no-n0` default to the entry machine's settings when you give neither. The cluster relay setting reaches the new machine in its join, whatever you pass.

### Global options

| Option | Env | Description |
|---|---|---|
| `--connect <conn>` | `IMESH_CONNECT` | Use only this connection. The config is neither read nor changed. Forms: `user@host`, `ssh://user@host[:port]`, `iroh://<endpoint id>`, `local://<path>`. |
| `-c, --context <name>` | `IMESH_CONTEXT` | Use this context instead of `current_context`. For `machine init`, the name of the new context. |
| `--log-file <path>` | `IMESH_LOG_FILE` | Append logs to this file instead of stderr. |

## Environment variables

| Variable | Used by | Default | Description |
|---|---|---|---|
| `IMESH_CONFIG_DIR` | CLI | `~/.config/imesh` | Directory for `config.toml` and `identity.key`. |
| `IMESH_CONFIG` | CLI | `$IMESH_CONFIG_DIR/config.toml` | Path of the config file. |
| `IMESH_CONNECT` | CLI | | Same as `--connect`. |
| `IMESH_CONTEXT` | CLI | | Same as `--context`. |
| `IMESH_SSH_CONFIG` | CLI | | Passed to `ssh -F`. |
| `IMESH_BINARY_<ARCH>` | CLI | | Linux binary for `machine init`, `add`, and `update`. |
| `IMESH_LOG` | both | `warn` for the CLI, `info` for the daemon | `tracing` filter. |
| `IMESH_LOG_FILE` | both | | Same as `--log-file`. |
| `HTTPS_PROXY`, `HTTP_PROXY` | both | | HTTP proxy for relay traffic. |
| `IMESH_DATA_DIR` | daemon | `~/.local/share/imesh`, Windows `%LOCALAPPDATA%\imesh`. The system unit sets `/var/lib/imesh`. | Key and database. |
| `IMESH_SOCKET` | daemon, CLI | `~/.local/share/imesh/imesh.sock`, Windows `\\.\pipe\imesh-<user>`. The system unit sets `/run/imesh/imesh.sock`. A client uses the user socket if it exists, else the system socket. | Local API. |
| `IMESH_SOCKET_GROUP` | daemon | | Also let this group use the local API: the socket's group (mode 0660) on Linux, a local group on Windows. The system unit and the Windows service set `imesh`. |
| `XDG_CONFIG_HOME`, `XDG_DATA_HOME` | CLI, daemon | `~/.config`, `~/.local/share` | Where a user install keeps its settings and data on Linux. |
| `IMESH_NAME` | daemon | hostname | Machine name used by `cluster.init`. |
| `IMESH_LABELS` | daemon | | Labels, `key=value` separated by commas. |
| `IMESH_RELAYS` | daemon | n0 relays | Relay URLs, separated by commas. |
| `IMESH_RELAY_TOKEN` | daemon | | Bearer token for `IMESH_RELAYS`. |
| `IMESH_RELAY_CA` | daemon | | PEM file of extra relay CA certificates. |
| `IMESH_NO_N0` | daemon | `false` | Turn off the n0 relays and n0 address lookup. |
| `IMESH_PKARR` | daemon | | pkarr server URL. |
| `IMESH_MDNS` | daemon | `false` | Turn on mDNS. |
| `IMESH_RELAY_ONLY` | daemon | `false` | Open no UDP port; connect through relays only. |
| `IMESH_SERVE_RELAY` | daemon | | TCP address for the embedded members-only relay. |
| `IMESH_SERVE_RELAY_TOKEN` | daemon | | Token that the served relay requires. |
| `IMESH_SERVE_RELAY_CERT`, `IMESH_SERVE_RELAY_KEY` | daemon | | PEM certificate and key for HTTPS on the served relay. |
| `IMESH_DEVTUNNEL` | daemon | | `devtunnel:<id>:<port>`; authenticated CLI port forwarding. |
| `IMESH_EXPOSE_RELAY` | daemon | | `devtunnel` or `devtunnel:<id>`. |

## Client config file

`$IMESH_CONFIG_DIR/config.toml`, mode 0600, in a 0700 directory.

```toml
current_context = "default"

[[contexts.default.connections]]
ssh = "deploy@203.0.113.7:22"
ssh_key_file = "/home/me/.ssh/id_ed25519"   # optional
machine = "<endpoint id>"                    # optional, used by machine rm

[[contexts.default.connections]]
iroh = "<endpoint id>"
relay = "https://relay.example.com/"         # optional
relay_token = "..."                          # optional
relay_ca = "/home/me/relay-ca.pem"           # optional, a local path
addrs = ["203.0.113.7:4433"]                 # optional
no_n0 = true                                 # optional
machine = "<endpoint id>"

[[contexts.default.connections]]
ssh = "admin@nas.local"
docker = "imesh"                             # the daemon runs in this container on the host

[[contexts.local.connections]]
local = "/run/imesh/imesh.sock"
```

An iroh connection may set `devtunnel = "devtunnel:<id>:<port>"` to forward its private relay through the logged-in CLI. Its `relay`, if set, must be `http://127.0.0.1:<port>/`. The port must be free locally.

Each connection has exactly one of `ssh`, `iroh`, or `local`. With `docker`, an `ssh` connection runs `docker exec -i <container> /imesh dial-stdio` on the host (through `sudo -n` when the user cannot run `docker` directly). `machine init` writes one `ssh` and one `iroh` connection for the machine. `machine add` appends the same pair for the new machine to the context that selected the entry machine, and nothing when you used `--connect`. A context name that is already in use gets a suffix: `default-1`, `default-2`.

`$IMESH_CONFIG_DIR/identity.key` holds the CLI's iroh secret key as hex, mode 0600. `imesh key` prints its public key.

## Files on a Linux machine

A user install (the default for any user but root):

| Path | Mode | Content |
|---|---|---|
| `~/.local/bin/imesh` | 0755 | The binary. |
| `~/.config/imesh/daemon.env` | 0600 | Daemon settings, one `IMESH_*=value` per line. Every `machine init`, `add`, or `update` rewrites it; `update` keeps what you do not change. |
| `~/.local/share/imesh/` | 0700 | Data directory: `secret.key` (0600), `corrosion-<id>/`, `app/`, as for a system install below. |
| `~/.local/share/imesh/imesh.sock` | 0600 | Local API. Only the user can open it. Not in `$XDG_RUNTIME_DIR`, which logind removes at logout and which does not exist at boot. |
| `~/.config/systemd/user/imesh.service` | 0644 | With a systemd user service: `Type=notify`, `Restart=always`, `RestartSec=2`. Linger starts it at boot. |
| `~/.local/share/imesh/supervisor.pid`, `imesh.log` | 0600 | With a detached daemon: the supervisor's PID and the log. The user's crontab has `@reboot '/home/<user>/.local/bin/imesh' daemon start # imesh`. |

A system install (`--system`, or a root login):

| Path | Owner and mode | Content |
|---|---|---|
| `/usr/local/bin/imesh` | root 0755 | The binary. |
| `/etc/systemd/system/imesh.service` | root 0644 | `User=` and `Group=` of the daemon, `Type=notify`, `Restart=always`, `RestartSec=2`, `IMESH_DATA_DIR=/var/lib/imesh`, `IMESH_SOCKET=/run/imesh/imesh.sock`, `EnvironmentFile=-/etc/imesh/env`, `RuntimeDirectory=imesh`, `StateDirectory=imesh`, `NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome=read-only`, `PrivateTmp`. |
| `/etc/imesh/env` | root 0600 | Daemon settings, as `daemon.env` above. |
| `/var/lib/imesh/secret.key` | daemon user 0600 | Machine key, hex. |
| `/var/lib/imesh/corrosion-<id>/` | daemon user | The Corrosion database of the machine with key `<id>`: `state.db` with its WAL files. A reset makes a new key and so a new directory. The daemon removes the directories of earlier keys at start. |
| `/var/lib/imesh/app/` | daemon user | App files. A reset deletes it. |
| `/run/imesh/` | daemon user and group, 0750 | Runtime directory. |
| `/run/imesh/imesh.sock` | daemon user and group `imesh`, 0660 | Local API. |

The system daemon runs as the SSH user, as `imesh` when the SSH user is root, or as the `--run-as` user. The install creates a missing user as a system user. The socket group is `imesh` (or `--run-as-group`), a group only for imesh, and the install adds the SSH user to it.

## Files on a Windows machine

| Path | Content |
|---|---|
| `%LOCALAPPDATA%\imesh\bin\imesh.exe` | The binary. |
| `%LOCALAPPDATA%\imesh\env` | Daemon settings, read at start. |
| `%LOCALAPPDATA%\imesh\secret.key`, `corrosion-<id>\state.db` | Machine key and database. A directory of an earlier key that Windows kept open is removed at the next start. |
| `%LOCALAPPDATA%\imesh\imesh.log` | Daemon log. |
| Scheduled task `imesh` | Runs `conhost.exe --headless imesh.exe daemon` at logon, restarts on failure. |
| `\\.\pipe\imesh-<user>` | Local API. Its access list lets in only the user (by SID) and SYSTEM, and it refuses remote clients. A client checks that the process that serves the pipe runs as the same user, or as a service identity that only an administrator can create, so another user cannot take the name first. |

A system install (`imesh daemon install --system`, from an elevated shell):

| Path | Content |
|---|---|
| `%ProgramFiles%\imesh\imesh.exe` | The binary. |
| Service `imesh` | Runs `imesh.exe --log-file ... daemon windows-service` as the virtual account `NT SERVICE\imesh`, not as SYSTEM. It starts at boot, before anyone logs in, and restarts after 2 s, 10 s, and 60 s when it fails. |
| `%ProgramData%\imesh\` | Key, database, `env` (settings), and `imesh.log`. Its access list, not inherited: SYSTEM, Administrators, and `NT SERVICE\imesh`. |
| Local group `imesh` | May use the service's pipe. The install creates it and adds the user who runs the install; that user logs in again to use it. |
| `\\.\pipe\imesh` | Local API of the service: `NT SERVICE\imesh`, SYSTEM, Administrators, and the group `imesh`. A client on the computer uses the user's pipe if it exists, else this one. |

## ALPNs

| ALPN | Who may open it | Content |
|---|---|---|
| `imesh/api/0` | members, keys in `client/` | API calls, one per bidirectional stream. |
| `imesh/corrosion/1` | members | One link per pair of members. Each side opens one unidirectional stream of frames: a 4-byte big-endian length, a 1-byte kind, and the payload. Kinds: `0` keepalive (empty, every 1 s), `1` a Corrosion gossip datagram with a 12-byte address header (source IPv4 and port, destination IPv4 and port), `2` the machine's tables as JSON. At most 4 MiB per frame. Gossip passes only while both sides have the same tables. |
| app ALPNs | members | Registered with `Builder::protocol` or `Builder::protocol_with`. |

During `cluster.join`, the peers named in the call may also connect, until the call ends. The embedded relay (`--serve-relay`) admits members, keys in `client/`, and the machine itself, and the token when `--serve-relay-token` is set.

## API wire format

Each frame has a 4-byte big-endian length, then UTF-8 JSON. The maximum frame is 16 MiB.

```json
{"to": "<endpoint id or omitted>", "method": "kv.put", "params": {"key": "a", "value": 1}}
```

The reply is `{"ok": <value>}` or `{"err": "<message>"}`. A method's writes are committed in SQLite, with `synchronous = FULL`, before its `ok` reply.

## Methods

| Method | Params | Result |
|---|---|---|
| `ping` | | `"pong"` |
| `methods` | | array of names |
| `whoami` | | `{caller: "local" or "client" or "member", id?}` |
| `machine.inspect` | | `{id, info, cluster, member, relays, no_n0}`. `info` holds the name, address, version, platform, and labels. Never returns tokens. |
| `machine.reset` | | `null`. The daemon stops, deletes `<data dir>/app` and its key, and starts again in the same process as a new machine with a new key and a new database directory. The old directory is removed then, or at the next start. |
| `cluster.init` | `{name?}` | `{cluster, id}` |
| `cluster.add` | `{id, name}` | `{cluster, peers: [EndpointAddr], relays: {relays, token?}}`. Refuses a removed id and a name that another member has. |
| `cluster.join` | `{cluster, peers, relays?}` | `{cluster, members}` |
| `cluster.remove` | `{id}` | `null` |
| `cluster.machines` | | `[{id, name, info, status: {state, path, rtt_ms, last_seen, error, swim?, schema?}}]`. `swim` says whether Corrosion's SWIM counts the member as alive. `schema` is `ok` when rows pass on the link, or why not. |
| `client.add` | `{key, name?}` | `null` |
| `client.remove` | `{key}` | `null` |
| `client.list` | | `[{key, name}]` |
| `relays.get` | | `{cluster, token_set, served: [{machine, url}], in_use}` |
| `relays.set` | `{relays, token?}` | `null` |
| `schema.apply` | `{sql}` | `null`. Applies `CREATE TABLE` and `CREATE INDEX` statements on this machine; members take them over their links. Refuses SQL that names `imesh_*`, reserved table names (starting with `__`, `sqlite_`, or `crsql`, or containing `__crsql_`), and any drop or change of a table or column. |
| `schema.list` | | `{tables: {name: sql}, members: {name: status}}`. `status` is `ok`, `waiting for the member's tables`, `waiting for the member to take this machine's tables`, or `schema conflict: ...`. |
| `sql.query` | `{sql, params?}` | `{columns, rows}`. One statement on this machine's copy. SQLite's authorizer allows reads only: no transactions, ATTACH, DDL, or setting PRAGMAs. It stops after 30 s. `params` bind `?1`, `?2`, ...: JSON null, bool (as 0 or 1), number, or string; arrays and objects bind as JSON text. Blobs return as base64. |
| `sql.execute` | `{statements: [{sql, params?}]}` | Rows changed per statement. Runs all statements in one transaction and sends the change to every member. Each statement must start with `INSERT`, `UPDATE`, `DELETE`, `REPLACE`, or `WITH`. SQLite's authorizer allows row changes of app tables only: not `imesh_*`, and not tables, indexes, or Corrosion's and cr-sqlite's own tables. |
| `kv.put` | `{key, value?}` | `null`. Writes the JSON value to `imesh_kv`. Omit `value`, or pass `null`, to delete. |
| `kv.get` | `{key}` | the value from `imesh_kv`, or `null` |
| `kv.list` | | `{key: value}` for every row of `imesh_kv` |
| `kv.fill` | `{prefix, count, bytes}` | `count`. Test helper of the example app (`examples/app.rs`), not of the library: writes rows `<prefix>/<i>` of `example_items` with distinct values, in one transaction. |
| `kv.count` | `{prefix}` | Test helper of the example app: number of `example_items` rows under `<prefix>/`. |
| `example.dial` | `{to, text}` | `{echo, from}`. The example app. |
| `example.note` | `{text}` | `{who}`. The example app: writes a row of `example_notes`; refuses CLI keys. |

## Tables

imesh's own tables. Each column is `TEXT` unless it says otherwise.

| Table | Columns | Writer | Content |
|---|---|---|---|
| `imesh_cluster` | `id` (key), `founder` | `cluster.init` | One row. A join into another cluster is refused. |
| `imesh_members` | `id` (key), `name`, `removed` (integer) | `cluster.add`, `cluster.init`, `cluster.remove` | `removed = 1` is final. |
| `imesh_machines` | `id` (key), `info` | machine `<id>`; `cluster.remove` deletes it | JSON `{name, addr, version, os, arch, relay?, labels?}`. `relay` is the public URL of a relay it exposes. |
| `imesh_clients` | `key` (key), `name` | `client.add`, `client.remove` | CLI keys that may use the API over iroh. |
| `imesh_settings` | `key` (key), `value` | `relays.set` | JSON by key. `relays` is `{relays, token?}`: every machine uses exactly these relays. |
| `imesh_kv` | `key` (key), `value` | `kv.put` | Any JSON. |

The example app (`examples/app.rs`) adds `example_notes (who, text)` and `example_items (key, value)`.

Every table is a cr-sqlite CRDT table. Changes carry no signature. Only admitted links deliver them. Merges run per column: the side that changed the column more times wins, then the larger value, then the site ID.

## Tickets

- Machine ticket: iroh's `EndpointTicket` for the machine's current `EndpointAddr`. It starts with `endpoint`.
- Join ticket: `imeshjoin` followed by base64url (no padding) of the JSON `cluster.add` result. It holds a secret only if the cluster relay setting has a token.

## Timers

| Timer | Value |
|---|---|
| Link keepalive, and the silence after which a member is `down` | every 1 s, 4 s |
| Link connect timeout | 10 s |
| Link write timeout | 5 s |
| Link reconnect backoff | 1 s, doubling, at most 4 s |
| Corrosion anti-entropy sync (`max_sync_backoff`) | at most every 5 s |
| Corrosion gossip QUIC idle timeout | 10 s |
| Refresh of the in-memory cluster tables | every 250 ms, and after each local write |
| Background loop (publish own row, links, relays, enforce) | on change, or every 2 s; each 2 s tick also steers SWIM |
| `cluster.join` wait for this machine's member row | at most 60 s |
| Connect timeout for `Machine::connect` and forwarded calls | 20 s |
| CLI connection attempt, including the probe | 20 s |
| Wait for the home relay before sharing addresses | at most 10 s |
| Idle gossip sockets in the tunnel | closed after 600 s |
