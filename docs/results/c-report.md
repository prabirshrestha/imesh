# Prototype C report: iroh + Corrosion

Written by the agent that built `poc/imesh-corrosion`, 2026-09-23. Condensed here. The numbers come from its two e2e runs. Its copy of the script expected the SSH-user socket owner in stage 3, which the shared script now also expects.

## Shape
- Corrosion (superfly/corrosion ef243dd) is embedded as a library in the one static musl binary. No Corrosion patches.
- Corrosion loads cr-sqlite with `dlopen` of a prebuilt glibc `.so` (corro-types/src/sqlite.rs:349-364). A static musl binary cannot do that. The prototype builds cr-sqlite 3133cac from source as a static object (nightly-2023-10-05, libclang 20 or lower, zig), and serves it through a SQLite VFS so Corrosion's own `load_extension` call works.
- Gossip bridge: each machine's Corrosion binds a loopback IP derived from its EndpointId (24 bits of the key). The daemon binds one loopback socket per peer and carries the UDP datagrams over one iroh stream per link (ALPN `imesh/corro/0`). Datagrams were not used, because Corrosion's QUIC packets are 1200 bytes or more.
- Tables: cluster, members(id, name, removed), machines(id, info), clients, kv. The admission hook reads a snapshot that is refreshed every 250 ms.
- Tuning: gossip idle timeout 10 s (a restarted peer's old QUIC connection cannot be reset), SQLite cache 64 MiB.

## Numbers (agent runs)
- Binary 39.8 MB (A: 15.6 MB at that time). RSS 66 to 91 MB, 20 threads (A: 19 to 31 MB, 17 to 18 threads).
- Down after 5 to 6 s (1 s keepalive, 5 s silence).
- 45 s split converged 3 s and 14 s after the heal (SWIM rejoin plus Corrosion's sync interval, which grows to 15 s).
- Stage 17: all 1,000 keys on n2 0.4 s after the write began. The fill is one transaction.

## Guarantees versus A
- Rows carry no author. Any member can write any row. Corrosion has no hook to reject a change before it is applied.
- Conflicts do not use time. cr-sqlite compares the column version (how often each side updated the column), then the value, then the site id (crsqlite core/rs/core/src/changes_vtab_write.rs:58-64, 91-108). In a split, two updates on one side beat one later update on the other side. With equal counts, the larger value wins. Stage 9b passes only because "n2-side-later" sorts after "n1-side".
- Removal is a column. A member that writes `removed = 0` twice beats one removal. In practice the hook, the link closing, and SWIM keep the machine out.
- Corrosion's HTTP API (loopback, random port, no auth) and gossip port accept any local process on a member machine. Such a process can write any row and bypass the socket permissions and iroh admission. Corrosion requires an HTTP API address, so this cannot be turned off without a patch.
- Removed members stay in Corrosion's `__corro_members`. Corrosion keeps announcing to them.
- Two machines can collide on the 24-bit loopback address.

## For app data
- Apps add their own tables and query with SQL, with indexes and joins. One write transaction is one replicated version.
- Schema changes may only add. A machine on an older schema rejects a whole transaction that touches an unknown table, including other rows in it, until it upgrades.
- Changes travel in 8 KiB chunks (corro-types/src/change.rs:182). Broadcast is limited to 10 MiB/s per machine (corro-agent/src/broadcast/mod.rs:467-469).
- Sync works by version ranges, so it scales with many keys better than A's full summary.

## Windows
- `cargo check --target x86_64-pc-windows-gnu` fails. `tripwire` uses `tokio::signal::unix` (tripwire/src/tripwire.rs:9, signalstream.rs:6). corro-types has no Windows extension loading ("TODO: support windows", sqlite.rs:349-356). cr-sqlite's Windows build needs `-Zbuild-std`.
- Windows support needs Corrosion patches.
