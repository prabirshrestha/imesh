mod example;

use std::path::PathBuf;

use anyhow::{Context as _, Result, bail, ensure};
use clap::{Parser, Subcommand};
use imesh::{
    api::{Admit, ClientRow, Inspect, Joined, MachineRow, RelaysInfo},
    client::{
        Client,
        config::{self, Config, Connection, Context},
    },
    daemon,
    install::{self, Mode, Target},
    net::local,
};
use iroh::{EndpointAddr, EndpointId};
use serde_json::{Value, json};

#[derive(Parser)]
#[command(name = "imesh", version, about = "Machines that talk over iroh. No control plane.")]
struct Cli {
    #[command(flatten)]
    at: At,
    /// Append logs to this file instead of stderr.
    #[arg(long, global = true, env = "IMESH_LOG_FILE")]
    log_file: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

/// Which machine the CLI talks to.
#[derive(clap::Args)]
struct At {
    /// Connect to this machine and ignore the config: user@host, ssh://user@host, iroh://<id>, local://<path>.
    #[arg(long, global = true, env = "IMESH_CONNECT")]
    connect: Option<String>,
    /// Context to use instead of current_context.
    #[arg(long, short = 'c', global = true, env = "IMESH_CONTEXT")]
    context: Option<String>,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // parsed once
enum Cmd {
    /// Run the machine daemon.
    #[command(args_conflicts_with_subcommands = true)]
    Daemon {
        #[command(subcommand)]
        cmd: Option<DaemonCmd>,
        #[command(flatten)]
        opts: daemon::Opts,
    },
    /// Bridge stdin and stdout to the local API.
    #[command(hide = true)]
    DialStdio {
        #[arg(long, env = "IMESH_SOCKET", default_value_os_t = local::default_path())]
        socket: PathBuf,
    },
    /// Manage machines.
    #[command(subcommand)]
    Machine(MachineCmd),
    /// Manage contexts.
    #[command(subcommand, name = "ctx")]
    Ctx(CtxCmd),
    /// Show or set the relays that every machine in the cluster uses.
    #[command(subcommand)]
    Relay(RelayCmd),
    /// Manage the CLI keys that may use the API over iroh.
    #[command(subcommand)]
    Client(ClientCmd),
    /// Print this CLI's public key, for `imesh client add` on a cluster you want to reach.
    Key,
    /// Call an API method.
    Call {
        method: String,
        /// JSON params.
        params: Option<String>,
        /// Machine (name or id) that serves the call. Defaults to the machine you connect to.
        #[arg(long, short)]
        machine: Option<String>,
    },
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // parsed once
enum DaemonCmd {
    /// Install this binary as the machine's daemon and start it. No SSH needed.
    /// Linux: needs sudo, uses systemd. Windows: a scheduled task for the current user.
    Install(install::Opts),
    /// Stop the daemon and remove the service and the binary. Keeps the data unless --purge.
    Uninstall {
        #[arg(long)]
        purge: bool,
    },
}

#[derive(Subcommand)]
enum MachineCmd {
    /// Install imesh on a server and start a new cluster on it.
    Init {
        #[command(flatten)]
        target: Target,
    },
    /// Add a machine to the current cluster: over SSH, or by the ticket from `imesh machine id`.
    Add {
        #[command(flatten)]
        target: Target,
    },
    /// Upgrade the binary on an installed machine or change its settings. Settings you do not
    /// pass stay as they are.
    Update {
        #[command(flatten)]
        target: Target,
    },
    /// List machines in the cluster.
    Ls {
        /// Show full IDs, last contact, and link errors.
        #[arg(long, short)]
        wide: bool,
    },
    /// Print this machine's ticket, for `imesh machine add <ticket>` on another computer.
    Id,
    /// Join a cluster with the ticket that `imesh machine add <ticket>` printed.
    Join { ticket: String },
    /// Remove a machine from the cluster and reset it.
    Rm {
        machine: String,
        /// Remove it even though it serves a relay that the cluster uses.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum RelayCmd {
    /// Show the cluster relays, relays that machines serve, and the relays the entry machine uses.
    Ls,
    /// Make every machine use exactly these relays, for example a relay that a member exposes.
    /// No URLs clears the setting, and machines go back to the relays they started with.
    Set {
        urls: Vec<iroh::RelayUrl>,
        /// Bearer token for these relays.
        #[arg(long)]
        token: Option<String>,
    },
}

#[derive(Subcommand)]
enum ClientCmd {
    /// List authorized CLI keys.
    Ls,
    /// Authorize a CLI key. Get it with `imesh key` on that computer.
    Add {
        key: EndpointId,
        #[arg(long, default_value = "")]
        name: String,
    },
    /// Revoke a CLI key. Its open connections close.
    Rm { key: EndpointId },
}

#[derive(Subcommand)]
enum CtxCmd {
    /// List contexts.
    Ls,
    /// Make a context current.
    Use { name: String },
    /// List the connections of the current context, or move one to the top.
    Conn { index: Option<usize> },
    /// Add connections to a context, creating it if needed: user@host, iroh://<id>, local://<path>.
    Add {
        name: String,
        #[arg(required = true)]
        connections: Vec<String>,
    },
}

fn main() -> Result<()> {
    // systemd reads /etc/imesh/env on Linux. On Windows the daemon reads its own file, before
    // clap reads the environment.
    #[cfg(windows)]
    if std::env::args().nth(1).as_deref() == Some("daemon") {
        install::load_env_file(&install::env_file());
    }
    let cli = Cli::parse();
    let level = if matches!(cli.cmd, Cmd::Daemon { .. }) { "info" } else { "warn" };
    let filter = tracing_subscriber::EnvFilter::try_from_env("IMESH_LOG").unwrap_or_else(|_| level.into());
    match &cli.log_file {
        Some(path) => {
            let file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file))
                .init();
        }
        None => tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init(),
    }
    tokio::runtime::Runtime::new()?.block_on(run(cli))
}

async fn run(cli: Cli) -> Result<()> {
    let at = &cli.at;
    match cli.cmd {
        Cmd::Daemon { cmd: Some(DaemonCmd::Install(opts)), .. } => install::run(opts),
        Cmd::Daemon { cmd: Some(DaemonCmd::Uninstall { purge }), .. } => install::uninstall(purge),
        Cmd::Daemon { cmd: None, opts } => {
            daemon::serve(|| example::register(daemon::Builder::new(opts.clone()))).await
        }
        Cmd::DialStdio { socket } => imesh::os::dial_stdio(socket).await,
        Cmd::Key => {
            println!("{}", config::client_key()?.public());
            Ok(())
        }
        Cmd::Ctx(cmd) => ctx(at, cmd),
        Cmd::Machine(MachineCmd::Init { target }) => init(at, target).await,
        Cmd::Machine(MachineCmd::Update { target }) => {
            install::provision(&target, Mode::Update).await?.close().await;
            println!("updated {}", target.dest);
            Ok(())
        }
        cmd => {
            // `id` and `join` act on the daemon of this computer, not on the context.
            let c = match cmd {
                Cmd::Machine(MachineCmd::Id | MachineCmd::Join { .. }) => {
                    Client::open(&at.connect.as_deref().map_or_else(|| Ok(Connection::local()), Connection::parse)?)
                        .await?
                }
                _ => at.client().await?,
            };
            let r = api(at, &c, cmd).await;
            c.close().await;
            r
        }
    }
}

/// The commands that call the API of one machine.
async fn api(at: &At, c: &Client, cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Machine(MachineCmd::Id) => {
            let i: Inspect = c.call(None, "machine.inspect", ()).await?;
            println!("{}", iroh_tickets::endpoint::EndpointTicket::new(i.info.addr));
        }
        Cmd::Machine(MachineCmd::Join { ticket }) => {
            let j: Joined = c.call(None, "cluster.join", decode_join_ticket(&ticket)?).await?;
            println!("joined cluster {} with {} members", j.cluster, j.members);
        }
        Cmd::Machine(MachineCmd::Add { target }) => add(at, c, target).await?,
        Cmd::Machine(MachineCmd::Ls { wide }) => ls(c, wide).await?,
        Cmd::Machine(MachineCmd::Rm { machine, force }) => rm(at, c, &machine, force).await?,
        Cmd::Relay(RelayCmd::Ls) => {
            let r: RelaysInfo = c.call(None, "relays.get", ()).await?;
            println!("{}", serde_json::to_string_pretty(&r)?);
        }
        Cmd::Relay(RelayCmd::Set { urls, token }) => {
            c.call::<Value>(None, "relays.set", json!({ "relays": urls, "token": token })).await?;
            println!("cluster relays set; every machine switches within about 10 s");
        }
        Cmd::Client(ClientCmd::Ls) => {
            for r in c.call::<Vec<ClientRow>>(None, "client.list", ()).await? {
                println!("{}  {}", r.key, r.name);
            }
        }
        Cmd::Client(ClientCmd::Add { key, name }) => {
            c.call::<Value>(None, "client.add", ClientRow { key, name }).await?;
            println!("authorized {key}");
        }
        Cmd::Client(ClientCmd::Rm { key }) => {
            c.call::<Value>(None, "client.remove", json!({ "key": key })).await?;
            println!("revoked {key}");
        }
        Cmd::Call { method, params, machine } => {
            let to = match machine {
                Some(m) => Some(resolve(c, &m).await?),
                None => None,
            };
            let params: Value = match params {
                Some(p) => serde_json::from_str(&p).context("params must be JSON")?,
                None => Value::Null,
            };
            let v: Value = c.call(to, &method, params).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        _ => unreachable!("not an API command"),
    }
    Ok(())
}

impl At {
    /// The first machine that answers: the `--connect` target, or a connection of the context.
    /// With no config at all, the daemon on this computer.
    async fn client(&self) -> Result<Client> {
        let conns = match &self.connect {
            Some(c) => vec![Connection::parse(c)?],
            None => {
                let cfg = Config::load()?;
                if cfg.contexts.is_empty() && self.context.is_none() {
                    vec![Connection::local()]
                } else {
                    cfg.context(self.context.as_deref())?.1.connections.clone()
                }
            }
        };
        Ok(Client::open_any(&conns).await?.0)
    }

    /// Changes the context that selected the entry machine. With `--connect` no context did, so
    /// nothing changes.
    fn edit_context(&self, f: impl FnOnce(&mut Context)) -> Result<()> {
        if self.connect.is_some() {
            return Ok(());
        }
        let mut cfg = Config::load()?;
        let Ok(ctx) = cfg.context_mut(self.context.as_deref()) else { return Ok(()) };
        f(ctx);
        cfg.save()
    }
}

const JOIN_PREFIX: &str = "imeshjoin";

/// A join ticket carries the `cluster.add` result. It holds no secret unless the cluster relay
/// setting has a token.
fn encode_join_ticket(admit: &Admit) -> Result<String> {
    Ok(format!("{JOIN_PREFIX}{}", data_encoding::BASE64URL_NOPAD.encode(&serde_json::to_vec(admit)?)))
}

fn decode_join_ticket(t: &str) -> Result<Admit> {
    let body = t.trim().strip_prefix(JOIN_PREFIX).context("not an imesh join ticket")?;
    let bytes = data_encoding::BASE64URL_NOPAD.decode(body.as_bytes()).context("bad join ticket")?;
    serde_json::from_slice(&bytes).context("bad join ticket")
}

/// The connections the CLI keeps for a machine installed over SSH: SSH first, then iroh.
fn connections(target: &Target, id: EndpointId) -> Vec<Connection> {
    let r = &target.opts.relay;
    vec![
        Connection { machine: Some(id), ..target.ssh() },
        Connection {
            iroh: Some(id),
            relay: r.relays.first().cloned(),
            relay_token: r.relay_token.clone(),
            no_n0: r.no_n0,
            machine: Some(id),
            ..Default::default()
        },
    ]
}

async fn init(at: &At, target: Target) -> Result<()> {
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

async fn add(at: &At, entry: &Client, mut target: Target) -> Result<()> {
    if let Ok(ticket) = target.dest.parse::<iroh_tickets::endpoint::EndpointTicket>() {
        return add_ticket(at, entry, &target, ticket.endpoint_addr().clone()).await;
    }
    // A new machine uses the entry machine's relays unless told otherwise.
    let r = &mut target.opts.relay;
    if r.relays.is_empty() && !r.no_n0 {
        let e: Inspect = entry.call(None, "machine.inspect", ()).await?;
        (r.relays, r.no_n0) = (e.relays, e.no_n0);
    }
    let c = install::provision(&target, Mode::New).await?;
    let i: Inspect = c.call(None, "machine.inspect", ()).await?;
    let name = target.name.clone().unwrap_or(i.info.name);
    let admit: Admit = entry.call(None, "cluster.add", json!({ "id": i.id, "name": name })).await?;
    let joined: Joined = c.call(None, "cluster.join", admit).await?;
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
    let admit: Admit = entry.call(None, "cluster.add", json!({ "id": id, "name": name })).await?;
    println!("admitted {name} ({id}). On that machine, run:\n\n  imesh machine join {}\n", encode_join_ticket(&admit)?);
    let conn = Connection {
        iroh: Some(id),
        relay: addr.relay_urls().next().cloned(),
        addrs: addr.ip_addrs().copied().collect(),
        machine: Some(id),
        ..Default::default()
    };
    remember(at, vec![conn])
}

async fn resolve(c: &Client, m: &str) -> Result<EndpointId> {
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

async fn ls(c: &Client, wide: bool) -> Result<()> {
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
        let state = serde_json::to_value(s.state)?;
        println!(
            "{:<12} {:<w$} {:<8} {:<7} {:<8} {:<14} {version}",
            r.name,
            &id[..w],
            state.as_str().unwrap_or_default(),
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

async fn rm(at: &At, c: &Client, m: &str, force: bool) -> Result<()> {
    let id = resolve(c, m).await?;
    let entry: Inspect = c.call(None, "machine.inspect", ()).await?;
    ensure!(entry.id != id, "you are connected through {m}; connect through another machine to remove it");
    let relays: RelaysInfo = c.call(None, "relays.get", ()).await?;
    let target = c.call::<Inspect>(Some(id), "machine.inspect", ()).await.ok();
    if let Some(url) = target.and_then(|i| i.info.relay)
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

fn ctx(at: &At, cmd: CtxCmd) -> Result<()> {
    ensure!(at.connect.is_none(), "--connect ignores the config; contexts are not used");
    let mut cfg = Config::load()?;
    match cmd {
        CtxCmd::Ls => {
            println!("{:<16} {:<8} CONNECTIONS", "NAME", "CURRENT");
            for (name, c) in &cfg.contexts {
                let cur = if cfg.current_context.as_deref() == Some(name) { "*" } else { "" };
                println!("{name:<16} {cur:<8} {}", c.connections.len());
            }
        }
        CtxCmd::Use { name } => {
            ensure!(cfg.contexts.contains_key(&name), "context {name:?} not found");
            cfg.current_context = Some(name.clone());
            cfg.save()?;
            println!("current context is now {name:?}");
        }
        CtxCmd::Conn { index } => {
            let ctx = cfg.context_mut(at.context.as_deref())?;
            if let Some(i) = index {
                ensure!(i < ctx.connections.len(), "no connection {i}");
                let c = ctx.connections.remove(i);
                ctx.connections.insert(0, c);
            }
            for (i, c) in ctx.connections.iter().enumerate() {
                println!("{i}  {c}");
            }
            if index.is_some() {
                cfg.save()?;
            }
        }
        CtxCmd::Add { name, connections } => {
            let parsed = connections.iter().map(|c| Connection::parse(c)).collect::<Result<Vec<_>>>()?;
            let ctx = cfg.contexts.entry(name.clone()).or_default();
            ctx.connections.extend(parsed);
            let n = ctx.connections.len();
            cfg.current_context.get_or_insert_with(|| name.clone());
            cfg.save()?;
            println!("context {name:?} has {n} connections");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn cli_is_valid() {
        <super::Cli as clap::CommandFactory>::command().debug_assert();
    }
}
