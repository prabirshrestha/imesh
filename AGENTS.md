# imesh

A cluster of machines over iroh, with a signed roster that every member keeps and no control
plane. Read the code and its module docs before a change that touches the roster, roster sync,
admission, or installs.

## Layout

- `crates/iroh-mesh`: the library. It runs a machine and talks to one.
- `crates/iroh-mesh-cli`: the `imesh` command, one program built on the library. It holds the
  command line, contexts, installs, SSH, and default paths.
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
- **Authority comes from the roster.** A change counts only when its signer may make it, judged
  from the entries above it (`roster.rs`). Every copy must derive the same members from the same
  entries in any order. A removed machine's decisions count only when its removal lists them; no
  clock decides it. A remote caller acts through another machine only where its own role lets it
  act (`methods.rs`).
- **Quiet when idle.** No standing links and no keepalives. Machines say hello at start, on a
  roster change, and every 10 minutes, and retry a member that did not answer with backoff up to
  10 minutes. Do not add a periodic message without a reason that battery and bandwidth can pay
  for.
- **Wire formats are versioned by ALPN.** `imesh/api/1`, `imesh/roster/0`, and `imesh/pair/1`. A
  change that an older machine would read wrongly needs a new ALPN version.
- **Security.** Authentication is iroh's key check only. The local socket is 0600 for a user
  daemon and 0660 with the group `imesh` for a system daemon. Installs never need sudo unless
  `--system`. Settings reach shell scripts base64-encoded or on stdin, never as shell source.
- **No backward compatibility shims.** Change the code, README.md, and the skill together.
- **Writing.** ASD-STE100 Simplified Technical English, ASCII, no em dashes.

## Checks

```bash
cargo x test       # unit tests
cargo x lint       # rustfmt and clippy
cargo x windows    # the Windows build
cargo x e2e        # the Docker cluster; about 6 minutes
```

A change to the roster, roster sync, admission, or installs needs a full `cargo x e2e` run.
