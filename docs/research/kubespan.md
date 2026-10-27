# Research notes: Talos KubeSpan and Discovery

Sources, read 2026-09-23: https://docs.siderolabs.com/talos/v1.14/networking/kubespan,
https://docs.siderolabs.com/talos/v1.14/configure-your-talos-cluster/system-configuration/discovery,
and shallow clones of siderolabs/discovery-service, discovery-api, discovery-client, and talos (HEAD).
Paths below are relative to those repos. `talos/` means `talos/internal/app/machined/pkg/`.

## Discovery

- Two registries. The service registry is the default (`https://discovery.talos.dev/`, gRPC over TCP 443). The Kubernetes registry stores data in Node annotations. It is deprecated and off by default.
- Several `DiscoveryServiceConfig` endpoints can be set. Nodes publish to all and read from all (`controllers/cluster/discovery_service.go:272-278`).
- The cluster ID only partitions server state. The cluster secret is the AES key (`controllers/cluster/config.go:66-76`).
- Affiliate data is AES-GCM with no associated data (`discovery-client/pkg/client/client.go:160-171`). Endpoints use AES-ECB so the server can deduplicate them (`client.go:126-150`).
- The affiliate record holds node_id (random), addresses, hostname, nodename, KubeSpan public key, overlay address, and extra prefixes (`discovery-api/api/v1alpha1/client/affiliate.proto:10-54`).
- `Hello` returns the client IP as the server sees it. Talos adds it to its endpoint list (`discovery_service.go:447-470`).
- Server limits: TTL 30 min max, data 2048 B, 64 endpoints, 1024 affiliates per cluster, 15 rps per IP (`discovery-service/pkg/limits/limits.go:14-28`).
- A member is an affiliate that has a nodename. There is no approval step (`controllers/cluster/member.go:21`).

## KubeSpan data plane

- WireGuard on UDP 51820. There is no relay. If both sides are behind firewalls, the connection can fail.
- Every 30 s the manager moves a down peer to its next endpoint, round-robin (`adapters/kubespan/peer_status.go:118-153`, `controllers/kubespan/manager.go:38`).
- Peer state is unknown, down, or up. The timers are 15 s and 275 s (`peer_status.go:32-95`).
- `talosctl get kubespanpeerstatuses` shows endpoint, state, rx, and tx per peer.
- harvestExtraEndpoints publishes "I reached X at A:P" on behalf of X. It is off by default, and the docs say not to enable it above 50 peers.
- allowDownPeerBypass=false (the default) black-holes traffic to a down peer instead of sending it in clear text.

## Security and outages

- A member proves only that it holds the shared cluster secret. There is no per-node revocation. To revoke, you must rotate the secret on every node.
- The server does not authenticate AffiliateUpdate or AffiliateDelete beyond the ID format. INFERRED: anyone who knows the cluster ID can delete affiliates.
- If a node reboots while the discovery service is down, it cannot find peers. If an outage lasts longer than the 30 min TTL, all records expire and nodes drop their peers.

## Lessons for imesh

1. Use the iroh EndpointId as the only machine ID.
2. Do not use a shared cluster secret for membership. Use signed per-member records, enforced in the iroh accept hook.
3. Keep "where is X" (iroh address lookup) apart from "who is a member" (replicated among members).
4. Persist the peer set and the last known addresses. A reboot must not depend on any lookup service.
5. Remove a peer only on a signed removal, never because a remote answer is empty or expired.
6. Let iroh choose paths. Do not port endpoint rotation or reflexive-IP logic.
7. Give each peer a status row: state, path (direct or relay), RTT, last seen.
8. Fail closed. Never send app traffic to a peer that is not a member.
