mod api;
mod client;
mod config;
mod daemon;
mod dns;
mod example;
mod install;
mod local;
mod relay;
mod ssh;
mod state;

use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};
use iroh::EndpointId;
use serde_json::{Value, json};

use crate::{
    client::Client,
    config::{Config, Connection},
    ssh::Ssh,
};

const INSTALL_SH: &str = include_str!("install.sh");

#[derive(Parser)]
#[command(name = "imesh", version, about = "Machines that talk over iroh. No control plane.")]
struct Cli {
    /// Connect to this machine and ignore the config: user@host, ssh://user@host, iroh://<id>, local://<path>.
    #[arg(long, global = true, env = "IMESH_CONNECT")]
    connect: Option<String>,
    /// Context to use instead of current_context.
    #[arg(long, short = 'c', global = true, env = "IMESH_CONTEXT")]
    context: Option<String>,
    /// Append logs to this file instead of stderr.
    #[arg(long, global = true, env = "IMESH_LOG_FILE")]
    log_file: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the machine daemon.
    #[command(args_conflicts_with_subcommands = true)]
    Daemon {
        #[command(subcommand)]
        cmd: Option<DaemonCmd>,
        #[command(flatten)]
        opts: daemon::Opts,
    },
    /// Bridge stdin and stdout to the local API socket.
    #[command(hide = true)]
    DialStdio {
        #[arg(long, env = "IMESH_SOCKET", default_value = local::DEFAULT_PATH)]
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
enum DaemonCmd {
    /// Install this binary as the machine's daemon and start it. No SSH needed.
    /// Linux: needs sudo, uses systemd. Windows: a scheduled task for the current user.
    Install(install::Opts),
}

#[derive(clap::Args)]
struct Target {
    /// user@host[:port] to install over SSH, or a machine ticket from `imesh machine id`
    /// (the machine installed itself with `imesh daemon install`).
    dest: String,
    /// SSH private key. Uses the SSH agent and ~/.ssh/config when omitted.
    #[arg(short = 'i', long = "ssh-key")]
    ssh_key: Option<PathBuf>,
    /// Machine name. Defaults to the remote hostname.
    #[arg(long)]
    name: Option<String>,
    /// Run the daemon as this user instead of the SSH user. Created as a system user if missing.
    #[arg(long)]
    run_as: Option<String>,
    /// Run the daemon with this group instead of the user's primary group.
    #[arg(long)]
    run_as_group: Option<String>,
    /// Labels for the machine, as key=value. Repeat for more.
    #[arg(long = "label")]
    labels: Vec<String>,
    /// Relay settings for the machine. `add` copies --relay, --relay-token, and --no-n0 from
    /// the entry machine when none of them is given.
    #[command(flatten)]
    relay: relay::Opts,
    /// Linux imesh binary to install. Defaults to $IMESH_BINARY_<ARCH>, for example IMESH_BINARY_AARCH64.
    #[arg(long)]
    binary: Option<PathBuf>,
}

#[derive(Subcommand)]
enum MachineCmd {
    /// Install imesh on a server and start a new cluster on it.
    Init {
        #[command(flatten)]
        target: Target,
    },
    /// Install imesh on a server and add it to the current cluster.
    Add {
        #[command(flatten)]
        target: Target,
    },
    /// List machines in the cluster.
    Ls,
    /// Print this machine's ticket, for `imesh machine add <ticket>` on another computer.
    Id,
    /// Join a cluster with the ticket that `imesh machine add <ticket>` printed.
    Join { ticket: String },
    /// Remove a machine from the cluster and reset it.
    Rm { machine: String },
}

#[derive(Subcommand)]
enum RelayCmd {
    /// Show the cluster relays, relays that machines serve, and the relays the entry machine uses.
    Ls,
    /// Make every machine use these relays, for example a relay that a member exposes. No URLs
    /// clears the setting; machines keep their current relays until they restart.
    Set { urls: Vec<iroh::RelayUrl> },
}

#[derive(Subcommand)]
enum CtxCmd {
    /// List contexts.
    Ls,
    /// Make a context current.
    Use { name: String },
    /// List the connections of the current context, or move one to the top.
    Conn { index: Option<usize> },
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
            tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false).with_writer(std::sync::Mutex::new(file)).init();
        }
        None => tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init(),
    }
    tokio::runtime::Runtime::new()?.block_on(run(cli))
}

async fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Daemon { cmd: Some(DaemonCmd::Install(opts)), .. } => install::run(opts),
        Cmd::Daemon { cmd: None, opts } => daemon::run(example::register(daemon::Builder::new(opts))).await,
        Cmd::Machine(MachineCmd::Id) => {
            let c = local_client(&cli.connect).await?;
            let v = c.call(None, "machine.inspect", Value::Null).await?;
            let addr: iroh::EndpointAddr = serde_json::from_value(v["info"]["addr"].clone())?;
            println!("{}", iroh_tickets::endpoint::EndpointTicket::new(addr));
            Ok(())
        }
        Cmd::Machine(MachineCmd::Join { ticket }) => {
            let c = local_client(&cli.connect).await?;
            let params = decode_join_ticket(&ticket)?;
            let v = c.call(None, "cluster.join", params).await?;
            println!("joined cluster {} with {} members", v["cluster"].as_str().unwrap_or_default(), v["members"]);
            Ok(())
        }
        Cmd::DialStdio { socket } => daemon::dial_stdio(socket).await,
        Cmd::Machine(MachineCmd::Init { target }) => init(&cli.context, target).await,
        Cmd::Machine(MachineCmd::Add { target }) => {
            let (entry, _) = connect(&cli.connect, &cli.context).await?;
            add(&cli.context, &entry, target).await
        }
        Cmd::Machine(MachineCmd::Ls) => {
            let (c, _) = connect(&cli.connect, &cli.context).await?;
            ls(&c).await
        }
        Cmd::Machine(MachineCmd::Rm { machine }) => {
            let (c, _) = connect(&cli.connect, &cli.context).await?;
            rm(&cli.connect, &cli.context, &c, &machine).await
        }
        Cmd::Ctx(cmd) => ctx(&cli.connect, cmd),
        Cmd::Relay(cmd) => {
            let (c, _) = connect(&cli.connect, &cli.context).await?;
            match cmd {
                RelayCmd::Ls => println!("{}", serde_json::to_string_pretty(&c.call(None, "relays.get", Value::Null).await?)?),
                RelayCmd::Set { urls } => {
                    c.call(None, "relays.set", json!({ "relays": urls })).await?;
                    println!("cluster relays set; machines switch within about 10 s");
                }
            }
            Ok(())
        }
        Cmd::Call { method, params, machine } => {
            let (c, _) = connect(&cli.connect, &cli.context).await?;
            let to = match machine {
                Some(m) => Some(resolve(&c, &m).await?),
                None => None,
            };
            let params = match params {
                Some(p) => serde_json::from_str(&p).context("params must be JSON")?,
                None => Value::Null,
            };
            let v = c.call(to, &method, params).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
            c.close().await;
            Ok(())
        }
    }
}

/// The daemon on this machine, or the `--connect` target.
async fn local_client(connect: &Option<String>) -> Result<Client> {
    let conn = match connect {
        Some(c) => Connection::parse(c)?,
        None => Connection { local: Some(local::DEFAULT_PATH.into()), ..Default::default() },
    };
    Client::open(&conn).await
}

const JOIN_PREFIX: &str = "imeshjoin";

/// A join ticket carries the `cluster.add` result: the cluster ID, member addresses, and the
/// namespace secret of the cluster document. Treat it as a secret.
fn encode_join_ticket(admit: &Value) -> Result<String> {
    Ok(format!("{JOIN_PREFIX}{}", data_encoding::BASE64URL_NOPAD.encode(&serde_json::to_vec(admit)?)))
}

fn decode_join_ticket(t: &str) -> Result<Value> {
    let body = t.trim().strip_prefix(JOIN_PREFIX).context("not an imesh join ticket")?;
    let bytes = data_encoding::BASE64URL_NOPAD.decode(body.as_bytes()).context("bad join ticket")?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Resolves the connection list: `--connect` wins, else the context from flags or config.
async fn connect(connect: &Option<String>, context: &Option<String>) -> Result<(Client, Connection)> {
    let conns = match connect {
        Some(c) => vec![Connection::parse(c)?],
        None => {
            let cfg = Config::load()?;
            if cfg.contexts.is_empty() && context.is_none() {
                vec![Connection { local: Some(local::DEFAULT_PATH.into()), ..Default::default() }]
            } else {
                cfg.context(context.as_deref())?.1.connections.clone()
            }
        }
    };
    let (client, c) = Client::open_any(&conns).await?;
    let c = c.clone();
    Ok((client, c))
}

/// Installs or updates imesh on the server, then returns a client over SSH.
async fn provision(target: &Target) -> Result<(Client, Ssh)> {
    let ssh = Ssh::new(&target.dest, target.ssh_key.clone());
    let facts = ssh
        .script("uname -m; id -un; id -u; (sha256sum /usr/local/bin/imesh 2>/dev/null || echo none) | cut -d' ' -f1; test -S /run/imesh/imesh.sock && echo running || echo stopped; systemctl show -p User --value imesh 2>/dev/null || true")
        .await?;
    let f: Vec<&str> = facts.lines().collect();
    ensure!(f.len() >= 5, "unexpected output from {}: {facts}", ssh.dest());
    let (arch, user, uid, installed, running) = (f[0], f[1], f[2], f[3], f[4]);
    let run_user = f.get(5).copied().unwrap_or("");
    let sudo = if uid == "0" {
        ""
    } else {
        ssh.run("sudo -n true", b"").await.map_err(|_| {
            anyhow::anyhow!("user {user} needs passwordless sudo on {} to install imesh (NOPASSWD in /etc/sudoers.d)", ssh.dest())
        })?;
        "sudo -n "
    };
    let bin = match &target.binary {
        Some(b) => b.clone(),
        None => std::env::var_os(format!("IMESH_BINARY_{}", arch.to_uppercase()))
            .map(PathBuf::from)
            .with_context(|| format!("no imesh binary for linux/{arch}: pass --binary or set IMESH_BINARY_{}", arch.to_uppercase()))?,
    };
    let bytes = std::fs::read(&bin).with_context(|| format!("read {}", bin.display()))?;
    let sha = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&bytes));
    let mut env = target.relay.to_env();
    if !target.labels.is_empty() {
        env.push_str(&format!("IMESH_LABELS={}\n", target.labels.join(",")));
    }
    let current_env = ssh.run("cat /etc/imesh/env 2>/dev/null || true", b"").await?;
    let want_user = target.run_as.as_deref().unwrap_or(user);
    if installed != sha || running != "running" || current_env.trim() != env.trim() || run_user != want_user {
        let src = if installed == sha {
            String::new()
        } else {
            eprintln!("uploading {} ({} MB) to {}", bin.display(), bytes.len() >> 20, ssh.dest());
            let tmp = ssh.run("mktemp /tmp/imesh.XXXXXX", b"").await?.trim().to_string();
            ssh.run(&format!("cat > {tmp}"), &bytes).await?;
            tmp
        };
        eprintln!("installing imesh daemon on {}", ssh.dest());
        let script = format!("IMESH_ENV='{}'\n{INSTALL_SH}", env.trim());
        let run_as = target.run_as.as_deref().map(|u| format!(" IMESH_RUN_USER={u}")).unwrap_or_default();
        let run_group = target.run_as_group.as_deref().map(|g| format!(" IMESH_RUN_GROUP={g}")).unwrap_or_default();
        ssh.run(&format!("{sudo}env IMESH_SRC={src} IMESH_SHA256={sha} IMESH_USER={user}{run_as}{run_group} sh -s"), script.as_bytes())
            .await?;
        // A new login picks up the imesh group.
        ssh.close().await;
    }
    let client = Client::open(&Connection { ssh: Some(target.dest.clone()), ssh_key_file: target.ssh_key.clone(), ..Default::default() })
        .await
        .with_context(|| format!("reach the imesh daemon on {}", ssh.dest()))?;
    Ok((client, ssh))
}

async fn inspect(c: &Client) -> Result<(EndpointId, Value)> {
    let v = c.call(None, "machine.inspect", Value::Null).await?;
    Ok((serde_json::from_value(v["id"].clone())?, v))
}

fn connections(target: &Target, id: EndpointId) -> Vec<Connection> {
    vec![
        Connection { ssh: Some(target.dest.clone()), ssh_key_file: target.ssh_key.clone(), machine: Some(id), ..Default::default() },
        Connection { iroh: Some(id), relay: target.relay.relays.first().cloned(), machine: Some(id), ..Default::default() },
    ]
}

async fn authorize_cli(c: &Client) -> Result<()> {
    let key = config::client_key()?.public();
    let name = std::env::var("USER").unwrap_or_else(|_| "cli".into());
    c.call(None, "client.add", json!({ "key": key, "name": name })).await?;
    Ok(())
}

async fn init(context: &Option<String>, target: Target) -> Result<()> {
    let (c, _) = provision(&target).await?;
    let (id, info) = inspect(&c).await?;
    if let Some(cluster) = info["cluster"].as_str() {
        bail!("{} is already in cluster {cluster}. Remove it first with `imesh machine rm`.", target.dest);
    }
    let v = c.call(None, "cluster.init", json!({ "name": target.name })).await?;
    authorize_cli(&c).await?;
    let mut cfg = Config::load()?;
    let base = context.clone().unwrap_or_else(|| "default".into());
    let mut conns = connections(&target, id).into_iter();
    let name = cfg.add_context(&base, conns.next().unwrap());
    cfg.contexts.get_mut(&name).unwrap().connections.extend(conns);
    cfg.save()?;
    println!("cluster {} initialised on {} ({id})", v["cluster"].as_str().unwrap_or_default(), target.dest);
    println!("current context is now {name:?}");
    Ok(())
}

async fn add(context: &Option<String>, entry: &Client, mut target: Target) -> Result<()> {
    if let Ok(ticket) = target.dest.parse::<iroh_tickets::endpoint::EndpointTicket>() {
        return add_ticket(context, entry, &target, ticket.endpoint_addr().clone()).await;
    }
    let r = &mut target.relay;
    if r.relays.is_empty() && r.relay_token.is_none() && !r.no_n0 {
        let e = entry.call(None, "machine.inspect", Value::Null).await?;
        r.relays = serde_json::from_value(e["relays"].clone()).unwrap_or_default();
        r.relay_token = e["relay_token"].as_str().map(String::from);
        r.no_n0 = e["no_n0"].as_bool().unwrap_or(false);
    }
    let (c, _) = provision(&target).await?;
    let (id, info) = inspect(&c).await?;
    if let Some(cluster) = info["cluster"].as_str() {
        bail!("{} is already in cluster {cluster}", target.dest);
    }
    let name = target
        .name
        .clone()
        .unwrap_or_else(|| info["info"]["name"].as_str().unwrap_or("machine").to_string());
    let admit = entry.call(None, "cluster.add", json!({ "id": id, "name": name })).await?;
    let joined = c.call(None, "cluster.join", admit).await?;
    let mut cfg = Config::load()?;
    let ctx_name = cfg.context(context.as_deref())?.0.to_string();
    let ctx = cfg.contexts.get_mut(&ctx_name).unwrap();
    for conn in connections(&target, id) {
        if !ctx.connections.contains(&conn) {
            ctx.connections.push(conn);
        }
    }
    cfg.save()?;
    println!("machine {name} ({id}) joined cluster {} with {} members", joined["cluster"].as_str().unwrap_or_default(), joined["members"]);
    Ok(())
}

/// Admits a machine that installed itself, and prints the ticket it joins with. The machine's
/// key is admitted before it connects, so the cluster accepts it by its iroh identity.
async fn add_ticket(context: &Option<String>, entry: &Client, target: &Target, addr: iroh::EndpointAddr) -> Result<()> {
    let id = addr.id;
    let name = target.name.clone().unwrap_or_else(|| format!("machine-{}", id.fmt_short()));
    let admit = entry.call(None, "cluster.add", json!({ "id": id, "name": name })).await?;
    let mut cfg = Config::load()?;
    let ctx_name = cfg.context(context.as_deref())?.0.to_string();
    let conn = Connection {
        iroh: Some(id),
        relay: addr.relay_urls().next().cloned(),
        addrs: addr.ip_addrs().copied().collect(),
        machine: Some(id),
        ..Default::default()
    };
    let ctx = cfg.contexts.get_mut(&ctx_name).unwrap();
    if !ctx.connections.iter().any(|c| c.iroh == Some(id)) {
        ctx.connections.push(conn);
    }
    cfg.save()?;
    println!("admitted {name} ({id}). On that machine, run:\n\n  imesh machine join {}\n", encode_join_ticket(&admit)?);
    Ok(())
}

async fn machines(c: &Client) -> Result<Vec<Value>> {
    Ok(serde_json::from_value(c.call(None, "cluster.machines", Value::Null).await?)?)
}

async fn resolve(c: &Client, m: &str) -> Result<EndpointId> {
    if let Ok(id) = m.parse() {
        return Ok(id);
    }
    machines(c)
        .await?
        .iter()
        .find(|r| r["name"] == m)
        .and_then(|r| serde_json::from_value(r["id"].clone()).ok())
        .with_context(|| format!("no machine named {m:?}"))
}

async fn ls(c: &Client) -> Result<()> {
    println!("{:<12} {:<10} {:<8} {:<7} {:<8} {:<14} {}", "NAME", "ID", "STATE", "PATH", "RTT", "OS/ARCH", "VERSION");
    for r in machines(c).await? {
        let s = &r["status"];
        let i = &r["info"];
        println!(
            "{:<12} {:<10} {:<8} {:<7} {:<8} {:<14} {}",
            r["name"].as_str().unwrap_or(""),
            &r["id"].as_str().unwrap_or("")[..10],
            s["state"].as_str().unwrap_or(""),
            s["path"].as_str().unwrap_or("-"),
            s["rtt_ms"].as_u64().map(|v| format!("{v}ms")).unwrap_or("-".into()),
            format!("{}/{}", i["os"].as_str().unwrap_or("?"), i["arch"].as_str().unwrap_or("?")),
            i["version"].as_str().unwrap_or(""),
        );
    }
    Ok(())
}

async fn rm(connect: &Option<String>, context: &Option<String>, c: &Client, m: &str) -> Result<()> {
    let id = resolve(c, m).await?;
    let entry: EndpointId = serde_json::from_value(c.call(None, "machine.inspect", Value::Null).await?["id"].clone())?;
    ensure!(entry != id, "you are connected through {m}; connect through another machine to remove it");
    match c.call(Some(id), "machine.reset", Value::Null).await {
        Ok(_) => eprintln!("reset {m}"),
        Err(e) => eprintln!("could not reset {m}, removing it anyway: {e:#}"),
    }
    c.call(None, "cluster.remove", json!({ "id": id })).await?;
    if connect.is_none() {
        let mut cfg = Config::load()?;
        let name = cfg.context(context.as_deref())?.0.to_string();
        cfg.contexts.get_mut(&name).unwrap().connections.retain(|c| c.machine != Some(id));
        cfg.save()?;
    }
    println!("removed {m} ({id})");
    Ok(())
}

fn ctx(connect: &Option<String>, cmd: CtxCmd) -> Result<()> {
    ensure!(connect.is_none(), "--connect ignores the config; contexts are not used");
    let mut cfg = Config::load()?;
    match cmd {
        CtxCmd::Ls => {
            println!("{:<16} {:<8} {}", "NAME", "CURRENT", "CONNECTIONS");
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
            let name = cfg.context(None)?.0.to_string();
            let ctx = cfg.contexts.get_mut(&name).unwrap();
            if let Some(i) = index {
                ensure!(i < ctx.connections.len(), "no connection {i}");
                let c = ctx.connections.remove(i);
                ctx.connections.insert(0, c);
                cfg.save()?;
            }
            for (i, c) in cfg.contexts[&name].connections.iter().enumerate() {
                println!("{i}  {c}");
            }
        }
    }
    Ok(())
}
