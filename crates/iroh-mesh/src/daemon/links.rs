//! One link per member: an iroh connection on [`CORRO_ALPN`] that carries Corrosion's gossip
//! (see [`crate::state::tunnel`]), a keepalive every second that decides whether the member is
//! up, and this machine's tables.
//!
//! Each side sends its tables when the link starts and whenever they change, and applies what the
//! other has and it lacks. Gossip passes in either direction only while both have the same tables
//! and columns, so no row reaches a machine that cannot store it (see [`crate::state::schema`]).
//! Gossip that the gate holds back is dropped; to Corrosion that looks like packet loss, and it
//! sends again.
//!
//! Frame: length (4 bytes, big endian, kind and payload), kind (1 byte), payload.

use std::{
    collections::HashMap,
    convert::Infallible,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use bytes::Bytes;
use iroh::{
    EndpointId,
    endpoint::{Connection, RecvStream, SendStream},
    protocol::{AcceptError, ProtocolHandler},
};
use n0_future::task::AbortOnDropHandle;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use super::Machine;
use crate::state::{schema::Tables, slot, tunnel};

/// The ALPN that carries Corrosion's gossip between members.
pub const CORRO_ALPN: &[u8] = b"imesh/corrosion/1";
const KEEPALIVE_FRAME: u8 = 0;
const GOSSIP_FRAME: u8 = 1;
const TABLES_FRAME: u8 = 2;
/// The largest frame: a table list can be larger than a datagram.
const MAX_FRAME: usize = 4 << 20;
/// Keepalives between two sends of this machine's tables when nothing changed.
const TABLES_EVERY: u64 = 10;
/// A member that sent nothing for this long is down.
const LIVENESS: Duration = Duration::from_secs(4);
const KEEPALIVE: Duration = Duration::from_secs(1);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// A failed link tries again after this long, doubling up to `MAX_BACKOFF`.
const BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(4);
/// Gossip frames that wait for one member's link.
const QUEUE: usize = 1024;

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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PeerStatus {
    pub state: PeerState,
    pub path: Option<String>,
    pub rtt_ms: Option<u64>,
    pub last_seen: Option<u64>,
    pub error: Option<String>,
    /// Whether Corrosion's SWIM layer counts the member as alive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swim: Option<bool>,
    /// `ok` when rows pass to and from the member; else why not, for example a schema conflict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// In the devtunnel mesh: `forwarded` while this machine forwards the member's relay tunnel,
    /// `down` while that forward does not run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel: Option<String>,
}

/// The link task to one member, its gossip queue, and what it last saw.
pub(crate) struct Peer {
    link: AbortOnDropHandle<()>,
    queue: tunnel::Sender,
    conn: Option<Connection>,
    last_rx: Option<Instant>,
    last_seen: Option<u64>,
    error: Option<String>,
    /// The member's tables, as it last sent them.
    tables: Option<Arc<Tables>>,
    /// Why this machine could not take the member's tables.
    schema_error: Option<String>,
    /// Set while this machine applies the member's tables.
    catching_up: bool,
}

impl Peer {
    fn new(link: AbortOnDropHandle<()>, queue: tunnel::Sender) -> Self {
        Peer {
            link,
            queue,
            conn: None,
            last_rx: None,
            last_seen: None,
            error: None,
            tables: None,
            schema_error: None,
            catching_up: false,
        }
    }

    /// Whether a frame came from the member recently.
    fn is_up(&self) -> bool {
        self.last_rx.is_some_and(|t| t.elapsed() < LIVENESS)
    }

    fn state(&self) -> PeerState {
        match (self.is_up(), self.last_rx) {
            (true, _) => PeerState::Up,
            (false, Some(_)) => PeerState::Down,
            (false, None) if self.error.is_some() => PeerState::Down,
            (false, None) => PeerState::Unknown,
        }
    }
}

impl Machine {
    /// One link per member and per peer of a join in progress. The tunnel gets each link's queue.
    pub(crate) fn reconcile_links(&self) {
        let mut want = self.members();
        want.extend(self.admission.bootstrap());
        want.remove(&self.id());
        let mut peers = self.peers.lock().unwrap();
        if self.stopped.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        peers.retain(|id, p| want.contains(id) && !p.link.is_finished());
        self.incoming.lock().unwrap().retain(|id, _| want.contains(id));
        for id in want {
            peers.entry(id).or_insert_with(|| {
                let (queue, rx) = mpsc::channel(QUEUE);
                Peer::new(AbortOnDropHandle::new(tokio::spawn(self.clone().link(id, rx))), queue)
            });
        }
        // Two keys can share a gossip slot. The first one keeps it; the others carry no rows,
        // and their status says why. A collision is never a silent overwrite.
        let base = self.store.gossip_base;
        let mine = slot(base, &self.id());
        let mut queues: HashMap<u16, tunnel::Sender> = HashMap::new();
        for (id, p) in peers.iter_mut() {
            let s = slot(base, id);
            if s == mine || queues.contains_key(&s) {
                p.error =
                    Some(format!("gossip port {s} collides with another member; reset this machine for a new key"));
                continue;
            }
            queues.insert(s, p.queue.clone());
        }
        self.tunnel.set_peers(queues);
    }

    fn set_peer(&self, id: EndpointId, f: impl FnOnce(&mut Peer)) {
        if let Some(p) = self.peers.lock().unwrap().get_mut(&id) {
            f(p);
        }
    }

    /// Whether gossip may pass to and from the member now.
    fn rows_may_pass(&self, id: &EndpointId) -> bool {
        let theirs = self.peers.lock().unwrap().get(id).and_then(|p| p.tables.clone());
        theirs.is_some_and(|t| self.store.rows_may_pass(&t))
    }

    pub(super) fn link_up(&self, id: &EndpointId) -> bool {
        self.peers.lock().unwrap().get(id).is_some_and(Peer::is_up)
    }

    /// The member's own connection to this machine, if it is open.
    fn live_incoming(&self, peer: &EndpointId) -> Option<Connection> {
        self.incoming.lock().unwrap().get(peer).filter(|c| c.close_reason().is_none()).cloned()
    }

    /// Forgets the member's connection to this machine if it is `conn`, and not a newer one.
    fn forget_incoming(&self, peer: &EndpointId, conn: &Connection) {
        let mut incoming = self.incoming.lock().unwrap();
        if incoming.get(peer).is_some_and(|c| c.stable_id() == conn.stable_id()) {
            incoming.remove(peer);
        }
    }

    /// How this machine last saw member `id`.
    pub fn peer_status(&self, id: &EndpointId) -> PeerStatus {
        if *id == self.id() {
            return PeerStatus { state: PeerState::Up, path: Some("self".into()), ..Default::default() };
        }
        let swim = Some(self.store.swim_alive(id));
        let tunnel = self.mesh.status(id).map(|(_, ready)| if ready { "forwarded" } else { "down" }.to_string());
        let peers = self.peers.lock().unwrap();
        let Some(p) = peers.get(id) else {
            return PeerStatus { swim, tunnel, ..Default::default() };
        };
        let schema = Some(match (&p.schema_error, &p.tables) {
            (Some(e), _) => e.clone(),
            (None, None) => "waiting for the member's tables".into(),
            (None, Some(t)) if self.store.rows_may_pass(t) => "ok".into(),
            (None, Some(_)) => "waiting for the member to take this machine's tables".into(),
        });
        let selected = p.conn.as_ref().filter(|_| p.is_up()).and_then(|c| {
            let paths = c.paths();
            paths.iter().find(|p| p.is_selected()).map(|p| (p.is_relay(), p.rtt()))
        });
        PeerStatus {
            state: p.state(),
            path: selected.map(|(relay, _)| if relay { "relay" } else { "direct" }.to_string()),
            rtt_ms: selected.map(|(_, rtt)| rtt.as_millis() as u64),
            last_seen: p.last_seen,
            error: p.error.clone(),
            swim,
            schema,
            tunnel,
        }
    }

    /// Keeps a link to one member. Both members dial each other, so a pair stays linked even
    /// when only one side can reach the other.
    async fn link(self, peer: EndpointId, mut rx: mpsc::Receiver<Bytes>) {
        let mut backoff = BACKOFF;
        loop {
            let Err(e) = self.link_once(peer, &mut rx).await;
            if self.link_up(&peer) {
                backoff = BACKOFF;
            }
            debug!(peer = %peer.fmt_short(), "link: {e:#}");
            let mut dead = None;
            self.set_peer(peer, |s| {
                dead = s.conn.take();
                s.error = Some(format!("{e:#}"));
            });
            // Close the failed connection, and forget it if the member opened it, so the next try
            // dials instead of picking it again until iroh's idle timeout.
            if let Some(c) = dead {
                c.close(0u32.into(), b"link failed");
                self.forget_incoming(&peer, &c);
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    /// A connection to the member: the member's own, or ours. A new member has no address in the
    /// cluster state yet, so our dial may not get through while its own dial to us already has.
    /// Whichever connection comes first carries the link.
    async fn first_connection(&self, peer: EndpointId) -> Result<Connection> {
        let dial = async {
            let c = tokio::time::timeout(CONNECT_TIMEOUT, self.endpoint.connect(self.peer_addr(peer), CORRO_ALPN))
                .await
                .with_context(|| format!("no connection within {CONNECT_TIMEOUT:?}"))??;
            tokio::spawn(self.clone().read_conn(peer, c.clone()));
            anyhow::Ok(c)
        };
        let incoming = async {
            loop {
                if let Some(c) = self.live_incoming(&peer) {
                    return c;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        };
        tokio::select! {
            biased;
            c = incoming => Ok(c),
            c = dial => c,
        }
    }

    /// Sends this machine's tables, the member's queued gossip, and a keepalive every second.
    async fn link_once(&self, peer: EndpointId, rx: &mut mpsc::Receiver<Bytes>) -> Result<Infallible> {
        let conn = self.first_connection(peer).await?;
        let mut send = conn.open_uni().await?;
        self.set_peer(peer, |s| s.conn = Some(conn.clone()));
        let mut tables = self.store.watch_tables();
        let mine = tables.borrow_and_update().clone();
        send_tables(&mut send, &mine).await?;
        let started = Instant::now();
        let mut no_path_since: Option<Instant> = None;
        let mut tick = tokio::time::interval(KEEPALIVE);
        let mut ticks = 0u64;
        loop {
            tokio::select! {
                f = rx.recv() => {
                    let f = f.context("link closed")?;
                    if self.rows_may_pass(&peer) {
                        write_frame(&mut send, GOSSIP_FRAME, &f).await?;
                    }
                }
                _ = tables.changed() => {
                    let mine = tables.borrow_and_update().clone();
                    send_tables(&mut send, &mine).await?;
                }
                _ = tick.tick() => {
                    write_frame(&mut send, KEEPALIVE_FRAME, &[]).await?;
                    // Now and then send the tables again: a late frame from an older connection
                    // may have left the member with an old copy.
                    ticks += 1;
                    if ticks.is_multiple_of(TABLES_EVERY) {
                        let mine = tables.borrow().clone();
                        send_tables(&mut send, &mine).await?;
                    }
                    // Try the member's tables again: an apply can fail for a passing reason.
                    if !self.rows_may_pass(&peer) {
                        self.retry_tables(peer);
                    }
                    // A restarted member dials us on a new connection while ours still looks open.
                    if self.live_incoming(&peer).is_some_and(|c| c.stable_id() != conn.stable_id()) {
                        return Err(anyhow!("member opened a newer connection"));
                    }
                    // iroh selects a path per member, not per connection, and changes it only
                    // when a path opens or closes. When a restarted member comes back on a new
                    // address, iroh can keep the dead address of a closed connection selected, so
                    // this connection has no selected path and every new dial to the member goes
                    // to the dead address. iroh forgets the selection when the member has no open
                    // connection, so close this one and dial again.
                    // A path can be unselected for a moment while iroh switches paths, so only a
                    // connection with none for LIVENESS on end counts.
                    if conn.paths().iter().any(|p| p.is_selected()) {
                        no_path_since = None;
                    } else if no_path_since.get_or_insert_with(Instant::now).elapsed() > LIVENESS {
                        bail!("iroh selected no path to the member on this connection for {LIVENESS:?}");
                    }
                    // Writes into a dead connection succeed until its buffers fill, so silence decides.
                    if started.elapsed() > 2 * LIVENESS && !self.link_up(&peer) {
                        bail!("no frames from member for {LIVENESS:?}");
                    }
                }
                _ = conn.closed() => bail!("connection closed"),
            }
        }
    }

    /// Reads frames from every stream the member opens on this connection.
    async fn read_conn(self, peer: EndpointId, conn: Connection) {
        while let Ok(mut recv) = conn.accept_uni().await {
            let m = self.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                loop {
                    if let Err(e) = read_frame(&mut recv, &mut buf).await {
                        debug!(peer = %peer.fmt_short(), "link stream: {e:#}");
                        break;
                    }
                    m.set_peer(peer, |s| {
                        s.last_rx = Some(Instant::now());
                        s.last_seen = Some(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs());
                        s.error = None;
                    });
                    match buf[0] {
                        GOSSIP_FRAME if m.rows_may_pass(&peer) => {
                            if let Err(e) = m.tunnel.deliver(slot(m.store.gossip_base, &peer), &buf[1..]).await {
                                debug!(peer = %peer.fmt_short(), "deliver: {e:#}");
                            }
                        }
                        TABLES_FRAME => match serde_json::from_slice::<Tables>(&buf[1..]) {
                            Ok(theirs) => m.take_tables(peer, Arc::new(theirs)),
                            Err(e) => debug!(peer = %peer.fmt_short(), "bad tables frame: {e}"),
                        },
                        _ => {}
                    }
                }
            });
        }
    }

    /// Records a member's tables and applies what this machine lacks. Applying changes this
    /// machine's tables, which the link then sends back, so both sides end up the same.
    fn take_tables(&self, peer: EndpointId, theirs: Arc<Tables>) {
        self.set_peer(peer, |s| s.tables = Some(theirs));
        self.retry_tables(peer);
    }

    /// Starts a catch-up with the member's last tables in its own task, unless one runs. A
    /// catch-up can take seconds on a large table; the reader keeps reading meanwhile.
    fn retry_tables(&self, peer: EndpointId) {
        let theirs = {
            let mut peers = self.peers.lock().unwrap();
            let Some(p) = peers.get_mut(&peer).filter(|p| !p.catching_up) else { return };
            let Some(t) = p.tables.clone() else { return };
            p.catching_up = true;
            t
        };
        let m = self.clone();
        tokio::spawn(async move { m.catch_up(peer, &theirs).await });
    }

    async fn catch_up(&self, peer: EndpointId, theirs: &Tables) {
        let error = self.store.catch_up(theirs).await.err().map(|e| format!("{e:#}"));
        self.set_peer(peer, |s| {
            if error.is_some() && s.schema_error != error {
                warn!(peer = %peer.fmt_short(), "cannot take the member's tables: {}", error.as_deref().unwrap_or_default());
            }
            s.schema_error = error;
            s.catching_up = false;
        });
    }
}

/// Reads one frame into `buf`: the kind, then the payload.
async fn read_frame(recv: &mut RecvStream, buf: &mut Vec<u8>) -> Result<()> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await?;
    let n = u32::from_be_bytes(len) as usize;
    ensure!(n > 0 && n <= MAX_FRAME, "bad frame length {n}");
    buf.resize(n, 0);
    recv.read_exact(buf).await?;
    Ok(())
}

async fn send_tables(send: &mut SendStream, tables: &Tables) -> Result<()> {
    write_frame(send, TABLES_FRAME, &serde_json::to_vec(tables)?).await
}

/// Writes one frame.
async fn write_frame(send: &mut SendStream, kind: u8, payload: &[u8]) -> Result<()> {
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.extend_from_slice(&(1 + payload.len() as u32).to_be_bytes());
    buf.push(kind);
    buf.extend_from_slice(payload);
    tokio::time::timeout(WRITE_TIMEOUT, send.write_all(&buf))
        .await
        .with_context(|| format!("member took no data for {WRITE_TIMEOUT:?}"))??;
    Ok(())
}

/// Accepts a member's link connection and reads its frames.
#[derive(Debug, Clone)]
pub(super) struct CorroProtocol(pub(super) Machine);

impl ProtocolHandler for CorroProtocol {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let peer = conn.remote_id();
        self.0.incoming.lock().unwrap().insert(peer, conn.clone());
        self.0.clone().read_conn(peer, conn.clone()).await;
        self.0.forget_incoming(&peer, &conn);
        Ok(())
    }
}
