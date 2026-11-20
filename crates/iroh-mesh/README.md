# iroh-mesh

A library to run a cluster of machines over [iroh](https://docs.iroh.computer). There is no control plane and no quorum. Machines connect directly when they can, and through a relay when they cannot, also when UDP is blocked. Every machine keeps a copy of the roster: a log of changes to the cluster, each signed by the machine that made it, merged by union. A root key (the recovery code, kept on no machine) names the owner; the owner promotes admins; the owner and admins add and remove machines. Apps add their own API methods and iroh protocols.

The crate is a library, like iroh. It has no command line, and it reads no environment variables, config files, or default paths. You give it plain structs, and you choose the command line, the config format, and where files go. The `imesh` command ([`crates/iroh-mesh-cli`](../iroh-mesh-cli)) is one program built on it.

## Use it

```rust
use iroh_mesh::daemon::{Builder, Config};

let mut config = Config::new("/var/lib/my-app");
config.net.n0 = true; // or relays, pkarr, mDNS, a served relay: see net::Config
let a = Builder::new(config)
    .method("app.hello", |m, caller, _params| async move {
        Ok(serde_json::json!(format!("{} greets {caller:?}", m.name())))
    })
    .spawn()
    .await?;

let init = a.machine.init_cluster(None).await?; // show init.recovery_code once
let ticket = a.machine.admit(Caller::Local, other_id, "b", None).await?; // give the ticket to the other machine
// on the other machine: b.machine.join(&ticket).await?
```

[`examples/two_machines.rs`](examples/two_machines.rs) runs this with two machines in one process:

```bash
cargo run -p iroh-mesh --example two_machines
```

## What it gives you

| Module | What it does |
| --- | --- |
| `daemon` | `Builder` (methods, protocols) and `spawn`, which returns `Running`. `Machine` has the cluster operations: `init_cluster`, `admit`, `join`, `remove_member`, `rename_member`, `promote`, `recover`, `leave`, `set_cluster_relays`, `machines`. `serve` runs a machine with its local API until your shutdown future resolves, and restarts it after a reset. |
| `roster` | The signed log of changes: who may make which change, the merge, and the bounds that keep a stolen machine from growing the log. |
| `state` | This machine's copy of the roster, saved to `roster.json`, and the `Snapshot` view of the cluster it gives. |
| `client` | `Client` calls a machine's API over its local socket (`Client::local`), over iroh (`Client::iroh`), or over any stream you open (`Client::new` with your `Dial`). |
| `api` | The API wire format, the reply types, and `JoinTicket`. |
| `net` | `net::Config`, relays, a members-only relay a machine can serve, its exposure through a dev tunnel, and the local API socket or pipe. |
| `os` | Helpers for daemons: `shutdown_signal`, `sd_notify_ready`, private files and keys. |

OS-specific code is in `src/platform/` only.

## Build

```bash
cargo test -p iroh-mesh
```

A machine runs on Linux, macOS, and Windows.
