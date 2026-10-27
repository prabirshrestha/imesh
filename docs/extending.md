# Build an app on imesh

This guide shows how an app runs imesh machines and adds its own tables, API methods, and iroh ALPN. The `imesh` crate is a library, like iroh: it has no command line and reads no config files or environment variables, so your app chooses those. Two complete examples:

- [`crates/imesh/examples/two_machines.rs`](../crates/imesh/examples/two_machines.rs): the library only. Two machines in one process form a cluster and replicate a row.
- [`crates/imesh-cli/examples/app.rs`](../crates/imesh-cli/examples/app.rs): an app that keeps the `imesh` command line and adds its own tables, methods, and ALPN. The e2e suite runs every check against it.

## Depend on the crate

```toml
[dependencies]
imesh = { path = "../imesh" }   # or a git or crates.io dependency
```

## Start a machine

A machine needs a `Config` with its data directory. Everything else has a default: the n0 relays and n0 address lookup, and the hostname as its name. Set `config.net` for your own relays, `no_n0`, pkarr, mDNS, or a relay the machine serves.

```rust
use imesh::daemon::{Builder, Config};

let mut config = Config::new("/var/lib/my-app");
config.labels.insert("role".into(), "worker".into());
let running = Builder::new(config).schema(SCHEMA).method("my.method", my_method).spawn().await?;
let machine = running.machine.clone();
```

`spawn` starts the endpoint and its protocols, the links to the other members, the database, and the relay when `serve_relay` is set. It fails if another machine runs with the same data directory. It does not open a local API.

- `running.machine` is the `Machine`.
- `running.reset_requested()` resolves when `machine.reset` was called. Decide what to do, for example `shutdown`, then `Running::delete_data(data_dir)`.
- `running.shutdown()` stops the relay, closes the connections, and stops the database agent. Committed writes are already in the database. Dropping `Running` stops its tasks.

To run a machine as a daemon with a local API, call `serve`. It runs until your shutdown future resolves. After `machine.reset` it deletes `<data dir>/app` and the key, and calls your closure again to start as a new machine in the same process. The database directory carries the key in its name, so the new machine starts with an empty database. Keep your app's own files under `<data dir>/app`, so a reset removes them too.

```rust
use imesh::daemon::{Access, Builder, LocalApi, serve};

let api = LocalApi { path: "/run/user/1000/my-app.sock".into(), access: Access::Owner };
serve(|| my_app::register(Builder::new(config.clone())), &api, imesh::os::shutdown_signal()).await?;
```

## Form a cluster

The cluster operations are `Machine` methods. The API methods (`cluster.init`, `cluster.add`, ...) call the same functions.

```rust
let cluster = a.init_cluster(None).await?;           // a new cluster with a as its only member
let ticket = a.admit(b_id, "b").await?;              // a JoinTicket; `ticket.to_string()` is text
let joined = b.join(&ticket).await?;                 // on machine b
a.authorize_client(cli_key, "ops").await?;           // a key that may call the API over iroh
a.remove_member(b_id).await?;                        // b must reset to come back
```

Get `b_id` from `Machine::id` on b, or from its `EndpointTicket`. A `JoinTicket` holds no secret unless the cluster relay setting has a token.

## Keep the imesh command line

If the `imesh` command line suits your app, depend on `imesh-cli` too and call `imesh_cli::run` with your registration:

```rust
fn main() -> std::process::ExitCode {
    imesh_cli::run(my_app::register)
}
```

`register` gets a `Builder` and adds the app's tables, methods, and protocols. `run` calls it for every daemon it starts, also after a reset. All commands work as they do for `imesh`: `my-app machine init deploy@host` uploads the Linux build of `my-app` (from `--binary` or `IMESH_BINARY_<ARCH>`) and installs it on the server as `imesh`, and `my-app call <method>` calls your methods.

## Register tables, methods, and a protocol

Add them at build time. iroh fixes the ALPN list when the router starts.

```rust
pub const ALPN: &[u8] = b"imesh-example/echo/0";

/// The app's tables. A later version may add tables, columns, and indexes here.
const SCHEMA: &str = "
CREATE TABLE example_notes (
    who TEXT NOT NULL PRIMARY KEY,
    text TEXT NOT NULL DEFAULT ''
);
CREATE TABLE example_items (
    key TEXT NOT NULL PRIMARY KEY,
    value TEXT NOT NULL DEFAULT ''
);
";

pub fn register(b: Builder) -> Builder {
    b.schema(SCHEMA)
        .protocol(ALPN, Echo)
        .method("example.dial", dial)
        .method("example.note", note)
}
```

`schema` takes complete `CREATE TABLE` and `CREATE INDEX` statements. Every machine that runs your binary applies them at start, and the daemon does not start if they fail. Name tables after your app. Tables named `imesh_*` belong to imesh. A later version of your app may add tables, columns that are nullable or have a default, and indexes. It gives the complete new `CREATE TABLE` statement. It cannot drop or change a table or column: Corrosion refuses it. [design.md](design.md#schema-changes) explains how machines on the old and new version agree on the tables.

Name ALPNs `<app>/<protocol>/<version>`. Bump the version when the wire format changes. Method names cannot replace built-in methods; `Builder::method` panics on a duplicate.

## Write the protocol handler

A protocol is an iroh `ProtocolHandler`. The admission hook has already rejected non-members, and it closes the connection if the peer is removed later. Do not check membership again.

```rust
impl ProtocolHandler for Echo {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let (mut send, mut recv) = conn.accept_bi().await?;
        tokio::io::copy(&mut recv, &mut send).await?;
        send.finish()?;
        conn.closed().await;
        Ok(())
    }
}
```

If the handler needs the machine when you build it, use `protocol_with`. It gets the `Machine` after the endpoint binds and before the router starts, so the handler can keep the endpoint, the state, or the machine itself (to dial members with `connect`). iroh-blobs and iroh-docs need it:

```rust
b.protocol_with(iroh_blobs::ALPN, |m| make_blobs_protocol(m.endpoint()))
```

## Write a method

A method gets the `Machine`, the `Caller`, and JSON params, and returns JSON. The caller is `Local` (the socket, which includes SSH), `Client(key)` (a CLI key over iroh, directly or forwarded by another machine), or `Member(id)` (another machine). Use it to decide who may call:

```rust
use imesh::state::rusqlite::params;

async fn note(m: Machine, caller: Caller, p: Value) -> Result<Value> {
    let p: NoteParams = imesh::api::params(p)?;
    let who = match caller {
        Caller::Local => "local".to_string(),
        Caller::Member(id) => format!("member-{}", id.fmt_short()),
        Caller::Client(_) => anyhow::bail!("CLI keys may not write notes"),
    };
    m.state()
        .write(|c| {
            c.execute(
                "INSERT INTO example_notes (who, text) VALUES (?1, ?2) ON CONFLICT (who) DO UPDATE SET text = excluded.text",
                params![who, p.text],
            )
        })
        .await?;
    Ok(json!({ "who": who }))
}
```

The daemon answers only after the method's transaction commits.

To reach another member on your ALPN, use `Machine::connect`. It checks membership and uses the address from the cluster state:

```rust
async fn dial(m: Machine, _caller: Caller, p: Value) -> Result<Value> {
    let p: DialParams = imesh::api::params(p)?;
    let conn = m.connect(p.to, ALPN).await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(p.text.as_bytes()).await?;
    send.finish()?;
    let back = recv.read_to_end(64 << 10).await?;
    Ok(json!({ "echo": String::from_utf8_lossy(&back), "from": p.to }))
}
```

## Call it

From the CLI, through any machine:

```bash
imesh call example.dial '{"to":"<endpoint id of node2>","text":"hi n2"}'
imesh call example.note '{"text":"hello"}' -m node2
```

`imesh machine ls -w` shows full endpoint IDs.

## Keep app state

`Machine` also gives `id()`, `name()`, `members()`, `info()` (what this machine publishes about itself), `peer_status(id)` (up or down, path, RTT), `machines()`, `endpoint()`, `config()`, and `connect(id, alpn)`.

`Machine::state()` is the replicated database, a `Store`. It is SQLite with cr-sqlite CRDT tables, run by an embedded Corrosion agent. `imesh::state::rusqlite` re-exports the `rusqlite` version it uses, so your app needs no own dependency on it.

- `write(|conn| ...)` runs your statements in one transaction and sends the change to every member. It returns after the commit on this machine.
- `query(|conn| ...)` runs a read on a pooled read connection.
- `changes()` is a `watch::Receiver<i64>` of the database version. It changes on every write to any table, local or from a member. Watch it and query the tables you care about.
- `tables()` gives this machine's tables and columns, and `watch_tables()` fires when they change.
- `apply_schema(sql)` adds or grows tables at run time, as `imesh schema apply` does.
- `snapshot()`, `read(f)`, and `subscribe()` give imesh's own cluster tables (members, machines, clients, settings), kept in memory.

```rust
let n: i64 = m
    .state()
    .query(|c| c.query_row("SELECT count(*) FROM example_items WHERE key LIKE ?1", params![prefix], |r| r.get(0)))
    .await?;
```

From the CLI, `imesh sql "SELECT * FROM example_notes"` reads your tables, and `imesh sql --exec "..."` changes rows. `kv.put` and `kv.get` use imesh's own `imesh_kv` table, not yours.

Every machine keeps the whole database. Merges run per column: the side that changed a column more times wins, then the larger value. Time plays no part, so "the later write wins" does not hold. Keep a version or timestamp in the row if you need it. Things that must be unique across the cluster, such as ports or address ranges, cannot be allocated safely from shared state while the network is split. Give each machine its own rows (with the machine ID in the key), or derive the value from the machine key.

## Call the API from Rust

`imesh::client::Client` calls a machine's API. `Client::local(path)` uses its local socket or pipe; `Client::iroh(endpoint, addr)` connects over iroh with a key that the cluster authorized; `Client::new(dial)` uses any stream you open, for example SSH to a helper that bridges to the local API, as `imesh-cli` does. `call` takes any params that serialize and decodes the reply into the type you ask for. `imesh::api` has the reply types of the built-in methods, for example `Inspect` and `MachineRow`:

```rust
let c = Client::local("/run/user/1000/my-app.sock");
let rows: Vec<imesh::api::MachineRow> = c.call(None, "cluster.machines", ()).await?;
c.close().await;
```

For large data, run iroh-blobs or iroh-docs on your own ALPN with `protocol_with`. [comparison.md](comparison.md) lists what to watch for with iroh-docs.
