//! Built-in API methods. Every caller that reaches a method is already authorized: the local
//! socket is protected by file permissions, and iroh callers passed the admission hook.

use anyhow::{Result, ensure};
use iroh::EndpointId;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    api::{self, Inspect, JoinTicket, RelaysInfo, Served},
    daemon::{Builder, Caller, Machine},
    state::RelaySettings,
};

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
        .method("cluster.rename", rename)
        .method("cluster.promote", promote)
        .method("cluster.recover", recover)
        .method("cluster.machines", machines)
        .method("relays.get", relays_get)
        .method("relays.set", relays_set)
}

#[derive(Deserialize)]
struct Id {
    id: EndpointId,
}

fn ensure_local(caller: Caller, what: &str) -> Result<()> {
    ensure!(caller == Caller::Local, "only this machine's own operator can {what}");
    Ok(())
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
    let role = m.role();
    Ok(json!(Inspect {
        id: m.id(),
        info: m.info(),
        cluster,
        member,
        role,
        removed: m.cut_off.load(std::sync::atomic::Ordering::Relaxed) || m.store.read(|s| s.is_removed(&m.id())),
        relays: m.config.net.relays.clone(),
        n0: m.config.net.n0,
        devtunnel_mesh: m.config.net.devtunnel_mesh,
        alone: m.alone(),
    }))
}

#[derive(Deserialize)]
struct ResetParams {
    /// Reset only while the machine is alone; see [`Machine::alone`].
    #[serde(default)]
    if_alone: bool,
}

/// Resets the machine to a new key, out of any cluster, once the reply has left. See
/// [`Machine::reset`].
async fn reset(m: Machine, caller: Caller, p: Value) -> Result<Value> {
    let p: ResetParams = api::params(p)?;
    m.reset(caller, p.if_alone)?;
    Ok(Value::Null)
}

/// Only the machine's own operator may make it leave, through its local socket.
async fn leave(m: Machine, caller: Caller, _: Value) -> Result<Value> {
    ensure!(caller == Caller::Local, "a member cannot make another machine leave");
    let (by, tunnel) = m.leave().await?;
    Ok(json!({ "removed_by": by, "tunnel": tunnel }))
}

#[derive(Deserialize)]
struct InitParams {
    name: Option<String>,
}

async fn init(m: Machine, caller: Caller, p: Value) -> Result<Value> {
    ensure_local(caller, "start a cluster")?;
    let p: InitParams = api::params(p)?;
    Ok(json!(m.init_cluster(p.name).await?))
}

#[derive(Deserialize)]
struct AddParams {
    id: EndpointId,
    name: String,
    /// The new machine's mesh tunnel, so this machine can reach it before its location arrives.
    #[serde(default)]
    tunnel: Option<crate::state::TunnelEndpoint>,
}

async fn add(m: Machine, caller: Caller, p: Value) -> Result<Value> {
    let p: AddParams = api::params(p)?;
    Ok(json!(m.admit(caller, p.id, &p.name, p.tunnel).await?))
}

async fn join(m: Machine, caller: Caller, p: Value) -> Result<Value> {
    ensure_local(caller, "join a cluster")?;
    let ticket: JoinTicket = api::params(p)?;
    Ok(json!(m.join(&ticket).await?))
}

#[derive(Deserialize)]
struct RemoveParams {
    id: EndpointId,
    #[serde(default)]
    cascade: bool,
}

async fn remove(m: Machine, caller: Caller, p: Value) -> Result<Value> {
    let p: RemoveParams = api::params(p)?;
    m.remove_member(caller, p.id, p.cascade).await?;
    Ok(Value::Null)
}

#[derive(Deserialize)]
struct RenameParams {
    id: EndpointId,
    name: String,
}

async fn rename(m: Machine, caller: Caller, p: Value) -> Result<Value> {
    let p: RenameParams = api::params(p)?;
    m.rename_member(caller, p.id, &p.name)?;
    Ok(Value::Null)
}

async fn promote(m: Machine, caller: Caller, p: Value) -> Result<Value> {
    ensure_local(caller, "promote a machine")?;
    let p: Id = api::params(p)?;
    m.promote(p.id)?;
    Ok(Value::Null)
}

#[derive(Deserialize)]
struct RecoverParams {
    code: String,
}

async fn recover(m: Machine, caller: Caller, p: Value) -> Result<Value> {
    ensure_local(caller, "move the owner")?;
    let p: RecoverParams = api::params(p)?;
    m.recover(&p.code)?;
    Ok(Value::Null)
}

#[derive(Deserialize, Default)]
struct MachinesParams {
    /// Say hello to every member first, to show who answers now.
    #[serde(default)]
    probe: bool,
}

async fn machines(m: Machine, caller: Caller, p: Value) -> Result<Value> {
    let p: MachinesParams = api::params(p)?;
    // A probe makes this machine say hello to every member, so only its own operator may ask.
    Ok(json!(m.machines(p.probe && caller == Caller::Local).await))
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

async fn relays_set(m: Machine, caller: Caller, p: Value) -> Result<Value> {
    let set: RelaySettings = api::params(p)?;
    m.set_cluster_relays(caller, &set).await?;
    Ok(Value::Null)
}
