# Research notes: Uncloud internals

Source: https://uncloud.run/docs/ and the checkout at ~/code/tmp/uncloud, HEAD f68c985, read 2026-09-23.
Paths are relative to that checkout. HEAD moved the API socket to /run/uncloud/api/uncloud.sock. The live docs still say /run/uncloud/uncloud.sock.

## Parts
- `uc` CLI talks gRPC to one machine (the entry point). That machine forwards to others.
- `uncloudd` runs as root under systemd. It has three gRPC servers: the machine API (machine.sock, real services), the cluster API (api/uncloud.sock, transparent proxy only), and the network API on [fdcc:...]:51000 over WireGuard (transparent proxy). See internal/machine/machine.go.
- Corrosion (cr-sqlite plus gossip) runs in a Docker container and holds all cluster state. Its gossip is plaintext because WireGuard encrypts.
- WireGuard full mesh on UDP 51820. There is no STUN, hole punching, or relay. It only rotates endpoints (internal/machine/network/peer.go:78-135).

## machine init / add
- SSH via the system `ssh` binary with ControlMaster, BatchMode=yes, StrictHostKeyChecking=accept-new (pkg/client/connector/sshcli.go:116-157).
- If the user is not root, the CLI runs `sudo true` and fails with guidance if sudo needs a password (internal/cli/machine.go:56-99).
- The install script is embedded in the CLI and piped as base64 through `sudo bash` (internal/cli/machine.go:33-52).
- The install script makes the system group `uncloud`, adds the SSH user to it, downloads the daemon from GitHub releases (no checksum), and writes uncloud.socket (root:uncloud 0660, dir 0750) and uncloud.service (Type=notify, Restart=always).
- The CLI reconnects after install so the new group membership applies (internal/cli/cli.go:603-609).
- The machine key is made on first daemon start. The cluster assigns the machine ID in AddMachine.
- add: the CLI gets a token (public key plus endpoints, no secret) from the new machine, calls Cluster.AddMachine on an existing member, then Machine.JoinCluster on the new machine with the peer list and a minimum store version.
- The new machine waits until its store reaches that version before it acts on the store.

## CLI connection
- Connection types: ssh (system ssh runs `uncloudd dial-stdio`), ssh_go (streamlocal forward), tcp, unix.
- `--connect` ignores the config. Otherwise the CLI uses `--context` or current_context and tries connections in order.
- Env overrides: UNCLOUD_CONFIG, UNCLOUD_CONTEXT, UNCLOUD_CONNECT.
- Config at ~/.config/uncloud/config.yaml: current_context, contexts.<name>.connections[] with one transport each plus ssh_key_file and machine_id.
- `uc ctx ls|use|conn`. `ctx conn` moves the chosen connection to index 0.
- Routing metadata: `machine: <name|id>` for one target, `machines: a,b|*` for fan-out. The proxy adds per-machine metadata and error envelopes to each reply. A proxy-authority header stops a second hop (internal/machine/api/proxy/).

## State and membership
- Corrosion tables: cluster (KV), machines (id, info JSON), containers.
- Each machine republishes its own MachineInfo every 60 s and on change.
- Every machine subscribes to the machines table and rebuilds WireGuard peers. An empty result is ignored (issue #155).
- Status UP, SUSPECT, DOWN comes from Corrosion SWIM, not from the store.
- Removal: stop the containers, reset the target (it wipes its data), delete the row through the entry machine, drop the config connection.

## Sharp edges
- There is no cryptographic membership. Socket access on any member can add any public key.
- Plaintext unauthenticated gRPC on port 51000 inside the mesh.
- Name and subnet uniqueness is check-then-insert on a local replica, so it races under partition.
- Two NATed machines cannot connect.
- A join blocks forever if the new machine cannot reach peers.
- The daemon binary download is not verified.

## Lessons for imesh
1. Keep the UX: init, add, rm, ls, contexts, ordered fallback, --connect, env overrides, auto-use of the local socket.
2. The iroh EndpointId is the machine ID and its address. No overlay IPs, no endpoint rotation.
3. Make joins cryptographic. Authenticate every inter-machine call by EndpointId.
4. Copy the routing model: one entry machine, one target or fan-out, per-machine errors, one hop.
5. Keep dial-stdio over SSH. Socket 0660 root:imesh.
6. Each machine owns its own record. Liveness comes from connections, not the store.
7. Verify the downloaded binary.
8. Removal revokes at once through the allow list.
