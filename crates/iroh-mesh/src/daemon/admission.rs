//! Who may connect: members, on every ALPN.
//!
//! iroh proves every remote key in the TLS handshake. This module only decides which keys are
//! allowed, and closes connections that stop being allowed.

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex, OnceLock},
};

use iroh::{
    EndpointId,
    endpoint::{AfterHandshakeOutcome, Connection, EndpointHooks, WeakConnectionHandle},
};
use tracing::{debug, info};

use iroh_relay::server::clients::Clients;

use crate::state::Store;

/// The close code for a key that the roster removed. The machine learns from it that it is out.
pub(crate) const REMOVED: u32 = 4031;
/// The close code for a key that is not a member.
pub(crate) const NOT_MEMBER: u32 = 4030;
/// How long a removed key keeps its relay session after its connection closed, so the close
/// frame reaches it.
const RELAY_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct Admission {
    store: Store,
    /// Every accepted connection, incoming and outgoing, so removal can close it.
    conns: Arc<Mutex<Vec<Open>>>,
    /// The sessions on the relay this machine serves, so removal can end them too.
    relay: Arc<OnceLock<Clients>>,
    /// The keys allowed at the last [`Admission::enforce`], to find the ones removed since.
    allowed: Arc<Mutex<BTreeSet<EndpointId>>>,
}

impl Admission {
    /// The close code for `id`: removed, or never a member.
    fn refusal(&self, id: &EndpointId) -> u32 {
        if self.store.read(|s| s.is_removed(id)) { REMOVED } else { NOT_MEMBER }
    }
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
        Admission { store, conns: Default::default(), relay: Default::default(), allowed: Default::default() }
    }

    pub fn allows(&self, id: &EndpointId) -> bool {
        self.store.read(|s| s.is_member(id))
    }

    pub fn caller(&self, id: EndpointId) -> Caller {
        Caller::Member(id)
    }

    pub(crate) fn set_relay_clients(&self, clients: Clients) {
        let _ = self.relay.set(clients);
    }

    /// Closes open connections of peers that are no longer allowed. The hook runs only at the
    /// handshake, and sync, API, and app connections stay open, so removal must close them. The
    /// relay checks a key only when it connects, so removal also ends its relay session.
    ///
    /// The connection closes first, with the code that tells the peer it is removed. Over a relay
    /// that close frame travels through the peer's relay session, so the session ends only after
    /// [`RELAY_GRACE`]. Else a machine that leaves through a relay never learns that a member took
    /// its removal.
    pub fn enforce(&self) {
        let allowed: BTreeSet<EndpointId> = self.store.read(|s| s.active_members());
        let gone: Vec<EndpointId> = {
            let mut before = self.allowed.lock().unwrap();
            let gone = before.difference(&allowed).filter(|id| !self.allows(id)).copied().collect();
            *before = allowed;
            gone
        };
        // The hook checks and records a connection under this lock, so a connection is either
        // recorded here and closed, or checked after the roster changed and refused.
        let mut conns = self.conns.lock().unwrap();
        conns.retain(|o| {
            let Some(conn) = o.conn.upgrade() else { return false };
            if self.allows(&o.id) {
                return true;
            }
            info!(peer = %o.id.fmt_short(), "closing connection of a removed key");
            conn.close(self.refusal(&o.id).into(), b"removed from cluster");
            false
        });
        drop(conns);
        if gone.is_empty() || self.relay.get().is_none() {
            return;
        }
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                let this = self.clone();
                runtime.spawn(async move {
                    tokio::time::sleep(RELAY_GRACE).await;
                    this.end_relay_sessions(&gone);
                });
            }
            Err(_) => self.end_relay_sessions(&gone),
        }
    }

    fn end_relay_sessions(&self, gone: &[EndpointId]) {
        let Some(relay) = self.relay.get() else { return };
        for id in gone {
            if !self.allows(id) && relay.disconnect(*id, None) {
                info!(peer = %id.fmt_short(), "ended the relay session of a removed key");
            }
        }
    }
}

impl EndpointHooks for Admission {
    async fn after_handshake<'a>(&'a self, conn: &'a Connection) -> AfterHandshakeOutcome {
        let id = conn.remote_id();
        let mut conns = self.conns.lock().unwrap();
        if self.allows(&id) {
            conns.retain(|o| o.conn.upgrade().is_some());
            conns.push(Open { id, conn: conn.weak_handle() });
            AfterHandshakeOutcome::accept()
        } else {
            debug!(peer = %id.fmt_short(), "rejected");
            AfterHandshakeOutcome::Reject {
                error_code: self.refusal(&id).into(),
                reason: b"not a member of this cluster".to_vec(),
            }
        }
    }
}
