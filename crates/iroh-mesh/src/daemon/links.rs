//! Roster sync. There are no standing links, so an idle cluster sends nothing.
//!
//! A hello is one connection on [`ROSTER_ALPN`] with one stream: the dialer sends its roster, the
//! other side merges it and answers with its own, and the dialer merges that. A machine says
//! hello to every member when it starts, when its roster changes, and every [`RESYNC`]. When a
//! member does not answer, the machine tries again after [`FIRST_RETRY`], then after twice as
//! long each time, up to [`RESYNC`]. A
//! machine that learns something from a hello changes its roster, so it tells everyone in turn,
//! and a change reaches members that its signer cannot reach. `imesh status` says hello to every
//! member, which also shows whether each one answers.
//!
//! Each side first sends the 32-byte digest of its roster. When the digests match, the hello
//! ends there, so the hellos that follow a change cost a few bytes each. Else each side sends a
//! frame: length (4 bytes, big endian), then the roster as JSON. One hello per member runs at a
//! time in each direction.

use std::{
    collections::BTreeMap,
    sync::atomic::Ordering,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, ensure};
use iroh::{
    EndpointId,
    endpoint::{Connection, ConnectionError, RecvStream, SendStream},
    protocol::{AcceptError, ProtocolHandler},
};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use super::{Machine, admission::REMOVED};
use crate::roster::{MAX_LOG, Roster};

/// The ALPN that carries roster hellos between members.
pub const ROSTER_ALPN: &[u8] = b"imesh/roster/0";
/// How often a machine says hello to every member when nothing changed. A member that missed a
/// change while it was down catches up at the latest then.
const RESYNC: Duration = Duration::from_secs(600);
/// How long a machine waits before it says hello again when a member did not answer. A new member
/// is out of reach until its first hello brings its location, so this must not wait for
/// [`RESYNC`].
const FIRST_RETRY: Duration = Duration::from_secs(5);
/// How long a burst of changes settles before the hellos go out.
const SETTLE: Duration = Duration::from_millis(500);
/// How long one hello may take, connect included.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PeerState {
    #[default]
    Unknown,
    Up,
    Down,
}

impl std::fmt::Display for PeerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PeerState::Unknown => "unknown",
            PeerState::Up => "up",
            PeerState::Down => "down",
        })
    }
}

/// How the last hello with a member went.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PeerStatus {
    pub state: PeerState,
    pub path: Option<String>,
    pub rtt_ms: Option<u64>,
    /// Unix seconds of the last hello that got an answer.
    pub last_seen: Option<u64>,
    pub error: Option<String>,
    /// In the devtunnel mesh: `forwarded` while this machine forwards the member's relay tunnel,
    /// `down` while that forward does not run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel: Option<String>,
    /// The member refused this machine as removed.
    #[serde(skip)]
    pub(crate) refused_as_removed: bool,
}

impl Machine {
    /// Says hello to every other member: now, on every roster change, and every [`RESYNC`]. After
    /// a member did not answer, again with backoff.
    pub(super) async fn sync_loop(self) {
        let mut changes = self.store.subscribe();
        let mut resync = tokio::time::interval(RESYNC);
        let mut retry: Option<Duration> = None;
        loop {
            let retrying = async {
                match retry {
                    Some(d) => tokio::time::sleep(d).await,
                    None => std::future::pending().await,
                }
            };
            let again = tokio::select! {
                r = changes.changed() => if r.is_err() { return } else { false },
                // A cut-off from refusals heals here: if the members still refuse this machine,
                // the hellos set it again.
                _ = resync.tick() => { self.cut_off.store(false, Ordering::Relaxed); false }
                _ = retrying => true,
            };
            tokio::time::sleep(SETTLE).await;
            changes.borrow_and_update();
            let all = self.hello_all().await;
            let missed = all.values().any(|s| s.state != PeerState::Up && !s.refused_as_removed);
            retry = missed.then(|| match retry {
                Some(d) if again => (d * 2).min(RESYNC),
                _ => FIRST_RETRY,
            });
        }
    }

    /// Says hello to every other member at once, and waits for all of them.
    pub(crate) async fn hello_all(&self) -> BTreeMap<EndpointId, PeerStatus> {
        if self.cut_off.load(Ordering::Relaxed) {
            return BTreeMap::new();
        }
        let me = self.id();
        let peers: Vec<EndpointId> = self.members().into_iter().filter(|id| *id != me).collect();
        let hellos = peers.into_iter().map(|id| {
            let m = self.clone();
            async move { (id, m.hello(id).await) }
        });
        let results = futures_util::future::join_all(hellos).await;
        // A member with a hello that did not end has no answer in this round, so the round
        // decides nothing about a cut-off.
        let complete = results.iter().all(|(_, s)| s.is_some());
        let all: BTreeMap<EndpointId, PeerStatus> =
            results.into_iter().map(|(id, s)| (id, s.unwrap_or_else(|| self.peer_status(&id)))).collect();
        // One member's word is not enough: a hostile member could refuse anyone as removed. Stop
        // only when every member that answered refused this machine, and at least one did.
        let answered: Vec<&PeerStatus> =
            all.values().filter(|s| s.state == PeerState::Up || s.refused_as_removed).collect();
        if complete && !answered.is_empty() && answered.iter().all(|s| s.refused_as_removed) {
            warn!("every member that answered refused this machine as removed; it says hello to no one now");
            self.cut_off.store(true, Ordering::Relaxed);
        }
        all
    }

    /// One hello with `peer`. Records how it went and returns that. When a hello to `peer` runs
    /// already, waits for it and returns how it went instead, or nothing when it does not end.
    async fn hello(&self, peer: EndpointId) -> Option<PeerStatus> {
        if self.hellos.lock().unwrap().insert(peer) {
            return Some(self.hello_owned(peer).await);
        }
        let deadline = Instant::now() + HELLO_TIMEOUT;
        while self.hellos.lock().unwrap().contains(&peer) {
            if Instant::now() > deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Some(self.peer_status(&peer))
    }

    /// A hello with `peer` that starts after any hello in flight to it ends, so it carries this
    /// machine's roster as it is now.
    pub(crate) async fn fresh_hello(&self, peer: EndpointId) -> PeerStatus {
        let deadline = Instant::now() + HELLO_TIMEOUT;
        while !self.hellos.lock().unwrap().insert(peer) {
            if Instant::now() > deadline {
                return PeerStatus {
                    state: PeerState::Down,
                    error: Some("a hello to this member did not end".into()),
                    ..Default::default()
                };
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        self.hello_owned(peer).await
    }

    /// The hello itself, once this machine holds the member's single-flight slot.
    async fn hello_owned(&self, peer: EndpointId) -> PeerStatus {
        let started = Instant::now();
        let r = tokio::time::timeout(HELLO_TIMEOUT, self.hello_once(peer))
            .await
            .unwrap_or_else(|_| Err(anyhow!("no answer within {HELLO_TIMEOUT:?}")));
        self.hellos.lock().unwrap().remove(&peer);
        let status = match r {
            Ok(path) => PeerStatus {
                state: PeerState::Up,
                path: Some(path.into()),
                rtt_ms: Some(started.elapsed().as_millis() as u64),
                last_seen: Some(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())),
                error: None,
                tunnel: None,
                refused_as_removed: false,
            },
            Err(e) => {
                debug!(peer = %peer.fmt_short(), "hello: {e:#}");
                let last_seen = self.peers.lock().unwrap().get(&peer).and_then(|p| p.last_seen);
                PeerStatus {
                    state: PeerState::Down,
                    last_seen,
                    error: Some(format!("{e:#}")),
                    refused_as_removed: is_removed_reply(&e),
                    ..Default::default()
                }
            }
        };
        let before = self.peers.lock().unwrap().insert(peer, status.clone()).map(|p| p.state);
        // Say once when a member goes down or comes back, and why.
        match (before, status.state) {
            (Some(PeerState::Up), PeerState::Down) if !status.refused_as_removed => {
                warn!(peer = %peer.fmt_short(), "member is down: {}", status.error.as_deref().unwrap_or(""));
            }
            (Some(PeerState::Down), PeerState::Up) => {
                info!(peer = %peer.fmt_short(), path = status.path.as_deref().unwrap_or(""), "member is up again");
            }
            _ => {}
        }
        status
    }

    async fn hello_once(&self, peer: EndpointId) -> Result<&'static str> {
        let mine = self.store.roster().context("machine is not in a cluster")?;
        let conn = self.dial(self.peer_addr(peer), ROSTER_ALPN).await?;
        let (mut send, mut recv) = conn.open_bi().await?;
        send.write_all(&mine.digest()).await?;
        let mut theirs = [0u8; 32];
        recv.read_exact(&mut theirs).await?;
        if theirs != mine.digest() {
            write_roster(&mut send, &mine).await?;
            let theirs = read_roster(&mut recv).await?;
            if self.store.merge(theirs, None).await? {
                info!(peer = %peer.fmt_short(), "roster updated from a member");
            }
        }
        send.finish()?;
        let path = path_of(&conn);
        conn.close(0u32.into(), b"done");
        Ok(path)
    }

    /// How this machine last saw member `id`, with no new hello.
    pub fn peer_status(&self, id: &EndpointId) -> PeerStatus {
        if *id == self.id() {
            return PeerStatus { state: PeerState::Up, path: Some("self".into()), ..Default::default() };
        }
        let tunnel = self.mesh.status(id).map(|(_, ready)| if ready { "forwarded" } else { "down" }.to_string());
        let mut status = self.peers.lock().unwrap().get(id).cloned().unwrap_or_default();
        status.tunnel = tunnel;
        status
    }
}

/// Whether a member refused this machine because the roster removed it.
fn is_removed_reply(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        let closed =
            c.downcast_ref::<ConnectionError>().or_else(|| match c.downcast_ref::<iroh::endpoint::ConnectError>() {
                Some(iroh::endpoint::ConnectError::Connection { source, .. }) => Some(source),
                _ => None,
            });
        matches!(closed, Some(ConnectionError::ApplicationClosed(a)) if a.error_code == REMOVED.into())
    })
}

fn path_of(conn: &Connection) -> &'static str {
    match conn.paths().iter().find(|p| p.is_selected()) {
        Some(p) if p.is_relay() => "relay",
        Some(_) => "direct",
        None => "unknown",
    }
}

async fn write_roster(send: &mut SendStream, roster: &Roster) -> Result<()> {
    let json = serde_json::to_vec(roster)?;
    ensure!(json.len() <= MAX_LOG, "the roster has more than {MAX_LOG} bytes");
    send.write_all(&(json.len() as u32).to_be_bytes()).await?;
    send.write_all(&json).await?;
    Ok(())
}

async fn read_roster(recv: &mut RecvStream) -> Result<Roster> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await?;
    let n = u32::from_be_bytes(len) as usize;
    ensure!(n <= MAX_LOG, "a roster of {n} bytes is larger than {MAX_LOG}");
    let mut buf = vec![0u8; n];
    recv.read_exact(&mut buf).await?;
    serde_json::from_slice(&buf).context("bad roster")
}

/// Answers a member's hello: merges its roster and sends back this machine's.
#[derive(Debug, Clone)]
pub(super) struct RosterProtocol(pub(super) Machine);

impl ProtocolHandler for RosterProtocol {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let peer = conn.remote_id();
        let m = &self.0;
        if !m.answering.lock().unwrap().insert(peer) {
            conn.close(0u32.into(), b"a hello from this member runs already");
            return Ok(());
        }
        let r = async {
            let (mut send, mut recv) = conn.accept_bi().await?;
            let mut theirs = [0u8; 32];
            recv.read_exact(&mut theirs).await?;
            let mine = m.store.roster().context("machine is not in a cluster")?;
            send.write_all(&mine.digest()).await?;
            if theirs != mine.digest() {
                let theirs = read_roster(&mut recv).await?;
                if m.store.merge(theirs, None).await? {
                    info!(peer = %peer.fmt_short(), "roster updated from a member");
                }
                let mine = m.store.roster().context("machine is not in a cluster")?;
                write_roster(&mut send, &mine).await?;
            }
            send.finish()?;
            // A member that said hello is up. Else this machine keeps its own failed hello, and
            // its backoff, as the last word until the next retry.
            if m.store.read(|s| s.is_member(&peer)) {
                let last_seen = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
                m.peers.lock().unwrap().insert(
                    peer,
                    PeerStatus {
                        state: PeerState::Up,
                        path: Some(path_of(&conn).into()),
                        last_seen: Some(last_seen),
                        ..Default::default()
                    },
                );
            }
            // The dialer closes once it has the answer.
            let _ = tokio::time::timeout(HELLO_TIMEOUT, conn.closed()).await;
            anyhow::Ok(())
        };
        let r = tokio::time::timeout(HELLO_TIMEOUT, r)
            .await
            .unwrap_or_else(|_| Err(anyhow!("the hello did not end within {HELLO_TIMEOUT:?}")));
        m.answering.lock().unwrap().remove(&peer);
        if let Err(e) = r {
            debug!(peer = %peer.fmt_short(), "hello from member: {e:#}");
        }
        Ok(())
    }
}
