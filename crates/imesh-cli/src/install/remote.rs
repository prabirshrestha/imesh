//! Installing on a server over SSH: `imesh machine init/add/update`.
//!
//! The CLI uploads the binary and runs `imesh daemon install` there, with the settings on stdin,
//! so the server installs itself the same way as a local install. For the SSH user by default: no
//! sudo. With `--system`, or for a root login, as a system service; a non-root user then needs
//! passwordless sudo.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::ValueEnum;
use serde::Deserialize;

use imesh::client::Client;

use super::{Env, Service, format_env, is_name, parse_env, sha256};
use crate::{args::Settings, config::Connection, connect, paths::SYSTEM_SOCKET, ssh::Ssh};

/// A Linux server to install on over SSH.
#[derive(Debug, Clone, clap::Args)]
pub struct Target {
    /// `user@host[:port]` to install over SSH, or, for `add`, a machine ticket from `imesh machine id`.
    pub dest: String,
    /// SSH private key. Uses the SSH agent and ~/.ssh/config when omitted.
    #[arg(short = 'i', long = "ssh-key")]
    pub ssh_key: Option<PathBuf>,
    /// Machine name. Defaults to the remote hostname. Required when adding by ticket.
    #[arg(long)]
    pub name: Option<String>,
    /// Install a system service instead of one for the SSH user. A root login always does; any
    /// other user then needs passwordless sudo.
    #[arg(long)]
    pub system: bool,
    /// How a user install keeps the daemon running.
    #[arg(long, value_enum, default_value_t)]
    pub service: Service,
    /// System service: run the daemon as this user. Defaults to the SSH user, or `imesh` for a
    /// root login. Created as a system user if missing.
    #[arg(long)]
    pub run_as: Option<String>,
    /// System service: the socket's group. Defaults to `imesh`.
    #[arg(long)]
    pub run_as_group: Option<String>,
    /// Run the daemon in a Docker container instead: sandboxed, started by Docker at boot. For
    /// hosts where installing is hard, such as a NAS. The SSH user must be able to run `docker`,
    /// or `sudo -n docker`.
    #[arg(long, conflicts_with_all = ["system", "run_as", "run_as_group"])]
    pub docker: bool,
    #[command(flatten)]
    pub container: super::docker::ContainerArgs,
    #[command(flatten)]
    pub settings: Settings,
    /// Linux imesh binary to install. Defaults to `$IMESH_BINARY_<ARCH>`, for example `IMESH_BINARY_AARCH64`.
    #[arg(long)]
    pub binary: Option<PathBuf>,
}

impl Target {
    /// The connection that reaches this server's daemon over SSH: on the host, or in its container.
    pub fn ssh(&self) -> Connection {
        Connection {
            ssh: Some(self.dest.clone()),
            ssh_key_file: self.ssh_key.clone(),
            docker: self.docker.then(|| self.container.docker_name.clone()),
            ..Default::default()
        }
    }

    /// The Linux binary for `arch`, its bytes, and its SHA-256.
    pub(super) fn load_binary(&self, arch: &str) -> Result<(PathBuf, Vec<u8>, String)> {
        let arch_var = format!("IMESH_BINARY_{}", arch.to_uppercase());
        let bin = match &self.binary {
            Some(b) => b.clone(),
            None => std::env::var_os(&arch_var)
                .map(PathBuf::from)
                .with_context(|| format!("no imesh binary for linux/{arch}: pass --binary or set {arch_var}"))?,
        };
        let bytes = std::fs::read(&bin).with_context(|| format!("read {}", bin.display()))?;
        let sha = sha256(&bytes);
        Ok((bin, bytes, sha))
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    /// A machine that must not be in a cluster yet.
    New,
    /// An installed machine whose binary or settings change. Settings not passed stay.
    Update,
}

/// What the server reports before an install.
struct Facts {
    arch: String,
    user: String,
    root: bool,
    home: String,
    /// This user's data directory for imesh, as the daemon there finds it.
    user_data: String,
    /// Checksums of `~/.local/bin/imesh` and `/usr/local/bin/imesh`.
    user_sha: Option<String>,
    system_sha: Option<String>,
    /// The user and group the system service runs as, if there is one.
    system_user: String,
    system_group: String,
    user_env: Env,
    system_env: Env,
}

const FACTS_SH: &str = r#"uname -m; id -un; id -u; printf '%s\n' "$HOME" "${XDG_DATA_HOME:-$HOME/.local/share}/imesh"
for b in "$HOME/.local/bin/imesh" /usr/local/bin/imesh; do if [ -x "$b" ]; then sha256sum "$b" | cut -d' ' -f1; else echo none; fi; done
systemctl show -p User --value imesh 2>/dev/null || echo
systemctl show -p Group --value imesh 2>/dev/null || echo
echo ---
cat "${XDG_CONFIG_HOME:-$HOME/.config}/imesh/daemon.env" 2>/dev/null || true
echo ---
if [ "$(id -u)" = 0 ]; then cat /etc/imesh/env; else sudo -n cat /etc/imesh/env; fi 2>/dev/null || true"#;

async fn facts(ssh: &Ssh) -> Result<Facts> {
    let out = ssh.script(FACTS_SH).await?;
    let mut parts = out.split("---\n");
    let head = parts.next().unwrap_or_default();
    let f: Vec<&str> = head.lines().map(str::trim).collect();
    ensure!(f.len() >= 7, "unexpected output from {}: {out}", ssh.dest());
    let sha = |s: &str| (s != "none").then(|| s.to_string());
    Ok(Facts {
        arch: f[0].into(),
        user: f[1].into(),
        root: f[2] == "0",
        home: f[3].into(),
        user_data: f[4].into(),
        user_sha: sha(f[5]),
        system_sha: sha(f[6]),
        system_user: f.get(7).copied().unwrap_or_default().into(),
        system_group: f.get(8).copied().unwrap_or_default().into(),
        user_env: parse_env(parts.next().unwrap_or_default()),
        system_env: parse_env(parts.next().unwrap_or_default()),
    })
}

/// What `imesh daemon status` prints.
#[derive(Debug, Deserialize)]
pub(super) struct Status {
    pub(super) cluster: Option<String>,
}

/// Quotes a value for the remote shell.
pub(super) fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Installs or updates imesh on the server over SSH, then returns a client over SSH. Refuses a
/// machine that is already in a cluster before it changes anything.
pub async fn provision(target: &Target, mode: Mode) -> Result<Client> {
    if target.docker {
        return super::docker::provision(target, mode).await;
    }
    let ssh = Ssh::new(&target.dest, target.ssh_key.clone());
    let f = facts(&ssh).await?;
    // An update keeps the kind of install the machine has.
    let system = target.system || f.root || (mode == Mode::Update && f.user_sha.is_none() && f.system_sha.is_some());
    // Two daemons on one machine would each take the other's place for clients there.
    ensure!(
        !system || f.root || f.user_sha.is_none(),
        "{} has a user install of imesh; remove it there with `imesh daemon uninstall` first",
        target.dest
    );
    ensure!(system || f.system_sha.is_none(), "{} has a system install of imesh; pass --system", target.dest);
    ensure!(
        system || (target.run_as.is_none() && target.run_as_group.is_none()),
        "--run-as and --run-as-group are for a system service: add --system"
    );
    for name in target.run_as.iter().chain(&target.run_as_group) {
        ensure!(is_name(name), "bad user or group name {name:?}");
    }
    let sudo = if system && !f.root {
        ssh.run("sudo -n true", b"").await.map_err(|_| {
            anyhow!(
                "user {} needs passwordless sudo on {} for a system service; without --system, imesh installs for {} with no sudo",
                f.user,
                ssh.dest(),
                f.user
            )
        })?;
        "sudo -n "
    } else {
        ""
    };
    let (installed, installed_path, current_env) = if system {
        (f.system_sha.clone(), "/usr/local/bin/imesh".to_string(), f.system_env.clone())
    } else {
        (f.user_sha.clone(), format!("{}/.local/bin/imesh", f.home), f.user_env.clone())
    };
    if mode == Mode::Update {
        ensure!(installed.is_some(), "{} has no imesh installed; use `machine init` or `machine add`", target.dest);
    }
    let (bin, bytes, sha) = target.load_binary(&f.arch)?;

    // Run the install with the installed binary when it is the same, else with an upload to the
    // SSH user's home: /tmp may be mounted noexec.
    let result = if installed.as_deref() == Some(sha.as_str()) {
        install(&ssh, target, mode, &f, system, sudo, &installed_path, &sha, current_env).await
    } else {
        eprintln!("uploading {} ({} MB) to {}", bin.display(), bytes.len() >> 20, ssh.dest());
        let tmp = ssh.run(&format!("mktemp {}/.imesh-XXXXXX", quote(&f.home)), b"").await?.trim().to_string();
        let result = async {
            ssh.run(&format!("cat > {0} && chmod 700 {0}", quote(&tmp)), &bytes).await?;
            install(&ssh, target, mode, &f, system, sudo, &tmp, &sha, current_env).await
        }
        .await;
        let _ = ssh.run(&format!("rm -f {}", quote(&tmp)), b"").await;
        result
    };
    result?;
    // A new login picks up new group memberships.
    ssh.close().await;
    connect::open(&target.ssh()).await.with_context(|| format!("reach the imesh daemon on {}", ssh.dest()))
}

#[allow(clippy::too_many_arguments)]
async fn install(
    ssh: &Ssh,
    target: &Target,
    mode: Mode,
    f: &Facts,
    system: bool,
    sudo: &str,
    exe: &str,
    sha: &str,
    current_env: Env,
) -> Result<()> {
    let exe = quote(exe);
    if mode == Mode::New {
        let socket = if system { SYSTEM_SOCKET.to_string() } else { format!("{}/imesh.sock", f.user_data) };
        let socket = format!(" --socket {}", quote(&socket));
        let out = ssh
            .run(&format!("{sudo}{exe} daemon status{socket}"), b"")
            .await
            .context("ask the daemon there whether it is in a cluster")?;
        let status: Status = serde_json::from_str(out.trim()).with_context(|| format!("daemon status: {out}"))?;
        if let Some(cluster) = status.cluster {
            bail!("{} is already in cluster {cluster}. Remove it first with `imesh machine rm`.", target.dest);
        }
    }
    let env = match mode {
        Mode::New => target.settings.to_env(),
        Mode::Update => {
            let mut env = current_env;
            target.settings.net.clear_replaced_env(&mut env);
            env.extend(target.settings.to_env());
            env
        }
    };
    let mut cmd = format!("{sudo}{exe} daemon install --settings-stdin --sha256 {sha}");
    if system {
        cmd.push_str(" --system");
        // An update keeps the service user unless a flag changes it.
        let run_as = target
            .run_as
            .clone()
            .or_else(|| (mode == Mode::Update && is_name(&f.system_user)).then(|| f.system_user.clone()));
        if let Some(u) = run_as {
            cmd.push_str(&format!(" --run-as {u}"));
        }
        let group = target
            .run_as_group
            .clone()
            .or_else(|| (mode == Mode::Update && is_name(&f.system_group)).then(|| f.system_group.clone()));
        if let Some(g) = group {
            cmd.push_str(&format!(" --run-as-group {g}"));
        }
    } else {
        let service = target.service.to_possible_value().expect("a value").get_name().to_string();
        cmd.push_str(&format!(" --service {service}"));
    }
    eprintln!("installing imesh on {} {}", ssh.dest(), if system { "as a system service" } else { "for the SSH user" });
    let out = ssh.run(&cmd, format_env(&env)?.as_bytes()).await?;
    eprint!("{out}");
    Ok(())
}
