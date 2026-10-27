//! The machine daemon: one iroh endpoint, the local API, the cluster document, and
//! liveness links to every member.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use futures_util::{SinkExt, StreamExt};
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl, Watcher,
    address_lookup::MemoryLookup,
    endpoint::{AfterHandshakeOutcome, Connection, EndpointHooks, WeakConnectionHandle, presets},
    protocol::{AcceptError, ProtocolHandler, Router},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::{
    api::{self, API_ALPN, LIVE_ALPN, Reply, Request},
    state::{ClusterInfo, MachineInfo, MemberInfo, State, View},
};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, clap::Args)]
pub struct Opts {
    /// Where the machine key and state live.
    #[arg(long, env = "IMESH_DATA_DIR", default_value_os_t = crate::local::default_data_dir())]
    pub data_dir: PathBuf,
    /// Local API: a unix socket, or a named pipe on Windows.
    #[arg(long, env = "IMESH_SOCKET", default_value = crate::local::DEFAULT_PATH)]
    pub socket: PathBuf,
    /// Machine name. Defaults to the hostname.
    #[arg(long, env = "IMESH_NAME")]
    pub name: Option<String>,
    /// Labels for this machine, as key=value. Metadata for listing and selection.
    #[arg(long = "label", env = "IMESH_LABELS", value_delimiter = ',', value_parser = parse_label)]
    pub labels: Vec<(String, String)>,
    #[command(flatten)]
    pub relay: crate::relay::Opts,
}

fn parse_label(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| format!("label {s:?} must be key=value"))
}

type MethodFuture = Pin<Box<dyn Future<Output = Result<Value>> + Send>>;
/// A method on the API. Apps add their own with [`Builder::method`].
pub type Method = Arc<dyn Fn(Machine, Value) -> MethodFuture + Send + Sync>;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PeerStatus {
    pub state: String,
    pub path: Option<String>,
    pub rtt_ms: Option<u64>,
    pub last_seen: Option<u64>,
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct Machine(Arc<Inner>);

struct Inner {
    endpoint: Endpoint,
    state: Arc<State>,
    lookup: MemoryLookup,
    admission: Admission,
    name: String,
    opts: Opts,
    methods: BTreeMap<String, Method>,
    peers: Mutex<BTreeMap<EndpointId, PeerStatus>>,
    links: Mutex<BTreeMap<EndpointId, JoinHandle<()>>>,
    /// Public URL of the relay this machine serves, once a tunnel exposes it.
    exposed: tokio::sync::watch::Sender<Option<RelayUrl>>,
    /// The relays the endpoint uses now. iroh has no getter for its relay map.
    relays: tokio::sync::Mutex<BTreeSet<RelayUrl>>,
}

/// Admits members on every ALPN, and authorized CLI keys on the API ALPN.
/// This covers the docs, gossip and blobs ALPNs, because the hook runs for the whole endpoint.
#[derive(Debug, Clone, Default)]
struct Admission {
    view: Arc<RwLock<View>>,
    /// Peers named in `cluster.join`, trusted until the join sees our own member entry.
    bootstrap: Arc<RwLock<BTreeSet<EndpointId>>>,
    /// Accepted connections, so they can be closed when their peer is removed.
    conns: Arc<Mutex<Vec<(EndpointId, Vec<u8>, WeakConnectionHandle)>>>,
}

impl Admission {
    fn allows(&self, id: &EndpointId, alpn: &[u8]) -> bool {
        let v = self.view.read().unwrap();
        v.members.contains(id)
            || self.bootstrap.read().unwrap().contains(id)
            || (alpn == API_ALPN && v.clients.contains(id))
    }

    /// Closes connections whose peer is no longer allowed. The hook only runs at the handshake,
    /// and gossip and docs keep their connections open.
    fn enforce(&self) {
        let mut conns = self.conns.lock().unwrap();
        conns.retain(|(id, alpn, h)| {
            let Some(conn) = h.upgrade() else { return false };
            if self.allows(id, alpn) {
                return true;
            }
            info!(peer = %id.fmt_short(), "closing connection of removed peer");
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
            conns.retain(|c| c.2.upgrade().is_some());
            conns.push((id, conn.alpn().to_vec(), conn.weak_handle()));
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

pub struct Builder {
    opts: Opts,
    methods: BTreeMap<String, Method>,
    protocols: Vec<(Vec<u8>, Box<dyn iroh::protocol::DynProtocolHandler>)>,
}

impl Builder {
    pub fn new(opts: Opts) -> Self {
        let mut b = Builder {
            opts,
            methods: BTreeMap::new(),
            protocols: Vec::new(),
        };
        builtin_methods(&mut b);
        b
    }

    /// Adds an API method, callable from the CLI and from other machines.
    pub fn method<F, Fut>(mut self, name: &str, f: F) -> Self
    where
        F: Fn(Machine, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value>> + Send + 'static,
    {
        self.methods
            .insert(name.to_string(), Arc::new(move |m, v| Box::pin(f(m, v))));
        self
    }

    /// Adds an app protocol. Only members can open it.
    pub fn protocol(mut self, alpn: &[u8], handler: impl ProtocolHandler) -> Self {
        self.protocols.push((alpn.to_vec(), Box::new(handler)));
        self
    }

    pub async fn spawn(self) -> Result<(Machine, Router)> {
        let opts = self.opts;
        std::fs::create_dir_all(&opts.data_dir)?;
        let secret = crate::config::load_or_create_key(&opts.data_dir.join("secret.key"))?;
        let admission = Admission::default();
        let lookup = MemoryLookup::new();
        let r = &opts.relay;
        let mut b = if r.no_n0 {
            Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Disabled)
        } else {
            Endpoint::builder(presets::N0)
        };
        let mut relays: BTreeSet<RelayUrl> = if r.no_n0 {
            BTreeSet::new()
        } else {
            iroh::endpoint::default_relay_mode().relay_map().urls()
        };
        if !r.relays.is_empty() {
            let map: iroh::RelayMap = r.relays.iter().map(|u| r.relay_config(u.clone())).collect();
            b = b.relay_mode(RelayMode::Custom(map));
            relays = r.relays.iter().cloned().collect();
        }
        if let Some(ca) = r.ca_tls_config()? {
            b = b.ca_tls_config(ca);
        }
        let endpoint = b
            .dns_resolver(crate::dns::OsResolver::resolver())
            .secret_key(secret.clone())
            .address_lookup(lookup.clone())
            .hooks(admission.clone())
            .bind()
            .await?;
        let state = State::open(&opts.data_dir, &secret, &endpoint, admission.view.clone()).await?;
        let name = opts.name.clone().unwrap_or_else(hostname);
        let machine = Machine(Arc::new(Inner {
            endpoint: endpoint.clone(),
            state: state.clone(),
            lookup,
            admission,
            name,
            opts: opts.clone(),
            methods: self.methods,
            peers: Default::default(),
            links: Default::default(),
            exposed: tokio::sync::watch::channel(None).0,
            relays: tokio::sync::Mutex::new(relays),
        }));
        let mut router = Router::builder(endpoint)
            .accept(API_ALPN, ApiProtocol(machine.clone()))
            .accept(LIVE_ALPN, LiveProtocol(machine.clone()))
            .accept(iroh_blobs::ALPN, iroh_blobs::BlobsProtocol::new(&state.blobs, None))
            .accept(iroh_gossip::ALPN, state.gossip.clone())
            .accept(iroh_docs::ALPN, state.docs.clone());
        for (alpn, h) in self.protocols {
            router = router.accept(alpn, h);
        }
        let router = router.spawn();
        info!(id = %machine.id(), name = %machine.0.name, "machine started");
        Ok((machine, router))
    }
}

pub async fn run(builder: Builder) -> Result<()> {
    let socket = builder.opts.socket.clone();
    let (machine, router) = builder.spawn().await?;
    let ropts = machine.0.opts.relay.clone();
    let _relay = match ropts.serve_relay {
        Some(addr) => {
            let admission = machine.0.admission.clone();
            let gate = crate::relay::Gate {
                me: machine.id(),
                token: ropts.serve_relay_token.clone(),
                allows: Box::new(move |id| admission.allows(id, API_ALPN)),
            };
            let server = crate::relay::serve(&ropts, addr, gate).await?;
            if let Some(how) = ropts.expose_relay.clone() {
                let port = server.https_addr().or(server.http_addr()).map(|a| a.port()).unwrap_or(addr.port());
                crate::relay::expose(how, port, machine.id(), machine.0.exposed.clone());
            }
            Some(server)
        }
        None => None,
    };
    let mut listener = crate::local::Listener::bind(&socket)?;
    tokio::spawn(machine.clone().background());
    tokio::spawn({
        let machine = machine.clone();
        async move {
            loop {
                match listener.accept().await {
                    Ok(s) => {
                        let m = machine.clone();
                        tokio::spawn(async move {
                            if let Err(e) = m.serve_api(api::framed(s), false).await {
                                debug!("local api: {e:#}");
                            }
                        });
                    }
                    Err(e) => warn!("accept on socket: {e}"),
                }
            }
        }
    });
    sd_notify_ready();
    tokio::signal::ctrl_c().await?;
    router.shutdown().await?;
    crate::local::Listener::remove(&socket);
    Ok(())
}

impl Machine {
    pub fn id(&self) -> EndpointId {
        self.0.endpoint.id()
    }

    /// Opens a connection to another member on any ALPN. Addresses come from the cluster state.
    pub async fn connect(&self, to: EndpointId, alpn: &[u8]) -> Result<Connection> {
        ensure!(self.0.state.members().contains(&to), "{} is not a member", to.fmt_short());
        let conn = tokio::time::timeout(Duration::from_secs(20), self.0.endpoint.connect(self.peer_addr(to).await, alpn))
            .await
            .context("connect timeout")??;
        Ok(conn)
    }

    /// Waits until the home relay is connected, so our address includes the relay URL.
    /// Without a relay this returns at once.
    async fn online(&self) {
        let relays = !(self.0.opts.relay.no_n0 && self.0.opts.relay.relays.is_empty());
        if relays {
            let _ = tokio::time::timeout(Duration::from_secs(10), self.0.endpoint.online()).await;
        }
    }

    fn info(&self) -> MachineInfo {
        MachineInfo {
            name: self.0.name.clone(),
            addr: self.0.endpoint.addr(),
            version: VERSION.to_string(),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            relay: self.0.exposed.borrow().clone(),
            labels: self.0.opts.labels.iter().cloned().collect(),
        }
    }

    /// Publishes our own record when it changes, keeps peer links, learns peer addresses,
    /// and closes connections of removed machines.
    async fn background(self) {
        let mut changes = self.0.state.subscribe();
        let mut addr = self.0.endpoint.watch_addr();
        let mut tick = tokio::time::interval(Duration::from_secs(10));
        let mut exposed = self.0.exposed.subscribe();
        let state = self.0.state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            loop {
                tick.tick().await;
                if let Err(e) = state.repair().await {
                    warn!("repair content: {e:#}");
                }
            }
        });
        loop {
            match self.0.state.list::<MachineInfo>("machine/").await {
                Ok(recs) => {
                    for info in recs.into_iter().filter_map(|r| r.1) {
                        self.0.lookup.add_endpoint_info(info.addr);
                    }
                }
                Err(e) => warn!("list machines: {e:#}"),
            }
            self.apply_cluster_relays().await;
            if self.0.state.cluster().is_some() {
                self.publish_self().await;
                self.reconcile_links();
                self.0.admission.enforce();
            }
            tokio::select! {
                _ = changes.recv() => {}
                _ = addr.updated() => {}
                _ = exposed.changed() => {}
                _ = tick.tick() => {}
            }
        }
    }

    /// Makes the relay map equal to the cluster's `settings/relays` plus this machine's
    /// `--relay` list. Every member then shares a relay that all of them can reach.
    async fn apply_cluster_relays(&self) {
        let Ok(Some(set)) = self.0.state.get::<RelaySettings>("settings/relays").await else {
            return;
        };
        if set.relays.is_empty() {
            return;
        }
        let mut want: BTreeSet<RelayUrl> = set.relays.into_iter().collect();
        want.extend(self.0.opts.relay.relays.iter().cloned());
        let mut have = self.0.relays.lock().await;
        for url in want.difference(&have) {
            info!(%url, "using cluster relay");
            let cfg = Arc::new(self.0.opts.relay.relay_config(url.clone()));
            self.0.endpoint.insert_relay(url.clone(), cfg).await;
        }
        for url in have.difference(&want) {
            info!(%url, "dropping relay not in the cluster settings");
            self.0.endpoint.remove_relay(url).await;
        }
        *have = want;
    }

    async fn publish_self(&self) {
        if !self.0.state.members().contains(&self.id()) {
            return;
        }
        let key = format!("machine/{}", self.id());
        let info = self.info();
        let cur = self.0.state.get::<MachineInfo>(&key).await.ok().flatten();
        let same = cur.is_some_and(|cur| serde_json::to_value(&cur).ok() == serde_json::to_value(&info).ok());
        if !same && let Err(e) = self.0.state.put(&key, Some(&info)).await {
            warn!("publish self: {e:#}");
        }
    }

    fn reconcile_links(&self) {
        let members = self.0.state.members();
        let me = self.id();
        let mut links = self.0.links.lock().unwrap();
        links.retain(|id, h| {
            let keep = members.contains(id) && !h.is_finished();
            if !keep {
                h.abort();
                self.0.peers.lock().unwrap().remove(id);
            }
            keep
        });
        for id in members {
            if id != me && !links.contains_key(&id) {
                links.insert(id, tokio::spawn(self.clone().link(id)));
            }
        }
    }

    async fn peer_addr(&self, id: EndpointId) -> EndpointAddr {
        self.0
            .state
            .get::<MachineInfo>(&format!("machine/{id}"))
            .await
            .ok()
            .flatten()
            .map(|i| i.addr)
            .unwrap_or_else(|| id.into())
    }

    fn set_peer(&self, id: EndpointId, f: impl FnOnce(&mut PeerStatus)) {
        f(self.0.peers.lock().unwrap().entry(id).or_default());
    }

    /// Keeps one liveness connection to a member, and resyncs the document with it.
    async fn link(self, peer: EndpointId) {
        let mut backoff = Duration::from_secs(1);
        loop {
            match self.link_once(peer).await {
                Ok(()) => backoff = Duration::from_secs(1),
                Err(e) => {
                    debug!(peer = %peer.fmt_short(), "link: {e:#}");
                    self.set_peer(peer, |s| {
                        s.state = "down".into();
                        s.path = None;
                        s.error = Some(format!("{e:#}"));
                    });
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(10));
        }
    }

    async fn link_once(&self, peer: EndpointId) -> Result<()> {
        let addr = self.peer_addr(peer).await;
        let conn = tokio::time::timeout(Duration::from_secs(20), self.0.endpoint.connect(addr.clone(), LIVE_ALPN))
            .await
            .context("connect timeout")??;
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        let mut n = 0u64;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    tokio::time::timeout(Duration::from_secs(10), ping(&conn))
                        .await
                        .context("peer did not answer within 10s")??;
                    self.observe(peer, &conn);
                    // iroh-docs has no resync timer. Sync on every (re)connect and every 30 s.
                    if n % 6 == 0 && let Err(e) = self.0.state.sync_with(vec![addr.clone()]).await {
                        debug!(peer = %peer.fmt_short(), "resync: {e:#}");
                    }
                    n += 1;
                }
                _ = conn.closed() => bail!("connection closed"),
            }
        }
    }

    fn observe(&self, peer: EndpointId, conn: &Connection) {
        let paths = conn.paths();
        let selected = paths.iter().find(|p| p.is_selected());
        let path = selected.as_ref().map(|p| if p.is_relay() { "relay" } else { "direct" }.to_string());
        let rtt = selected.as_ref().map(|p| p.rtt().as_millis() as u64);
        self.set_peer(peer, |s| {
            s.state = "up".into();
            s.path = path;
            s.rtt_ms = rtt;
            s.last_seen = Some(now_secs());
            s.error = None;
        });
    }

    /// Serves one API call. Calls for another machine go one hop over iroh.
    async fn serve_api(&self, mut s: api::Stream, from_iroh: bool) -> Result<()> {
        let Some(req) = api::recv::<Request>(&mut s).await? else {
            return Ok(());
        };
        if let Some(to) = req.to
            && to != self.id()
        {
            if from_iroh {
                api::send(&mut s, &Reply::Err("forwarding is limited to one hop".into())).await?;
                return Ok(());
            }
            return self.forward(s, to, req).await;
        }
        let reply = match self.0.methods.get(&req.method) {
            Some(m) => match m(self.clone(), req.params).await {
                Ok(v) => Reply::Ok(v),
                Err(e) => Reply::Err(format!("{e:#}")),
            },
            None => Reply::Err(format!("unknown method {:?}", req.method)),
        };
        api::send(&mut s, &reply).await
    }

    async fn forward(&self, mut client: api::Stream, to: EndpointId, mut req: Request) -> Result<()> {
        let remote = async {
            let conn = self.0.endpoint.connect(self.peer_addr(to).await, API_ALPN).await?;
            let (send, recv) = conn.open_bi().await?;
            anyhow::Ok((conn, api::framed(tokio::io::join(recv, send))))
        };
        let (_conn, mut remote) = match tokio::time::timeout(Duration::from_secs(20), remote).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return api::send(&mut client, &Reply::Err(format!("reach {}: {e:#}", to.fmt_short()))).await,
            Err(_) => return api::send(&mut client, &Reply::Err(format!("reach {}: timeout", to.fmt_short()))).await,
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

    fn peer_status(&self, id: &EndpointId) -> PeerStatus {
        if *id == self.id() {
            return PeerStatus {
                state: "up".into(),
                path: Some("self".into()),
                ..Default::default()
            };
        }
        self.0
            .peers
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .unwrap_or(PeerStatus {
                state: "unknown".into(),
                ..Default::default()
            })
    }
}

async fn ping(conn: &Connection) -> Result<()> {
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(&[1]).await?;
    send.finish()?;
    recv.read_to_end(8).await?;
    Ok(())
}

/// Answers liveness pings from members.
#[derive(Debug, Clone)]
struct LiveProtocol(Machine);

impl ProtocolHandler for LiveProtocol {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let peer = conn.remote_id();
        loop {
            let (mut send, mut recv) = conn.accept_bi().await?;
            let pong = async {
                recv.read_to_end(8).await?;
                send.write_all(&[1]).await?;
                send.finish()?;
                anyhow::Ok(())
            };
            if let Err(e) = pong.await {
                debug!(peer = %peer.fmt_short(), "pong: {e:#}");
            }
            if !self.0.0.admission.allows(&peer, LIVE_ALPN) {
                conn.close(403u32.into(), b"removed from cluster");
                return Ok(());
            }
        }
    }
}

#[derive(Debug, Clone)]
struct ApiProtocol(Machine);

impl std::fmt::Debug for Machine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Machine({})", self.id().fmt_short())
    }
}

impl ProtocolHandler for ApiProtocol {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        loop {
            let (send, recv) = conn.accept_bi().await?;
            let m = self.0.clone();
            tokio::spawn(async move {
                if let Err(e) = m.serve_api(api::framed(tokio::io::join(recv, send)), true).await {
                    debug!("iroh api: {e:#}");
                }
            });
        }
    }
}

#[derive(Deserialize)]
struct InitParams {
    name: Option<String>,
}

#[derive(Deserialize)]
struct AddParams {
    id: EndpointId,
    name: String,
}

#[derive(Deserialize)]
struct JoinParams {
    cluster: String,
    founder: EndpointId,
    /// Hex of the namespace secret.
    secret: String,
    peers: Vec<EndpointAddr>,
}

#[derive(Deserialize)]
struct IdParams {
    id: EndpointId,
}

#[derive(Deserialize)]
struct ClientParams {
    key: EndpointId,
    name: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RelaySettings {
    relays: Vec<RelayUrl>,
}

#[derive(Deserialize)]
struct FillParams {
    prefix: String,
    #[serde(default)]
    count: usize,
    #[serde(default)]
    bytes: usize,
}

#[derive(Deserialize)]
struct KvParams {
    key: String,
    #[serde(default)]
    value: Option<Value>,
}

fn builtin_methods(b: &mut Builder) {
    let mut add = |name: &str, f: fn(Machine, Value) -> MethodFuture| {
        b.methods.insert(name.to_string(), Arc::new(f));
    };
    add("ping", |_, _| Box::pin(async { Ok(json!("pong")) }));
    add("methods", |m, _| {
        Box::pin(async move { Ok(json!(m.0.methods.keys().collect::<Vec<_>>())) })
    });
    add("machine.inspect", |m, _| {
        Box::pin(async move {
            Ok(json!({
                "id": m.id(),
                "info": m.info(),
                "cluster": m.0.state.cluster().map(|c| c.id),
                "member": m.0.state.members().contains(&m.id()),
                "relays": m.0.opts.relay.relays,
                "relay_token": m.0.opts.relay.relay_token,
                "no_n0": m.0.opts.relay.no_n0,
            }))
        })
    });
    add("cluster.init", |m, p| {
        Box::pin(async move {
            let p: InitParams = api::params(p)?;
            ensure!(m.0.state.cluster().is_none(), "machine is already in a cluster");
            let st = &m.0.state;
            let c = st.init(m.id()).await?;
            let name = p.name.unwrap_or_else(|| m.0.name.clone());
            st.put(&format!("member/{}", m.id()), Some(&MemberInfo { name })).await?;
            st.refresh().await?;
            m.online().await;
            m.publish_self().await;
            Ok(json!({ "cluster": c.id, "id": m.id() }))
        })
    });
    add("cluster.add", |m, p| {
        Box::pin(async move {
            let p: AddParams = api::params(p)?;
            let st = &m.0.state;
            let cluster = st.cluster().context("machine is not in a cluster")?;
            ensure!(
                !st.view().removed.contains(&p.id),
                "machine {} was removed; reset it to get a new identity",
                p.id.fmt_short()
            );
            ensure!(
                !member_names(&m).await?.iter().any(|(id, n)| *n == p.name && *id != p.id),
                "name {:?} is taken",
                p.name
            );
            st.put(&format!("member/{}", p.id), Some(&MemberInfo { name: p.name })).await?;
            st.refresh().await?;
            m.online().await;
            let mut peers = vec![m.0.endpoint.addr()];
            for id in st.members() {
                if id != m.id() && id != p.id {
                    peers.push(m.peer_addr(id).await);
                }
            }
            // The write capability of the whole document travels through the CLI to the new machine.
            let secret = hex::encode(st.secret().await?);
            Ok(json!({ "cluster": cluster.id, "founder": cluster.founder, "secret": secret, "peers": peers }))
        })
    });
    add("cluster.join", |m, p| {
        Box::pin(async move {
            let p: JoinParams = api::params(p)?;
            let st = &m.0.state;
            if let Some(c) = st.cluster() {
                ensure!(c.id == p.cluster, "machine is already in cluster {}", c.id);
            }
            let secret: [u8; 32] = hex::decode(&p.secret)?.try_into().ok().context("bad secret")?;
            m.online().await;
            *m.0.admission.bootstrap.write().unwrap() = p.peers.iter().map(|a| a.id).collect();
            for addr in &p.peers {
                m.0.lookup.add_endpoint_info(addr.clone());
            }
            let info = ClusterInfo { id: p.cluster.clone(), founder: p.founder };
            let joined = async {
                st.join(info, secret, p.peers.clone()).await?;
                for _ in 0..120 {
                    st.refresh().await?;
                    let v = st.view();
                    if v.founded && v.members.contains(&m.id()) {
                        return Ok(());
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Err(anyhow!("peers have not admitted us within 30s"))
            }
            .await;
            m.0.admission.bootstrap.write().unwrap().clear();
            joined?;
            m.publish_self().await;
            m.reconcile_links();
            Ok(json!({ "cluster": p.cluster, "members": st.members().len() }))
        })
    });
    add("cluster.remove", |m, p| {
        Box::pin(async move {
            let p: IdParams = api::params(p)?;
            ensure!(p.id != m.id(), "a machine cannot remove itself; connect through another machine");
            ensure!(m.0.state.members().contains(&p.id), "not a member");
            m.0.state.put::<()>(&format!("member/{}", p.id), None).await?;
            m.0.state.refresh().await?;
            m.0.admission.enforce();
            Ok(json!(null))
        })
    });
    add("cluster.machines", |m, _| {
        Box::pin(async move {
            let names = member_names(&m).await?;
            let mut rows = Vec::new();
            for (id, name) in names {
                let info = m.0.state.get::<MachineInfo>(&format!("machine/{id}")).await?;
                rows.push(json!({ "id": id, "name": name, "info": info, "status": m.peer_status(&id) }));
            }
            Ok(json!(rows))
        })
    });
    add("client.add", |m, p| {
        Box::pin(async move {
            let p: ClientParams = api::params(p)?;
            ensure!(m.0.state.cluster().is_some(), "machine is not in a cluster");
            m.0.state.put(&format!("client/{}", p.key), Some(&json!({ "name": p.name }))).await?;
            m.0.state.refresh().await?;
            Ok(json!(null))
        })
    });
    add("machine.reset", |m, _| {
        Box::pin(async move {
            let dir = m.0.state.dir().to_path_buf();
            let socket = m.0.opts.socket.clone();
            tokio::spawn(async move {
                // Let the reply reach the caller, then forget everything. systemd starts a fresh daemon.
                tokio::time::sleep(Duration::from_millis(200)).await;
                crate::local::Listener::remove(&socket);
                for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                    let p = e.path();
                    let _ = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
                }
                // Nonzero, so a Windows scheduled task restarts it too.
                std::process::exit(75);
            });
            Ok(json!(null))
        })
    });
    add("relays.get", |m, _| {
        Box::pin(async move {
            let set = m.0.state.get::<RelaySettings>("settings/relays").await?.unwrap_or_default();
            let served: Vec<_> = m
                .0
                .state
                .list::<MachineInfo>("machine/")
                .await?
                .into_iter()
                .filter_map(|(_, i)| i)
                .filter_map(|i| i.relay.map(|u| json!({ "machine": i.name, "url": u })))
                .collect();
            Ok(json!({ "cluster": set.relays, "served": served, "in_use": *m.0.relays.lock().await }))
        })
    });
    add("relays.set", |m, p| {
        Box::pin(async move {
            let set: RelaySettings = api::params(p)?;
            m.0.state.put("settings/relays", Some(&set)).await?;
            Ok(json!(null))
        })
    });
    add("kv.put", |m, p| {
        Box::pin(async move {
            let p: KvParams = api::params(p)?;
            let version = m.0.state.put(&format!("app/{}", p.key), p.value.as_ref()).await?;
            Ok(json!({ "version": version }))
        })
    });
    add("kv.get", |m, p| {
        Box::pin(async move {
            let p: KvParams = api::params(p)?;
            Ok(json!(m.0.state.get::<Value>(&format!("app/{}", p.key)).await?))
        })
    });
    add("kv.fill", |m, p| {
        Box::pin(async move {
            let p: FillParams = api::params(p)?;
            for i in 0..p.count {
                let value = format!("{i:06}{}", "x".repeat(p.bytes.saturating_sub(6)));
                m.0.state.put(&format!("app/{}/{i:06}", p.prefix), Some(&value)).await?;
            }
            Ok(json!(p.count))
        })
    });
    add("kv.count", |m, p| {
        Box::pin(async move {
            let p: FillParams = api::params(p)?;
            Ok(json!(m.0.state.list_entries(&format!("app/{}/", p.prefix)).await?.len()))
        })
    });
    add("kv.list", |m, _| {
        Box::pin(async move {
            let out: BTreeMap<_, _> = m
                .0
                .state
                .list::<Value>("app/")
                .await?
                .into_iter()
                .map(|(k, v)| (k["app/".len()..].to_string(), v))
                .collect();
            Ok(json!(out))
        })
    });
}

async fn member_names(m: &Machine) -> Result<BTreeMap<EndpointId, String>> {
    let members = m.0.state.members();
    let names: BTreeMap<String, Option<MemberInfo>> = m.0.state.list("member/").await?.into_iter().collect();
    Ok(members
        .into_iter()
        .map(|id| {
            let name = names.get(&format!("member/{id}")).cloned().flatten().map(|i| i.name).unwrap_or_default();
            (id, name)
        })
        .collect())
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| std::env::var("COMPUTERNAME").ok().map(|s| s.to_lowercase()))
        .unwrap_or_else(|| "machine".into())
}

#[cfg(not(unix))]
fn sd_notify_ready() {}

#[cfg(unix)]
fn sd_notify_ready() {
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    let sock = match std::os::unix::net::UnixDatagram::unbound() {
        Ok(s) => s,
        Err(_) => return,
    };
    let path = path.to_string_lossy().into_owned();
    #[cfg(target_os = "linux")]
    if let Some(name) = path.strip_prefix('@') {
        use std::os::linux::net::SocketAddrExt;
        if let Ok(addr) = std::os::unix::net::SocketAddr::from_abstract_name(name) {
            let _ = sock.send_to_addr(b"READY=1", &addr);
        }
        return;
    }
    let _ = sock.send_to(b"READY=1", path);
}

/// Bridges stdin and stdout to the local API. The CLI runs this over SSH.
pub async fn dial_stdio(socket: PathBuf) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let s = crate::local::connect(&socket).await?;
    let (mut r, mut w) = tokio::io::split(s);
    let up = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut tokio::io::stdin(), &mut w).await;
        let _ = w.shutdown().await;
    });
    let mut stdout = tokio::io::stdout();
    tokio::io::copy(&mut r, &mut stdout).await?;
    stdout.flush().await?;
    up.abort();
    Ok(())
}
