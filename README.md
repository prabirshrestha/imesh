# imesh

imesh connects your computers into one cluster with no central server. Any machine can go down
while the others keep working. Machines connect with [iroh](https://iroh.computer), over
encrypted QUIC connections that iroh authenticates by machine key. A private relay through
[devtunnel](https://learn.microsoft.com/azure/developer/dev-tunnels/get-started) is optional.

Every machine keeps a copy of the roster: a log of changes to the cluster, each one signed by
the machine that made it. A machine sends its roster to the others when it starts and when the
roster changes. When nothing changes, machines send nothing to each other, which saves battery
and bandwidth.

imesh also has built-in SSH. `imesh ssh machine2` opens a shell on another member, with no
`sshd`, no SSH keys, and no passwords.

imesh runs on Linux, macOS, and Windows (x64 and arm64). It needs no root or sudo.

## Install

```bash
npm install -g iroh-mesh
```

The npm package is `iroh-mesh`. The command it installs is `imesh`.

imesh is written in Rust, and the `imesh` program is a single Rust binary. npm is only a way to
distribute it. The package installs the prebuilt binary for your platform. To build from source,
see [Build from source](#build-from-source).

## Start a cluster

On the first machine:

```bash
imesh up
```

`imesh up` installs the daemon on this computer, starts it, and starts a cluster with this
machine as its owner. Run it again at any time: it keeps the machine's settings, and flags you
pass change only those settings.

`imesh up` prints a recovery code once. Keep it in a safe place. No machine keeps it. If the
owner machine is lost, the recovery code makes another machine the owner. See
[Roles](#roles).

To add another machine you can open a terminal on, run `imesh invite` on the owner or an admin.
It prints a pairing code. Then, on the new machine:

```bash
imesh up --join <pairing code>
```

Both machines print the short ID of the new machine. If the IDs are the same, answer `y` on the
inviting machine. A code is valid for 15 minutes and admits one machine.

To add a server that you reach over SSH, from the owner or an admin:

```bash
imesh add deploy@198.51.100.20
imesh status
```

`imesh add` copies imesh to the server, starts the daemon for the SSH user, and adds it to the
cluster. On a computer that is not in a cluster, `imesh add` starts a new cluster on the server.

By default, imesh uses no relay, so the machines must reach each other directly. To connect
machines behind NAT or a firewall, use a relay. See [Connect machines that cannot reach each other](#connect-machines-that-cannot-reach-each-other).

To install a system service on a server, add `--system`. This needs root or passwordless sudo.
To run the daemon in a Docker container, for example on a NAS, add `--docker`.

## Use SSH

A machine that runs `imesh up --ssh` serves SSH to the other members, as the user that runs the
daemon.

```bash
imesh ssh machine2
imesh scp ./notes.txt machine2:notes.txt
```

To use plain `ssh`, `scp`, and editors such as Zed or VS Code:

```bash
imesh configure ssh --write
ssh machine2
```

To record each terminal session, add `--ssh-record` to `imesh up`.

## Connect machines that cannot reach each other

### Use the public n0 relays

```bash
imesh up --relay n0
```

The n0 relays are public relays that the iroh team runs. iroh encrypts all traffic end to end,
so a relay cannot read it.

### Use dev tunnels (optional)

Use this when the machines have no SSH server, no open port, and no public relay to use. Each machine needs the
[devtunnel CLI](https://learn.microsoft.com/azure/developer/dev-tunnels/get-started), logged in
once with `devtunnel user login`.

On machine1:

```bash
imesh up --relay devtunnel --ssh
imesh invite machine2 --devtunnel
```

On machine2, with the code that `invite` printed:

```bash
imesh up --join imeshpair1-dtm-... --ssh
```

The code tells machine2 that the cluster uses the devtunnel relay, so `up --join` adds
`--relay devtunnel` itself. Add `--relay none` to turn it off.

With `--relay devtunnel`, each machine serves a relay through its own private dev tunnel, and
connects to the relay of every other member. Each user's devtunnel login controls access to the
tunnel, and iroh checks the machine keys.

Each machine serves its relay on one local port. By default it takes a free port and keeps it
between runs while it is free, so a port that another program uses is never a problem. Add
`--mesh-port <port>` only to force one. Machines can use the same port: each member forwards the
other relays to free local ports.

### Run your own relay

One machine can serve a relay. Only cluster members can use it.

```bash
imesh add deploy@gateway --new \
	--serve-relay 0.0.0.0:3340 \
	--relay http://gateway.example.com:3340
imesh add deploy@locked-down-host
```

Every member must reach `gateway.example.com:3340` over TCP. `imesh add` copies `--relay` from
the machine that adds the new one.

To make every member use one relay, including members that are already in the cluster:

```bash
imesh relay set https://relay.example.com
```

## Roles

Each machine has one role:

| Role | What it can do |
| --- | --- |
| Owner | Adds machines, removes any machine, makes plain members admins. The machine that started the cluster. |
| Admin | Adds machines and removes plain members. |
| Member | Uses the cluster. It can rename itself and leave. |

New machines are plain members. To make a machine an admin, run this on the owner:

```bash
imesh promote laptop
```

A removed machine stays out. To come back, it resets and joins again with a new key. If a machine
was stolen, remove it with `--cascade`. This also undoes every machine it added, promoted, or
removed:

```bash
imesh rm laptop --cascade
```

If the owner machine is lost, make another machine the owner with the recovery code. The command
reads the code from stdin, so it does not stay in your shell history:

```bash
imesh recover
```

## Common commands

| Command | What it does |
| --- | --- |
| `imesh up` | Installs and starts the daemon on this computer, in a cluster. |
| `imesh invite <name>` | Prints a pairing code for a new machine. |
| `imesh add <user@host>` | Installs imesh on a server over SSH, in the cluster. |
| `imesh status` | Asks every machine to answer, and lists the machines, their roles, and how each one is reached. |
| `imesh ping <machine>` | Checks that a machine answers. |
| `imesh rename <machine> <name>` | Renames a machine. A member renames only itself. |
| `imesh rm <machine>` | Removes a machine from the cluster and resets it. Owner or admin only. |
| `imesh promote <machine>` | Makes a plain member an admin. Owner only. |
| `imesh recover` | Makes this machine the owner with the recovery code from stdin. |
| `imesh update` | Installs a new version on this computer. `imesh update server <user@host>` updates a server. |
| `imesh down` | Stops the daemon. The machine stays in its cluster. |
| `imesh logout` | Leaves the cluster, and deletes the machine's key and data. |

Run `imesh <command> --help` to see all options.

## Use imesh with a coding agent

imesh has a skill file that teaches a coding agent to use imesh. To install it in
`~/.agents/skills`, which many agents read:

```bash
imesh skill install
```

Some agents read only their own directory. To install the skill for one of them, use `--agent`
with `claude`, `codex`, `copilot`, `opencode`, or `pi`. To choose from a list, use `-i`:

```bash
imesh skill install --agent claude
imesh skill install -i
```

Run `imesh skill install` again after you update imesh. `imesh skill uninstall` takes the same
options and removes the skill. `imesh skill` prints the skill file.

## Build from source

```bash
git clone https://github.com/prabirshrestha/imesh
cd imesh
cargo x tools
cargo x install
```

`cargo x install` builds imesh for every platform, makes the npm packages, and installs them with
`npm install -g`, the same as the published package. Then `imesh` is on your PATH.

`cargo x tools` installs the build tools on macOS: Zig, cargo-zigbuild, and MinGW-w64. The
Windows arm64 build also needs LLVM on your PATH (`brew install llvm`, then add `$(brew --prefix llvm)/bin`) and [cargo-xwin](https://github.com/rust-cross/cargo-xwin) (`cargo install --locked cargo-xwin`). It downloads Microsoft's C runtime and Windows SDK libraries on first use, about 1 GB.
Run `imesh --help` for every command.

## License

imesh is licensed under the [Apache License 2.0](LICENSE).

The roster design follows, and parts of its code are adapted from,
[codync-network](https://github.com/leepokai/codync-network) (Apache-2.0). See
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
