//! The devtunnel mesh: this machine forwards every other member's relay tunnel to a loopback
//! port, with this machine's devtunnel login. The port is the member's own port, or another one
//! when that is in use here, so each machine names a member's relay by its own forward.
//!
//! Each forward has one owner, a task that holds the `devtunnel connect` child and starts it
//! again when it stops. A forward is ready only while its child runs and the member's relay
//! answers through it. A wake from sleep or a network change checks every forward at once. The tasks hold no
//! [`super::Machine`], so dropping the mesh stops every child.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use iroh::{EndpointId, RelayUrl};
use n0_future::task::AbortOnDropHandle;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::{net, state::TunnelEndpoint};

struct Forward {
    endpoint: TunnelEndpoint,
    /// The local port while the forward runs.
    ready: Arc<Mutex<Option<u16>>>,
    /// Set for a forward from a join ticket or an admission, until the member's row arrives.
    pinned: bool,
    _task: AbortOnDropHandle<()>,
}

pub(crate) struct Mesh {
    forwards: Mutex<BTreeMap<EndpointId, Forward>>,
    changed: watch::Sender<u64>,
    /// The devtunnel CLI, else `devtunnel` from the PATH.
    cli: Option<std::path::PathBuf>,
    /// Wakes from sleep and network changes: each forward checks its member's relay at once.
    wake: net::devtunnel::Wake,
}

impl Mesh {
    pub fn new(cli: Option<std::path::PathBuf>, wake: net::devtunnel::Wake) -> Self {
        Mesh { forwards: Default::default(), changed: Default::default(), cli, wake }
    }

    /// Fires when a forward becomes ready or stops.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    /// Starts forwarding `id`'s tunnel unless it already is. Pinned until a reconcile sees the
    /// member's row with the same endpoint.
    pub fn ensure(&self, id: EndpointId, endpoint: TunnelEndpoint) {
        let mut f = self.forwards.lock().unwrap();
        if f.get(&id).is_some_and(|x| x.endpoint == endpoint) {
            return;
        }
        f.insert(id, self.start(id, endpoint, true));
    }

    /// Makes the forwards equal to `want`, the members that publish a tunnel. A pinned forward
    /// stays: its machine is joining and has no row yet. It is unpinned when its row arrives.
    pub fn reconcile(&self, want: &BTreeMap<EndpointId, TunnelEndpoint>) {
        let mut f = self.forwards.lock().unwrap();
        let before = f.len();
        let mut gone = Vec::new();
        for (id, x) in f.iter_mut() {
            match want.get(id) {
                Some(e) if *e == x.endpoint => x.pinned = false,
                Some(_) => gone.push(*id),
                None if x.pinned => {}
                None => gone.push(*id),
            }
        }
        for id in gone {
            if let Some(x) = f.remove(&id) {
                linger(x);
            }
        }
        for (id, e) in want {
            if !f.contains_key(id) {
                f.insert(*id, self.start(*id, e.clone(), false));
            }
        }
        if f.len() != before {
            self.changed.send_modify(|v| *v += 1);
        }
    }

    /// Drops the pinned forwards: a join ended.
    pub fn unpin(&self) {
        self.forwards.lock().unwrap().retain(|_, x| !x.pinned);
        self.changed.send_modify(|v| *v += 1);
    }

    /// The relays of the forwards that run now, and whose they are.
    pub fn ready_urls(&self) -> Vec<(EndpointId, RelayUrl)> {
        let f = self.forwards.lock().unwrap();
        f.iter().filter_map(|(id, x)| Some((*id, local_url((*x.ready.lock().unwrap())?)))).collect()
    }

    /// Whether `id`'s forward runs now, and its endpoint.
    pub fn status(&self, id: &EndpointId) -> Option<(TunnelEndpoint, bool)> {
        self.forwards.lock().unwrap().get(id).map(|x| (x.endpoint.clone(), x.ready.lock().unwrap().is_some()))
    }

    /// Stops every forward. The machine is stopping.
    pub fn clear(&self) {
        self.forwards.lock().unwrap().clear();
    }

    fn start(&self, id: EndpointId, endpoint: TunnelEndpoint, pinned: bool) -> Forward {
        let ready = Arc::new(Mutex::new(None));
        let TunnelEndpoint::Devtunnel { id: tunnel, port } = endpoint.clone();
        let connect = net::DevTunnel { id: tunnel, port };
        let flag = ready.clone();
        let changed = self.changed.clone();
        let set = move |v: Option<u16>| {
            let mut r = flag.lock().unwrap();
            if *r != v {
                *r = v;
                changed.send_modify(|n| *n += 1);
            }
        };
        info!(member = %id.fmt_short(), endpoint = ?endpoint, pinned, "mesh relay: starting forward");
        let cli = self.cli.clone();
        let mut wake = self.wake.clone();
        let task = tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            loop {
                match connect.run_once(cli.as_deref()).await {
                    Ok((mut child, local)) => {
                        info!(member = %id.fmt_short(), tunnel = %connect.id, port, local, "mesh relay forwarded");
                        set(Some(local));
                        backoff = Duration::from_secs(1);
                        let why =
                            connect.watch(&mut child, local, &mut wake).await.unwrap_or_else(|e| format!("{e:#}"));
                        set(None);
                        warn!(member = %id.fmt_short(), "mesh relay forward stopped ({why}); starting it again");
                    }
                    Err(e) => {
                        set(None);
                        warn!(member = %id.fmt_short(), "mesh relay: {e:#}");
                    }
                }
                backoff = net::devtunnel::nap(backoff, &mut wake).await;
            }
        });
        Forward { endpoint, ready, pinned, _task: AbortOnDropHandle::new(task) }
    }
}

/// How long a forward that the roster no longer wants keeps running. A member that was removed
/// is reached only through its own relay, which is this forward. The answer to its leave, and the
/// close that tells it that it is removed, go through it. It must outlive
/// the relay grace in `admission`.
const FORWARD_GRACE: Duration = Duration::from_secs(5);

fn linger(forward: Forward) {
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => {
            runtime.spawn(async move {
                tokio::time::sleep(FORWARD_GRACE).await;
                drop(forward);
            });
        }
        Err(_) => drop(forward),
    }
}

fn local_url(port: u16) -> RelayUrl {
    format!("http://127.0.0.1:{port}/").parse().expect("loopback URL")
}
