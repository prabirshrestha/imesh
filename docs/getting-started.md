# Build a three-machine cluster

In this tutorial, we turn three Linux servers into one imesh cluster, list the machines, and reach the cluster through a second context. We use the POC build. It takes about ten minutes.

You need:

- Three Linux servers on amd64 or arm64. They can be on different networks. You need SSH access as any user. No root and no sudo: imesh installs for that user, as tmux does, and the daemon runs as that user. If you log in as root, imesh installs a system service that runs as the system user `imesh`, never as root.
- A machine with git, Rust, and `cargo-zigbuild` for the build. The CLI runs on Linux, Windows, and macOS. The daemon runs on Linux and Windows only.
- For the static Linux build, the tools that build cr-sqlite from source: `rustup` (the build installs the `nightly-2023-10-05` toolchain), zig, `llvm-objcopy`, and a libclang older than 21, for example LLVM 20. For a Windows build, mingw-w64 instead of zig.

No test servers? `tests/e2e/linux.sh` runs the same steps against Docker containers.

## Build the binaries

First, fetch Corrosion. It is a git submodule in `vendor/corrosion`, and `vendor/corrosion.patch` holds imesh's changes to it. Run this once after you clone, from the repository root:

```bash
cargo x setup
```

Then build the CLI for your machine and the daemon for the servers:

```bash
cargo build --release -p imesh-cli
cargo zigbuild --release -p imesh-cli --target x86_64-unknown-linux-musl
```

The musl build runs `crsqlite.sh` once per target and again when that script changes. It clones cr-sqlite at commit `3133cac`, builds it with the nightly toolchain, and links it into the binary, so the daemon needs no `crsqlite.so`. The archive stays in `crates/imesh/target/crsqlite/<target>/`. If the script does not find libclang, set `LIBCLANG_PATH`, for example to `/opt/homebrew/opt/llvm@20/lib`. The CLI build for macOS or glibc Linux uses the cr-sqlite library that Corrosion bundles.

The Windows target build is cross-compiled, not a verified Windows-host build.
The current cr-sqlite script needs Bash and MinGW and assumes a Homebrew MinGW
include path. A native Windows-only build needs changes to tool discovery and
build environment handling. The full end-to-end suite also needs Linux containers.

For arm64 servers, build `--target aarch64-unknown-linux-musl` too. From the repository root: Then tell the CLI where the Linux binaries are, one variable per architecture:

```bash
export IMESH_BINARY_X86_64=$PWD/target/x86_64-unknown-linux-musl/release/imesh
export IMESH_BINARY_AARCH64=$PWD/target/aarch64-unknown-linux-musl/release/imesh
alias imesh=$PWD/target/release/imesh
```

## Start the cluster

Now, initialise the first server:

```bash
imesh machine init deploy@203.0.113.10
```

You see:

```
uploading .../imesh (43 MB) to deploy@203.0.113.10
installing imesh on deploy@203.0.113.10 for the SSH user
imesh daemon running for deploy as a systemd user service; it starts at boot and keeps running after logout
socket /home/deploy/.local/share/imesh/imesh.sock
cluster bdc635b4... initialised on deploy@203.0.113.10 (56ee5ed9...)
current context is now "default"
```

The server now runs the daemon as `deploy`. The binary is in `~/.local/bin/imesh`, the key and the database are in `~/.local/share/imesh`, and the settings are in `~/.config/imesh/daemon.env`. Only `deploy` can open the socket. Your `~/.config/imesh/config.toml` has a context named `default`, which is now current.

The daemon runs as a systemd user service when the user may keep one running after logout (`loginctl enable-linger`, which most systems allow a user for themselves). Otherwise it runs as a background process that restarts itself, and a `@reboot` line in the user's crontab starts it at boot. `--service systemd` or `--service detached` selects one. `--system` installs a system service instead; for a user other than root, that needs passwordless sudo.

If your SSH key is not in the agent, add `-i ~/.ssh/id_xxx`.

## Add two machines

Add the other two servers. The second one logs in as root, so it gets a system service that runs as the system user `imesh`:

```bash
imesh machine add deploy@198.51.100.20
imesh machine add root@192.0.2.30
```

Each prints `machine <name> (<id>) joined cluster ... with N members`.

## List the machines

```bash
imesh machine ls
```

```
NAME         ID         STATE    PATH    RTT      OS/ARCH        VERSION
node1        56ee5ed9db up       self    -        linux/x86_64   0.1.0
node2        307e0de155 up       direct  12ms     linux/x86_64   0.1.0
node3        ccb4b45101 up       relay   48ms     linux/aarch64  0.1.0
```

`PATH` is how the machine you are connected to reaches each peer. `relay` means no direct path exists yet. iroh keeps trying to find a direct one.

## Talk to one machine through another

Write a value on one machine and read it on another:

```bash
imesh call kv.put '{"key":"greeting","value":"hello"}'
imesh call kv.get '{"key":"greeting"}' -m node3
```

The second command goes to your entry machine, which sends it on to `node3`. You see `"hello"`.

## Add a table

Every machine has a copy of one SQLite database. Add a table, write a row on one machine, and read it on another:

```bash
echo "CREATE TABLE notes (id TEXT NOT NULL PRIMARY KEY, body TEXT NOT NULL DEFAULT '');" > notes.sql
imesh schema apply notes.sql
imesh sql --exec "INSERT INTO notes (id, body) VALUES ('a', 'hello')"
imesh call sql.query '{"sql":"SELECT id, body FROM notes"}' -m node3
```

`imesh sql` runs on the entry machine. The last command reads the copy on `node3`.

`imesh schema ls` shows the tables, and for each member whether rows pass. A table can only grow: later you can add columns that are nullable or have a default, and indexes.

## Connect over iroh instead of SSH

List the connections of the current context:

```bash
imesh ctx conn
```

```
0  ssh://deploy@203.0.113.10
1  iroh://56ee5ed9...
2  ssh://deploy@198.51.100.20
...
```

Move the iroh connection to the top, and run `ls` again:

```bash
imesh ctx conn 1
imesh machine ls
```

The CLI now connects with its own key over iroh. SSH is not used. If that fails, the CLI tries the next connection in the list.

## Remove a machine

```bash
imesh machine rm node3
```

`node3` resets. It gets a new key and leaves the cluster. The others refuse its old key from now on and close its open connections. This works over iroh too: the entry machine sends the reset on to `node3`.

## Add a NAS with Docker

Some hosts, such as a Synology NAS, are hard to install on: no systemd user services, no crontab, and sudo with a password. If the host runs Docker, add it with `--docker`. imesh then runs in a sandboxed container that Docker starts at boot:

```bash
imesh machine add admin@nas.local --docker --name nas
```

The CLI builds a small image on the NAS from the binary (on `alpine:3`), and starts the container with a read-only root file system, as a non-root user, with no capabilities, and with host networking, so it uses the NAS's own addresses and reaches machines on the same LAN directly, not through a relay. The key and the database are in the Docker volume `imesh-data`. `imesh machine update admin@nas.local --docker` puts in a new binary and keeps the machine. To give an app NAS folders, add `--docker-volume /volume1/share:/share:ro`. To open no port on the NAS at all, add `--relay-only`: the machine then only connects out to a relay, and all its traffic goes through that relay.

The SSH user must be able to run `docker`, or `sudo docker` with no password. If neither is possible, start the container once from the NAS's container manager and join it with tickets, as for the Windows laptop below.

## Add a Windows laptop

On the laptop, with `imesh.exe` from `cargo build --release -p imesh-cli --target x86_64-pc-windows-gnu`. That build needs mingw-w64 and links cr-sqlite into the exe, so no DLL is needed:

```powershell
.\imesh.exe daemon install
imesh machine id
```

`daemon install` copies the binary to `%LOCALAPPDATA%\imesh\bin` and registers a scheduled task that starts at logon. To run the daemon from boot, also at the login screen, run `imesh.exe daemon install --system` from an elevated shell instead: it installs a Windows service. Put that folder on your `PATH`, or call the exe there. On your own computer, admit the ticket that `machine id` printed:

```bash
imesh machine add endpoint... --name laptop
```

It prints `imesh machine join imeshjoin...`. Run that on the laptop. `imesh machine ls` now shows `laptop`.

You now have a cluster with no control plane. Any machine can go down, and the others keep working. [design.md](design.md) explains how.
