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
    Endpoint, EndpointAddr, EndpointId, Watcher,
    address_lookup::MemoryLookup,
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler},
};
use n0_future::task::AbortOnDropHandle;
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

use super::{Admission, Caller, Config, Method, VERSION, links::PeerStatus};
use crate::{
    api::{self, API_ALPN, Reply, Request},
    net::{self, Relays},
    roster::{Change, Location},
    state::{MachineInfo, Snapshot, Store},
};

/// The file in the data directory that keeps the mesh relay port between runs.
const MESH_PORT_FILE: &str = "mesh-port";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const ONLINE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long an address change settles before this machine signs where it is now.
const ADDR_SETTLE: Duration = Duration::from_secs(2);

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
    /// The ALPNs that the local `open` call may reach. See [`super::Builder::openable`].
    pub(crate) openable: BTreeSet<Vec<u8>>,

    // Who may connect, and where members are.
    pub(crate) admission: Admission,
    pub(crate) lookup: MemoryLookup,

    // Roster sync (see `links.rs`).
    /// How the last hello with each member went.
    pub(crate) peers: Mutex<BTreeMap<EndpointId, PeerStatus>>,
    /// Set when every member that answered refused this machine as removed. It then says hello
    /// to no one.
    pub(crate) cut_off: std::sync::atomic::AtomicBool,
    /// Members this machine says hello to now, and members whose hello it answers now.
    pub(crate) hellos: Mutex<BTreeSet<EndpointId>>,
    pub(crate) answering: Mutex<BTreeSet<EndpointId>>,

    // Relays.
    /// The relays the endpoint started with. Used again when the cluster setting is cleared.
    pub(crate) initial_relays: Relays,
    /// The relays the endpoint uses now. iroh has no getter for its relay map.
    pub(crate) relays: tokio::sync::Mutex<Relays>,
    /// The cluster relays from a join ticket, used until the join ends.
    pub(crate) joining: Mutex<Relays>,
    /// The relay this machine serves through a private tunnel, while its host is ready.
    pub(crate) exposed: tokio::sync::watch::Sender<Option<net::devtunnel::Hosted>>,
    /// The forwards of other members' tunnels.
    pub(crate) mesh: super::mesh::Mesh,
    /// Counts wakes from sleep and network changes (see `wake.rs`).
    pub(crate) wake: tokio::sync::watch::Sender<u64>,

    // Lifecycle.
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

    /// This machine's copy of the roster.
    pub fn state(&self) -> &Store {
        &self.store
    }

    /// The machine's endpoint. It talks only to members: the admission
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

    /// Opens a connection to another member on any ALPN. Addresses come from the roster.
    pub async fn connect(&self, to: EndpointId, alpn: &[u8]) -> Result<Connection> {
        ensure!(self.members().contains(&to), "{} is not a member", to.fmt_short());
        self.dial(self.peer_addr(to), alpn).await
    }

    pub(crate) async fn dial(&self, addr: EndpointAddr, alpn: &[u8]) -> Result<Connection> {
        let id = addr.id;
        let tried = format!("{:?}", addr.addrs);
        let r = tokio::time::timeout(CONNECT_TIMEOUT, self.endpoint.connect(addr, alpn))
            .await
            .map_err(|_| anyhow!("connect to {} timed out", id.fmt_short()))
            .and_then(|c| c.with_context(|| format!("connect to {}", id.fmt_short())));
        if let Err(e) = &r {
            warn!(peer = %id.fmt_short(), %tried, "dial failed: {e:#}");
        }
        r
    }

    /// Waits until the home relay is connected, so our address includes the relay URL.
    pub(crate) async fn online(&self) {
        if !self.relays.lock().await.is_empty() {
            let _ = tokio::time::timeout(ONLINE_TIMEOUT, self.endpoint.online()).await;
        }
    }

    /// What this machine says about itself in the roster.
    pub fn info(&self) -> MachineInfo {
        MachineInfo {
            name: self.name.clone(),
            addr: self.endpoint.addr(),
            version: VERSION.to_string(),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            relay: self.exposed.borrow().as_ref().map(|h| h.url.clone()),
            tunnel: self.tunnel_endpoint(),
            labels: self.config.labels.clone(),
        }
    }

    /// The loopback port of this machine's mesh relay. An explicit `--mesh-port` is used as it is,
    /// and the relay fails to start when it is in use. Else the port of the last run when it is
    /// free, else 0: the system picks a free port. Machines may use the same port or not, and a
    /// clash never needs a person.
    fn mesh_relay_port(&self) -> u16 {
        if let Some(port) = self.config.net.mesh_port {
            return port;
        }
        std::fs::read_to_string(self.store.dir().join(MESH_PORT_FILE))
            .ok()
            .and_then(|s| s.trim().parse::<u16>().ok())
            .filter(|port| std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, *port)).is_ok())
            .unwrap_or(0)
    }

    /// Runs the embedded relay when `--serve-relay` is set, and exposes it when asked.
    pub(super) async fn serve_relay(
        &self,
    ) -> Result<(Option<iroh_relay::server::Server>, Option<AbortOnDropHandle<()>>)> {
        let r = &self.config.net;
        let mesh_addr = r.devtunnel_mesh.then(|| std::net::SocketAddr::from(([127, 0, 0, 1], self.mesh_relay_port())));
        let Some(addr) = r.serve_relay.or(mesh_addr) else { return Ok((None, None)) };
        let gate =
            net::relay::Gate { me: self.id(), token: r.serve_relay_token.clone(), admission: self.admission.clone() };
        let server = net::relay::serve(r, addr, gate).await.with_context(|| match r.serve_relay {
            Some(_) => format!("serve the relay on {addr}"),
            None => format!(
                "serve the devtunnel mesh relay on {addr}: the port may be in use or reserved; set another mesh port"
            ),
        })?;
        if let Some(service) = server.relay_service() {
            self.admission.set_relay_clients(service.clients().clone());
        }
        if r.devtunnel_mesh
            && r.mesh_port.is_none()
            && r.serve_relay.is_none()
            && let Some(a) = server.http_addr()
        {
            let _ = std::fs::write(self.store.dir().join(MESH_PORT_FILE), a.port().to_string());
        }
        let how = r.expose_relay.clone().or(r.devtunnel_mesh.then_some(net::Expose::DevTunnel { id: None }));
        let tunnel = how.map(|how| {
            let port = server.https_addr().or(server.http_addr()).map_or(addr.port(), |a| a.port());
            net::devtunnel::expose(
                how,
                port,
                self.id(),
                self.name().to_string(),
                net::devtunnel::Cli::owned(r.devtunnel_cli.clone(), &self.config.data_dir),
                self.exposed.clone(),
                self.wake.subscribe(),
            )
        });
        Ok((Some(server), tunnel))
    }

    /// Learns member addresses, enforces removal, follows the cluster relays, and signs where
    /// this machine is when that changes.
    pub(super) async fn background(self) {
        let mut changes = self.store.subscribe();
        let mut addr = self.endpoint.watch_addr();
        let mut exposed = self.exposed.subscribe();
        let mut mesh_changed = self.mesh.subscribe();
        let mut known: BTreeMap<EndpointId, EndpointAddr> = BTreeMap::new();
        loop {
            let snap = self.store.snapshot();
            let forwarded = self.mesh.ready_urls().into_iter().map(|(id, _)| id);
            let ids: BTreeSet<EndpointId> = snap.machines.keys().copied().chain(forwarded).collect();
            for id in ids.into_iter().filter(|id| *id != self.id()) {
                let addr = self.peer_addr(id);
                if known.get(&id) != Some(&addr) {
                    self.lookup.set_endpoint_info(addr.clone());
                    known.insert(id, addr);
                }
            }
            self.admission.enforce();
            if self.config.net.devtunnel_mesh {
                self.reconcile_mesh(&snap);
            }
            self.apply_cluster_relays(&snap).await;
            if snap.cluster.is_some() {
                self.publish_self().await;
                self.bound_unbounded(&snap);
            }
            tokio::select! {
                _ = changes.changed() => {}
                _ = addr.updated() => tokio::time::sleep(ADDR_SETTLE).await,
                _ = exposed.changed() => {}
                _ = mesh_changed.changed() => {}
            }
        }
    }

    /// Makes the relay map equal to the cluster's relay setting. The setting replaces the relays
    /// the machine started with. With no setting, the machine uses those again, plus the relays
    /// of a join in progress.
    pub(crate) async fn apply_cluster_relays(&self, snap: &Snapshot) {
        let set = snap.relay_settings();
        let mut want = if set.relays.is_empty() {
            let mut want = self.initial_relays.clone();
            want.extend(self.joining.lock().unwrap().clone());
            want
        } else {
            net::relays(&set.relays, &set.token)
        };
        // The mesh relays come on top: this machine's own, and each member's forwarded one. With
        // all of them in the set, a relay that goes down does not cut the machine off: iroh moves
        // its home to another one.
        if let Some(h) = self.exposed.borrow().as_ref() {
            want.insert(h.url.clone(), None);
        }
        for (_, url) in self.mesh.ready_urls() {
            want.insert(url, None);
        }
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

    /// On the owner: removes former owners and admins that left, keeping what they decided until
    /// now. Until then their stolen keys could sign decisions with an old time.
    fn bound_unbounded(&self, snap: &Snapshot) {
        if snap.role(&self.id()) != Some(crate::roster::Role::Owner) || self.role().is_none() {
            return;
        }
        for id in &snap.unbounded {
            match self.store.remove(self.endpoint.secret_key(), *id, false) {
                Ok(()) => info!(machine = %id.fmt_short(), "removed a former owner or an admin that left"),
                Err(e) => warn!(machine = %id.fmt_short(), "cannot remove a former owner or an admin that left: {e:#}"),
            }
        }
    }

    /// Signs where this machine is and what it runs, when that differs from the roster.
    pub(crate) async fn publish_self(&self) {
        let me = self.id();
        let info = self.info();
        // The roster keeps at most MAX_ADDRS addresses, so compare with what it would keep.
        let info = MachineInfo { addr: Location::new(info.addr).addr, ..info };
        let (member, same) = self.store.read(|s| {
            let now = s.machine(&me).map(|m| MachineInfo { name: info.name.clone(), ..m });
            (s.is_member(&me), now.as_ref() == Some(&info))
        });
        if !member || same {
            return;
        }
        let mut loc = Location::new(info.addr);
        loc.version = info.version;
        loc.os = info.os;
        loc.arch = info.arch;
        loc.relay = info.relay;
        loc.tunnel = info.tunnel;
        loc.labels = info.labels;
        if let Err(e) = self.store.sign(self.endpoint.secret_key(), Change::Locate(loc)) {
            warn!("publish self: {e:#}");
        }
    }

    /// Where member `id` is: its location in the roster, and also this machine's own relays and
    /// the cluster relays. A member whose relay changed signs its new place, but a machine that
    /// reaches it only through a relay cannot learn that place until they connect. Members that
    /// share a relay reach each other through it.
    ///
    /// A loopback relay in a location names a relay on the machine that signed it. In the
    /// devtunnel mesh this machine reaches it through its own forward of that member's tunnel.
    /// Else the URL stays only when this machine uses it too, as with a shared tunnel.
    pub(crate) fn peer_addr(&self, id: EndpointId) -> EndpointAddr {
        let known = self.store.read(|s| s.machine(&id).map(|i| i.addr));
        self.reachable(known.unwrap_or_else(|| id.into()))
    }

    /// `addr`, which another machine published, as this machine can use it. See
    /// [`Self::peer_addr`].
    pub(crate) fn reachable(&self, addr: EndpointAddr) -> EndpointAddr {
        let cluster = self.store.read(|s| s.relays.relays.clone());
        let shared = |u: &iroh::RelayUrl| {
            !self.config.net.devtunnel_mesh && (self.initial_relays.contains_key(u) || cluster.contains(u))
        };
        let id = addr.id;
        let addrs: Vec<iroh::TransportAddr> = addr
            .addrs
            .into_iter()
            .filter(|a| !matches!(a, iroh::TransportAddr::Relay(u) if loopback(u) && !shared(u)))
            .collect();
        let mine: Vec<iroh::RelayUrl> = self.endpoint.addr().relay_urls().cloned().collect();
        // Every relay this machine reaches, not only the peer's own: the peer is a client of one of
        // them, and which one changes when a relay goes down. A URL here names a relay by this
        // machine's own forward, so it is no use to another machine.
        let forwarded = self.mesh.ready_urls().into_iter().map(|(_, url)| url);
        let relays = mine.into_iter().chain(cluster).chain(forwarded).map(iroh::TransportAddr::Relay);
        EndpointAddr::from_parts(id, addrs.into_iter().chain(relays))
    }

    /// Serves one API call. A call for another machine goes one hop over iroh. A member never
    /// forwards a call that another member sent, so calls cannot loop.
    pub(super) async fn serve_api(&self, mut s: api::Stream, caller: Caller) -> Result<()> {
        let Some(req) = api::recv::<Request>(&mut s).await? else {
            return Ok(());
        };
        if req.method == api::OPEN_METHOD {
            if req.to.is_some() {
                return api::send(&mut s, &Reply::Err("open takes no `to`".into())).await;
            }
            return self.serve_open(s, caller, req.params).await;
        }
        if let Some(to) = req.to
            && to != self.id()
        {
            if let Caller::Member(_) = caller {
                return api::send(&mut s, &Reply::Err("forwarding is limited to one hop".into())).await;
            }
            return self.forward(s, to, req).await;
        }
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

    /// `open`: after an Ok reply, the stream carries raw bytes to a new stream on `alpn` of
    /// member `to`, opened with this machine's key. Only the local API may ask, so the machine
    /// acts for its own user and never relays for a remote caller.
    async fn serve_open(&self, mut s: api::Stream, caller: Caller, params: serde_json::Value) -> Result<()> {
        #[derive(serde::Deserialize)]
        struct Open {
            to: EndpointId,
            alpn: String,
        }
        let r = async {
            ensure!(caller == Caller::Local, "open is for the local API only");
            let p: Open = api::params(params)?;
            ensure!(self.openable.contains(p.alpn.as_bytes()), "this machine does not open {:?} streams", p.alpn);
            ensure!(p.to != self.id(), "open a stream to another member");
            let conn = self.connect(p.to, p.alpn.as_bytes()).await?;
            let (send, recv) = conn.open_bi().await?;
            anyhow::Ok((conn, tokio::io::join(recv, send)))
        };
        let (conn, mut remote) = match r.await {
            Ok(r) => r,
            Err(e) => return api::send(&mut s, &Reply::Err(format!("{e:#}"))).await,
        };
        api::send(&mut s, &Reply::Ok(serde_json::Value::Null)).await?;
        let parts = s.into_parts();
        let mut local = parts.io;
        if !parts.read_buf.is_empty() {
            tokio::io::AsyncWriteExt::write_all(&mut remote, &parts.read_buf).await?;
        }
        let _ = tokio::io::copy_bidirectional(&mut local, &mut remote).await;
        conn.close(0u32.into(), b"");
        Ok(())
    }

    pub(super) async fn forward(&self, mut client: api::Stream, to: EndpointId, mut req: Request) -> Result<()> {
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

impl Machine {
    /// This machine's mesh endpoint, once its tunnel host is ready.
    pub(crate) fn tunnel_endpoint(&self) -> Option<crate::state::TunnelEndpoint> {
        let h = self.exposed.borrow();
        let h = h.as_ref()?;
        Some(crate::state::TunnelEndpoint::Devtunnel { id: h.tunnel.clone(), port: h.url.port()? })
    }

    /// Forwards one member's tunnel now, before the member's row arrives. A join uses it for the
    /// members in its ticket, and an admission for the machine it admits.
    pub(crate) fn forward_tunnel(&self, id: EndpointId, endpoint: crate::state::TunnelEndpoint) {
        if self.config.net.devtunnel_mesh && id != self.id() {
            self.mesh.ensure(id, endpoint);
        }
    }

    /// Waits up to a minute until a forward of the ticket's tunnels runs, so the first dial has a
    /// relay. Joining continues either way; links retry.
    pub(crate) async fn wait_mesh_ready(&self, tunnels: &[(EndpointId, crate::state::TunnelEndpoint)]) {
        let mut changes = self.mesh.subscribe();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            if tunnels.iter().any(|(id, _)| self.mesh.status(id).is_some_and(|(_, ready)| ready)) {
                self.apply_cluster_relays(&self.store.snapshot()).await;
                return;
            }
            if tokio::time::timeout_at(deadline, changes.changed()).await.is_err() {
                warn!("no member's mesh relay was ready within 60 s; dialing anyway");
                return;
            }
        }
    }

    /// Forwards every member that publishes a tunnel, and stops forwarding the others. Pinned
    /// forwards, of a machine being admitted or of a join ticket, stay until their row arrives
    /// or the join ends.
    fn reconcile_mesh(&self, snap: &Snapshot) {
        let me = self.id();
        let want: BTreeMap<EndpointId, crate::state::TunnelEndpoint> = snap
            .machines
            .iter()
            .filter(|(id, _)| **id != me)
            .filter_map(|(id, m)| Some((*id, m.tunnel.clone()?)))
            .collect();
        self.mesh.reconcile(&want);
    }
}

fn loopback(url: &iroh::RelayUrl) -> bool {
    url.host_str().is_some_and(|h| {
        h == "localhost" || h.trim_matches(['[', ']']).parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
    })
}
