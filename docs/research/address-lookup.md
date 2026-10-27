# Research notes: iroh address lookup

Measured 2026-09-23 by a harness in Docker (arm64 musl). The dialer knew only the EndpointId. Each cell is the min and max of 3 runs, in ms. "Move" means the listener stopped, and the same key restarted on a new port on another network. The dialer redialed with a 5 s timeout. The harness lives outside the repo; its numbers are copied here.

Sources: https://docs.iroh.computer/connecting/dns-address-lookup and the sibling pages under /connecting/, iroh-1.2.0/src/address_lookup/*, iroh-mdns-address-lookup 0.5.0, iroh-mainline-address-lookup 0.5.0, iroh-dns-server 1.2.0.

| Approach | First connect | Move | No internet | Binary delta | Idle RSS |
|---|---|---|---|---|---|
| n0 (`presets::N0`) | 417-439 | 1188-1244 | fails | 0 | 14.6-18.3 MB |
| Self-hosted iroh-dns-server, pkarr over HTTP | 4-5 | 224-263 | 4-5 | 0 | 6.0-12.7 MB |
| Self-hosted, resolve over DNS | 4-5 | 31239-31274 (30 s TTL cache) | | 0 | |
| mDNS | 50-677 | never (see below) | endpoint bind fails | +0.99 MB | 8.8-9.0 MB |
| Mainline DHT | 1967-3015 | 6276-13277 | fails | +0.63 MB | 12.4-14.6 MB |
| MemoryLookup from cluster records | 2-3 | 5184-5226 (the dial timeout) | 2-3 | 0 | 5.0-11.8 MB |
| MemoryLookup, 2 s dial timeout | | 2182-2213 | | | |
| MemoryLookup plus a fixed relay, record not updated | | 1220-1263 | | | |
| n0 plus MemoryLookup | | | 2-8 | 0 | |

## What each one needs and publishes
- n0: TCP 443 to dns.iroh.link and the relays. Publishes a pkarr packet, signed by the endpoint key, with only the relay URL (`AddrFilter::relay_only()`, iroh-1.2.0/src/address_lookup/pkarr.rs:166-168). Anyone who knows the id can read it. TTL 30 s, republish every 5 min (pkarr.rs:143-146).
- Self-hosted iroh-dns-server: one HTTP(S) port. `PkarrPublisher::builder(url)` and `PkarrResolver::builder(url)` use plain GET and PUT on `<url>/<z32>` (pkarr.rs:620-650), so no DNS zone is needed. The server checks signatures and keeps records 7 days. PUT has no auth: strangers cannot forge your record but can store their own.
- mDNS: UDP 5353 multicast on one L2 network. Publishes every address, unsigned. TLS still stops impersonation.
- DHT: UDP to the internet. Slow. Publishes nothing extra next to a relay unless the filter is opened, and then it publishes IPs on a public network.
- Cluster records: nothing beyond reaching one member. Only members see them.

## Surprises
- mDNS makes the endpoint fail to bind when the host has no default route ("Cannot bind to IPv4 or IPv6"). It listens only on the default-route interface (swarm-discovery-0.6.3/src/socket.rs:140-150; iroh-mdns-address-lookup-0.5.0/src/lib.rs:497-499).
- The iroh docs show version 0.4 of the mDNS and DHT crates; both are 0.5.
- A running dial does not see new MemoryLookup entries; lookup runs once per dial (socket/remote_map/remote_state.rs:850-880). So after a move, reconnect waits for the dial timeout.
- `MemoryLookup::add_endpoint_info` merges addresses, so old ones pile up. `set_endpoint_info` replaces them (address_lookup/memory.rs:162-200).

## Decision for imesh
- Default: cluster records in `MemoryLookup` (`set_endpoint_info`), plus `presets::N0` as a fallback that is never required. With n0 unreachable, member records still connected in 2-8 ms.
- `--no-n0` with `--relay`: a fixed relay gives about 1.2 s reconnect even with a stale record.
- `--pkarr <url>`: optional self-hosted iroh-dns-server over HTTP. Fastest after a move.
- `--mdns`: opt-in for LAN-only clusters, added after bind, errors tolerated.
- No DHT.
- Redial a down peer at once when its record changes.
