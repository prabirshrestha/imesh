//! Built-in API methods. Every caller that reaches a method is already authorized: the local
//! socket is protected by file permissions, and iroh callers passed the admission hook.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail, ensure};
use iroh::EndpointId;
use rusqlite::{OptionalExtension, params, types::ValueRef};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    api::{self, Inspect, JoinTicket, RelaysInfo, Served},
    daemon::{Builder, Caller, Machine},
    state::{RelaySettings, Snapshot},
};

/// The largest `sql.query` result.
const MAX_ROWS: usize = 100_000;
const MAX_RESULT: usize = 64 << 20;

pub(crate) fn register(b: Builder) -> Builder {
    b.method("ping", ping)
        .method("methods", methods)
        .method("whoami", whoami)
        .method("machine.inspect", inspect)
        .method("machine.reset", reset)
        .method("machine.leave", leave)
        .method("cluster.init", init)
        .method("cluster.add", add)
        .method("cluster.join", join)
        .method("cluster.remove", remove)
        .method("cluster.leave", member_leave)
        .method("cluster.rename", rename)
        .method("cluster.machines", machines)
        .method("relays.get", relays_get)
        .method("relays.set", relays_set)
        .method("schema.apply", schema_apply)
        .method("schema.list", schema_list)
        .method("sql.query", sql_query)
        .method("sql.execute", sql_execute)
        .method("kv.put", kv_put)
        .method("kv.get", kv_get)
        .method("kv.list", kv_list)
        .method("kv.rm", kv_rm)
}

#[derive(Deserialize)]
struct Id {
    id: EndpointId,
}

async fn ping(_: Machine, _: Caller, _: Value) -> Result<Value> {
    Ok(json!("pong"))
}

async fn methods(m: Machine, _: Caller, _: Value) -> Result<Value> {
    Ok(json!(m.methods.keys().collect::<Vec<_>>()))
}

async fn whoami(_: Machine, c: Caller, _: Value) -> Result<Value> {
    Ok(json!(c))
}

async fn inspect(m: Machine, _: Caller, _: Value) -> Result<Value> {
    let (cluster, member) = m.store.read(|s| (s.cluster.as_ref().map(|c| c.id.clone()), s.is_member(&m.id())));
    Ok(json!(Inspect {
        id: m.id(),
        info: m.info(),
        cluster,
        member,
        relays: m.config.net.relays.clone(),
        n0: m.config.net.n0,
        devtunnel_mesh: m.config.net.devtunnel_mesh,
        gossip_port_base: m.store.gossip_base,
        alone_and_empty: m.alone_and_empty().await?,
    }))
}

/// Resets the machine to a new key, out of any cluster, once the reply has left. See
/// [`Machine::request_reset`].
async fn reset(m: Machine, _: Caller, _: Value) -> Result<Value> {
    m.reset_after_reply();
    Ok(Value::Null)
}

/// Only the machine's own operator may make it leave, through its local socket.
async fn leave(m: Machine, caller: Caller, _: Value) -> Result<Value> {
    ensure!(caller == Caller::Local, "a member cannot make another machine leave");
    let (by, tunnel) = m.leave().await?;
    Ok(json!({ "removed_by": by, "tunnel": tunnel }))
}

/// A member asks to leave the cluster. Only for the calling member itself.
async fn member_leave(m: Machine, caller: Caller, _: Value) -> Result<Value> {
    let Caller::Member(id) = caller else { bail!("only a member can leave, and only itself") };
    m.accept_leave(id)?;
    Ok(Value::Null)
}

#[derive(Deserialize)]
struct InitParams {
    name: Option<String>,
}

async fn init(m: Machine, _: Caller, p: Value) -> Result<Value> {
    let p: InitParams = api::params(p)?;
    let cluster = m.init_cluster(p.name).await?;
    Ok(json!({ "cluster": cluster, "id": m.id() }))
}

#[derive(Deserialize)]
struct AddParams {
    id: EndpointId,
    name: String,
    /// The new machine's mesh tunnel, so this machine can reach it before its row arrives.
    #[serde(default)]
    tunnel: Option<crate::state::TunnelEndpoint>,
}

async fn add(m: Machine, _: Caller, p: Value) -> Result<Value> {
    let p: AddParams = api::params(p)?;
    Ok(json!(m.admit(p.id, &p.name, p.tunnel).await?))
}

async fn join(m: Machine, _: Caller, p: Value) -> Result<Value> {
    let ticket: JoinTicket = api::params(p)?;
    Ok(json!(m.join(&ticket).await?))
}

async fn remove(m: Machine, _: Caller, p: Value) -> Result<Value> {
    let p: Id = api::params(p)?;
    m.remove_member(p.id).await?;
    Ok(Value::Null)
}

#[derive(serde::Deserialize)]
struct RenameParams {
    id: EndpointId,
    name: String,
}

async fn rename(m: Machine, _: Caller, p: Value) -> Result<Value> {
    let p: RenameParams = api::params(p)?;
    m.rename_member(p.id, &p.name).await?;
    Ok(Value::Null)
}

async fn machines(m: Machine, _: Caller, _: Value) -> Result<Value> {
    Ok(json!(m.machines()))
}

async fn relays_get(m: Machine, _: Caller, _: Value) -> Result<Value> {
    let snap = m.store.snapshot();
    let set = snap.relay_settings();
    let served = snap
        .machines
        .keys()
        .filter_map(|id| snap.machine(id))
        .filter_map(|i| Some(Served { url: i.relay?, machine: i.name }))
        .collect();
    let in_use = m.relays.lock().await.keys().cloned().collect();
    Ok(json!(RelaysInfo { cluster: set.relays, token_set: set.token.is_some(), served, in_use }))
}

async fn relays_set(m: Machine, _: Caller, p: Value) -> Result<Value> {
    let set: RelaySettings = api::params(p)?;
    m.set_cluster_relays(&set).await?;
    Ok(Value::Null)
}

#[derive(Deserialize)]
struct SchemaParams {
    sql: String,
}

async fn schema_apply(m: Machine, _: Caller, p: Value) -> Result<Value> {
    let p: SchemaParams = api::params(p)?;
    m.apply_schema(&p.sql).await?;
    Ok(Value::Null)
}

/// This machine's tables, and for each member whether rows pass: `ok`, or why not.
async fn schema_list(m: Machine, _: Caller, _: Value) -> Result<Value> {
    let tables: BTreeMap<String, String> = m.store.tables().0.iter().map(|(n, t)| (n.clone(), t.sql.clone())).collect();
    let snap: Snapshot = m.store.snapshot();
    let members: BTreeMap<String, Option<String>> = snap
        .members
        .iter()
        .filter(|(id, mb)| !mb.removed && **id != m.id())
        .map(|(id, mb)| (mb.name.clone(), m.peer_status(id).schema))
        .collect();
    Ok(json!({ "tables": tables, "members": members }))
}

#[derive(Deserialize)]
struct SqlParams {
    sql: String,
    #[serde(default)]
    params: Vec<Value>,
}

/// Runs one read-only statement on this machine's copy of the database. Returns the column
/// names and the rows.
async fn sql_query(m: Machine, _: Caller, p: Value) -> Result<Value> {
    let p: SqlParams = api::params(p)?;
    let (columns, rows) = m
        .store
        .query(|c| {
            let mut st = c.prepare(&p.sql)?;
            let columns: Vec<String> = st.column_names().into_iter().map(String::from).collect();
            let n = columns.len();
            let mut rows = Vec::new();
            let mut size = 0;
            let mut cursor = st.query(rusqlite::params_from_iter(p.params.iter().map(to_sql)))?;
            while let Some(r) = cursor.next()? {
                let row = (0..n).map(|i| r.get_ref(i).map(from_sql)).collect::<rusqlite::Result<Vec<Value>>>()?;
                size += row.iter().map(|v| v.to_string().len()).sum::<usize>();
                if rows.len() >= MAX_ROWS || size > MAX_RESULT {
                    return Err(rusqlite::Error::ModuleError(format!(
                        "the result is larger than {MAX_ROWS} rows or {} MiB; add a LIMIT",
                        MAX_RESULT >> 20
                    )));
                }
                rows.push(row);
            }
            Ok((columns, rows))
        })
        .await
        .context("sql.query runs one read-only statement")?;
    Ok(json!({ "columns": columns, "rows": rows }))
}

#[derive(Deserialize)]
struct ExecuteParams {
    statements: Vec<SqlParams>,
}

/// Runs statements in one transaction and sends the changes to every member. Returns the number
/// of rows each statement changed.
async fn sql_execute(m: Machine, _: Caller, p: Value) -> Result<Value> {
    let p: ExecuteParams = api::params(p)?;
    ensure!(!p.statements.is_empty(), "no statements");
    // The store's write guard allows only rows of app tables. This check gives a clearer error.
    for s in &p.statements {
        let first = s.sql.split_whitespace().next().unwrap_or_default().to_ascii_lowercase();
        if !["insert", "update", "delete", "replace", "with"].contains(&first.as_str()) {
            bail!("sql.execute runs INSERT, UPDATE, DELETE, and REPLACE; use schema.apply for tables");
        }
    }
    let statements = p.statements.iter().map(|s| (s.sql.clone(), s.params.iter().map(to_sql).collect())).collect();
    let counts = m.store.execute_app(statements).await?;
    Ok(json!(counts))
}

fn to_sql(v: &Value) -> rusqlite::types::Value {
    use rusqlite::types::Value as S;
    match v {
        Value::Null => S::Null,
        Value::Bool(b) => S::Integer(*b as i64),
        Value::Number(n) => n.as_i64().map(S::Integer).unwrap_or_else(|| S::Real(n.as_f64().unwrap_or_default())),
        Value::String(s) => S::Text(s.clone()),
        other => S::Text(other.to_string()),
    }
}

fn from_sql(v: ValueRef) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => json!(i),
        ValueRef::Real(f) => json!(f),
        ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
        ValueRef::Blob(b) => json!(data_encoding::BASE64.encode(b)),
    }
}

#[derive(Deserialize)]
struct KvParams {
    key: String,
    #[serde(default)]
    value: Option<Value>,
    /// Seconds until the key expires. None: it does not expire.
    #[serde(default)]
    ttl: Option<u64>,
}

async fn kv_put(m: Machine, _: Caller, p: Value) -> Result<Value> {
    let p: KvParams = api::params(p)?;
    m.kv_put(&p.key, p.value.as_ref(), p.ttl.map(std::time::Duration::from_secs)).await?;
    Ok(Value::Null)
}

async fn kv_rm(m: Machine, _: Caller, p: Value) -> Result<Value> {
    let p: KvParams = api::params(p)?;
    m.kv_put(&p.key, None, None).await?;
    Ok(Value::Null)
}

async fn kv_get(m: Machine, _: Caller, p: Value) -> Result<Value> {
    let p: KvParams = api::params(p)?;
    let now = super::cluster::now_secs();
    let v: Option<String> = m
        .store
        .query(move |c| {
            c.query_row(
                "SELECT value FROM imesh_kv WHERE key = ?1 AND (expires_at IS NULL OR expires_at >= ?2)",
                params![p.key, now],
                |r| r.get(0),
            )
            .optional()
        })
        .await?;
    Ok(v.and_then(|v| serde_json::from_str(&v).ok()).unwrap_or(Value::Null))
}

#[derive(Deserialize, Default)]
struct KvListParams {
    #[serde(default)]
    prefix: String,
}

async fn kv_list(m: Machine, _: Caller, p: Value) -> Result<Value> {
    let p: KvListParams = api::params(p)?;
    let now = super::cluster::now_secs();
    let rows: Vec<(String, String)> = m
        .store
        .query(move |c| {
            c.prepare(
                "SELECT key, value FROM imesh_kv WHERE substr(key, 1, length(?1)) = ?1 \
                 AND (expires_at IS NULL OR expires_at >= ?2) ORDER BY key",
            )?
            .query_map(params![p.prefix, now], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect()
        })
        .await?;
    let out: BTreeMap<_, Value> =
        rows.into_iter().map(|(k, v)| (k, serde_json::from_str(&v).unwrap_or(Value::Null))).collect();
    Ok(json!(out))
}
