//! Client config: `~/.config/imesh/config.toml`.

use std::{collections::BTreeMap, path::PathBuf};

use anyhow::{Context as _, Result, bail};
use iroh::EndpointId;
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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Connection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_key_file: Option<PathBuf>,
    /// With `ssh`: the daemon runs in this Docker container on the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker: Option<String>,
    /// The local API of a machine: a unix socket path, or a named pipe on Windows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<PathBuf>,
    /// With `ssh`: the remote OS, when it is not a Unix shell. `windows` runs PowerShell and cmd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// With `ssh`: reach the SSH server through this private devtunnel, as devtunnel:<id>:<port>.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_tunnel: Option<String>,
    /// The machine this connection reaches. Informational, used by `imesh rm`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<EndpointId>,
}

impl Connection {
    /// The daemon on this machine.
    pub fn local() -> Self {
        let path = std::env::var_os("IMESH_SOCKET").map(std::path::PathBuf::from).unwrap_or_else(crate::paths::find);
        Connection { local: Some(path), ..Default::default() }
    }

    /// Parses a connection for `ctx add`: `ssh://user@host[:port]`, `user@host`, or
    /// `local://<path>`.
    pub fn parse(s: &str) -> Result<Self> {
        let mut c = Connection::default();
        if s.contains("://") && !s.starts_with("ssh://") && !s.starts_with("local://") {
            bail!("unknown connection {s:?}; use user@host, ssh://user@host[:port], or local://<path>");
        }
        if let Some(rest) = s.strip_prefix("local://") {
            c.local = Some(rest.into());
        } else {
            c.ssh = Some(s.strip_prefix("ssh://").unwrap_or(s).to_string());
        }
        Ok(c)
    }

    pub fn validate(&self) -> Result<()> {
        if self.ssh_tunnel.is_some() || self.platform.is_some() {
            anyhow::ensure!(self.ssh.is_some(), "ssh_tunnel and platform are for an ssh connection");
        }
        if let Some(t) = &self.ssh_tunnel {
            t.parse::<iroh_mesh::net::DevTunnel>()?;
        }
        if self.ssh.is_some() == self.local.is_some() {
            bail!("a connection needs exactly one of ssh, local");
        }
        Ok(())
    }
}

impl std::fmt::Display for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(s) = &self.ssh {
            write!(f, "ssh://{s}")?;
            if let Some(t) = &self.ssh_tunnel {
                write!(f, " (through {t})")?;
            }
            if let Some(c) = &self.docker {
                write!(f, " (docker {c})")?;
            }
            if let Some(k) = &self.ssh_key_file {
                write!(f, " (key {})", k.display())?;
            }
        } else if let Some(p) = &self.local {
            write!(f, "local://{}", p.display())?;
        }
        Ok(())
    }
}

impl Config {
    pub fn edit<T>(f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        let path = Self::path();
        let _lock = crate::local_state::lock(&path.with_extension("lock"))?;
        let mut cfg = Self::load()?;
        let result = f(&mut cfg)?;
        cfg.save()?;
        Ok(result)
    }

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

    fn save(&self) -> Result<()> {
        iroh_mesh::os::write_private(&self.path, toml::to_string_pretty(self)?.as_bytes())
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
        name.or(self.current_context.as_deref()).context("no context: run `imesh up` here, or `imesh add user@host`")
    }
}
