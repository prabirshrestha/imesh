//! Installing on a server over SSH: `imesh add` and `imesh update server`.
//!
//! The CLI uploads the binary and runs `imesh service install` there, with the settings on stdin,
//! so the server installs itself the same way as a local install. For the SSH user by default: no
//! sudo. With `--system`, or for a root login, as a system service; a non-root user then needs
//! passwordless sudo.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::ValueEnum;
use serde::Deserialize;

use iroh_mesh::client::Client;

use super::{Env, Service, format_env_for, is_name, parse_env, sha256};
use crate::{args::Settings, config::Connection, connect, paths::SYSTEM_SOCKET, ssh::Ssh};

/// A computer to install on: over SSH, or through a prepared tunnel.
#[derive(Debug, Clone, clap::Args)]
pub struct Target {
    /// `[ssh://][user@]host[:port]`, an SSH alias, or `devtunnel://<id>:<port>`.
    pub dest: String,
    /// SSH private key. Uses the SSH agent and ~/.ssh/config when omitted.
    #[arg(short = 'i', long = "ssh-key")]
    pub ssh_key: Option<PathBuf>,
    /// SSH user, for a tunnel target.
    #[arg(long)]
    pub ssh_user: Option<String>,
    /// Machine name. Defaults to the remote hostname.
    #[arg(long, value_parser = crate::args::member_name)]
    pub name: Option<String>,
    /// Install a system service instead of one for the SSH user. A root login always does; any
    /// other user then needs passwordless sudo. Linux only.
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
    /// The bundle to install from: a directory with manifest.json, or its ZIP. Defaults to the
    /// bundle beside this executable, or the one an install kept. `--binary` takes precedence.
    #[arg(long, env = "IMESH_BUNDLE")]
    pub bundle: Option<PathBuf>,
    /// Development override: upload this one Linux executable, with no bundle for onward installs.
    #[arg(long)]
    pub binary: Option<PathBuf>,
    /// Do not write an SSH alias for the machine to ~/.ssh/imesh/config.
    #[arg(long)]
    pub no_ssh_config: bool,
    /// Name of the SSH alias to write. Defaults to the machine name.
    #[arg(long)]
    pub ssh_alias: Option<String>,
    /// The parsed destination, set by [`Target::resolve`].
    #[arg(skip)]
    pub resolved: Option<Resolved>,
}

/// Where `dest` points, after parsing.
#[derive(Debug, Clone, PartialEq)]
pub enum Dest {
    /// SSH, with `user@host[:port]` or an alias.
    Ssh(String),
    /// A prepared SSH tunnel.
    Tunnel(iroh_mesh::net::DevTunnel),
}

/// Parses a target. Unknown schemes fail.
pub fn parse_dest(s: &str) -> Result<Dest> {
    if let Some(rest) = s.strip_prefix("ssh://") {
        ensure!(!rest.is_empty(), "ssh:// needs a host");
        return Ok(Dest::Ssh(rest.into()));
    }
    if let Some(rest) = s.strip_prefix("devtunnel://") {
        return Ok(Dest::Tunnel(format!("devtunnel:{rest}").parse()?));
    }
    if let Some((scheme, _)) = s.split_once("://") {
        bail!("unknown target scheme {scheme:?}: use ssh:// or devtunnel://");
    }
    ensure!(!s.is_empty() && !s.starts_with('-'), "bad target {s:?}");
    Ok(Dest::Ssh(s.into()))
}

/// The SSH endpoint an install uses, after resolution.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// `user@host[:port]`, or the SSH alias.
    pub ssh: String,
    pub tunnel: Option<iroh_mesh::net::DevTunnel>,
}

impl Target {
    /// Resolves `dest` to an SSH endpoint.
    pub async fn resolve(&mut self) -> Result<()> {
        let dest = parse_dest(&self.dest)?;
        if matches!(dest, Dest::Tunnel(_)) {
            crate::devtunnel_cli::check_devtunnel().await?;
        }
        let r = match dest {
            Dest::Ssh(s) => Resolved { ssh: s, tunnel: None },
            Dest::Tunnel(t) => Resolved { ssh: self.tunnel_ssh(&t)?, tunnel: Some(t) },
        };
        self.resolved = Some(r);
        Ok(())
    }

    /// The SSH destination behind a tunnel: the user, at loopback and the tunnel port.
    fn tunnel_ssh(&self, t: &iroh_mesh::net::DevTunnel) -> Result<String> {
        let user = self.ssh_user.as_deref().context("a tunnel target needs --ssh-user <user>")?;
        ensure!(is_ssh_user(user), "bad SSH user {user:?}");
        Ok(format!("{user}@127.0.0.1:{}", t.port))
    }

    /// The SSH destination text, for messages.
    pub fn display(&self) -> &str {
        self.resolved.as_ref().map_or(&self.dest, |r| &r.ssh)
    }

    /// The SSH client for this target.
    pub fn ssh_client(&self) -> Ssh {
        let r = self.resolved.as_ref().expect("resolved before install");
        Ssh::new(&r.ssh, self.ssh_key.clone()).with_tunnel(r.tunnel.clone())
    }

    /// The connection that reaches this server's daemon over SSH: on the host, or in its container.
    pub fn ssh(&self) -> Connection {
        let r = self.resolved.as_ref().expect("resolved before install");
        Connection {
            ssh: Some(r.ssh.clone()),
            ssh_key_file: self.ssh_key.clone(),
            ssh_tunnel: r.tunnel.as_ref().map(|t| t.to_string()),
            docker: self.docker.then(|| self.container.docker_name.clone()),
            ..Default::default()
        }
    }

    /// The executable for `triple`: from `--binary`, else the bundle.
    pub(super) fn load_binary(&self, triple: &str) -> Result<Payload> {
        if let Some(b) = &self.binary {
            let bytes = std::fs::read(b).with_context(|| format!("read {}", b.display()))?;
            crate::bundle::check_format(triple, &bytes)?;
            let sha = sha256(&bytes);
            return Ok(Payload { label: b.display().to_string(), bytes, sha, zip: None });
        }
        let bundle = crate::bundle::Bundle::find(self.bundle.as_deref())?;
        let (a, path) = bundle.artifact(triple)?;
        let bytes = std::fs::read(&path)?;
        ensure!(sha256(&bytes) == a.sha256, "checksum mismatch: {}", path.display());
        Ok(Payload { label: path.display().to_string(), bytes, sha: a.sha256.clone(), zip: Some(bundle) })
    }
}

/// An executable to install and, from a bundle, the whole bundle to keep there.
pub(super) struct Payload {
    pub label: String,
    pub bytes: Vec<u8>,
    pub sha: String,
    pub zip: Option<crate::bundle::Bundle>,
}

fn is_ssh_user(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '\\'))
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    /// A new cluster: the server must be in no cluster. A server in a cluster is refused before
    /// anything on it changes.
    NewCluster,
    /// A new member: the server must be in no cluster, or alone in its own empty cluster, which
    /// it leaves to join.
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
    /// Checksums of the user install `<user_data>/bin/imesh` and `/usr/local/bin/imesh`.
    user_sha: Option<String>,
    system_sha: Option<String>,
    /// The user and group the system service runs as, if there is one.
    system_user: String,
    system_group: String,
    user_env: Env,
    system_env: Env,
    /// SHA-256 of the bundle ZIP the user install keeps.
    kept_bundle: Option<String>,
}

const FACTS_SH: &str = r#"uname -m; id -un; id -u; printf '%s\n' "$HOME" "${XDG_DATA_HOME:-$HOME/.local/share}/imesh"
for b in "${XDG_DATA_HOME:-$HOME/.local/share}/imesh/bin/imesh" /usr/local/bin/imesh; do if [ -x "$b" ]; then sha256sum "$b" | cut -d' ' -f1; else echo none; fi; done
systemctl show -p User --value imesh 2>/dev/null || echo
systemctl show -p Group --value imesh 2>/dev/null || echo
cat "${XDG_DATA_HOME:-$HOME/.local/share}/imesh/bundle/imesh.zip.sha256" 2>/dev/null || echo none
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
        kept_bundle: f.get(9).copied().and_then(sha),
        user_env: parse_env(parts.next().unwrap_or_default()),
        system_env: parse_env(parts.next().unwrap_or_default()),
    })
}

/// What `imesh service status` prints.
#[derive(Debug, Deserialize)]
pub(super) struct Status {
    pub(super) cluster: Option<String>,
    /// Alone in its own cluster, with no `kv` rows and no app rows.
    #[serde(default)]
    pub(super) alone_and_empty: bool,
}

impl Status {
    /// Refuses a new install on a machine whose cluster holds something.
    pub(super) fn ensure_new(&self, target: &Target, mode: Mode) -> Result<()> {
        match &self.cluster {
            Some(c) if mode == Mode::NewCluster => {
                bail!("{} is already in cluster {c}; run `imesh logout` there first", target.display())
            }
            Some(c) if !self.alone_and_empty => bail!(
                "{} is in cluster {c} with other members or data; run `imesh logout` there first",
                target.display()
            ),
            _ => Ok(()),
        }
    }
}

/// `unix` or `windows`. Windows OpenSSH runs cmd.exe or PowerShell, which have no `uname`.
pub(super) async fn probe_os(ssh: &Ssh) -> Result<&'static str> {
    match ssh.run("uname -s", b"").await {
        Ok(out) if !out.trim().is_empty() && !["MINGW", "MSYS", "CYGWIN"].iter().any(|p| out.trim().starts_with(p)) => {
            return Ok("unix");
        }
        Ok(_) => {}
        Err(e) if format!("{e:#}").contains("Permission denied") || format!("{e:#}").contains("timed out") => {
            return Err(e);
        }
        Err(_) => {}
    }
    let out = ssh
        .powershell("[Environment]::OSVersion.Platform", b"")
        .await
        .with_context(|| format!("{} has neither `uname` nor PowerShell; is SSH working there?", ssh.dest()))?;
    ensure!(out.contains("Win32NT"), "{}: unknown remote OS: {}", ssh.dest(), out.trim());
    Ok("windows")
}

/// Quotes a value for the remote shell.
pub(super) fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Installs or updates imesh on the server over SSH, then returns a client over SSH. Refuses a
/// machine that is already in a cluster before it changes anything.
pub async fn provision(target: &Target, mode: Mode) -> Result<Client> {
    let ssh = target.ssh_client();
    if probe_os(&ssh).await? == "windows" {
        return super::remote_windows::provision(target, &ssh, mode).await;
    }
    if target.docker {
        return super::docker::provision(target, mode).await;
    }
    let f = facts(&ssh).await?;
    // An update keeps the kind of install the machine has.
    let system = target.system || f.root || (mode == Mode::Update && f.user_sha.is_none() && f.system_sha.is_some());
    // Two daemons on one machine would each take the other's place for clients there.
    ensure!(
        !system || f.root || f.user_sha.is_none(),
        "{} has a user install of imesh; remove it there with `imesh service uninstall` first",
        target.display()
    );
    ensure!(system || f.system_sha.is_none(), "{} has a system install of imesh; pass --system", target.display());
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
        (f.user_sha.clone(), format!("{}/bin/imesh", f.user_data), f.user_env.clone())
    };
    if mode == Mode::Update {
        ensure!(installed.is_some(), "{} has no imesh installed; use `imesh add`", target.display());
    }
    let p = target.load_binary(crate::bundle::triple("linux", &f.arch)?)?;
    // The whole bundle goes along, unless the machine keeps this one already.
    let zip = p.zip.as_ref().filter(|b| system || f.kept_bundle.as_deref() != Some(b.zip_sha256.as_str()));

    // Uploads go to a private directory in the SSH user's home: /tmp may be mounted noexec.
    let tmp = ssh.run(&format!("mktemp -d {}/.imesh-XXXXXX", quote(&f.home)), b"").await?.trim().to_string();
    let result = async {
        let exe = if installed.as_deref() == Some(p.sha.as_str()) {
            installed_path.clone()
        } else {
            eprintln!("uploading {} ({} MB) to {}", p.label, p.bytes.len() >> 20, ssh.dest());
            let exe = format!("{tmp}/imesh");
            ssh.run(&format!("cat > {0} && chmod 700 {0}", quote(&exe)), &p.bytes).await?;
            exe
        };
        let zip_path = match zip {
            Some(b) => {
                let bytes = std::fs::read(&b.zip)?;
                eprintln!("uploading the bundle {} ({} MB) to {}", b.zip.display(), bytes.len() >> 20, ssh.dest());
                let z = format!("{tmp}/imesh.zip");
                ssh.run(&format!("cat > {}", quote(&z)), &bytes).await?;
                Some(z)
            }
            None => None,
        };
        install(&ssh, target, mode, &f, system, sudo, &exe, &p.sha, zip_path.as_deref(), current_env).await
    }
    .await;
    let _ = ssh.run(&format!("rm -rf {}", quote(&tmp)), b"").await;
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
    zip: Option<&str>,
    current_env: Env,
) -> Result<()> {
    let exe = quote(exe);
    if mode != Mode::Update {
        let socket = if system { SYSTEM_SOCKET.to_string() } else { format!("{}/imesh.sock", f.user_data) };
        let socket = format!(" --socket {}", quote(&socket));
        let out = ssh
            .run(&format!("{sudo}{exe} service status{socket}"), b"")
            .await
            .context("ask the daemon there whether it is in a cluster")?;
        let status: Status = serde_json::from_str(out.trim()).with_context(|| format!("service status: {out}"))?;
        status.ensure_new(target, mode)?;
    }
    let env = match mode {
        Mode::New | Mode::NewCluster => target.settings.to_env(),
        Mode::Update => {
            let mut env = current_env;
            target.settings.net.clear_replaced_env(&mut env);
            env.extend(target.settings.to_env());
            env
        }
    };
    let mut cmd = format!("{sudo}{exe} service install --settings-stdin --sha256 {sha}");
    if let Some(z) = zip {
        cmd.push_str(&format!(" --bundle {}", quote(z)));
    }
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
    let out = ssh.run(&cmd, format_env_for(&env, false)?.as_bytes()).await?;
    eprint!("{out}");
    Ok(())
}
