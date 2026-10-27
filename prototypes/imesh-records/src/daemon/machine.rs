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
    Admission, Caller, Method, Opts, VERSION,
    links::{EXCHANGE_EVERY, Peer, PeerState},
};
use crate::{
    api::{self, API_ALPN, Reply, Request},
    net::{self, Relays},
    state::{MachineInfo, State, keys, sync},
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const ONLINE_TIMEOUT: Duration = Duration::from_secs(10);

/// A running machine. Apps get it in their methods and protocols.
#[derive(Clone)]
pub struct Machine(pub(crate) Arc<Inner>);

/// The machine's parts. Apps use the methods of [`Machine`].
#[doc(hidden)]
pub struct Inner {
    pub(crate) endpoint: Endpoint,
    pub(crate) state: Arc<State>,
    pub(crate) opts: Opts,
    pub(crate) admission: Admission,
    pub(crate) lookup: MemoryLookup,
    pub(crate) name: String,
    pub(crate) methods: BTreeMap<String, Method>,
    pub(crate) peers: Mutex<BTreeMap<EndpointId, Peer>>,
    /// Set, under the `peers` lock, when the machine stops.
    pub(crate) stopped: std::sync::atomic::AtomicBool,
    /// Public URL of the relay this machine serves, once a tunnel exposes it.
    pub(crate) exposed: tokio::sync::watch::Sender<Option<RelayUrl>>,
    /// The relays the endpoint started with. Used again when the cluster setting is cleared.
    pub(crate) initial_relays: Relays,
    /// The relays the endpoint uses now. iroh has no getter for its relay map.
    pub(crate) relays: tokio::sync::Mutex<Relays>,
    /// The cluster relays from a join ticket, used until the join ends.
    pub(crate) joining: Mutex<Relays>,
    /// Set by `machine.reset`. [`Running::reset_requested`] waits for it.
    pub(crate) reset: tokio::sync::watch::Sender<bool>,
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

    /// The replicated state. Apps keep their records under `app/<app name>/`.
    pub fn state(&self) -> &Arc<State> {
        &self.state
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

    pub fn opts(&self) -> &Opts {
        &self.opts
    }

    /// Serves the local API until `stop`. Then it closes every connection and returns.
    pub(super) async fn serve_local(
        self,
        mut listener: crate::net::local::Listener,
        mut stop: tokio::sync::oneshot::Receiver<()>,
    ) {
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
                    });
                }
                Err(e) => {
                    warn!("accept on the local API: {e:#}");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        }
    }

    pub fn members(&self) -> BTreeSet<EndpointId> {
        self.state.members()
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

    /// What this machine publishes about itself under `machine/<id>`.
    pub fn info(&self) -> MachineInfo {
        MachineInfo {
            name: self.name.clone(),
            addr: self.endpoint.addr(),
            version: VERSION.to_string(),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            relay: self.exposed.borrow().clone(),
            labels: self.opts.labels.iter().cloned().collect(),
        }
    }

    /// Runs the embedded relay when `--serve-relay` is set, and exposes it when asked.
    pub(super) async fn serve_relay(
        &self,
    ) -> Result<(Option<iroh_relay::server::Server>, Option<AbortOnDropHandle<()>>)> {
        let r = &self.opts.relay;
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

    /// Publishes our own record, keeps links, learns peer addresses, and enforces removal.
    pub(super) async fn background(self) {
        let mut changes = self.state.subscribe();
        let mut addr = self.endpoint.watch_addr();
        let mut tick = tokio::time::interval(EXCHANGE_EVERY);
        let mut exposed = self.exposed.subscribe();
        let mut known: BTreeMap<EndpointId, EndpointAddr> = BTreeMap::new();
        loop {
            for (_, r) in self.state.live(keys::MACHINE) {
                let Some(info) = r.json::<MachineInfo>() else { continue };
                let id = info.addr.id;
                if known.get(&id) == Some(&info.addr) {
                    continue;
                }
                self.lookup.set_endpoint_info(info.addr.clone());
                // A running dial does not see new addresses. Restart the link of a peer that is
                // not up, so it dials the new address at once.
                if known.insert(id, info.addr).is_some() && self.peer_status(&id).state != PeerState::Up {
                    self.peers.lock().unwrap().remove(&id);
                }
            }
            self.admission.enforce();
            self.apply_cluster_relays().await;
            if self.state.cluster().is_some() {
                self.publish_self();
            }
            // Also with no cluster: after a failed join, this drops the links it started.
            self.reconcile_links();
            tokio::select! {
                _ = changes.recv() => {}
                _ = addr.updated() => {}
                _ = exposed.changed() => {}
                _ = tick.tick() => {}
            }
        }
    }

    /// Makes the relay map equal to the cluster's `settings/relays`. The setting replaces the
    /// relays the machine started with. With no setting, the machine uses those again, plus the
    /// relays of a join in progress.
    pub(crate) async fn apply_cluster_relays(&self) {
        let set = self.state.relay_settings();
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

    pub(crate) fn publish_self(&self) {
        let info = self.info();
        if self.state.machine(&self.id()).as_ref() != Some(&info)
            && self.members().contains(&self.id())
            && let Err(e) = self.state.put(&keys::machine(&self.id()), Some(&info))
        {
            warn!("publish self: {e:#}");
        }
    }

    pub(crate) fn peer_addr(&self, id: EndpointId) -> EndpointAddr {
        self.state.machine(&id).map_or_else(|| id.into(), |i| i.addr)
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
            (Caller::Member(_), Some(key)) if self.state.clients().contains(&key) => Caller::Client(key),
            _ => {
                return api::send(&mut s, &Reply::Err("a forwarded call names a key that is not authorized".into()))
                    .await;
            }
        };
        let result = match self.methods.get(&req.method) {
            // Answer only after the writes are on disk, so an acknowledged write survives a crash.
            Some(m) => match m(self.clone(), caller, req.params).await {
                Ok(v) => self.state.commit().await.map(|()| v),
                Err(e) => Err(e),
            },
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
pub(super) struct SyncProtocol(pub(super) Arc<State>);

impl ProtocolHandler for SyncProtocol {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        each_stream(conn, "sync", |s| {
            let state = self.0.clone();
            async move { sync::serve(&state, s).await }
        })
        .await
    }
}

#[derive(Debug, Clone)]
pub(super) struct ApiProtocol(pub(super) Machine);

impl ProtocolHandler for ApiProtocol {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let caller = self.0.admission.caller(conn.remote_id());
        each_stream(conn, "iroh api", |s| {
            let m = self.0.clone();
            async move { m.serve_api(s, caller).await }
        })
        .await
    }
}
