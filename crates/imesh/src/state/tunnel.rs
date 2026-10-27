//! Carries Corrosion's UDP gossip between machines over iroh.
//!
//! Every machine M runs Corrosion on `lo(M):GOSSIP_PORT` (see `store::lo`). For every other member P,
//! the daemon on M binds `lo(P):GOSSIP_PORT`, so M's Corrosion reaches P there. A datagram from
//! `src` to `dst` becomes a frame that the peer link sends to P. The daemon on P sends the payload
//! to `dst` from a socket bound at `src`, which it binds on first use. Both Corrosions see the
//! same addresses, and replies take the same way back.
//!
//! A member can only reach this machine's Corrosion: a frame must come from the member's own
//! address and go to Corrosion's gossip port, or to a port that Corrosion recently sent from (its
//! outgoing QUIC connections). Sockets bound for a member are few and short-lived.
//!
//! Frame: src ip (4), src port (2), dst ip (4), dst port (2), payload.

use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

use anyhow::{Result, ensure};
use bytes::{BufMut, Bytes, BytesMut};
use tokio::{net::UdpSocket, sync::mpsc, task::JoinHandle};
use tracing::{debug, warn};

use super::GOSSIP_PORT;

const ADDR_LEN: usize = 6;
const HEADER: usize = 2 * ADDR_LEN;
/// Sockets bound on first use, and ports our Corrosion sent from, are forgotten after this long
/// without traffic.
const IDLE: Duration = Duration::from_secs(120);
/// The most sockets bound for one member's source ports. The least recently used goes first.
const SOCKETS_PER_MEMBER: usize = 32;

pub type Sender = mpsc::Sender<Bytes>;

struct Sock {
    socket: Arc<UdpSocket>,
    task: JoinHandle<()>,
    last: Arc<Mutex<Instant>>,
}

impl Drop for Sock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Clone)]
pub struct Tunnel(Arc<Inner>);

struct Inner {
    /// Where our own Corrosion listens and sends from.
    me: Ipv4Addr,
    /// Link queue for each peer, by the peer's loopback address.
    peers: Mutex<HashMap<Ipv4Addr, Sender>>,
    sockets: Mutex<HashMap<SocketAddrV4, Sock>>,
    /// Ports our Corrosion sent from, and when. Members may answer to these.
    sent_from: Mutex<HashMap<u16, Instant>>,
}

impl Tunnel {
    pub fn new(me: Ipv4Addr) -> Self {
        Tunnel(Arc::new(Inner {
            me,
            peers: Default::default(),
            sockets: Default::default(),
            sent_from: Default::default(),
        }))
    }

    /// Sets the peers. Binds a gossip listener for each new one and closes sockets of old ones.
    pub fn set_peers(&self, peers: HashMap<Ipv4Addr, Sender>) {
        let mut socks = self.0.sockets.lock().unwrap();
        socks.retain(|a, _| peers.contains_key(a.ip()));
        for ip in peers.keys() {
            let a = SocketAddrV4::new(*ip, GOSSIP_PORT);
            if let std::collections::hash_map::Entry::Vacant(slot) = socks.entry(a) {
                match self.bind(a) {
                    Ok(s) => {
                        slot.insert(s);
                    }
                    // Without this socket, our Corrosion cannot reach the member.
                    Err(e) => warn!("bind gossip address of a member {a}: {e:#}"),
                }
            }
        }
        *self.0.peers.lock().unwrap() = peers;
    }

    /// Closes every socket and forgets every peer. The machine is stopping.
    pub fn close(&self) {
        self.0.sockets.lock().unwrap().clear();
        self.0.peers.lock().unwrap().clear();
    }

    fn bind(&self, addr: SocketAddrV4) -> Result<Sock> {
        let std = std::net::UdpSocket::bind(addr)?;
        std.set_nonblocking(true)?;
        let socket = Arc::new(UdpSocket::from_std(std)?);
        let last = Arc::new(Mutex::new(Instant::now()));
        // The task holds a weak reference, so dropping the tunnel closes its sockets.
        let task = tokio::spawn(read_socket(Arc::downgrade(&self.0), socket.clone(), addr, last.clone()));
        Ok(Sock { socket, task, last })
    }

    /// Delivers a frame from the peer whose loopback address is `peer` to our Corrosion.
    pub async fn deliver(&self, peer: Ipv4Addr, frame: &[u8]) -> Result<()> {
        ensure!(frame.len() >= HEADER, "short frame");
        let src = get_addr(&frame[..ADDR_LEN]);
        let dst = get_addr(&frame[ADDR_LEN..HEADER]);
        ensure!(*src.ip() == peer, "frame source {src} is not the sending peer");
        ensure!(*dst.ip() == self.0.me, "frame destination {dst} is not this machine");
        let now = Instant::now();
        let answer = self.0.sent_from.lock().unwrap().get(&dst.port()).is_some_and(|t| now.duration_since(*t) < IDLE);
        ensure!(dst.port() == GOSSIP_PORT || answer, "frame destination {dst} is not our Corrosion");
        let socket = {
            let mut socks = self.0.sockets.lock().unwrap();
            if !self.0.peers.lock().unwrap().contains_key(&peer) {
                return Ok(());
            }
            let idle = |s: &Sock| now.duration_since(*s.last.lock().unwrap());
            socks.retain(|a, s| a.port() == GOSSIP_PORT || idle(s) < IDLE);
            let theirs: Vec<_> = socks
                .iter()
                .filter(|(a, _)| *a.ip() == peer && a.port() != GOSSIP_PORT && **a != src)
                .map(|(a, s)| (*a, idle(s)))
                .collect();
            if theirs.len() >= SOCKETS_PER_MEMBER
                && let Some((oldest, _)) = theirs.iter().max_by_key(|(_, idle)| *idle)
            {
                socks.remove(oldest);
            }
            let s = match socks.entry(src) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(e) => e.insert(self.bind(src)?),
            };
            *s.last.lock().unwrap() = now;
            s.socket.clone()
        };
        socket.send_to(&frame[HEADER..], dst).await?;
        Ok(())
    }
}

/// Datagrams from our Corrosion to `addr` go to the peer that owns `addr`.
async fn read_socket(inner: Weak<Inner>, socket: Arc<UdpSocket>, addr: SocketAddrV4, last: Arc<Mutex<Instant>>) {
    let mut buf = vec![0u8; 65536];
    loop {
        let (n, from) = match socket.recv_from(&mut buf).await {
            Ok(r) => r,
            Err(e) => {
                debug!("recv on {addr}: {e}");
                continue;
            }
        };
        let SocketAddr::V4(from) = from else { continue };
        let Some(inner) = inner.upgrade() else { return };
        if *from.ip() != inner.me {
            continue;
        }
        let now = Instant::now();
        *last.lock().unwrap() = now;
        {
            let mut sent = inner.sent_from.lock().unwrap();
            sent.retain(|_, t| now.duration_since(*t) < IDLE);
            sent.insert(from.port(), now);
        }
        let Some(tx) = inner.peers.lock().unwrap().get(addr.ip()).cloned() else { continue };
        let mut f = BytesMut::with_capacity(HEADER + n);
        put_addr(&mut f, from);
        put_addr(&mut f, addr);
        f.put_slice(&buf[..n]);
        // A full queue drops the datagram, as a congested network would.
        let _ = tx.try_send(f.freeze());
    }
}

fn put_addr(b: &mut BytesMut, a: SocketAddrV4) {
    b.put_slice(&a.ip().octets());
    b.put_u16(a.port());
}

fn get_addr(b: &[u8]) -> SocketAddrV4 {
    SocketAddrV4::new(Ipv4Addr::new(b[0], b[1], b[2], b[3]), u16::from_be_bytes([b[4], b[5]]))
}
