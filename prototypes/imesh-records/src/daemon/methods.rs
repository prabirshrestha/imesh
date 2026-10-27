//! Built-in API methods. Every caller that reaches a method is already authorized: the local
//! socket is protected by file permissions, and iroh callers passed the admission hook.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use iroh::EndpointId;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    api::{self, Admit, ClientRow, Inspect, Joined, MachineRow, RelaysInfo, Served},
    daemon::{Builder, Caller, Machine},
    net,
    state::sync::{self, SYNC_ALPN},
    state::{ClientInfo, ClusterInfo, MemberInfo, RelaySettings, keys},
};

pub(crate) fn register(b: Builder) -> Builder {
    b.method("ping", ping)
        .method("methods", methods)
        .method("whoami", whoami)
        .method("machine.inspect", inspect)
        .method("machine.reset", reset)
        .method("cluster.init", init)
        .method("cluster.add", add)
        .method("cluster.join", join)
        .method("cluster.remove", remove)
        .method("cluster.machines", machines)
        .method("client.add", client_add)
        .method("client.remove", client_remove)
        .method("client.list", client_list)
        .method("relays.get", relays_get)
        .method("relays.set", relays_set)
        .method("kv.put", kv_put)
        .method("kv.get", kv_get)
        .method("kv.list", kv_list)
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
    Ok(json!(Inspect {
        id: m.id(),
        info: m.info(),
        cluster: m.state.cluster().map(|c| c.id),
        member: m.members().contains(&m.id()),
        relays: m.opts.relay.relays.clone(),
        no_n0: m.opts.relay.no_n0,
    }))
}

/// Resets the machine to a new key, out of any cluster. The owner of the machine stops it and
/// deletes its data; see [`crate::daemon::serve`] and [`crate::daemon::Running`].
async fn reset(m: Machine, _: Caller, _: Value) -> Result<Value> {
    tokio::spawn(async move {
        // Let the reply reach the caller. Then stop all merges and writes, so nothing writes the
        // old state back.
        tokio::time::sleep(Duration::from_millis(200)).await;
        m.state.freeze();
        m.reset.send_replace(true);
    });
    Ok(Value::Null)
}

#[derive(Deserialize)]
struct InitParams {
    name: Option<String>,
}

async fn init(m: Machine, _: Caller, p: Value) -> Result<Value> {
    let p: InitParams = api::params(p)?;
    ensure!(m.state.cluster().is_none(), "machine is already in a cluster");
    let id = hex::encode(rand::random::<[u8; 16]>());
    m.state.put(keys::CLUSTER, Some(&ClusterInfo { id: id.clone(), founder: m.id() }))?;
    m.state.put(&keys::member(&m.id()), Some(&MemberInfo { name: p.name.unwrap_or_else(|| m.name.clone()) }))?;
    m.online().await;
    m.publish_self();
    Ok(json!({ "cluster": id, "id": m.id() }))
}

#[derive(Deserialize)]
struct AddParams {
    id: EndpointId,
    name: String,
}

async fn add(m: Machine, _: Caller, p: Value) -> Result<Value> {
    let p: AddParams = api::params(p)?;
    let cluster = m.state.cluster().context("machine is not in a cluster")?;
    ensure!(!m.state.is_removed(&p.id), "machine {} was removed; reset it to get a new identity", p.id.fmt_short());
    ensure!(!member_names(&m).into_iter().any(|(id, n)| n == p.name && id != p.id), "name {:?} is taken", p.name);
    m.state.put(&keys::member(&p.id), Some(&MemberInfo { name: p.name }))?;
    m.online().await;
    let others = m.members().into_iter().filter(|id| *id != m.id() && *id != p.id);
    let peers = std::iter::once(m.endpoint.addr()).chain(others.map(|id| m.peer_addr(id))).collect();
    Ok(json!(Admit { cluster: cluster.id, peers, relays: m.state.relay_settings() }))
}

/// Joins a cluster: syncs from the given peers until this machine sees its own member record.
async fn join(m: Machine, _: Caller, p: Value) -> Result<Value> {
    let p: Admit = api::params(p)?;
    if let Some(c) = m.state.cluster() {
        bail!("machine is already in cluster {}", c.id);
    }
    // Use the cluster's relays before dialing, so a machine that can reach only those relays
    // can join.
    *m.joining.lock().unwrap() = net::relays(&p.relays.relays, &p.relays.token);
    m.apply_cluster_relays().await;
    m.online().await;
    m.admission.set_bootstrap(p.peers.iter().map(|a| a.id).collect());
    let mut result = Err(anyhow!("no peers to join"));
    for addr in &p.peers {
        m.lookup.set_endpoint_info(addr.clone());
        let id = addr.id.fmt_short();
        let synced = async {
            let conn = m.dial(addr.clone(), SYNC_ALPN).await?;
            sync::exchange(&m.state, &conn).await
        };
        result = match synced.await {
            Ok(_) if m.members().contains(&m.id()) => Ok(()),
            Ok(_) => Err(anyhow!("peer {id} has not admitted us")),
            Err(e) => Err(e.context(format!("sync with {id}"))),
        };
        if result.is_ok() {
            break;
        }
    }
    m.admission.set_bootstrap(BTreeSet::new());
    m.joining.lock().unwrap().clear();
    m.apply_cluster_relays().await;
    let result = result.and_then(|()| {
        let c = m.state.cluster().context("no cluster record after sync")?;
        ensure!(c.id == p.cluster, "peers are in cluster {}, expected {}", c.id, p.cluster);
        Ok(())
    });
    if let Err(e) = result {
        // Do not stay half joined, and drop the links the join started.
        m.state.clear();
        m.reconcile_links();
        return Err(e);
    }
    m.publish_self();
    m.reconcile_links();
    Ok(json!(Joined { cluster: p.cluster, members: m.members().len() }))
}

async fn remove(m: Machine, _: Caller, p: Value) -> Result<Value> {
    let p: Id = api::params(p)?;
    ensure!(p.id != m.id(), "a machine cannot remove itself; connect through another machine");
    ensure!(m.members().contains(&p.id), "not a member");
    m.state.put::<()>(&keys::member(&p.id), None)?;
    m.state.put::<()>(&keys::machine(&p.id), None)?;
    Ok(Value::Null)
}

fn member_names(m: &Machine) -> BTreeMap<EndpointId, String> {
    m.members().into_iter().map(|id| (id, m.state.member(&id).map(|i| i.name).unwrap_or_default())).collect()
}

async fn machines(m: Machine, _: Caller, _: Value) -> Result<Value> {
    let rows: Vec<_> = member_names(&m)
        .into_iter()
        .map(|(id, name)| MachineRow { id, name, info: m.state.machine(&id), status: m.peer_status(&id) })
        .collect();
    Ok(json!(rows))
}

async fn client_add(m: Machine, _: Caller, p: Value) -> Result<Value> {
    let p: ClientRow = api::params(p)?;
    ensure!(m.state.cluster().is_some(), "machine is not in a cluster");
    m.state.put(&keys::client(&p.key), Some(&ClientInfo { name: p.name }))?;
    Ok(Value::Null)
}

async fn client_remove(m: Machine, _: Caller, p: Value) -> Result<Value> {
    let p: ClientRow = api::params(p)?;
    ensure!(m.state.clients().contains(&p.key), "not an authorized CLI key");
    m.state.put::<()>(&keys::client(&p.key), None)?;
    Ok(Value::Null)
}

async fn client_list(m: Machine, _: Caller, _: Value) -> Result<Value> {
    let rows: Vec<ClientRow> = m
        .state
        .live(keys::CLIENT)
        .into_iter()
        .filter_map(|(key, r)| Some(ClientRow { key: key.parse().ok()?, name: r.json::<ClientInfo>()?.name }))
        .collect();
    Ok(json!(rows))
}

async fn relays_get(m: Machine, _: Caller, _: Value) -> Result<Value> {
    let set = m.state.relay_settings();
    let served = m
        .state
        .live(keys::MACHINE)
        .into_iter()
        .filter_map(|(_, r)| r.json::<crate::state::MachineInfo>())
        .filter_map(|i| Some(Served { url: i.relay?, machine: i.name }))
        .collect();
    let in_use = m.relays.lock().await.keys().cloned().collect();
    Ok(json!(RelaysInfo { cluster: set.relays, token_set: set.token.is_some(), served, in_use }))
}

async fn relays_set(m: Machine, _: Caller, p: Value) -> Result<Value> {
    let set: RelaySettings = api::params(p)?;
    m.state.put(keys::RELAYS, Some(&set))?;
    Ok(Value::Null)
}

#[derive(Deserialize)]
struct KvParams {
    key: String,
    #[serde(default)]
    value: Option<Value>,
}

async fn kv_put(m: Machine, _: Caller, p: Value) -> Result<Value> {
    let p: KvParams = api::params(p)?;
    let r = m.state.put(&keys::app(&p.key), p.value.as_ref())?;
    Ok(json!({ "version": r.version }))
}

async fn kv_get(m: Machine, _: Caller, p: Value) -> Result<Value> {
    let p: KvParams = api::params(p)?;
    Ok(json!(m.state.get(&keys::app(&p.key)).and_then(|r| r.json::<Value>())))
}

async fn kv_list(m: Machine, _: Caller, _: Value) -> Result<Value> {
    let out: BTreeMap<_, _> = m.state.live(keys::APP).into_iter().map(|(k, r)| (k, r.json::<Value>())).collect();
    Ok(json!(out))
}
