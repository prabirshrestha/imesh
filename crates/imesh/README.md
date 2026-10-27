# imesh

A library to run a cluster of equal machines over [iroh](https://docs.iroh.computer). There is no control plane and no quorum. Machines connect directly when they can, and through a relay when they cannot, also when UDP is blocked. Every machine keeps a copy of one replicated SQLite database, through embedded [Corrosion](https://github.com/superfly/corrosion) (cr-sqlite tables, SWIM, gossip). Apps add their own tables, API methods, and iroh protocols.

The crate is a library, like iroh. It has no command line, and it reads no environment variables, config files, or default paths. You give it plain structs, and you choose the command line, the config format, and where files go. The `imesh` command ([`crates/imesh-cli`](../imesh-cli)) is one program built on it.

The design, with its reasons, is in [docs/design.md](../../docs/design.md). [docs/extending.md](../../docs/extending.md) is the guide for apps.

## Use it

```rust
use imesh::daemon::{Builder, Config};

let mut config = Config::new("/var/lib/my-app");
config.net.no_n0 = true; // and relays, pkarr, mDNS, a served relay: see net::Config
let a = Builder::new(config)
    .schema("CREATE TABLE notes (id TEXT NOT NULL PRIMARY KEY, body TEXT NOT NULL DEFAULT '');")
    .method("notes.count", |m, _caller, _params| async move {
        let n: i64 = m.state().query(|c| c.query_row("SELECT count(*) FROM notes", [], |r| r.get(0))).await?;
        Ok(serde_json::json!(n))
    })
    .spawn()
    .await?;

let cluster = a.machine.init_cluster(None).await?;
let ticket = a.machine.admit(other_id, "b").await?; // give the ticket to the other machine
// on the other machine: b.machine.join(&ticket).await?
a.machine.state().write(|c| c.execute("INSERT INTO notes (id, body) VALUES ('x', 'hi')", [])).await?;
```

[`examples/two_machines.rs`](examples/two_machines.rs) runs this with two machines in one process:

```bash
cargo run -p imesh --example two_machines
```

## What it gives you

| Module | What it does |
| --- | --- |
| `daemon` | `Builder` (tables, methods, protocols) and `spawn`, which returns `Running`. `Machine` has the cluster operations: `init_cluster`, `admit`, `join`, `remove_member`, `authorize_client`, `set_cluster_relays`, `apply_schema`. `serve` runs a machine with its local API until your shutdown future resolves, and restarts it after a reset. |
| `state` | The replicated database: `Store::query` (read-only, guarded), `Store::write`, schema changes, and the schema gate. |
| `client` | `Client` calls a machine's API over its local socket (`Client::local`), over iroh (`Client::iroh`), or over any stream you open (`Client::new` with your `Dial`). |
| `api` | The API wire format, the reply types, and `JoinTicket`. |
| `net` | `net::Config`, relays, a members-only relay a machine can serve, its exposure through a dev tunnel, and the local API socket or pipe. |
| `os` | Helpers for daemons: `shutdown_signal`, `sd_notify_ready`, private files and keys. |

OS-specific code is in `src/platform/` only.

## Build

Corrosion is a git submodule with a small patch. From the repository root, once:

```bash
cargo x setup
cargo test -p imesh
```

A static musl build and a Windows build link cr-sqlite in, so nothing loads a library at run time. `build.rs` runs `crsqlite.sh` the first time, which builds cr-sqlite from source. It needs git, rustup (it installs the `nightly-2023-10-05` toolchain), a libclang older than 21, and zig and llvm-objcopy (musl) or mingw-w64 (Windows). On macOS and glibc Linux, Corrosion loads its own bundled cr-sqlite library.

A machine runs on Linux and Windows. macOS can build the crate and use `Client`, but cannot run a machine: Corrosion needs one loopback address per machine in 127.0.0.0/8, and macOS answers only 127.0.0.1.
