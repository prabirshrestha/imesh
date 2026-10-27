# About the imesh design

imesh turns a set of computers into one cluster of equal machines. Each machine runs one daemon. The daemons talk over [iroh](https://docs.iroh.computer), so a cloud VM, a server in the office, a Windows laptop, and a Raspberry Pi at home join the same cluster without VPN setup, port forwarding, or DNS. There is no control plane and no quorum. You reach the whole cluster through any one machine.

imesh copies the user experience of [Uncloud](https://uncloud.run/docs/) (`uc machine init`, contexts, connections) and the peer model of [Talos KubeSpan](https://docs.siderolabs.com/talos/v1.14/networking/kubespan). It replaces WireGuard with iroh QUIC connections. Other projects build on imesh as a Rust library: they add API methods and ALPN protocols to the same daemon.

[`crates/imesh`](../crates/imesh) (the library) and [`crates/imesh-cli`](../crates/imesh-cli) (the `imesh` command) build this design. [verification.md](verification.md) lists every check that proves a claim here and how to run it. [comparison.md](comparison.md) explains why imesh uses embedded Corrosion for its state, and not its own record engine or iroh-docs. [reviews.md](reviews.md) records the adversarial reviews and what changed because of them.

## Terms

- **Machine.** A computer that runs the imesh daemon. Its iroh key is its identity.
- **Member.** A machine that the cluster admitted and has not removed.
- **Cluster.** The members that share one replicated state.
- **Entry machine.** The machine the CLI connects to. It serves the call, or sends it on to another machine.
- **Context.** Your local list of ways to reach one cluster, in `~/.config/imesh/config.toml`.
- **ALPN.** The protocol name that a QUIC connection asks for when it opens. iroh routes each connection to the handler for its ALPN.
- **Relay.** An iroh relay server. Two machines that cannot reach each other directly send encrypted packets through it. It speaks HTTP and WebSocket over TCP.
- **Home relay.** The one relay an iroh endpoint stays connected to. Other endpoints reach it through that relay.
- **n0.** number 0, the company behind iroh. It runs public relays and an address lookup service that iroh uses by default.
- **pkarr.** Signed address records that iroh publishes and resolves over HTTP or DNS.

## Goals and limits

imesh is for:

- Clusters of 1 to about 100 machines that you own.
- Machines on any network: public IP, NAT, carrier-grade NAT, or a firewall that blocks UDP.
- Linux and Windows. Windows may have no SSH server. The CLI also runs on macOS; the daemon does not (see [Windows](#windows)).
- No root at run time. The daemon runs as a normal user.
- One static binary: about 44 MB on Linux, with about 72 to 74 MB of daemon RSS. See [Footprint](#footprint).
- A base for other tools. They add their own protocols, methods, and SQL tables.

imesh does not:

- Give machines IP addresses or a virtual network interface. It connects processes, not kernels. An app that needs a TCP port on another machine opens an iroh stream to it. See [Carry app traffic](#carry-app-traffic).
- Run containers or schedule work. Those belong to the apps that build on imesh.
- Give strong consistency. State is eventually consistent. See [Consistency](#consistency).

## The parts

```
 laptop                                  cluster
+----------------------+      ssh        +--------------------------------------------+
| imesh CLI            |---------------->| machine A                                  |
|  ~/.config/imesh/    |  dial-stdio     |  imesh daemon, as the SSH user, no root    |
|   config.toml        |                 |   ~/.local/share/imesh/                    |
|   identity.key       |      iroh       |     imesh.sock (0600), secret.key,         |
|                      |                 |     corrosion-<id>/state.db                |
|                      |---------------->|   iroh endpoint, ALPNs: imesh/api/0,       |
+----------------------+  imesh/api/0    |     imesh/corrosion/1, app ALPNs           |
                                         +--------------------------------------------+
                                              |  iroh QUIC: direct, or through a relay
                                    +---------+---------+
                                    |                   |
                               machine B           machine C
```

- **The binary.** One file, `imesh`. `imesh daemon` runs the daemon. Every other subcommand is the CLI. `imesh dial-stdio` is a hidden helper that the CLI runs over SSH.
- **The daemon.** One iroh endpoint, one iroh `Router`, the local API, an embedded Corrosion agent with the SQLite database, and one link to every other member.
- **The CLI.** It reads contexts and reaches one machine through SSH, iroh, or the local API. That machine does the rest.
- **The library.** The `imesh` crate. The binary is a thin CLI on top of it.

## Identity: the machine key is the machine

Each daemon creates an Ed25519 secret key on first start, in `/var/lib/imesh/secret.key` (`%LOCALAPPDATA%\imesh\secret.key` on Windows). The public key is the iroh `EndpointId`. imesh uses that one value for everything:

- It is the machine ID in every table.
- It is the address you dial. iroh finds the machine by this key.
- It is what every connection proves in its TLS handshake.

KubeSpan keeps a separate node ID and WireGuard key. Uncloud keeps a WireGuard key and a machine ID that the cluster assigns. One key removes the tables that map between them.

The CLI has its own key in `~/.config/imesh/identity.key`. `imesh key` prints its public half. `machine init` authorizes it, because the user proved admin rights through SSH.

## Membership and admission

iroh does all authentication. Every connection proves the remote key in its TLS handshake, and imesh adds no tokens, certificates, or signatures of its own. The one thing iroh cannot know is which keys belong to this cluster. imesh keeps that list in the replicated database and checks it with iroh's connection hook, and with iroh-relay's access control on an embedded relay.

### Who is a member

Membership lives in the replicated database, in the `imesh_*` tables:

| Table | Meaning |
|---|---|
| `imesh_cluster` | Cluster ID and founder key. Written once by `cluster.init`. A join into another cluster is refused. |
| `imesh_members` | `<id>` is a member, with a name. `removed = 1` means removed. |
| `imesh_machines` | What `<id>` publishes about itself: name, current iroh address, version, OS, architecture, labels, and a relay it serves. Removal deletes the row. |
| `imesh_clients` | A CLI key that may use the API over iroh. |
| `imesh_settings` | Cluster settings. The `relays` row holds the relays every machine uses, and their token. See [Relays](#relays-firewalls-and-self-hosting). |
| `imesh_kv` | Values for the `kv.*` methods. |

Apps keep their data in their own tables.

The *members* are the keys with an `imesh_members` row where `removed` is 0.

All members are equal and trust each other. Changes reach a machine only from itself, from its local API, or over a link that iroh authenticated and the hook admitted. imesh does not sign changes or check who wrote which row. A compromised member can write any row, as a compromised node can in Uncloud or KubeSpan. Remove a machine that you no longer trust.

Removal cannot be undone. imesh never sets `removed` back to 0, so a removal beats a stale add after a partition heals (see [Writes and merges](#writes-and-merges)). A removed machine must reset and join with a new key.

### Admission at the transport

An iroh `EndpointHooks::after_handshake` hook runs on every connection, incoming and outgoing, after the TLS handshake and before any protocol handler. It allows the connection if the remote key is:

- a member, on any ALPN, or
- a key in `imesh_clients`, on `imesh/api/0` only, or
- a peer named in a `cluster.join` that is running.

Anything else is closed with error code 403. This covers app ALPNs too. An app protocol never sees a stranger.

The hook runs only when a connection opens. So the hook also records every connection it admits, and each time the state changes, the daemon closes the open connections of keys that are no longer allowed (`admission::enforce`). Removing a member or revoking a CLI key closes its link, API, and app connections at once.

### Callers

Every API call knows who made it:

- `Local`: the local socket or pipe, which includes SSH through `dial-stdio`.
- `Client(key)`: a CLI key over iroh.
- `Member(id)`: another machine over iroh.

The one-hop rule uses this: a machine sends a call on to another machine only when the caller is local or a CLI key. It never sends on a call that a member sent, so calls cannot loop. Apps use the caller to decide who may call their methods.

### How a machine joins over SSH

`imesh machine add user@new-host` runs these steps:

1. The CLI connects over SSH. If a daemon already runs there and is in a cluster, it stops here and changes nothing.
2. It installs or updates the daemon, and reads the new machine's key (`machine.inspect`).
3. It calls `cluster.add {id, name}` on the entry machine. The entry machine writes the `imesh_members` row and returns the cluster ID, the members' addresses, and the cluster relay setting.
4. It calls `cluster.join` on the new machine with that result. The new machine uses the cluster relays, trusts the named peers for this call only, links to them, and asks SWIM to contact them.
5. The new machine waits up to 60 s until Corrosion brings it the `imesh_cluster` row and its own `imesh_members` row. It checks the cluster ID. Then it publishes its `imesh_machines` row and links to all members. A join that fails can run again into the same cluster. Rows of another cluster need a reset first.

SSH access is the proof of admin rights. No join token exists, so no token can leak.

### Joining without SSH

Some machines have no SSH server, for example a Windows laptop. The person at that machine installs the daemon and trades two tickets with an admin:

1. On the new machine: `imesh daemon install`, then `imesh machine id`. It prints iroh's `EndpointTicket`: the machine key and its current addresses.
2. On the admin's computer: `imesh machine add <ticket> --name <name>`. The entry machine admits the key. The CLI prints a join ticket, `imeshjoin...`.
3. On the new machine: `imesh machine join <join ticket>`.

The join ticket is the `cluster.add` result: the cluster ID, member addresses, and the cluster relay setting. It holds a secret only if the relay setting has a token. Authentication stays with iroh: the new machine proves its key when it connects.

## State and sync

### The data model

The state is one SQLite database. An embedded [Corrosion](https://github.com/superfly/corrosion) agent runs it in the daemon. Corrosion is the engine that Uncloud uses: cr-sqlite CRDT tables, SWIM membership, and QUIC gossip. Every machine keeps the whole database, in `<data dir>/corrosion-<id>/state.db`.

imesh's own tables start with `imesh_`:

| Table | Content |
|---|---|
| `imesh_cluster` | One row: the cluster ID and the founder key. |
| `imesh_members` | One row per admitted machine: its key, its name, and `removed`. |
| `imesh_machines` | One row per machine: what it publishes about itself, as JSON. |
| `imesh_clients` | The CLI keys that may use the API over iroh. |
| `imesh_settings` | Cluster settings by key, as JSON. `relays` is the cluster relay setting. |
| `imesh_kv` | JSON values by key, for the `kv.*` methods. |

Apps add their own tables. See [Schema changes](#schema-changes).

The cluster tables are small and read often. The daemon keeps a copy of them in memory (`Snapshot`) and reads the database again every 250 ms and after each local write.

### Writes and merges

A write is one SQL transaction. It commits on this machine first. Then Corrosion sends its changes to the other members. One transaction is one replicated change.

cr-sqlite merges per column, not per row. Two machines that change different columns of one row both keep their change. For one column, the side that changed the column more times wins, then the larger value, then the site ID. Time plays no part, so clock skew has no effect. This is not "the later write wins": in a split, n1 writes `z-early` and n2 later writes `a-later`, once each. After the heal, both keep `z-early`, because `z` sorts after `a`. An app that needs "the latest setting wins" must write a timestamp or version into the row and compare it itself.

A removal is `removed = 1` in `imesh_members`. imesh never writes `removed = 0` after the insert, and `cluster.add` refuses a key that is removed. So a removal beats a stale add after a split heals.

### Durable writes

An API call answers only after its transaction commits in SQLite. imesh patches Corrosion to open the database with `PRAGMA synchronous = FULL` instead of `NORMAL` (`vendor/corrosion.patch`), so a committed write survives a power loss, not only a crash of the process. E2e stage 20 kills a cut-off machine with `kill -9` right after a write and reads the write back.

### How writes spread

Every member dials every other member on `imesh/corrosion/1` and keeps that link. Both sides dial, so a pair stays linked when only one side can reach the other. The link carries typed frames: a keepalive, Corrosion's gossip, and the machine's table list.

- **Keepalive, every 1 s.** A member that sent nothing for 4 s is `down`. The link then reconnects with a backoff from 1 s to 4 s.
- **Gossip.** Corrosion broadcasts each change to the other members, and SWIM checks which members are alive. Corrosion sends its gossip (SWIM over UDP, and QUIC) on the machine's own loopback address, `127.x.y.z:51001`, derived from its key (`state::lo`). For every member, the daemon binds that member's loopback address, takes the datagrams, and sends them over the member's link. The daemon on the other side sends them on to its Corrosion from the same addresses (`state::tunnel`). So Corrosion traffic between machines goes only over iroh, and a machine with UDP blocked still syncs through a relay over TCP.
- **Anti-entropy.** Corrosion syncs with a random member at least every 5 s (`max_sync_backoff`, 15 s in Corrosion's default). It repairs what gossip missed: a split, a paused machine, or gossip that the schema gate held back.

The daemon also steers SWIM. Every 2 s it announces members whose link is up and SWIM does not see yet, and it marks removed members as down for good.

Measured in e2e: a write reached the other machines in under 1 s. 1,000 new 1 KiB rows were all on another machine 0.2 s after the write began. After a 45 s split, all machines converged 4 to 5 s after the heal. With Corrosion's default 15 s backoff it took 21 s.

### Schema changes

Apps add tables in two ways:

- **In the binary.** `Builder::schema(sql)` takes complete `CREATE TABLE` and `CREATE INDEX` statements. At start, a machine applies only what they add to its database. The database can already have more: columns added at run time, or tables taken from a member with a newer app. The daemon does not start if the compiled tables conflict with the database.
- **At run time.** `imesh schema apply <file>` (the `schema.apply` method) applies them on the entry machine. The members then take them over their links.

A schema can only grow:

- New tables.
- New columns that are nullable or have a default. A NOT NULL column without a default is refused, also in a new table: a machine that has the table with rows could never add it.
- New indexes.

A schema holds only `CREATE TABLE` and `CREATE INDEX` statements.

A statement for a table that exists gives the complete new table. Dropping or changing a table or column is refused on the machine where you apply it, and reaches no one. Corrosion has no destructive mode. Tables named `imesh_*` belong to imesh: `schema.apply` and `sql.execute` refuse them. Table names that start with `__`, `sqlite_`, or `crsql`, or contain `__crsql_`, are refused too: the gate leaves such names out, so it could not protect their rows. `sql.execute` runs only `INSERT`, `UPDATE`, `DELETE`, and `REPLACE`, optionally with `WITH`, so tables change only through a schema.

### The schema gate

Corrosion replicates rows, not tables. A change for a table or column that a machine does not have yet is counted as received, and it is never fetched again. So plain Corrosion loses rows silently. E2e stage 21 found this: a paused machine missed rows that another machine wrote to a new column and a new table while it was away.

imesh adds a gate on each link (`daemon::links`, `state::schema`):

1. Each side sends its tables and columns when the link starts, and again whenever they change.
2. Each side applies what the other has and it lacks: new tables, new columns, and indexes. This is additive only. For a table it has, a machine keeps its own definition and adds only the new columns, so the same column written with its constraints in another order (for example `PRIMARY KEY NOT NULL` and `NOT NULL PRIMARY KEY`) causes no conflict. A default written two ways (`DEFAULT 0` and `DEFAULT (0)`) does count as a different column. An apply that fails for a passing reason runs again at the next keepalive, each second.
3. Gossip passes in either direction only while both sides have the same tables and columns, and never while a local schema change runs. Gossip that the gate holds back is dropped. To Corrosion that looks like packet loss: QUIC sends it again, or the next anti-entropy sync repairs it.

With the gate, the paused machine had all rows 1 s after it resumed.

A conflict happens when both sides added different columns to one table, for example in a split. The link then stays closed for rows, and `imesh schema ls` shows `schema conflict: table <name>: ...` for that member. Apply a schema with both columns on either machine. The gate spreads it and the link opens again. In stage 21b, the rows from both sides arrived 2 s after the fix.

### Consistency

The state is AP. Any machine accepts writes while it is cut off. When the network heals, the merge rule above decides each column, and rows written on both sides survive. This is enough for membership, addresses, and settings.

It is not enough for anything that must be unique across the cluster. Names are an example: `cluster.add` checks names against local state only, so two sides of a split can each admit a machine with the same name. The CLI then refuses the ambiguous name and asks for the ID. Apps that need uniqueness must avoid shared allocation, for example by deriving values from the machine key or by giving each machine its own rows.

### Limits

- Every machine holds the whole database. It is for membership, settings, and app data of moderate size, not large payloads.
- Deleted rows leave cr-sqlite clock entries. Nothing collects them yet.
- A schema cannot drop or change a table or column.
- Corrosion is a 0.x beta at a git commit, and cr-sqlite is an alpha C library built with a 2023 nightly toolchain.

### Other sync engines

imesh first had its own record engine, now kept as [`prototypes/imesh-records`](../prototypes/imesh-records). A prototype with iroh-docs (B) used the same CLI and test suite too. [comparison.md](comparison.md) explains why imesh switched to Corrosion, and compares the engines.

## Connecting from the CLI

### Contexts

`~/.config/imesh/config.toml` holds contexts. A context is your view of a cluster: which machines to connect through, and in which order. Two people, or you in two places, can reach the same cluster through different contexts.

```toml
current_context = "home"

[[contexts.home.connections]]
ssh = "deploy@10.0.0.5"
machine = "56ee5ed9..."

[[contexts.home.connections]]
iroh = "56ee5ed9..."
machine = "56ee5ed9..."

[[contexts.travel.connections]]
iroh = "56ee5ed9..."
relay = "https://relay.example.com"
relay_token = "..."
```

The CLI picks the connections like this:

1. If `--connect` or `IMESH_CONNECT` is set, the CLI uses only that connection. It does not read or change the config.
2. Else it takes the context from `--context`, `IMESH_CONTEXT`, or `current_context`.
3. If the config has no contexts, it uses the local API of this machine.
4. It tries each connection in order. Each attempt, including a `ping`, has a 20 s deadline (100 s when it must start a devtunnel client), so a dead or hung machine fails and the next one is tried.

`imesh ctx ls`, `imesh ctx use <name>`, and `imesh ctx conn [n]` list, switch, and reorder. `imesh ctx add <name> <connection>...` makes a context by hand, for example for a second laptop. `machine init` makes a new context current.

A second person or laptop runs `imesh key`, an admin runs `imesh client add <key>`, and the second person adds an `iroh://` connection with `ctx add`. `imesh client rm <key>` revokes it.

### Connection types

| Type | Config | How it works |
|---|---|---|
| SSH | `ssh = "user@host[:port]"`, optional `ssh_key_file` | Runs the system `ssh`, so `~/.ssh/config`, agents, and jump hosts work. Connections are shared through `~/.ssh/imesh-cm` when the current user owns that directory. Each call runs `imesh dial-stdio` on the host. The SSH user must be the daemon's user, or, for a system daemon, in the group `imesh`. |
| iroh | `iroh = "<endpoint id>"`, optional `relay`, `relay_token`, `relay_ca`, `addrs`, `no_n0` | Opens `imesh/api/0` with the CLI key. Direct when possible, else through a relay. Needs the CLI key in `imesh_clients`. |
| Local | `local = "<socket path>"`, or the named pipe on Windows | The local API of the machine the CLI runs on. |

### One hop to any machine

Every API call can name a target machine. The entry machine sends the call on to that machine over `imesh/api/0` and relays the frames back:

```
CLI --(ssh or iroh)--> entry machine --(iroh, imesh/api/0)--> target machine
```

`imesh call <method> -m <name>` uses this path. So does `machine rm`, to reset the removed machine.

A forward goes one hop only: a machine does not forward a call that another member sent. When the entry machine forwards a call for a CLI key, it names that key in the request (`via`), and the target serves the call as that key. A target accepts `via` only from a member, and only for a key that is still authorized. So forwarding gives a CLI key no more rights than a direct call.

## The API

### Framing

One call uses one byte stream:

1. The client sends a `Request`: `{"to": <id or null>, "method": "...", "params": {...}}`. A forwarded request can also carry `via`, described above.
2. The server answers with one frame: `{"ok": ...}` or `{"err": "..."}`.

Each frame is JSON with a 4-byte big-endian length prefix, at most 16 MiB. The same bytes travel over the unix socket, SSH `dial-stdio`, the Windows named pipe, and an iroh bidirectional stream.

irpc 0.17 does not fit here. Its remote transport is QUIC streams only, and the SSH, socket, and pipe paths are plain byte streams. Apps that talk machine to machine can still use irpc on their own ALPN.

### Built-in methods

[reference.md](reference.md#methods) lists them. The groups are `ping`, `methods`, and `whoami`; `machine.*`; `cluster.*`; `client.*`; `relays.*`; `schema.*`; `sql.*`; and `kv.*`.

## Build on imesh

The `imesh` crate is a library, like iroh. It has no command line and reads no environment variables, config files, or default paths: an app passes plain structs and chooses those itself. The `imesh` command (`crates/imesh-cli`) is one program built on it. An app builds its own daemon:

```rust
use imesh::daemon::{Builder, Caller, Config, LocalApi, Machine, serve};

serve(
    || {
        Builder::new(config.clone())
            .schema("CREATE TABLE myapp_items (id TEXT NOT NULL PRIMARY KEY, body TEXT NOT NULL DEFAULT '');")
            .method("myapp.hello", |m: Machine, caller: Caller, params| async move { ... })
            .protocol(b"myapp/files/0", MyHandler)
            .protocol_with(b"myapp/blobs/0", |m| make_blobs(m.endpoint(), m.state()))
    },
    &LocalApi { path, access },
    imesh::os::shutdown_signal(),
)
.await
```

- A method gets the `Machine`, the `Caller`, and JSON params, and returns JSON. The CLI and other machines call it by name. Built-in names cannot be replaced.
- A protocol is an iroh `ProtocolHandler`. The admission hook runs first, so it sees only members.
- `schema` adds app tables. Every machine applies them at start. Name them after the app, not `imesh_*`.
- `protocol_with` builds a handler after the endpoint binds, with the `Machine`: its endpoint, state, and members. iroh-blobs and iroh-docs need the endpoint this way.
- `Machine::state()` gives the replicated database, a `Store`: `query`, `write`, `changes`, `tables`, and `apply_schema`.
- `Machine::connect(id, alpn)` opens a connection to a member on any ALPN, with the address from the state.
- The cluster operations are `Machine` methods: `init_cluster`, `admit` (returns a `JoinTicket`), `join`, `remove_member`, `authorize_client`, `set_cluster_relays`, and `apply_schema`. The API methods call them.
- A machine locks its data directory, so a second machine cannot run on the same key and database.

iroh fixes the ALPN list when the `Router` spawns. Every machine usually runs the same binary, so every machine offers the same ALPNs. A machine that lacks an ALPN refuses that connection. The caller gets an error.

[extending.md](extending.md) shows a full example.

### Carry app traffic

imesh has no virtual network. An app that needs a TCP service on another machine speaks its own ALPN, or forwards a TCP port over an iroh stream as [dumbpipe](https://github.com/n0-computer/dumbpipe) does. This is the largest difference from Uncloud and KubeSpan. imesh chooses no kernel changes, no root, and no IP planning.

## Relays, firewalls, and self-hosting

By default, the daemon uses the n0 relays and n0 address lookup (`presets::N0`). You do not configure anything, and machines behind NAT find each other. Both the daemon and the CLI send relay traffic through an HTTP proxy when `HTTPS_PROXY` or `HTTP_PROXY` names one (`proxy_from_env`).

### When UDP is blocked

iroh tries a direct UDP path first. When a firewall blocks UDP, iroh still connects through a relay over TCP. The payload stays end-to-end encrypted between the two machines, so the relay sees only which keys talk to each other.

Any machine can serve as the cluster's relay. The daemon embeds the `iroh-relay` server:

```
imesh machine init deploy@gateway --serve-relay 0.0.0.0:3340 --relay http://gateway.example.com:3340 --no-n0
imesh machine add deploy@locked-down-host
```

- `--serve-relay <addr>` runs a relay on that TCP address, in the same daemon.
- The relay's `AccessControl` lets in only members, CLI keys, and the machine itself.
- `--relay <url>` makes the machine use that relay instead of the n0 relays.
- `--no-n0` also turns off n0 address lookup.
- `machine add` copies the entry machine's `--relay` and `--no-n0` settings when you give neither.

E2e stage 18 blocked all UDP on a machine before it joined. It joined through the relay that another machine served and synced both ways.

The embedded relay serves plain HTTP by default. iroh encrypts the payload, so that is enough for iroh. A relay token sent over plain HTTP can be read on the path, so use TLS when you use tokens. Some firewalls allow only TLS on port 443. For both cases, serve the relay with a certificate: `--serve-relay-cert` and `--serve-relay-key`. Let's Encrypt is not wired in yet.

### One relay that every member can reach

An iroh endpoint has one home relay, and other endpoints reach it through that relay. So a machine that can reach only one relay can join only if every member uses that relay too:

```
imesh relay set https://relay.example.com [--token <token>]
```

The setting is the `relays` row of the replicated table `imesh_settings`. Every daemon applies it at runtime: it adds those relays, removes every other relay, including the ones it started with, and moves its home relay within about 10 s. `imesh relay set` with no URLs clears the setting, and every machine goes back to the relays it started with. The token lives in the setting, so every member can use a token-protected relay. `imesh relay ls` shows the setting, the relays that machines serve, and the relays the entry machine uses.

### Expose a served relay through a tunnel

A logged-in `devtunnel` CLI can carry the relay without anonymous access. The relay
host and each client use the same tunnel and port. The CLI owns the login and
refreshes its credentials; imesh does not read or copy its tokens.

On the relay host:

```bash
imesh daemon install --serve-relay 127.0.0.1:3340 \
  --expose-relay devtunnel:my-relay --relay http://127.0.0.1:3340 --no-n0 --relay-only
```

On each other machine, use the full tunnel ID, including its service cluster:

```bash
imesh daemon install --devtunnel devtunnel:my-relay.use:3340 --no-n0 --relay-only
```

Then initialize and join the machines through their local APIs or SSH as usual.
`relay set http://127.0.0.1:3340` selects this relay for the cluster. On the host,
that URL reaches the embedded relay. On each client, it reaches the port that
`devtunnel connect` forwards to that same relay.

- `--expose-relay` finds or creates a tunnel with the `imesh` and machine labels,
  without anonymous access, and supervises `devtunnel host`. An existing tunnel
  with anonymous access is refused. Prefer an explicit tunnel ID for setup.
- `--devtunnel` supervises `devtunnel connect <id>`. It waits for the CLI to report
  the exact loopback port before it starts the endpoint. A busy port fails;
  imesh does not accept the CLI's alternate-port fallback.
- Both processes run as the daemon's user, which must already be logged in to
  devtunnel and have permission on that tunnel. A login alone does not grant
  access to someone else's tunnel. No browser login or token prompt runs in the daemon.
- Each client must configure its own `--devtunnel`. A join ticket has the relay URL,
  not local CLI setup or login credentials. One dedicated, single-port private tunnel per cluster is
  supported by this setup. Do not combine distinct tunnels at the same loopback URL.
- The CLI can use a private tunnel too. Set `devtunnel = "devtunnel:my-relay.use:3340"`
  on an iroh context connection. The port must be free, so use the local API on a
  machine whose daemon already owns that forwarding port.
- Dropping the daemon or client stops its own CLI child. A daemon restarts a failed
  child indefinitely, with retry delays of 1, 2, 4, 8, 16, 32, then 60 seconds.
  Retries reset to 1 second after the ready child stays running for 60 seconds.
  The host has 60 seconds to report that it is ready before imesh kills it and retries.
  A running child manages its own network reconnects; imesh does not probe for hangs
  after readiness.
  Initial client startup still returns an error if the tunnel cannot become ready. It does not stop unrelated devtunnel processes or delete tunnels on shutdown.
- The relay URL reported by imesh is the loopback URL, not the private HTTPS web
  URL. The tunnel ID appears in the host log and `devtunnel list --all-labels imesh`.
- The CLI must be on the service's PATH. A Linux user service has a separate PATH
  from fish or bash; add the devtunnel directory in a systemd service drop-in.
- Devtunnel checks account access; iroh checks machine keys and cluster membership.
  The forwarded payload remains encrypted by iroh. No inbound firewall port is needed.

Do not point iroh at a private devtunnel HTTPS URL with `--relay-token`: that option
is for the relay's bearer-token check, not the CLI's account login. No anonymous
fallback exists. If login expires or access is denied, the tunnel stays unavailable
until the user fixes that login or access.

### Relay tokens and certificates

- `--relay-token <token>` sends `Authorization: Bearer <token>` to the relays in `--relay`, through iroh's `RelayConfig::with_auth_token`. For relays that every member uses, prefer `relay set --token`.
- `--relay-ca <pem>` trusts extra CA certificates for relay TLS, through iroh's `CaTlsConfig::with_extra_roots`.
- `--serve-relay-token <token>` makes the served relay require that token as well as membership.
- These settings go into `/etc/imesh/env`, mode 0600, or `%LOCALAPPDATA%\imesh\env` on Windows. `machine.inspect` never returns tokens.
- A CLI connection has its own `relay_token` and `relay_ca`.

### Finding machines

imesh never needs a DNS zone or a managed lookup service. [research/address-lookup.md](research/address-lookup.md) has the measurements behind these choices.

- **Cluster tables first.** Every `imesh_machines` row holds that machine's current relay URL and direct addresses. Each daemon puts them into iroh's `MemoryLookup` with `set_endpoint_info`. With n0 unreachable, members still connected in 2 to 8 ms this way.
- **Redial on a move.** A running dial does not see new addresses. When a peer that is not `up` publishes a new address, the daemon restarts its link at once.
- **n0 as a fallback.** It helps only when a peer's row is stale and its relay changed. `--no-n0` turns it off.
- **`--pkarr <url>`.** Publish and resolve through a self-hosted `iroh-dns-server` over plain HTTP. It needs one HTTP port and no DNS zone.
- **`--mdns`.** Find machines on the same LAN. It is off by default: it publishes unsigned addresses, listens only on the default-route interface, and fails on hosts without a default route.
- **No DHT.** It took 2 to 13 s, needs UDP to the internet, and adds nothing next to a relay.

### Name resolution

The daemon and the CLI resolve host names through the operating system. On the Windows VM, iroh's own resolver failed with "Network unreachable" while the OS resolved the names, so the machine had no relay. `OsResolver` (`src/net/dns.rs`) sends A and AAAA lookups to the OS and keeps iroh's resolver for TXT records.

## Windows

The same binary runs on Windows:

| | Linux | Windows |
|---|---|---|
| Local API | `~/.local/share/imesh/imesh.sock`; system: `/run/imesh/imesh.sock` | named pipe `\\.\pipe\imesh-<user>` |
| Service | systemd user service, or a detached process that the crontab starts at boot; system: a systemd unit | scheduled task for the current user, at logon, restart on failure, headless through `conhost --headless`; system: a Windows service at boot as `NT SERVICE\imesh` |
| Settings | `~/.config/imesh/daemon.env`; system: `/etc/imesh/env` | `%LOCALAPPDATA%\imesh\env`; system: `%ProgramData%\imesh\env` |
| Data | `~/.local/share/imesh`; system: `/var/lib/imesh` | `%LOCALAPPDATA%\imesh`; system: `%ProgramData%\imesh` |
| cr-sqlite | Corrosion's bundled library on glibc; linked in statically on musl | linked in statically, no DLL |
| Logs | journald, or `~/.local/share/imesh/imesh.log` for a detached daemon | `%LOCALAPPDATA%\imesh\imesh.log`, not rotated |
| Install | `imesh machine init/add` over SSH, or `imesh daemon install`, no root | `imesh daemon install`, no admin; `--system` needs an elevated shell |
| Remove | `imesh daemon uninstall [--purge] [--system]` | `imesh daemon uninstall [--purge] [--system]` |

- The pipe name includes the user. Another user could still create a pipe with that name first, so the client checks that the process that serves the pipe runs as its own user before it sends anything. The daemon's pipe has an access list for its owner and SYSTEM only, and refuses remote clients.
- A user install is a scheduled task. It first tries a task that runs without a logon session (S4U). Windows refused that for a normal user in the test, so the task runs while the user is logged on.
- To run before anyone logs in, at the login screen, install the Windows service with `--system` from an elevated shell. It runs as the virtual account `NT SERVICE\imesh`, with its own data directory that only it, SYSTEM, and Administrators may open. The members of the local group `imesh` may use its pipe. This is the same idea as the Linux system unit with the socket group `imesh`.
- On Linux, a user daemon already starts at boot with no login: a systemd user service with linger, or the crontab's `@reboot` line.
- Reinstalling always stops the running daemon first, so new settings apply.

### macOS

The CLI runs on macOS. The daemon does not. Each machine's Corrosion gossips on its own address in 127.0.0.0/8, and the daemon binds the addresses of its members there too. Linux and Windows answer all of 127.0.0.0/8 on loopback. macOS answers only 127.0.0.1.

## Labels

`--label key=value` adds labels to a machine. They go into its `imesh_machines` row, and `machine.inspect` shows them. Labels are metadata for people and apps. imesh does not use them to connect, because machines find each other by key.

## Operating a cluster

- **Upgrade.** `imesh machine update user@host` uploads the current Linux binary and restarts the daemon. Settings you do not pass stay as they are. On Windows, run `imesh daemon install` again with the new binary.
- **Logs.** `journalctl --user -u imesh` for a user service, `~/.local/share/imesh/imesh.log` for a detached daemon, `journalctl -u imesh` for a system service, `%LOCALAPPDATA%\imesh\imesh.log` on Windows. `IMESH_LOG=debug` in the env file shows link errors. `imesh machine ls -w` shows each link's last contact and last error.
- **Reset a machine that was removed while it was down.** `imesh --connect user@host call machine.reset`.
- **Move a machine.** Copy `secret.key` and the `corrosion-<id>` directory to the new host before its first start. Its addresses update by themselves. Fix `ssh =` entries in your contexts by hand.
- **Rotate a CLI key.** Run `imesh key` on the new key's computer, `imesh client add` for it, and `imesh client rm` for the old one.
- **Version skew.** The wire formats have version numbers in their ALPNs (`imesh/api/0`, `imesh/corrosion/1`). A machine with an older link ALPN cannot link to a newer one. Upgrade all machines together when the ALPN version changes. The `imesh/sync/1` machines of the former record engine cannot link to `imesh/corrosion/1` machines at all: build a new cluster.
- **App schema changes.** A new app version may add tables, columns, and indexes to its `Builder::schema`. The schema gate brings the older machines the new tables before any rows, so machines on the old and new binary work side by side. Two versions that add different columns to one table conflict, see [The schema gate](#the-schema-gate).
- **Reset.** `machine.reset` makes a new key and so a new database directory, `corrosion-<new id>`. The daemon removes the old directory at once when it can, and else at the next start: Windows keeps open files.

## Failure behaviour

| Event | What happens | Proof |
|---|---|---|
| A machine stops | Its peers mark it `down` within about 5 s. The others keep reading and writing. | Stage 9 |
| It comes back | Its links reconnect. Gossip and the next anti-entropy sync send it what it missed. | Stage 9 |
| The network splits | Each side keeps working. After the heal, the merge rule decides each column, and all rows from both sides stay. | Stage 9b |
| A machine misses a schema change | When it comes back, its links send it the new tables before any row that uses them. | Stage 21 |
| Two machines add different columns to one table | Rows stop on that link, and `imesh schema ls` shows the conflict. A schema with both columns ends it. | Stage 21b |
| A daemon crashes right after a write | The write is committed with `synchronous = FULL` before the call answers. | Stage 20 |
| A daemon restarts | Same key and database from disk. Links reconnect. | Stage 10 |
| The entry machine is down or hung | The CLI tries the next connection in the context after at most 20 s. | Stage 12 |
| n0 is down | Machines that know each other's addresses still connect. Self-hosted relays avoid n0. | Stage 15 |
| A firewall blocks UDP | The machine connects through a relay over TCP. Corrosion's UDP stays on loopback. | Stage 18, Windows W5 |

No quorum exists, so no failure blocks writes. The cost is the consistency model above.

## Security model

- **Authentication.** iroh does it. Every connection proves its key in the TLS handshake. imesh adds no tokens or signatures.
- **Authorization.** The member and client lists. The iroh connection hook and the embedded relay's access control reject others before any handler runs. Open connections of a removed member or revoked CLI key close at once.
- **Trust between members.** Equal. Any member can write any row, and add tables.
- **Local processes.** Corrosion always serves an HTTP API, on a random TCP port at the machine's loopback address. imesh sets a random bearer token that it keeps only in memory, so no process can use that API. The gossip port on loopback is different: a local process can reach the Corrosion agent there. imesh forwards only datagrams that come from its own Corrosion, and a member may send only to the gossip port or to a port that this machine's Corrosion sent to it from, with at most 32 sockets per member. It cannot stop a local process from talking to the agent directly. So a member machine must not run untrusted local users. Corrosion supports mutual TLS for gossip; using it needs a certificate authority that all members share, which is an open question.
- **SQL from callers.** `sql.query` and the library's `Store::query` run under SQLite's authorizer, which SQLite asks while it compiles each statement: they may read any table, and nothing else (no transactions, ATTACH, DDL, or setting PRAGMAs). A query stops after 30 s. `sql.execute` compiles each statement under the authorizer too: it may change rows of app tables only, never `imesh_*`, tables, indexes, or Corrosion's and cr-sqlite's own tables, also not through a virtual table or a bind parameter. The check is off while the statement runs, so cr-sqlite can keep its own books. `Store::write`, for code in the daemon's process, fails and rolls back if the transaction changed a table or an index.
- **The local socket or pipe.** A user daemon's socket is mode 0600 in a 0700 directory: only that user. A system daemon's socket is mode 0660 with the group `imesh`, which exists only for imesh. On Windows, see [Windows](#windows). Access to it is full control of the cluster, as with the Docker socket.
- **CLI keys.** A CLI key is full admin over iroh, like the socket. `imesh client rm` revokes one.
- **SSH.** The CLI uses `BatchMode=yes` and `StrictHostKeyChecking=accept-new`. The first connection trusts the host key.
- **The install.** The CLI uploads the binary to a private temporary file and the binary checks its own SHA-256 before it installs itself. Settings reach it on stdin, and the system install script base64-encoded, so no value can run as shell code. The CLI asks the daemon there with `daemon status` first, and refuses a machine that is in a cluster before it changes anything.
- **Schemas from members.** A member's table list arrives on the link. The receiver checks it as it checks `schema.apply`: `CREATE TABLE` and `CREATE INDEX` only, plain names, no `imesh_*` or reserved names, and at most 1000 tables.
- **Removal window.** A removed machine can reach machines that have not yet received the removal: usually under 1 s by gossip, else the next anti-entropy sync, which runs at least every 5 s. A relay connection it already holds stays open until it reconnects, but members close its QUIC connections and reject new ones.

### What a machine exposes

- **One UDP port** for iroh's QUIC, on all interfaces. A connection with a key that is not a member or an authorized CLI key is refused right after the TLS handshake, before any API or data. The open surface is the QUIC and TLS code before that check.
- **Nothing else on the network.** Corrosion's gossip and HTTP API listen on the machine's own 127.x address; the HTTP API also needs a random token that only the daemon's memory holds. The local API is a socket or pipe. A relay port opens only with `--serve-relay`.
- **`--relay-only`** removes the UDP port too (iroh's `clear_ip_transports`): the machine only makes outgoing HTTPS connections to its relay, and every path is `relay`. It costs speed and relay bandwidth, so pair it with a relay of your own.
- **On a shared host**, other local processes can reach Corrosion's loopback gossip port. `--docker` with `--docker-network bridge` gives the daemon a network namespace of its own, so they cannot.

Uncloud exposes more: WireGuard on UDP 51820 on every machine, with no relay, so one side of each pair must be reachable, and its machine API inside the mesh is gRPC with no authentication.

### No root, no sudo

imesh serves ALPNs and a socket. Unlike Uncloud, it does not manage Docker, firewall rules, or network interfaces, so it needs no root, at run time or to install. It installs for a user as tmux runs for one:

- **For the user**, the default for any user but root. The binary goes to `~/.local/bin/imesh`, the data to `~/.local/share/imesh`, the settings to `~/.config/imesh/daemon.env`. The socket is `~/.local/share/imesh/imesh.sock`, mode 0600. It is not in `$XDG_RUNTIME_DIR`: logind deletes that directory when the user's last session ends, and it does not exist at boot.
- **Kept running.** A systemd user service when the user may turn on linger (`loginctl enable-linger`, which logind allows a user for themselves by default): it starts at boot, with no login. Else a detached supervisor (`imesh daemon supervise`, in its own session) that starts the daemon again when it fails, with a `@reboot` line in the user's crontab. `--service systemd` or `--service detached` selects one.
- **For the system**, with `--system` or a root login: a systemd unit, `/usr/local/bin/imesh`, `/etc/imesh/env`, `/var/lib/imesh`, and `/run/imesh/imesh.sock`. The daemon runs as the SSH user, as the system user `imesh` for root, or as the `--run-as` user, never as root. The socket group is `imesh`. A user other than root needs passwordless sudo for this.
- **In a container** (`--docker`), for hosts where installing is hard, such as a NAS: the CLI builds an image on the host from the binary on `alpine:3` (no registry), and runs it with Docker's restart policy, a read-only root file system, a non-root user, no capabilities, and host networking. The key and database are in a named volume. The CLI reaches it with `docker exec` over SSH.
- A client on the machine uses the user's socket when it exists, else the system socket. Over SSH, `dial-stdio` runs `~/.local/bin/imesh`, then `/usr/local/bin/imesh`, then `imesh` from `PATH`.

## Footprint

Measured by the e2e runs on linux/aarch64 in Docker:

- Linux binary: 44.4 MB, static musl, release build, with Corrosion, cr-sqlite, the relay server, and mDNS.
- Linux daemon RSS: about 67 to 82 MB, with 22 to 31 threads.
- Windows exe: 59.7 MB, with cr-sqlite linked in and no DLL. The later work-machine build runs Corrosion on Windows and syncs with Linux through real dev tunnels; see [verification.md](verification.md#windows-and-linux-work-machines-2026-09-25-utc).

The former record engine ([`prototypes/imesh-records`](../prototypes/imesh-records)) was 19.2 MB with 21 to 33 MB RSS. Corrosion, cr-sqlite, and SQLite make the difference. [comparison.md](comparison.md) has the numbers side by side.

## Compared with Uncloud and KubeSpan

| | Uncloud | KubeSpan | imesh |
|---|---|---|---|
| Transport | WireGuard, kernel | WireGuard, kernel | iroh QUIC, user space |
| Both sides behind NAT | Fails | Fails | Works through a relay |
| UDP blocked | Fails | Fails | Works through a relay over TCP |
| Peer discovery | Corrosion `machines` table | Discovery service (shared secret) | `imesh_machines` table, plus iroh lookup |
| Membership proof | SSH access at join time only | Cluster secret | The key, checked by iroh on every connection |
| Revoke one machine | Delete the row | Rotate the secret on every node | `removed = 1`; open connections close |
| State | Corrosion (cr-sqlite) in a container | None | Corrosion (cr-sqlite) embedded in the daemon, gossip over iroh |
| Overlay IPs | Yes | Yes | No |
| Root at run time | Yes | Yes | No |
| Windows | No | No | Yes |
| Control plane or quorum | None | Kubernetes | None |

## Open questions

- **Release distribution.** How `machine init` gets the binary without a local build: signed GitHub releases, or a CLI that carries the Linux builds.
- **Destructive schema changes.** A table or column cannot be dropped or changed. An app that needs it must add a new table and move its rows.
- **A daemon on macOS.** Corrosion's gossip needs one loopback address per machine, and macOS answers only 127.0.0.1. A tunnel with one port per machine would remove this need.
- **Binary size and RSS.** 43.5 MB and about 73 MB, 2.3x the record engine. Leaner Corrosion features, a smaller SQLite cache, or a `current_thread` runtime may help. Nobody has measured that yet.
- **Tombstone collection.** Deleted rows leave cr-sqlite clock entries. Nothing collects them.
- **The merge rule for settings.** cr-sqlite does not follow time. A setting that must follow "the last write wins" needs a version in the row.
- **The Windows service is not tested yet.** It builds and passes clippy, but no Windows machine has run it.
- **A user daemon with no linger and no cron.** It runs until the next reboot. `imesh daemon start` starts it again.
- **Let's Encrypt for the embedded relay.**
- **Streaming API methods.** A method returns one value. Streaming replies, for example to follow logs, need a new reply frame and a method signature that can send it.
- **Other service managers.** macOS launchd and OpenRC are not built.
