//! The machine daemon: one iroh endpoint, the local API, and one link to every other member.

mod admission;
mod links;
mod machine;
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
pub use links::{PeerState, PeerStatus};
use machine::{ApiProtocol, SyncProtocol};
pub use machine::{Inner, Machine};

use crate::{api::API_ALPN, net, state::State, state::sync::SYNC_ALPN};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const KEY_FILE: &str = "secret.key";
pub const STATE_FILE: &str = "state.bin";

#[derive(Debug, Clone, clap::Args)]
pub struct Opts {
    /// Where the machine key and state live.
    #[arg(long, env = "IMESH_DATA_DIR", default_value_os_t = crate::net::local::default_data_dir())]
    pub data_dir: PathBuf,
    /// Local API: a unix socket, or a named pipe on Windows.
    #[arg(long, env = "IMESH_SOCKET", default_value_os_t = crate::net::local::default_path())]
    pub socket: PathBuf,
    /// Machine name. Defaults to the hostname.
    #[arg(long, env = "IMESH_NAME")]
    pub name: Option<String>,
    /// Labels for this machine, as key=value. Metadata for listing and selection.
    #[arg(long = "label", env = "IMESH_LABELS", value_delimiter = ',', value_parser = parse_label)]
    pub labels: Vec<(String, String)>,
    #[command(flatten)]
    pub relay: net::Opts,
}

/// Parses `key=value`. Labels travel comma-separated in `IMESH_LABELS`, so no comma.
pub(crate) fn parse_label(s: &str) -> Result<(String, String), String> {
    if s.contains(',') {
        return Err(format!("label {s:?} must not contain a comma"));
    }
    s.split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| format!("label {s:?} must be key=value"))
}

type MethodFuture = Pin<Box<dyn Future<Output = Result<Value>> + Send>>;
/// A method on the API. Apps add their own with [`Builder::method`].
pub(crate) type Method = Arc<dyn Fn(Machine, Caller, Value) -> MethodFuture + Send + Sync>;

type ProtocolFactory = Box<dyn FnOnce(&Machine) -> Box<dyn iroh::protocol::DynProtocolHandler> + Send>;

pub struct Builder {
    opts: Opts,
    methods: BTreeMap<String, Method>,
    protocols: Vec<(Vec<u8>, ProtocolFactory)>,
}

impl Builder {
    pub fn new(opts: Opts) -> Self {
        crate::daemon::methods::register(Builder { opts, methods: BTreeMap::new(), protocols: Vec::new() })
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

    /// Starts the machine: the endpoint and its protocols, sync with the other members, the state
    /// writer, and the relay when `--serve-relay` is set. It does not open the local API; [`serve`]
    /// does that.
    pub async fn spawn(self) -> Result<Running> {
        let opts = self.opts;
        std::fs::create_dir_all(&opts.data_dir)?;
        let secret = crate::client::config::load_or_create_key(&opts.data_dir.join(KEY_FILE))?;
        let state = Arc::new(State::open(secret.public(), Some(opts.data_dir.join(STATE_FILE)))?);
        let admission = Admission::new(state.clone());
        let lookup = MemoryLookup::new();
        let r = &opts.relay;
        let exact = !r.relays.is_empty() || r.no_n0;
        let urls = if exact { r.relays.clone() } else { iroh::endpoint::default_relay_mode().relay_map().urls() };
        let initial = net::relays(&urls, &r.relay_token);
        // Any relay option makes the map exact. Else the preset keeps its own relays.
        let custom = (exact || r.relay_token.is_some()).then_some(&initial);
        let mut b = net::endpoint_builder(r.no_n0, custom, r.relay_ca.as_deref())?;
        if let Some(url) = &r.pkarr {
            b = b
                .address_lookup(iroh::address_lookup::PkarrPublisher::builder(url.clone()))
                .address_lookup(iroh::address_lookup::PkarrResolver::builder(url.clone()));
        }
        let endpoint = b.secret_key(secret).address_lookup(lookup.clone()).hooks(admission.clone()).bind().await?;
        if r.mdns {
            // mDNS fails on hosts without a default route. Run without it then.
            match iroh_mdns_address_lookup::MdnsAddressLookup::builder().build(endpoint.id()) {
                Ok(m) => endpoint.address_lookup()?.add(m),
                Err(e) => warn!("mdns disabled: {e:#}"),
            }
        }
        let machine = Machine(Arc::new(Inner {
            endpoint: endpoint.clone(),
            state,
            name: opts.name.clone().unwrap_or_else(crate::os::hostname),
            opts,
            admission,
            lookup,
            methods: self.methods,
            peers: Default::default(),
            stopped: Default::default(),
            exposed: tokio::sync::watch::channel(None).0,
            relays: tokio::sync::Mutex::new(initial.clone()),
            joining: Default::default(),
            reset: tokio::sync::watch::channel(false).0,
            initial_relays: initial,
        }));
        let mut router = Router::builder(endpoint.clone())
            .accept(API_ALPN, ApiProtocol(machine.clone()))
            .accept(SYNC_ALPN, SyncProtocol(machine.state.clone()));
        for (alpn, make) in self.protocols {
            router = router.accept(alpn, make(&machine));
        }
        let router = router.spawn();
        let (relay, tunnel) = machine.serve_relay().await?;
        let mut tasks = vec![
            AbortOnDropHandle::new(tokio::spawn(machine.clone().background())),
            AbortOnDropHandle::new(tokio::spawn(machine.state.clone().flush_loop())),
        ];
        tasks.extend(tunnel);
        info!(id = %machine.id(), name = %machine.name, "machine started");
        Ok(Running { machine, router, _tasks: tasks, relay })
    }
}

/// A started machine. Dropping it stops its tasks. Call [`Running::shutdown`] to also close the
/// connections and write the state.
pub struct Running {
    pub machine: Machine,
    router: Router,
    _tasks: Vec<AbortOnDropHandle<()>>,
    relay: Option<iroh_relay::server::Server>,
}

impl Running {
    /// Resolves when `machine.reset` was called. The state is then frozen and empty. The owner
    /// shuts the machine down and calls [`Running::delete_data`].
    pub async fn reset_requested(&self) {
        let _ = self.machine.reset.subscribe().wait_for(|r| *r).await;
    }

    /// Stops the relay and the endpoint, then writes the state. Runs every step, and returns
    /// the first error.
    pub async fn shutdown(mut self) -> Result<()> {
        let relay = match self.relay.take() {
            Some(relay) => relay.shutdown().await.map_err(anyhow::Error::from),
            None => Ok(()),
        };
        let router = self.router.shutdown().await.map_err(anyhow::Error::from);
        let state = self.machine.state.clone();
        let flush =
            tokio::task::spawn_blocking(move || state.flush()).await.map_err(anyhow::Error::from).and_then(|r| r);
        relay.and(router).and(flush)
    }

    /// Deletes `<data dir>/app`, where apps keep their own files, then the state, then the key.
    /// The key goes last, so a failure leaves the old identity and the reset can run again.
    pub fn delete_data(opts: &Opts) -> Result<()> {
        let dir = &opts.data_dir;
        let gone = |r: std::io::Result<()>, what: &Path| match r {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("delete {}", what.display()))
            }
            _ => Ok(()),
        };
        let app = dir.join("app");
        gone(std::fs::remove_dir_all(&app), &app)?;
        for f in [dir.join(STATE_FILE), dir.join(KEY_FILE)] {
            gone(std::fs::remove_file(&f), &f)?;
        }
        Ok(())
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // Link tasks hold the machine, and the machine holds their handles. Break the cycle,
        // and start no new links.
        let mut peers = self.machine.peers.lock().unwrap();
        self.machine.stopped.store(true, std::sync::atomic::Ordering::Relaxed);
        peers.clear();
    }
}

/// Why [`run`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Exit {
    Signal,
    Reset,
}

/// Runs the daemon until a shutdown signal: the machine plus the local API. After
/// `machine.reset` it deletes the key, the state, and `<data dir>/app`, and starts again as a new
/// machine from `make()`.
pub async fn serve(make: impl Fn() -> Builder) -> Result<()> {
    // One signal listener for every run, so a signal during a restart is not lost.
    let signal = crate::os::shutdown_signal();
    tokio::pin!(signal);
    while run(make(), signal.as_mut()).await? == Exit::Reset {
        info!("reset: starting as a new machine");
    }
    Ok(())
}

async fn run(builder: Builder, signal: std::pin::Pin<&mut impl Future<Output = ()>>) -> Result<Exit> {
    let socket = builder.opts.socket.clone();
    let running = builder.spawn().await?;
    let listener = crate::net::local::Listener::bind(&socket)?;
    let (stop_local, stop) = tokio::sync::oneshot::channel();
    let local = AbortOnDropHandle::new(tokio::spawn(running.machine.clone().serve_local(listener, stop)));
    crate::os::sd_notify_ready();
    let exit = tokio::select! {
        _ = signal => Exit::Signal,
        _ = running.reset_requested() => Exit::Reset,
    };
    // Close every local connection, so the next run can bind the pipe or socket again.
    let _ = stop_local.send(());
    let _ = local.await;
    crate::net::local::Listener::remove(&socket);
    let opts = running.machine.opts.clone();
    let stopped = running.shutdown().await;
    if exit == Exit::Reset {
        // The old identity must go even when the shutdown failed.
        if let Err(e) = &stopped {
            warn!("shutdown before reset: {e:#}");
        }
        Running::delete_data(&opts)?;
    } else {
        stopped?;
    }
    Ok(exit)
}
