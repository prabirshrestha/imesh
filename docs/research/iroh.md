# Research notes: iroh building blocks

Read 2026-09-23 from crate sources (iroh 1.2.0, iroh-relay 1.2.0, iroh-gossip 0.101.0, iroh-docs 0.101.0,
iroh-blobs 0.103.0, irpc 0.17.0), https://docs.iroh.computer, and a footprint test. Paths are in ~/.cargo/registry/src/*/.

## Endpoint
- Presets: `Minimal` sets only the crypto provider. `N0` adds n0 pkarr publish, n0 DNS lookup, and n0 relays (iroh-1.2.0/src/endpoint/presets.rs:59,113). `Empty` fails at bind because it has no crypto provider.
- The docs "airgapped" example uses `presets::Empty`, which fails at bind. Use `Minimal`.
- `RelayMode::Disabled | Default | Staging | Custom(RelayMap)`. `RelayMode::custom(urls)` (endpoint.rs:2023). Relays can be added and removed at runtime (endpoint.rs:992,1006).
- Address lookup services add up. `MemoryLookup` holds static entries (address_lookup/memory.rs). mDNS is in `iroh-mdns-address-lookup` 0.5, and the DHT is in `iroh-mainline-address-lookup` 0.5.
- `connect(EndpointAddr)` needs direct IPs, a relay URL, or a lookup hit.
- Identity: `SecretKey` (Ed25519). `EndpointId = PublicKey`, which prints as hex and parses from hex or base32.

## Router and admission
- `Router::builder(ep).accept(alpn, handler)...spawn()` (protocol.rs:484,501). `spawn` calls `set_alpns` with every registered ALPN. You cannot add an ALPN after spawn.
- `EndpointHooks::after_handshake(&Connection)` runs for incoming and outgoing connections, after TLS and before any handler. On Reject, iroh closes the connection with your code (endpoint/hooks.rs:102). POC p1 confirmed this.
- Do not store `Endpoint` or a strong `Connection` in a hook.
- Do not use 0-RTT handlers on gated ALPNs. They get streams before the hook runs.

## Connectivity
- `conn.paths()` lists paths. Each `Path` has `is_selected`, `is_ip`, `is_relay`, `rtt`, and `stats` (socket/remote_map/remote_state/path_watcher.rs:455-494). The selected path shows whether the connection is direct or relayed.
- `endpoint.home_relay_status()` and `endpoint.online()` report relay state.

## Self-hosted relay
- The `iroh-relay` crate with the `server` feature can run a relay in process or as its own binary (`n0computer/iroh-relay` Docker image).
- TLS can be manual or LetsEncrypt. QUIC address discovery uses UDP 7842.
- `AccessControl::on_connect` sees the proven endpoint id and a bearer token. The binary has everyone, allowlist, denylist, http, and shared_token presets (iroh-relay-1.2.0/src/server.rs:285-372, main.rs:160-310).
- Relay traffic is HTTPS upgraded to WebSocket on TCP 443. It works through HTTP CONNECT proxies (`proxy_url`, `proxy_from_env`). It works when UDP is blocked, with everything relayed.

## Sync options
- iroh-gossip: HyParView plus PlumTree per 32-byte topic. The default maximum message size is 4096 B. No persistence. Slow receivers lose messages (Lagged).
- iroh-docs: signed entries per (namespace, author, key), LWW by timestamp. Remote timestamps may be at most 10 min in the future. Range-based set reconciliation. Stored in redb. Needs iroh-blobs and iroh-gossip. There is no periodic resync timer; sync runs only on join, new neighbor, report, or resync.
- The namespace write capability is one shared secret, or one secret per writer with a namespace per machine.
- iroh-willow 0.0.1 is abandoned. There is no maintained iroh CRDT crate. The automerge integration is an example repo only.

## RPC
- irpc 0.17 remote transport is noq (QUIC) streams only. There is no unix socket or byte-stream transport (irpc-0.17.0/src/lib.rs:2004-2012).
- irpc-iroh serves an irpc service on an ALPN.
- quic-rpc 0.20 is pinned to iroh 0.35. Do not use it.

## Forwarding
- iroh has no multi-hop proxy. Build it: accept a stream, open a stream to the target, and relay bytes. dumbpipe 0.39 and iroh-proxy-utils 0.3 show TCP and unix socket forwarding patterns.

## Footprint (measured by the research agent, release, thin LTO, strip)
| Target | iroh + Router | + gossip + docs + blobs |
|---|---|---|
| aarch64-unknown-linux-musl | 12.3 MB | 18.1 MB |
| x86_64-unknown-linux-musl | 13.8 MB | 20.6 MB |
Idle RSS on macOS arm64: 19.8 MB bare, 26.8 MB full.

## Platforms
- The default crypto is ring. `cargo zigbuild` builds static musl binaries for x86_64 and aarch64 with no extra setup.
- The portmapper (UPnP) is on by default. `PortmapperConfig::Disabled` turns it off.
