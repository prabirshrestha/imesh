//! Installing the daemon: over SSH for `machine init/add/update`, and locally for
//! `imesh daemon install`.

use std::{collections::BTreeMap, path::PathBuf};

use anyhow::{Context, Result, bail, ensure};

use crate::{api::Inspect, client::Client, client::config::Connection, client::ssh::Ssh, net};

/// The Linux install script. `machine init/add/update` run it over SSH; `daemon install` runs it
/// locally.
const INSTALL_SH: &str = include_str!("install.sh");

/// Daemon settings that an install writes to the daemon's env file.
#[derive(Debug, Clone, clap::Args)]
pub struct Opts {
    /// Labels for the machine, as key=value. Repeat for more.
    #[arg(long = "label", value_parser = crate::daemon::parse_label)]
    pub labels: Vec<(String, String)>,
    #[command(flatten)]
    pub relay: net::Opts,
}

impl Opts {
    /// The settings as environment variables for the daemon.
    pub fn env(&self) -> Env {
        let mut env = self.relay.to_env();
        if !self.labels.is_empty() {
            let labels: Vec<_> = self.labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
            env.insert("IMESH_LABELS".into(), labels.join(","));
        }
        env
    }
}

/// Daemon environment variables. Stored as `VAR=value` lines.
type Env = BTreeMap<String, String>;

fn parse_env(text: &str) -> Env {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| l.trim().split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
        .collect()
}

fn format_env(env: &Env) -> String {
    env.iter().map(|(k, v)| format!("{k}={v}\n")).collect()
}

/// The install script with the settings in front. They go base64-encoded, so no value can break
/// out of the script.
fn script(env: &Env) -> String {
    format!("IMESH_ENV_B64={}\n{INSTALL_SH}", data_encoding::BASE64.encode(format_env(env).as_bytes()))
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(<sha2::Sha256 as sha2::Digest>::digest(bytes))
}

/// Where Windows keeps the daemon settings. The daemon reads it at start.
#[cfg(windows)]
pub fn env_file() -> PathBuf {
    crate::net::local::default_data_dir().join("env")
}

/// Loads the env file into the process environment, without overriding what is set.
#[cfg(windows)]
pub fn load_env_file(path: &std::path::Path) {
    let Ok(text) = std::fs::read_to_string(path) else { return };
    for (k, v) in parse_env(&text) {
        if std::env::var_os(&k).is_none() {
            // SAFETY: called before the runtime starts any other thread.
            unsafe { std::env::set_var(k, v) };
        }
    }
}

/// `imesh daemon install`: installs the running binary as this machine's daemon.
pub fn run(opts: Opts) -> Result<()> {
    let env = opts.env();
    let exe = std::env::current_exe().context("find the running binary")?;
    platform(exe, env)
}

#[cfg(windows)]
fn platform(exe: PathBuf, env: Env) -> Result<()> {
    let dir = crate::net::local::default_data_dir();
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin)?;
    let target = bin.join("imesh.exe");
    // Stop the running daemon first, even when the binary does not change, so new settings apply.
    // Only the task and the process started from the installed path are stopped.
    {
        let _ = powershell(&format!(
            "Stop-ScheduledTask -TaskName imesh -ErrorAction SilentlyContinue; \
             Get-Process imesh -ErrorAction SilentlyContinue | Where-Object {{ $_.Path -eq '{}' }} | Stop-Process -Force; \
             Start-Sleep -Milliseconds 500",
            target.display()
        ));
    }
    if exe != target {
        let mut tries = 0;
        while let Err(e) = std::fs::copy(&exe, &target) {
            tries += 1;
            if tries > 20 {
                return Err(e).with_context(|| format!("copy to {}", target.display()));
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }
    crate::client::config::write_private(&env_file(), format_env(&env).as_bytes())?;
    let log = dir.join("imesh.log");
    let script = format!(
        r#"$ErrorActionPreference = 'Stop'
$exe = '{exe}'
$log = '{log}'
$a = New-ScheduledTaskAction -Execute 'conhost.exe' -Argument "--headless `"$exe`" daemon --log-file `"$log`""
$t = New-ScheduledTaskTrigger -AtLogOn -User "$env:USERDOMAIN\$env:USERNAME"
$s = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 9999 -RestartInterval (New-TimeSpan -Minutes 1) -MultipleInstances IgnoreNew
try {{
  $p = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -LogonType S4U -RunLevel Limited
  Register-ScheduledTask -TaskName imesh -Action $a -Trigger $t -Settings $s -Principal $p -Force | Out-Null
  'registered imesh task (runs without a logon session)'
}} catch {{
  $p = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -LogonType Interactive -RunLevel Limited
  Register-ScheduledTask -TaskName imesh -Action $a -Trigger $t -Settings $s -Principal $p -Force | Out-Null
  'registered imesh task (runs while the user is logged on)'
}}
Start-ScheduledTask -TaskName imesh
"#,
        exe = target.display(),
        log = log.display()
    );
    print!("{}", powershell(&script)?);
    wait_for_pipe()
}

#[cfg(windows)]
fn powershell(script: &str) -> Result<String> {
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .context("run powershell")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(windows)]
fn wait_for_pipe() -> Result<()> {
    for _ in 0..100 {
        if std::fs::metadata(crate::net::local::default_path()).is_ok() {
            println!("imesh daemon is running");
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    bail!("the daemon did not start; see {}", crate::net::local::default_data_dir().join("imesh.log").display())
}

/// Runs the install script here, with this binary. Needs root.
#[cfg(unix)]
fn platform(exe: PathBuf, env: Env) -> Result<()> {
    use std::io::Write;
    let bytes = std::fs::read(&exe)?;
    let sha = sha256(&bytes);
    // A private directory, so no other user can swap the file between the check and the install.
    let dir = tempfile::tempdir()?;
    let tmp = dir.path().join("imesh");
    std::fs::write(&tmp, &bytes)?;
    let user = std::env::var("SUDO_USER").or_else(|_| std::env::var("USER")).unwrap_or_else(|_| "root".into());
    let run_user = if user == "root" { "imesh".to_string() } else { user.clone() };
    let script = script(&env);
    let mut child = std::process::Command::new("sh")
        .arg("-s")
        .env("IMESH_SRC", &tmp)
        .env("IMESH_SHA256", sha)
        .env("IMESH_USER", user)
        .env("IMESH_RUN_USER", run_user)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("run sh")?;
    child.stdin.take().unwrap().write_all(script.as_bytes())?;
    if !child.wait()?.success() {
        bail!("install failed; run it with sudo");
    }
    Ok(())
}

/// Stops the daemon and removes the service and the binary. With `purge`, also the key and state.
pub fn uninstall(purge: bool) -> Result<()> {
    #[cfg(unix)]
    {
        let data = if purge { " /var/lib/imesh /etc/imesh" } else { "" };
        let script = format!(
            "systemctl disable --now imesh.service 2>/dev/null || true\n\
             rm -f /etc/systemd/system/imesh.service /usr/local/bin/imesh\n\
             systemctl daemon-reload\n\
             rm -rf{data}\n"
        );
        let status = std::process::Command::new("sh").arg("-c").arg(script).status()?;
        anyhow::ensure!(status.success(), "uninstall failed; run it with sudo");
    }
    #[cfg(windows)]
    {
        let dir = crate::net::local::default_data_dir();
        let bin = dir.join("bin").join("imesh.exe");
        let _ = powershell(&format!(
            "Stop-ScheduledTask -TaskName imesh -ErrorAction SilentlyContinue; \
             Unregister-ScheduledTask -TaskName imesh -Confirm:$false -ErrorAction SilentlyContinue; \
             Get-Process imesh -ErrorAction SilentlyContinue | Where-Object {{ $_.Path -eq '{}' }} | Stop-Process -Force",
            bin.display()
        ));
        std::thread::sleep(std::time::Duration::from_millis(500));
        let _ = std::fs::remove_file(&bin);
        if purge {
            std::fs::remove_dir_all(&dir).ok();
        }
    }
    println!("imesh daemon removed{}", if purge { " with its data" } else { "; data kept" });
    Ok(())
}

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
    /// Run the daemon as this user. Defaults to the SSH user, or to a system user `imesh` when the
    /// SSH user is root. Created as a system user if missing.
    #[arg(long)]
    pub run_as: Option<String>,
    /// Run the daemon with this group instead of the user's primary group.
    #[arg(long)]
    pub run_as_group: Option<String>,
    #[command(flatten)]
    pub opts: Opts,
    /// Linux imesh binary to install. Defaults to `$IMESH_BINARY_<ARCH>`, for example `IMESH_BINARY_AARCH64`.
    #[arg(long)]
    pub binary: Option<PathBuf>,
}

impl Target {
    /// The connection that reaches this server's daemon over SSH.
    pub fn ssh(&self) -> Connection {
        Connection { ssh: Some(self.dest.clone()), ssh_key_file: self.ssh_key.clone(), ..Default::default() }
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
    installed: Option<String>,
    running: bool,
    run_user: String,
    env: Env,
}

const FACTS_SH: &str = r#"uname -m; id -un; id -u; (sha256sum /usr/local/bin/imesh 2>/dev/null || echo none) | cut -d' ' -f1
test -S /run/imesh/imesh.sock && echo running || echo stopped
systemctl show -p User --value imesh 2>/dev/null || echo
echo ---
if [ "$(id -u)" = 0 ]; then cat /etc/imesh/env; else sudo -n cat /etc/imesh/env; fi 2>/dev/null || true"#;

async fn facts(ssh: &Ssh) -> Result<Facts> {
    let out = ssh.script(FACTS_SH).await?;
    let (head, env) = out.split_once("---\n").unwrap_or((&out, ""));
    let f: Vec<&str> = head.lines().map(str::trim).collect();
    ensure!(f.len() >= 5, "unexpected output from {}: {out}", ssh.dest());
    Ok(Facts {
        arch: f[0].into(),
        user: f[1].into(),
        root: f[2] == "0",
        installed: (f[3] != "none").then(|| f[3].into()),
        running: f[4] == "running",
        run_user: f.get(5).copied().unwrap_or_default().into(),
        env: parse_env(env),
    })
}

fn is_name(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

/// Installs or updates imesh on the server over SSH, then returns a client over SSH. Changes
/// nothing when the binary, the settings, and the user are already as wanted.
pub async fn provision(target: &Target, mode: Mode) -> Result<Client> {
    let ssh = Ssh::new(&target.dest, target.ssh_key.clone());
    let f = facts(&ssh).await?;
    let sudo = if f.root {
        ""
    } else {
        ssh.run("sudo -n true", b"").await.map_err(|_| {
            anyhow::anyhow!(
                "user {} needs passwordless sudo on {} to install imesh (NOPASSWD in /etc/sudoers.d)",
                f.user,
                ssh.dest()
            )
        })?;
        "sudo -n "
    };
    match mode {
        // Refuse before changing anything on a machine that is already in a cluster.
        Mode::New if f.running => {
            if let Ok(c) = Client::open(&target.ssh()).await
                && let Some(cluster) = c.call::<Inspect>(None, "machine.inspect", ()).await?.cluster
            {
                bail!("{} is already in cluster {cluster}. Remove it first with `imesh machine rm`.", target.dest);
            }
        }
        Mode::Update => ensure!(
            f.installed.is_some(),
            "{} has no imesh installed; use `machine init` or `machine add`",
            target.dest
        ),
        Mode::New => {}
    }
    let arch_var = format!("IMESH_BINARY_{}", f.arch.to_uppercase());
    let bin = match &target.binary {
        Some(b) => b.clone(),
        None => std::env::var_os(&arch_var)
            .map(PathBuf::from)
            .with_context(|| format!("no imesh binary for linux/{}: pass --binary or set {arch_var}", f.arch))?,
    };
    let bytes = std::fs::read(&bin).with_context(|| format!("read {}", bin.display()))?;
    let sha = sha256(&bytes);
    let (env, run_user) = match mode {
        Mode::New => (
            target.opts.env(),
            target.run_as.clone().unwrap_or_else(|| if f.root { "imesh".into() } else { f.user.clone() }),
        ),
        Mode::Update => {
            let mut env = f.env.clone();
            env.extend(target.opts.env());
            let user = target.run_as.clone().or_else(|| (!f.run_user.is_empty()).then(|| f.run_user.clone()));
            (env, user.unwrap_or_else(|| f.user.clone()))
        }
    };
    ensure!(is_name(&run_user), "bad user name {run_user:?}");
    if let Some(g) = &target.run_as_group {
        ensure!(is_name(g), "bad group name {g:?}");
    }
    let same = f.installed.as_deref() == Some(&sha) && f.running && f.env == env && f.run_user == run_user;
    if !same || target.run_as_group.is_some() {
        let src = if f.installed.as_deref() == Some(&sha) {
            String::new()
        } else {
            eprintln!("uploading {} ({} MB) to {}", bin.display(), bytes.len() >> 20, ssh.dest());
            let tmp = ssh.run("mktemp /tmp/imesh.XXXXXX", b"").await?.trim().to_string();
            ssh.run(&format!("cat > {tmp}"), &bytes).await?;
            tmp
        };
        eprintln!("installing imesh daemon on {}", ssh.dest());
        let group = target.run_as_group.as_deref().map(|g| format!(" IMESH_RUN_GROUP={g}")).unwrap_or_default();
        let cmd = format!(
            "{sudo}env IMESH_SRC={src} IMESH_SHA256={sha} IMESH_USER={} IMESH_RUN_USER={run_user}{group} sh -s",
            f.user
        );
        ssh.run(&cmd, script(&env).as_bytes()).await?;
        // A new login picks up new group memberships.
        ssh.close().await;
    }
    Client::open(&target.ssh()).await.with_context(|| format!("reach the imesh daemon on {}", ssh.dest()))
}
