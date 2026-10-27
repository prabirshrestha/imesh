//! The `imesh machine` commands: install and join machines, list and remove them.

use anyhow::{Context as _, Result, bail, ensure};
use iroh::{EndpointAddr, EndpointId};
use serde_json::{Value, json};

use imesh::{
    api::{ClientRow, Inspect, JoinTicket, Joined, MachineRow, RelaysInfo},
    client::Client,
};

use crate::{
    cli::At,
    config::{self, Config, Connection},
    install::{self, Mode, Target},
};

/// The connections the CLI keeps for a machine installed over SSH: SSH first, then iroh.
fn connections(target: &Target, id: EndpointId) -> Vec<Connection> {
    let r = &target.settings.net;
    vec![
        Connection { machine: Some(id), ..target.ssh() },
        Connection {
            iroh: Some(id),
            relay: r.devtunnel.as_ref().map(|t| t.relay_url()).or_else(|| r.relays.first().cloned()),
            devtunnel: r.devtunnel.as_ref().map(|t| t.to_string()),
            relay_token: r.relay_token.clone(),
            no_n0: r.no_n0,
            machine: Some(id),
            ..Default::default()
        },
    ]
}

pub(crate) async fn init(at: &At, target: Target) -> Result<()> {
    let c = install::provision(&target, Mode::New).await?;
    let i: Inspect = c.call(None, "machine.inspect", ()).await?;
    let v: Value = c.call(None, "cluster.init", json!({ "name": target.name })).await?;
    let key = config::client_key()?.public();
    let name = std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_else(|_| "cli".into());
    c.call::<Value>(None, "client.add", ClientRow { key, name }).await?;
    c.close().await;
    let mut cfg = Config::load()?;
    let name = cfg.add_context(at.context.as_deref().unwrap_or("default"), connections(&target, i.id));
    cfg.save()?;
    println!("cluster {} initialised on {} ({})", v["cluster"].as_str().unwrap_or_default(), target.dest, i.id);
    println!("current context is now {name:?}");
    Ok(())
}

/// Adds connections for a new machine to the context, skipping ones it has.
fn remember(at: &At, conns: Vec<Connection>) -> Result<()> {
    at.edit_context(|ctx| {
        for conn in conns {
            if !ctx.connections.iter().any(|c| c.machine == conn.machine && c.ssh == conn.ssh && c.iroh == conn.iroh) {
                ctx.connections.push(conn);
            }
        }
    })
}

pub(crate) async fn add(at: &At, entry: &Client, mut target: Target) -> Result<()> {
    if let Ok(ticket) = target.dest.parse::<iroh_tickets::endpoint::EndpointTicket>() {
        return add_ticket(at, entry, &target, ticket.endpoint_addr().clone()).await;
    }
    // A new machine uses the entry machine's relays unless told otherwise.
    let r = &mut target.settings.net;
    if r.relays.is_empty() && r.devtunnel.is_none() && !r.no_n0 {
        let e: Inspect = entry.call(None, "machine.inspect", ()).await?;
        (r.relays, r.no_n0) = (e.relays, e.no_n0);
    }
    let c = install::provision(&target, Mode::New).await?;
    let i: Inspect = c.call(None, "machine.inspect", ()).await?;
    let name = target.name.clone().unwrap_or(i.info.name);
    let ticket: JoinTicket = entry.call(None, "cluster.add", json!({ "id": i.id, "name": name })).await?;
    let joined: Joined = c.call(None, "cluster.join", ticket).await?;
    c.close().await;
    remember(at, connections(&target, i.id))?;
    println!("machine {name} ({}) joined cluster {} with {} members", i.id, joined.cluster, joined.members);
    Ok(())
}

/// Admits a machine that installed itself, and prints the ticket it joins with. The machine's
/// key is admitted before it connects, so the cluster accepts it by its iroh identity.
async fn add_ticket(at: &At, entry: &Client, target: &Target, addr: EndpointAddr) -> Result<()> {
    let id = addr.id;
    let name = target.name.clone().context("adding by ticket needs --name <name>")?;
    let ticket: JoinTicket = entry.call(None, "cluster.add", json!({ "id": id, "name": name })).await?;
    println!("admitted {name} ({id}). On that machine, run:\n\n  imesh machine join {ticket}\n");
    let conn = Connection {
        iroh: Some(id),
        relay: addr.relay_urls().next().cloned(),
        addrs: addr.ip_addrs().copied().collect(),
        machine: Some(id),
        ..Default::default()
    };
    remember(at, vec![conn])
}

pub(crate) async fn resolve(c: &Client, m: &str) -> Result<EndpointId> {
    if let Ok(id) = m.parse() {
        return Ok(id);
    }
    let all: Vec<MachineRow> = c.call(None, "cluster.machines", ()).await?;
    let found: Vec<_> = all.iter().filter(|r| r.name == m).collect();
    match found.as_slice() {
        [one] => Ok(one.id),
        [] => bail!("no machine named {m:?}"),
        _ => bail!("{} machines are named {m:?}; use the ID from `imesh machine ls -w`", found.len()),
    }
}

pub(crate) async fn ls(c: &Client, wide: bool) -> Result<()> {
    let rows: Vec<MachineRow> = c.call(None, "cluster.machines", ()).await?;
    let w = if wide { 64 } else { 10 };
    println!("{:<12} {:<w$} {:<8} {:<7} {:<8} {:<14} VERSION", "NAME", "ID", "STATE", "PATH", "RTT", "OS/ARCH");
    for r in rows {
        let s = &r.status;
        let id = r.id.to_string();
        let (os, version) = match &r.info {
            Some(i) => (format!("{}/{}", i.os, i.arch), i.version.as_str()),
            None => ("?/?".into(), ""),
        };
        println!(
            "{:<12} {:<w$} {:<8} {:<7} {:<8} {:<14} {version}",
            r.name,
            &id[..w],
            s.state.to_string(),
            s.path.as_deref().unwrap_or("-"),
            s.rtt_ms.map_or("-".into(), |v| format!("{v}ms")),
            os,
        );
        if wide {
            if let Some(t) = s.last_seen {
                let ago =
                    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs().saturating_sub(t);
                println!("{:12} last contact {ago}s ago", "");
            }
            if let Some(e) = &s.error {
                println!("{:12} error: {e}", "");
            }
        }
    }
    Ok(())
}

pub(crate) async fn rm(at: &At, c: &Client, m: &str, force: bool) -> Result<()> {
    let id = resolve(c, m).await?;
    let entry: Inspect = c.call(None, "machine.inspect", ()).await?;
    ensure!(entry.id != id, "you are connected through {m}; connect through another machine to remove it");
    // From the replicated state, so the guard holds also when the machine is down.
    let relays: RelaysInfo = c.call(None, "relays.get", ()).await?;
    let rows: Vec<MachineRow> = c.call(None, "cluster.machines", ()).await?;
    let served = rows.into_iter().find(|r| r.id == id).and_then(|r| r.info).and_then(|i| i.relay);
    if let Some(url) = served
        && relays.cluster.contains(&url)
    {
        ensure!(
            force,
            "{m} serves {url}, which the cluster relay setting uses. Change it with `imesh relay set` first, or pass --force"
        );
    }
    match c.call::<Value>(Some(id), "machine.reset", ()).await {
        Ok(_) => eprintln!("reset {m}"),
        Err(e) => eprintln!(
            "could not reset {m} ({e:#}). It is removed anyway; reset it later with `imesh --connect <it> call machine.reset`."
        ),
    }
    c.call::<Value>(None, "cluster.remove", json!({ "id": id })).await?;
    at.edit_context(|ctx| ctx.connections.retain(|c| c.machine != Some(id)))?;
    println!("removed {m} ({id})");
    Ok(())
}
