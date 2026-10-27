# imesh-cli

The `imesh` command, built on the [`imesh`](../imesh) library. It installs machines over SSH or on this computer, manages clusters through contexts, and runs the daemon.

```bash
imesh machine init deploy@203.0.113.10    # install the daemon for deploy (no sudo), start a cluster
imesh machine add deploy@198.51.100.20    # add a machine on any network
imesh machine ls                          # every machine, and how it is reached
```

[docs/getting-started.md](../../docs/getting-started.md) is the tutorial, and [docs/reference.md](../../docs/reference.md) lists every command, option, variable, and file.

## What this crate adds to the library

| Module | What it does |
| --- | --- |
| `cli`, `machine`, `ctx` | The commands and their handlers. |
| `args` | The daemon's flags and `IMESH_*` variables, mapped to `imesh::daemon::Config`. |
| `install` | Installs: for the user with no root or sudo (a systemd user service, or a detached process that the crontab starts at boot), as a system service, or as a Windows scheduled task; and over SSH (`remote.rs`). |
| `config`, `connect`, `ssh` | Contexts in `~/.config/imesh/config.toml`, and opening a `Client` over SSH, iroh, or the local socket. |
| `paths` | Where the daemon keeps its files, and which local daemon a client uses. |

## Reuse the command line in an app

An app that wants its own command line uses the `imesh` library directly. An app that is happy with this one calls `imesh_cli::run` with its own registration; [`examples/app.rs`](examples/app.rs) does that, and the e2e suite runs every check against it:

```rust
fn main() -> std::process::ExitCode {
    imesh_cli::run(|b| b.schema(MY_TABLES).method("my.method", my_method))
}
```

The installed binary then takes the place of `imesh` on each machine.
