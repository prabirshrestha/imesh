//! Replicated cluster state: one iroh-docs document per cluster.
//!
//! Every machine writes as its own author, and the author key is the machine key, so an entry's
//! author is the machine's EndpointId. iroh-docs keeps one entry per (author, key). Entries arrive
//! only over connections that the admission hook allowed, so all members are trusted equally, as
//! in POC A. Reads resolve the entries of a key like POC A: a removal of `member/<id>` is final,
//! otherwise the latest timestamp wins, then the larger author key.
//!
//! Every key is stored with a trailing NUL. iroh-docs drops older entries of the same author whose
//! key starts with a newer entry's key (`ranger.rs:554`), so without the terminator a write to
//! `app/k` would delete that author's `app/k2`.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Duration,
};

use anyhow::{Context, Result};
use bytes::Bytes;
use futures_util::StreamExt;
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};
use iroh_blobs::{Hash, api::downloader::Downloader, store::fs::FsStore};
use iroh_docs::{
    Author, AuthorId, Capability, Entry, NamespaceId, NamespaceSecret,
    api::{Doc, protocol::{AddrInfoOptions, ShareMode}},
    engine::LiveEvent,
    protocol::Docs,
    store::Query,
};
use iroh_gossip::net::Gossip;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::{debug, warn};

/// The cluster a machine belongs to. Kept in `cluster.json` and written as the `cluster` entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterInfo {
    /// Hex of the namespace id.
    pub id: String,
    pub founder: EndpointId,
}

impl ClusterInfo {
    fn namespace(&self) -> Result<NamespaceId> {
        let b: [u8; 32] = hex::decode(&self.id)?.try_into().ok().context("bad cluster id")?;
        Ok(NamespaceId::from(b))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberInfo {
    pub name: String,
}

/// What a machine says about itself under `machine/<id>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// Membership resolved from the entries, cached for the admission hook.
#[derive(Debug, Default, Clone)]
pub struct View {
    /// A `cluster` entry is present.
    pub founded: bool,
    /// Machines with a `member/<id>` removal. Removal is final.
    pub removed: BTreeSet<EndpointId>,
    pub members: BTreeSet<EndpointId>,
    pub clients: BTreeSet<EndpointId>,
}

impl View {
    /// The entry that wins for one key, or `None` when the key is absent or deleted.
    fn resolve<'a>(&self, key: &str, entries: &'a [Entry]) -> Option<&'a Entry> {
        if let Some(id) = key.strip_prefix("member/")
            && id.parse::<EndpointId>().is_ok_and(|id| self.removed.contains(&id))
        {
            return None;
        }
        entries
            .iter()
            .max_by_key(|e| (e.timestamp(), e.author()))
            .filter(|e| !e.record().is_empty())
    }
}

fn key_of(e: &Entry) -> Option<String> {
    let k = e.key().strip_suffix(b"\0")?;
    String::from_utf8(k.to_vec()).ok()
}

fn stored(key: &str) -> Bytes {
    let mut k = key.as_bytes().to_vec();
    k.push(0);
    k.into()
}

pub struct State {
    dir: PathBuf,
    author: AuthorId,
    pub docs: Docs,
    pub blobs: FsStore,
    pub gossip: Gossip,
    downloader: Downloader,
    me: EndpointId,
    doc: RwLock<Option<(Doc, ClusterInfo)>>,
    view: Arc<RwLock<View>>,
    changes: broadcast::Sender<()>,
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("State")
    }
}

impl State {
    /// Opens the docs and blobs stores under `dir` and the cluster document, if any.
    pub async fn open(dir: &Path, secret: &SecretKey, endpoint: &Endpoint, view: Arc<RwLock<View>>) -> Result<Arc<Self>> {
        let blobs = FsStore::load(dir.join("blobs")).await?;
        let gossip = Gossip::builder().spawn(endpoint.clone());
        std::fs::create_dir_all(dir.join("docs"))?;
        let docs = Docs::persistent(dir.join("docs"))
            .spawn(endpoint.clone(), (*blobs).clone(), gossip.clone())
            .await?;
        let downloader = blobs.downloader(endpoint);
        let author = Author::from(secret.clone());
        docs.author_import(author.clone()).await?;
        let state = Arc::new(State {
            dir: dir.to_path_buf(),
            author: author.id(),
            docs,
            blobs,
            gossip,
            downloader,
            me: secret.public(),
            doc: RwLock::new(None),
            view,
            changes: broadcast::channel(64).0,
        });
        if let Ok(b) = std::fs::read(dir.join("cluster.json")) {
            let info: ClusterInfo = serde_json::from_slice(&b).context("corrupt cluster.json")?;
            let doc = state.docs.open(info.namespace()?).await?.context("cluster document missing")?;
            state.attach(doc, info).await?;
        }
        Ok(state)
    }

    /// Makes `doc` the cluster document: pins it, follows its events, and turns on live sync.
    async fn attach(self: &Arc<Self>, doc: Doc, info: ClusterInfo) -> Result<()> {
        crate::config::write_private(&self.dir.join("cluster.json"), &serde_json::to_vec(&info)?)?;
        let mut events = doc.subscribe().await?;
        *self.doc.write().unwrap() = Some((doc.clone(), info));
        self.refresh().await?;
        doc.start_sync(vec![]).await?;
        // The event consumer must not call into docs: iroh-docs awaits every subscriber send, so
        // a consumer that waits on the docs actors closes a cycle and stalls all of docs.
        let dirty = Arc::new(tokio::sync::Notify::new());
        tokio::spawn({
            let dirty = dirty.clone();
            async move {
                while let Some(ev) = events.next().await {
                    match ev {
                        Ok(LiveEvent::InsertLocal { entry }) | Ok(LiveEvent::InsertRemote { entry, .. })
                            if !entry.key().starts_with(b"app/") => dirty.notify_one(),
                        Ok(LiveEvent::ContentReady { .. }) => dirty.notify_one(),
                        Err(e) => warn!("doc events: {e:#}"),
                        Ok(_) => {}
                    }
                }
            }
        });
        let me = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                dirty.notified().await;
                let Some(s) = me.upgrade() else { return };
                if let Err(e) = s.refresh().await {
                    warn!("refresh view: {e:#}");
                }
                let _ = s.changes.send(());
            }
        });
        Ok(())
    }

    /// Creates the cluster document with this machine as founder.
    pub async fn init(self: &Arc<Self>, founder: EndpointId) -> Result<ClusterInfo> {
        let secret = NamespaceSecret::from_bytes(&rand::random());
        let doc = self.docs.import_namespace(Capability::Write(secret)).await?;
        let info = ClusterInfo { id: hex::encode(doc.id().as_bytes()), founder };
        self.attach(doc, info.clone()).await?;
        self.put("cluster", Some(&info)).await?;
        Ok(info)
    }

    /// Imports the cluster document from its write capability and syncs with `peers`.
    pub async fn join(self: &Arc<Self>, info: ClusterInfo, secret: [u8; 32], peers: Vec<EndpointAddr>) -> Result<()> {
        let secret = NamespaceSecret::from_bytes(&secret);
        anyhow::ensure!(secret.id() == info.namespace()?, "secret does not match cluster {}", info.id);
        let doc = self.docs.import_namespace(Capability::Write(secret)).await?;
        if self.doc.read().unwrap().is_none() {
            self.attach(doc.clone(), info).await?;
        }
        doc.start_sync(peers).await
    }

    /// The namespace secret: write capability for the whole document.
    pub async fn secret(&self) -> Result<[u8; 32]> {
        let ticket = self.doc()?.share(ShareMode::Write, AddrInfoOptions::Id).await?;
        match ticket.capability {
            Capability::Write(s) => Ok(s.to_bytes()),
            Capability::Read(_) => anyhow::bail!("read-only document"),
        }
    }

    /// Runs a sync with each peer now and makes them gossip neighbors.
    pub async fn sync_with(&self, peers: Vec<EndpointAddr>) -> Result<()> {
        self.doc()?.start_sync(peers).await
    }

    fn doc(&self) -> Result<Doc> {
        Ok(self.doc.read().unwrap().as_ref().context("machine is not in a cluster")?.0.clone())
    }

    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.changes.subscribe()
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn cluster(&self) -> Option<ClusterInfo> {
        self.doc.read().unwrap().as_ref().map(|d| d.1.clone())
    }

    pub fn view(&self) -> View {
        self.view.read().unwrap().clone()
    }

    pub fn members(&self) -> BTreeSet<EndpointId> {
        self.view.read().unwrap().members.clone()
    }

    /// Writes an entry as this machine. `None` deletes.
    pub async fn put<T: Serialize>(&self, key: &str, value: Option<&T>) -> Result<u64> {
        let doc = self.doc()?;
        match value {
            Some(v) => doc.set_bytes(self.author, stored(key), serde_json::to_vec(v)?).await.map(|_| ())?,
            None => doc.del(self.author, stored(key)).await.map(|_| ())?,
        }
        let e = doc.get_exact(self.author, stored(key), true).await?.context("entry vanished")?;
        Ok(e.timestamp())
    }

    async fn entries(&self, doc: &Doc, q: Query) -> Result<Vec<Entry>> {
        let s = doc.get_many(q).await?;
        s.collect::<Vec<_>>().await.into_iter().collect()
    }

    /// Winning entries by key under `prefix`, without deleted keys.
    pub async fn list_entries(&self, prefix: &str) -> Result<BTreeMap<String, Entry>> {
        let Ok(doc) = self.doc() else { return Ok(BTreeMap::new()) };
        let all = self.entries(&doc, Query::key_prefix(prefix).include_empty().build()).await?;
        let by_key = group(all);
        let view = self.view();
        Ok(by_key
            .into_iter()
            .filter_map(|(k, es)| view.resolve(&k, &es).cloned().map(|e| (k, e)))
            .collect())
    }

    /// Content of an entry. Fetches it from members when it is missing here.
    pub async fn content(&self, e: &Entry) -> Option<Bytes> {
        if let Ok(b) = self.blobs.blobs().get_bytes(e.content_hash()).await {
            return Some(b);
        }
        self.fetch(e.content_hash()).await;
        self.blobs.blobs().get_bytes(e.content_hash()).await.ok()
    }

    async fn fetch(&self, hash: Hash) {
        let peers: Vec<EndpointId> = self.members().into_iter().filter(|id| *id != self.me).collect();
        let dl = self.downloader.download(hash, peers);
        match tokio::time::timeout(Duration::from_secs(10), dl).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => debug!(%hash, "fetch: {e:#}"),
            Err(_) => debug!(%hash, "fetch: timeout"),
        }
    }

    /// Downloads content that iroh-docs gave up on. A failed download stays missing until a
    /// neighbor announces the same hash again (`engine/live.rs:661`, `live.rs:735`).
    pub async fn repair(&self) -> Result<()> {
        let Ok(doc) = self.doc() else { return Ok(()) };
        let hashes: BTreeSet<Hash> = self.entries(&doc, Query::all().build()).await?.iter().map(|e| e.content_hash()).collect();
        for h in hashes {
            if !self.blobs.blobs().has(h).await.unwrap_or(false) {
                self.fetch(h).await;
            }
        }
        Ok(())
    }

    pub async fn list<T: for<'a> Deserialize<'a>>(&self, prefix: &str) -> Result<Vec<(String, Option<T>)>> {
        let mut out = Vec::new();
        for (k, e) in self.list_entries(prefix).await? {
            let v = self.content(&e).await.and_then(|b| serde_json::from_slice(&b).ok());
            out.push((k, v));
        }
        Ok(out)
    }

    pub async fn get<T: for<'a> Deserialize<'a>>(&self, key: &str) -> Result<Option<T>> {
        let Ok(doc) = self.doc() else { return Ok(None) };
        let es = self.entries(&doc, Query::key_exact(stored(key)).include_empty().build()).await?;
        let Some(e) = self.view().resolve(key, &es).cloned() else { return Ok(None) };
        Ok(self.content(&e).await.and_then(|b| serde_json::from_slice(&b).ok()))
    }

    /// Recomputes membership from the `cluster`, `member/` and `client/` entries.
    pub async fn refresh(&self) -> Result<()> {
        let Some((doc, _)) = self.doc.read().unwrap().clone() else { return Ok(()) };
        let q = |p: &str| Query::key_prefix(p).include_empty().build();
        let cluster = self.entries(&doc, Query::key_exact(stored("cluster")).build()).await?;
        let mut v = View { founded: !cluster.is_empty(), ..Default::default() };
        let members = group(self.entries(&doc, q("member/")).await?);
        for (k, es) in &members {
            if es.iter().any(|e| e.record().is_empty())
                && let Ok(id) = k["member/".len()..].parse()
            {
                v.removed.insert(id);
            }
        }
        for (prefix, set) in [("member/", &members), ("client/", &group(self.entries(&doc, q("client/")).await?))] {
            for (k, es) in set {
                if v.resolve(k, es).is_some()
                    && let Ok(id) = k[prefix.len()..].parse()
                {
                    if prefix == "member/" { v.members.insert(id); } else { v.clients.insert(id); }
                }
            }
        }
        *self.view.write().unwrap() = v;
        Ok(())
    }
}

fn group(entries: Vec<Entry>) -> BTreeMap<String, Vec<Entry>> {
    let mut by_key: BTreeMap<String, Vec<Entry>> = BTreeMap::new();
    for e in entries {
        if let Some(k) = key_of(&e) {
            by_key.entry(k).or_default().push(e);
        }
    }
    by_key
}
