//! SSH through the system `ssh` binary, so `~/.ssh/config`, agents and jump hosts work.

use std::{path::PathBuf, process::Stdio};

use anyhow::{Context, Result, bail};
use tokio::{io::AsyncWriteExt, process::Command};

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
