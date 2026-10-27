//! The machine daemon: one iroh endpoint, the local API, and one link to every other member.

mod admission;
mod cluster;
mod links;
mod machine;
mod mesh;
pub(crate) mod methods;

use std::{
    collections::BTreeMap,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

use anyhow::{Context, Result};
use iroh::{
    address_lookup::MemoryLookup,
    protocol::{ProtocolHandler, Router},
};
use n0_future::task::AbortOnDropHandle;
use serde_json::Value;
use tracing::{info, warn};

pub(crate) use admission::Admission;
pub use admission::Caller;
pub use cluster::valid_name as valid_member_name;
use links::CorroProtocol;
pub use links::{CORRO_ALPN, PeerState, PeerStatus};
use machine::ApiProtocol;
pub use machine::{Inner, Machine};

pub use crate::net::local::Access;
use crate::{
    api::API_ALPN,
    net,
    state::{Store, slot, store_dir, tunnel::Tunnel},
};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const KEY_FILE: &str = "secret.key";

/// What a machine needs to start. Only `data_dir` has no default: see [`Config::new`].
#[derive(Debug, Clone)]
pub struct Config {
    /// Where the machine key and database live. Only one machine at a time may use it.
    pub data_dir: PathBuf,
    /// Machine name. Defaults to the hostname.
    pub name: Option<String>,
    /// Labels, metadata for listing and selection. Other members see them.
    pub labels: BTreeMap<String, String>,
    pub net: net::Config,
}

impl Config {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Config { data_dir: data_dir.into(), name: None, labels: BTreeMap::new(), net: net::Config::default() }
    }
}

/// Where [`serve`] opens the local API.
#[derive(Debug, Clone)]
pub struct LocalApi {
    /// A unix socket path, or a named pipe (`\\.\pipe\<name>`) on Windows.
    pub path: PathBuf,
    pub access: Access,
    /// Tell systemd (`Type=notify`) when the machine and its local API are up.
    pub notify_systemd: bool,
}

type MethodFuture = Pin<Box<dyn Future<Output = Result<Value>> + Send>>;
/// A method on the API. Apps add their own with [`Builder::method`].
pub(crate) type Method = Arc<dyn Fn(Machine, Caller, Value) -> MethodFuture + Send + Sync>;

type ProtocolFactory = Box<dyn FnOnce(&Machine) -> Box<dyn iroh::protocol::DynProtocolHandler> + Send>;

pub struct Builder {
    config: Config,
    methods: BTreeMap<String, Method>,
    protocols: Vec<(Vec<u8>, ProtocolFactory)>,
    openable: std::collections::BTreeSet<Vec<u8>>,
    schema: Vec<String>,
}

impl Builder {
    pub fn new(config: Config) -> Self {
        crate::daemon::methods::register(Builder {
            config,
            methods: BTreeMap::new(),
            protocols: Vec::new(),
            openable: Default::default(),
            schema: Vec::new(),
        })
    }

    /// Adds app tables: complete `CREATE TABLE` and `CREATE INDEX` statements. Every machine that
    /// runs this binary applies them at start, and refuses to start if they do not apply. A new
    /// version of the app may add tables and columns (nullable, or with a default) and indexes;
    /// it cannot drop or change them. Name app tables after the app, not `imesh_*`.
    pub fn schema(mut self, sql: &str) -> Self {
        self.schema.push(sql.to_string());
        self
    }

    /// Adds an API method, callable from the CLI and from other machines. The method gets the
    /// caller, so it can decide who may call it. A name can be registered once.
    pub fn method<F, Fut>(mut self, name: &str, f: F) -> Self
    where
        F: Fn(Machine, Caller, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value>> + Send + 'static,
    {
        assert!(!self.methods.contains_key(name), "method {name} already exists");
        self.methods.insert(name.to_string(), Arc::new(move |m, c, v| Box::pin(f(m, c, v))));
        self
    }

    /// Adds an app protocol. Only members can open it.
    pub fn protocol(self, alpn: &[u8], handler: impl ProtocolHandler) -> Self {
        self.protocol_with(alpn, move |_| handler)
    }

    /// Adds an app protocol whose handler needs the machine: its endpoint (for example for
    /// iroh-blobs or iroh-docs), its state, or its members. `make` runs after the endpoint binds
    /// and before the router starts.
    pub fn protocol_with<H: ProtocolHandler>(
        mut self,
        alpn: &[u8],
        make: impl FnOnce(&Machine) -> H + Send + 'static,
    ) -> Self {
        self.protocols.push((alpn.to_vec(), Box::new(move |m| Box::new(make(m)))));
        self
    }

    /// Lets the local API's `open` call reach `alpn` on members, as this machine. None by default:
    /// `open` gives a local caller a raw stream with this machine's identity, so allow only the
    /// app protocols a local user should drive, for example a shell protocol.
    pub fn openable(mut self, alpn: &[u8]) -> Self {
        self.openable.insert(alpn.to_vec());
        self
    }

    /// Starts the machine: the endpoint and its protocols, sync with the other members, the
    /// database, and the relay when `serve_relay` is set. It does not open the local API;
    /// [`serve`] does that. Fails if another machine uses the data directory.
    pub async fn spawn(self) -> Result<Running> {
        // The database runs blocking SQLite calls in place, which needs worker threads.
        anyhow::ensure!(
            tokio::runtime::Handle::current().runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread,
            "imesh needs a multi-thread tokio runtime"
        );
        let config = self.config;
        config.net.validate()?;
        let devtunnel_cli = config.net.devtunnel_cli.clone();
        let forwarding = match &config.net.devtunnel {
            Some(tunnel) => Some(tunnel.start(config.net.devtunnel_cli.clone()).await?),
            None => None,
        };
        crate::os::create_private_dir(&config.data_dir)?;
        let lock = lock_data_dir(&config.data_dir)?;
        let secret = crate::os::load_or_create_key(&config.data_dir.join(KEY_FILE))?;
        let me = secret.public();
        remove_old_stores(&config.data_dir, Some(&me));
        let base = config.net.gossip_port_base.unwrap_or(crate::state::GOSSIP_PORT_BASE);
        let store = Store::start(&store_dir(&config.data_dir, &me), &me, base, &self.schema).await?;
        let admission = Admission::new(store.clone());
        let lookup = MemoryLookup::new();
        let (initial, custom) = initial_relays(&config.net);
        let endpoint = bind_endpoint(&config.net, custom.then_some(&initial), secret, &lookup, &admission).await?;
        let machine = Machine(Arc::new(Inner {
            endpoint: endpoint.clone(),
            store,
            name: config.name.clone().unwrap_or_else(crate::os::hostname),
            config,
            admission,
            lookup,
            methods: self.methods,
            openable: self.openable,
            peers: Default::default(),
            incoming: Default::default(),
            tunnel: Tunnel::new(slot(base, &me)),
            stopped: Default::default(),
            exposed: tokio::sync::watch::channel(None).0,
            mesh: mesh::Mesh::new(devtunnel_cli),
            relays: tokio::sync::Mutex::new(initial.clone()),
            joining: Default::default(),
            reset: tokio::sync::watch::channel(false).0,
            reset_after_reply: Default::default(),
            initial_relays: initial,
        }));
        let mut router = Router::builder(endpoint.clone())
            .accept(API_ALPN, ApiProtocol(machine.clone()))
            .accept(CORRO_ALPN, CorroProtocol(machine.clone()));
        for (alpn, make) in self.protocols {
            router = router.accept(alpn, make(&machine));
        }
        let router = router.spawn();
        let (relay, tunnel) = machine.serve_relay().await?;
        let mut tasks = vec![AbortOnDropHandle::new(tokio::spawn(machine.clone().background()))];
        tasks.extend(tunnel);
        tasks.extend(forwarding);
        info!(id = %machine.id(), name = %machine.name, "machine started");
        Ok(Running { machine, router, _tasks: tasks, relay, _lock: lock })
    }
}

/// The relays a machine starts with, and whether they are exact: any relay option makes the map
/// exact. Else the n0 preset keeps its own relays.
fn initial_relays(net: &net::Config) -> (net::Relays, bool) {
    let exact = !net.relays.is_empty() || !net.n0;
    let urls = if exact { net.relays.clone() } else { iroh::endpoint::default_relay_mode().relay_map().urls() };
    (net::relays(&urls, &net.relay_token), exact || net.relay_token.is_some())
}

/// Binds the machine's endpoint: relays, address lookup (memory, pkarr, mDNS), and the admission
/// hook that refuses keys outside the cluster.
async fn bind_endpoint(
    net: &net::Config,
    relays: Option<&net::Relays>,
    secret: iroh::SecretKey,
    lookup: &MemoryLookup,
    admission: &Admission,
) -> Result<iroh::Endpoint> {
    let mut b = net::endpoint_builder(net.n0, relays, net.relay_ca.as_deref(), net.proxy.as_ref())?;
    if net.relay_only {
        b = b.clear_ip_transports();
    } else if let Some(addr) = net.bind {
        b = b.clear_ip_transports().bind_addr(addr).context("bind the iroh endpoint")?;
    }
    if let Some(url) = &net.pkarr {
        b = b
            .address_lookup(iroh::address_lookup::PkarrPublisher::builder(url.clone()))
            .address_lookup(iroh::address_lookup::PkarrResolver::builder(url.clone()));
    }
    let endpoint = b.secret_key(secret).address_lookup(lookup.clone()).hooks(admission.clone()).bind().await?;
    if net.mdns {
        // mDNS fails on hosts without a default route. Run without it then.
        match iroh_mdns_address_lookup::MdnsAddressLookup::builder().build(endpoint.id()) {
            Ok(m) => endpoint.address_lookup()?.add(m),
            Err(e) => warn!("mdns disabled: {e:#}"),
        }
    }
    Ok(endpoint)
}

/// A started machine. Dropping it stops its tasks. Call [`Running::shutdown`] to also close the
/// connections and stop the database agent.
pub struct Running {
    pub machine: Machine,
    router: Router,
    _tasks: Vec<AbortOnDropHandle<()>>,
    relay: Option<iroh_relay::server::Server>,
    /// Held while the machine runs, so a second one cannot use the same data directory.
    _lock: std::fs::File,
}

impl Running {
    /// Resolves when `machine.reset` was called. The owner shuts the machine down and calls
    /// [`Running::delete_data`].
    pub async fn reset_requested(&self) {
        let _ = self.machine.reset.subscribe().wait_for(|r| *r).await;
    }

    /// Stops the relay, the endpoint, and the database agent. Runs every step, and returns the
    /// first error. Committed writes are already in the database.
    pub async fn shutdown(mut self) -> Result<()> {
        self.stop().await
    }

    /// Stops everything but keeps the data directory locked until `self` drops.
    async fn stop(&mut self) -> Result<()> {
        let relay = match self.relay.take() {
            Some(relay) => relay.shutdown().await.map_err(anyhow::Error::from),
            None => Ok(()),
        };
        let router = self.router.shutdown().await.map_err(anyhow::Error::from);
        self.machine.tunnel.close();
        self.machine.mesh.clear();
        self.machine.store.shutdown().await;
        relay.and(router)
    }

    /// Deletes `<data dir>/app`, where apps keep their own files, then the key. The key goes
    /// last, so a failure leaves the old identity and the reset can run again. The database
    /// directory carries the key, so the next machine never opens it; it is removed now if it
    /// can be, and at the next start otherwise (Windows keeps open files).
    pub fn delete_data(data_dir: &Path) -> Result<()> {
        let dir = data_dir;
        let gone = |r: std::io::Result<()>, what: &Path| match r {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("delete {}", what.display()))
            }
            _ => Ok(()),
        };
        let app = dir.join("app");
        gone(std::fs::remove_dir_all(&app), &app)?;
        let key = dir.join(KEY_FILE);
        gone(std::fs::remove_file(&key), &key)?;
        remove_old_stores(dir, None);
        Ok(())
    }
}

/// Takes the data directory's lock file for this process. The OS releases it when the process
/// ends, also after a crash.
fn lock_data_dir(dir: &Path) -> Result<std::fs::File> {
    let path = dir.join("lock");
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => {
            anyhow::bail!("another machine runs with the data directory {}", dir.display())
        }
        Err(std::fs::TryLockError::Error(e)) => Err(e).with_context(|| format!("lock {}", path.display())),
    }
}

/// Removes the database directories of this machine's earlier identities: all but `keep`'s.
fn remove_old_stores(data_dir: &Path, keep: Option<&iroh::EndpointId>) {
    let keep = keep.map(|id| store_dir(data_dir, id));
    let Ok(entries) = std::fs::read_dir(data_dir) else { return };
    for e in entries.flatten() {
        let path = e.path();
        if Some(&path) != keep.as_ref() && e.file_name().to_string_lossy().starts_with("corrosion-") {
            match std::fs::remove_dir_all(&path) {
                Ok(()) => info!(path = %path.display(), "removed the database of an earlier identity"),
                Err(err) => warn!(path = %path.display(), "cannot remove old database yet: {err}"),
            }
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // Link tasks hold the machine, and the machine holds their handles. Break the cycle,
        // and start no new links.
        let mut peers = self.machine.peers.lock().unwrap();
        self.machine.stopped.store(true, std::sync::atomic::Ordering::Relaxed);
        peers.clear();
        self.machine.incoming.lock().unwrap().clear();
        self.machine.tunnel.close();
        self.machine.mesh.clear();
    }
}

/// Why [`run`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Exit {
    Shutdown,
    Reset,
}

/// Runs a machine and its local API until `shutdown` resolves. After `machine.reset` it deletes
/// the key, the database, and `<data dir>/app`, and starts again as a new machine from `make()`.
pub async fn serve(make: impl Fn() -> Builder, api: &LocalApi, shutdown: impl Future<Output = ()>) -> Result<()> {
    tokio::pin!(shutdown);
    while run(make(), api, shutdown.as_mut()).await? == Exit::Reset {
        info!("reset: starting as a new machine");
    }
    Ok(())
}

async fn run(builder: Builder, api: &LocalApi, shutdown: Pin<&mut impl Future<Output = ()>>) -> Result<Exit> {
    let mut running = builder.spawn().await?;
    let listener = match net::local::Listener::bind(&api.path, api.access.clone()) {
        Ok(l) => l,
        Err(e) => {
            let _ = running.stop().await;
            return Err(e);
        }
    };
    let (stop_local, stop) = tokio::sync::oneshot::channel();
    let local = AbortOnDropHandle::new(tokio::spawn(running.machine.clone().serve_local(listener, async move {
        let _ = stop.await;
    })));
    if api.notify_systemd {
        crate::os::sd_notify_ready();
    }
    let exit = tokio::select! {
        _ = shutdown => Exit::Shutdown,
        _ = running.reset_requested() => Exit::Reset,
    };
    // Close every local connection, so the next run can bind the pipe or socket again.
    let _ = stop_local.send(());
    let _ = local.await;
    net::local::Listener::remove(&api.path);
    let data_dir = running.machine.config.data_dir.clone();
    let stopped = running.stop().await;
    // The data directory stays locked until the old identity is gone.
    if exit == Exit::Reset {
        // The old identity must go even when the shutdown failed.
        if let Err(e) = &stopped {
            warn!("shutdown before reset: {e:#}");
        }
        Running::delete_data(&data_dir)?;
    } else {
        stopped?;
    }
    Ok(exit)
}
