//! SSH through the system `ssh` binary, so `~/.ssh/config`, agents and jump hosts work.

use std::{
    path::PathBuf,
    pin::Pin,
    process::Stdio,
    task::{Context as TaskContext, Poll},
};

use anyhow::{Context, Result, bail};
use iroh_mesh::{
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
    /// Reach the SSH server through this private devtunnel, with `imesh ssh-tunnel-proxy`.
    tunnel: Option<iroh_mesh::net::DevTunnel>,
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
        Ssh { dest, port, key, tunnel: None }
    }

    /// Connects through a private devtunnel. SSH still checks the host key, under an alias made
    /// from the tunnel ID and port, and authenticates the user.
    pub fn with_tunnel(mut self, tunnel: Option<iroh_mesh::net::DevTunnel>) -> Self {
        self.tunnel = tunnel;
        self
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
        if let Some(dir) = Self::control_dir().filter(|_| self.tunnel.is_none()) {
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
        if let Some(t) = &self.tunnel {
            c.args(["-o", "ControlMaster=no", "-o", "ControlPath=none"]);
            c.arg("-o").arg(format!("HostKeyAlias={}", tunnel_host_alias(t)));
            c.arg("-o").arg(format!("ProxyCommand={}", proxy_command(t)));
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

    /// Runs a PowerShell script on a Windows host, as `-EncodedCommand`, so no shell reads it.
    pub async fn powershell(&self, script: &str, stdin: &[u8]) -> Result<String> {
        let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let enc = data_encoding::BASE64.encode(&utf16);
        self.run(&format!("powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand {enc}"), stdin)
            .await
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

/// The host key alias for an SSH server behind a tunnel: the same for every local port.
pub fn tunnel_host_alias(t: &iroh_mesh::net::DevTunnel) -> String {
    format!("imesh-tunnel-{}-{}", t.id, t.port)
}

/// The SSH `ProxyCommand` that reaches the server through the tunnel: this executable, by its
/// absolute path, as `imesh ssh-proxy`. `%` is escaped for SSH; the tunnel ID is checked.
pub fn proxy_command(t: &iroh_mesh::net::DevTunnel) -> String {
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "imesh".into());
    let dt = crate::devtunnel_cli::devtunnel_cli();
    format!(
        "\"{}\" ssh-tunnel-proxy {t} --devtunnel-cli \"{}\"",
        exe.replace('%', "%%"),
        dt.display().to_string().replace('%', "%%")
    )
}

/// `imesh ssh-tunnel-proxy devtunnel:<id>:<port>`: starts its own devtunnel client, then joins stdin and
/// stdout to the forwarded SSH port. SSH runs it as a `ProxyCommand`. Diagnostics go to stderr.
pub async fn proxy(tunnel: iroh_mesh::net::DevTunnel, cli: Option<PathBuf>) -> Result<()> {
    let cli = cli.unwrap_or_else(crate::devtunnel_cli::devtunnel_cli);
    crate::devtunnel_cli::check_devtunnel_at(&cli).await?;
    let forwarding = forwarding(&tunnel, &cli).await?;
    let stream = tokio::net::TcpStream::connect(forwarding.addr)
        .await
        .with_context(|| format!("connect to the forwarded port {}", tunnel.port))?;
    let (mut r, mut w) = stream.into_split();
    let up = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut tokio::io::stdin(), &mut w).await;
        let _ = w.shutdown().await;
    });
    let mut stdout = tokio::io::stdout();
    tokio::io::copy(&mut r, &mut stdout).await?;
    stdout.flush().await?;
    up.abort();
    Ok(())
}

/// Runs `imesh dial-stdio` on the server: this user's daemon binary in the data directory, else the
/// system one, else `imesh` on the PATH. Through `sh`, because the login shell may be any shell.
const REMOTE_DIAL: &str = r#"sh -c 'for b in "${XDG_DATA_HOME:-$HOME/.local/share}/imesh/bin/imesh" /usr/local/bin/imesh; do if [ -x "$b" ]; then exec "$b" dial-stdio; fi; done; exec imesh dial-stdio'"#;

/// Reaches a machine's local API over SSH: each call runs `imesh dial-stdio` there, on the host
/// or in its container.
pub struct SshDial {
    ssh: Ssh,
    command: String,
}

/// The same on Windows, through the default shell of Windows OpenSSH, cmd.exe.
const REMOTE_DIAL_WINDOWS: &str = r#""%LOCALAPPDATA%\imesh\bin\imesh.exe" dial-stdio"#;

impl SshDial {
    pub fn host(ssh: Ssh) -> Self {
        SshDial { ssh, command: REMOTE_DIAL.into() }
    }

    /// A user install on Windows.
    pub fn windows(ssh: Ssh) -> Self {
        SshDial { ssh, command: REMOTE_DIAL_WINDOWS.into() }
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

struct Forwarding {
    _child: tokio::process::Child,
    addr: std::net::SocketAddr,
}

async fn forwarding(t: &iroh_mesh::net::DevTunnel, cli: &std::path::Path) -> Result<Forwarding> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let details = Command::new(cli).args(["show", &t.id, "--json"]).kill_on_drop(true).output();
    let details = tokio::time::timeout(std::time::Duration::from_secs(30), details).await??;
    anyhow::ensure!(details.status.success(), "cannot inspect private SSH tunnel {}", t.id);
    let v: serde_json::Value = serde_json::from_slice(&details.stdout)?;
    let ports = v.pointer("/tunnel/ports").and_then(serde_json::Value::as_array).context("tunnel has no ports")?;
    anyhow::ensure!(
        ports.len() == 1 && ports[0]["portNumber"].as_u64() == Some(u64::from(t.port)),
        "use a dedicated tunnel with only the requested SSH port"
    );
    fn anonymous(v: &serde_json::Value) -> bool {
        match v {
            serde_json::Value::Object(m) => {
                m.get("type").and_then(serde_json::Value::as_str).is_some_and(|s| s.eq_ignore_ascii_case("anonymous"))
                    || m.values().any(anonymous)
            }
            serde_json::Value::Array(a) => a.iter().any(anonymous),
            _ => false,
        }
    }
    anyhow::ensure!(!anonymous(&v), "SSH tunnel allows anonymous access");
    let mut child = Command::new(cli)
        .args(["connect", &t.id])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut out = BufReader::new(child.stdout.take().expect("piped")).lines();
    let mut err = BufReader::new(child.stderr.take().expect("piped")).lines();
    let mut out_open = true;
    let mut err_open = true;
    let mut connected = false;
    let mut address = None;
    let addr = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            let line = tokio::select! {
                line = out.next_line(), if out_open => { let l = line?; out_open = l.is_some(); l },
                line = err.next_line(), if err_open => { let l = line?; err_open = l.is_some(); l },
                status = child.wait() => bail!("devtunnel connect exited: {}", status?),
            };
            if let Some(line) = line {
                connected |= line.contains("Connected to tunnel:");
                if let Some(a) = forwarded_address(&line) {
                    address = Some(a);
                }
                if connected && let Some(addr) = address {
                    return anyhow::Ok(addr);
                }
            }
        }
    })
    .await
    .context("SSH tunnel did not become ready")??;
    tokio::spawn(async move { while let Ok(Some(_)) = out.next_line().await {} });
    tokio::spawn(async move { while let Ok(Some(_)) = err.next_line().await {} });
    Ok(Forwarding { _child: child, addr })
}

fn forwarded_address(line: &str) -> Option<std::net::SocketAddr> {
    let (_, addr) = line.split_once("PortForwardingService listening on ")?;
    let addr = addr.trim().trim_end_matches('.').parse::<std::net::SocketAddr>().ok()?;
    (addr.ip().is_loopback() && addr.port() != 0).then_some(addr)
}

#[cfg(test)]
mod forwarding_tests {
    #[test]
    fn accepts_owned_alternate_loopback_ports_only() {
        assert_eq!(
            super::forwarded_address("SSH: PortForwardingService listening on 127.0.0.1:2223.").unwrap().port(),
            2223
        );
        assert!(super::forwarded_address("SSH: PortForwardingService listening on 0.0.0.0:2222.").is_none());
        assert!(super::forwarded_address("Connected to tunnel: x").is_none());
    }
}
