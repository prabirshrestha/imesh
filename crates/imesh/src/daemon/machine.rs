//! The running machine: its parts, its API, and the protocols it serves.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    ops::Deref,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, ensure};
use futures_util::{SinkExt, StreamExt};
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayUrl, Watcher,
    address_lookup::MemoryLookup,
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler},
};
use n0_future::task::AbortOnDropHandle;
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

use super::{
    Admission, Caller, Config, Method, VERSION,
    links::{Peer, PeerState},
};
use crate::{
    api::{self, API_ALPN, Reply, Request},
    net::{self, Relays},
    state::{MachineInfo, Snapshot, Store, tunnel::Tunnel},
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const ONLINE_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the background loop runs without a change, to steer SWIM.
const TICK: Duration = Duration::from_secs(2);

/// A running machine. Apps get it in their methods and protocols.
#[derive(Clone)]
pub struct Machine(pub(crate) Arc<Inner>);

/// The machine's parts. Apps use the methods of [`Machine`].
#[doc(hidden)]
pub struct Inner {
    // What the machine is.
    pub(crate) name: String,
    pub(crate) config: Config,
    pub(crate) endpoint: Endpoint,
    pub(crate) store: Store,
    pub(crate) methods: BTreeMap<String, Method>,

    // Who may connect, and where members are.
    pub(crate) admission: Admission,
    pub(crate) lookup: MemoryLookup,

    // Links to members (see `links.rs`).
    pub(crate) peers: Mutex<BTreeMap<EndpointId, Peer>>,
    /// The latest link connection each member opened to us.
    pub(crate) incoming: Mutex<BTreeMap<EndpointId, Connection>>,
    /// Carries Corrosion's gossip between this machine's loopback and the member links.
    pub(crate) tunnel: Tunnel,

    // Relays.
    /// The relays the endpoint started with. Used again when the cluster setting is cleared.
    pub(crate) initial_relays: Relays,
    /// The relays the endpoint uses now. iroh has no getter for its relay map.
    pub(crate) relays: tokio::sync::Mutex<Relays>,
    /// The cluster relays from a join ticket, used until the join ends.
    pub(crate) joining: Mutex<Relays>,
    /// Loopback URL of the relay this machine serves through a private tunnel.
    pub(crate) exposed: tokio::sync::watch::Sender<Option<RelayUrl>>,

    // Lifecycle.
    /// Set, under the `peers` lock, when the machine stops.
    pub(crate) stopped: std::sync::atomic::AtomicBool,
    /// Set by `machine.reset`. [`Running::reset_requested`] waits for it.
    pub(crate) reset: tokio::sync::watch::Sender<bool>,
    /// Set by the `machine.reset` method: reset once its reply has left.
    pub(crate) reset_after_reply: std::sync::atomic::AtomicBool,
}

impl Deref for Machine {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

impl std::fmt::Debug for Machine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Machine({})", self.id().fmt_short())
    }
}

impl Machine {
    pub fn id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// The replicated database. Apps query and write their own tables with it.
    pub fn state(&self) -> &Store {
        &self.store
    }

    /// The machine's endpoint. It talks only to members and authorized CLI keys: the admission
    /// hook refuses other keys in both directions. Use an endpoint of your own for peers outside
    /// the cluster.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Serves the API on a local listener until `stop` resolves, with every caller as
    /// [`Caller::Local`]: full control. Then it closes every connection and returns. [`super::serve`]
    /// runs this; call it yourself after [`super::Builder::spawn`] to serve the API elsewhere.
    pub async fn serve_local(self, mut listener: crate::net::local::Listener, stop: impl Future<Output = ()> + Send) {
        tokio::pin!(stop);
        let mut conns = JoinSet::new();
        loop {
            // Reap finished connections.
            while conns.try_join_next().is_some() {}
            let accepted = tokio::select! {
                a = listener.accept() => a,
                _ = &mut stop => {
                    conns.shutdown().await;
                    return;
                }
            };
            match accepted {
                Ok(s) => {
                    let m = self.clone();
                    conns.spawn(async move {
                        if let Err(e) = m.serve_api(api::framed(s), Caller::Local).await {
                            debug!("local api: {e:#}");
                        }
                        // The reply is in the socket's buffer, which the caller reads also after we stop.
                        m.finish_reset();
                    });
                }
                Err(e) => {
                    warn!("accept on the local API: {e:#}");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        }
    }

    /// The machines in the cluster now, this one included.
    pub fn members(&self) -> BTreeSet<EndpointId> {
        self.store.read(Snapshot::active_members)
    }

    /// Opens a connection to another member on any ALPN. Addresses come from the cluster state.
    pub async fn connect(&self, to: EndpointId, alpn: &[u8]) -> Result<Connection> {
        ensure!(self.members().contains(&to), "{} is not a member", to.fmt_short());
        self.dial(self.peer_addr(to), alpn).await
    }

    pub(crate) async fn dial(&self, addr: EndpointAddr, alpn: &[u8]) -> Result<Connection> {
        let id = addr.id;
        tokio::time::timeout(CONNECT_TIMEOUT, self.endpoint.connect(addr, alpn))
            .await
            .map_err(|_| anyhow!("connect to {} timed out", id.fmt_short()))?
            .with_context(|| format!("connect to {}", id.fmt_short()))
    }

    /// Waits until the home relay is connected, so our address includes the relay URL.
    pub(crate) async fn online(&self) {
        if !self.relays.lock().await.is_empty() {
            let _ = tokio::time::timeout(ONLINE_TIMEOUT, self.endpoint.online()).await;
        }
    }

    /// What this machine publishes about itself in `imesh_machines`.
    pub fn info(&self) -> MachineInfo {
        MachineInfo {
            name: self.name.clone(),
            addr: self.endpoint.addr(),
            version: VERSION.to_string(),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            relay: self.exposed.borrow().clone(),
            labels: self.config.labels.clone(),
        }
    }

    /// Runs the embedded relay when `--serve-relay` is set, and exposes it when asked.
    pub(super) async fn serve_relay(
        &self,
    ) -> Result<(Option<iroh_relay::server::Server>, Option<AbortOnDropHandle<()>>)> {
        let r = &self.config.net;
        let Some(addr) = r.serve_relay else { return Ok((None, None)) };
        let gate =
            net::relay::Gate { me: self.id(), token: r.serve_relay_token.clone(), admission: self.admission.clone() };
        let server = net::relay::serve(r, addr, gate).await?;
        if let Some(service) = server.relay_service() {
            self.admission.set_relay_clients(service.clients().clone());
        }
        let tunnel = r.expose_relay.clone().map(|how| {
            let port = server.https_addr().or(server.http_addr()).map_or(addr.port(), |a| a.port());
            net::devtunnel::expose(how, port, self.id(), self.exposed.clone())
        });
        Ok((Some(server), tunnel))
    }

    /// Publishes our own record, keeps links, learns peer addresses, enforces removal, and steers
    /// SWIM.
    pub(super) async fn background(self) {
        let mut changes = self.store.subscribe();
        let mut addr = self.endpoint.watch_addr();
        let mut tick = tokio::time::interval(TICK);
        let mut exposed = self.exposed.subscribe();
        let mut known: BTreeMap<EndpointId, EndpointAddr> = BTreeMap::new();
        loop {
            let snap = self.store.snapshot();
            for id in snap.machines.keys() {
                let Some(info) = snap.machine(id) else { continue };
                if known.get(id) == Some(&info.addr) {
                    continue;
                }
                self.lookup.set_endpoint_info(info.addr.clone());
                // A running dial does not see new addresses. Restart the link of a member that is
                // not up, so it dials the new address at once.
                if known.insert(*id, info.addr).is_some() && self.peer_status(id).state != PeerState::Up {
                    self.peers.lock().unwrap().remove(id);
                }
            }
            self.admission.enforce();
            self.apply_cluster_relays(&snap).await;
            if snap.cluster.is_some() {
                self.publish_self().await;
            }
            // Also with no cluster: after a failed join, this drops the links it started.
            self.reconcile_links();
            tokio::select! {
                _ = changes.changed() => {}
                _ = addr.updated() => {}
                _ = exposed.changed() => {}
                _ = tick.tick() => self.steer_swim(&snap).await,
            }
        }
    }

    /// SWIM finds members on its own, but slowly through a tunnel. Tell it about members whose
    /// link is up, and that removed members are gone for good.
    async fn steer_swim(&self, snap: &Snapshot) {
        for (id, m) in &snap.members {
            if m.removed && self.store.swim_alive(id) {
                self.store.forget(id).await;
            } else if !m.removed && *id != self.id() && self.link_up(id) && !self.store.swim_alive(id) {
                self.store.announce(id).await;
            }
        }
    }

    /// Makes the relay map equal to the cluster's relay setting. The setting replaces the relays
    /// the machine started with. With no setting, the machine uses those again, plus the relays
    /// of a join in progress.
    pub(crate) async fn apply_cluster_relays(&self, snap: &Snapshot) {
        let set = snap.relay_settings();
        let want = if set.relays.is_empty() {
            let mut want = self.initial_relays.clone();
            want.extend(self.joining.lock().unwrap().clone());
            want
        } else {
            net::relays(&set.relays, &set.token)
        };
        self.set_relays(want).await;
    }

    pub(super) async fn set_relays(&self, want: Relays) {
        let mut have = self.relays.lock().await;
        if *have == want {
            return;
        }
        for (url, token) in &want {
            if have.get(url) != Some(token) {
                info!(%url, "using relay");
                self.endpoint.insert_relay(url.clone(), Arc::new(net::config(url, token))).await;
            }
        }
        for url in have.keys().filter(|u| !want.contains_key(*u)) {
            info!(%url, "dropping relay");
            self.endpoint.remove_relay(url).await;
        }
        *have = want;
    }

    pub(crate) async fn publish_self(&self) {
        let me = self.id();
        let info = self.info();
        let (member, same) = self.store.read(|s| (s.is_member(&me), s.machine(&me).as_ref() == Some(&info)));
        if !member || same {
            return;
        }
        let json = serde_json::to_string(&info).expect("machine info is JSON");
        let r = self
            .store
            .write(|c| {
                c.execute(
                    "INSERT INTO imesh_machines (id, info) VALUES (?1, ?2) ON CONFLICT (id) DO UPDATE SET info = excluded.info",
                    rusqlite::params![me.to_string(), json],
                )
            })
            .await;
        if let Err(e) = r {
            warn!("publish self: {e:#}");
        }
    }

    pub(crate) fn peer_addr(&self, id: EndpointId) -> EndpointAddr {
        self.store.read(|s| s.machine(&id)).map_or_else(|| id.into(), |i| i.addr)
    }

    /// Serves one API call. A call for another machine goes one hop over iroh. A member never
    /// forwards a call that another member sent, so calls cannot loop.
    pub(super) async fn serve_api(&self, mut s: api::Stream, caller: Caller) -> Result<()> {
        let Some(req) = api::recv::<Request>(&mut s).await? else {
            return Ok(());
        };
        if let Some(to) = req.to
            && to != self.id()
        {
            if let Caller::Member(_) = caller {
                return api::send(&mut s, &Reply::Err("forwarding is limited to one hop".into())).await;
            }
            return self.forward(s, to, req, caller).await;
        }
        // A call that a member forwards for a CLI key runs as that key, so forwarding gives the
        // key no more rights than a direct call.
        let caller = match (caller, req.via) {
            (_, None) => caller,
            (Caller::Member(_), Some(key)) if self.store.read(|s| s.clients.contains_key(&key)) => Caller::Client(key),
            _ => {
                return api::send(&mut s, &Reply::Err("a forwarded call names a key that is not authorized".into()))
                    .await;
            }
        };
        // A method's writes are committed when its store calls return, so an acknowledged write
        // survives a crash of the daemon.
        let result = match self.methods.get(&req.method) {
            Some(m) => m(self.clone(), caller, req.params).await,
            None => Err(anyhow!("unknown method {:?}", req.method)),
        };
        let reply = match result {
            Ok(v) => Reply::Ok(v),
            Err(e) => Reply::Err(format!("{e:#}")),
        };
        api::send(&mut s, &reply).await
    }

    pub(super) async fn forward(
        &self,
        mut client: api::Stream,
        to: EndpointId,
        mut req: Request,
        caller: Caller,
    ) -> Result<()> {
        let remote = async {
            let conn = self.connect(to, API_ALPN).await?;
            let (send, recv) = conn.open_bi().await?;
            anyhow::Ok((conn, api::framed(tokio::io::join(recv, send))))
        };
        let (_conn, mut remote) = match remote.await {
            Ok(r) => r,
            Err(e) => return api::send(&mut client, &Reply::Err(format!("{e:#}"))).await,
        };
        req.to = None;
        req.via = match caller {
            Caller::Client(key) => Some(key),
            Caller::Local | Caller::Member(_) => None,
        };
        api::send(&mut remote, &req).await?;
        loop {
            tokio::select! {
                f = remote.next() => match f {
                    Some(f) => client.send(f?.freeze()).await?,
                    None => return Ok(()),
                },
                f = client.next() => match f {
                    Some(f) => remote.send(f?.freeze()).await?,
                    None => return Ok(()),
                },
            }
        }
    }
}

/// Serves every stream that the peer opens on `conn`, each in its own task.
async fn each_stream<F, Fut>(conn: Connection, what: &'static str, serve: F) -> Result<(), AcceptError>
where
    F: Fn(api::Stream) -> Fut,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    loop {
        let (send, recv) = conn.accept_bi().await?;
        let task = serve(api::framed(tokio::io::join(recv, send)));
        tokio::spawn(async move {
            if let Err(e) = task.await {
                debug!("{what}: {e:#}");
            }
        });
    }
}

#[derive(Debug, Clone)]
pub(super) struct ApiProtocol(pub(super) Machine);

impl ProtocolHandler for ApiProtocol {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let caller = self.0.admission.caller(conn.remote_id());
        let c = conn.clone();
        each_stream(conn, "iroh api", |s| {
            let (m, conn) = (self.0.clone(), c.clone());
            async move {
                let served = m.serve_api(s, caller).await;
                if m.reset_after_reply.load(std::sync::atomic::Ordering::Relaxed) {
                    // The caller closes the connection once it has the reply.
                    let _ = tokio::time::timeout(super::cluster::REPLY_TIMEOUT, conn.closed()).await;
                    m.finish_reset();
                }
                served
            }
        })
        .await
    }
}
