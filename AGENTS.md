# imesh

A cluster of equal machines over iroh, with one replicated SQLite database (embedded Corrosion)
and no control plane. Read the code and its module docs before a change that touches links, the
schema gate, admission, or installs.

## Layout

- `crates/iroh-mesh`: the library. It runs a machine and talks to one.
- `crates/iroh-mesh-cli`: the `imesh` command, one program built on the library. It holds the
  command line, contexts, installs, SSH, and default paths.
- `vendor/corrosion` + `vendor/corrosion.patch`: Corrosion as a submodule plus our patch.
- `tests/e2e/`: end-to-end runs in Docker (Linux) and on a Windows VM.

## Rules

- **The library is a library, like iroh.** `crates/iroh-mesh` has no command line, reads no
  environment variables and no config files, and has no default paths. Callers pass plain
  structs (`daemon::Config`, `net::Config`). Everything an app might want to choose is a field
  or an argument, not a flag or a variable.
- **Operations are functions first.** A cluster operation is a `Machine` method
  (`daemon/cluster.rs`); the JSON API method in `methods.rs` only decodes params and calls it.
- **Platform code is isolated.** OS-specific code in the library lives in `src/platform/unix.rs`
  and `src/platform/windows.rs`, which give the same functions. Other modules have no
  `#[cfg(unix)]` or `#[cfg(windows)]`.
- **No silent row loss.** Rows pass on a link only while both machines have the same tables
  (the schema gate). Schemas only grow. Do not add a path that writes rows past the gate.
- **Wire formats are versioned by ALPN.** `imesh/api/0` and `imesh/corrosion/1`. A change that an
  older machine would read wrongly needs a new ALPN version.
- **Security.** Authentication is iroh's key check only. The local socket is 0600 for a user
  daemon and 0660 with the group `imesh` for a system daemon. Installs never need sudo unless
  `--system`. Settings reach shell scripts base64-encoded or on stdin, never as shell source.
- **No backward compatibility shims.** Change the code, README.md, and the skill together.
- **Writing.** ASD-STE100 Simplified Technical English, ASCII, no em dashes.

## Checks

```bash
cargo x setup      # once after clone
cargo x test       # unit tests
cargo x lint       # rustfmt and clippy
cargo x windows    # the Windows build
cargo x e2e        # the Docker cluster; about 6 minutes
```

A change to links, installs, or the schema gate needs a full `cargo x e2e` run.
