//! SSH aliases for installed machines, in `~/.ssh/imesh/config`, which imesh owns. `~/.ssh/config`
//! gets one `Include` line at its start. User entries are never changed, and host keys stay checked.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use iroh::EndpointId;

use crate::config::Connection;

const INCLUDE_LINE: &str = "Include ~/.ssh/imesh/config";
const MARK: &str = "# imesh machine ";

/// `~/.ssh`, or `$IMESH_SSH_DIR` for tests.
fn ssh_dir() -> Result<PathBuf> {
    if let Some(d) = std::env::var_os("IMESH_SSH_DIR") {
        return Ok(d.into());
    }
    Ok(dirs::home_dir().context("no home directory")?.join(".ssh"))
}

/// One managed entry.
#[derive(Debug, Clone, PartialEq)]
struct Entry {
    machine: EndpointId,
    alias: String,
    body: String,
}

fn parse(text: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut cur: Option<Entry> = None;
    for line in text.lines() {
        if let Some(id) = line.strip_prefix(MARK) {
            out.extend(cur.take());
            if let Ok(machine) = id.trim().parse() {
                cur = Some(Entry { machine, alias: String::new(), body: String::new() });
            }
            continue;
        }
        if let Some(e) = &mut cur {
            if let Some(a) = line.strip_prefix("Host ") {
                e.alias = a.trim().to_string();
            }
            e.body.push_str(line);
            e.body.push('\n');
        }
    }
    out.extend(cur);
    out
}

fn format(entries: &[Entry]) -> String {
    let mut s = String::from("# Managed by imesh. Edits here are replaced.\n\n");
    for e in entries {
        s.push_str(&format!("{MARK}{}\n{}\n", e.machine, e.body.trim_end()));
        s.push('\n');
    }
    s
}

pub fn valid_alias(a: &str) -> bool {
    !a.is_empty() && !a.starts_with('-') && a.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Writes the entry body for a connection.
fn body(alias: &str, c: &Connection) -> Result<String> {
    let dest = c.ssh.as_deref().context("not an SSH connection")?;
    let (user, rest) = match dest.rsplit_once('@') {
        Some((u, r)) => (Some(u), r),
        None => (None, dest),
    };
    let (host, port) = match rest.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') && p.parse::<u16>().is_ok() => (h, Some(p)),
        _ => (rest.trim_start_matches('[').trim_end_matches(']'), None),
    };
    for v in [Some(host), user, port].into_iter().flatten() {
        ensure!(!v.contains([' ', '\t', '"', '\n', '%']), "cannot write {v:?} to an SSH config");
    }
    let mut b = format!("Host {alias}\n    HostName {host}\n");
    if let Some(u) = user {
        b.push_str(&format!("    User {u}\n"));
    }
    if let Some(p) = port {
        b.push_str(&format!("    Port {p}\n"));
    }
    if let Some(k) = &c.ssh_key_file {
        let k = k.display().to_string();
        ensure!(!k.contains(['"', '\n']), "cannot write the key path to an SSH config");
        b.push_str(&format!("    IdentityFile \"{}\"\n", k.replace('%', "%%")));
    }
    if let Some(t) = &c.ssh_tunnel {
        let t: iroh_mesh::net::DevTunnel = t.parse()?;
        // tssh reads this comment and uses UDP mode; plain ssh ignores it.
        b.push_str("    #!! UdpMode yes\n");
        b.push_str(&format!("    HostKeyAlias {}\n", crate::ssh::tunnel_host_alias(&t)));
        b.push_str(&format!("    ProxyCommand {}\n", crate::ssh::proxy_command(&t)));
    }
    Ok(b)
}

/// Adds or updates the entry of `machine` with a body that `make` writes for the chosen alias.
pub fn write_block(machine: EndpointId, name: &str, make: impl Fn(&str) -> String) -> Result<String> {
    let dir = ssh_dir()?;
    let _lock = crate::local_state::lock(&dir.join("imesh/config.lock"))?;
    write_with(&dir, machine, name, None, |a| Ok(make(a)), user_defines)
}

/// Detect an existing user Host block, including blocks from Include files. OpenSSH's
/// verbose parse output identifies applied blocks even if they only set User or IdentityFile.
fn user_defines(alias: &str) -> bool {
    user_defines_in(&ssh_dir().unwrap_or_default(), alias).unwrap_or(true)
}

fn user_defines_in(dir: &Path, alias: &str) -> Result<bool> {
    let text = std::fs::read_to_string(dir.join("config")).unwrap_or_default();
    let line = include_line(dir);
    let kept: String = text.lines().filter(|l| l.trim() != line).map(|l| format!("{l}\n")).collect();
    let tmp = dir.join("imesh").join(format!("check-{}", hex::encode(&iroh::SecretKey::generate().to_bytes()[..8])));
    iroh_mesh::os::write_private(&tmp, kept.as_bytes())?;
    let out = std::process::Command::new("ssh").args(["-v", "-G", "-F"]).arg(&tmp).arg(alias).output();
    let _ = std::fs::remove_file(&tmp);
    let out = out.context("check the existing SSH config")?;
    ensure!(out.status.success(), "cannot read the existing SSH config");
    let trace = String::from_utf8_lossy(&out.stderr);
    Ok(trace
        .lines()
        .any(|line| line.split_once("Applying options for ").is_some_and(|(_, patterns)| patterns.trim() != "*")))
}

/// Adds or updates the alias for `machine`. Returns the alias written. A name another machine or
/// the user has gets a short machine ID suffix.
pub fn write(machine: EndpointId, name: &str, alias: Option<&str>, c: &Connection) -> Result<String> {
    let dir = ssh_dir()?;
    let _lock = crate::local_state::lock(&dir.join("imesh/config.lock"))?;
    write_in(&dir, machine, name, alias, c, user_defines)
}

fn write_in(
    dir: &Path,
    machine: EndpointId,
    name: &str,
    alias: Option<&str>,
    c: &Connection,
    taken_by_user: impl Fn(&str) -> bool,
) -> Result<String> {
    write_with(dir, machine, name, alias, |a| body(a, c), taken_by_user)
}

fn write_with(
    dir: &Path,
    machine: EndpointId,
    name: &str,
    alias: Option<&str>,
    make: impl Fn(&str) -> Result<String>,
    taken_by_user: impl Fn(&str) -> bool,
) -> Result<String> {
    let path = dir.join("imesh").join("config");
    let mut entries = parse(&std::fs::read_to_string(&path).unwrap_or_default());
    let mine = entries.iter().find(|e| e.machine == machine).map(|e| e.alias.clone());
    let base = alias.unwrap_or(name).to_ascii_lowercase();
    ensure!(valid_alias(&base), "bad SSH alias {base:?}");
    let taken = |a: &str| entries.iter().any(|e| e.machine != machine && e.alias == a) || taken_by_user(a);
    let chosen = if alias.is_none() && mine.as_deref().is_some_and(|m| m.starts_with(&base) && !taken(m)) {
        mine.clone().expect("checked")
    } else if !taken(&base) {
        base.clone()
    } else {
        ensure!(alias.is_none(), "the SSH alias {base:?} is taken");
        let id = machine.to_string();
        let a = format!("{base}-{}", &id[..8]);
        ensure!(!taken(&a), "the SSH alias {a:?} is taken");
        a
    };
    let entry = Entry { machine, alias: chosen.clone(), body: make(&chosen)? };
    entries.retain(|e| e.machine != machine);
    entries.push(entry);
    iroh_mesh::os::write_private(&path, format(&entries).as_bytes())?;
    include(dir)?;
    Ok(chosen)
}

/// The Include line: `~/.ssh/imesh/config`, or the absolute path under `$IMESH_SSH_DIR`.
fn include_line(dir: &Path) -> String {
    match dirs::home_dir() {
        Some(h) if dir == h.join(".ssh") => INCLUDE_LINE.into(),
        _ => format!("Include {}", dir.join("imesh").join("config").display()),
    }
}

/// Adds the Include line at the start of `~/.ssh/config`, outside any Host block, once.
fn include(dir: &Path) -> Result<()> {
    let path = dir.join("config");
    let text = match std::fs::read(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let line = include_line(dir);
    if String::from_utf8_lossy(&text).lines().any(|l| l.trim() == line) {
        return Ok(());
    }
    let mut new = format!("{line}\n\n").into_bytes();
    new.extend_from_slice(&text);
    iroh_mesh::os::write_private(&path, &new)
}

/// Removes the alias of `machine`. Returns it, if there was one.
pub fn remove(machine: EndpointId) -> Result<Option<String>> {
    let dir = ssh_dir()?;
    let _lock = crate::local_state::lock(&dir.join("imesh/config.lock"))?;
    remove_in(&dir, machine)
}

fn remove_in(dir: &Path, machine: EndpointId) -> Result<Option<String>> {
    let path = dir.join("imesh/config");
    let Ok(text) = std::fs::read_to_string(&path) else { return Ok(None) };
    let mut entries = parse(&text);
    let alias = entries.iter().find(|e| e.machine == machine).map(|e| e.alias.clone());
    if alias.is_some() {
        entries.retain(|e| e.machine != machine);
        iroh_mesh::os::write_private(&path, format(&entries).as_bytes())?;
    }
    Ok(alias)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_only_aliases_and_included_aliases_conflict() {
        let dir = std::env::temp_dir().join(format!("imesh-sshconflict-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let child = dir.join("extra");
        std::fs::write(&child, "Host included\n  IdentityFile /some/key\n").unwrap();
        std::fs::write(
            dir.join("config"),
            format!("Include {}\nHost local\n  User alice\nHost *\n  ServerAliveInterval 30\n", child.display()),
        )
        .unwrap();
        assert!(user_defines_in(&dir, "local").unwrap());
        assert!(user_defines_in(&dir, "included").unwrap());
        assert!(!user_defines_in(&dir, "fresh").unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn writes_updates_and_removes() {
        let dir = std::env::temp_dir().join(format!("imesh-sshcfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config"), "Host mine\n    HostName example\n").unwrap();
        let a = iroh::SecretKey::generate().public();
        let b = iroh::SecretKey::generate().public();
        let conn = |s: &str| Connection {
            ssh: Some(s.into()),
            ssh_tunnel: Some("devtunnel:box.use:2222".into()),
            ..Default::default()
        };
        let none = |_: &str| false;
        assert_eq!(write_in(&dir, a, "box", None, &conn("alice@127.0.0.1:2222"), none).unwrap(), "box");
        assert_eq!(write_in(&dir, a, "box", None, &conn("alice@127.0.0.1:2222"), none).unwrap(), "box");
        let other = write_in(&dir, b, "box", None, &conn("bob@h"), none).unwrap();
        assert!(other.starts_with("box-"));
        let renamed = write_in(&dir, b, "x", None, &conn("bob@h"), |a: &str| a == "x").unwrap();
        assert!(renamed.starts_with("x-"));
        let cfg = std::fs::read_to_string(dir.join("config")).unwrap();
        let line = include_line(&dir);
        assert!(cfg.starts_with(&line) && cfg.ends_with("Host mine\n    HostName example\n"));
        assert_eq!(cfg.matches(&line).count(), 1);
        let ours = std::fs::read_to_string(dir.join("imesh/config")).unwrap();
        assert!(ours.contains("HostKeyAlias imesh-tunnel-box.use-2222"));
        assert!(ours.contains("    #!! UdpMode yes\n"));
        assert!(ours.contains("ssh-tunnel-proxy devtunnel:box.use:2222"));
        assert_eq!(parse(&ours).len(), 2);
        assert!(write_in(&dir, a, "bad name", None, &conn("h"), none).is_err());
        assert_eq!(remove_in(&dir, a).unwrap().as_deref(), Some("box"));
        let left = std::fs::read_to_string(dir.join("imesh/config")).unwrap();
        assert!(!left.contains("Host box\n") && left.contains(&format!("Host {renamed}\n")));
        assert_eq!(remove_in(&dir, a).unwrap(), None);
    }
}
