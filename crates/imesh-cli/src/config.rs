//! Client config: `~/.config/imesh/config.toml` plus the CLI identity key.

use std::{collections::BTreeMap, path::PathBuf};

use anyhow::{Context as _, Result, bail};
use iroh::{EndpointId, RelayUrl, SecretKey};
use serde::{Deserialize, Serialize};

pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("IMESH_CONFIG_DIR") {
        return dir.into();
    }
    // `$XDG_CONFIG_HOME`, else `~/.config`, on every platform, as the install over SSH expects.
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| dirs::home_dir().expect("home directory").join(".config"))
        .join("imesh")
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_context: Option<String>,
    #[serde(default)]
    pub contexts: BTreeMap<String, Context>,
    #[serde(skip)]
    path: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Context {
    #[serde(default)]
    pub connections: Vec<Connection>,
}

/// One way to reach one machine. Exactly one of `ssh`, `iroh`, `local` is set.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Connection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_key_file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iroh: Option<EndpointId>,
    /// Relay to reach the `iroh` machine through, for example a self-hosted relay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<RelayUrl>,
    /// Private relay forwarded by the logged-in devtunnel CLI, as devtunnel:<id>:<port>.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub devtunnel: Option<String>,
    /// Bearer token for `relay`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_token: Option<String>,
    /// Extra CA certificates (PEM, a local path) to trust for `relay`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_ca: Option<PathBuf>,
    /// Do not use n0 relays or n0 address lookup for this connection.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_n0: bool,
    /// Known direct addresses of the `iroh` machine.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addrs: Vec<std::net::SocketAddr>,
    /// With `ssh`: the daemon runs in this Docker container on the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker: Option<String>,
    /// The local API of a machine: a unix socket path, or a named pipe on Windows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<PathBuf>,
    /// The machine this connection reaches. Informational, used by `machine rm`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<EndpointId>,
}

impl Connection {
    /// The daemon on this machine.
    pub fn local() -> Self {
        Connection { local: Some(crate::paths::find()), ..Default::default() }
    }

    /// Parses `--connect` values: `ssh://user@host[:port]`, `user@host`, `iroh://<id or ticket>`,
    /// `local://<path>`.
    pub fn parse(s: &str) -> Result<Self> {
        let mut c = Connection::default();
        if let Some(rest) = s.strip_prefix("iroh://") {
            c.iroh = Some(rest.parse().context("invalid endpoint id")?);
        } else if let Some(rest) = s.strip_prefix("local://") {
            c.local = Some(rest.into());
        } else {
            c.ssh = Some(s.strip_prefix("ssh://").unwrap_or(s).to_string());
        }
        Ok(c)
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(tunnel) = &self.devtunnel {
            let t: imesh::net::DevTunnel = tunnel.parse()?;
            anyhow::ensure!(self.iroh.is_some(), "devtunnel is for an iroh connection");
            anyhow::ensure!(
                self.relay.as_ref().is_none_or(|r| *r == t.relay_url()),
                "devtunnel relay must match its loopback port"
            );
        }
        let n = [self.ssh.is_some(), self.iroh.is_some(), self.local.is_some()].iter().filter(|b| **b).count();
        if n != 1 {
            bail!("a connection needs exactly one of ssh, iroh, local");
        }
        Ok(())
    }
}

impl std::fmt::Display for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(s) = &self.ssh {
            write!(f, "ssh://{s}")?;
            if let Some(c) = &self.docker {
                write!(f, " (docker {c})")?;
            }
            if let Some(k) = &self.ssh_key_file {
                write!(f, " (key {})", k.display())?;
            }
        } else if let Some(id) = &self.iroh {
            write!(f, "iroh://{id}")?;
            if let Some(r) = &self.relay {
                write!(f, " (relay {r})")?;
            }
        } else if let Some(p) = &self.local {
            write!(f, "local://{}", p.display())?;
        }
        Ok(())
    }
}

impl Config {
    pub fn path() -> PathBuf {
        match std::env::var_os("IMESH_CONFIG") {
            Some(p) => p.into(),
            None => config_dir().join("config.toml"),
        }
    }

    pub fn load() -> Result<Self> {
        let path = Self::path();
        let mut cfg: Config = match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parse {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        cfg.path = path;
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        imesh::os::write_private(&self.path, toml::to_string_pretty(self)?.as_bytes())
    }

    /// Adds a context with a fresh name based on `base` and makes it current.
    pub fn add_context(&mut self, base: &str, connections: Vec<Connection>) -> String {
        let mut name = base.to_string();
        let mut i = 1;
        while self.contexts.contains_key(&name) {
            name = format!("{base}-{i}");
            i += 1;
        }
        self.contexts.insert(name.clone(), Context { connections });
        self.current_context = Some(name.clone());
        name
    }

    /// The named context, or the current one.
    pub fn context(&self, name: Option<&str>) -> Result<(&str, &Context)> {
        let name = self.name(name)?;
        let (k, v) = self.contexts.get_key_value(name).with_context(|| format!("context {name:?} not found"))?;
        Ok((k, v))
    }

    pub fn context_mut(&mut self, name: Option<&str>) -> Result<&mut Context> {
        let name = self.name(name)?.to_string();
        self.contexts.get_mut(&name).with_context(|| format!("context {name:?} not found"))
    }

    fn name<'a>(&'a self, name: Option<&'a str>) -> Result<&'a str> {
        name.or(self.current_context.as_deref()).context("no context: run `imesh machine init` or pass --connect")
    }
}

/// The CLI's own iroh identity, used for `iroh://` connections.
pub fn client_key() -> Result<SecretKey> {
    imesh::os::load_or_create_key(&config_dir().join("identity.key"))
}
