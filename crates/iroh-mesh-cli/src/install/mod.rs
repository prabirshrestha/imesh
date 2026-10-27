//! Installing the daemon so it keeps running.
//!
//! - **For a user**, the default for everyone but root: no root and no sudo. The daemon
//!   binary goes to `~/.local/share/imesh/bin`, the key and database to `~/.local/share/imesh`, the settings to
//!   `~/.config/imesh/daemon.env`, and the socket to `~/.local/share/imesh/imesh.sock`. It runs as a
//!   systemd user service that starts at boot, or, where that is not possible, as a background
//!   process that restarts itself and that the user's crontab starts at boot.
//! - **For the system** (`--system`, and any root login over SSH): a systemd unit with a service
//!   user, and a socket group `imesh`.
//! - **On Windows**: a scheduled task for the user.
//!
//! `imesh add` and `imesh update server` do the same over SSH ([`remote`]).

mod docker;
mod local;
mod remote;
mod remote_windows;
#[cfg(unix)]
mod system;
#[cfg(unix)]
mod user;
#[cfg(windows)]
mod windows;

use std::{collections::BTreeMap, io::Read, path::PathBuf};

use anyhow::{Context, Result, ensure};
pub use local::{LocalArgs, up, update};
pub use remote::{Mode, Target, provision};
#[cfg(unix)]
pub use user::supervise;

use crate::args::Settings;

/// How a user install keeps the daemon running.
#[derive(Debug, Clone, Copy, Default, PartialEq, clap::ValueEnum)]
pub enum Service {
    /// A systemd user service when the user may keep one running after logout, else detached.
    #[default]
    Auto,
    /// A systemd user service, with linger so it starts at boot and outlives the login.
    Systemd,
    /// A background process that restarts itself; the user's crontab starts it at boot.
    Detached,
}

/// `imesh service install`.
#[derive(Debug, clap::Args)]
pub struct InstallArgs {
    #[command(flatten)]
    pub settings: Settings,
    /// Install a system service that starts at boot, before anyone logs in. Linux: a systemd unit
    /// with its own user; needs root, and root always gets one. Windows: a Windows service as
    /// `NT SERVICE\imesh`; needs an elevated shell.
    #[arg(long)]
    pub system: bool,
    /// How a user install keeps the daemon running.
    #[arg(long, value_enum, default_value_t)]
    pub service: Service,
    /// With --system: run the daemon as this user. Defaults to the user who runs the install,
    /// or `imesh` for root. Created as a system user if missing.
    #[arg(long, requires = "system")]
    pub run_as: Option<String>,
    /// With --system: the socket's group. Defaults to `imesh`.
    #[arg(long, requires = "system")]
    pub run_as_group: Option<String>,
    /// Read the daemon settings from stdin, as VAR=value lines, instead of from the options.
    #[arg(long, hide = true)]
    pub settings_stdin: bool,
    /// Keep this verified bundle ZIP for onward installs and `imesh update`.
    #[arg(long, hide = true)]
    pub bundle: Option<PathBuf>,
    /// Refuse to install unless this binary has this SHA-256.
    #[arg(long, hide = true)]
    pub sha256: Option<String>,
}

/// Daemon environment variables. Stored as `VAR=value` lines.
pub type Env = BTreeMap<String, String>;

pub(crate) fn parse_env(text: &str) -> Env {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| l.trim().split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
        .collect()
}

/// Formats the env file. Refuses values that systemd's `EnvironmentFile=` would read differently
/// from [`parse_env`]: quotes, backslashes, line breaks, and spaces at either end. Windows paths
/// have backslashes, and no systemd reads the file there.
pub(crate) fn format_env(env: &Env) -> Result<String> {
    format_env_for(env, cfg!(windows))
}

pub(crate) fn format_env_for(env: &Env, windows: bool) -> Result<String> {
    let special: &[char] = if windows { &['"', '\'', '\n', '\r', '\0'] } else { &['"', '\'', '\\', '\n', '\r', '\0'] };
    for (k, v) in env {
        ensure!(k.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'), "bad setting name {k:?}");
        ensure!(
            !v.contains(special) && v.trim() == v,
            "setting {k}: the value may not contain quotes, backslashes, line breaks, or spaces at either end"
        );
    }
    Ok(env.iter().map(|(k, v)| format!("{k}={v}\n")).collect())
}

pub(crate) fn sha256(bytes: &[u8]) -> String {
    hex::encode(<sha2::Sha256 as sha2::Digest>::digest(bytes))
}

/// Where a user daemon keeps its settings. The daemon reads it at start.
pub fn env_file() -> PathBuf {
    #[cfg(unix)]
    return crate::config::config_dir().join("daemon.env");
    #[cfg(windows)]
    return crate::paths::data_dir().join("env");
}

/// Loads the env file into the process environment, without overriding what is set. A system
/// daemon gets its settings from systemd instead, and must not read the settings of the user
/// whose home it may share.
pub fn load_env_file() {
    if std::env::var_os("IMESH_SOCKET").is_some_and(|s| s == crate::paths::SYSTEM_SOCKET) {
        return;
    }
    load_env(&env_file());
}

/// Sets each `VAR=value` of the file that is not set yet. Call it before any other thread runs.
pub fn load_env(path: &std::path::Path) {
    let Ok(text) = std::fs::read_to_string(path) else { return };
    for (k, v) in parse_env(&text) {
        if std::env::var_os(&k).is_none() {
            // SAFETY: the caller runs this before any other thread starts.
            unsafe { std::env::set_var(k, v) };
        }
    }
}

/// Keeps the full path of the devtunnel CLI that this shell finds, when the daemon uses devtunnel.
/// A daemon started at boot, by cron or launchd, may have a PATH without it.
fn remember_devtunnel_cli(env: &mut Env) {
    let uses = ["IMESH_DEVTUNNEL", "IMESH_EXPOSE_RELAY"].iter().any(|k| env.contains_key(*k))
        || env.get("IMESH_RELAYS").is_some_and(|r| r.split(',').any(|r| r == "devtunnel"));
    if !uses {
        env.remove("IMESH_DEVTUNNEL_CLI");
        return;
    }
    let cli = crate::devtunnel_cli::devtunnel_cli();
    if cli.is_absolute() {
        env.insert("IMESH_DEVTUNNEL_CLI".into(), cli.display().to_string());
    }
}

/// `imesh service install`: installs the running binary as this machine's daemon and starts it.
pub fn install_here(args: InstallArgs) -> Result<()> {
    let exe = std::env::current_exe().context("find the running binary")?;
    if let Some(want) = &args.sha256 {
        ensure!(sha256(&std::fs::read(&exe)?) == *want, "checksum mismatch: {}", exe.display());
    }
    let mut env = if args.settings_stdin {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        parse_env(&text)
    } else {
        args.settings.to_env()
    };
    remember_devtunnel_cli(&mut env);
    if let Some(zip) = &args.bundle {
        crate::bundle::keep(zip, &crate::paths::bundle_dir()).context("keep the bundle")?;
    }
    #[cfg(windows)]
    if args.system {
        windows::install_system(&exe, &env)
    } else {
        windows::install(&exe, &env)
    }
    #[cfg(unix)]
    if args.system || is_root() {
        system::install(&exe, &env, args.run_as.as_deref(), args.run_as_group.as_deref())
    } else {
        user::install(&exe, &env, args.service)
    }
}

/// What `uninstall --purge` must clean up after the daemon stops: the devtunnel that the machine
/// hosted, which it may delete only once its host no longer runs.
pub struct Left {
    machine: iroh::EndpointId,
    tunnel: Option<String>,
}

/// Before `uninstall --purge` deletes the key: the running daemon leaves its cluster, so the
/// other members drop it now instead of showing it down forever. With `force`, a failure is only
/// a warning, and the data goes anyway.
pub async fn leave_before_purge(system: bool, force: bool) -> Result<Option<Left>> {
    let (socket, data) = if system {
        (PathBuf::from(crate::paths::SYSTEM_SOCKET), system_data_dir())
    } else {
        (crate::paths::socket(), crate::paths::data_dir())
    };
    let has_key = data.join(iroh_mesh::daemon::KEY_FILE).exists();
    let client = iroh_mesh::client::Client::local(&socket);
    let refuse = |e: anyhow::Error| -> Result<()> {
        if force || !has_key {
            eprintln!(
                "warning: the cluster was not told ({e:#}). Remove this machine with `imesh rm` from another machine."
            );
            Ok(())
        } else {
            Err(e.context("could not leave the cluster, so nothing was deleted. Start the daemon and another member, then try again, or pass --force"))
        }
    };
    let inspect = match client.call::<iroh_mesh::api::Inspect>(None, "machine.inspect", ()).await {
        Ok(i) => i,
        Err(e) => return refuse(e.context(format!("no daemon answers at {}", socket.display()))).map(|()| None),
    };
    // The tunnel goes after the daemon stops, also when the cluster could not be told.
    let tunnel = inspect.info.tunnel.as_ref().map(|iroh_mesh::state::TunnelEndpoint::Devtunnel { id, .. }| id.clone());
    if inspect.cluster.is_some() {
        match client.call::<serde_json::Value>(None, "machine.leave", ()).await {
            Ok(v) => {
                println!("left the cluster; removed by {}", v["removed_by"].as_str().unwrap_or("another member"))
            }
            Err(e) => refuse(e)?,
        }
    }
    Ok(Some(Left { machine: inspect.id, tunnel }))
}

/// After the daemon stopped: deletes the devtunnel that the machine hosted, if it carries the
/// machine's label.
pub async fn after_leave(left: Left) {
    let Some(tunnel) = left.tunnel else { return };
    match iroh_mesh::net::delete_devtunnel(Some(&crate::devtunnel_cli::devtunnel_cli()), &tunnel, left.machine).await {
        Ok(true) => println!("deleted devtunnel {tunnel}"),
        Ok(false) => println!("kept devtunnel {tunnel}: it does not carry this machine's label"),
        Err(e) => eprintln!("warning: could not delete devtunnel {tunnel}: {e:#}"),
    }
}

fn system_data_dir() -> PathBuf {
    #[cfg(windows)]
    return crate::paths::system_data_dir();
    #[cfg(unix)]
    return PathBuf::from("/var/lib/imesh");
}

/// `imesh service uninstall`: stops the daemon and removes the service and the binary. With
/// `purge`, also the key, the database, and the settings.
pub fn uninstall(purge: bool, system: bool) -> Result<()> {
    #[cfg(unix)]
    if system || is_root() {
        system::uninstall(purge)?;
    } else {
        user::uninstall(purge)?;
    }
    #[cfg(windows)]
    if system {
        windows::uninstall_system(purge)?;
    } else {
        windows::uninstall(purge)?;
    }
    println!("imesh daemon removed{}", if purge { " with its data" } else { "; data kept" });
    Ok(())
}

/// Root gets a system service, as a root login over SSH does.
#[cfg(unix)]
fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// Accepts user and group names that are safe on a command line and in a unit file. The first
/// character is a letter, digit, or `_`, so a name is never read as an option.
pub(crate) fn is_name(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

pub fn start_service() -> Result<()> {
    #[cfg(unix)]
    return user::ensure_started();
    #[cfg(windows)]
    return windows::ensure_started();
}

pub fn stop_service() -> Result<()> {
    #[cfg(unix)]
    return user::stop_all();
    #[cfg(windows)]
    return windows::stop_service();
}
