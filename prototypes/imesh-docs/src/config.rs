//! Client config: `~/.config/imesh/config.toml` plus the CLI identity key.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};
use iroh::{EndpointId, RelayUrl, SecretKey};
use serde::{Deserialize, Serialize};

pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("IMESH_CONFIG_DIR") {
        return dir.into();
    }
    dirs::home_dir().expect("home directory").join(".config/imesh")
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
    /// Known direct addresses of the `iroh` machine.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addrs: Vec<std::net::SocketAddr>,
    /// The local API of a machine: a unix socket path, or a named pipe on Windows.
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "unix")]
    pub local: Option<PathBuf>,
    /// The machine this connection reaches. Informational, used by `machine rm`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<EndpointId>,
}

impl Connection {
    /// Parses `--connect` values: `ssh://user@host[:port]`, `user@host`, `iroh://<id or ticket>`,
    /// `local://<path>`, `unix:///path`.
    pub fn parse(s: &str) -> Result<Self> {
        let mut c = Connection::default();
        if let Some(rest) = s.strip_prefix("iroh://") {
            c.iroh = Some(rest.parse().context("invalid endpoint id")?);
        } else if let Some(rest) = s.strip_prefix("unix://").or_else(|| s.strip_prefix("local://")) {
            c.local = Some(rest.into());
        } else {
            c.ssh = Some(s.strip_prefix("ssh://").unwrap_or(s).to_string());
        }
        Ok(c)
    }

    pub fn validate(&self) -> Result<()> {
        let n = [self.ssh.is_some(), self.iroh.is_some(), self.local.is_some()]
            .iter()
            .filter(|b| **b)
            .count();
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
        write_private(&self.path, toml::to_string_pretty(self)?.as_bytes())
    }

    /// Adds a context with a fresh name based on `base` and makes it current.
    pub fn add_context(&mut self, base: &str, connection: Connection) -> String {
        let mut name = base.to_string();
        let mut i = 1;
        while self.contexts.contains_key(&name) {
            name = format!("{base}-{i}");
            i += 1;
        }
        self.contexts.insert(
            name.clone(),
            Context {
                connections: vec![connection],
            },
        );
        self.current_context = Some(name.clone());
        name
    }

    pub fn context(&self, name: Option<&str>) -> Result<(&str, &Context)> {
        let name = name
            .or(self.current_context.as_deref())
            .context("no context: run `imesh machine init` or pass --connect")?;
        let (k, v) = self
            .contexts
            .get_key_value(name)
            .with_context(|| format!("context {name:?} not found"))?;
        Ok((k, v))
    }
}

/// The CLI's own iroh identity, used for `iroh://` connections.
pub fn client_key() -> Result<SecretKey> {
    load_or_create_key(&config_dir().join("identity.key"))
}

pub fn load_or_create_key(path: &Path) -> Result<SecretKey> {
    match std::fs::read_to_string(path) {
        Ok(s) => s.trim().parse().context("invalid key file"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let key = SecretKey::generate();
            write_private(path, hex::encode(key.to_bytes()).as_bytes())?;
            Ok(key)
        }
        Err(e) => Err(e.into()),
    }
}

/// Writes a file with mode 0600 through a temp file and rename.
pub fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let dir = path.parent().context("path has no parent")?;
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    b.mode(0o700);
    b.create(dir)?;
    let tmp = path.with_extension("tmp");
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    o.mode(0o600);
    let mut f = o.open(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
