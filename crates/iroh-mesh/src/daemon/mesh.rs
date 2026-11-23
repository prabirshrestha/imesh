//! The devtunnel mesh: this machine forwards every other member's relay tunnel to a loopback
//! port, with this machine's devtunnel login. The port is the member's own port, or another one
//! when that is in use here, so each machine names a member's relay by its own forward.
//!
//! Each forward has one owner, a task that holds the `devtunnel connect` child and starts it
//! again when it stops. A forward is ready only while its child runs and the member's relay
//! answers through it. A wake from sleep or a network change checks every forward at once. The tasks hold no
//! [`super::Machine`], so dropping the mesh stops every child.
//!
//! The roster is not the only source. Every machine also lists the hosted mesh tunnels of its
//! devtunnel login ([`Mesh::set_listed`]), each named by its machine's `imesh-<id>` label. A
//! member's tunnel in that list wins over a stale one in its row, so a member whose tunnel
//! changed is reached before its new row arrives (the row itself comes only through a
//! connection). A listed tunnel that no member row names, such as a machine that is joining, is
//! forwarded too ([`Mesh::reconcile_extra`]): its relay is in every peer's address.

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
    /// Forwards of listed tunnels that no member's forward covers, by tunnel ID.
    extra: Mutex<BTreeMap<String, Forward>>,
    /// The hosted mesh tunnels of this devtunnel login, by short machine ID.
    listed: Mutex<BTreeMap<String, Vec<TunnelEndpoint>>>,
    changed: watch::Sender<u64>,
    /// The devtunnel CLI, else `devtunnel` from the PATH.
    cli: net::devtunnel::Cli,
    /// Wakes from sleep and network changes: each forward checks its member's relay at once.
    wake: net::devtunnel::Wake,
}

impl Mesh {
    pub(crate) fn new(cli: net::devtunnel::Cli, wake: net::devtunnel::Wake) -> Self {
        Mesh {
            forwards: Default::default(),
            extra: Default::default(),
            listed: Default::default(),
            changed: Default::default(),
            cli,
            wake,
        }
    }

    /// The devtunnel CLI, marked as this machine's.
    pub(crate) fn cli(&self) -> &net::devtunnel::Cli {
        &self.cli
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

    /// Keeps the hosted mesh tunnels that `devtunnel list` shows, by short machine ID. Fires the
    /// change watch when they differ, so the next reconcile uses them.
    pub fn set_listed(&self, listed: BTreeMap<String, Vec<TunnelEndpoint>>) {
        let mut l = self.listed.lock().unwrap();
        if *l != listed {
            *l = listed;
            drop(l);
            self.changed.send_modify(|v| *v += 1);
        }
    }

    /// The endpoint to forward for member `id`: the one in its row while that tunnel is listed
    /// as hosted or no list names the member, else the member's first listed one.
    pub fn endpoint_for(&self, id: &EndpointId, row: Option<&TunnelEndpoint>) -> Option<TunnelEndpoint> {
        let l = self.listed.lock().unwrap();
        match l.get(&id.fmt_short().to_string()).filter(|v| !v.is_empty()) {
            Some(listed) if row.is_none_or(|r| !listed.contains(r)) => listed.first().cloned(),
            _ => row.cloned(),
        }
    }

    /// Forwards each listed tunnel whose machine has no forward here (call it after
    /// [`Self::reconcile`]) and is not `me`, at most [`MAX_EXTRA`] of them, and stops the others.
    pub fn reconcile_extra(&self, me: &EndpointId) {
        let skip: std::collections::BTreeSet<String> = {
            let f = self.forwards.lock().unwrap();
            f.keys().chain([me]).map(|id| id.fmt_short().to_string()).collect()
        };
        let want: BTreeMap<String, TunnelEndpoint> = {
            let l = self.listed.lock().unwrap();
            l.iter()
                .filter(|(short, _)| !skip.contains(*short))
                .flat_map(|(_, v)| v.iter())
                .map(|e| {
                    let TunnelEndpoint::Devtunnel { id, .. } = e;
                    (id.clone(), e.clone())
                })
                .take(MAX_EXTRA)
                .collect()
        };
        let mut x = self.extra.lock().unwrap();
        let before: Vec<String> = x.keys().cloned().collect();
        x.retain(|tunnel, f| want.get(tunnel) == Some(&f.endpoint));
        for (tunnel, e) in want {
            x.entry(tunnel).or_insert_with(|| self.start_listed(e));
        }
        if x.keys().cloned().collect::<Vec<_>>() != before {
            self.changed.send_modify(|v| *v += 1);
        }
    }

    /// The relays of the extra forwards that run now.
    pub fn extra_urls(&self) -> Vec<RelayUrl> {
        let x = self.extra.lock().unwrap();
        x.values().filter_map(|f| Some(local_url((*f.ready.lock().unwrap())?))).collect()
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
        self.extra.lock().unwrap().clear();
    }

    fn start(&self, id: EndpointId, endpoint: TunnelEndpoint, pinned: bool) -> Forward {
        info!(member = %id.fmt_short(), endpoint = ?endpoint, pinned, "mesh relay: starting forward");
        self.spawn(id.fmt_short().to_string(), endpoint, pinned)
    }

    fn start_listed(&self, endpoint: TunnelEndpoint) -> Forward {
        info!(endpoint = ?endpoint, "mesh relay: starting forward of a listed tunnel");
        self.spawn("listed".into(), endpoint, false)
    }

    fn spawn(&self, who: String, endpoint: TunnelEndpoint, pinned: bool) -> Forward {
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
        let cli = self.cli.clone();
        let mut wake = self.wake.clone();
        let task = tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            loop {
                match connect.run_once(&cli).await {
                    Ok((mut child, local)) => {
                        info!(member = %who, tunnel = %connect.id, port, local, "mesh relay forwarded");
                        set(Some(local));
                        backoff = Duration::from_secs(1);
                        let why =
                            connect.watch(&mut child, local, &mut wake).await.unwrap_or_else(|e| format!("{e:#}"));
                        set(None);
                        warn!(member = %who, "mesh relay forward stopped ({why}); starting it again");
                    }
                    Err(e) => {
                        set(None);
                        warn!(member = %who, "mesh relay: {e:#}");
                    }
                }
                backoff = net::devtunnel::nap(backoff, &mut wake).await;
            }
        });
        Forward { endpoint, ready, pinned, _task: AbortOnDropHandle::new(task) }
    }
}

/// The most listed tunnels of machines with no row that one machine forwards: each is a
/// `devtunnel connect` child.
const MAX_EXTRA: usize = 16;

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

#[cfg(test)]
mod tests {
    use super::*;

    fn tunnel(id: &str, port: u16) -> TunnelEndpoint {
        TunnelEndpoint::Devtunnel { id: id.into(), port }
    }

    #[test]
    fn a_listed_tunnel_wins_over_a_stale_row() {
        let mesh = Mesh::new(net::devtunnel::Cli::plain(None), watch::channel(0).1);
        let (x, y) = (iroh::SecretKey::generate().public(), iroh::SecretKey::generate().public());
        let (old, new, other) = (tunnel("old.usw2", 4400), tunnel("new.usw2", 4400), tunnel("two.usw2", 4401));
        // No list yet: the row.
        assert_eq!(mesh.endpoint_for(&x, Some(&old)), Some(old.clone()));
        mesh.set_listed(BTreeMap::from([(x.fmt_short().to_string(), vec![new.clone(), other.clone()])]));
        // The row's tunnel is not hosted: the listed one, also for a row with no tunnel.
        assert_eq!(mesh.endpoint_for(&x, Some(&old)), Some(new.clone()));
        assert_eq!(mesh.endpoint_for(&x, None), Some(new.clone()));
        // The row's tunnel is hosted: it stays.
        assert_eq!(mesh.endpoint_for(&x, Some(&other)), Some(other));
        // A machine the list does not name: its row.
        assert_eq!(mesh.endpoint_for(&y, Some(&old)), Some(old));
        assert_eq!(mesh.endpoint_for(&y, None), None);
    }
}
