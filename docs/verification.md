# How the design was verified

Each claim in [design.md](design.md) maps to a check that runs against real processes. Run the checks again with the commands below. The last results are in [results/](results).

The state engine of `crates/imesh` changed from the record engine (now [`prototypes/imesh-records`](../prototypes/imesh-records)) to embedded Corrosion. The Linux results below are from the Corrosion engine. The historical Windows W1 to W6 results are from the record engine. The 2026-09-25 work-machine checks below use Corrosion on Windows and Linux.

## Private tunnel review, 2026-09-25

The review covered the pending private devtunnel change, process ownership,
startup and retry behavior, settings updates, and build changes. It found and fixed:

- A library caller could pass an unchecked host tunnel ID to the CLI. The host
  now validates the ID before it runs any command. A unit test covers this path.
- The host published its relay before the CLI was ready. It now waits for the
  ready message, with a 60-second timeout, and retries failed startup attempts.
- Switching from a relay host to a private client kept conflicting host settings.
  Both SSH and Docker updates now clear the replaced settings. A unit test covers
  the transition and preservation of unrelated settings.
- The Windows test did not use DEVTUNNEL_ID when starting its host. It now uses
  the same validated ID for the host and clients.
- The private HTTP example accepted any response status. It now requires 200.

Host and client retry delays are 1, 2, 4, 8, 16, 32, then 60 seconds indefinitely.
They reset after a ready child stays running for 60 seconds. Initial client startup
still returns an error if it cannot become ready. There is no hang probe after
readiness; the CLI manages network reconnects while its process stays alive.

The final unit run passed 19 tests, lint passed, and Windows clippy and release
builds passed. Shell syntax and diff whitespace checks passed. See
[the results](results/private-devtunnel.txt).

A full `cargo x e2e PREFIX=review PORTS=31` run passed stages 1 through 14,
including network partition recovery and admission checks. It stalled in stage 15
while initializing n4. Independent `docker exec ... true` checks against n1 and n4,
and `docker ps`, also timed out. The runner was stopped. The revised private tunnel
stage, including connector restart, has not completed. This is a release blocker,
not an end-to-end pass. The run began before the final review edits; those edits
still need a full run after Docker recovers. Container cleanup failed because
Docker's socket became unavailable; test containers may remain. The user approved committing and pushing with this
check incomplete.

The Windows executable is cross-compiled with MinGW. A Windows-only build is not
verified or ready: crsqlite.sh assumes a Homebrew include path, build.rs passes
only a Unix-oriented environment, and the full suite needs Linux containers.

## Unit tests

```bash
cargo x test
```

- `state::schema::tests::a_new_table_and_new_columns_catch_up`: a machine with an older table gets the statements for the new table, the new column, and the index. The newer machine needs nothing. `ALTER TABLE` and a full `CREATE TABLE` give the same shape.
- `state::schema::tests::columns_added_on_both_sides_conflict`: two machines that added different columns to one table report a conflict that names both.
- `state::schema::tests::a_changed_column_type_conflicts`
- `state::schema::tests::internal_tables_are_left_out`: SQLite, cr-sqlite, and Corrosion tables are not part of the table list.
- `state::schema::tests::growing_keeps_this_machines_column_text`: a machine that lacks a column keeps its own table definition and adds only the new column.
- `state::schema::tests::reserved_table_names_are_refused` and `reserved_names_are_found_by_parsing`
- `state::schema::tests::statements_are_compared_as_corrosion_renders_them`: `key` and `KEY` are the same column.
- `state::tests::reads_are_read_only`: the read guard refuses writes, transactions, savepoints, ATTACH, DDL, and setting a PRAGMA.
- `state::tests::the_schema_reader_passes_the_read_guard`
- `state::tests::writes_change_rows_of_replicated_tables_only`: `sql.execute` may change rows of app tables, not `imesh_*`, Corrosion's tables, or the schema.
- `cli::tests::cli_is_valid` (`imesh-cli`): clap accepts the command tree. Two flattened `Opts` groups once made every `machine init` panic.

## The library alone

[`crates/imesh/examples/two_machines.rs`](../crates/imesh/examples/two_machines.rs) uses only the `imesh` library: two machines in one process, `init_cluster`, `admit`, `join`, and a row written on one and read on the other. It needs Linux or Windows.

```bash
cargo zigbuild --release -p imesh --example two_machines --target aarch64-unknown-linux-musl
docker run --rm -v $PWD/target/aarch64-unknown-linux-musl/release/examples/two_machines:/tm:ro debian:trixie-slim /tm
```

Result: PASS. `b joined, 2 members`, then `b reads "from a"`.

## Windows build checks

`cargo x windows` runs clippy with warnings as errors for `x86_64-pc-windows-gnu` and builds the `imesh` command and the example app. It passes. The user install, named pipe, and Corrosion daemon also pass the work-machine checks below. The system service and group access have not been checked on Windows.

## POC p1: iroh building blocks

[`prototypes/router`](../prototypes/router) starts four endpoints in one process with no relay and no n0 lookup.

```bash
cd prototypes/router && cargo run -q
```

It shows a custom ALPN through `Router`, an `after_handshake` hook that rejects a non-member, and iroh-gossip carrying a broadcast.

## End to end on Linux

[`tests/e2e/linux.sh`](../tests/e2e/linux.sh) builds the CLI for the host and the daemon for Linux musl. It starts eight Debian containers with systemd and sshd on three Docker networks, plus an `iroh-relay` container. Then it drives the real CLI over real SSH.

```bash
tests/e2e/linux.sh                                          # about 5 minutes
KEEP=1 tests/e2e/linux.sh                                   # keeps the containers
CRATE=prototypes/imesh-records PREFIX=r PORTS=25 tests/e2e/linux.sh  # the former engine, stages 1 to 20
CRATE=prototypes/imesh-docs PREFIX=b PORTS=24 tests/e2e/linux.sh     # another prototype
```

| Stage | Claim | Result |
|---|---|---|
| 1 | `machine init` installs the binary, starts the systemd service, creates a cluster, writes a context | PASS |
| 2 | A machine that is already in a cluster refuses a second `init` | PASS |
| 3 | `machine add ops@n3`, a user with no sudo, installs for `ops`: the daemon runs as `ops`, the socket is `~/.local/share/imesh/imesh.sock` mode 600, nothing is written to `/etc`, and the crontab has the `@reboot` line. A root login runs the daemon as `imesh`, and `--run-as imeshd` as `imeshd` | PASS |
| 4 | Every machine sees every other machine `up` | PASS, `direct` paths |
| 5 | The CLI reaches a user daemon over SSH `dial-stdio` | PASS |
| 6 | The CLI reaches a machine over iroh with its own key | PASS, through the n0 relay |
| 7 | One machine forwards a call to another | PASS |
| 7b | An app method opens an app ALPN to another machine | PASS |
| 8 | A write on one machine reaches the others | PASS |
| 9 | A paused machine shows as `down`, the rest keep working, and it catches up | PASS, `down` after 5 s, caught up 0 s after resume |
| 9b | A 45 s network split (nftables): both sides write, then all converge, and the later write wins | PASS, converged 4 to 5 s after the heal. The later value also sorts last, so this stage cannot tell the merge rule from "the later write wins" |
| 10 | A daemon restart keeps the key and the state | PASS |
| 11 | `ctx add`, `ctx use`, `ctx conn <n>`, and `IMESH_CONTEXT` | PASS |
| 12 | A dead connection falls through to the next one | PASS |
| 13 | An unknown CLI key is rejected over iroh | PASS |
| 14 | `machine rm` resets the machine to a new key and out of the cluster | PASS |
| 15 | Two machines on networks with no route between them connect through a self-hosted relay with `--no-n0` | PASS, path `relay` |
| 17 | 1,000 distinct app rows of 1 KiB, written on one machine in one transaction, reach another | PASS, written in 0.1 s, all there 0.2 s after the write began |
| 18 | With all UDP blocked, a machine joins and syncs through a members-only relay that another machine's daemon serves | PASS, path `relay`. Corrosion's UDP gossip stays on loopback and travels over iroh |
| 19 | A machine installs itself with `imesh daemon install`, prints a ticket, and joins with the join ticket from `machine add <ticket>`, with no SSH from the CLI | PASS |
| 20 | A call over the CLI's iroh connection is forwarded | PASS |
| 20 | A call that a machine forwards for a CLI key runs as that key: `example.note`, which refuses CLI keys, is refused on the target | PASS |
| 20 | A second CLI key works after `client add` and fails after `client rm` | PASS |
| 20 | A write acknowledged while the machine is cut off survives `kill -9` | PASS, with `synchronous = FULL` |
| 20 | A refused `init` keeps the daemon PID and its settings | PASS |
| 20 | `machine update` keeps relay-only settings, adds a label, and the relay-only pair stays up | PASS |
| 20 | `machine rm` over the CLI's iroh connection resets the machine | PASS |
| 20 | After `machine update deploy@n1 --system --run-as imeshsvc`, the daemon runs as `imeshsvc`, the socket is group `imesh` mode 660, and `deploy` reaches it through that group | PASS |
| 21 | A table from `schema apply` reaches every machine, and rows flow both ways | PASS |
| 21 | A machine that was paused while another added a column and a table and wrote to both has all of it after it resumes | PASS, 1 s after resume. Without the schema gate, these rows were lost |
| 21 | Dropping or changing a column is refused where it is applied and reaches no one; `sql --exec` cannot drop a table | PASS: `won't remove column without the destructive flag set` |
| 21b | During a split, two machines add different columns to one table. After the heal, the link reports `schema conflict` and carries no rows | PASS |
| 21b | A schema with both columns, applied on one machine, ends the conflict; rows from both sides and a `kv` write arrive | PASS, 2 s after the fix |
| 21c | A compiled table that grew at run time does not stop a restart | PASS |
| 21c | A machine reset in stage 14 joins again with a new key and syncs both ways | PASS |
| 22 | A machine exposes its served relay through a dev tunnel (a stand-in `devtunnel` CLI), with the `imesh` labels; the tunnel host starts again when it stops; a second machine syncs with the tunnel URL as the cluster relay | PASS |
| 23 | After a reboot of n3, the crontab starts the detached daemon again with the same key | PASS |
| 23 | With linger on, `machine update ops@n3 --service systemd` moves the daemon to a systemd user service and removes the supervisor and the crontab line; after a reboot it runs again with no login | PASS |
| 24 | `machine add ops@n9 --docker`, on a host that runs its own Docker (as a NAS), by a user in its docker group with no sudo: the container runs with a restart policy, a read-only root file system, user 65532, and no capabilities; it syncs, and `n1` on the same LAN reaches it `direct`; it keeps its key over a container restart and over `machine update --docker` | PASS |
| 24 | `--docker-network bridge`: a second container behind Docker's NAT on the same host syncs, and `n1` reaches it `direct` | PASS |
| 24 | `--relay-only` in a bridge container: it opens no UDP socket except Corrosion's on loopback (read from the kernel's table for its network namespace), syncs, and `n1` reaches it as `relay` | PASS |
| 24 | `machine rm` resets all three | PASS |
| 16 | Footprint | 44.4 MB binary, 67 to 82 MB RSS, 22 to 31 threads |

## Private devtunnel CLI forwarding

The private implementation creates tunnels without anonymous access and runs
`devtunnel connect` as the logged-in user on each peer. It requires a dedicated
single-port tunnel and the same free loopback port on every client. The host runs
`devtunnel host`. iroh still checks machine membership.

The real CLI check used a temporary Linux tunnel with an empty access-control list.
A logged-in client reached its loopback HTTP forward with status 200; a request to
the public HTTPS URL without credentials returned a login redirect, status 302.
`crates/imesh/examples/private_tunnel.rs` checks the library's child startup,
forwarding, and port release on shutdown against such a tunnel. Result: PASS,
HTTP 200 and port released. The check found a readiness race: the forwarding-rule
message arrives before the CLI is ready. imesh now waits for both the connected
message and the exact loopback listener message. A unit test covers both orders.

The full Docker run passed through the stage 20 key-revocation, durable-write,
update, and reset checks, then timed out on the existing group-socket check.
Docker subsequently stopped answering even `docker info`; the repeat run could
not start. Stage 22 now covers private CLI forwarding with a local stand-in, but
that revised stage has not completed. Do not treat this as a full end-to-end pass.
The ARM64 build used a local Zig wrapper that omitted the unsupported
`--fix-cortex-a53-843419` linker option for the Docker VM test only.

The final unit run passes all 17 tests, lint passes, and Windows clippy and release builds pass.

The temporary private tunnel and HTTP test service were deleted after the check.
Neither work machine has an imesh installation or data left.

## Windows and Linux work machines, 2026-09-25 UTC

The current Corrosion build ran on psworkwin01 (Windows 11 Enterprise x86_64) and
psworklinux01 (Linux x86_64). The Mac only uploaded files and issued setup and check
commands over SSH. It was not a member or relay. See [the results](results/work-devtunnels.txt).

| Check | Result |
|---|---|
| User install on Windows and Linux | PASS. Windows user task and named pipe; Linux systemd user service with linger. |
| Windows serves the relay through a real dev tunnel | PASS. Both peers use only the Windows tunnel; KV replication and forwarded calls work both ways. |
| Switch the cluster relay to a real Linux dev tunnel | PASS. Both peers use only the Linux tunnel; KV replication and forwarded calls work both ways. |
| Relay-only peers with n0 disabled | PASS after restart. Both addresses contain only the dev tunnel relay; the peer link reports `up`, `relay`, and schema `ok`. |
| Restart each daemon | PASS. Each keeps its key and database and reconnects. |
| SQL and schema changes | PASS. A new table and rows reach both peers. Linux catches up with a new column and its row after being offline. |
| No Mac test connection | PASS. A detached systemd job on Linux completes six rounds of local and forwarded Windows writes and checks replication both ways. |

Local checks for the build fix: `cargo x test` passes all 15 tests, `cargo x lint`
passes, and `cargo x windows` passes. `cargo x e2e PREFIX=work PORTS=29` was
attempted but stopped before any Docker test. The local ARM64 linker rejects
`--fix-cortex-a53-843419`. The historical Docker PASS results below were not
reverified in this run.

The user later asked to stop and remove this setup. Both daemon installations,
settings, data directories, uploaded binaries, and imesh tunnel resources were
removed. The logged-in devtunnel CLI and SSH configuration remain. These results
record the earlier anonymous web-forwarding implementation, not the new private
CLI forwarding implementation above.

The Linux devtunnel installer placed the binary in `~/bin`. Fish needed
`fish_add_path $HOME/bin`. The systemd user manager had a separate PATH, so this
host also needed an `imesh.service.d/devtunnel.conf` drop-in with that directory
in `Environment=PATH=...`. Each host used its own existing devtunnel login.

The first Windows binary failed to load. MinGW placed the SQLite archive's
`.llvmbc` metadata section outside the PE image. `crsqlite.sh` now removes
`.llvmbc` and `.llvmcmd` before making the archive. The build records the script
contents and rebuilds an archive when the script changes. The rebuilt binary
started and passed the checks above.

## Windows and dev tunnels

[`tests/e2e/windows.sh`](../tests/e2e/windows.sh) uses pswin01, a Windows 11 ARM VM that runs the x86_64-pc-windows-gnu build, and a Docker cluster on the Mac. SSH to pswin01 goes through the `sshdt-pswin01` dev tunnel. The script uses it only to type the commands a person at the Windows console would type. imesh itself does not use SSH for Windows.

```bash
CRATE=crates/imesh tests/e2e/windows.sh
```

| Stage | Claim | Before the review fixes | After |
|---|---|---|---|
| W1 | A Linux cluster over SSH | PASS | PASS |
| W2 | `imesh daemon install` registers the per-user scheduled task and starts the daemon on its named pipe; `machine id` prints a ticket | PASS | PASS |
| W3 | `machine add <ticket>` on the Mac and `machine join` on Windows | PASS, `direct` | PASS, `relay` |
| W4 | KV both ways, a forwarded call, the app ALPN into Windows, the Mac CLI straight to Windows over iroh | PASS | PASS |
| W5 | pswin01 serves the relay through a dev tunnel; a Linux machine that can reach only that tunnel's IP on TCP 443 joins with `relay set` and syncs | PASS | not run: the dev tunnels service returned HTTP 429 |
| W6 | Footprint | 29.8 MB exe, 63 MB working set, 17 threads | |

These runs used the record engine. With Corrosion, the Windows exe is 59.7 MB, with cr-sqlite linked in and no DLL. That build was not run in the historical VM checks below. The newer work-machine checks above now cover Corrosion on Windows.

Prototype B passed W1 to W5 with a 42.2 MB exe and 83 MB working set.

The W5 run after the fixes hit the dev tunnels rate limit (HTTP 429) after many test runs in one afternoon. The SSH path to pswin01 goes through the same service and timed out at the same time. `devtunnel limits` then showed the cause: the account had used its whole 5.0 GB monthly bandwidth (`BandwidthPerUser`, resets 2026-10-06). Copying the 25 to 80 MB exe to the VM through the SSH tunnel on every run, and the relay traffic of W5, used it up. A dev tunnel is a fine fallback relay for light traffic, but not a path for test binaries.

The VM itself kept running, with VMware Tools up. Its console was black because of a VMware Fusion 26H1u1 problem on the macOS 27.2 beta: Fusion's display process exits right after it starts. At the time, Windows runs on the Corrosion engine had to wait for the bandwidth reset or another route to that VM. `tests/e2e/windows.sh` has two ways to reach the VM:

- `WIN_VIA=ssh` (default): `ssh $WIN`.
- `WIN_VIA=vmrun`: through VMware Tools, with no network path and no tunnel. A person runs `tests/e2e/vmrun-agent.sh <queue dir>` in a terminal and types the VM password and the Windows login at hidden prompts. The test script only queues file copies and program runs for it.

`SKIP_W5=1` skips W5, which is the only stage that needs dev tunnel bandwidth.

The manual Windows work before the script found these problems:

- iroh's own DNS resolver failed on the VM while the OS resolved the names. Fixed with `OsResolver`.
- The headless child kept the installed exe locked during a reinstall. Fixed: the install stops the process that runs from that exact path.
- The `devtunnel` CLI wants `--all-labels` repeated, and a port URL appears only after the host connects. Fixed in the supervisor.
- A machine that can reach only the tunnel could not join until every member used that relay, because an iroh endpoint has one home relay. Fixed with `imesh relay set`.

## Bugs the checks found

Before the reviews, the checks caught these bugs in prototype A, the record engine:

- `cluster.add` returned the entry machine's address before it had a home relay. With `--no-n0`, the address had only unreachable IPs. Fixed: wait up to 10 s for `endpoint.online()`.
- A paused peer took 48 s to show as `down`: the exchange had no deadline. Fixed, and later replaced by a separate ping.
- A rebuilt binary with the same version did not reinstall. Fixed: the CLI compares SHA-256 hashes, which also check the upload.
- `machine.reset` left the old socket file for 2 s. Fixed.
- `dial-stdio` closed the connection at stdin EOF, before the reply. Fixed: it closes only the write half.
- The state file was written on every change, and stage 17 took 3.3 to 11 s. Fixed with batched writes; now 0.1 s.
- Prototype B deadlocked with 1,000 distinct values, and never retried a failed blob download. Fixed in B.
- On the Corrosion engine, stage 21 found that plain Corrosion silently loses rows for a table or column that a machine does not have yet when the change arrives. Fixed with the schema gate on each link; see [design.md](design.md#the-schema-gate).
- On the Corrosion engine, convergence after the 45 s split took 21 s with Corrosion's default `max_sync_backoff` of 15 s. Now 5 s, and convergence took 4 to 5 s.

The reviews then found the problems listed in [reviews.md](reviews.md). Stage 20 checks their fixes.

## What is not verified

- Real servers on different networks. The relay tests used Docker networks and a VM, not two real NATs across the internet.
- The full Docker suite on x86_64 Linux. The work-machine checks cover a smaller set.
- More than eight machines, and a database with more than about 1,000 app rows.
- That a connection already open from a removed member closes. Stage 20 checks revocation for new connections only.
- That removal ends a relay session on a machine's served relay. The code calls iroh-relay's `Clients::disconnect`; no stage removes a machine that uses a served relay.
- HTTP proxy support. The code calls `proxy_from_env()`, but no test runs behind a proxy.
- The embedded relay over HTTPS with `--serve-relay-cert`.
- `--pkarr` and `--mdns` inside imesh. The address lookup study measured them in its own harness.
- The restart after `machine.reset` on Windows.
- Clock skew between machines. The Corrosion merge rule does not use time, but no test checks it.
- Private CLI forwarding between Windows and Linux imesh daemons.
- A power loss. `synchronous = FULL` is set, but the tests only kill the process.
- A daemon on macOS. It cannot run there: macOS answers only 127.0.0.1 on loopback.
