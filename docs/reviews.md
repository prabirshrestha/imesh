# Reviews of the imesh design and prototype A

Two kinds of review ran on 2026-09-23, after prototype A and the comparison were complete:

- **Adversarial review.** One reviewer per model family got the same prompt and rubric (the `interrogate` skill). They read the docs, the prototype A source, the test scripts, and the results, read-only.
- **Rubber duck review.** A reviewer read the docs as a newcomer, explained the whole life cycle back step by step, and checked each step against the code and the evidence.

This page records what they found, what I decided, and what changed. The changes landed in prototype A; `tests/e2e/linux.sh` stage 20 and the unit tests of its state and sync modules check them. Prototype A is now [`prototypes/imesh-records`](../prototypes/imesh-records). `crates/imesh` switched to Corrosion afterwards and kept every fix outside the state engine; see [Engine switch](#engine-switch).

## Intent given to the reviewers

> imesh is a design plus proof-of-concept prototypes for a leaderless cluster of equal machines connected by iroh 1.2 instead of WireGuard, with the UX of Uncloud. Requirements: no control plane and no quorum; any machine can be the entry point; the cluster keeps working when machines or parts of the network go down; authentication only from iroh; no root at run time; Linux and Windows, where Windows may have no SSH; works when UDP is blocked; apps extend it with custom ALPNs and API methods; minimal footprint; it is the foundation other projects build on, and apps will sync more than machine records. The docs claim that the prototype proves every statement, and the comparison recommends prototype A's sync engine over iroh-docs (B) and Corrosion (C).

## Reviewers

- **Reviewer A:** GPT-6 Astra, `xhigh`, through `codex exec`, read-only sandbox. 14 findings.
- **Reviewer B:** Grok. The skill names Grok 4.6; this account offers only `grok-4.7`. The run stopped at the account's free usage limit before it produced findings. **Dropout.**
- **Reviewer C:** Claude Opus, as a read-only subagent. 16 findings.
- **Rubber duck:** Claude Opus, as a read-only subagent. 27 gaps.

## Act on (fixed)

| Finding | Raised by | Change |
|---|---|---|
| Removal did not revoke connections that were already open; the hook runs only at the handshake, and CLI keys could never be removed | A, C, duck | `admission.rs` records every admitted connection and `enforce()` closes those of keys that are no longer allowed, on every state change. New `client.remove`, `imesh client rm`. Stage 20 revokes a CLI key. |
| Anti-entropy compared records with another rule than merge, so a removal and a re-add across a split could diverge forever | C | One function, `state::compare`, orders records for both. Summaries carry a deleted bit. A unit test runs the split case in both directions. |
| `machine init` and `add` rewrote settings and restarted the daemon before they refused a machine that was already in a cluster; the env file was read without sudo | C, duck | The CLI asks the running daemon first and refuses before any change. It reads the env file with sudo. Stage 20 checks that a refused `init` keeps the same daemon PID and settings. |
| Sync sent the whole summary and diff in one frame with a 16 MiB limit, and the exchange doubled as the liveness probe with a 10 s deadline | A, C | New `imesh/sync/1`: chunked summaries and records, a separate 5 s ping for liveness, a background exchange with its own 120 s deadline. |
| Every applied record was pushed again to all peers: N x (N-1) messages per write | C | Only this machine's own writes are pushed. The exchange repairs the rest. |
| An acknowledged write could be lost on a crash | A | `State::commit`: the API answers only after the write is on disk, with a 20 ms group commit. Stage 20 kills a cut-off machine with `kill -9` right after a write and reads it back. |
| Reset could race with a state file write already in progress and restore the old state under a new key | A, C | A file lock makes `freeze()` wait for any write in progress, and stops merges and writes before reset deletes files. A unit test checks that a frozen state accepts nothing. |
| The CLI's own iroh connection could not forward calls, so `call -m` and `machine rm` failed over iroh, and the tutorial led into it | A, duck | Calls carry a `Caller`. Only calls from members are refused for forwarding. Stage 20 forwards over iroh and runs `machine rm` over iroh. |
| Remote settings became shell source in a script that runs as root | A, C | Settings reach the install script as base64 and are decoded into the env file. User and group names are checked. |
| `/tmp/imesh-ssh-$USER` could be planted by another local user; `sudo imesh daemon install` used a predictable temp file | C | The SSH control directory is `~/.ssh/imesh-cm`, used only when the current user owns it. The local install uses a private temp directory. |
| The CLI connection probe had no deadline, so a hung daemon blocked the fallback | A | Each attempt, including the ping, has a 20 s deadline. |
| `--connect` could add machines to an unrelated context, and a ticket add failed after admitting when there was no context | A | Only the context that selected the entry machine changes. The join ticket prints before the config is saved. |
| `relay set` added to the startup relays instead of replacing them; clearing it did nothing; `--no-n0` machines had no relay transport | A, C, duck | A non-empty setting is the exact relay set. Clearing it restores the startup relays. `--no-n0` starts with an empty custom relay map, which keeps a relay transport. |
| `machine.inspect` returned the relay token; ticket joins did not carry relay settings | duck | The token lives in `settings/relays` and `inspect` never returns it. `cluster.add` returns the relay setting, and `cluster.join` applies it before it dials. |
| The CLI could not use token-protected or private-CA relays | A, C | Connections have `relay_token`, `relay_ca`, and `no_n0`. |
| HTTP proxy support was claimed but not configured | A | Both endpoints call `proxy_from_env()`. |
| A root login ran the daemon as root, against the no-root goal; group access to the socket was untested | C, duck | A root login runs the daemon as the system user `imesh`. Stage 20 switches a machine with `machine update --run-as` and reaches the socket through the group. |
| No upgrade path; re-running `add` changed settings | duck | `imesh machine update` keeps what you do not change. Stage 20 updates a relay-only machine and it stays linked. |
| No library, no state API for apps, and no way to build a handler that needs the endpoint | A, C, duck | The crate has `lib.rs`. `Machine::state()`, `members()`, and `connect()` are public. Methods get the `Caller`. `Builder::protocol_with` builds a handler after the endpoint binds. Built-in methods cannot be replaced. |
| Windows reinstall from the installed exe did not apply new settings; the pipe name could be taken first by another user | A, C | The install always stops the daemon first. The pipe is `\\.\pipe\imesh-<user>`. |
| A failed join left the machine half joined | C | The join forgets the received records on failure. |
| Removing the machine that serves the cluster relay cut off the machines that use it | duck | `machine rm` refuses it unless `--force`, and says to change `relay set` first. |
| Docs: "Signed" in the comparison table, stale numbers, broken tables, undefined terms, the tutorial's root login and missing arm64 export, the env file mode, the tunnel label length, TLS text that contradicted itself, missing `--log-file` | A, C, duck | Rewritten. `design.md` has a Terms section and an Operating section. |

## Consider (not changed yet)

- **Range or hash reconciliation.** The summary still grows with the key count (Claude). The limit is documented at about 10,000 keys.
- **Tombstone collection.** Removed keys stay forever (Claude). Documented.
- **A relay connection of a removed machine stays open until it reconnects** (Claude, GPT). iroh-relay's `AccessControl` has no way to disconnect a client. Members close its QUIC connections and reject new ones, so the relay carries only packets that nobody accepts. Documented.
- **Stage 17 counts keys, not usable values, for prototype B** (GPT). B's `kv.count` lists entries, and a value may still be downloading. `comparison.md` says so.
- **Names are unique only per machine view** (duck). The CLI refuses an ambiguous name. A split can still admit two machines with one name.
- **Streaming methods** (duck). The protocol has item frames, but methods return one value.

## Noted

- `daemon.rs` mixed layers (Claude). It is split into `admission.rs`, `sync.rs`, `methods.rs`, and `os.rs`.
- The three prototypes are copies of one shared layer that drifts (Claude). B stays an evaluation snapshot taken before these fixes. C now copies A's CLI, API, and install layer. A is the one that carries on. `comparison.md` says so.
- Windows restart after `machine.reset` depended on the task's restart setting (duck, Claude), which may apply only when the task fails to start. Review pass 4 raised it again. Fixed: the daemon now starts again as a new machine in the same process, on every platform.
- The Windows log is not rotated (duck). Documented.

## Dismissed

- **"The comparison favors A over B on secrets and removal"** (Claude) is half right. B's per-machine-namespace variant would avoid a shared secret, and B's `enforce()` closed open connections when A did not. A now closes them too. The comparison now says that A wins mainly on footprint, platform reach, build simplicity, and the time-based conflict rule, and that B is the better tool for large values.

## Refactor passes

After the fixes above, prototype A went through simplicity refactors. Each pass had a new reviewer, who read the whole crate and reported only material items. The full Linux suite ran after each pass.

**Pass 1** (20 items, all applied). One key module and typed getters for the state. Built-in methods as plain async functions registered through `Builder::method`. One endpoint builder for the daemon and the CLI. One env format for the install over SSH, the local install, and the Windows env file. `provision` moved into `install.rs`. Typed replies (`Inspect`, `MachineRow`, `Admit`, `ClientRow`, `RelaysInfo`) and one typed `Client::call`. One peer map that owns each link task. The suite then found a clap panic in `machine init`: two flattened option groups had the same name. A unit test now checks the command tree.

**Pass 2** found five bugs the refactor had kept or added:

- The link backoff never went back to 1 s after a link that had been up.
- The background loop could remove the join ticket's relays before the join dialed.
- A failed join again cleared a member's whole state.
- Links started during a failed join kept dialing.
- Every API call, even `ping`, wrote the state file.

It also removed the unused streaming reply frame, moved the e2e helpers `kv.fill` and `kv.count` into the example app, and gave labels one parsed type.

**Pass 3** found a security bug: a CLI key could call a method on another machine through forwarding, and the target served the call as a member. Now the entry machine names the key in the request (`via`). The target accepts `via` only from a member and only for a key that is still authorized, and it serves the call as that key. Stage 20 checks it. Pass 3 also found these:

- `Builder::spawn` returned a half-started machine. It now starts everything but the local API and returns a `Running` handle with `shutdown`.
- `machine.reset` called `process::exit` inside the library. `run` now does it, and an embedding app decides for itself.
- Two local writes to one key could apply in the wrong order.
- A push held up by flow control stopped the pings.
- The state file was encoded under the lock and written on an async thread.
- A failed named-pipe connect left the Windows listener with no pipe.

**Pass 4** found no security issue. It found gaps around reset and shutdown:

- A failed shutdown skipped the data deletion, so the service restarted with the old key.
- The devtunnel host task outlived `Running`.
- Link tasks and the machine held each other, so a dropped `Running` kept dialing.
- A record near the 16 MiB frame limit stopped anti-entropy for every key. Records are now limited to 1 MiB.
- A write that raced a reset waited 10 s before it failed.

`run` now returns why it stopped, deletes the data even when the shutdown fails, and leaves the restart to its caller. `imesh daemon` restarts in the same process, so Windows no longer depends on the task's restart setting.

**Pass 5** checked the in-process restart. Tasks, the endpoint, the relay port, the socket, and the devtunnel child do not outlive the old machine. It found four more issues:

- Each run made new signal listeners, so a SIGTERM during a restart was lost. `daemon::serve` now owns the loop and one listener for all runs.
- Local connections outlived the run, so on Windows the next run could not create the named pipe again. The connections now belong to the local API task.
- A relay shutdown error skipped the endpoint shutdown. `shutdown` now runs every step.
- The key was deleted before the app data. Now it goes last, so a failed reset keeps the old identity and can run again.

**Pass 6** found one material gap: the state file was renamed into place, but its directory was not synced, so an acknowledged write could be lost on power loss. The directory is now synced after the rename. The same applies to key files. Pass 6 also led to these smaller changes:

- The local API now closes its connections on a stop signal and waits for them, instead of aborting its task.
- Stale doc links were fixed.
- It noted that one machine with a clock far in the future moves every clock forward. That is documented in design.md, not capped.

**Pass 7** found a security gap: the served relay checked a key only when it connected, so a removed member or a revoked CLI key kept its relay session. Removal now also ends that session through iroh-relay's `Clients::disconnect`. Pass 7 also led to these changes:

- `protocol_with` now gives app handlers the `Machine`, not only the endpoint and the state, so they can dial members.
- A small race could start links while `Running` was dropped.
- Labels with a comma are refused, because labels travel comma-separated.

**Pass 8** found nothing material. Its three optional notes were applied: the endpoint docs say that it talks only to members, the serving side of an exchange has the same 120 s limit as the side that starts it, and the internal `Inner` type is hidden from the docs.

After 8 passes, prototype A is about 4,000 lines of Rust. `cargo clippy` and `cargo doc` give no warnings, and the full Linux suite passes with 31 checks.

## Engine switch

After the refactor passes, the user chose Corrosion as the state engine of `crates/imesh`, for SQL tables that are easy to sync. The engine of prototype C moved into `crates/imesh`. The record engine became `prototypes/imesh-records`. The CLI, API, admission, relays, and install layer stayed, with all fixes above. The review findings about the record engine (anti-entropy order, the 16 MiB frame, the group commit, tombstones, range reconciliation) apply to `prototypes/imesh-records` only.

The switch brought these changes:

- **Data loss in plain Corrosion.** E2e stage 21 found it. A machine that was paused while another machine added a column and a table missed the rows written to them. Corrosion counted those changes as received, because the machine had no place to store them, and never fetched them again. Fixed with the schema gate: each link exchanges the tables of both sides, each side applies what it lacks, and gossip passes only while both sides have the same tables and columns. Stages 21 and 21b check it, including a conflict between two machines that added different columns.
- **Durability.** Corrosion opens SQLite with `synchronous = NORMAL`. `vendor/corrosion.patch` sets `FULL`, so an acknowledged write survives a power loss.
- **Slow repair.** Convergence after a 45 s split took 21 s. `max_sync_backoff` is now 5 s, and it takes 4 to 5 s.
- **Resets.** The database directory carries the key (`corrosion-<id>`), so a new identity never opens the old database. Old directories are removed at the next start when Windows keeps them open.

### First review of the new engine

A new reviewer read the ported code and found 8 material issues. All are fixed, and stage 21c checks the first two:

- **Reset, then rejoin.** The gossip tunnel's socket tasks held the tunnel, so its sockets outlived the machine. After an in-process reset, the new identity could not bind its members' gossip addresses and exchanged no gossip. The tasks now hold a weak reference, and shutdown closes the tunnel.
- **Restart after the schema grew.** At start, the compiled schema was applied as full table definitions, so a column added at run time to a compiled table made every later start fail. Now the start applies only what the compiled schema adds.
- **`sql.query "BEGIN"`.** SQLite counts `BEGIN` as read-only, so a pooled read connection could stay in a transaction and serve old data, also to admission. Reads now run under SQLite's authorizer, and a connection in a transaction is rolled back.
- **Bypassing `sql.execute`.** A text check can be bypassed, for example through cr-sqlite's `crsql_changes` table. Each statement is now compiled under SQLite's authorizer: rows of app tables only.
- **A gate that stayed closed.** A catch-up that failed once was not tried again. It now runs again at each keepalive.
- **Reserved names.** A table named like cr-sqlite's or Corrosion's own was left out of the gate. Such names are now refused.
- **DDL in `Store::write`.** A write that changes a table now fails.
- **Column spelling.** When a machine lacks only columns, it keeps its own table definition and adds the new columns, so Corrosion does not see its columns as changed.

The reviewer's note that Corrosion's HTTP API accepted anything from a local process led to a random bearer token that only the daemon's memory holds.

The fixes had two bugs of their own, which the e2e suite caught at once. First, the authorizer also refused cr-sqlite's internal statements, so no write worked; the authorizer now checks only the caller's statements while they compile. Second, Corrosion stores statements as its parser renders them (`key` becomes `KEY`), so a restart found the compiled schema different from the database; compiled statements are now rendered the same way.

### Second review of the new engine

A second reviewer checked the fixes and looked for problems they brought in. It found no way around either SQL guard: a caller cannot create a trigger or a view, so only cr-sqlite's own triggers run unchecked, and `WITH`, `REPLACE`, and upserts still report the table they write. It found these, all fixed:

- **An index alone.** At start, a compiled schema that added only an index to an existing table was skipped. The start now also adds indexes that only the compiled schema has.
- **A failed read after a schema change.** If reading the tables failed after a schema change, the gate compared the old tables and could open. This was the last way for a row to reach a machine that lacks its column. The gate now stays closed until a read succeeds.
- **A late frame from an old connection** could leave a member with an old copy of the tables, and the gate closed until the next reconnect. Each side now sends its tables again every 10 seconds.
- **A long catch-up blocked the reader,** so a large `ALTER` could drop the link. The catch-up now runs in its own task.
- **Smaller notes.** `crsql_changes` could not be read under the read guard; it now can. A compiled schema with statements other than `CREATE TABLE` and `CREATE INDEX` is refused instead of partly ignored. A NOT NULL column without a default is refused, because a machine whose table has rows could never add it, and the link would stay closed.

### Third review: security, correctness, clarity

Three reviewers read the whole crate again: one for security, one for correctness, and one for how easy the code is to follow. Fixed:

- **The gossip tunnel.** A member could make this machine send datagrams to any local UDP port, and open sockets without limit. Now a member may send only to the gossip port or to a port that this machine's Corrosion recently sent it from, with at most 32 sockets per member.
- **The Windows pipe.** Another local user could create the pipe first. The daemon's pipe now has an access list for its owner and SYSTEM only and refuses remote clients, and a client checks that the process that serves the pipe runs as its own user.
- **Tables from members.** A member's table list is now checked as `schema.apply` is: `CREATE TABLE` and `CREATE INDEX` only, plain names, no reserved names, at most 1000 tables.
- **Smaller fixes.** Peers of a join count in the admission set; `sql.query` stops at 100,000 rows or 64 MiB; `machine rm` checks the relay setting from the replicated state; a failed link connection is closed; `schema.apply` finds `imesh_*` names by parsing.
- **Clarity.** The link code got named helpers (`first_connection`, `read_frame`, `send_tables`, `live_incoming`, `Peer::is_up`), named constants, and timeouts in messages. `Snapshot::active_members` and `is_member` replaced a method with the same name as a field. The machine's fields are grouped. Cluster operations became `Machine` methods, and the API methods only call them.

### Installs with no sudo

The user asked for installs that work like tmux: no sudo. A user install is now the default. The binary, data, settings, and socket are in the user's own directories, and the daemon runs as a systemd user service with linger, or as a detached supervisor that the crontab starts at boot. A root login, or `--system`, still installs a system service. Stage 3 and stage 23 check both kinds, also across reboots.

A reviewer then read the install code and found 13 issues. Fixed:

- **Two daemons on one data directory.** Nothing stopped a second daemon, which took over the socket. A machine now locks its data directory, and `daemon start` refuses when a systemd user service is installed.
- **A user install and a system install on one machine.** Clients would reach whichever socket they found first. An install now refuses when the other kind exists, and `update` keeps the kind the machine has. The "already in a cluster" check asks the socket of the kind being installed.
- **The supervisor's PID file.** A stale file after a reboot could name an unrelated process, which `stop` would then kill. The supervisor now holds a lock on the file for its whole life, and "running" means "the lock is held".
- **Windows install stopped itself** when it ran from the installed path. It now skips its own process.
- **Setting values** with quotes, backslashes, or line breaks could change other settings or reach systemd changed. They are refused.
- **A system daemon read the user's settings** from the home directory it shares. It no longer reads that file.
- **Permissions.** The data directory is created 0700 and the log 0600. The socket gets its mode in a private directory before it moves into place, so there is no moment with a wider mode.
- **Smaller fixes.** An update keeps the socket group. The upload goes to the SSH user's home, because `/tmp` may be `noexec`, and the upload is removed also when the install fails. Paths in the unit, the crontab line, and PowerShell are quoted. A user or group name cannot start with `-`.

Not fixed: detached mode dies at logout where logind has `KillUserProcesses=yes`; there, turn on linger. The Windows pipe check has a small PID-reuse race; it needs a Windows run to change safely.

### The library split

The user asked that `imesh` be a library like iroh, so an app chooses its own command line, config, and paths. `crates/imesh` now has no command line, reads no environment variables or config files, and has no default paths: callers pass `daemon::Config` and `net::Config`. The `imesh` command moved to `crates/imesh-cli`, which also holds contexts, installs, SSH, and the `IMESH_*` variables. `Client` takes any transport through the `Dial` trait, so SSH lives in the command, not the library. All OS-specific code of the library is in `src/platform/`. `examples/two_machines.rs` forms a cluster with the library alone.

A reviewer then read the split. Fixed:

- **The library still read the environment:** the host name from `$HOSTNAME`, and the HTTP proxy. It now uses the `gethostname` crate, and the proxy is `net::Config::proxy`; the command reads `HTTPS_PROXY`. systemd readiness is opt-in (`LocalApi::notify_systemd`).
- **A second daemon could take over a live socket** with a different data directory. A bind now refuses a socket that answers.
- **The supervisor ignored a stop while it waited to restart** the daemon, for up to a minute. **An install accepted a stale socket** from a crashed daemon as a started one; it now checks that the socket answers.
- **Every SSH command closed the shared SSH connection**, also under other commands that used it. Only the install closes it now, to pick up new groups.
- **The Windows pipe's access list** named OWNER RIGHTS, which is Administrators for an elevated process. It now names the user's SID. A setting with a backslash, a normal Windows path, was refused on Windows.
- **Smaller fixes.** The data directory stays locked until a reset has deleted the old key. A failed bind stops the machine. `Builder::spawn` refuses a single-thread runtime with a clear error. cr-sqlite's loader registers once per process. `Machine::serve_local` is public. A failed probe closes its iroh endpoint.

Then the open items were fixed:

- **A Windows daemon at the login screen.** `imesh daemon install --system` on Windows installs a Windows service that starts at boot, as the virtual account `NT SERVICE\imesh`. Its pipe lets in the local group `imesh`, as the Linux socket lets in the group `imesh`. `Access::Group` in the library names that group on both platforms.
- **Windows data directories.** A directory that the library creates gets its own access list (the user, SYSTEM, Administrators), so a parent such as ProgramData no longer decides who reads the key.
- **The reset reply.** `machine.reset` now waits until its reply has left: for a local caller, until it is written; over iroh, until the caller closes the connection, at most 5 s.
- **Logout.** A detached install warns when logind stops the user's processes at logout and linger is off.
- **Paths over SSH** follow `XDG_DATA_HOME` and `XDG_CONFIG_HOME` on the server, as the daemon there does.
- **`uninstall --purge`** stops if the daemon does not stop, so it never deletes data under a running daemon.
- **Clarity.** `Builder::spawn` and `Store::start` are split into named steps (`initial_relays`, `bind_endpoint`, `corrosion_config`, `apply_compiled_schema`). `PeerState` has a `Display`.

Still open: the Windows code (pipe access by group, the service, the private directories) is built and passes clippy but has not run on Windows. The pipe owner check has a PID-reuse window in theory; a squatter's pipe closes with its process, so the window needs a handle duplicated into another process first. `ApiCmd` stays merged into the command tree, because a separate enum would change the command syntax.

The split also fixed a bug that stage 22 found: `machine update --expose-relay` demanded `--serve-relay` again, although the machine keeps its served relay from its settings. The check now runs where the settings are complete: when the machine starts.

## Agreement map

GPT and Claude agreed on the most serious findings on their own: removal did not revoke open connections, the 16 MiB single-frame sync, the missing forwarding for the CLI's iroh connection, the root-shell injection path, and CLI relay credentials. Each also found something the other missed. Claude found the merge versus anti-entropy order bug and the re-run restart. GPT found the lost acknowledged write, the half-fixed reset race, the probe without a deadline, and the proxy claim. The rubber duck found few bugs but many operator gaps, and it was the only one to catch the tutorial trap (`rm` over iroh) as a user would hit it. Every finding that two or more reviewers raised was real.
