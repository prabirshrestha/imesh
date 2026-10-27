# Which sync engine imesh should use

## Decision: Corrosion

imesh now uses embedded Corrosion, the engine of prototype C, in [`crates/imesh`](../crates/imesh). The user decided this after the comparison below, which had recommended A. The reason: with SQLite, apps get tables, SQL queries, and transactions, and syncing them is easy. The former record engine (A) is kept as [`prototypes/imesh-records`](../prototypes/imesh-records) for comparison. Prototype C's own copy was removed; its engine lives in `crates/imesh`.

### What imesh adds on top of Corrosion

- **The schema gate.** A link carries rows only while both sides have the same tables and columns. It is described below and in [design.md](design.md#the-schema-gate).
- **Durable writes.** `vendor/corrosion.patch` sets `PRAGMA synchronous = FULL` instead of `NORMAL`, so an acknowledged write survives a power loss. The patch also has the Windows build fixes.
- **Faster repair.** `max_sync_backoff` is 5 s, not 15 s. Convergence after a 45 s split went from 21 s to 4 to 5 s.
- **UDP over iroh.** Corrosion's UDP and QUIC gossip stays on the machine's loopback address. The daemon carries it over the member link on `imesh/corrosion/1`, so it works through a relay over TCP when UDP is blocked.
- **The rest of imesh stays.** Admission, CLI keys, relays, tickets, and install are the same code as before, with all review fixes.

### A data-loss finding in plain Corrosion

E2e stage 21 paused one machine. Another machine then added a column and a table, and wrote rows to both. When the paused machine came back, those rows never arrived. Corrosion had received the changes for a table and a column that the machine did not have yet. It counted those versions as received and never fetched them again. The loss is silent.

The fix is the schema gate on each link:

1. Each side sends its tables when the link starts and whenever they change.
2. Each side applies what it lacks. This is additive only.
3. Gossip passes in either direction only while both sides have the same tables and columns, and never while a local schema change runs. Held-back gossip is dropped. QUIC sends it again, or the next sync repairs it.

With the gate, the paused machine had everything 1 s after it resumed. A conflict (both sides added different columns to one table) keeps the link closed and shows `schema conflict: ...` in `imesh schema ls`. A schema with both columns, applied on either machine, ends it. In stage 21b the rows arrived 2 s after the fix.

### A against the new engine

| | A: records (`prototypes/imesh-records`) | Corrosion (`crates/imesh`) |
|---|---|---|
| Linux suite | stages 1 to 20 PASS | stages 1 to 21b PASS |
| Windows proof | PASS | not run yet; builds with cr-sqlite linked in (no DLL) |
| Linux binary (aarch64 musl) | 19.2 MB | 43.5 MB |
| Windows exe | 24.7 MB | 59.7 MB |
| Daemon RSS on Linux | 21-33 MB | about 72-74 MB |
| Threads | 17-29 | 22-31 |
| A paused machine shows `down` after | 8 s | 5 s |
| 45 s split: converged after the heal | 3-4 s | 4-5 s |
| 1,000 distinct 1 KiB keys or rows: all on another machine after | 0.2 s | 0.2 s |
| Durability | file sync after a 20 ms group commit | SQLite commit with `synchronous = FULL` |
| Conflict rule | higher `(hybrid logical clock, author)` | per column: more changes, then the larger value, then the site ID |
| App data | JSON records under `app/` | SQL tables with indexes and transactions; add-only schema |
| Daemon platforms | Linux, Windows | Linux, Windows; not macOS, which answers only 127.0.0.1 on loopback |
| Build | cargo | cargo, `cargo x setup`, and cr-sqlite from source for musl and Windows (nightly-2023-10-05, zig or mingw, libclang older than 21) |

The cost is size: 2.3x the binary and more than 2x the RSS. The cr-sqlite build chain and the Corrosion patch also stay. The gain is SQL for apps, and a merge rule that clock skew does not affect.

## History: the three prototypes

This part is the comparison as it was before the decision. "A" is now `prototypes/imesh-records`, and "C" is now the engine of `crates/imesh`.

Three prototypes share one CLI, one API, one transport layer, and one test suite. They differ only in how machines replicate state:

- **A** (then `crates/imesh`, now `prototypes/imesh-records`): a small record map written for imesh. Last writer wins by hybrid logical clock. Per-member QUIC links push this machine's writes, and swap summaries every 10 s.
- **B** (`prototypes/imesh-docs`): iroh-docs with iroh-gossip and iroh-blobs. One document (namespace) per cluster.
- **C** (then `prototypes/imesh-corrosion`, now the engine of `crates/imesh`): Corrosion, the cr-sqlite and SWIM engine that Uncloud uses. Embedded as a library. Its gossip runs over iroh through a loopback bridge.

Everything else started as the same code: SSH install, contexts, tickets, the admission hook, relays, the dev tunnel exposure, Windows support. B and C are evaluation snapshots of that shared layer from before the reviews. At the time, A carried on, with the review fixes in [reviews.md](reviews.md).

My recommendation was **A as the imesh core**, with room for apps to add iroh-docs or iroh-blobs on their own ALPNs when they need what B offers. The rest of this page gives the evidence.

### Results

All three passed the 19-stage Linux suite (`tests/e2e/linux.sh`), run one after another on the same computer so no run shared the CPU. A also passes stage 20, which checks the review fixes. The raw lines are in [results/](results). `results/e2e-a.txt` now holds the latest run of `crates/imesh` on Corrosion; the A numbers here are from the run before the switch. "Windows" is the `tests/e2e/windows.sh` proof on pswin01, a Windows 11 ARM VM that runs the x86_64 build.

| | A: records | B: iroh-docs | C: Corrosion |
|---|---|---|---|
| Linux suite, 19 stages | PASS | PASS | PASS |
| Review fixes, stage 20 | PASS | not ported | PASS (C shares A's CLI, API, and install layer) |
| Windows proof | PASS | PASS | builds with a 4-file Corrosion patch and cr-sqlite linked in statically (no DLL); not run yet |
| Linux binary (aarch64 musl) | 19.2 MB | 25.2 MB | 42.8 MB |
| Windows exe | 24.7 MB | 42.2 MB | 80.5 MB |
| Daemon RSS on Linux | 21-33 MB | 29-45 MB | 65-91 MB |
| Threads | 17-29 | 34-38 | 22-28 |
| A paused machine shows `down` after | 8 s | 12 s | 6 s |
| 45 s split: converged after the heal | 3-4 s | 3 s | 12-16 s |
| 1,000 distinct 1 KiB keys: all on another machine after | 0.2 s | 0.7 s | 0.2-0.4 s |
| Crates behind the engine | none (postcard, serde) | iroh-docs 0.101, iroh-blobs 0.103, iroh-gossip 0.101 | Corrosion 0.2 beta at a git commit, cr-sqlite alpha, rusqlite |
| Build | cargo | cargo | cargo, plus a script that builds cr-sqlite with nightly-2023-10-05, zig, and libclang 20 or lower |

The down-detection times follow each prototype's own keepalive: A pings every 5 s, C every second. B's stage 17 number counts document entries; a value can still be downloading when its entry counts, so B's number is optimistic.

### Semantics

This is where the engines differ most. The e2e suite cannot see all of it, because stage 9b's "later write wins" string also sorts last.

| | A | B | C |
|---|---|---|---|
| Conflict rule | higher `(hybrid logical clock, author)` | later wall-clock timestamp per (author, key), then the larger author | per column: the side that updated the column more times, then the larger value, then the site id. Time plays no part |
| A slow clock | orders its writes after what it has seen | loses its newer writes | no effect |
| A clock more than 10 min ahead | accepted | entries rejected | no effect |
| Member removal | final: a removal beats any add, in merge and in anti-entropy | final at read time | a column; a member that writes `removed = 0` twice beats one removal |
| Open connections of a removed member | closed at once | closed at once | not checked |
| Who can write what | any member, any key | any holder of the namespace secret, any key | any member, any row, and any local process through Corrosion's HTTP API |
| Secrets | none | the namespace secret travels in every join ticket, and a removed machine keeps it | none |
| Recovery after a partition | built into the link | needs an imesh timer; iroh-docs has none | SWIM rejoin plus Corrosion's sync interval |
| Surprises found | the reviews found an order bug between merge and anti-entropy, and a 16 MiB sync frame limit; both fixed | deadlock when the event consumer calls into docs; failed blob downloads never retry | cr-sqlite loads by `dlopen` of a glibc `.so`; no Windows support upstream |

C's conflict rule is the most surprising. In a split, n1 wrote `z-early` and n2 later wrote `a-later`. After the heal both kept `z-early`, because `z` sorts after `a`. That is fine for a CRDT and wrong for "the last setting wins".

B's two surprises were real bugs in B that only showed up with 1,000 distinct values. iroh-docs awaits every event send, so a subscriber that queries docs while it handles an event deadlocks the engine (iroh-docs-0.101.0/src/engine/live.rs:871-881, 957-959, sync.rs:144-152). A blob download that fails goes into `missing_hashes` and is retried only when a neighbor gossips that hash again (live.rs:661, 735).

### For apps that sync more than machines

imesh is a base for other projects, so the engine also decides what apps get.

| Need | A | B | C |
|---|---|---|---|
| Small shared settings | `app/<key>` records | entries in the cluster document or an app document | rows in an app table |
| Queries | prefix scan | prefix scan | SQL with indexes and joins |
| Large values | not suitable; every machine keeps every value in memory | blobs: content-addressed, verified, resumable, deduplicated | 8 KiB change chunks; not meant for large values |
| Many keys | the summary costs about 60 bytes per key per exchange; fine to about 10,000 keys | range reconciliation; cost grows with the difference | version ranges; scales well |
| Transactions | one record at a time | one entry at a time | one SQL transaction is one replicated change |
| Schema changes | none needed | none needed | add only; a machine on an older schema rejects whole transactions that touch a new table |
| Isolation between apps | key prefix | one namespace per app | one table per app, one database |

C gives apps the most, as SQL, and costs the most: a 2.1x binary, 3x RSS, an alpha C dependency built with a 2023 nightly toolchain, and Corrosion patches for Windows. B gives apps the best tool for large data (blobs). But in this prototype it adds a shared secret, it has two engine gotchas, and the core then depends on three 0.x crates. A per-machine-namespace layout would remove B's shared secret at the cost of one gossip topic per machine. A gives apps the least. It wins on footprint, platform reach, build simplicity, and a conflict rule that follows time, and it meets every imesh rule without extra work: iroh-only auth, no root, one binary on Linux and Windows.

### Recommendation at the time

The user later chose Corrosion instead; see [Decision](#decision-corrosion).

1. **Use A for the imesh core:** membership, machine records, cluster settings, and small app settings. It is the smallest, it runs everywhere the requirements name, it holds no secrets, and its conflict rule follows time.
2. **Let apps bring their own engine on their own ALPN.** An app that moves files uses iroh-blobs. An app that needs a large replicated key space uses iroh-docs with one namespace per app, and handles the two gotchas above. The admission hook guards those ALPNs the same way, with no extra work.
3. **Do not use Corrosion in the core.** Uncloud's choice fits Uncloud, a Linux-only container platform with a Docker daemon beside it. In imesh, its size, build chain, Windows gap, and non-time conflict rule cost more than SQL gives.
4. **Fix A's remaining limits before they bite:**
   - Replace the full summary with a range or hash summary before state passes about 10,000 keys.
   - Collect tombstones.
   - Tune the ping interval if faster down detection matters.
