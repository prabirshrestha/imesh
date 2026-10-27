//! Who may connect: members on every ALPN, authorized CLI keys on the API ALPN.
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

use crate::{
    api::API_ALPN,
    state::{State, keys},
};

#[derive(Debug, Clone)]
pub struct Admission {
    state: Arc<State>,
    /// Peers named in `cluster.join`, trusted until the join ends.
    bootstrap: Arc<RwLock<BTreeSet<EndpointId>>>,
    /// Every accepted connection, incoming and outgoing, so removal can close it.
    conns: Arc<Mutex<Vec<Open>>>,
    /// The sessions on the relay this machine serves, so removal can end them too.
    relay: Arc<OnceLock<Clients>>,
}

/// An accepted connection: who, on which ALPN.
#[derive(Debug)]
struct Open {
    id: EndpointId,
    alpn: Vec<u8>,
    conn: WeakConnectionHandle,
}

/// Who is on the other end of an API call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "caller", content = "id", rename_all = "lowercase")]
pub enum Caller {
    /// The local socket or named pipe of this machine, which includes SSH through dial-stdio.
    Local,
    /// A CLI key from `client/<key>`, over iroh.
    Client(EndpointId),
    /// Another member, over iroh.
    Member(EndpointId),
}

impl Admission {
    pub fn new(state: Arc<State>) -> Self {
        Admission { state, bootstrap: Default::default(), conns: Default::default(), relay: Default::default() }
    }

    /// Members, and the peers of a join in progress.
    fn is_member(&self, id: &EndpointId) -> bool {
        self.state.members().contains(id) || self.bootstrap.read().unwrap().contains(id)
    }

    pub fn allows(&self, id: &EndpointId, alpn: &[u8]) -> bool {
        self.is_member(id) || (alpn == API_ALPN && self.state.clients().contains(id))
    }

    pub fn caller(&self, id: EndpointId) -> Caller {
        if self.is_member(&id) { Caller::Member(id) } else { Caller::Client(id) }
    }

    pub fn set_bootstrap(&self, peers: BTreeSet<EndpointId>) {
        *self.bootstrap.write().unwrap() = peers;
    }

    pub(crate) fn set_relay_clients(&self, clients: Clients) {
        let _ = self.relay.set(clients);
    }

    /// Closes open connections of peers that are no longer allowed. The hook runs only at the
    /// handshake, and sync, API, and app connections stay open, so removal must close them. The
    /// relay checks a key only when it connects, so removal also ends its relay session.
    pub fn enforce(&self) {
        if let Some(relay) = self.relay.get() {
            let removed = [keys::MEMBER, keys::CLIENT]
                .into_iter()
                .flat_map(|prefix| {
                    self.state
                        .list(prefix)
                        .into_iter()
                        .filter(|r| r.is_deleted())
                        .map(move |r| r.key[prefix.len()..].to_string())
                })
                .filter_map(|id| id.parse::<EndpointId>().ok());
            for id in removed {
                if !self.allows(&id, API_ALPN) && relay.disconnect(id, None) {
                    info!(peer = %id.fmt_short(), "ended the relay session of a removed key");
                }
            }
        }
        let mut conns = self.conns.lock().unwrap();
        conns.retain(|o| {
            let Some(conn) = o.conn.upgrade() else { return false };
            if self.allows(&o.id, &o.alpn) {
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
        if self.allows(&id, conn.alpn()) {
            let mut conns = self.conns.lock().unwrap();
            conns.retain(|o| o.conn.upgrade().is_some());
            conns.push(Open { id, alpn: conn.alpn().to_vec(), conn: conn.weak_handle() });
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
