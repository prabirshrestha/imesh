//! One link task per member: pings for liveness, pushes local writes, and runs anti-entropy.

use std::{
    convert::Infallible,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Result, anyhow};
use iroh::{EndpointId, endpoint::Connection};
use n0_future::task::AbortOnDropHandle;
use serde::{Deserialize, Serialize};
use tokio::{sync::broadcast::error::RecvError, task::JoinSet};
use tracing::debug;

use super::Machine;
use crate::state::sync::{self, SYNC_ALPN};

const PING_EVERY: Duration = Duration::from_secs(5);
const PUSH_TIMEOUT: Duration = Duration::from_secs(10);
pub(super) const EXCHANGE_EVERY: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PeerState {
    #[default]
    Unknown,
    Up,
    Down,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PeerStatus {
    pub state: PeerState,
    pub path: Option<String>,
    pub rtt_ms: Option<u64>,
    pub last_seen: Option<u64>,
    pub error: Option<String>,
}

/// The link task to one member, and what it last saw.
pub(crate) struct Peer {
    pub(super) link: AbortOnDropHandle<()>,
    pub(super) status: PeerStatus,
}

impl Machine {
    pub(crate) fn reconcile_links(&self) {
        let members = self.members();
        let me = self.id();
        let mut peers = self.peers.lock().unwrap();
        if self.stopped.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        peers.retain(|id, p| members.contains(id) && !p.link.is_finished());
        for id in members {
            if id != me && !peers.contains_key(&id) {
                let link = AbortOnDropHandle::new(tokio::spawn(self.clone().link(id)));
                peers.insert(id, Peer { link, status: PeerStatus::default() });
            }
        }
    }

    pub(super) fn set_peer(&self, id: EndpointId, f: impl FnOnce(&mut PeerStatus)) {
        if let Some(p) = self.peers.lock().unwrap().get_mut(&id) {
            f(&mut p.status);
        }
    }

    /// How this machine last saw member `id`.
    pub fn peer_status(&self, id: &EndpointId) -> PeerStatus {
        if *id == self.id() {
            return PeerStatus { state: PeerState::Up, path: Some("self".into()), ..Default::default() };
        }
        self.peers.lock().unwrap().get(id).map(|p| p.status.clone()).unwrap_or_default()
    }

    /// Keeps a connection to one member. Both members dial each other, so a pair stays linked
    /// even when only one side can reach the other.
    pub(super) async fn link(self, peer: EndpointId) {
        let mut backoff = Duration::from_secs(1);
        loop {
            let Err(e) = self.link_once(peer).await;
            // A link that was up failed now, not again: retry soon.
            if self.peer_status(&peer).state == PeerState::Up {
                backoff = Duration::from_secs(1);
            }
            debug!(peer = %peer.fmt_short(), "link: {e:#}");
            self.set_peer(peer, |s| {
                s.state = PeerState::Down;
                s.path = None;
                s.error = Some(format!("{e:#}"));
            });
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    }

    /// Pings for liveness, pushes this machine's own writes at once, and runs anti-entropy in
    /// the background, so a slow exchange never marks a peer down.
    pub(super) async fn link_once(&self, peer: EndpointId) -> Result<Infallible> {
        let conn = self.dial(self.peer_addr(peer), SYNC_ALPN).await?;
        let mut changes = self.state.subscribe();
        let mut ping = tokio::time::interval(PING_EVERY);
        let mut anti_entropy = tokio::time::interval(EXCHANGE_EVERY);
        let mut exchange = JoinSet::new();
        let start = |exchange: &mut JoinSet<Result<usize>>| {
            if exchange.is_empty() {
                let (state, conn) = (self.state.clone(), conn.clone());
                exchange.spawn(async move { sync::exchange(&state, &conn).await });
            }
        };
        loop {
            tokio::select! {
                _ = ping.tick() => {
                    tokio::time::timeout(PING_EVERY, sync::ping(&conn))
                        .await
                        .map_err(|_| anyhow!("peer did not answer a ping within {PING_EVERY:?}"))??;
                    self.observe(peer, &conn);
                }
                _ = anti_entropy.tick() => start(&mut exchange),
                Some(done) = exchange.join_next() => match done {
                    Ok(Ok(n)) if n > 0 => debug!(peer = %peer.fmt_short(), applied = n, "anti-entropy"),
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => debug!(peer = %peer.fmt_short(), "anti-entropy: {e:#}"),
                    Err(e) => debug!(peer = %peer.fmt_short(), "anti-entropy task: {e}"),
                },
                change = changes.recv() => match change {
                    Ok(first) => {
                        let records: Vec<_> = std::iter::once(first)
                            .chain(std::iter::from_fn(|| changes.try_recv().ok()))
                            .filter(|c| c.local)
                            .map(|c| c.record)
                            .collect();
                        if !records.is_empty() {
                            // A push that flow control holds up must not stop the pings.
                            tokio::time::timeout(PUSH_TIMEOUT, sync::push(&conn, records))
                                .await
                                .map_err(|_| anyhow!("push took over {PUSH_TIMEOUT:?}"))??;
                        }
                    }
                    Err(RecvError::Lagged(_)) => start(&mut exchange),
                    Err(RecvError::Closed) => return Err(anyhow!("state closed")),
                },
                _ = conn.closed() => return Err(anyhow!("connection closed")),
            }
        }
    }

    pub(super) fn observe(&self, peer: EndpointId, conn: &Connection) {
        let paths = conn.paths();
        let selected = paths.iter().find(|p| p.is_selected());
        let path = selected.as_ref().map(|p| if p.is_relay() { "relay" } else { "direct" }.to_string());
        let rtt = selected.as_ref().map(|p| p.rtt().as_millis() as u64);
        self.set_peer(peer, |s| {
            s.state = PeerState::Up;
            s.path = path;
            s.rtt_ms = rtt;
            s.last_seen = Some(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs());
            s.error = None;
        });
    }
}
