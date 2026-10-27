//! Replicated cluster state: a map of records, last writer wins.
//!
//! Every machine keeps every record. Records arrive only over iroh connections that the
//! admission hook allowed, so every sender is a member and all members are trusted equally.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail, ensure};
use iroh::EndpointId;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::sync::{Notify, broadcast, watch};

/// The largest record, key plus value. Records are metadata; keep payloads in iroh-blobs or
/// iroh-docs on an app ALPN.
pub const MAX_RECORD: usize = 1 << 20;

/// Record keys. Every key the daemon reads or writes is built here.
pub(crate) mod sync;

pub mod keys {
    use iroh::EndpointId;

    pub const CLUSTER: &str = "cluster";
    pub const RELAYS: &str = "settings/relays";
    pub const MEMBER: &str = "member/";
    pub const MACHINE: &str = "machine/";
    pub const CLIENT: &str = "client/";
    pub const APP: &str = "app/";

    pub fn member(id: &EndpointId) -> String {
        format!("{MEMBER}{id}")
    }
    pub fn machine(id: &EndpointId) -> String {
        format!("{MACHINE}{id}")
    }
    pub fn client(id: &EndpointId) -> String {
        format!("{CLIENT}{id}")
    }
    pub fn app(key: &str) -> String {
        format!("{APP}{key}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub key: String,
    /// JSON bytes. Empty means deleted.
    pub value: Vec<u8>,
    /// Hybrid logical clock: milliseconds << 16 | counter.
    pub version: u64,
    pub author: EndpointId,
}

/// What anti-entropy compares: enough of a record to know which side has the winner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    pub version: u64,
    pub author: EndpointId,
    pub deleted: bool,
}

impl Record {
    pub fn is_deleted(&self) -> bool {
        self.value.is_empty()
    }

    pub fn json<T: DeserializeOwned>(&self) -> Option<T> {
        serde_json::from_slice(&self.value).ok()
    }

    pub fn stamp(&self) -> Stamp {
        Stamp { version: self.version, author: self.author, deleted: self.is_deleted() }
    }
}

/// The one order for two versions of a key. Merge and anti-entropy both use it, so they cannot
/// disagree. A member removal beats any add, whatever the clocks say.
pub fn compare(key: &str, a: &Stamp, b: &Stamp) -> Ordering {
    if key.starts_with(keys::MEMBER) && a.deleted != b.deleted {
        return if a.deleted { Ordering::Greater } else { Ordering::Less };
    }
    (a.version, a.author.as_bytes()).cmp(&(b.version, b.author.as_bytes()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterInfo {
    pub id: String,
    pub founder: EndpointId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberInfo {
    pub name: String,
}

/// A CLI key under `client/<key>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInfo {
    #[serde(default)]
    pub name: String,
}

/// What a machine says about itself under `machine/<id>`.
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

/// The cluster relay setting under `settings/relays`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RelaySettings {
    pub relays: Vec<iroh::RelayUrl>,
    /// Bearer token for these relays. Members are trusted, so it lives in the shared state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

/// A change to the state, and whether this machine wrote it.
#[derive(Debug, Clone)]
pub struct Change {
    pub record: Record,
    pub local: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Data {
    records: BTreeMap<String, Record>,
    clock: u64,
    /// Changes applied since start. `commit` waits until this many are on disk.
    #[serde(skip)]
    changes: u64,
    /// Set by reset: no more merges or writes.
    #[serde(skip)]
    frozen: bool,
}

pub struct State {
    me: EndpointId,
    path: Option<PathBuf>,
    data: Mutex<Data>,
    changes: broadcast::Sender<Change>,
    dirty: Notify,
    /// Held while the state file is written, so reset cannot interleave with a write.
    file: Mutex<()>,
    /// How many changes are on disk.
    written: watch::Sender<u64>,
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("State")
    }
}

impl State {
    pub(crate) fn open(me: EndpointId, path: Option<PathBuf>) -> Result<Self> {
        let data = match &path {
            Some(p) => match std::fs::read(p) {
                Ok(b) => postcard::from_bytes(&b).context("corrupt state file")?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Data::default(),
                Err(e) => return Err(e.into()),
            },
            None => Data::default(),
        };
        Ok(State {
            me,
            path,
            data: Mutex::new(data),
            changes: broadcast::channel(4096).0,
            dirty: Notify::new(),
            file: Mutex::new(()),
            written: watch::channel(0).0,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Change> {
        self.changes.subscribe()
    }

    /// Writes a record as this machine. `None` deletes.
    pub fn put<T: Serialize>(&self, key: &str, value: Option<&T>) -> Result<Record> {
        let value = match value {
            Some(v) => serde_json::to_vec(v)?,
            None => Vec::new(),
        };
        ensure!(key.len() + value.len() <= MAX_RECORD, "record {key} is over {MAX_RECORD} bytes");
        // One lock for the version and the store, so two writes to one key keep their order.
        let mut d = self.data.lock().unwrap();
        let now = (SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64) << 16;
        d.clock = now.max(d.clock.saturating_add(1));
        let rec = Record { key: key.to_string(), value, version: d.clock, author: self.me };
        ensure!(self.apply(&mut d, rec.clone(), true)?, "local write lost");
        Ok(rec)
    }

    /// Offers a record from a member. Returns whether it won and was stored.
    pub(crate) fn merge(&self, rec: Record) -> Result<bool> {
        self.apply(&mut self.data.lock().unwrap(), rec, false)
    }

    /// Merges a batch. Returns how many records won.
    pub(crate) fn merge_all(&self, recs: Vec<Record>) -> usize {
        recs.into_iter().map(|r| self.merge(r)).filter(|r| matches!(r, Ok(true))).count()
    }

    /// Stores `rec` if it wins. Sends the change while the lock is held, so subscribers see
    /// changes in the order they were stored.
    fn apply(&self, d: &mut Data, rec: Record, local: bool) -> Result<bool> {
        if d.frozen {
            bail!("state is being reset");
        }
        ensure!(rec.key.len() + rec.value.len() <= MAX_RECORD, "record {} is over {MAX_RECORD} bytes", rec.key);
        d.clock = d.clock.max(rec.version);
        if let Some(cur) = d.records.get(&rec.key)
            && compare(&rec.key, &rec.stamp(), &cur.stamp()) != Ordering::Greater
        {
            return Ok(false);
        }
        if rec.key == keys::CLUSTER
            && let (Some(cur), Some(new)) =
                (d.records.get(keys::CLUSTER).and_then(|r| r.json::<ClusterInfo>()), rec.json::<ClusterInfo>())
        {
            ensure!(cur.id == new.id, "record belongs to another cluster");
        }
        d.records.insert(rec.key.clone(), rec.clone());
        d.changes += 1;
        self.dirty.notify_one();
        let _ = self.changes.send(Change { record: rec, local });
        Ok(true)
    }

    /// Writes the state file after changes. Writes that arrive within 20 ms share one file write.
    pub(crate) async fn flush_loop(self: Arc<Self>) {
        loop {
            self.dirty.notified().await;
            tokio::time::sleep(Duration::from_millis(20)).await;
            let state = self.clone();
            match tokio::task::spawn_blocking(move || state.flush()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!("write state: {e:#}"),
                Err(e) => tracing::warn!("write state: {e}"),
            }
        }
    }

    /// Waits until every change made so far is on disk. The API calls it before it answers, so
    /// an acknowledged write survives a crash.
    pub(crate) async fn commit(&self) -> Result<()> {
        if self.path.is_none() {
            return Ok(());
        }
        let target = {
            let d = self.data.lock().unwrap();
            ensure!(!d.frozen, "state is being reset");
            d.changes
        };
        if *self.written.borrow() >= target {
            return Ok(());
        }
        let mut rx = self.written.subscribe();
        self.dirty.notify_one();
        tokio::time::timeout(Duration::from_secs(10), rx.wait_for(|w| *w >= target))
            .await
            .context("state was not written within 10 s")?
            .context("state writer stopped")?;
        Ok(())
    }

    /// Writes the state file. Blocks: call it off the async threads.
    pub(crate) fn flush(&self) -> Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        let _file = self.file.lock().unwrap();
        // Copy under the lock and encode outside it, so reads and writes do not wait on it.
        let (copy, changes) = {
            let d = self.data.lock().unwrap();
            if d.frozen {
                return Ok(());
            }
            (Data { records: d.records.clone(), clock: d.clock, ..Default::default() }, d.changes)
        };
        let bytes = postcard::to_stdvec(&copy)?;
        crate::client::config::write_private(path, &bytes)?;
        self.written.send_replace(changes);
        Ok(())
    }

    /// Forgets every record. Used when a join fails, so the machine is not half joined.
    pub(crate) fn clear(&self) {
        let mut d = self.data.lock().unwrap();
        d.records.clear();
        d.changes += 1;
        drop(d);
        self.dirty.notify_one();
    }

    /// Stops all merges and writes, and forgets every record. Used by reset. It waits for a
    /// file write in progress, so no write can land after it returns.
    pub(crate) fn freeze(&self) {
        let _file = self.file.lock().unwrap();
        let mut d = self.data.lock().unwrap();
        d.frozen = true;
        d.records.clear();
    }

    pub fn get(&self, key: &str) -> Option<Record> {
        self.data.lock().unwrap().records.get(key).cloned()
    }

    fn get_json<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        self.get(key).and_then(|r| r.json())
    }

    /// Every record under `prefix`, removals included.
    pub fn list(&self, prefix: &str) -> Vec<Record> {
        let d = self.data.lock().unwrap();
        d.records
            .range(prefix.to_string()..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .map(|(_, r)| r.clone())
            .collect()
    }

    /// The records under `prefix` that are not removals, with the rest of each key.
    pub fn live(&self, prefix: &str) -> Vec<(String, Record)> {
        self.list(prefix)
            .into_iter()
            .filter(|r| !r.is_deleted())
            .map(|r| (r.key[prefix.len()..].to_string(), r))
            .collect()
    }

    /// Summary for anti-entropy: the stamp of every record.
    pub(crate) fn summary(&self) -> Vec<(String, Stamp)> {
        let d = self.data.lock().unwrap();
        d.records.values().map(|r| (r.key.clone(), r.stamp())).collect()
    }

    /// Records that win here over what `theirs` holds, or that `theirs` lacks.
    pub(crate) fn newer_than(&self, theirs: &[(String, Stamp)]) -> Vec<Record> {
        let theirs: BTreeMap<_, _> = theirs.iter().map(|(k, s)| (k, s)).collect();
        let d = self.data.lock().unwrap();
        d.records
            .values()
            .filter(|r| theirs.get(&r.key).is_none_or(|s| compare(&r.key, &r.stamp(), s) == Ordering::Greater))
            .cloned()
            .collect()
    }

    pub fn cluster(&self) -> Option<ClusterInfo> {
        self.get_json(keys::CLUSTER)
    }

    pub fn machine(&self, id: &EndpointId) -> Option<MachineInfo> {
        self.get_json(&keys::machine(id))
    }

    pub fn member(&self, id: &EndpointId) -> Option<MemberInfo> {
        self.get_json(&keys::member(id))
    }

    /// Whether `member/<id>` is a removal.
    pub fn is_removed(&self, id: &EndpointId) -> bool {
        self.get(&keys::member(id)).is_some_and(|r| r.is_deleted())
    }

    /// Machines that are members now: a `member/<id>` record that is not a removal.
    pub fn members(&self) -> BTreeSet<EndpointId> {
        self.ids(keys::MEMBER)
    }

    /// Keys the CLI may connect with over iroh, besides members.
    pub fn clients(&self) -> BTreeSet<EndpointId> {
        self.ids(keys::CLIENT)
    }

    fn ids(&self, prefix: &str) -> BTreeSet<EndpointId> {
        self.live(prefix).into_iter().filter_map(|(id, _)| id.parse().ok()).collect()
    }

    pub fn relay_settings(&self) -> RelaySettings {
        self.get_json(keys::RELAYS).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(key: &str, value: &str, version: u64, author: u8) -> Record {
        Record {
            key: key.into(),
            value: value.as_bytes().to_vec(),
            version,
            author: iroh::SecretKey::from_bytes(&[author; 32]).public(),
        }
    }

    /// Removal on one side, a later re-add on the other: both sides must end with the removal,
    /// whichever direction anti-entropy runs.
    #[test]
    fn removal_survives_anti_entropy_both_ways() {
        let a = State::open(rec("x", "", 0, 1).author, None).unwrap();
        let b = State::open(rec("x", "", 0, 2).author, None).unwrap();
        let removal = rec("member/m", "", 10, 1);
        let later_add = rec("member/m", "{\"name\":\"m\"}", 20, 2);
        a.merge(removal.clone()).unwrap();
        b.merge(later_add).unwrap();

        let to_b = a.newer_than(&b.summary());
        let to_a = b.newer_than(&a.summary());
        b.merge_all(to_b);
        a.merge_all(to_a);

        assert_eq!(a.get("member/m").unwrap(), removal);
        assert_eq!(b.get("member/m").unwrap(), removal);
        assert!(a.newer_than(&b.summary()).is_empty());
        assert!(b.newer_than(&a.summary()).is_empty());
    }

    #[test]
    fn later_write_wins_for_other_keys() {
        let a = State::open(rec("x", "", 0, 1).author, None).unwrap();
        a.merge(rec("app/k", "\"old\"", 10, 1)).unwrap();
        assert!(a.merge(rec("app/k", "\"new\"", 20, 2)).unwrap());
        assert!(!a.merge(rec("app/k", "\"older\"", 5, 3)).unwrap());
        assert_eq!(a.get("app/k").unwrap().value, b"\"new\"");
    }

    #[test]
    fn frozen_state_accepts_nothing() {
        let a = State::open(rec("x", "", 0, 1).author, None).unwrap();
        a.freeze();
        assert!(a.merge(rec("app/k", "1", 1, 1)).is_err());
        assert!(a.get("app/k").is_none());
    }
}
