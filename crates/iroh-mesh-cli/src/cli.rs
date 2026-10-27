//! The command line: its commands, and what each one does.

use std::{path::PathBuf, process::ExitCode, sync::Arc};

use anyhow::{Context as _, Result};
use iroh_mesh::{
    api::{Inspect, RelaysInfo},
    client::Client,
    daemon,
};

use crate::{
    args::DaemonArgs,
    config::{Config, Connection, Context},
    connect, ctx,
    install::{self, Target},
    machine,
    output::OutputArgs,
    paths,
};
use clap::{Parser, Subcommand};
use iroh::EndpointId;
use serde_json::{Value, json};

#[derive(Parser)]
#[command(name = "imesh", version, about = "Connect your computers into one cluster, with no central server.")]
struct Cli {
    #[command(flatten)]
    at: At,
    /// Append logs to this file instead of stderr.
    #[arg(long, global = true, env = "IMESH_LOG_FILE")]
    log_file: Option<PathBuf>,
    /// Same as IMESH_HOME. Installed services pass it when IMESH_HOME was set at install.
    #[arg(long, global = true, hide = true)]
    imesh_home: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

/// Which cluster the CLI talks to: the daemon on this computer, else a context.
#[derive(clap::Args)]
pub(crate) struct At {
    /// Context to use instead of the current one.
    #[arg(long, short = 'c', global = true, env = "IMESH_CONTEXT")]
    pub(crate) context: Option<String>,
}

#[derive(Subcommand)]
enum SkillCmd {
    /// Install the skill as imesh/SKILL.md in a coding agent's skills directory. Default:
    /// ~/.agents/skills, which many agents read. Run it again after an update of imesh.
    Install {
        /// The agent whose skills directory to use. Repeat for more than one.
        #[arg(long, value_enum)]
        agent: Vec<crate::skill::Agent>,
        /// Every directory that --agent knows.
        #[arg(long, conflicts_with = "agent")]
        all: bool,
        /// Ask which agents to use.
        #[arg(short, long, conflicts_with_all = ["agent", "all"])]
        interactive: bool,
    },
    /// Remove the skill from a coding agent's skills directory. Default: ~/.agents/skills.
    Uninstall {
        /// The agent whose skills directory to use. Repeat for more than one.
        #[arg(long, value_enum)]
        agent: Vec<crate::skill::Agent>,
        /// Every directory that --agent knows.
        #[arg(long, conflicts_with = "agent")]
        all: bool,
        /// Ask which agents to use.
        #[arg(short, long, conflicts_with_all = ["agent", "all"])]
        interactive: bool,
    },
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // parsed once
enum Cmd {
    /// Install and start the daemon on this computer, and start a cluster if it is in none. With
    /// --join, join the cluster of a pairing code instead. Keeps its settings: flags you pass
    /// change only those settings.
    Up {
        /// Join the cluster of this pairing code, from `imesh invite` on a member.
        #[arg(long)]
        join: Option<String>,
        /// Join even when this machine's own cluster holds data, which is then lost.
        #[arg(long, requires = "join")]
        force: bool,
        #[command(flatten)]
        args: install::LocalArgs,
        #[command(flatten)]
        out: OutputArgs,
    },
    /// Install imesh on a server over SSH. It joins the current cluster, or starts a new one when
    /// there is none.
    Add {
        #[command(flatten)]
        target: Target,
        /// Start a new cluster on the server, also when there is a current cluster.
        #[arg(long)]
        new: bool,
        #[command(flatten)]
        out: OutputArgs,
    },
    /// Pair a machine that you can open a terminal on. Prints a code for `imesh up --join` on
    /// that machine, waits, and admits it after you confirm.
    Invite {
        /// The member name of the new machine.
        #[arg(value_parser = crate::args::member_name)]
        name: String,
        /// Pair through a private devtunnel. Each side uses its own devtunnel login. Without it,
        /// pair over iroh: the new machine must reach this computer directly or through a relay.
        #[arg(long)]
        devtunnel: bool,
        /// Admit the machine without asking.
        #[arg(long, short)]
        yes: bool,
        #[command(flatten)]
        out: OutputArgs,
    },
    /// List the machines of the cluster, and how each one is reached.
    Status {
        /// Show full IDs, last contact, and link errors.
        #[arg(long, short)]
        wide: bool,
        #[command(flatten)]
        out: OutputArgs,
    },
    /// Check that a machine answers, and show the path and the round-trip time.
    Ping {
        machine: String,
        /// How many pings to send.
        #[arg(short = 'n', long, default_value_t = 4)]
        count: u32,
        /// Ping until the path is direct, not through a relay, up to `count` times.
        #[arg(long)]
        until_direct: bool,
        #[command(flatten)]
        out: OutputArgs,
    },
    /// Rename a machine. `self` names this computer's machine.
    Rename {
        machine: String,
        #[arg(value_parser = crate::args::member_name)]
        name: String,
    },
    /// Remove a machine from the cluster and reset it.
    Rm {
        machine: String,
        /// Remove it even though it serves a relay that the cluster uses.
        #[arg(long)]
        force: bool,
        #[command(flatten)]
        out: OutputArgs,
    },
    /// Install a new version on this computer, or with `server user@host` on a server over SSH.
    /// Keeps the settings: flags you pass change only those settings.
    #[command(args_conflicts_with_subcommands = true)]
    Update {
        #[command(flatten)]
        args: install::LocalArgs,
        #[command(flatten)]
        out: OutputArgs,
        #[command(subcommand)]
        server: Option<UpdateCmd>,
    },
    /// Stop the daemon on this computer. It stays in its cluster; `imesh up` starts it again.
    Down,
    /// Leave the cluster for good: another member removes this machine, and its key, data, and
    /// devtunnel are deleted.
    Logout {
        /// Delete the data even when no other member can be told. Then remove this machine with
        /// `imesh rm` from another machine.
        #[arg(long)]
        force: bool,
        /// Log out the system daemon (as root or an administrator) instead of this user's.
        #[arg(long)]
        system: bool,
    },
    /// SSH to a machine of the cluster: `ssh` with every argument passed through. The machine
    /// must run `imesh up --ssh`. This machine's daemon connects, with its machine key.
    #[command(disable_help_flag = true)]
    Ssh {
        /// The arguments of `ssh`, with [user@]machine as the destination.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        args: Vec<String>,
    },
    /// Copy files with a machine of the cluster: `scp` with every argument passed through, and
    /// <machine>:<path> for the remote side.
    #[command(disable_help_flag = true)]
    Scp {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        args: Vec<String>,
    },
    /// Small values that every machine has a copy of. The last write wins.
    #[command(subcommand)]
    Kv(KvCmd),
    /// Configure this computer to use the cluster in more ways.
    #[command(subcommand)]
    Configure(ConfigureCmd),
    /// Show or set the relays that every machine in the cluster uses.
    #[command(subcommand)]
    Relay(RelayCmd),
    /// Manage contexts: the clusters this CLI reaches over SSH.
    #[command(subcommand, name = "ctx")]
    Ctx(CtxCmd),
    /// Manage the daemon service on this computer.
    #[command(subcommand)]
    Service(ServiceCmd),
    /// Print the agent skill file, which teaches a coding agent to use iroh_mesh, or install it.
    Skill {
        #[command(subcommand)]
        cmd: Option<SkillCmd>,
    },
    /// Commands for tests and apps built on imesh: raw API calls, SQL, and app tables.
    #[command(subcommand, hide = true)]
    Dev(DevCmd),
    /// Bridge stdin and stdout to the local API.
    #[command(hide = true)]
    DialStdio {
        #[arg(long, env = "IMESH_SOCKET", default_value_os_t = paths::find())]
        socket: PathBuf,
    },
    /// Write the manifest and ZIP for the executables in <dir>/targets/<triple>/. `cargo x build` runs this.
    #[command(hide = true)]
    BundlePack {
        dir: PathBuf,
        zip: PathBuf,
        #[arg(long)]
        revision: String,
    },
    /// Run by SSH as a ProxyCommand: reach an SSH server through a private devtunnel.
    #[command(hide = true)]
    SshTunnelProxy {
        tunnel: iroh_mesh::net::DevTunnel,
        #[arg(long)]
        devtunnel_cli: Option<PathBuf>,
    },
    /// Run by SSH as a ProxyCommand: reach a machine's imesh SSH server over the cluster.
    #[command(hide = true)]
    SshProxy { machine: EndpointId },
}

#[derive(Subcommand)]
enum UpdateCmd {
    /// Update a server over SSH instead of this computer.
    Server {
        #[command(flatten)]
        target: Target,
        #[command(flatten)]
        out: OutputArgs,
    },
}

#[derive(Subcommand)]
enum KvCmd {
    /// Set a value on every machine. The value is JSON, or a string when it is not JSON.
    Set {
        key: String,
        value: String,
        /// Remove the key after this long, for example 30s, 10m, 12h, or 7d.
        #[arg(long, value_parser = crate::args::parse_period)]
        ttl: Option<std::time::Duration>,
    },
    /// Print a value from this machine's copy.
    Get {
        key: String,
        #[command(flatten)]
        out: OutputArgs,
    },
    /// List the keys and values, or those whose key starts with a prefix.
    Ls {
        prefix: Option<String>,
        #[command(flatten)]
        out: OutputArgs,
    },
    /// Remove a key from every machine.
    Rm { key: String },
}

#[derive(Subcommand)]
enum DevCmd {
    /// Call an API method.
    Call {
        method: String,
        /// JSON params.
        params: Option<String>,
        /// Read JSON parameters from stdin. Used by remote installers.
        #[arg(long, conflicts_with = "params")]
        params_stdin: bool,
        /// Machine (name or id) that serves the call. Defaults to the machine you connect to.
        #[arg(long, short)]
        machine: Option<String>,
    },
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
    /// Apply CREATE TABLE and CREATE INDEX statements from a file. Every machine takes the new
    /// tables before any row that uses them. A table that exists may only grow.
    SchemaApply { file: PathBuf },
    /// List this machine's tables, and for each member whether rows pass.
    SchemaLs,
}

#[derive(Subcommand)]
enum ConfigureCmd {
    /// Print SSH Host blocks for the machines of the cluster, for ssh, scp, and tools like tssh.
    /// With --write, add them to ~/.ssh/imesh/config, which ~/.ssh/config includes.
    Ssh {
        /// Machines to include, by name or ID prefix. Default: every other machine.
        machines: Vec<String>,
        /// Add or update the blocks in ~/.ssh/imesh/config. Blocks of other machines stay.
        #[arg(long)]
        write: bool,
    },
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // parsed once
enum ServiceCmd {
    /// Install this binary as this user's daemon and start it; no root or sudo. `--system`
    /// installs a system service instead (as root). Windows: a scheduled task for the user.
    Install(install::InstallArgs),
    /// Stop the daemon and remove the service and the binary. Keeps the key and data; `imesh
    /// logout` deletes them.
    Uninstall {
        /// Remove the system service (as root) instead of this user's daemon.
        #[arg(long)]
        system: bool,
    },
    /// Print whether a daemon runs here and its cluster, as JSON.
    Status {
        #[arg(long, env = "IMESH_SOCKET", default_value_os_t = paths::find())]
        socket: PathBuf,
    },
    /// Start the installed daemon.
    Start,
    /// Restart the installed daemon.
    Restart,
    /// Run the daemon in the foreground. The installed service runs this.
    #[command(hide = true)]
    Run {
        #[command(flatten)]
        args: DaemonArgs,
    },
    /// Run the daemon and start it again when it fails, for an install without systemd.
    #[cfg(unix)]
    #[command(hide = true)]
    Supervise,
    /// Run as a Windows service, for the Service Control Manager.
    #[cfg(windows)]
    #[command(hide = true)]
    WindowsService,
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
pub(crate) enum CtxCmd {
    /// List contexts.
    Ls,
    /// Make a context current.
    Use { name: String },
    /// List the connections of the current context, or move one to the top.
    Conn { index: Option<usize> },
    /// Add connections to a context, creating it if needed: user@host, ssh://user@host[:port],
    /// local://<path>.
    Add {
        name: String,
        #[arg(required = true)]
        connections: Vec<String>,
    },
}

/// Adds the SSH server, for every member, when `on`. As Tailscale SSH: the machine key is the
/// identity, and a later policy can narrow who may connect.
pub(crate) fn with_ssh(b: daemon::Builder, args: &DaemonArgs) -> daemon::Builder {
    // `imesh ssh` from this computer. Not from a system daemon: every member of its socket group
    // could then open a shell on other members as their users.
    let b = if args.socket_group.is_none() { b.openable(crate::ssh_server::ALPN) } else { b };
    let s = &args.settings;
    if !s.ssh() {
        return b;
    }
    iroh_mesh::os::allow_job_breakaway();
    let policy = crate::ssh_policy::Policy {
        record: (s.ssh_record == Some(true)).then(|| crate::ssh_policy::recordings_dir(&args.data_dir)),
    };
    b.protocol_with(crate::ssh_server::ALPN, move |m| crate::ssh_server::SshProtocol::new(m, policy))
}

/// Adds an app's tables, methods, and protocols to a daemon. See [`iroh_mesh::daemon::Builder`].
pub type Register = Arc<dyn Fn(daemon::Builder) -> daemon::Builder + Send + Sync>;

/// Runs the `imesh` command and, for `imesh service run`, the daemon with `register` applied. Prints
/// an error and returns a failure code when the command fails.
pub fn run(register: impl Fn(daemon::Builder) -> daemon::Builder + Send + Sync + 'static) -> ExitCode {
    // The daemon and the processes that relay streams must survive a closed peer.
    let relays = std::env::args().nth(1).is_some_and(|a| a == "service" || a == "dial-stdio" || a.ends_with("-proxy"));
    if !relays {
        iroh_mesh::os::exit_on_closed_pipe();
    }
    match start(Arc::new(register)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn start(register: Register) -> Result<()> {
    // `--imesh-home` must apply before the env file and the default paths are read.
    let all: Vec<std::ffi::OsString> = std::env::args_os().collect();
    for (i, a) in all.iter().enumerate() {
        let home = match a.to_str() {
            Some("--imesh-home") => all.get(i + 1).cloned(),
            Some(s) if s.starts_with("--imesh-home=") => Some(s["--imesh-home=".len()..].into()),
            _ => None,
        };
        if let Some(h) = home {
            // SAFETY: no other thread runs yet.
            unsafe { std::env::set_var(paths::HOME_VAR, h) };
        }
    }
    // A user daemon reads its settings from its own file, before clap reads the environment. A
    // system daemon gets them from systemd. Only the daemon itself: an install must not take the
    // old settings as defaults.
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some("service") && args.next().as_deref() == Some("run") {
        install::load_env_file();
    }
    let cli = Cli::parse();
    let level = if matches!(cli.cmd, Cmd::Service(ServiceCmd::Run { .. })) { "info" } else { "warn" };
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
        Cmd::Service(ServiceCmd::Install(args)) => install::install_here(args),
        Cmd::Service(ServiceCmd::Uninstall { system }) => install::uninstall(false, system),
        Cmd::Logout { force, system } => {
            let rt = tokio::runtime::Runtime::new()?;
            let left = rt.block_on(install::leave_before_purge(system, force))?;
            install::uninstall(true, system)?;
            if let Some(left) = left {
                rt.block_on(install::after_leave(left));
            }
            Ok(())
        }
        Cmd::Service(ServiceCmd::Start) => install::start_service(),
        Cmd::Down => install::stop_service(),
        Cmd::Service(ServiceCmd::Restart) => install::stop_service().and_then(|()| install::start_service()),
        #[cfg(unix)]
        Cmd::Service(ServiceCmd::Supervise) => install::supervise(),
        #[cfg(windows)]
        Cmd::Service(ServiceCmd::WindowsService) => crate::service::run(register),
        _ => tokio::runtime::Runtime::new()?.block_on(dispatch(cli, register)),
    }
}

/// `imesh service status`: whether a daemon answers on `socket`, and its cluster. A missing or
/// dead socket means none runs.
async fn status(socket: PathBuf) -> Result<()> {
    let conn = Connection { local: Some(socket.clone()), ..Default::default() };
    let inspect = match socket.exists() {
        false => None,
        true => match connect::open(&conn).await {
            Ok(c) => Some(c.call::<Inspect>(None, "machine.inspect", ()).await?),
            Err(e) if format!("{e:#}").contains("Connection refused") => None,
            Err(e) => return Err(e),
        },
    };
    println!(
        "{}",
        json!({
            "running": inspect.is_some(),
            "cluster": inspect.as_ref().and_then(|i| i.cluster.clone()),
            "alone_and_empty": inspect.as_ref().is_some_and(|i| i.alone_and_empty),
        })
    );
    Ok(())
}

async fn dispatch(cli: Cli, register: Register) -> Result<()> {
    let at = &cli.at;
    match cli.cmd {
        Cmd::Service(ServiceCmd::Status { socket }) => status(socket).await,
        Cmd::Service(ServiceCmd::Run { args }) => {
            let config = args.config();
            let ssh = args.settings.ssh();
            anyhow::ensure!(
                !ssh || args.socket_group.is_none(),
                "--ssh is for a user daemon; a system daemon would give shells as its service user"
            );
            daemon::serve(
                || with_ssh(register(daemon::Builder::new(config.clone())), &args),
                &args.local_api(),
                iroh_mesh::os::shutdown_signal(),
            )
            .await
        }
        Cmd::Service(_) | Cmd::Down | Cmd::Logout { .. } => unreachable!("handled before the runtime starts"),
        Cmd::DialStdio { socket } => paths::dial_stdio(socket).await,
        Cmd::Skill { cmd: None } => {
            print!("{}", crate::skill::TEXT);
            Ok(())
        }
        Cmd::Skill { cmd: Some(SkillCmd::Install { agent, all, interactive }) } => {
            let agent = if interactive { crate::skill::pick("install")? } else { agent };
            if interactive && agent.is_empty() {
                return Ok(());
            }
            crate::skill::install(&agent, all)
        }
        Cmd::Skill { cmd: Some(SkillCmd::Uninstall { agent, all, interactive }) } => {
            let agent = if interactive { crate::skill::pick("uninstall")? } else { agent };
            if interactive && agent.is_empty() {
                return Ok(());
            }
            crate::skill::uninstall(&agent, all)
        }
        Cmd::Ctx(cmd) => ctx::ctx(at, cmd),
        Cmd::Up { join, force, args, out } => {
            local_only(at)?;
            install::up(args, join, force, &out.output()?).await
        }
        Cmd::Update { server: Some(UpdateCmd::Server { target, out }), .. } => {
            machine::update(target, &out.output()?).await
        }
        Cmd::Update { args, out, server: None } => {
            local_only(at)?;
            install::update(args, &out.output()?).await
        }
        Cmd::Add { target, new, out } => machine::add(at, target, new, &out.output()?).await,
        Cmd::Invite { name, devtunnel, yes, out } => {
            crate::pair::invite(at, name, devtunnel, yes, &out.output()?).await
        }
        Cmd::BundlePack { dir, zip, revision } => {
            let m = crate::bundle::pack(&dir, &zip, env!("CARGO_PKG_VERSION"), &revision)?;
            println!("{}", serde_json::to_string_pretty(&m)?);
            Ok(())
        }
        Cmd::SshTunnelProxy { tunnel, devtunnel_cli } => crate::ssh::proxy(tunnel, devtunnel_cli).await,
        Cmd::SshProxy { machine } => crate::ssh_client::proxy(at, machine).await,
        Cmd::Ssh { args } => crate::ssh_client::ssh(at, args).await,
        Cmd::Scp { args } => crate::ssh_client::scp(at, args).await,
        Cmd::Configure(ConfigureCmd::Ssh { machines, write }) => crate::ssh_client::config(at, machines, write).await,
        cmd => {
            let c = at.client().await?;
            let r = api(at, &c, cmd).await;
            c.close().await;
            r
        }
    }
}

/// `up` and `update` without a host act on this computer only.
fn local_only(at: &At) -> Result<()> {
    anyhow::ensure!(
        at.context.is_none(),
        "this command acts on this computer; drop --context (for a server, use `imesh update server user@host`)"
    );
    Ok(())
}

/// The commands that call the API of one machine.
async fn api(at: &At, c: &Client, cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Status { wide, out } => machine::ls(c, wide, &out.output()?).await?,
        Cmd::Ping { machine, count, until_direct, out } => {
            machine::ping(c, &machine, count, until_direct, &out.output()?).await?
        }
        Cmd::Rename { machine, name } => {
            let id = if machine == "self" {
                c.call::<Inspect>(None, "machine.inspect", ()).await?.id
            } else {
                machine::resolve(c, &machine).await?
            };
            c.call::<Value>(None, "cluster.rename", json!({ "id": id, "name": name })).await?;
            println!("renamed {machine} to {name}");
        }
        Cmd::Rm { machine, force, out } => machine::rm(at, c, &machine, force, &out.output()?).await?,
        Cmd::Kv(KvCmd::Set { key, value, ttl }) => {
            let value: Value = serde_json::from_str(&value).unwrap_or(Value::String(value));
            c.call::<Value>(None, "kv.put", json!({ "key": key, "value": value, "ttl": ttl.map(|t| t.as_secs()) }))
                .await?;
        }
        Cmd::Kv(KvCmd::Get { key, out }) => {
            let v: Value = c.call(None, "kv.get", json!({ "key": key })).await?;
            out.output()?.print(&v, |v| match v {
                Value::Null => {}
                Value::String(s) => println!("{s}"),
                v => println!("{v}"),
            })?;
        }
        Cmd::Kv(KvCmd::Ls { prefix, out }) => {
            let v: Value = c.call(None, "kv.list", json!({ "prefix": prefix.unwrap_or_default() })).await?;
            out.output()?.print(&v, |v| {
                for (k, v) in v.as_object().into_iter().flatten() {
                    println!("{k}\t{v}");
                }
            })?;
        }
        Cmd::Kv(KvCmd::Rm { key }) => {
            c.call::<Value>(None, "kv.rm", json!({ "key": key })).await?;
        }
        Cmd::Relay(RelayCmd::Ls) => {
            let r: RelaysInfo = c.call(None, "relays.get", ()).await?;
            println!("{}", serde_json::to_string_pretty(&r)?);
        }
        Cmd::Relay(RelayCmd::Set { urls, token }) => {
            c.call::<Value>(None, "relays.set", json!({ "relays": urls, "token": token })).await?;
            println!("cluster relays set; every machine switches within about 10 s");
        }
        Cmd::Dev(DevCmd::SchemaApply { file }) => {
            let sql = std::fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))?;
            c.call::<Value>(None, "schema.apply", json!({ "sql": sql })).await?;
            println!("applied here; members take the tables before any row that uses them");
        }
        Cmd::Dev(DevCmd::SchemaLs) => {
            let v: Value = c.call(None, "schema.list", ()).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        Cmd::Dev(DevCmd::Sql { sql, params, exec }) => {
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
        Cmd::Dev(DevCmd::Call { method, params, params_stdin, machine }) => {
            let to = match machine {
                Some(m) => Some(machine::resolve(c, &m).await?),
                None => None,
            };
            let params = if params_stdin {
                use std::io::Read;
                let mut text = String::new();
                std::io::stdin().read_to_string(&mut text)?;
                Some(text)
            } else {
                params
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
    /// The machine the CLI talks to: with `-c`, a connection of that context. Else the daemon on
    /// this computer when it is a member, and else a connection of the current context.
    pub(crate) async fn client(&self) -> Result<Client> {
        self.entry()
            .await?
            .context("no imesh daemon runs here and there is no context: run `imesh up`, or `imesh add user@host`")
    }

    /// Like [`At::client`], but `None` when there is neither a member daemon here nor a context.
    pub(crate) async fn entry(&self) -> Result<Option<Client>> {
        let cfg = Config::load()?;
        if self.context.is_none() {
            if let Ok(c) = connect::open(&Connection::local()).await {
                match c.call::<Inspect>(None, "machine.inspect", ()).await {
                    Ok(i) if i.cluster.is_some() => return Ok(Some(c)),
                    _ => c.close().await,
                }
            }
            if cfg.current_context.is_none() {
                return Ok(None);
            }
        }
        let conns = cfg.context(self.context.as_deref())?.1.connections.clone();
        Ok(Some(connect::open_any(&conns).await?.0))
    }

    /// Changes the context that selected the entry machine. With none, nothing changes: the CLI
    /// used the daemon on this computer, which needs no context.
    pub(crate) fn edit_context(&self, f: impl FnOnce(&mut Context)) -> Result<()> {
        Config::edit(|cfg| {
            let Ok(ctx) = cfg.context_mut(self.context.as_deref()) else { return Ok(()) };
            f(ctx);
            Ok(())
        })
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
