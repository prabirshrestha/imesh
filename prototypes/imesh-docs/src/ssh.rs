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

    fn control_dir() -> PathBuf {
        // Short fixed path: unix socket paths are limited to about 104 bytes on macOS.
        let user = std::env::var("USER").unwrap_or_default();
        PathBuf::from(format!("/tmp/imesh-ssh-{user}"))
    }

    fn options(&self) -> Command {
        let dir = Self::control_dir();
        let _ = std::fs::create_dir_all(&dir);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        let mut c = Command::new("ssh");
        if let Some(f) = std::env::var_os("IMESH_SSH_CONFIG") {
            c.arg("-F").arg(f);
        }
        // Windows OpenSSH has no connection sharing.
        if cfg!(unix) {
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
            bail!(
                "ssh {}: {} failed: {}",
                self.dest,
                remote,
                String::from_utf8_lossy(&out.stderr).trim()
            );
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

