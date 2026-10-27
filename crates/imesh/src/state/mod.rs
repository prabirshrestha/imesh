//! Replicated cluster state: SQLite tables in an embedded Corrosion agent (cr-sqlite CRDTs, SWIM
//! membership, QUIC gossip).
//!
//! Every machine has the whole database. A write commits locally and then spreads to the other
//! members; conflicting writes merge per column. Corrosion gossips on this machine's loopback
//! address ([`gossip_addr`]); the daemon carries that traffic over iroh.
//!
//! Tables named `imesh_*` belong to imesh. Apps add their own tables with a schema: compiled in
//! with [`crate::daemon::Builder::schema`], or at run time with the `schema.apply` method. A
//! schema can only grow: new tables, new columns that are nullable or have a default, and
//! indexes. Corrosion refuses to drop or change tables and columns. Members pass tables to each
//! other before rows, see [`schema`].

#[cfg(any(all(target_os = "linux", target_env = "musl"), windows))]
mod crsqlite;
pub mod schema;
pub(crate) mod tunnel;

use std::{
    collections::{BTreeMap, BTreeSet},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    path::{Path, PathBuf},
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use camino::Utf8PathBuf;
use corro_agent::api::public::make_broadcastable_changes;
use corro_types::{
    agent::{Agent, ChangeError},
    broadcast::FocaInput,
    config::Config,
};
use iroh::EndpointId;
use n0_future::task::AbortOnDropHandle;
pub use rusqlite;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::{
    sync::{mpsc, watch},
    task::block_in_place,
};
use tracing::{info, warn};

/// The UDP port every machine's Corrosion gossips on, at its own loopback address.
pub const GOSSIP_PORT: u16 = 51001;

/// imesh's own tables. The cluster tables are small and read often, so the daemon keeps a copy
/// of them in memory ([`Snapshot`]).
const SCHEMA: &str = "
CREATE TABLE imesh_cluster (
    id TEXT NOT NULL PRIMARY KEY,
    founder TEXT NOT NULL DEFAULT ''
);
CREATE TABLE imesh_members (
    id TEXT NOT NULL PRIMARY KEY,
    name TEXT NOT NULL DEFAULT '',
    removed INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE imesh_machines (
    id TEXT NOT NULL PRIMARY KEY,
    info TEXT NOT NULL DEFAULT '{}'
);
CREATE TABLE imesh_clients (
    key TEXT NOT NULL PRIMARY KEY,
    name TEXT NOT NULL DEFAULT ''
);
CREATE TABLE imesh_settings (
    key TEXT NOT NULL PRIMARY KEY,
    value TEXT NOT NULL DEFAULT 'null'
);
CREATE TABLE imesh_kv (
    key TEXT NOT NULL PRIMARY KEY,
    value TEXT NOT NULL DEFAULT 'null'
);
";

/// How often the in-memory copy of the cluster tables is refreshed.
const REFRESH: Duration = Duration::from_millis(250);
/// The longest a query may run.
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);

/// What the caller's statements may do.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Access {
    /// Read any table.
    Read,
    /// Also change rows of app tables, not `imesh_*`.
    AppRows,
}

/// Lets the caller's statements do only what `access` allows. SQLite asks while it compiles a
/// statement, so this sees every table a statement touches, also through a virtual table or a
/// bind parameter. Statements that triggers run (cr-sqlite's clocks) are allowed.
/// Corrosion's settings for the machine with key `me`: its database in `dir`, gossip on its own
/// loopback address, and an HTTP API that nothing can use.
fn corrosion_config(dir: &Path, me: &EndpointId) -> Result<Config> {
    let dir = Utf8PathBuf::from_path_buf(dir.to_path_buf()).map_err(|_| anyhow!("data dir is not UTF-8"))?;
    let ip = lo(me);
    let mut conf: Config = Config::builder()
        .db_path(dir.join("state.db"))
        .gossip_addr(gossip_addr(me))
        .api_addr(SocketAddrV4::new(ip, 0).into())
        .admin_path(dir.join("admin.sock"))
        .build()?;
    // Outgoing QUIC from our own loopback address, so peers see one address per machine.
    conf.gossip.client_addr = Some(SocketAddrV4::new(ip, 0).into());
    // A restarted peer cannot reset our old QUIC connection to it (new stateless reset key), so
    // Corrosion keeps sending into it until the idle timeout. The default is 30 s.
    conf.gossip.idle_timeout_secs = 10;
    conf.db.cache_size_kib = -65536;
    // Corrosion always serves an HTTP API. Nothing may use it: imesh talks to the agent in
    // process, and a local process must not change rows past imesh's checks. The token lives
    // only in memory.
    conf.api.authorization =
        Some(corro_types::config::AuthzConfig::BearerToken(hex::encode(rand::random::<[u8; 32]>())));
    // Anti-entropy with a random member at least every 5 s instead of 15 s. It repairs what
    // gossip missed: a split, a paused machine, or rows a link held back during a schema
    // change. Cheap for clusters of tens of machines.
    conf.perf.max_sync_backoff = 5;
    Ok(conf)
}

/// Applies only what the compiled schema (imesh's tables and the app's) adds. The database may
/// already have more: columns and indexes added at run time, or taken from a member with a newer
/// app.
async fn apply_compiled_schema(agent: &Agent, app_schema: &[String]) -> Result<()> {
    let mut compiled = vec![SCHEMA.to_string()];
    compiled.extend(app_schema.iter().cloned());
    let compiled = schema::Tables::from_sql(&compiled).context("compiled schema")?;
    let conn = agent.pool().read().await.context("read connection")?;
    let existing = block_in_place(|| schema::Tables::read(&conn))?;
    drop(conn);
    let mut add = existing.catch_up(&compiled).context("the compiled schema does not fit this database")?;
    add.extend(existing.missing_indexes(&compiled));
    if !add.is_empty() {
        corro_agent::agent::util::execute_schema(agent, add).await.map_err(|e| anyhow!("apply schema: {e:#}"))?;
    }
    Ok(())
}

fn guard(c: &rusqlite::Connection, access: Access, tables: &schema::Tables) -> rusqlite::Result<()> {
    use rusqlite::hooks::{AuthAction as A, AuthContext, Authorization};
    let replicated: BTreeSet<String> = tables.0.keys().cloned().collect();
    c.authorizer(Some(move |ctx: AuthContext<'_>| {
        if ctx.accessor.is_some() {
            return Authorization::Allow;
        }
        let row_change = |table: &str| match access {
            Access::Read => false,
            Access::AppRows => replicated.contains(table) && !table.starts_with("imesh_"),
        };
        let allowed = match ctx.action {
            A::Select | A::Read { .. } | A::Recursive => true,
            A::Function { function_name } => {
                let f = function_name.to_ascii_lowercase();
                // Read-only helpers; crsql_pack_columns also runs inside crsql_changes.
                !f.starts_with("crsql")
                    || ["crsql_db_version", "crsql_site_id", "crsql_pack_columns"].contains(&f.as_str())
            }
            // Introspection only, which pragma_table_info() and its kind also run.
            A::Pragma { pragma_name, pragma_value } => {
                const LOOK: [&str; 6] =
                    ["table_info", "table_xinfo", "index_list", "index_info", "index_xinfo", "foreign_key_list"];
                pragma_value.is_none() && access == Access::Read
                    || LOOK.contains(&pragma_name.to_ascii_lowercase().as_str())
            }
            A::Insert { table_name } | A::Delete { table_name } | A::Update { table_name, .. } => {
                row_change(table_name)
            }
            _ => false,
        };
        if allowed { Authorization::Allow } else { Authorization::Deny }
    }))
}

/// The loopback address a machine's Corrosion gossips on, derived from its key. Linux and
/// Windows answer all of 127.0.0.0/8 on loopback; macOS answers only 127.0.0.1, so the daemon
/// runs on Linux and Windows.
pub fn lo(id: &EndpointId) -> Ipv4Addr {
    let b = id.as_bytes();
    Ipv4Addr::new(127, b[0].max(1), b[1], b[2].clamp(1, 254))
}

pub fn gossip_addr(id: &EndpointId) -> SocketAddr {
    SocketAddrV4::new(lo(id), GOSSIP_PORT).into()
}

/// The directory of a machine's database. It carries the key, so a machine that resets to a new
/// key never opens the old database.
pub(crate) fn store_dir(data_dir: &Path, id: &EndpointId) -> PathBuf {
    data_dir.join(format!("corrosion-{id}"))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClusterInfo {
    pub id: String,
    pub founder: EndpointId,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    pub name: String,
    /// A removal is final: the machine must reset and join with a new key.
    pub removed: bool,
}

/// What a machine says about itself in `imesh_machines`.
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

/// The cluster relay setting, the `relays` row of `imesh_settings`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RelaySettings {
    pub relays: Vec<iroh::RelayUrl>,
    /// Bearer token for these relays. Members are trusted, so it lives in the shared state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

/// The cluster tables, in memory.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub cluster: Option<ClusterInfo>,
    /// Every machine that was ever added, also removed ones. See [`Snapshot::active_members`].
    pub members: BTreeMap<EndpointId, Member>,
    /// Raw JSON, compared as text to detect changes.
    pub machines: BTreeMap<EndpointId, String>,
    /// CLI keys that may use the API over iroh, with their names.
    pub clients: BTreeMap<EndpointId, String>,
    /// Cluster-wide settings, raw JSON by key.
    pub settings: BTreeMap<String, String>,
}

impl Snapshot {
    /// Machines that are members now: listed and not removed.
    pub fn active_members(&self) -> BTreeSet<EndpointId> {
        self.members.iter().filter(|(_, m)| !m.removed).map(|(id, _)| *id).collect()
    }

    pub fn is_member(&self, id: &EndpointId) -> bool {
        self.members.get(id).is_some_and(|m| !m.removed)
    }

    pub fn is_removed(&self, id: &EndpointId) -> bool {
        self.members.get(id).is_some_and(|m| m.removed)
    }

    pub fn relay_settings(&self) -> RelaySettings {
        self.setting("relays").unwrap_or_default()
    }

    pub fn setting<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        self.settings.get(key).and_then(|s| serde_json::from_str(s).ok())
    }

    pub fn machine(&self, id: &EndpointId) -> Option<MachineInfo> {
        self.machines.get(id).and_then(|s| serde_json::from_str(s).ok())
    }
}

/// The replicated database of one machine. Cloning is cheap.
#[derive(Clone)]
pub struct Store {
    agent: Agent,
    snap: Arc<RwLock<Snapshot>>,
    /// Bumped when the cluster tables change.
    cluster: watch::Sender<u64>,
    /// The database version: bumped by every change, local or from a member, to any table.
    version: watch::Sender<i64>,
    /// Keeps the snapshot refresh running. Shared by all clones.
    _refresh: Arc<AbortOnDropHandle<()>>,
    /// Stops the agent when sent to. Dropping it does not: see [`Store::shutdown`].
    stop: mpsc::Sender<()>,
    /// This machine's tables, updated after every schema change.
    tables: watch::Sender<Arc<schema::Tables>>,
    /// Set while a schema change runs. Links carry no rows then, see [`schema`].
    changing: Arc<AtomicBool>,
    /// One schema change at a time.
    apply: Arc<tokio::sync::Mutex<()>>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Store")
    }
}

impl Store {
    /// Starts the Corrosion agent with its database under `dir`, gossiping on `lo(me)`, and
    /// applies imesh's schema and `app_schema`. Fails if a schema cannot be applied.
    pub(crate) async fn start(dir: &Path, me: &EndpointId, app_schema: &[String]) -> Result<Store> {
        std::fs::create_dir_all(dir)?;
        let conf = corrosion_config(dir, me)?;
        #[cfg(any(all(target_os = "linux", target_env = "musl"), windows))]
        crsqlite::install();
        let (tripwire, worker, stop) = tripwire::Tripwire::new_simple();
        tokio::spawn(worker);
        let (agent, _bookie, _transport, _handles) = corro_agent::agent::start_with_config(conf, tripwire)
            .await
            .map_err(|e| anyhow!("start corrosion: {e:#}"))?;
        apply_compiled_schema(&agent, app_schema).await?;
        let snap = Arc::new(RwLock::new(Snapshot::default()));
        let (cluster, version) = (watch::channel(0).0, watch::channel(0).0);
        let refresh = {
            let (agent, snap, cluster, version) = (agent.clone(), snap.clone(), cluster.clone(), version.clone());
            AbortOnDropHandle::new(tokio::spawn(async move {
                loop {
                    if let Err(e) = refresh(&agent, &snap, &cluster, &version).await {
                        warn!("refresh snapshot: {e:#}");
                    }
                    tokio::time::sleep(REFRESH).await;
                }
            }))
        };
        let store = Store {
            agent,
            snap,
            cluster,
            version,
            _refresh: Arc::new(refresh),
            stop,
            tables: watch::channel(Arc::default()).0,
            changing: Default::default(),
            apply: Default::default(),
        };
        store.refresh().await?;
        store.read_tables().await?;
        Ok(store)
    }

    /// Stops the Corrosion agent. The database closes when the last clone is dropped.
    pub(crate) async fn shutdown(&self) {
        let _ = self.stop.send(()).await;
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snap.read().unwrap().clone()
    }

    pub fn read<T>(&self, f: impl FnOnce(&Snapshot) -> T) -> T {
        f(&self.snap.read().unwrap())
    }

    /// Fires when the cluster tables change.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.cluster.subscribe()
    }

    /// Fires on every change to any table, local or from a member, with the new database
    /// version. Apps watch it and query the tables they care about.
    pub fn changes(&self) -> watch::Receiver<i64> {
        self.version.subscribe()
    }

    async fn refresh(&self) -> Result<()> {
        refresh(&self.agent, &self.snap, &self.cluster, &self.version).await
    }

    /// Runs a read-only query on a pooled connection. Statements that write or change tables,
    /// transactions, and ATTACH are refused, and a query that runs longer than 30 s is stopped.
    pub async fn query<T>(&self, f: impl FnOnce(&rusqlite::Connection) -> rusqlite::Result<T>) -> Result<T> {
        let conn = self.agent.pool().read().await.context("read connection")?;
        block_in_place(|| {
            guard(&conn, Access::Read, &self.tables())?;
            let deadline = std::time::Instant::now() + QUERY_TIMEOUT;
            conn.progress_handler(10_000, Some(move || std::time::Instant::now() > deadline))?;
            let result = f(&conn);
            conn.progress_handler(0, None::<fn() -> bool>)?;
            conn.authorizer(None::<fn(rusqlite::hooks::AuthContext<'_>) -> rusqlite::hooks::Authorization>)?;
            // The pool keeps connections as they are; never return one inside a transaction.
            if !conn.is_autocommit() {
                conn.execute_batch("ROLLBACK")?;
            }
            Ok(result?)
        })
    }

    /// Runs statements in one transaction and sends the changes to the other members. Returns
    /// after the transaction commits here. Only rows may change: a transaction that changes a
    /// table or an index fails, because members take tables only through a schema.
    pub async fn write<T>(&self, f: impl FnOnce(&rusqlite::Connection) -> rusqlite::Result<T>) -> Result<T> {
        let actor_id = self.agent.actor_id();
        let (v, _, _) = make_broadcastable_changes(&self.agent, None, |tx| {
            let run = || {
                let version =
                    |c: &rusqlite::Connection| c.query_row("PRAGMA schema_version", [], |r| r.get::<_, i64>(0));
                let before = version(tx)?;
                let v = f(tx)?;
                if version(tx)? != before {
                    return Err(rusqlite::Error::ModuleError(
                        "a write may change rows only; use a schema for tables".into(),
                    ));
                }
                Ok(v)
            };
            run().map_err(|source| ChangeError::Rusqlite { source, actor_id: Some(actor_id), version: None })
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;
        // The write is committed. A failed refresh only delays the in-memory copy.
        if let Err(e) = self.refresh().await {
            warn!("refresh after write: {e:#}");
        }
        Ok(v)
    }

    /// Runs the caller's statements in one transaction, as [`Store::write`] does. SQLite's
    /// authorizer checks each statement while it compiles: rows of app tables only, never
    /// `imesh_*`, tables, indexes, or Corrosion's and cr-sqlite's own tables. The check is off
    /// while the statements run, so cr-sqlite can keep its own books.
    pub(crate) async fn execute_app(
        &self,
        statements: Vec<(String, Vec<rusqlite::types::Value>)>,
    ) -> Result<Vec<usize>> {
        let tables = self.tables();
        self.write(|c| {
            let mut counts = Vec::with_capacity(statements.len());
            for (sql, params) in &statements {
                guard(c, Access::AppRows, &tables)?;
                let st = c.prepare(sql);
                c.authorizer(None::<fn(rusqlite::hooks::AuthContext<'_>) -> rusqlite::hooks::Authorization>)?;
                counts.push(st?.execute(rusqlite::params_from_iter(params))?);
            }
            Ok(counts)
        })
        .await
    }

    /// Applies `CREATE TABLE` and `CREATE INDEX` statements on this machine. A table that is
    /// already there may only grow. Links carry no rows while this runs, and afterwards only to
    /// members that have the same tables.
    pub async fn apply_schema(&self, sql: Vec<String>) -> Result<()> {
        if sql.iter().all(|s| s.trim().is_empty()) {
            bail!("empty schema");
        }
        schema::check_statements(&sql)?;
        let _one = self.apply.lock().await;
        self.apply_locked(sql).await
    }

    /// Applies what a member has and this machine lacks. Fails with a conflict when both have
    /// columns the other lacks.
    pub(crate) async fn catch_up(&self, theirs: &schema::Tables) -> Result<()> {
        let _one = self.apply.lock().await;
        ensure!(theirs.0.len() <= schema::MAX_TABLES, "the member sent more than {} tables", schema::MAX_TABLES);
        let sql = self.tables().catch_up(theirs).context("schema conflict")?;
        if sql.is_empty() {
            return Ok(());
        }
        // The statements come from a member: they get the same checks as a local schema.
        schema::check_statements(&sql).context("the member's tables")?;
        self.apply_locked(sql).await
    }

    async fn apply_locked(&self, sql: Vec<String>) -> Result<()> {
        self.changing.store(true, Ordering::SeqCst);
        let applied = corro_agent::agent::util::execute_schema(&self.agent, sql).await.map_err(|e| anyhow!("{e:#}"));
        // The links compare against the tables read here. Until a read succeeds, this machine's
        // real tables are unknown, so the gate stays closed.
        let mut tries = 0;
        while let Err(e) = self.read_tables().await {
            tries += 1;
            if tries >= 20 {
                return Err(e.context("read tables after a schema change; links stay closed"));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        self.changing.store(false, Ordering::SeqCst);
        applied?;
        info!("schema applied");
        Ok(())
    }

    /// This machine's tables.
    pub fn tables(&self) -> Arc<schema::Tables> {
        self.tables.borrow().clone()
    }

    /// Fires when this machine's tables change.
    pub fn watch_tables(&self) -> watch::Receiver<Arc<schema::Tables>> {
        self.tables.subscribe()
    }

    /// Whether rows may pass to or from a member that reported `theirs`.
    pub(crate) fn rows_may_pass(&self, theirs: &schema::Tables) -> bool {
        !self.changing.load(Ordering::SeqCst) && self.tables.borrow().same_shape(theirs)
    }

    async fn read_tables(&self) -> Result<()> {
        let tables = self.query(schema::Tables::read).await?;
        self.tables.send_if_modified(|t| {
            let changed = **t != tables;
            if changed {
                *t = Arc::new(tables);
            }
            changed
        });
        Ok(())
    }

    /// Asks the SWIM layer to contact a machine now instead of at the next announce interval.
    pub(crate) async fn announce(&self, id: &EndpointId) {
        let _ = self.agent.tx_foca().send(FocaInput::Announce(gossip_addr(id).into())).await;
    }

    /// Tells SWIM, and through it every member, that a removed machine is down for good.
    /// Otherwise Corrosion keeps probing it until its own failure detector gives up.
    pub(crate) async fn forget(&self, id: &EndpointId) {
        let addr = gossip_addr(id);
        let down: Vec<_> = self
            .agent
            .members()
            .read()
            .states
            .iter()
            .filter(|(_, m)| m.addr == addr)
            .map(|(a, m)| foca::Member::new(m.to_actor(*a), 0, foca::State::Down))
            .collect();
        if !down.is_empty() {
            let _ = self.agent.tx_foca().send(FocaInput::ApplyMany(down)).await;
        }
    }

    /// Whether SWIM currently counts the machine as a live member.
    pub(crate) fn swim_alive(&self, id: &EndpointId) -> bool {
        self.agent.members().read().by_addr.contains_key(&gossip_addr(id))
    }
}

/// Reads the cluster tables and the database version, and bumps the watchers that changed.
async fn refresh(
    agent: &Agent,
    snap: &RwLock<Snapshot>,
    cluster: &watch::Sender<u64>,
    version: &watch::Sender<i64>,
) -> Result<()> {
    let conn = agent.pool().read().await.context("read connection")?;
    let (next, db_version) = block_in_place(|| read_snapshot(&conn))?;
    version.send_if_modified(|v| std::mem::replace(v, db_version) != db_version);
    let mut cur = snap.write().unwrap();
    if *cur != next {
        *cur = next;
        drop(cur);
        cluster.send_modify(|v| *v += 1);
    }
    Ok(())
}

fn read_snapshot(c: &rusqlite::Connection) -> rusqlite::Result<(Snapshot, i64)> {
    let mut s = Snapshot::default();
    let row = c.query_row("SELECT id, founder FROM imesh_cluster LIMIT 1", [], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    });
    if let Ok((id, founder)) = row
        && let Ok(founder) = founder.parse()
    {
        s.cluster = Some(ClusterInfo { id, founder });
    }
    for (id, name, removed) in rows(c, "SELECT id, name, removed FROM imesh_members", |r| {
        Ok((r.get::<_, String>(0)?, r.get(1)?, r.get::<_, i64>(2)?))
    })? {
        if let Ok(id) = id.parse() {
            s.members.insert(id, Member { name, removed: removed != 0 });
        }
    }
    for (id, info) in rows(c, "SELECT id, info FROM imesh_machines", |r| Ok((r.get::<_, String>(0)?, r.get(1)?)))? {
        if let Ok(id) = id.parse() {
            s.machines.insert(id, info);
        }
    }
    for (key, name) in rows(c, "SELECT key, name FROM imesh_clients", |r| Ok((r.get::<_, String>(0)?, r.get(1)?)))? {
        if let Ok(key) = key.parse() {
            s.clients.insert(key, name);
        }
    }
    s.settings =
        rows(c, "SELECT key, value FROM imesh_settings", |r| Ok((r.get(0)?, r.get(1)?)))?.into_iter().collect();
    let db_version = c.query_row("SELECT crsql_db_version()", [], |r| r.get(0))?;
    Ok((s, db_version))
}

fn rows<T>(
    c: &rusqlite::Connection,
    sql: &str,
    f: impl FnMut(&rusqlite::Row) -> rusqlite::Result<T>,
) -> rusqlite::Result<Vec<T>> {
    c.prepare(sql)?.query_map([], f)?.collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> rusqlite::Connection {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE imesh_kv (key TEXT NOT NULL PRIMARY KEY, value TEXT);
             CREATE TABLE app (id TEXT NOT NULL PRIMARY KEY, v TEXT);
             CREATE TABLE __corro_bookkeeping (a);",
        )
        .unwrap();
        c
    }

    fn allowed(access: Access, sql: &str) -> bool {
        let c = conn();
        let tables = schema::Tables::read(&c).unwrap();
        guard(&c, access, &tables).unwrap();
        c.prepare(sql).is_ok()
    }

    #[test]
    fn reads_are_read_only() {
        assert!(allowed(Access::Read, "SELECT * FROM app JOIN imesh_kv"));
        assert!(allowed(Access::Read, "SELECT name FROM pragma_table_info('app')"));
        for sql in [
            "INSERT INTO app (id) VALUES ('x')",
            "BEGIN",
            "SAVEPOINT s",
            "ATTACH ':memory:' AS other",
            "CREATE TABLE t (a)",
            "PRAGMA journal_mode = DELETE",
        ] {
            assert!(!allowed(Access::Read, sql), "{sql}");
        }
    }

    #[test]
    fn the_schema_reader_passes_the_read_guard() {
        let c = conn();
        let before = schema::Tables::read(&c).unwrap();
        guard(&c, Access::Read, &before).unwrap();
        assert_eq!(schema::Tables::read(&c).unwrap(), before);
    }

    #[test]
    fn writes_change_rows_of_replicated_tables_only() {
        assert!(allowed(Access::AppRows, "INSERT INTO app (id) VALUES ('x')"));
        assert!(!allowed(Access::AppRows, "UPDATE imesh_kv SET value = 'v'"));
        assert!(allowed(Access::AppRows, "DELETE FROM app"));
        for sql in [
            "DELETE FROM __corro_bookkeeping",
            "CREATE TABLE t (a)",
            "ALTER TABLE app ADD COLUMN w TEXT",
            "DROP TABLE app",
            "CREATE INDEX i ON app (v)",
            "BEGIN",
        ] {
            assert!(!allowed(Access::AppRows, sql), "{sql}");
        }
    }
}
