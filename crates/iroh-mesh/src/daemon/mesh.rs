//! The devtunnel mesh: this machine forwards every other member's relay tunnel to that member's
//! own loopback port, with this machine's devtunnel login. `http://127.0.0.1:<port>` then names
//! the same relay on every member, so published addresses work unchanged.
//!
//! Each forward has one owner, a task that holds the `devtunnel connect` child and starts it
//! again when it stops. A forward is ready only while its child runs. The tasks hold no
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
    ready: Arc<Mutex<bool>>,
    /// Set for a forward from a join ticket or an admission, until the member's row arrives.
    pinned: bool,
    _task: AbortOnDropHandle<()>,
}

pub(crate) struct Mesh {
    forwards: Mutex<BTreeMap<EndpointId, Forward>>,
    changed: watch::Sender<u64>,
    /// The devtunnel CLI, else `devtunnel` from the PATH.
    cli: Option<std::path::PathBuf>,
}

impl Mesh {
    pub fn new(cli: Option<std::path::PathBuf>) -> Self {
        Mesh { forwards: Default::default(), changed: Default::default(), cli }
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
        f.retain(|id, x| match want.get(id) {
            Some(e) if *e == x.endpoint => {
                x.pinned = false;
                true
            }
            Some(_) => false,
            None => x.pinned,
        });
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

    /// The relays of the forwards that run now.
    pub fn ready_urls(&self) -> Vec<RelayUrl> {
        self.forwards
            .lock()
            .unwrap()
            .values()
            .filter(|x| *x.ready.lock().unwrap())
            .map(|x| x.endpoint.relay_url())
            .collect()
    }

    /// Whether `id`'s forward runs now, and its endpoint.
    pub fn status(&self, id: &EndpointId) -> Option<(TunnelEndpoint, bool)> {
        self.forwards.lock().unwrap().get(id).map(|x| (x.endpoint.clone(), *x.ready.lock().unwrap()))
    }

    /// Stops every forward. The machine is stopping.
    pub fn clear(&self) {
        self.forwards.lock().unwrap().clear();
    }

    fn start(&self, id: EndpointId, endpoint: TunnelEndpoint, pinned: bool) -> Forward {
        let ready = Arc::new(Mutex::new(false));
        let TunnelEndpoint::Devtunnel { id: tunnel, port } = endpoint.clone();
        let connect = net::DevTunnel { id: tunnel, port };
        let flag = ready.clone();
        let changed = self.changed.clone();
        let set = move |v: bool| {
            let mut r = flag.lock().unwrap();
            if *r != v {
                *r = v;
                changed.send_modify(|n| *n += 1);
            }
        };
        info!(member = %id.fmt_short(), endpoint = ?endpoint, pinned, "mesh relay: starting forward");
        let cli = self.cli.clone();
        let task = tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            loop {
                match connect.run_once(cli.as_deref()).await {
                    Ok(mut child) => {
                        info!(member = %id.fmt_short(), tunnel = %connect.id, port, "mesh relay forwarded");
                        set(true);
                        backoff = Duration::from_secs(1);
                        let why = connect.watch(&mut child).await.unwrap_or_else(|e| format!("{e:#}"));
                        set(false);
                        warn!(member = %id.fmt_short(), "mesh relay forward stopped ({why}); starting it again");
                    }
                    Err(e) => {
                        set(false);
                        warn!(member = %id.fmt_short(), "mesh relay: {e:#}");
                    }
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        });
        Forward { endpoint, ready, pinned, _task: AbortOnDropHandle::new(task) }
    }
}
