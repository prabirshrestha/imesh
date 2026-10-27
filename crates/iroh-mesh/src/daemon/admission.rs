//! Who may connect: members, on every ALPN.
//!
//! iroh proves every remote key in the TLS handshake. This module only decides which keys are
//! allowed, and closes connections that stop being allowed.

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex, OnceLock, RwLock},
};

use iroh::{
    EndpointId,
    endpoint::{AfterHandshakeOutcome, Connection, EndpointHooks, WeakConnectionHandle},
};
use tracing::{debug, info};

use iroh_relay::server::clients::Clients;

use crate::state::Store;

#[derive(Debug, Clone)]
pub struct Admission {
    store: Store,
    /// Peers named in `cluster.join`, trusted until the join ends.
    bootstrap: Arc<RwLock<BTreeSet<EndpointId>>>,
    /// Every accepted connection, incoming and outgoing, so removal can close it.
    conns: Arc<Mutex<Vec<Open>>>,
    /// The sessions on the relay this machine serves, so removal can end them too.
    relay: Arc<OnceLock<Clients>>,
    /// The keys allowed at the last [`Admission::enforce`], to find the ones removed since.
    allowed: Arc<Mutex<BTreeSet<EndpointId>>>,
}

/// An accepted connection: who, on which ALPN.
#[derive(Debug)]
struct Open {
    id: EndpointId,
    conn: WeakConnectionHandle,
}

/// Who is on the other end of an API call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "caller", content = "id", rename_all = "lowercase")]
pub enum Caller {
    /// The local socket or named pipe of this machine, which includes SSH through dial-stdio.
    Local,
    /// Another member, over iroh.
    Member(EndpointId),
}

impl Admission {
    pub fn new(store: Store) -> Self {
        Admission {
            store,
            bootstrap: Default::default(),
            conns: Default::default(),
            relay: Default::default(),
            allowed: Default::default(),
        }
    }

    /// Members, and the peers of a join in progress.
    fn is_member(&self, id: &EndpointId) -> bool {
        self.store.read(|s| s.is_member(id)) || self.bootstrap.read().unwrap().contains(id)
    }

    pub fn allows(&self, id: &EndpointId) -> bool {
        self.is_member(id)
    }

    pub fn caller(&self, id: EndpointId) -> Caller {
        Caller::Member(id)
    }

    pub fn set_bootstrap(&self, peers: BTreeSet<EndpointId>) {
        *self.bootstrap.write().unwrap() = peers;
    }

    pub(crate) fn bootstrap(&self) -> BTreeSet<EndpointId> {
        self.bootstrap.read().unwrap().clone()
    }

    pub(crate) fn set_relay_clients(&self, clients: Clients) {
        let _ = self.relay.set(clients);
    }

    /// Closes open connections of peers that are no longer allowed. The hook runs only at the
    /// handshake, and sync, API, and app connections stay open, so removal must close them. The
    /// relay checks a key only when it connects, so removal also ends its relay session.
    pub fn enforce(&self) {
        // Peers of a join count too, so their relay sessions end when the join does.
        let mut allowed: BTreeSet<EndpointId> = self.store.read(|s| s.active_members());
        allowed.extend(self.bootstrap());
        let mut before = self.allowed.lock().unwrap();
        if let Some(relay) = self.relay.get() {
            for id in before.difference(&allowed) {
                if !self.allows(id) && relay.disconnect(*id, None) {
                    info!(peer = %id.fmt_short(), "ended the relay session of a removed key");
                }
            }
        }
        *before = allowed;
        drop(before);
        let mut conns = self.conns.lock().unwrap();
        conns.retain(|o| {
            let Some(conn) = o.conn.upgrade() else { return false };
            if self.allows(&o.id) {
                return true;
            }
            info!(peer = %o.id.fmt_short(), "closing connection of a removed key");
            conn.close(403u32.into(), b"removed from cluster");
            false
        });
    }
}

impl EndpointHooks for Admission {
    async fn after_handshake<'a>(&'a self, conn: &'a Connection) -> AfterHandshakeOutcome {
        let id = conn.remote_id();
        if self.allows(&id) {
            let mut conns = self.conns.lock().unwrap();
            conns.retain(|o| o.conn.upgrade().is_some());
            conns.push(Open { id, conn: conn.weak_handle() });
            AfterHandshakeOutcome::accept()
        } else {
            debug!(peer = %id.fmt_short(), "rejected");
            AfterHandshakeOutcome::Reject {
                error_code: 403u32.into(),
                reason: b"not a member of this cluster".to_vec(),
            }
        }
    }
}
