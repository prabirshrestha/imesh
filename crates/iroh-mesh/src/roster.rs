//! The roster: who is in the cluster, as a log of signed changes that every member keeps.
//!
//! The design follows codync-network (Apache-2.0), and parts of the entry and signature code are
//! adapted from it. See THIRD_PARTY_NOTICES.md.
//!
//! Copies merge by union, and every machine derives the same members from the same entries in
//! any order. An entry counts only when its signer may make the change, judged from the entries
//! above it:
//!
//! - the root key (the recovery code, kept on no machine) names the owner;
//! - the owner promotes plain members to admins, and admits and removes machines;
//! - admins admit machines and remove plain members;
//! - a machine says where it is reachable, and may remove itself.
//!
//! Removal is permanent. To come back, a machine joins again with a new key. A removal lists the
//! decisions of the removed machine that the remover counted (`kept`), and only those decisions
//! keep counting. Thus a removed machine cannot add a decision with an old time. No timestamp
//! decides this.
//!
//! Every copy drops the entries that cannot count and caps each signer's share, so the serialized
//! log stays under [`MAX_LOG`] whatever a stolen machine signs.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail, ensure};
use iroh::{EndpointAddr, EndpointId, RelayUrl, SecretKey, Signature};
use serde::{Deserialize, Serialize};

pub(crate) const FILE: &str = "roster.json";

/// Entries one machine may have in the log, apart from its newest location. Far more changes
/// than one machine makes in the life of a cluster.
pub const MAX_PER_SIGNER: usize = 1024;
/// Bytes one serialized entry may have, not counting the `kept` list of a removal.
pub const MAX_ENTRY: usize = 8 * 1024;
/// Addresses one location may have.
pub const MAX_ADDRS: usize = 64;
/// Bytes a machine name may have.
pub const MAX_NAME: usize = 48;
/// Bytes the serialized log may have.
pub const MAX_LOG: usize = 4 * 1024 * 1024;
/// Whether `name` is a machine name: 1 to [`MAX_NAME`] ASCII letters, digits, '-', and '_'. Such
/// a name is safe in addresses, paths, SSH config, and a terminal.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}
/// Bytes one digest adds to a serialized `kept` list: 64 hex digits, two quotes, and a comma.
const DIGEST_JSON: usize = 67;
/// Bytes the serialized roster has around its entries: the network id and the field names.
const LOG_OVERHEAD: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Role {
    Member,
    Admin,
    Owner,
}

/// One machine in the cluster, as the log has it now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub name: String,
    pub role: Role,
}

/// Where a machine is reachable, and what it runs. A machine signs only its own.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Location {
    pub addr: EndpointAddr,
    pub version: String,
    pub os: String,
    pub arch: String,
    pub relay: Option<RelayUrl>,
    pub tunnel: Option<TunnelEndpoint>,
    pub labels: BTreeMap<String, String>,
}

impl Location {
    /// A location at `addr`, cut to the first [`MAX_ADDRS`] addresses, with no other facts.
    pub fn new(addr: EndpointAddr) -> Self {
        Self {
            addr: EndpointAddr::from_parts(addr.id, addr.addrs.into_iter().take(MAX_ADDRS)),
            version: String::new(),
            os: String::new(),
            arch: String::new(),
            relay: None,
            tunnel: None,
            labels: BTreeMap::new(),
        }
    }
}

/// A relay that members reach through a tunnel: the kind, the tunnel, and the port that the
/// machine serves its relay on. Only `devtunnel` exists now; other kinds can be added.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum TunnelEndpoint {
    Devtunnel { id: String, port: u16 },
}

impl TunnelEndpoint {
    pub fn port(&self) -> u16 {
        match self {
            TunnelEndpoint::Devtunnel { port, .. } => *port,
        }
    }
}

/// The relay servers the cluster uses, set by the owner or an admin.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Relays {
    pub urls: Vec<String>,
    pub token: Option<String>,
}

/// The BLAKE3 hash of an entry's signature. It names one entry: a signer cannot make two entries
/// with the same signature.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Digest(#[serde(with = "hex_bytes")] pub [u8; 32]);

impl std::fmt::Debug for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Change {
    /// The root names the owner. The newest by the root's time is the current owner.
    Owner {
        id: EndpointId,
        name: String,
    },
    Admit {
        id: EndpointId,
        name: String,
    },
    Promote {
        id: EndpointId,
    },
    /// `cascade` voids every decision of the removed machine (a stolen one). Otherwise only the
    /// decisions in `kept` keep counting. [`Roster::removal`] fills it from the current log.
    Remove {
        id: EndpointId,
        cascade: bool,
        kept: BTreeSet<Digest>,
    },
    /// A new name for `id`. A machine renames itself; the owner and admins rename anyone.
    Rename {
        id: EndpointId,
        name: String,
    },
    Locate(Location),
    Relays(Relays),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Signed {
    /// When it was signed (Unix ms), as its signer says.
    pub at: u64,
    pub by: EndpointId,
    pub change: Change,
    /// Ed25519 over the network id, the signer, the time, and the change.
    #[serde(with = "hex_bytes")]
    sig: [u8; Signature::LENGTH],
}

impl Signed {
    pub fn new(network: &EndpointId, key: &SecretKey, at: u64, change: Change) -> Self {
        let by = key.public();
        let sig = key.sign(&body(network, &by, at, &change)).to_bytes();
        Self { at, by, change, sig }
    }

    pub fn digest(&self) -> Digest {
        Digest(hash(&self.sig))
    }

    fn verifies(&self, network: &EndpointId) -> bool {
        self.by.verify(&body(network, &self.by, self.at, &self.change), &Signature::from_bytes(&self.sig)).is_ok()
    }

    /// Whether it decides who is in: the entries a removal lists in `kept`.
    fn decides(&self) -> bool {
        match &self.change {
            Change::Admit { .. } | Change::Promote { .. } => true,
            Change::Remove { id, .. } => *id != self.by,
            Change::Owner { .. } | Change::Rename { .. } | Change::Locate(_) | Change::Relays(_) => false,
        }
    }
}

fn body(network: &EndpointId, by: &EndpointId, at: u64, change: &Change) -> Vec<u8> {
    serde_json::to_vec(&(network, by, at, change)).expect("a change always serializes")
}

/// The cluster as this machine knows it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Roster {
    /// The cluster's id: the root public key.
    pub network: EndpointId,
    log: BTreeSet<Signed>,
    #[serde(skip)]
    view: View,
}

impl PartialEq for Roster {
    fn eq(&self, other: &Self) -> bool {
        self.network == other.network && self.log == other.log
    }
}

/// What the log says now.
#[derive(Clone, Debug, Default)]
struct View {
    members: BTreeMap<EndpointId, Member>,
    removed: BTreeSet<EndpointId>,
    locations: BTreeMap<EndpointId, Location>,
    relays: Option<Relays>,
    /// The entries that took effect.
    counted: BTreeSet<Digest>,
    /// Machines that could still sign decisions with an old time: a former owner, or an admin
    /// that left, that no other machine removed. The owner removes them, which bounds what counts.
    unbounded: BTreeSet<EndpointId>,
}

impl Roster {
    /// A new cluster with `me` as its owner. Returns it with the root key, which the caller shows
    /// once as the recovery code and keeps nowhere.
    pub fn create(me: &SecretKey, name: String, at: u64) -> Result<(Self, SecretKey)> {
        let root = SecretKey::generate();
        Ok((Self::founded(&root, me, name, at)?, root))
    }

    fn founded(root: &SecretKey, me: &SecretKey, name: String, at: u64) -> Result<Self> {
        let mut roster = Self::empty(root.public());
        roster.sign(root, at, Change::Owner { id: me.public(), name })?;
        Ok(roster)
    }

    /// A copy with no entries, for a machine that joins `network` and merges a member's copy.
    pub fn empty(network: EndpointId) -> Self {
        Self { network, log: BTreeSet::new(), view: View::default() }
    }

    /// Signs `change` as `key` and adds it. Refuses a change that `key` may not make.
    pub fn sign(&mut self, key: &SecretKey, at: u64, change: Change) -> Result<()> {
        let entry = Signed::new(&self.network, key, at, change);
        check_bounds(&entry)?;
        let digest = entry.digest();
        let mut next = self.clone();
        next.log.insert(entry.clone());
        next.rebuild();
        if !next.view.counted.contains(&digest) {
            let most: usize = self.log.iter().chain([&entry]).map(|s| most_bytes(s) + 1).sum();
            let full = LOG_OVERHEAD + most > MAX_LOG && LOG_OVERHEAD + json_len(&self.log) + json_len(&entry) > MAX_LOG;
            ensure!(!full, "the roster is full");
            bail!("this machine may not make that change");
        }
        *self = next;
        Ok(())
    }

    /// A removal of `id` with `kept` filled from this copy: the decisions of `id` that count now.
    /// With `cascade`, `kept` is empty, as none of them count.
    pub fn removal(&self, id: EndpointId, cascade: bool) -> Change {
        let kept = if cascade {
            BTreeSet::new()
        } else {
            self.log
                .iter()
                .filter(|s| s.by == id && s.decides())
                .map(Signed::digest)
                .filter(|d| self.view.counted.contains(d))
                .collect()
        };
        Change::Remove { id, cascade, kept }
    }

    /// Takes in another copy: the entries that verify and are in bounds. Returns whether this
    /// copy changed.
    pub fn merge(&mut self, other: &Roster) -> bool {
        self.merge_verified(&Self::verified(other))
    }

    /// The entries of `other` that are in bounds and verify. It checks at most [`MAX_PER_SIGNER`]
    /// entries of each signer and only each signer's newest location, so a member cannot make a
    /// machine check more entries than a log can hold. Signature checks cost time: run this
    /// outside any lock.
    pub fn verified(other: &Roster) -> Self {
        let mut share: BTreeMap<EndpointId, usize> = BTreeMap::new();
        let mut newest: BTreeMap<EndpointId, &Signed> = BTreeMap::new();
        for s in other.log.iter().filter(|s| matches!(s.change, Change::Locate(_))) {
            newest.insert(s.by, s);
        }
        // The same order as the cap in [`Roster::compact`]. The entries are not checked yet, so
        // a forged removal can change which entries get checked, but not what counts after.
        let kept = kept_by_others(&other.log, |_| true);
        let mut order: Vec<&Signed> = other.log.iter().collect();
        order.sort_by_key(|s| (!kept.contains(&(s.by, s.digest())), *s));
        let log = order
            .into_iter()
            .filter(|s| match s.change {
                Change::Locate(_) => newest[&s.by] == *s,
                _ => {
                    let n = share.entry(s.by).or_default();
                    *n += 1;
                    *n <= MAX_PER_SIGNER
                }
            })
            .filter(|s| check_bounds(s).is_ok() && s.verifies(&other.network))
            .cloned()
            .collect();
        Self { network: other.network, log, view: View::default() }
    }

    /// The entries of `other` that this copy lacks. Cheap: run it under the lock, then
    /// [`Roster::verified`] outside it.
    pub fn missing_from(&self, other: &Roster) -> Self {
        let log = other.log.iter().filter(|s| !self.log.contains(s)).cloned().collect();
        Self { network: other.network, log, view: View::default() }
    }

    /// The newest time `id` signed with, so that its next entry can be newer.
    pub fn last_at(&self, id: &EndpointId) -> u64 {
        self.log.iter().filter(|s| s.by == *id).map(|s| s.at).max().unwrap_or(0)
    }

    /// Takes in entries that [`Roster::verified`] already checked. Returns whether this copy
    /// changed.
    pub(crate) fn merge_verified(&mut self, other: &Roster) -> bool {
        if other.network != self.network {
            return false;
        }
        let new: Vec<Signed> = other.log.iter().filter(|s| !self.log.contains(s)).cloned().collect();
        if new.is_empty() {
            return false;
        }
        let before = self.log.clone();
        self.log.extend(new);
        self.rebuild();
        self.log != before
    }

    pub fn members(&self) -> &BTreeMap<EndpointId, Member> {
        &self.view.members
    }

    pub fn role(&self, id: &EndpointId) -> Option<Role> {
        self.view.members.get(id).map(|m| m.role)
    }

    pub fn is_member(&self, id: &EndpointId) -> bool {
        self.view.members.contains_key(id)
    }

    pub fn is_removed(&self, id: &EndpointId) -> bool {
        self.view.removed.contains(id)
    }

    /// Former owners and admins that left, that no other machine removed yet. Until a removal by
    /// another machine bounds them, their stolen keys could sign decisions with an old time. The
    /// owner removes them.
    pub fn unbounded(&self) -> &BTreeSet<EndpointId> {
        &self.view.unbounded
    }

    /// Every machine that was removed.
    pub fn removed(&self) -> &BTreeSet<EndpointId> {
        &self.view.removed
    }

    pub fn name(&self, id: &EndpointId) -> Option<&str> {
        self.view.members.get(id).map(|m| m.name.as_str())
    }

    pub fn relays(&self) -> Option<&Relays> {
        self.view.relays.as_ref()
    }

    /// Where member `id` last said it is reachable.
    pub fn locate(&self, id: &EndpointId) -> Option<&Location> {
        self.view.locations.get(id)
    }

    /// The BLAKE3 hash of the serialized log. Two copies with the same digest have the same entries.
    pub fn digest(&self) -> [u8; 32] {
        hash(&serde_json::to_vec(&self.log).expect("a log always serializes"))
    }

    /// Reads `roster.json` from `dir`, keeping only the entries that verify. `None` when there is
    /// no file.
    pub fn load(dir: &Path) -> Result<Option<Self>> {
        let path = dir.join(FILE);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        let mut roster: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("{} is damaged; join the cluster again", path.display()))?;
        let network = roster.network;
        roster.log.retain(|s| check_bounds(s).is_ok() && s.verifies(&network));
        roster.rebuild();
        Ok(Some(roster))
    }

    /// Writes `roster.json` in `dir` with mode 0600, through a temp file and a rename.
    pub fn save(&self, dir: &Path) -> Result<()> {
        let path = dir.join(FILE);
        crate::os::write_private(&path, &serde_json::to_vec(self)?).with_context(|| format!("save {}", path.display()))
    }

    /// Counts the log, then drops the entries that cannot count, until nothing more goes.
    fn rebuild(&mut self) {
        loop {
            self.view = count(self.network, &self.log);
            if !self.compact() {
                break;
            }
        }
    }

    /// Drops what cannot count: entries by machines that no admission names, decisions by
    /// machines that may not decide, all but each signer's first leave and newest relay setting,
    /// and every location that did not count. Then caps each signer's share and the whole log.
    ///
    /// The order of merges does not change what remains, because a copy that holds an entry also
    /// holds the admission and promotion that let its signer make it: copies merge whole. The
    /// exceptions are a location that arrives before its signer's admission, which its machine
    /// sends again in its next hello, and the cut at [`MAX_LOG`], which only stolen deciders can
    /// reach. Returns whether anything went.
    fn compact(&mut self) -> bool {
        let root = self.network;
        let owners: BTreeSet<EndpointId> = self
            .log
            .iter()
            .filter_map(|s| match &s.change {
                Change::Owner { id, .. } if s.by == root => Some(*id),
                _ => None,
            })
            .collect();
        let top = |id: &EndpointId| *id == root || owners.contains(id);
        let mut deciders: BTreeSet<EndpointId> = owners.iter().copied().chain([root]).collect();
        for s in &self.log {
            if let Change::Promote { id } = &s.change
                && top(&s.by)
            {
                deciders.insert(*id);
            }
        }
        let mut signers = deciders.clone();
        for s in &self.log {
            if let Change::Admit { id, .. } = &s.change
                && deciders.contains(&s.by)
            {
                signers.insert(*id);
            }
        }
        // The log is in time order, so the first leave, the last relay setting, and the last
        // rename win.
        let mut leave: BTreeMap<EndpointId, &Signed> = BTreeMap::new();
        let mut relays: BTreeMap<EndpointId, &Signed> = BTreeMap::new();
        let mut renames: BTreeMap<(EndpointId, EndpointId), &Signed> = BTreeMap::new();
        let mut locates: BTreeMap<EndpointId, &Signed> = BTreeMap::new();
        for s in &self.log {
            match &s.change {
                Change::Locate(_) => {
                    locates.insert(s.by, s);
                }
                Change::Rename { id, .. } => {
                    renames.insert((s.by, *id), s);
                }
                Change::Remove { id, .. } if *id == s.by => {
                    leave.entry(s.by).or_insert(s);
                }
                Change::Relays(_) => {
                    relays.insert(s.by, s);
                }
                _ => {}
            }
        }
        let counts = |s: &Signed| self.view.counted.contains(&s.digest());
        let may_stay = |s: &Signed| {
            signers.contains(&s.by)
                && match &s.change {
                    Change::Owner { .. } => s.by == root,
                    Change::Promote { .. } => top(&s.by),
                    Change::Admit { .. } => deciders.contains(&s.by),
                    Change::Remove { id, .. } if *id == s.by => leave[&s.by] == s,
                    Change::Remove { .. } => deciders.contains(&s.by),
                    Change::Relays(_) => deciders.contains(&s.by) && relays[&s.by] == s,
                    Change::Rename { id, .. } => {
                        (*id == s.by || deciders.contains(&s.by)) && renames[&(s.by, *id)] == s
                    }
                    // Each signer's newest location stays even before it counts: its admission may
                    // arrive after it, and the machine does not sign the same place again. A
                    // removed machine keeps none.
                    Change::Locate(_) => locates[&s.by] == s && !self.view.removed.contains(&s.by),
                }
        };
        // Each signer keeps first the entries that a decider's removal of it lists in `kept`, then
        // the rest in time order. So a stolen key cannot push out what its removal kept, whatever
        // times it signs its junk with. This set only grows as entries arrive, and the order
        // depends on nothing else, so each signer keeps a prefix of one order on every copy.
        let kept = kept_by_others(&self.log, |by| deciders.contains(by));
        let mut candidates: Vec<&Signed> = self.log.iter().filter(|s| may_stay(s)).collect();
        candidates.sort_by_key(|s| (!kept.contains(&(s.by, s.digest())), *s));
        let mut share: BTreeMap<EndpointId, usize> = BTreeMap::new();
        let mut stay: Vec<(bool, u8, usize, &Signed)> = candidates
            .into_iter()
            .filter(|s| {
                if matches!(s.change, Change::Locate(_)) {
                    return true;
                }
                let n = share.entry(s.by).or_default();
                *n += 1;
                *n <= MAX_PER_SIGNER
            })
            .map(|s| {
                let rank = if s.by == root {
                    0
                } else if owners.contains(&s.by) {
                    1
                } else if deciders.contains(&s.by) {
                    2
                } else {
                    3
                };
                (!counts(s), rank, most_bytes(s), s)
            })
            .collect();
        // Fill the log with counted entries first, then by rank, the root's first, then the
        // smallest first, and stop at the first entry that does not fit. The signer chooses
        // neither whether an entry counts nor its rank, so junk from a stolen machine goes before
        // the entries of others. A prefix of a fixed order is what lets every copy cut the same
        // entries. Every entry is in bounds, so when the bounds fit, all entries fit and no entry
        // is serialized.
        let most: usize = stay.iter().map(|(.., s)| most_bytes(s) + 1).sum();
        if LOG_OVERHEAD + most > MAX_LOG {
            stay.sort();
            let mut size = LOG_OVERHEAD;
            let fit = stay.iter().take_while(|(.., s)| {
                size += json_len(s) + 1;
                size <= MAX_LOG
            });
            stay = fit.copied().collect();
        }
        let kept: BTreeSet<Signed> = stay.into_iter().map(|(.., s)| s.clone()).collect();
        let changed = kept.len() != self.log.len();
        self.log = kept;
        changed
    }
}

/// The entries, as signer and digest, that a removal of their signer by another signer that
/// `may_decide` allows lists in `kept`.
fn kept_by_others(log: &BTreeSet<Signed>, may_decide: impl Fn(&EndpointId) -> bool) -> BTreeSet<(EndpointId, Digest)> {
    let mut kept = BTreeSet::new();
    for s in log {
        if let Change::Remove { id, kept: list, .. } = &s.change
            && *id != s.by
            && may_decide(&s.by)
        {
            kept.extend(list.iter().map(|d| (*id, *d)));
        }
    }
    kept
}

/// The removals of one machine that count.
struct Removal<'a> {
    at: u64,
    cascade: bool,
    kept: &'a BTreeSet<Digest>,
    /// A machine's own leave. It removes the machine but decides nothing about its decisions:
    /// else a thief with the key could void them, or pick the ones that count.
    leave: bool,
}

/// Which decisions of a machine count.
enum Allowed {
    All,
    Only(BTreeSet<Digest>),
    Nothing,
}

impl Allowed {
    /// A machine that no other machine removed keeps all, also after it left. A cascade removal
    /// voids all. Otherwise a decision counts only when every removal by another machine lists
    /// it.
    fn of(removals: Option<&Vec<Removal>>) -> Self {
        let others: Vec<&Removal> = removals.into_iter().flatten().filter(|r| !r.leave).collect();
        if others.is_empty() {
            return Self::All;
        }
        if others.iter().any(|r| r.cascade) {
            return Self::Nothing;
        }
        let mut lists = others.iter().map(|r| r.kept);
        let first = lists.next().expect("others is not empty").clone();
        Self::Only(lists.fold(first, |acc, kept| acc.intersection(kept).copied().collect()))
    }

    fn admits(&self, digest: &Digest) -> bool {
        match self {
            Self::All => true,
            Self::Only(kept) => kept.contains(digest),
            Self::Nothing => false,
        }
    }
}

/// Works out the cluster from the log, from the root down, so the result does not depend on the
/// order the entries arrived in.
fn count(root: EndpointId, log: &BTreeSet<Signed>) -> View {
    let mut counted = BTreeSet::new();

    // 1. Owners, each from the root's time that named it.
    let mut owners: Vec<(u64, EndpointId, &str)> = Vec::new();
    for s in log.iter().filter(|s| s.by == root) {
        if let Change::Owner { id, name } = &s.change {
            owners.push((s.at, *id, name));
            counted.insert(s.digest());
        }
    }
    owners.sort();
    let owner = owners.last().map(|o| o.1);
    let owner_at = |at: u64| owners.iter().rev().find(|o| o.0 <= at).map(|o| o.1);
    let top = |s: &Signed| s.by == root || owner_at(s.at) == Some(s.by);
    // The root, then the owners from the latest named back. One removes only those after it.
    let mut above = vec![root];
    for (_, id, _) in owners.iter().rev() {
        if !above.contains(id) {
            above.push(*id);
        }
    }

    // 2. Removals: each machine's own leave, then the root's and the owners' in that order. Only
    // the root removes the current owner.
    let mut removals: BTreeMap<EndpointId, Vec<Removal>> = BTreeMap::new();
    for s in log {
        if let Change::Remove { id, cascade, kept } = &s.change
            && *id == s.by
            && *id != root
            && Some(*id) != owner
        {
            removals.entry(*id).or_default().push(Removal { at: s.at, cascade: *cascade, kept, leave: true });
            counted.insert(s.digest());
        }
    }
    for (rank, by) in above.iter().enumerate() {
        let allowed = Allowed::of(removals.get(by));
        for s in log.iter().filter(|s| s.by == *by && top(s)) {
            if let Change::Remove { id, cascade, kept } = &s.change
                && *id != *by
                && *id != root
                && (*by == root || (Some(*id) != owner && above.iter().position(|a| a == id).is_none_or(|r| r > rank)))
                && allowed.admits(&s.digest())
            {
                removals.entry(*id).or_default().push(Removal { at: s.at, cascade: *cascade, kept, leave: false });
                counted.insert(s.digest());
            }
        }
    }
    // Admins remove plain members only, so no later removal changes whose decisions count.
    let allowed: BTreeMap<EndpointId, Allowed> =
        removals.iter().map(|(id, list)| (*id, Allowed::of(Some(list)))).collect();
    let may = |s: &Signed| allowed.get(&s.by).is_none_or(|a| a.admits(&s.digest()));

    // 3. Promotions by the owner of their time. An admin that the root later names owner keeps its
    // promotion, so what it decided as an admin still counts.
    let promotes: Vec<(&Signed, EndpointId)> = log
        .iter()
        .filter_map(|s| match &s.change {
            Change::Promote { id } if top(s) && may(s) && *id != root => Some((s, *id)),
            _ => None,
        })
        .collect();
    let promoted: BTreeSet<EndpointId> = promotes.iter().map(|(_, id)| *id).collect();

    // 5 and 6. Admissions by owners and by admitted admins, until no more machines get in. Each
    // machine keeps the first time and name it got in with.
    let mut admitted: BTreeMap<EndpointId, (u64, &str)> = BTreeMap::new();
    for &(at, id, name) in &owners {
        admitted.entry(id).or_insert((at, name));
    }
    let admits = |s: &Signed, admitted: &BTreeMap<EndpointId, (u64, &str)>| {
        may(s) && (top(s) || (promoted.contains(&s.by) && admitted.contains_key(&s.by)))
    };
    loop {
        let before = admitted.len();
        for s in log {
            if let Change::Admit { id, name } = &s.change
                && admits(s, &admitted)
            {
                let first = admitted.entry(*id).or_insert((s.at, name));
                if (s.at, name.as_str()) < *first {
                    *first = (s.at, name);
                }
            }
        }
        if admitted.len() == before {
            break;
        }
    }
    for s in log {
        if matches!(s.change, Change::Admit { .. }) && admits(s, &admitted) {
            counted.insert(s.digest());
        }
    }
    let admins: BTreeSet<EndpointId> = promoted.iter().filter(|id| admitted.contains_key(*id)).copied().collect();
    for (s, id) in &promotes {
        if admitted.contains_key(id) {
            counted.insert(s.digest());
        }
    }

    // 4. Removals by admins, of plain members: never an admin or a machine the root named owner.
    let named: BTreeSet<EndpointId> = owners.iter().map(|o| o.1).collect();
    for s in log {
        if let Change::Remove { id, cascade, kept } = &s.change
            && *id != s.by
            && *id != root
            && admins.contains(&s.by)
            && may(s)
            && admitted.contains_key(id)
            && !admins.contains(id)
            && !named.contains(id)
        {
            removals.entry(*id).or_default().push(Removal { at: s.at, cascade: *cascade, kept, leave: false });
            counted.insert(s.digest());
        }
    }

    // A removal of a machine that was never admitted decides nothing.
    for s in log {
        if let Change::Remove { id, .. } = &s.change
            && !admitted.contains_key(id)
        {
            counted.remove(&s.digest());
        }
    }

    // 7. Members are the admitted minus the removed. The newest rename that counts names a
    // machine: its own, or one by the owner or an admin.
    let removed: BTreeSet<EndpointId> = removals.keys().filter(|id| admitted.contains_key(*id)).copied().collect();
    let renames_by = |s: &Signed, id: &EndpointId| {
        may(s) && !removed.contains(&s.by) && (s.by == *id || top(s) || admins.contains(&s.by))
    };
    let mut renamed: BTreeMap<EndpointId, &Signed> = BTreeMap::new();
    for s in log {
        if let Change::Rename { id, .. } = &s.change
            && admitted.contains_key(id)
            && renames_by(s, id)
            && renamed.get(id).is_none_or(|r| (r.at, r.by) < (s.at, s.by))
        {
            renamed.insert(*id, s);
        }
    }
    let mut renamed_at: BTreeMap<EndpointId, u64> = BTreeMap::new();
    for (id, s) in &renamed {
        if let Change::Rename { name, .. } = &s.change
            && let Some(entry) = admitted.get_mut(id)
        {
            entry.1 = name;
            renamed_at.insert(*id, s.at);
            counted.insert(s.digest());
        }
    }
    let removed_at: BTreeMap<EndpointId, u64> = removals
        .iter()
        .map(|(id, list)| (*id, list.iter().map(|r| r.at).min().expect("a removal list is never empty")))
        .collect();
    let members: BTreeMap<EndpointId, Member> = distinct_names(&admitted, &renamed_at, &removed_at)
        .into_iter()
        .filter(|(id, _)| !removed.contains(id))
        .map(|(id, name)| {
            let role = if Some(id) == owner {
                Role::Owner
            } else if admins.contains(&id) {
                Role::Admin
            } else {
                Role::Member
            };
            (id, Member { name, role })
        })
        .collect();

    let sets_relays =
        |by: &EndpointId| *by == root || Some(*by) == owner || (admins.contains(by) && !removed.contains(by));
    let relays =
        log.iter().filter(|s| matches!(s.change, Change::Relays(_)) && sets_relays(&s.by)).max_by_key(|s| (s.at, s.by));
    let relays = relays.and_then(|s| {
        counted.insert(s.digest());
        match &s.change {
            Change::Relays(r) => Some(r.clone()),
            _ => None,
        }
    });

    // The log is in time order, so the last location of each member is its newest.
    let mut newest: BTreeMap<EndpointId, &Signed> = BTreeMap::new();
    for s in log {
        if let Change::Locate(loc) = &s.change
            && loc.addr.id == s.by
            && members.contains_key(&s.by)
        {
            newest.insert(s.by, s);
        }
    }
    let locations = newest
        .into_iter()
        .filter_map(|(id, s)| {
            counted.insert(s.digest());
            match &s.change {
                Change::Locate(loc) => Some((id, loc.clone())),
                _ => None,
            }
        })
        .collect();

    let unbounded = admitted
        .keys()
        .copied()
        .filter(|id| !removals.get(id).is_some_and(|list| list.iter().any(|r| !r.leave)))
        .filter(|id| {
            let former_owner = named.contains(id) && Some(*id) != owner;
            let left_admin = promoted.contains(id) && removed.contains(id);
            former_owner || left_admin
        })
        .collect();

    View { members, removed, locations, relays, counted, unbounded }
}

/// Names every machine uniquely, case blind. When two admins admit a `laptop` each before their
/// copies meet, the one admitted later (by time, then id) gets the first free number, the same
/// on every copy. A renamed machine yields to every machine that was not renamed. A removed machine holds its name against the machines admitted while it was
/// in, so no name moves when it goes. A machine admitted after may take the name.
fn distinct_names(
    admitted: &BTreeMap<EndpointId, (u64, &str)>,
    renamed_at: &BTreeMap<EndpointId, u64>,
    removed_at: &BTreeMap<EndpointId, u64>,
) -> BTreeMap<EndpointId, String> {
    // A renamed machine comes after every machine that keeps the name it got in with, so a
    // rename never takes a name that another machine holds.
    let mut order: Vec<(bool, u64, EndpointId, &str)> = admitted
        .iter()
        .map(|(id, &(at, name))| match renamed_at.get(id) {
            Some(r) => (true, *r, *id, name),
            None => (false, at, *id, name),
        })
        .collect();
    order.sort();
    // A signer chooses the time of its rename, so time cannot settle two renames to one name: a
    // stolen key could backdate its rename and take the name. Such a name goes to neither.
    let mut claims: BTreeMap<String, usize> = BTreeMap::new();
    for (renamed, _, id, name) in &order {
        if *renamed && !removed_at.contains_key(id) {
            *claims.entry(name.to_lowercase()).or_default() += 1;
        }
    }
    // Each name given, and until when it is held: for good, or until its machine was removed.
    let mut held: BTreeMap<String, Option<u64>> = BTreeMap::new();
    let mut names = BTreeMap::new();
    for (renamed, at, id, name) in order {
        let free = |n: &str| held.get(&n.to_lowercase()).is_none_or(|until| until.is_some_and(|t| t < at));
        let contested = renamed && claims.get(&name.to_lowercase()).is_some_and(|n| *n > 1);
        let unique = std::iter::once(name.to_string())
            .filter(|_| !contested)
            // One more number than there are names, so one of them is free.
            .chain((2..=held.len() + 2).map(|n| format!("{name}-{n}")))
            .find(|n| free(n))
            .expect("a number is free");
        held.insert(unique.to_lowercase(), removed_at.get(&id).copied());
        names.insert(id, unique);
    }
    names
}

/// The most bytes an entry in bounds serializes to.
fn most_bytes(s: &Signed) -> usize {
    match &s.change {
        Change::Remove { kept, .. } => MAX_ENTRY + kept.len() * DIGEST_JSON,
        _ => MAX_ENTRY,
    }
}

/// Refuses an entry out of bounds. Such an entry never counts, whoever signed it.
fn check_bounds(s: &Signed) -> Result<()> {
    match &s.change {
        Change::Owner { name, .. } | Change::Admit { name, .. } | Change::Rename { name, .. } => {
            ensure!(valid_name(name), "a machine name has 1 to {MAX_NAME} letters, digits, '-', and '_'")
        }
        Change::Locate(loc) => {
            ensure!(loc.addr.addrs.len() <= MAX_ADDRS, "a location has at most {MAX_ADDRS} addresses")
        }
        Change::Remove { kept, .. } => {
            ensure!(kept.len() <= MAX_PER_SIGNER, "a removal keeps at most {MAX_PER_SIGNER} decisions")
        }
        Change::Promote { .. } | Change::Relays(_) => {}
    }
    ensure!(json_len(s) <= most_bytes(s), "the change has more than {MAX_ENTRY} bytes");
    Ok(())
}

fn json_len(value: &impl Serialize) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |b| b.len())
}

fn hash(data: &[u8]) -> [u8; 32] {
    blake3::hash(data).into()
}

/// The recovery code for a root key: its 64 hex digits in groups of 8.
pub fn recovery_code(root: &SecretKey) -> String {
    let hex = hex::encode(root.to_bytes());
    hex.as_bytes().chunks(8).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join("-")
}

/// The root key from a recovery code. Ignores everything but the hex digits.
pub fn root_from_code(code: &str) -> Result<SecretKey> {
    let hex: String = code.chars().filter(char::is_ascii_hexdigit).collect();
    let mut bytes = [0u8; 32];
    hex::decode_to_slice(&hex, &mut bytes).map_err(|_| anyhow!("a recovery code has 64 hex digits"))?;
    Ok(SecretKey::from_bytes(&bytes))
}

/// Fixed-size bytes as a hex string in JSON.
mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer, const N: usize>(bytes: &[u8; N], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>, const N: usize>(d: D) -> Result<[u8; N], D::Error> {
        let text = String::deserialize(d)?;
        let mut bytes = [0u8; N];
        hex::decode_to_slice(&text, &mut bytes).map_err(D::Error::custom)?;
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests;
