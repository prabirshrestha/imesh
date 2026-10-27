//! The client side of `imesh/ssh/0`: `imesh ssh` and `imesh scp` run the system `ssh` and `scp`
//! with every argument passed through, plus a `ProxyCommand` that reaches the machine over the
//! cluster. `imesh configure ssh` prints or writes the same as a config block, for any SSH tool.

use std::path::PathBuf;

use anyhow::{Context as _, Result, ensure};
use iroh::EndpointId;
use iroh_mesh::api::MachineRow;
use tokio::io::AsyncWriteExt;

use crate::{cli::At, config};

/// A machine the CLI reaches over SSH: its ID and name in the cluster.
struct Target {
    id: EndpointId,
    name: String,
    devtunnel: bool,
}

async fn find(at: &At, machine: &str) -> Result<Target> {
    let c = at.client().await?;
    let rows: Vec<MachineRow> = c.call(None, "cluster.machines", ()).await?;
    let me: Option<iroh_mesh::api::Inspect> = c.call(None, "machine.inspect", ()).await.ok();
    c.close().await;
    let r = crate::machine::pick(&rows, machine)?;
    ensure!(
        me.is_none_or(|m| m.id != r.id),
        "{} is this computer; imesh ssh reaches other machines only. Open a local shell instead",
        r.name
    );
    Ok(Target { id: r.id, name: r.name.clone(), devtunnel: r.info.as_ref().is_some_and(|i| i.tunnel.is_some()) })
}

fn ssh_dir() -> PathBuf {
    config::config_dir().join("ssh")
}

/// The known_hosts file imesh owns. Its lines name machines by their host key alias.
fn known_hosts() -> PathBuf {
    ssh_dir().join("known_hosts")
}

fn alias(id: &EndpointId) -> String {
    format!("imesh-{}", &id.to_string()[..16])
}

/// Writes the host key of `id`. The key is the machine key, so it is known without asking.
fn trust(id: &EndpointId) -> Result<()> {
    let line = format!("{} {}\n", alias(id), crate::ssh_server::host_public_key(id)?);
    let path = known_hosts();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    if text.lines().any(|l| format!("{l}\n") == line) {
        return Ok(());
    }
    let kept: String =
        text.lines().filter(|l| !l.starts_with(&format!("{} ", alias(id)))).map(|l| format!("{l}\n")).collect();
    iroh_mesh::os::create_private_dir(&ssh_dir())?;
    iroh_mesh::os::write_private(&path, format!("{kept}{line}").as_bytes())
}

fn exe() -> String {
    std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "imesh".into())
}

/// Quotes a path for an SSH config value. `%` is a token there.
fn quoted(s: &str) -> String {
    format!("\"{}\"", s.replace('%', "%%"))
}

/// The options that make `ssh` reach `t` through imesh. The proxy names the machine by ID, so a
/// rename does not break a written config.
fn options(t: &Target) -> Vec<(&'static str, String)> {
    vec![
        ("ProxyCommand", format!("{} ssh-proxy {}", quoted(&exe()), t.id)),
        ("HostKeyAlias", alias(&t.id)),
        ("UserKnownHostsFile", quoted(&known_hosts().display().to_string())),
        ("StrictHostKeyChecking", "yes".into()),
        ("PreferredAuthentications", "none,publickey".into()),
    ]
}

/// ssh options that take a value, as `ssh(1)` lists them.
const SSH_VALUE_FLAGS: &str = "BbcDEeFIiJLlmOoPpQRSWw";
/// scp options that take a value.
const SCP_VALUE_FLAGS: &str = "cDFiJloPSX";

/// The positional arguments of `args`: what follows the options, as `getopt` reads them.
fn positionals<'a>(args: &'a [String], value_flags: &str) -> Vec<(usize, &'a str)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            out.extend(args[i + 1..].iter().enumerate().map(|(j, s)| (i + 1 + j, s.as_str())));
            break;
        }
        if let Some(flags) = a.strip_prefix('-').filter(|f| !f.is_empty()) {
            // `-p22` holds its value; `-p 22` takes the next argument.
            if flags.find(|c| value_flags.contains(c)).is_some_and(|pos| pos + 1 == flags.len()) {
                i += 1;
            }
        } else {
            out.push((i, a.as_str()));
        }
        i += 1;
    }
    out
}

/// `imesh ssh [ssh args]`: `ssh` with every argument passed through. The destination is a machine.
pub async fn ssh(at: &At, args: Vec<String>) -> Result<()> {
    let dest = positionals(&args, SSH_VALUE_FLAGS).first().map(|(_, d)| d.to_string()).context("name a machine")?;
    let dest = dest.strip_prefix("ssh://").unwrap_or(&dest).to_string();
    run(at, "ssh", &dest, args).await
}

/// `imesh scp [scp args]`: `scp` where `machine:path` names a cluster machine.
pub async fn scp(at: &At, args: Vec<String>) -> Result<()> {
    let hosts: Vec<String> = positionals(&args, SCP_VALUE_FLAGS)
        .iter()
        .filter_map(|(_, a)| {
            let a = a.strip_prefix("scp://").unwrap_or(a);
            // `C:\x` is a local Windows path, not host `C`.
            let (h, _) = a.split_once(':')?;
            // On Windows `C:\x` is a local path, not host `C`.
            let drive = cfg!(windows) && h.len() == 1;
            (!h.is_empty() && !drive && !h.contains(['/', '\\'])).then(|| h.rsplit('@').next().unwrap_or(h).to_string())
        })
        .collect();
    let machine = hosts.first().cloned().context("name a machine as <machine>:<path>")?;
    ensure!(hosts.iter().all(|h| *h == machine), "imesh scp copies to or from one machine at a time");
    run(at, "scp", &machine, args).await
}

async fn run(at: &At, tool: &str, dest: &str, args: Vec<String>) -> Result<()> {
    let host = dest.rsplit('@').next().unwrap_or(dest);
    let t = find(at, host).await?;
    trust(&t.id)?;
    let mut cmd = std::process::Command::new(tool);
    for (k, v) in options(&t) {
        cmd.arg("-o").arg(format!("{k}={v}"));
    }
    let status = cmd.args(args).status().with_context(|| format!("run {tool}"))?;
    std::process::exit(status.code().unwrap_or(255));
}

/// `imesh ssh-proxy <id>`: run by `ssh` as its ProxyCommand. Joins stdin and stdout to the
/// machine's SSH server over the cluster, with this CLI's key.
pub async fn proxy(at: &At, id: EndpointId) -> Result<()> {
    let c = at.client().await?;
    let stream = c
        .open(id, crate::ssh_server::ALPN)
        .await
        .context("reach the SSH server; does that machine run `imesh up --ssh`?")?;
    let (mut r, mut w) = tokio::io::split(stream);
    let up = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut tokio::io::stdin(), &mut w).await;
        let _ = w.shutdown().await;
    });
    let mut stdout = tokio::io::stdout();
    let _ = tokio::io::copy(&mut r, &mut stdout).await;
    stdout.flush().await?;
    up.abort();
    c.close().await;
    Ok(())
}

/// The Host block for `t`. A machine behind a devtunnel gets tssh's UDP mode.
fn block(t: &Target, host: &str) -> String {
    let mut b = format!("Host {host}\n");
    if t.devtunnel {
        b.push_str("    #!! UdpMode yes\n");
    }
    for (k, v) in options(t) {
        b.push_str(&format!("    {k} {v}\n"));
    }
    b
}

/// `imesh configure ssh [machine...]`: prints Host blocks, or with `write` adds them to
/// `~/.ssh/imesh/config`, which `~/.ssh/config` includes.
pub async fn config(at: &At, machines: Vec<String>, write: bool) -> Result<()> {
    let c = at.client().await?;
    let rows: Vec<MachineRow> = c.call(None, "cluster.machines", ()).await?;
    let me: iroh_mesh::api::Inspect = c.call(None, "machine.inspect", ()).await?;
    c.close().await;
    // Named machines, as `imesh ssh` finds them: a name or an ID prefix. An unknown name or this
    // computer is an error. With no names, every other member.
    let picked: Vec<&MachineRow> = if machines.is_empty() {
        rows.iter().filter(|r| r.id != me.id).collect()
    } else {
        let mut v: Vec<&MachineRow> = Vec::new();
        for m in &machines {
            let r = crate::machine::pick(&rows, m)?;
            ensure!(r.id != me.id, "{} is this computer; SSH config is for other machines", r.name);
            if !v.iter().any(|x| x.id == r.id) {
                v.push(r);
            }
        }
        v
    };
    let mut out = String::new();
    for r in picked {
        let t =
            Target { id: r.id, name: r.name.clone(), devtunnel: r.info.as_ref().is_some_and(|i| i.tunnel.is_some()) };
        trust(&t.id)?;
        if write {
            let a = crate::ssh_config::write_block(t.id, &t.name, |host| block(&t, host))?;
            println!("wrote Host {a} to ~/.ssh/imesh/config");
        } else {
            out.push_str(&block(&t, &t.name));
            out.push('\n');
        }
    }
    print!("{out}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Vec<String> {
        s.split(' ').map(String::from).collect()
    }

    #[test]
    fn finds_the_destination_after_options() {
        let p = |s: &str, f| positionals(&v(s), f).into_iter().map(|(_, a)| a.to_string()).collect::<Vec<_>>();
        assert_eq!(p("-tt -p 22 me@box ls", SSH_VALUE_FLAGS), ["me@box", "ls"]);
        assert_eq!(p("-o BatchMode=yes -p22 box", SSH_VALUE_FLAGS), ["box"]);
        assert_eq!(p("-r -P 22 /tmp/a box:/tmp/b", SCP_VALUE_FLAGS), ["/tmp/a", "box:/tmp/b"]);
    }
}
