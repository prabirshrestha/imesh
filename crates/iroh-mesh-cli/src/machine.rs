//! Commands for the machines of a cluster: add servers over SSH, list, rename, remove, and
//! update them. Each returns a typed result that prints as text, JSON, or through `--jq`.

use anyhow::{Context as _, Result, bail, ensure};
use iroh::EndpointId;
use serde::Serialize;
use serde_json::{Value, json};

use iroh_mesh::{
    api::{Inspect, JoinTicket, Joined, MachineRow, RelaysInfo},
    client::Client,
};

use crate::{
    cli::At,
    config::{Config, Connection},
    install::{self, Mode, Target},
    output::Output,
};

/// The connection the CLI keeps for a server installed over SSH.
fn connection(target: &Target, id: EndpointId, platform: Option<&str>) -> Connection {
    Connection { machine: Some(id), platform: platform.map(String::from), ..target.ssh() }
}

/// The result of `add` and `update`.
#[derive(Debug, Serialize)]
pub struct Installed {
    pub id: EndpointId,
    pub name: String,
    pub target: String,
    pub cluster: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub members: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// The SSH alias written to ~/.ssh/imesh/config.
    pub ssh_alias: Option<String>,
    /// Why no SSH alias was written, when that failed after the machine joined.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_alias_error: Option<String>,
    pub version: String,
}

/// Saves the SSH alias after a successful install.
fn save_local(target: &Target, id: EndpointId, name: &str, platform: Option<&str>) -> (Option<String>, Option<String>) {
    if target.no_ssh_config || target.docker || target.resolved.is_none() {
        return (None, None);
    }
    let mut conn = connection(target, id, platform);
    conn.docker = None;
    match crate::ssh_config::write(id, name, target.ssh_alias.as_deref(), &conn) {
        Ok(a) => (Some(a), None),
        Err(e) => {
            eprintln!("warning: the machine is installed, but the SSH alias was not written: {e:#}");
            (None, Some(format!("{e:#}")))
        }
    }
}

fn platform_of(i: &Inspect) -> Option<&'static str> {
    (i.info.os == "windows").then_some("windows")
}

/// Starts a new cluster on a server over SSH, and makes a context for it the current one.
async fn add_new(at: &At, target: Target, out: &Output) -> Result<()> {
    let c = install::provision(&target, Mode::NewCluster).await?;
    let i: Inspect = c.call(None, "machine.inspect", ()).await?;
    let v: Value = c.call(None, "cluster.init", json!({ "name": target.name })).await?;
    c.close().await;
    let name = target.name.clone().unwrap_or(i.info.name.clone());
    let ctx = Config::edit(|cfg| {
        Ok(cfg
            .add_context(at.context.as_deref().unwrap_or("default"), vec![connection(&target, i.id, platform_of(&i))]))
    })?;
    let (ssh_alias, ssh_alias_error) = save_local(&target, i.id, &name, platform_of(&i));
    let r = Installed {
        id: i.id,
        name,
        target: target.dest.clone(),
        cluster: v["cluster"].as_str().map(String::from),
        members: Some(1),
        context: Some(ctx),
        ssh_alias,
        ssh_alias_error,
        version: i.info.version.clone(),
    };
    out.print(&r, |r| {
        println!("cluster {} started on {} ({})", r.cluster.as_deref().unwrap_or_default(), r.target, r.id);
        println!("current context is now {:?}", r.context.as_deref().unwrap_or_default());
        print_alias(r);
    })
}

fn print_alias(r: &Installed) {
    if let Some(a) = &r.ssh_alias {
        println!("ssh alias: ssh {a}");
    }
}

/// Adds a connection for a new machine to the context, unless it has it.
fn remember(at: &At, conn: Connection) -> Result<()> {
    at.edit_context(|ctx| {
        if !ctx.connections.iter().any(|c| c.machine == conn.machine && c.ssh == conn.ssh) {
            ctx.connections.push(conn);
        }
    })
}

/// `imesh add user@host`: installs imesh on a server over SSH. The server joins the cluster that
/// this computer's daemon or the current context reaches. With none, or with `new`, it starts a
/// new cluster.
pub(crate) async fn add(at: &At, mut target: Target, new: bool, out: &Output) -> Result<()> {
    target.resolve().await?;
    if new {
        return add_new(at, target, out).await;
    }
    let Some(entry) = at.entry().await? else {
        return add_new(at, target, out).await;
    };
    let r = add_to(at, &entry, target, out).await;
    entry.close().await;
    r
}

async fn add_to(at: &At, entry: &Client, mut target: Target, out: &Output) -> Result<()> {
    // A new machine uses the entry machine's relays unless told otherwise.
    let e: Inspect = entry.call(None, "machine.inspect", ()).await?;
    let r = &mut target.settings.net;
    if r.relays.is_empty() && r.devtunnel.is_none() {
        use crate::args::Relay;
        r.relays = e.relays.iter().cloned().map(Relay::Url).collect();
        if e.n0 {
            r.relays.push(Relay::N0);
        }
        if e.devtunnel_mesh {
            r.relays.push(Relay::DevTunnel);
        }
    }
    let mut c = install::provision(&target, Mode::New).await?;
    let mut i: Inspect = c.call(None, "machine.inspect", ()).await?;
    match &i.cluster {
        // A machine already admitted to this cluster may finish a join that failed before.
        Some(cluster) if Some(cluster) == e.cluster.as_ref() && i.member => {
            c.close().await;
            eprintln!("the machine is in this cluster already; finishing the local setup");
            return finish_local(at, &target, cluster, out).await;
        }
        // A server that ran `imesh up` alone holds its own cluster. With nothing in it, it starts
        // again as a new machine, out of any cluster, and joins.
        Some(cluster) if i.alone_and_empty => {
            eprintln!("{} is alone in its own empty cluster {cluster}; it leaves it to join", target.display());
            c.call::<Value>(None, "machine.reset", ()).await?;
            c.close().await;
            (c, i) = reconnect_after_reset(&target, i.id, platform_of(&i)).await?;
        }
        Some(cluster) => bail!(
            "{} is in cluster {cluster} with other members or data; run `imesh logout` there first",
            target.display()
        ),
        None => {}
    }
    let name = target.name.clone().unwrap_or(i.info.name.clone());
    // The new machine's mesh tunnel, so the entry machine reaches it before its row arrives.
    let tunnel = if target.settings.net.has(&crate::args::Relay::DevTunnel) { wait_tunnel(&c).await? } else { None };
    let ticket: JoinTicket =
        entry.call(None, "cluster.add", json!({ "id": i.id, "name": name, "tunnel": tunnel })).await?;
    let joined: Joined = c.call(None, "cluster.join", ticket).await.with_context(|| {
        format!("{name} ({}) is admitted, but did not join; run the same add again to finish", i.id)
    })?;
    c.close().await;
    remember(at, connection(&target, i.id, platform_of(&i)))?;
    let (ssh_alias, ssh_alias_error) = save_local(&target, i.id, &name, platform_of(&i));
    let r = Installed {
        id: i.id,
        name,
        target: target.dest.clone(),
        cluster: Some(joined.cluster.clone()),
        members: Some(joined.members),
        context: None,
        ssh_alias,
        ssh_alias_error,
        version: i.info.version.clone(),
    };
    out.print(&r, |r| {
        println!(
            "machine {} ({}) joined cluster {} with {} members",
            r.name,
            r.id,
            r.cluster.as_deref().unwrap_or_default(),
            r.members.unwrap_or(0)
        );
        print_alias(r);
    })
}

/// Waits until a machine that reset answers again as a new machine, out of any cluster.
async fn reconnect_after_reset(target: &Target, old: EndpointId, platform: Option<&str>) -> Result<(Client, Inspect)> {
    let conn = connection(target, old, platform);
    for _ in 0..60 {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let Ok(c) = crate::connect::open(&conn).await else { continue };
        match c.call::<Inspect>(None, "machine.inspect", ()).await {
            Ok(i) if i.id != old && i.cluster.is_none() => return Ok((c, i)),
            _ => c.close().await,
        }
    }
    bail!("{} did not come back as a new machine within 60 seconds", target.display())
}

/// Saves the context and SSH alias for a machine that joined this cluster before.
/// Waits for the new machine's devtunnel relay to be hosted, and returns where members reach it.
async fn wait_tunnel(c: &Client) -> Result<Option<iroh_mesh::state::TunnelEndpoint>> {
    for _ in 0..90 {
        let i: Inspect = c.call(None, "machine.inspect", ()).await?;
        if i.info.tunnel.is_some() {
            return Ok(i.info.tunnel);
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    bail!("the new machine did not host its devtunnel relay within 90s; check `devtunnel user show` there")
}

async fn finish_local(at: &At, target: &Target, cluster: &str, out: &Output) -> Result<()> {
    let mut conn = target.ssh();
    let c = match crate::connect::open(&conn).await {
        Ok(c) => c,
        Err(_) => {
            conn.platform = Some("windows".into());
            crate::connect::open(&conn).await?
        }
    };
    let i: Inspect = c.call(None, "machine.inspect", ()).await?;
    c.close().await;
    ensure!(i.cluster.as_deref() == Some(cluster) && i.member, "the target is not a member of the intended cluster");
    let name = target.name.clone().unwrap_or(i.info.name.clone());
    remember(at, connection(target, i.id, platform_of(&i)))?;
    let (ssh_alias, ssh_alias_error) = save_local(target, i.id, &name, platform_of(&i));
    let r = Installed {
        id: i.id,
        name,
        target: target.dest.clone(),
        cluster: i.cluster.clone(),
        members: None,
        context: None,
        ssh_alias,
        ssh_alias_error,
        version: i.info.version.clone(),
    };
    out.print(&r, |r| {
        println!("machine {} ({}) is in cluster {}", r.name, r.id, r.cluster.as_deref().unwrap_or_default());
        print_alias(r);
    })
}

pub(crate) async fn update(mut target: Target, out: &Output) -> Result<()> {
    target.resolve().await?;
    let c = install::provision(&target, Mode::Update).await?;
    let i: Inspect = c.call(None, "machine.inspect", ()).await?;
    c.close().await;
    let name = target.name.clone().unwrap_or(i.info.name.clone());
    let (ssh_alias, ssh_alias_error) = save_local(&target, i.id, &name, platform_of(&i));
    let r = Installed {
        id: i.id,
        name,
        target: target.dest.clone(),
        cluster: i.cluster.clone(),
        members: None,
        context: None,
        ssh_alias,
        ssh_alias_error,
        version: i.info.version.clone(),
    };
    out.print(&r, |r| println!("updated {} ({}), version {}", r.target, r.id, r.version))
}

pub(crate) async fn resolve(c: &Client, m: &str) -> Result<EndpointId> {
    if let Ok(id) = m.parse() {
        return Ok(id);
    }
    let all: Vec<MachineRow> = c.call(None, "cluster.machines", ()).await?;
    pick(&all, m).map(|r| r.id)
}

/// The member that `m` names: its exact name, else its full ID, else the one ID that starts with
/// `m` (4 characters or more), as `status` shows IDs short.
pub(crate) fn pick<'a>(rows: &'a [MachineRow], m: &str) -> Result<&'a MachineRow> {
    let by = |f: &dyn Fn(&MachineRow) -> bool| rows.iter().filter(|r| f(r)).collect::<Vec<_>>();
    let named = by(&|r| r.name == m);
    if let [one] = named.as_slice() {
        return Ok(one);
    }
    ensure!(named.is_empty(), "{} machines are named {m:?}; use the ID from `imesh status`", named.len());
    let m = m.to_ascii_lowercase();
    let ids = if m.len() >= 4 { by(&|r| r.id.to_string().starts_with(&m)) } else { Vec::new() };
    match ids.as_slice() {
        [one] => Ok(one),
        [] => bail!("no machine named {m:?} or with an ID that starts with it; see `imesh status`"),
        _ => bail!("{} machine IDs start with {m:?}; give more of the ID", ids.len()),
    }
}

/// One reply of `ping`.
#[derive(Debug, Serialize)]
pub struct Pong {
    pub machine: String,
    pub id: EndpointId,
    /// `direct` or `relay`, as the machine the CLI talks to sees its link.
    pub path: Option<String>,
    pub rtt_ms: u64,
}

/// `imesh ping <machine>`: calls `ping` on the machine through the entry machine, and reports
/// the path of the entry machine's link to it.
pub(crate) async fn ping(c: &Client, m: &str, count: u32, until_direct: bool, out: &Output) -> Result<()> {
    let id = resolve(c, m).await?;
    let me: Inspect = c.call(None, "machine.inspect", ()).await?;
    let mut ok = 0;
    for n in 0..count.max(1) {
        if n > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        let start = std::time::Instant::now();
        let target = if id == me.id { None } else { Some(id) };
        if let Err(e) = c.call::<Value>(target, "ping", ()).await {
            eprintln!("no answer from {m}: {e:#}");
            continue;
        }
        let rtt_ms = start.elapsed().as_millis() as u64;
        let rows: Vec<MachineRow> = c.call(None, "cluster.machines", ()).await?;
        let row = rows.iter().find(|r| r.id == id);
        let pong = Pong {
            machine: row.map_or(m.to_string(), |r| r.name.clone()),
            id,
            path: row.and_then(|r| r.status.path.clone()),
            rtt_ms,
        };
        out.print(&pong, |p| {
            println!(
                "pong from {} ({}) via {} in {}ms",
                p.machine,
                id.fmt_short(),
                p.path.as_deref().unwrap_or("-"),
                p.rtt_ms
            )
        })?;
        ok += 1;
        if until_direct && pong.path.as_deref() == Some("direct") {
            break;
        }
    }
    ensure!(ok > 0, "{m} did not answer");
    Ok(())
}

pub(crate) async fn ls(c: &Client, wide: bool, out: &Output) -> Result<()> {
    let rows: Vec<MachineRow> = c.call(None, "cluster.machines", ()).await?;
    out.print(&rows, |rows| print_machines(rows, wide))
}

/// Prints rows as columns as wide as their longest cell. On a terminal the first column shrinks
/// to fit, then the last columns are dropped. Piped output keeps every column.
fn table<const N: usize>(head: [&str; N], rows: &[[String; N]]) {
    let mut w = head.map(str::len);
    for r in rows {
        for (i, c) in r.iter().enumerate() {
            w[i] = w[i].max(c.chars().count());
        }
    }
    let mut n = N;
    if let Some((terminal_size::Width(cols), _)) = terminal_size::terminal_size() {
        let cols = cols as usize;
        let total = |w: &[usize], n: usize| w[..n].iter().sum::<usize>() + 2 * (n - 1);
        let over = total(&w, n).saturating_sub(cols);
        w[0] = w[0].saturating_sub(over).max(head[0].len().max(8));
        while n > 1 && total(&w, n) > cols {
            n -= 1;
        }
    }
    let line = |cells: Vec<String>| {
        let s: Vec<String> =
            cells.iter().take(n).enumerate().map(|(i, c)| format!("{:<1$}", fit(c, w[i]), w[i])).collect();
        println!("{}", s.join("  ").trim_end());
    };
    line(head.iter().map(|h| h.to_string()).collect());
    for r in rows {
        line(r.to_vec());
    }
}

/// Shortens `s` to `max` characters, ending in `...`.
fn fit(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep: String = s.chars().take(max.saturating_sub(3)).collect();
    format!("{keep}...")
}

/// Names longer than this are cut in the table. `--json` and `-w` show them in full.
const NAME_MAX: usize = 24;

fn print_machines(rows: &[MachineRow], wide: bool) {
    let w = if wide { 64 } else { 10 };
    let cells: Vec<[String; 7]> = rows
        .iter()
        .map(|r| {
            let s = &r.status;
            let (os, version) = match &r.info {
                Some(i) => (format!("{}/{}", i.os, i.arch), i.version.clone()),
                None => ("?/?".into(), String::new()),
            };
            [
                if wide { r.name.clone() } else { fit(&r.name, NAME_MAX) },
                r.id.to_string()[..w].to_string(),
                s.state.to_string(),
                s.path.clone().unwrap_or_else(|| "-".into()),
                s.rtt_ms.map_or("-".into(), |v| format!("{v}ms")),
                os,
                version,
            ]
        })
        .collect();
    table(["NAME", "ID", "STATE", "PATH", "RTT", "OS/ARCH", "VERSION"], &cells);
    if !wide {
        return;
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    for r in rows {
        let s = &r.status;
        if let Some(t) = s.last_seen {
            println!("{}: last contact {}s ago", r.name, now.saturating_sub(t));
        }
        if let Some(e) = &s.error {
            println!("{}: error: {e}", r.name);
        }
    }
}

/// The result of `rm`: each step separately.
#[derive(Debug, Serialize)]
struct Removed {
    id: EndpointId,
    name: String,
    /// The machine left the cluster.
    removed: bool,
    /// The machine reset itself. False when it could not be reached.
    reset: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reset_error: Option<String>,
    /// Connections removed from the context.
    context_connections: usize,
    /// The SSH alias removed from ~/.ssh/imesh/config.
    ssh_alias: Option<String>,
}

pub(crate) async fn rm(at: &At, c: &Client, m: &str, force: bool, out: &Output) -> Result<()> {
    let id = resolve(c, m).await?;
    let entry: Inspect = c.call(None, "machine.inspect", ()).await?;
    ensure!(entry.id != id, "you are connected through {m}; connect through another machine to remove it");
    // From the replicated state, so the guard holds also when the machine is down.
    let relays: RelaysInfo = c.call(None, "relays.get", ()).await?;
    let rows: Vec<MachineRow> = c.call(None, "cluster.machines", ()).await?;
    let row = rows.into_iter().find(|r| r.id == id);
    let name = row.as_ref().map_or(m.to_string(), |r| r.name.clone());
    let served = row.and_then(|r| r.info).and_then(|i| i.relay);
    if let Some(url) = served
        && relays.cluster.contains(&url)
    {
        ensure!(
            force,
            "{m} serves {url}, which the cluster relay setting uses. Change it with `imesh relay set` first, or pass --force"
        );
    }
    let reset_error = match c.call::<Value>(Some(id), "machine.reset", ()).await {
        Ok(_) => None,
        Err(e) => {
            eprintln!("could not reset {m} ({e:#}). It is removed anyway; on that machine, run `imesh logout`.");
            Some(format!("{e:#}"))
        }
    };
    c.call::<Value>(None, "cluster.remove", json!({ "id": id })).await?;
    let mut removed_conns = 0;
    at.edit_context(|ctx| {
        let before = ctx.connections.len();
        ctx.connections.retain(|c| c.machine != Some(id));
        removed_conns = before - ctx.connections.len();
    })?;
    let ssh_alias = crate::ssh_config::remove(id).unwrap_or_else(|e| {
        eprintln!("warning: could not remove the SSH alias: {e:#}");
        None
    });
    let r = Removed {
        id,
        name,
        removed: true,
        reset: reset_error.is_none(),
        reset_error,
        context_connections: removed_conns,
        ssh_alias,
    };
    out.print(&r, |r| {
        println!("removed {} ({})", r.name, r.id);
        if let Some(a) = &r.ssh_alias {
            println!("removed SSH alias {a}");
        }
    })
}

#[cfg(test)]
mod pick_tests {
    use super::*;

    fn row(name: &str) -> MachineRow {
        MachineRow {
            id: iroh::SecretKey::generate().public(),
            name: name.into(),
            info: None,
            status: Default::default(),
        }
    }

    #[test]
    fn picks_by_name_full_id_or_unique_prefix() {
        let rows = vec![row("mac"), row("win")];
        let win = rows[1].id.to_string();
        assert_eq!(pick(&rows, "mac").unwrap().name, "mac");
        assert_eq!(pick(&rows, &win).unwrap().name, "win");
        assert_eq!(pick(&rows, &win[..10]).unwrap().name, "win");
        assert_eq!(pick(&rows, &win[..10].to_uppercase()).unwrap().name, "win");
        assert!(pick(&rows, &win[..3]).is_err());
        assert!(pick(&rows, "nope").unwrap_err().to_string().contains("no machine named"));
    }
}
