//! Linux and macOS: the local API is a unix socket.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail, ensure};

use crate::{api::Io, net::local::Access};

pub async fn connect(path: &Path) -> Result<Box<dyn Io>> {
    let s = tokio::net::UnixStream::connect(path).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            anyhow!("permission denied on {}: add this user to the group that owns it and log in again", path.display())
        } else {
            anyhow!("connect {}: {e} (is the imesh daemon running?)", path.display())
        }
    })?;
    Ok(Box::new(s))
}

pub struct Listener {
    inner: tokio::net::UnixListener,
}

impl Listener {
    /// The socket gets its mode before it appears at `path`, so no one else can connect in between.
    pub fn bind(path: &Path, access: Access) -> Result<Self> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let dir = path.parent().context("socket path has no parent")?;
        if !dir.exists() {
            // The group must be able to enter the directory of a group socket.
            let mode = match access {
                Access::Owner => 0o700,
                Access::Group(_) => 0o750,
            };
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(mode)
                .create(dir)
                .with_context(|| format!("create {}", dir.display()))?;
        }
        // A socket that answers belongs to a daemon that runs; do not take it over.
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            bail!("{} is in use by a running daemon", path.display());
        }
        // Bind in a new directory that only this user may enter, set the mode, then move the
        // socket into place.
        let staging = tempfile::Builder::new().prefix(".imesh-bind").tempdir_in(dir)?;
        let tmp = staging.path().join("s");
        let inner = tokio::net::UnixListener::bind(&tmp).with_context(|| format!("bind {}", tmp.display()))?;
        let mode = match &access {
            Access::Owner => 0o600,
            Access::Group(group) => {
                set_group(&tmp, group)?;
                0o660
            }
        };
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
        std::fs::rename(&tmp, path).with_context(|| format!("bind {}", path.display()))?;
        Ok(Listener { inner })
    }

    pub async fn accept(&mut self) -> Result<Box<dyn Io>> {
        let (s, _) = self.inner.accept().await?;
        Ok(Box::new(s))
    }

    pub fn remove(path: &Path) {
        let _ = std::fs::remove_file(path);
    }
}

/// Ctrl-C, or SIGTERM from systemd.
pub fn shutdown_signal() -> impl std::future::Future<Output = ()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("listen for SIGTERM");
    let mut int = signal(SignalKind::interrupt()).expect("listen for SIGINT");
    async move {
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
    }
}

/// Tells systemd (`Type=notify`) that the daemon is ready.
/// Nothing to do: unix has no job objects.
pub fn allow_job_breakaway() {}

/// Ends the process quietly when it writes to a closed pipe, as `ls | head` does, instead of the
/// write failing. Call it before any other thread starts.
pub fn exit_on_closed_pipe() {
    // SAFETY: restores the default action of one signal; no other thread runs yet.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

pub fn sd_notify_ready() {
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    let Ok(sock) = std::os::unix::net::UnixDatagram::unbound() else {
        return;
    };
    let path = path.to_string_lossy().into_owned();
    #[cfg(target_os = "linux")]
    if let Some(name) = path.strip_prefix('@') {
        use std::os::linux::net::SocketAddrExt;
        if let Ok(addr) = std::os::unix::net::SocketAddr::from_abstract_name(name) {
            let _ = sock.send_to_addr(b"READY=1", &addr);
        }
        return;
    }
    let _ = sock.send_to(b"READY=1", path);
}

/// Gives the socket to a group. The daemon must be a member, for example through systemd's `Group=`.
fn set_group(path: &Path, group: &str) -> Result<()> {
    let name = std::ffi::CString::new(group)?;
    // SAFETY: a NUL-terminated name; the result points to static storage, read at once.
    let grp = unsafe { libc::getgrnam(name.as_ptr()) };
    ensure!(!grp.is_null(), "no group {group:?}");
    // SAFETY: checked above.
    let gid = unsafe { (*grp).gr_gid };
    std::os::unix::fs::chown(path, None, Some(gid)).with_context(|| format!("give {} to group {group}", path.display()))
}

/// Creates a directory and its missing parents; the ones it creates only this user may enter.
pub fn create_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("create {}", dir.display()))
}

/// Only this user may read a file it creates.
pub fn private_file(o: &mut std::fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    o.mode(0o600);
}

/// Makes a rename in `dir` durable.
pub fn sync_dir(dir: &Path) -> Result<()> {
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}
