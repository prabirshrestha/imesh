//! Wakes: the computer woke from sleep, or its network changed. Either can leave every
//! connection that ran before it dead while it still looks open, so a wake makes the endpoint
//! look at its network again, and makes every devtunnel child check its tunnel at once (see
//! `net::devtunnel::supervise`), instead of at its next check.
//!
//! Sleep shows as a jump of the wall clock against the monotonic clock, which stops while the
//! computer sleeps on macOS and Linux. A network change shows as a change of the endpoint's own
//! addresses, which iroh watches.

use std::time::{Duration, Instant, SystemTime};

use iroh::Watcher;
use tracing::info;

use super::Machine;

/// How often the clocks are compared.
const TICK: Duration = Duration::from_secs(5);
/// How far the wall clock may run ahead of the monotonic clock before it counts as sleep. More
/// than a tick late, so a busy runtime or a clock fix by NTP does not count.
const SLEPT: Duration = Duration::from_secs(15);

/// Whether the wall clock moved `wall` while the monotonic clock moved `mono`, by more than
/// [`SLEPT`] past it: the computer slept between the two readings.
pub(crate) fn slept(wall: Duration, mono: Duration) -> bool {
    wall.saturating_sub(mono) > SLEPT
}

impl Machine {
    /// Sends a wake on sleep and on network changes, for as long as the machine runs.
    pub(super) async fn watch_wake(self) {
        let mut addr = self.endpoint.watch_addr();
        let mut last = addr.get();
        let mut mono = Instant::now();
        let mut wall = SystemTime::now();
        loop {
            let why = tokio::select! {
                _ = tokio::time::sleep(TICK) => {
                    let (m, w) = (Instant::now(), SystemTime::now());
                    let jumped = slept(w.duration_since(wall).unwrap_or_default(), m.duration_since(mono));
                    (mono, wall) = (m, w);
                    if !jumped {
                        continue;
                    }
                    "the computer woke from sleep"
                }
                now = addr.updated() => {
                    let Ok(now) = now else { return };
                    // Only the local addresses tell of a network change; a relay coming and going
                    // is a change of the endpoint's own.
                    let ips = |a: &iroh::EndpointAddr| a.ip_addrs().copied().collect::<std::collections::BTreeSet<_>>();
                    let changed = ips(&now) != ips(&last);
                    last = now;
                    if !changed {
                        continue;
                    }
                    "the network changed"
                }
            };
            info!("{why}: checking the network and the tunnels again");
            self.wake.send_modify(|n| *n += 1);
            self.endpoint.network_change().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_wall_clock_far_ahead_is_sleep() {
        let s = Duration::from_secs;
        assert!(!slept(s(5), s(5)));
        assert!(!slept(s(12), s(5)), "a late tick is not sleep");
        assert!(!slept(s(0), s(5)), "a wall clock set back is not sleep");
        assert!(slept(s(600), s(5)));
    }
}
