//! This machine's copy of the roster, and the view of the cluster it gives.
//!
//! One lock holds the copy. A change signs or merges entries, saves the copy, and then publishes a
//! new [`Snapshot`] under the same lock, so every reader sees the copy that is on disk.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use iroh::{EndpointId, SecretKey};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

pub use crate::roster::TunnelEndpoint;
use crate::roster::{Change, Role, Roster};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClusterInfo {
    /// The root public key, as text.
    pub id: String,
    pub owner: Option<EndpointId>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    pub name: String,
    pub role: Role,
}

/// What a machine says about itself, with the name the cluster gave it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MachineInfo {
    pub name: String,
    pub addr: iroh::EndpointAddr,
    pub version: String,
    pub os: String,
    pub arch: String,
    /// Public URL of a relay this machine serves to members, when it exposes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<iroh::RelayUrl>,
    /// Where members reach the relay this machine serves in the mesh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel: Option<TunnelEndpoint>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

/// The cluster relay setting.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RelaySettings {
    pub relays: Vec<iroh::RelayUrl>,
    /// Bearer token for these relays. Members are trusted, so it is in the roster.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

/// The cluster as the roster has it now.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub cluster: Option<ClusterInfo>,
    pub members: BTreeMap<EndpointId, Member>,
    pub removed: BTreeSet<EndpointId>,
    pub machines: BTreeMap<EndpointId, MachineInfo>,
    pub relays: RelaySettings,
    /// The roster's digest. Two machines with the same digest have the same roster.
    pub digest: [u8; 32],
    /// Former owners and admins that left, that no other machine removed yet. See
    /// [`Roster::unbounded`].
    pub unbounded: BTreeSet<EndpointId>,
}

impl Snapshot {
    fn of(roster: Option<&Roster>) -> Self {
        let Some(r) = roster else { return Self::default() };
        let members: BTreeMap<EndpointId, Member> =
            r.members().iter().map(|(id, m)| (*id, Member { name: m.name.clone(), role: m.role })).collect();
        let machines = members
            .iter()
            .filter_map(|(id, m)| {
                let l = r.locate(id)?;
                Some((
                    *id,
                    MachineInfo {
                        name: m.name.clone(),
                        addr: l.addr.clone(),
                        version: l.version.clone(),
                        os: l.os.clone(),
                        arch: l.arch.clone(),
                        relay: l.relay.clone(),
                        tunnel: l.tunnel.clone(),
                        labels: l.labels.clone(),
                    },
                ))
            })
            .collect();
        let relays = r
            .relays()
            .map(|s| RelaySettings {
                relays: s.urls.iter().filter_map(|u| u.parse().ok()).collect(),
                token: s.token.clone(),
            })
            .unwrap_or_default();
        let owner = members.iter().find(|(_, m)| m.role == Role::Owner).map(|(id, _)| *id);
        Snapshot {
            cluster: Some(ClusterInfo { id: r.network.to_string(), owner }),
            members,
            removed: r.removed().clone(),
            machines,
            relays,
            digest: r.digest(),
            unbounded: r.unbounded().clone(),
        }
    }

    /// Machines that are members now.
    pub fn active_members(&self) -> BTreeSet<EndpointId> {
        self.members.keys().copied().collect()
    }

    pub fn is_member(&self, id: &EndpointId) -> bool {
        self.members.contains_key(id)
    }

    pub fn is_removed(&self, id: &EndpointId) -> bool {
        self.removed.contains(id)
    }

    pub fn relay_settings(&self) -> RelaySettings {
        self.relays.clone()
    }

    pub fn machine(&self, id: &EndpointId) -> Option<MachineInfo> {
        self.machines.get(id).cloned()
    }

    pub fn role(&self, id: &EndpointId) -> Option<Role> {
        self.members.get(id).map(|m| m.role)
    }
}

/// This machine's copy of the roster. Cloning is cheap.
#[derive(Clone, Debug)]
pub struct Store {
    dir: PathBuf,
    roster: Arc<Mutex<Option<Roster>>>,
    snapshot: watch::Sender<Arc<Snapshot>>,
}

impl Store {
    /// Opens the copy in `dir`, if there is one.
    pub fn open(dir: &Path) -> Result<Self> {
        let roster = Roster::load(dir)?;
        let snapshot = watch::channel(Arc::new(Snapshot::of(roster.as_ref()))).0;
        Ok(Store { dir: dir.to_path_buf(), roster: Arc::new(Mutex::new(roster)), snapshot })
    }

    /// The data directory the copy is in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot.borrow().clone()
    }

    pub fn read<T>(&self, f: impl FnOnce(&Snapshot) -> T) -> T {
        f(&self.snapshot.borrow())
    }

    /// Ticks on every change of the roster.
    pub fn subscribe(&self) -> watch::Receiver<Arc<Snapshot>> {
        self.snapshot.subscribe()
    }

    /// A copy of the roster, to send to a member.
    pub fn roster(&self) -> Option<Roster> {
        self.roster.lock().unwrap().clone()
    }

    /// Runs `f` on the copy. When the copy changed, saves it and publishes a new snapshot before
    /// the lock is released. The save happens first, so a crash never loses a change that a
    /// caller saw. On any error the copy stays as it was, so memory, disk, and the snapshot that
    /// the gate reads always agree.
    fn update<T>(&self, f: impl FnOnce(&mut Option<Roster>) -> Result<T>) -> Result<T> {
        let mut guard = self.roster.lock().unwrap();
        let before = guard.clone();
        let out = match f(&mut guard) {
            Ok(out) => out,
            Err(e) => {
                *guard = before;
                return Err(e);
            }
        };
        if *guard != before {
            if let Some(r) = guard.as_ref()
                && let Err(e) = r.save(&self.dir)
            {
                *guard = before;
                return Err(e);
            }
            self.snapshot.send_replace(Arc::new(Snapshot::of(guard.as_ref())));
        }
        Ok(out)
    }

    /// Starts a new cluster with `me` as its owner. Returns the root key, which the caller shows
    /// once and keeps nowhere.
    pub fn create(&self, me: &SecretKey, name: String) -> Result<SecretKey> {
        self.update(|r| {
            anyhow::ensure!(r.is_none(), "machine is already in a cluster");
            let (roster, root) = Roster::create(me, name, now_ms())?;
            *r = Some(roster);
            Ok(root)
        })
    }

    /// Runs `f` on the copy under the lock, so no change lands while it runs.
    pub fn locked<T>(&self, f: impl FnOnce(Option<&Roster>) -> T) -> T {
        f(self.roster.lock().unwrap().as_ref())
    }

    /// Signs `change` as `key`. Fails when `key` may not make it.
    pub fn sign(&self, key: &SecretKey, change: Change) -> Result<()> {
        self.sign_if(key, |_| Ok(()), |_| change)
    }

    /// Signs a removal of `id`, with the decisions of `id` that count now kept.
    pub fn remove(&self, key: &SecretKey, id: EndpointId, cascade: bool) -> Result<()> {
        self.sign_if(key, |_| Ok(()), |r| r.removal(id, cascade))
    }

    /// Signs the change that `change` makes from the copy, as `key`, when `allowed` passes on the
    /// same copy. Both run under the lock, so nothing that `allowed` checks can change before the
    /// signature.
    pub fn sign_if(
        &self,
        key: &SecretKey,
        allowed: impl FnOnce(&Roster) -> Result<()>,
        change: impl FnOnce(&Roster) -> Change,
    ) -> Result<()> {
        self.update(|r| {
            let r = r.as_mut().context("machine is not in a cluster")?;
            allowed(r)?;
            let change = change(r);
            r.sign(key, at(r, key), change)
        })
    }

    /// Makes `me` the owner with the root key, and removes the owner before it, keeping what that
    /// owner decided until now. Without the removal, the old owner could sign decisions with a
    /// time before the handover. Both entries go in one change, or neither does.
    pub fn recover(&self, root: &SecretKey, me: &SecretKey, name: String) -> Result<()> {
        self.update(|r| {
            let r = r.as_mut().context("machine is not in a cluster")?;
            let old = r.members().iter().find(|(_, m)| m.role == Role::Owner).map(|(id, _)| *id);
            r.sign(root, at(r, root), Change::Owner { id: me.public(), name })?;
            if let Some(old) = old.filter(|old| *old != me.public()) {
                let change = r.removal(old, false);
                r.sign(me, at(r, me), change)?;
            }
            Ok(())
        })
    }

    /// Takes in a member's copy. A machine with no copy takes one of `expect` only: the cluster
    /// it joins. Returns whether this copy changed. The signatures are checked off the lock and
    /// off the async workers.
    pub async fn merge(&self, theirs: Roster, expect: Option<EndpointId>) -> Result<bool> {
        let missing = match self.roster.lock().unwrap().as_ref() {
            Some(mine) if mine.network == theirs.network => mine.missing_from(&theirs),
            Some(_) => return Ok(false),
            None if expect == Some(theirs.network) => theirs,
            None => return Ok(false),
        };
        let checked = tokio::task::spawn_blocking(move || Roster::verified(&missing)).await?;
        self.update(|r| match r {
            Some(mine) => Ok(mine.merge_verified(&checked)),
            None => {
                let mut mine = Roster::empty(checked.network);
                let changed = mine.merge_verified(&checked);
                *r = Some(mine);
                Ok(changed)
            }
        })
    }
}

/// The time for `key`'s next entry: now, or just after its newest entry when the clock went
/// back, so that its new entry is still its newest.
fn at(r: &Roster, key: &SecretKey) -> u64 {
    now_ms().max(r.last_at(&key.public()) + 1)
}

/// Milliseconds since the Unix epoch, by this machine's clock.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}
