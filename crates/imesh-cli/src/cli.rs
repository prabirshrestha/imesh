//! The command line: its commands, and what each one does.

use std::{path::PathBuf, process::ExitCode, sync::Arc};

use anyhow::{Context as _, Result};
use imesh::{
    api::{ClientRow, Inspect, JoinTicket, Joined, RelaysInfo},
    client::Client,
    daemon,
};

use crate::{
    args::DaemonArgs,
    config::{self, Config, Connection, Context},
    connect, ctx,
    install::{self, Mode, Target},
    machine, paths,
};
use clap::{Parser, Subcommand};
use iroh::EndpointId;
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
pub(crate) struct At {
    /// Connect to this machine and ignore the config: user@host, ssh://user@host, iroh://<id>, local://<path>.
    #[arg(long, global = true, env = "IMESH_CONNECT")]
    pub(crate) connect: Option<String>,
    /// Context to use instead of current_context.
    #[arg(long, short = 'c', global = true, env = "IMESH_CONTEXT")]
    pub(crate) context: Option<String>,
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
        args: DaemonArgs,
    },
    /// Bridge stdin and stdout to the local API.
    #[command(hide = true)]
    DialStdio {
        #[arg(long, env = "IMESH_SOCKET", default_value_os_t = paths::find())]
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
    /// Add or grow app tables on every machine.
    #[command(subcommand)]
    Schema(SchemaCmd),
    /// Query this machine's copy of the database, or change rows on every machine with --exec.
    Sql {
        /// One SQL statement. Without --exec it must be read-only.
        sql: String,
        /// Parameters for ?1, ?2, ... as JSON values.
        #[arg(long = "param", short)]
        params: Vec<String>,
        /// Change rows: run the statement in a transaction and send the change to every machine.
        #[arg(long)]
        exec: bool,
    },
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
    /// Install this binary as this user's daemon and start it; no root or sudo. `--system`
    /// installs a system service instead (as root). Windows: a scheduled task for the user.
    Install(install::InstallArgs),
    /// Stop the daemon and remove the service and the binary. Keeps the data unless --purge.
    Uninstall {
        #[arg(long)]
        purge: bool,
        /// Remove the system service (as root) instead of this user's daemon.
        #[arg(long)]
        system: bool,
    },
    /// Print whether a daemon runs here and its cluster, as JSON.
    Status {
        #[arg(long, env = "IMESH_SOCKET", default_value_os_t = paths::find())]
        socket: PathBuf,
    },
    /// Start this user's daemon in the background, for an install without a systemd user service.
    #[cfg(unix)]
    Start,
    /// Stop the background daemon that `start` started.
    #[cfg(unix)]
    Stop,
    /// Run the daemon, and start it again when it fails. `start` runs this in the background.
    #[cfg(unix)]
    #[command(hide = true)]
    Supervise,
    /// Run the daemon as a Windows service. The Service Control Manager runs this.
    #[cfg(windows)]
    #[command(hide = true)]
    WindowsService,
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
enum SchemaCmd {
    /// Apply CREATE TABLE and CREATE INDEX statements from a file. Every machine takes the new
    /// tables before any row that uses them. A table that exists may only grow: new columns
    /// (nullable or with a default) and indexes.
    Apply { file: PathBuf },
    /// List this machine's tables, and for each member whether rows pass.
    Ls,
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
pub(crate) enum CtxCmd {
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

/// Adds an app's tables, methods, and protocols to a daemon. See [`imesh::daemon::Builder`].
pub type Register = Arc<dyn Fn(daemon::Builder) -> daemon::Builder + Send + Sync>;

/// Runs the `imesh` command and, for `imesh daemon`, the daemon with `register` applied. Prints
/// an error and returns a failure code when the command fails.
pub fn run(register: impl Fn(daemon::Builder) -> daemon::Builder + Send + Sync + 'static) -> ExitCode {
    match start(Arc::new(register)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn start(register: Register) -> Result<()> {
    // A user daemon reads its settings from its own file, before clap reads the environment. A
    // system daemon gets them from systemd. Only the daemon itself: an install must not take the
    // old settings as defaults.
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some("daemon") && args.next().is_none_or(|a| a.starts_with('-')) {
        install::load_env_file();
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
    // Installs and the supervisor manage processes and need no runtime; the supervisor builds
    // its own.
    match cli.cmd {
        Cmd::Daemon { cmd: Some(DaemonCmd::Install(args)), .. } => install::install_here(args),
        Cmd::Daemon { cmd: Some(DaemonCmd::Uninstall { purge, system }), .. } => install::uninstall(purge, system),
        #[cfg(unix)]
        Cmd::Daemon { cmd: Some(DaemonCmd::Start), .. } => install::start(),
        #[cfg(unix)]
        Cmd::Daemon { cmd: Some(DaemonCmd::Stop), .. } => install::stop(),
        #[cfg(unix)]
        Cmd::Daemon { cmd: Some(DaemonCmd::Supervise), .. } => install::supervise(),
        #[cfg(windows)]
        Cmd::Daemon { cmd: Some(DaemonCmd::WindowsService), .. } => crate::service::run(register),
        _ => tokio::runtime::Runtime::new()?.block_on(dispatch(cli, register)),
    }
}

/// `imesh daemon status`: whether a daemon answers on `socket`, and its cluster. A missing or
/// dead socket means none runs.
async fn status(socket: PathBuf) -> Result<()> {
    let conn = Connection { local: Some(socket.clone()), ..Default::default() };
    let cluster = match socket.exists() {
        false => None,
        true => match connect::open(&conn).await {
            Ok(c) => Some(c.call::<Inspect>(None, "machine.inspect", ()).await?.cluster),
            Err(e) if format!("{e:#}").contains("Connection refused") => None,
            Err(e) => return Err(e),
        },
    };
    let running = cluster.is_some();
    println!("{}", json!({ "running": running, "cluster": cluster.flatten() }));
    Ok(())
}

async fn dispatch(cli: Cli, register: Register) -> Result<()> {
    let at = &cli.at;
    match cli.cmd {
        Cmd::Daemon { cmd: Some(DaemonCmd::Status { socket }), .. } => status(socket).await,
        Cmd::Daemon { cmd: Some(_), .. } => unreachable!("handled before the runtime starts"),
        Cmd::Daemon { cmd: None, args } => {
            let config = args.config();
            daemon::serve(
                || register(daemon::Builder::new(config.clone())),
                &args.local_api(),
                imesh::os::shutdown_signal(),
            )
            .await
        }
        Cmd::DialStdio { socket } => paths::dial_stdio(socket).await,
        Cmd::Key => {
            println!("{}", config::client_key()?.public());
            Ok(())
        }
        Cmd::Ctx(cmd) => ctx::ctx(at, cmd),
        Cmd::Machine(MachineCmd::Init { target }) => machine::init(at, target).await,
        Cmd::Machine(MachineCmd::Update { target }) => {
            install::provision(&target, Mode::Update).await?.close().await;
            println!("updated {}", target.dest);
            Ok(())
        }
        cmd => {
            // `id` and `join` act on the daemon of this computer, not on the context.
            let c = match cmd {
                Cmd::Machine(MachineCmd::Id | MachineCmd::Join { .. }) => {
                    connect::open(&at.connect.as_deref().map_or_else(|| Ok(Connection::local()), Connection::parse)?)
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
            let j: Joined = c.call(None, "cluster.join", ticket.parse::<JoinTicket>()?).await?;
            println!("joined cluster {} with {} members", j.cluster, j.members);
        }
        Cmd::Machine(MachineCmd::Add { target }) => machine::add(at, c, target).await?,
        Cmd::Machine(MachineCmd::Ls { wide }) => machine::ls(c, wide).await?,
        Cmd::Machine(MachineCmd::Rm { machine, force }) => machine::rm(at, c, &machine, force).await?,
        Cmd::Schema(SchemaCmd::Apply { file }) => {
            let sql = std::fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))?;
            c.call::<Value>(None, "schema.apply", json!({ "sql": sql })).await?;
            println!("applied here; members take the tables before any row that uses them");
        }
        Cmd::Schema(SchemaCmd::Ls) => {
            let v: Value = c.call(None, "schema.list", ()).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        Cmd::Sql { sql, params, exec } => {
            let params = params
                .iter()
                .map(|p| serde_json::from_str(p).or_else(|_| Ok::<_, anyhow::Error>(Value::String(p.clone()))))
                .collect::<Result<Vec<Value>>>()?;
            let stmt = json!({ "sql": sql, "params": params });
            if exec {
                let counts: Vec<usize> = c.call(None, "sql.execute", json!({ "statements": [stmt] })).await?;
                println!("{} rows changed", counts.iter().sum::<usize>());
            } else {
                let v: Value = c.call(None, "sql.query", stmt).await?;
                print_rows(&v);
            }
        }
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
                Some(m) => Some(machine::resolve(c, &m).await?),
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
    pub(crate) async fn client(&self) -> Result<Client> {
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
        Ok(connect::open_any(&conns).await?.0)
    }

    /// Changes the context that selected the entry machine. With `--connect` no context did, so
    /// nothing changes.
    pub(crate) fn edit_context(&self, f: impl FnOnce(&mut Context)) -> Result<()> {
        if self.connect.is_some() {
            return Ok(());
        }
        let mut cfg = Config::load()?;
        let Ok(ctx) = cfg.context_mut(self.context.as_deref()) else { return Ok(()) };
        f(ctx);
        cfg.save()
    }
}

/// Prints `sql.query` results as a tab-separated table with a header.
fn print_rows(v: &Value) {
    let cell = |c: &Value| match c {
        Value::String(s) => s.clone(),
        Value::Null => "NULL".into(),
        other => other.to_string(),
    };
    if let Some(cols) = v["columns"].as_array() {
        println!("{}", cols.iter().map(cell).collect::<Vec<_>>().join("\t"));
    }
    for row in v["rows"].as_array().into_iter().flatten() {
        println!("{}", row.as_array().into_iter().flatten().map(cell).collect::<Vec<_>>().join("\t"));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn cli_is_valid() {
        <super::Cli as clap::CommandFactory>::command().debug_assert();
    }
}
