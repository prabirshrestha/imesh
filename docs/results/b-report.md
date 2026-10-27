# Prototype B report: iroh-docs + iroh-gossip

Written by the agent that built `poc/imesh-docs`, 2026-09-23. It is condensed here and checked by the rerun in [e2e-b.txt](e2e-b.txt).

## Shape
- One iroh-docs namespace per cluster. `cluster.init` creates it. `cluster.add` returns `{cluster, founder, secret, peers}`, and the CLI carries the namespace secret over SSH into `cluster.join`.
- The founder is pinned locally in `<data>/cluster.json`, because any secret holder can write a `cluster` entry.
- Author = machine key (`Author::from(secret_key)`).
- Keys get a trailing NUL. iroh-docs deletes older same-author entries whose key has the new key as a prefix (iroh-docs-0.101.0/src/store/ranger.rs:554-579).
- POC A's rules apply at read time. Bad entries are stored and synced, but ignored.
- Liveness: a per-member link on `imesh/live/0` pings every 5 s with a 10 s timeout, and calls `start_sync([peer])` on connect and every 30 s. iroh-docs has no resync timer (engine/live.rs:245-281).
- The hook guards the blobs, gossip, and docs ALPNs too. `enforce()` closes the open connections of removed peers, because docs and gossip keep connections open after the handshake.

## Numbers (agent runs)
- Binary 23.0 MB (A: 15.6 MB). RSS 27-34 MB with 34-35 threads (A: 19-31 MB, 17-18 threads). iroh-blobs FsStore starts its own tokio runtime (iroh-blobs-0.103.0/src/store/fs.rs:1400).
- Down after 10-12 s. The 45 s split converged 4 s after the heal.
- Stage 17: write 0.3 s, all on n2 after 0.4 s. This flatters B: the 1,000 values are identical, so they share one blob. 200 distinct 600-byte values were all readable on n2 0.1 s after the write loop ended.

## Guarantees versus A
- Every holder of the namespace secret can insert anything. Rules are read-time only.
- A removed machine keeps the secret. The hook blocks its connections. The "ignore entries written after removal" rule uses author-chosen timestamps, so a backdated entry passes, including a backdated `member/<stranger>`. The real fix is to rotate the namespace on every removal.
- Timestamps are plain system microseconds, not a hybrid logical clock. A slow clock loses its newer writes. Entries more than 10 min in the future are rejected (sync.rs:48, 640).
- Every value is a blob. A read returns null until the blob download completes.

## For app data
- Give each app its own namespace. The admission hook and the author identity are shared.
- Large values are content-addressed, verified, resumable, and deduplicated. This is B's main strength.
- Range reconciliation scales with the difference, not the total.
- Storage grows with keys times writers, because there is one entry per (author, key).

## Upstream surprises
- `Docs::persistent(path)` does not create its directory.
- Docs dials peers by ID only (engine/live.rs:376), so addresses must come from a lookup.
- Warning noise on concurrent syncs (net.rs:243).
