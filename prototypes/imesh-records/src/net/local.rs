//! The local API endpoint: a unix socket on Linux and macOS, a named pipe on Windows.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use crate::api::Io;

/// The local API path: `/run/imesh/imesh.sock`, or on Windows a named pipe per user,
/// `\\.\pipe\imesh-<user>`, so another user cannot create the pipe first.
pub fn default_path() -> PathBuf {
    #[cfg(unix)]
    return PathBuf::from("/run/imesh/imesh.sock");
    #[cfg(windows)]
    return PathBuf::from(format!(r"\\.\pipe\imesh-{}", std::env::var("USERNAME").unwrap_or_default().to_lowercase()));
}

pub fn default_data_dir() -> PathBuf {
    #[cfg(unix)]
    return PathBuf::from("/var/lib/imesh");
    #[cfg(windows)]
    return dirs::data_local_dir().expect("LOCALAPPDATA").join("imesh");
}

pub async fn connect(path: &Path) -> Result<Box<dyn Io>> {
    #[cfg(unix)]
    {
        let s = tokio::net::UnixStream::connect(path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                anyhow!(
                    "permission denied on {}: add this user to the group that owns it and log in again",
                    path.display()
                )
            } else {
                anyhow!("connect {}: {e} (is the imesh daemon running?)", path.display())
            }
        })?;
        Ok(Box::new(s))
    }
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        // ERROR_PIPE_BUSY: every server instance is taken for a moment. Retry.
        for _ in 0..50 {
            match ClientOptions::new().open(path) {
                Ok(c) => return Ok(Box::new(c)),
                Err(e) if e.raw_os_error() == Some(231) => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await
                }
                Err(e) => return Err(anyhow!("connect {}: {e} (is the imesh daemon running?)", path.display())),
            }
        }
        Err(anyhow!("connect {}: pipe busy", path.display()))
    }
}

pub struct Listener {
    #[cfg(unix)]
    inner: tokio::net::UnixListener,
    #[cfg(windows)]
    path: PathBuf,
    #[cfg(windows)]
    next: tokio::net::windows::named_pipe::NamedPipeServer,
}

impl Listener {
    /// Unix: mode 0660, so the daemon's user and group can connect.
    /// Windows: the default pipe security, which gives write access to the owner, SYSTEM, and administrators.
    pub fn bind(path: &Path) -> Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = path.parent().context("socket path has no parent")?;
            std::fs::create_dir_all(dir)?;
            let _ = std::fs::remove_file(path);
            let inner = tokio::net::UnixListener::bind(path).with_context(|| format!("bind {}", path.display()))?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
            Ok(Listener { inner })
        }
        #[cfg(windows)]
        {
            use tokio::net::windows::named_pipe::ServerOptions;
            let next = ServerOptions::new()
                .first_pipe_instance(true)
                .create(path)
                .with_context(|| format!("create pipe {} (is another daemon running?)", path.display()))?;
            Ok(Listener { path: path.to_path_buf(), next })
        }
    }

    pub async fn accept(&mut self) -> Result<Box<dyn Io>> {
        #[cfg(unix)]
        {
            let (s, _) = self.inner.accept().await?;
            Ok(Box::new(s))
        }
        #[cfg(windows)]
        {
            use tokio::net::windows::named_pipe::ServerOptions;
            let connected = self.next.connect().await;
            // The pipe instance is used up either way. Make the next one first.
            let fresh = ServerOptions::new().create(&self.path)?;
            let this = std::mem::replace(&mut self.next, fresh);
            connected?;
            Ok(Box::new(this))
        }
    }

    pub fn remove(path: &Path) {
        #[cfg(unix)]
        let _ = std::fs::remove_file(path);
        #[cfg(windows)]
        let _ = path;
    }
}
