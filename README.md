# iroh-mesh2

A design and prototypes for `imesh`: a cluster of equal machines connected by [iroh](https://docs.iroh.computer) instead of WireGuard, with no control plane. It is modeled on [Uncloud](https://uncloud.run/docs/) and Talos KubeSpan, runs on Linux and Windows, needs no root at run time, and works when UDP is blocked. Every machine keeps a copy of one replicated SQLite database, through embedded [Corrosion](https://github.com/superfly/corrosion), and apps add their own tables to it.

```bash
imesh machine init deploy@203.0.113.10    # install the daemon, start a cluster
imesh machine add deploy@198.51.100.20    # add a machine on any network
imesh machine ls                          # every machine, and how it is reached
```

## Documents

- [docs/design.md](docs/design.md): the design and its reasons.
- [docs/getting-started.md](docs/getting-started.md): build a cluster, and add a Windows laptop.
- [docs/extending.md](docs/extending.md): build on the `imesh` library: embed a machine, add tables, methods, and protocols.
- [docs/reference.md](docs/reference.md): commands, options, config, files, ALPNs, methods, tables.
- [docs/comparison.md](docs/comparison.md): the sync engines that were built and measured, and why imesh uses Corrosion.
- [docs/verification.md](docs/verification.md): every check, its result, and how to run it again.
- [docs/reviews.md](docs/reviews.md): adversarial and rubber duck reviews, and what changed.
- [docs/research/](docs/research): notes on Uncloud, KubeSpan, iroh, and iroh address lookup.

## Layout

```
crates/imesh/            the library: run a machine, talk to one (no CLI, no config files)
  src/daemon/            the machine: Builder, cluster operations, links to members, API methods
  src/state/             the replicated database: embedded Corrosion (cr-sqlite tables),
                         the schema gate, and the gossip tunnel over iroh (ALPN imesh/corrosion/1)
  src/net/               network settings, relays, devtunnel exposure, the local API socket
  src/platform/          everything OS-specific: unix.rs and windows.rs
  src/client.rs          calling a machine's API over its socket, iroh, or any stream
  examples/              two_machines.rs: a cluster in one process, with the library only
  build.rs, crsqlite.sh  build cr-sqlite from source and link it statically (musl and Windows)
crates/imesh-cli/        the imesh command, built on the library
  src/cli.rs             commands; machine.rs, ctx.rs: their handlers
  src/install/           installs: for the user with no sudo, as a system service, over SSH
  src/config.rs          contexts in ~/.config/imesh/config.toml
  examples/app.rs        the imesh command plus an app's own tables, methods, and ALPN
vendor/                  Corrosion as a git submodule, plus corrosion.patch; run `cargo x setup` once
prototypes/              evaluation work kept for comparison; see prototypes/README.md
tests/e2e/               end-to-end proofs: Linux in Docker, and Windows
docs/                    design, guides, reference, comparison, verification, reviews
AGENTS.md                the rules for changing the code
xtask/                   project tasks: `cargo x help`
```

Build and test:

```bash
cargo x setup                        # once after clone: Corrosion submodule and patch
cargo x test                         # unit tests
cargo x e2e                          # the whole cluster in Docker
```

The Linux (musl) and Windows builds also build cr-sqlite from source. They need `rustup` with `nightly-2023-10-05`, zig (musl) or mingw-w64 (Windows), and a libclang older than 21. [docs/getting-started.md](docs/getting-started.md) has the details.
