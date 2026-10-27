//! SSH through the system `ssh` binary, so `~/.ssh/config`, agents and jump hosts work.

use std::{
    path::PathBuf,
    pin::Pin,
    process::Stdio,
    task::{Context as TaskContext, Poll},
};

use anyhow::{Context, Result, bail};
use imesh::{
    api::Io,
    client::{BoxFuture, Dial},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf},
    process::Command,
};

#[derive(Debug, Clone)]
pub struct Ssh {
    /// `user@host` or `host`.
    dest: String,
    port: Option<u16>,
    key: Option<PathBuf>,
}

impl Ssh {
    /// Parses `[ssh://]user@host[:port]`.
    pub fn new(dest: &str, key: Option<PathBuf>) -> Self {
        let dest = dest.strip_prefix("ssh://").unwrap_or(dest);
        let (user, host) = match dest.rsplit_once('@') {
            Some((u, h)) => (Some(u), h),
            None => (None, dest),
        };
        let (host, port) = if let Some(rest) = host.strip_prefix('[') {
            let (h, tail) = rest.split_once(']').unwrap_or((rest, ""));
            (h, tail.strip_prefix(':').and_then(|p| p.parse().ok()))
        } else if host.matches(':').count() == 1 {
            let (h, p) = host.split_once(':').unwrap();
            (h, p.parse().ok())
        } else {
            (host, None)
        };
        let dest = match user {
            Some(u) => format!("{u}@{host}"),
            None => host.to_string(),
        };
        Ssh { dest, port, key }
    }

    /// The directory for shared SSH connections: `~/.ssh/imesh-cm`, mode 0700. It is used only
    /// when the current user owns it, so another user cannot plant a master socket there.
    fn control_dir() -> Option<PathBuf> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
            let home = dirs::home_dir()?;
            let dir = home.join(".ssh").join("imesh-cm");
            let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir);
            let meta = std::fs::metadata(&dir).ok()?;
            let owner = std::fs::metadata(&home).ok()?.uid();
            if meta.uid() != owner {
                return None;
            }
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
            Some(dir)
        }
        #[cfg(not(unix))]
        None
    }

    fn options(&self) -> Command {
        let mut c = Command::new("ssh");
        if let Some(f) = std::env::var_os("IMESH_SSH_CONFIG") {
            c.arg("-F").arg(f);
        }
        // Windows OpenSSH has no connection sharing.
        if let Some(dir) = Self::control_dir() {
            c.args(["-o", "ControlMaster=auto"])
                .arg("-o")
                .arg(format!("ControlPath={}/%C", dir.display()))
                .args(["-o", "ControlPersist=10m"]);
        }
        c.args(["-o", "ConnectTimeout=10"])
            .args(["-o", "BatchMode=yes"])
            .args(["-o", "StrictHostKeyChecking=accept-new"])
            .arg("-T");
        if let Some(p) = self.port {
            c.arg("-p").arg(p.to_string());
        }
        if let Some(k) = &self.key {
            c.arg("-i").arg(k);
        }
        c
    }

    fn command(&self) -> Command {
        let mut c = self.options();
        c.arg(&self.dest).arg("--");
        c
    }

    /// Runs `remote` through the remote login shell and feeds it `stdin`.
    pub async fn run(&self, remote: &str, stdin: &[u8]) -> Result<String> {
        let mut child = self
            .command()
            .arg(remote)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("run ssh")?;
        let mut sin = child.stdin.take().unwrap();
        sin.write_all(stdin).await?;
        drop(sin);
        let out = child.wait_with_output().await?;
        if !out.status.success() {
            bail!("ssh {}: {} failed: {}", self.dest, remote, String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Runs a POSIX sh script on the remote host.
    pub async fn script(&self, script: &str) -> Result<String> {
        self.run("sh -s", script.as_bytes()).await
    }

    /// Stops the shared connection so the next command logs in again.
    /// A new login picks up new group memberships.
    pub async fn close(&self) {
        let mut c = self.options();
        c.args(["-O", "exit"]).arg(&self.dest);
        let _ = c.stdout(Stdio::null()).stderr(Stdio::null()).status().await;
    }

    /// Starts `remote` and returns its stdio as a byte stream.
    pub fn stdio(&self, remote: &str) -> Result<tokio::process::Child> {
        self.command()
            .arg(remote)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .context("run ssh")
    }

    pub fn dest(&self) -> &str {
        &self.dest
    }
}

/// Runs `imesh dial-stdio` on the server: this user's binary, else the system one, else `imesh` on
/// the PATH. Through `sh`, because the login shell may be any shell, and `~/.local/bin` is often
/// not on the PATH of a command that SSH runs.
const REMOTE_DIAL: &str = r#"sh -c 'for b in "$HOME/.local/bin/imesh" /usr/local/bin/imesh; do if [ -x "$b" ]; then exec "$b" dial-stdio; fi; done; exec imesh dial-stdio'"#;

/// Reaches a machine's local API over SSH: each call runs `imesh dial-stdio` there, on the host
/// or in its container.
pub struct SshDial {
    ssh: Ssh,
    command: String,
}

impl SshDial {
    pub fn host(ssh: Ssh) -> Self {
        SshDial { ssh, command: REMOTE_DIAL.into() }
    }

    /// `imesh dial-stdio` in a container: through `docker`, or `sudo -n docker` when the user
    /// cannot run it directly. `container` must be a plain name.
    pub fn container(ssh: Ssh, container: &str) -> Self {
        let exec = format!("exec -i {container} /imesh dial-stdio");
        SshDial {
            ssh,
            command: format!(
                "sh -c 'if docker version >/dev/null 2>&1; then exec docker {exec}; else exec sudo -n docker {exec}; fi'"
            ),
        }
    }
}

impl Dial for SshDial {
    fn open(&self) -> BoxFuture<'_, Result<Box<dyn Io>>> {
        Box::pin(async {
            let mut child = self.ssh.stdio(&self.command)?;
            let out = child.stdout.take().expect("piped");
            let inp = child.stdin.take().expect("piped");
            // The child is killed when the stream is dropped.
            Ok(Box::new(ChildIo { _child: child, io: tokio::io::join(out, inp) }) as Box<dyn Io>)
        })
    }

    // The shared SSH connection stays for the next command (`ControlPersist`), and other imesh
    // commands may use it now.
}

/// A child process's stdio as one stream, which owns the child.
struct ChildIo<T> {
    _child: tokio::process::Child,
    io: T,
}

impl<T: AsyncRead + Unpin> AsyncRead for ChildIo<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for ChildIo<T> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}
