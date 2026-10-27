//! `imesh up` and `imesh update`: the daemon on this computer. They use the same install
//! functions as `imesh service install` and the remote installs.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use iroh_mesh::api::Inspect;

use super::{Env, Service, env_file, parse_env, sha256};
use crate::{args::Settings, bundle::Bundle, config::Connection, output::Output, paths};

/// Options of `imesh up` and `imesh update`.
#[derive(Debug, clap::Args)]
pub struct LocalArgs {
    #[command(flatten)]
    pub settings: Settings,
    /// This machine's name. Kept for later starts. In a cluster, it also renames this member.
    #[arg(long, value_parser = crate::args::member_name)]
    pub name: Option<String>,
    /// The bundle to install from: a directory with manifest.json, or its ZIP. Defaults to the
    /// bundle beside this executable, or the one the install kept.
    #[arg(long, env = "IMESH_BUNDLE")]
    pub bundle: Option<PathBuf>,
    /// How a user install keeps the daemon running (Linux).
    #[arg(long, value_enum, default_value_t)]
    pub service: Service,
}

/// The result of `up` and `update`.
#[derive(Debug, Serialize)]
pub struct Local {
    pub id: Option<iroh::EndpointId>,
    pub version: Option<String>,
    pub running: bool,
    pub cluster: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub members: Option<usize>,
    /// What changed in `update`: `installed`, `started`, `settings`, `binary`, `bundle`,
    /// `restarted`. In `up`: `cluster` when it started one, `joined` when it joined one.
    pub changed: Vec<&'static str>,
}

/// The installed user executable, if any.
fn installed() -> Option<PathBuf> {
    let p = crate::paths::daemon_bin();
    p.exists().then_some(p)
}

fn system_installed() -> bool {
    #[cfg(unix)]
    return std::path::Path::new("/usr/local/bin/imesh").exists()
        && std::path::Path::new("/etc/systemd/system/imesh.service").exists();
    #[cfg(windows)]
    return super::windows::system_installed();
}

fn host_triple() -> Result<&'static str> {
    crate::bundle::triple(std::env::consts::OS, std::env::consts::ARCH)
}

/// The daemon's answer, `None` when nothing listens, or an error when it is there but broken.
async fn health() -> Result<Option<Inspect>> {
    let socket = paths::find();
    // A Windows named pipe does not show as a file; connecting is the only test.
    if cfg!(unix) && !socket.exists() {
        return Ok(None);
    }
    let conn = Connection { local: Some(socket.clone()), ..Default::default() };
    match crate::connect::open(&conn).await {
        Ok(c) => {
            let i = c.call::<Inspect>(None, "machine.inspect", ()).await;
            c.close().await;
            Ok(Some(i?))
        }
        Err(e) if is_absent(&e) => Ok(None),
        Err(e) => Err(e).with_context(|| format!("the daemon at {} does not answer", socket.display())),
    }
}

fn is_absent(e: &anyhow::Error) -> bool {
    let t = format!("{e:#}");
    ["Connection refused", "No such file", "cannot find the file", "os error 2"].iter().any(|m| t.contains(m))
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f).await?
}

/// Runs the installer of `exe` with `env`: the exe itself, so a new version installs itself.
async fn run_install(exe: PathBuf, env: &Env, service: Service, zip: Option<PathBuf>) -> Result<()> {
    let text = super::format_env(env)?;
    let service = clap::ValueEnum::to_possible_value(&service).expect("a value").get_name().to_string();
    blocking(move || {
        use std::io::Write;
        let mut cmd = std::process::Command::new(&exe);
        cmd.args(["service", "install", "--settings-stdin", "--service", &service]);
        cmd.env("IMESH_CALLER_PID", std::process::id().to_string());
        if let Some(z) = &zip {
            cmd.arg("--bundle").arg(z);
        }
        // Progress of the installer goes to stderr, so JSON output stays clean.
        let mut child = cmd
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("run {}", exe.display()))?;
        child.stdin.take().expect("piped").write_all(text.as_bytes())?;
        let out = child.wait_with_output()?;
        eprint!("{}", String::from_utf8_lossy(&out.stdout));
        anyhow::ensure!(out.status.success(), "the install failed: {}", out.status);
        Ok(())
    })
    .await
}

fn current_env() -> Env {
    parse_env(&std::fs::read_to_string(env_file()).unwrap_or_default())
}

fn merged(settings: &Settings, name: Option<&str>) -> Env {
    let mut env = current_env();
    settings.clear_replaced_env(&mut env);
    env.extend(settings.to_env());
    if let Some(n) = name {
        env.insert("IMESH_NAME".into(), n.into());
    }
    env
}

async fn finish(changed: Vec<&'static str>, out: &Output) -> Result<()> {
    let i = health().await?;
    let members = match &i {
        Some(i) if i.cluster.is_some() => members().await.ok(),
        _ => None,
    };
    let r = Local {
        id: i.as_ref().map(|i| i.id),
        version: i.as_ref().map(|i| i.info.version.clone()),
        running: i.is_some(),
        cluster: i.as_ref().and_then(|i| i.cluster.clone()),
        members,
        changed,
    };
    out.print(&r, |r| {
        let what = if r.changed.is_empty() { "nothing changed".to_string() } else { r.changed.join(", ") };
        println!(
            "imesh {} running ({what}); machine {}",
            r.version.as_deref().unwrap_or("?"),
            r.id.map(|i| i.to_string()).unwrap_or_default()
        );
        match (&r.cluster, r.members) {
            (Some(c), Some(1)) => println!(
                "cluster {c}, with only this machine. To add one, run `imesh invite <name>` here, or `imesh add user@host`."
            ),
            (Some(c), Some(n)) => println!("cluster {c}, with {n} machines"),
            (Some(c), None) => println!("cluster {c}"),
            (None, _) => println!("in no cluster"),
        }
    })
}

/// The number of members of this machine's cluster.
async fn members() -> Result<usize> {
    let c = crate::connect::open(&Connection::local()).await?;
    let r = c.call::<Vec<iroh_mesh::api::MachineRow>>(None, "cluster.machines", ()).await;
    c.close().await;
    Ok(r?.len())
}

/// `imesh up`: makes sure the daemon on this computer is installed and running.
/// `imesh up`: makes sure the daemon on this computer runs, and is in a cluster. With `join`, it
/// joins the cluster of that pairing code; else it starts a cluster when it is in none.
pub async fn up(mut args: LocalArgs, join: Option<String>, force: bool, out: &Output) -> Result<()> {
    let code = join.as_deref().map(str::parse::<crate::pair::Code>).transpose()?;
    // A code from a cluster in the devtunnel mesh puts this machine in the mesh, unless it has
    // relay settings of its own, saved or passed now.
    if code.as_ref().is_some_and(|c| c.mesh) && !has_relay_settings(&args) {
        eprintln!("the cluster uses the devtunnel mesh, so this machine joins it too (--relay devtunnel)");
        args.settings.net.relays.push(crate::args::Relay::DevTunnel);
    }
    start_daemon(args).await?;
    let i = health().await?.context("the daemon does not answer after it started")?;
    let mut changed = vec![];
    match (code, &i.cluster) {
        (Some(code), _) => {
            if crate::pair::join(&code, force).await? {
                changed.push("joined");
            }
        }
        (None, Some(_)) => {}
        (None, None) => {
            start_cluster().await?;
            changed.push("cluster");
        }
    }
    finish(changed, out).await
}

/// Whether this machine chose its relays: with flags now, or in its saved settings.
fn has_relay_settings(args: &LocalArgs) -> bool {
    let n = &args.settings.net;
    let saved = current_env();
    !n.relays.is_empty()
        || n.devtunnel.is_some()
        || ["IMESH_RELAYS", "IMESH_DEVTUNNEL"].iter().any(|k| saved.contains_key(*k))
}

/// Starts a cluster on this computer's daemon, with this machine as its only member.
async fn start_cluster() -> Result<()> {
    let c = crate::connect::open(&Connection::local()).await?;
    let r = c.call::<serde_json::Value>(None, "cluster.init", serde_json::json!({})).await;
    c.close().await;
    r.map(drop).context("start a cluster on this machine")
}

/// Installs the daemon if needed, applies the settings, and starts it.
async fn start_daemon(args: LocalArgs) -> Result<()> {
    if installed().is_none() && system_installed() {
        if health().await?.is_some() {
            return Ok(());
        }
        bail!(
            "the system daemon here is not running; start it as an administrator (systemctl start imesh, or Start-Service imesh)"
        );
    }
    // A running member takes the new name in its cluster first, so a refused name changes nothing.
    let renamed = match &args.name {
        Some(n) if installed().is_some() => rename_self(n).await?,
        _ => false,
    };
    let mut changed = Vec::new();
    match installed() {
        None => {
            // A fresh install: the executable for this computer from the bundle, else this one.
            let (exe, zip) = match Bundle::find(args.bundle.as_deref()) {
                Ok(b) => (b.artifact(host_triple()?)?.1, Some(b.zip.clone())),
                Err(e) if args.bundle.is_some() => return Err(e),
                Err(_) => (std::env::current_exe()?, None),
            };
            eprintln!("installing the imesh daemon from {}", exe.display());
            let mut env = args.settings.to_env();
            if let Some(n) = &args.name {
                env.insert("IMESH_NAME".into(), n.clone());
            }
            run_install(exe, &env, args.service, zip).await?;
            changed.push("installed");
        }
        Some(exe) => {
            let env = merged(&args.settings, args.name.as_deref());
            if env != current_env() {
                run_install(exe, &env, args.service, None).await?;
                changed.push("settings");
            } else if health().await?.is_none() {
                blocking(start).await?;
                changed.push("started");
            }
        }
    }
    if let Some(n) = args.name.as_deref().filter(|_| !renamed) {
        rename_self(n).await?;
    }
    if !changed.is_empty() {
        eprintln!("daemon: {}", changed.join(", "));
    }
    Ok(())
}

/// Gives this machine the name `name` in its cluster. Returns whether it is in one.
async fn rename_self(name: &str) -> Result<bool> {
    let Some(i) = health().await? else { return Ok(false) };
    if i.cluster.is_none() {
        return Ok(false);
    }
    let conn = Connection { local: Some(paths::find()), ..Default::default() };
    let c = crate::connect::open(&conn).await?;
    let r = c.call::<serde_json::Value>(None, "cluster.rename", serde_json::json!({ "id": i.id, "name": name })).await;
    c.close().await;
    r.map(|_| true).context("rename this machine in its cluster")
}

fn start() -> Result<()> {
    #[cfg(unix)]
    return super::user::ensure_started();
    #[cfg(windows)]
    return super::windows::ensure_started();
}

/// `imesh update`: applies a verified bundle to the installed daemon on this computer.
pub async fn update(args: LocalArgs, out: &Output) -> Result<()> {
    let Some(installed_exe) = installed() else {
        if system_installed() {
            bail!(
                "this computer has a system install; update it with `imesh service install --system` as an administrator"
            );
        }
        bail!("imesh is not installed here; run `imesh up`");
    };
    let bundle = Bundle::find(args.bundle.as_deref())?;
    let (a, exe) = bundle.artifact(host_triple()?)?;
    let same_exe = sha256(&std::fs::read(&installed_exe)?) == a.sha256;
    let kept = std::fs::read_to_string(paths::bundle_dir().join(crate::bundle::ZIP_SHA)).unwrap_or_default();
    let same_bundle = kept.trim() == bundle.zip_sha256;
    let env = merged(&args.settings, args.name.as_deref());
    let same_env = env == current_env();
    let mut changed = Vec::new();
    if same_exe && same_bundle && same_env {
        if health().await?.is_none() {
            blocking(start).await?;
            changed.push("started");
        }
        return finish(changed, out).await;
    }
    if !same_exe {
        changed.push("binary");
    }
    if !same_bundle {
        changed.push("bundle");
    }
    if !same_env {
        changed.push("settings");
    }
    // The kept bundle may be the one we install from; copy the ZIP aside so it can be replaced.
    let zip = if same_bundle {
        None
    } else {
        let tmp = paths::tmp_dir().join(format!("imesh-update-{}.zip", std::process::id()));
        std::fs::copy(&bundle.zip, &tmp)?;
        Some(tmp)
    };
    let result = run_install(exe.clone(), &env, args.service, zip.clone()).await;
    if let Some(z) = zip {
        let _ = std::fs::remove_file(z);
    }
    result?;
    changed.push("restarted");
    finish(changed, out).await
}
