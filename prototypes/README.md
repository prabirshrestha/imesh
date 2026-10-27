# Prototypes

These are evaluation work, kept so the comparison in [docs/comparison.md](../docs/comparison.md) can be run again. The design that carries on is [`crates/imesh`](../crates/imesh). It now uses Corrosion, the engine of prototype C, and prototype C's own copy was removed.

- [`imesh-docs`](imesh-docs) (B): state on iroh-docs, with iroh-gossip and iroh-blobs. A snapshot from before the reviews.
- [`imesh-records`](imesh-records) (A): the former engine of `crates/imesh`, a small record map with last-writer-wins by hybrid logical clock over the ALPN `imesh/sync/1`. A snapshot from just before the switch to Corrosion, with all review fixes. It is kept to compare the two engines: 19.2 MB binary, 21 to 33 MB RSS, convergence 3 to 4 s after a 45 s split.
- [`router`](router): iroh `Router`, an admission hook, and iroh-gossip in one process. The first proof that the building blocks fit.

Each is its own Cargo package with its own lock file, so their dependency versions do not affect `crates/imesh`. Run the Linux proof against one with:

```bash
CRATE=prototypes/imesh-docs PREFIX=b PORTS=24 tests/e2e/linux.sh
```

Stages 21 and 21b need `imesh schema` and `imesh sql`, which only `crates/imesh` has. So a run against `imesh-records` fails at stage 21, after stages 1 to 20 pass, and prints no footprint. `imesh-docs` has no stage 20 fixes either.
