//! Carries Corrosion's UDP gossip between machines over iroh.
//!
//! Every machine has two ports on 127.0.0.1, from its key (see [`super::slot`]): Corrosion listens
//! on the first and sends from the second. For every other member P, the daemon on M binds P's two
//! ports, so M's Corrosion reaches P there and P's replies seem to come from P. A datagram from
//! `src` to `dst` becomes a frame that the peer link sends to P. The daemon on P sends the payload
//! to `dst` from its socket bound at `src`. Both Corrosions see the same addresses.
//!
//! A member can only reach this machine's Corrosion: a frame must come from one of the member's
//! own two ports and go to one of this machine's two ports. Only 127.0.0.1 is used, so the
//! daemon runs on macOS too, where loopback answers no other address.
//!
//! Frame: src port (2), dst port (2), payload.

use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::{Arc, Mutex, Weak},
};

use anyhow::{Result, ensure};
use bytes::{BufMut, Bytes, BytesMut};
use tokio::{net::UdpSocket, sync::mpsc, task::JoinHandle};
use tracing::{debug, warn};

const HEADER: usize = 4;

pub type Sender = mpsc::Sender<Bytes>;

struct Sock {
    socket: Arc<UdpSocket>,
    task: JoinHandle<()>,
}

impl Drop for Sock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Clone)]
pub struct Tunnel(Arc<Inner>);

struct Inner {
    /// This machine's slot: Corrosion listens on it and sends from the next port.
    me: u16,
    /// Link queue for each peer, by the peer's slot.
    peers: Mutex<HashMap<u16, Sender>>,
    /// The two sockets bound for each peer, by local port.
    sockets: Mutex<HashMap<u16, Sock>>,
}

impl Tunnel {
    pub fn new(me: u16) -> Self {
        Tunnel(Arc::new(Inner { me, peers: Default::default(), sockets: Default::default() }))
    }

    /// Sets the peers. Binds both ports of each new one and closes the sockets of old ones.
    pub fn set_peers(&self, peers: HashMap<u16, Sender>) {
        let mut socks = self.0.sockets.lock().unwrap();
        socks.retain(|port, _| peers.contains_key(&slot_of(*port)));
        for slot in peers.keys() {
            if *slot == self.0.me {
                warn!(port = slot, "a member has this machine's gossip ports; it cannot sync");
                continue;
            }
            for port in [*slot, slot + 1] {
                if let std::collections::hash_map::Entry::Vacant(e) = socks.entry(port) {
                    match self.bind(port) {
                        Ok(s) => {
                            e.insert(s);
                        }
                        // Without this socket, our Corrosion cannot reach the member.
                        Err(e) => warn!("bind gossip port {port} of a member: {e:#}"),
                    }
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

    fn bind(&self, port: u16) -> Result<Sock> {
        let std = std::net::UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))?;
        std.set_nonblocking(true)?;
        let socket = Arc::new(UdpSocket::from_std(std)?);
        // The task holds a weak reference, so dropping the tunnel closes its sockets.
        let task = tokio::spawn(read_socket(Arc::downgrade(&self.0), socket.clone(), port));
        Ok(Sock { socket, task })
    }

    /// Delivers a frame from the peer with slot `peer` to our Corrosion.
    pub async fn deliver(&self, peer: u16, frame: &[u8]) -> Result<()> {
        ensure!(frame.len() >= HEADER, "short frame");
        let src = u16::from_be_bytes([frame[0], frame[1]]);
        let dst = u16::from_be_bytes([frame[2], frame[3]]);
        ensure!(slot_of(src) == peer, "frame source port {src} is not the sending peer");
        ensure!(slot_of(dst) == self.0.me, "frame destination port {dst} is not this machine");
        let socket = {
            if !self.0.peers.lock().unwrap().contains_key(&peer) {
                return Ok(());
            }
            match self.0.sockets.lock().unwrap().get(&src) {
                Some(s) => s.socket.clone(),
                None => return Ok(()),
            }
        };
        socket.send_to(&frame[HEADER..], SocketAddrV4::new(Ipv4Addr::LOCALHOST, dst)).await?;
        Ok(())
    }
}

/// The slot that `port` belongs to: a slot is even, and its client port is the next one.
fn slot_of(port: u16) -> u16 {
    port & !1
}

/// Datagrams from our Corrosion to `port` go to the peer that owns `port`.
async fn read_socket(inner: Weak<Inner>, socket: Arc<UdpSocket>, port: u16) {
    let mut buf = vec![0u8; 65536];
    loop {
        let (n, from) = match socket.recv_from(&mut buf).await {
            Ok(r) => r,
            Err(e) => {
                debug!("recv on {port}: {e}");
                continue;
            }
        };
        let SocketAddr::V4(from) = from else { continue };
        let Some(inner) = inner.upgrade() else { return };
        // Only our own Corrosion sends to these sockets: from its own two ports on 127.0.0.1.
        if *from.ip() != Ipv4Addr::LOCALHOST || slot_of(from.port()) != inner.me {
            continue;
        }
        let Some(tx) = inner.peers.lock().unwrap().get(&slot_of(port)).cloned() else { continue };
        let mut f = BytesMut::with_capacity(HEADER + n);
        f.put_u16(from.port());
        f.put_u16(port);
        f.put_slice(&buf[..n]);
        // A full queue drops the datagram, as a congested network would.
        let _ = tx.try_send(f.freeze());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_reach_only_this_machines_ports_from_the_peer() {
        let me = 40000;
        let peer = 40010;
        let t = Tunnel::new(me);
        let (tx, mut rx) = mpsc::channel(8);
        t.set_peers(HashMap::from([(peer, tx)]));
        let corrosion = UdpSocket::bind((Ipv4Addr::LOCALHOST, me)).await.unwrap();
        let frame = |src: u16, dst: u16| {
            let mut f = BytesMut::new();
            f.put_u16(src);
            f.put_u16(dst);
            f.put_slice(b"hi");
            f.freeze()
        };
        // A frame from the peer's client port to our gossip port reaches our Corrosion from there.
        t.deliver(peer, &frame(peer + 1, me)).await.unwrap();
        let mut buf = [0u8; 16];
        let (n, from) = corrosion.recv_from(&mut buf).await.unwrap();
        assert_eq!((&buf[..n], from.port()), (&b"hi"[..], peer + 1));
        // Wrong sender, or a destination that is not ours, is refused.
        assert!(t.deliver(peer, &frame(40020, me)).await.is_err());
        assert!(t.deliver(peer, &frame(peer, 40020)).await.is_err());
        // Our Corrosion sending to the peer's gossip port becomes a frame for the peer.
        corrosion.send_to(b"yo", (Ipv4Addr::LOCALHOST, peer)).await.unwrap();
        let f = rx.recv().await.unwrap();
        assert_eq!(&f[..], &[&me.to_be_bytes()[..], &peer.to_be_bytes()[..], b"yo"].concat()[..]);
        // A process that is not our Corrosion is ignored.
        let other = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        other.send_to(b"no", (Ipv4Addr::LOCALHOST, peer)).await.unwrap();
        corrosion.send_to(b"ok", (Ipv4Addr::LOCALHOST, peer)).await.unwrap();
        assert_eq!(&rx.recv().await.unwrap()[HEADER..], b"ok");
    }
}
