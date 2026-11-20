# Third-party notices

## codync-network

The roster in `crates/iroh-mesh/src/roster.rs` follows the design of
[codync-network](https://github.com/leepokai/codync-network) by leepokai, at commit
`c616482d8335d5792b68ea5532ca395c32a9424f`. That includes the signed change log, the union merge,
and the root, owner, admin and member roles. Parts of the entry and signature code are adapted from
it. imesh changed it: machines instead of people, its own change set, removal that lists the
decisions it counts, and no clock.

codync-network is licensed under the Apache License 2.0. A copy is in
[licenses/codync-network-APACHE-2.0.txt](licenses/codync-network-APACHE-2.0.txt).
