//! Installing the daemon so it keeps running.
//!
//! - **For a user**, the default for everyone but root: no root and no sudo, like tmux. The binary
//!   goes to `~/.local/bin`, the key and database to `~/.local/share/imesh`, the settings to
//!   `~/.config/imesh/daemon.env`, and the socket to `~/.local/share/imesh/imesh.sock`. It runs as a
//!   systemd user service that starts at boot, or, where that is not possible, as a background
//!   process that restarts itself and that the user's crontab starts at boot.
//! - **For the system** (`--system`, and any root login over SSH): a systemd unit with a service
//!   user, and a socket group `imesh`.
//! - **On Windows**: a scheduled task for the user.
//!
//! `imesh machine init/add/update` do the same over SSH ([`remote`]).

mod docker;
mod remote;
#[cfg(unix)]
mod system;
#[cfg(unix)]
mod user;
#[cfg(windows)]
mod windows;

use std::{collections::BTreeMap, io::Read, path::PathBuf};

use anyhow::{Context, Result, ensure};
pub use remote::{Mode, Target, provision};
#[cfg(unix)]
pub use user::{start, stop, supervise};

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

/// `imesh daemon install`.
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
    /// Refuse to install unless this binary has this SHA-256.
    #[arg(long, hide = true)]
    pub sha256: Option<String>,
}

/// Daemon environment variables. Stored as `VAR=value` lines.
pub type Env = BTreeMap<String, String>;

fn parse_env(text: &str) -> Env {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| l.trim().split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
        .collect()
}

/// Formats the env file. Refuses values that systemd's `EnvironmentFile=` would read differently
/// from [`parse_env`]: quotes, backslashes, line breaks, and spaces at either end. Windows paths
/// have backslashes, and no systemd reads the file there.
fn format_env(env: &Env) -> Result<String> {
    let special: &[char] =
        if cfg!(windows) { &['"', '\'', '\n', '\r', '\0'] } else { &['"', '\'', '\\', '\n', '\r', '\0'] };
    for (k, v) in env {
        ensure!(k.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'), "bad setting name {k:?}");
        ensure!(
            !v.contains(special) && v.trim() == v,
            "setting {k}: the value may not contain quotes, backslashes, line breaks, or spaces at either end"
        );
    }
    Ok(env.iter().map(|(k, v)| format!("{k}={v}\n")).collect())
}

fn sha256(bytes: &[u8]) -> String {
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

/// `imesh daemon install`: installs the running binary as this machine's daemon and starts it.
pub fn install_here(args: InstallArgs) -> Result<()> {
    let exe = std::env::current_exe().context("find the running binary")?;
    if let Some(want) = &args.sha256 {
        ensure!(sha256(&std::fs::read(&exe)?) == *want, "checksum mismatch: {}", exe.display());
    }
    let env = if args.settings_stdin {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        parse_env(&text)
    } else {
        args.settings.to_env()
    };
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

/// `imesh daemon uninstall`: stops the daemon and removes the service and the binary. With
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
