//! Exposes the served relay through a Microsoft dev tunnel, found or created by labels.

use std::{process::Stdio, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use iroh::{EndpointId, RelayUrl};
use n0_future::task::AbortOnDropHandle;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::watch;
use tracing::{info, warn};

/// How a machine publishes the relay it serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expose {
    /// Through a Microsoft dev tunnel, with the `devtunnel` CLI on the PATH. With no ID, the
    /// tunnel is found or created by the labels `imesh` and `imesh-<first 10 hex of the id>`.
    DevTunnel { id: Option<String> },
}

impl std::fmt::Display for Expose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Expose::DevTunnel { id: None } => f.write_str("devtunnel"),
            Expose::DevTunnel { id: Some(id) } => write!(f, "devtunnel:{id}"),
        }
    }
}

impl std::str::FromStr for Expose {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.split_once(':') {
            None if s == "devtunnel" => Ok(Expose::DevTunnel { id: None }),
            Some(("devtunnel", id)) if !id.is_empty() => {
                Ok(Expose::DevTunnel { id: Some(valid_id(id).map_err(|e| e.to_string())?.to_string()) })
            }
            _ => Err(format!("unknown relay exposure {s:?}; supported: devtunnel, devtunnel:<id>")),
        }
    }
}

/// Keeps the relay reachable through a private tunnel and reports its loopback URL. Dropping the handle
/// stops the tunnel host.
pub(crate) fn expose(
    how: Expose,
    port: u16,
    machine: EndpointId,
    url: watch::Sender<Option<RelayUrl>>,
) -> AbortOnDropHandle<()> {
    match how {
        Expose::DevTunnel { id } => AbortOnDropHandle::new(tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            loop {
                match devtunnel(id.as_deref(), port, machine, &url).await {
                    Ok(stable) => {
                        if stable {
                            backoff = Duration::from_secs(1);
                        }
                        warn!("devtunnel host exited");
                    }
                    Err(e) => warn!("devtunnel: {e:#}"),
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        })),
    }
}

async fn dt(args: &[&str]) -> Result<String> {
    let out = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new("devtunnel").args(args).stdin(Stdio::null()).kill_on_drop(true).output(),
    )
    .await
    .context("devtunnel command timed out")?
    .context("run devtunnel (is it installed and logged in?)")?;
    if !out.status.success() {
        bail!(
            "devtunnel {}: {}{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Tunnel {
    tunnel_id: String,
    #[serde(default)]
    ports: Vec<Port>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Port {
    port_number: u16,
}

/// Finds or creates this machine's tunnel, adds the relay port, publishes the URL, and runs
/// `devtunnel host` until it exits. Only the child started here is ever stopped.
async fn devtunnel(
    id: Option<&str>,
    port: u16,
    machine: EndpointId,
    url: &watch::Sender<Option<RelayUrl>>,
) -> Result<bool> {
    let label = format!("imesh-{}", machine.fmt_short());
    let tunnel = match id {
        Some(id) => {
            valid_id(id)?;
            if dt(&["show", id, "-j"]).await.is_err() {
                dt(&["create", id, "-l", "imesh", "-l", &label, "-j"]).await?;
            }
            id.to_string()
        }
        None => {
            #[derive(Deserialize)]
            struct List {
                #[serde(default)]
                tunnels: Vec<Tunnel>,
            }
            let list: List =
                serde_json::from_str(&dt(&["list", "--all-labels", "imesh", "--all-labels", &label, "-j"]).await?)?;
            match list.tunnels.into_iter().next() {
                Some(t) => t.tunnel_id,
                None => {
                    #[derive(Deserialize)]
                    struct Created {
                        tunnel: Tunnel,
                    }
                    let c: Created = serde_json::from_str(&dt(&["create", "-l", "imesh", "-l", &label, "-j"]).await?)?;
                    c.tunnel.tunnel_id
                }
            }
        }
    };
    valid_id(&tunnel)?;
    let p = port.to_string();
    #[derive(Deserialize)]
    struct Show {
        tunnel: Tunnel,
    }
    let details: serde_json::Value = serde_json::from_str(&dt(&["show", &tunnel, "-j"]).await?)?;
    let show: Show = serde_json::from_value(details.clone())?;
    ensure!(
        show.tunnel.ports.iter().all(|p| p.port_number == port),
        "use a dedicated devtunnel with only the relay port"
    );
    ensure!(!anonymous(&details), "devtunnel {tunnel} allows anonymous access; remove that access first");
    if !show.tunnel.ports.iter().any(|x| x.port_number == port) {
        dt(&["port", "create", &tunnel, "-p", &p, "--protocol", "http"]).await?;
    }
    let mut child = tokio::process::Command::new("devtunnel")
        .args(["host", &tunnel])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("start devtunnel host")?;
    // Clients authenticate with `devtunnel connect` and forward this same port locally.
    // The HTTPS web-forwarding URL stays private and is not an iroh relay address.
    let local: RelayUrl = format!("http://127.0.0.1:{port}/").parse()?;
    wait_ready(&mut child, None).await?;
    info!(%tunnel, %local, "private relay hosted through devtunnel");
    url.send_replace(Some(local));
    let started = tokio::time::Instant::now();
    let status = child.wait().await;
    url.send_replace(None);
    let status = status?;
    info!(?status, "devtunnel host stopped");
    Ok(started.elapsed() >= Duration::from_secs(60))
}

fn valid_id(id: &str) -> Result<&str> {
    ensure!(
        id.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
            && id.len() <= 200
            && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'.'),
        "invalid devtunnel ID"
    );
    Ok(id)
}

fn anonymous(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(o) => {
            o.get("type").and_then(|v| v.as_str()).is_some_and(|t| t.eq_ignore_ascii_case("anonymous"))
                || o.values().any(anonymous)
        }
        serde_json::Value::Array(a) => a.iter().any(anonymous),
        _ => false,
    }
}

/// A private tunnel forwarded by the logged-in devtunnel CLI. The same port must be
/// free on each client. Use one tunnel per endpoint; loopback URLs do not name a tunnel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connect {
    pub id: String,
    pub port: u16,
}

impl std::str::FromStr for Connect {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        let (id, port) = s
            .strip_prefix("devtunnel:")
            .context("expected devtunnel:<id>:<port>")?
            .rsplit_once(':')
            .context("expected devtunnel:<id>:<port>")?;
        let port = port.parse::<u16>()?;
        ensure!(port != 0, "devtunnel port must not be zero");
        Ok(Self { id: valid_id(id)?.to_string(), port })
    }
}

impl std::fmt::Display for Connect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "devtunnel:{}:{}", self.id, self.port)
    }
}

impl Connect {
    pub fn relay_url(&self) -> RelayUrl {
        format!("http://127.0.0.1:{}/", self.port).parse().expect("loopback URL")
    }

    /// Starts the CLI and waits for its forwarding announcement. Dropping the handle
    /// stops only this child. No login credentials or tokens enter imesh's database.
    pub async fn start(&self) -> Result<AbortOnDropHandle<()>> {
        valid_id(&self.id)?;
        ensure!(self.port != 0, "devtunnel port must not be zero");
        let first = self.connect().await?;
        let config = self.clone();
        Ok(AbortOnDropHandle::new(tokio::spawn(async move {
            let mut child = first;
            let mut backoff = Duration::from_secs(1);
            loop {
                let started = tokio::time::Instant::now();
                let _ = child.wait().await;
                if started.elapsed() >= Duration::from_secs(60) {
                    backoff = Duration::from_secs(1);
                }
                loop {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                    match config.connect().await {
                        Ok(next) => {
                            child = next;
                            break;
                        }
                        Err(e) => warn!("devtunnel connect: {e:#}"),
                    }
                }
            }
        })))
    }

    async fn connect(&self) -> Result<tokio::process::Child> {
        // Do not attach to an unrelated listener or accept the CLI's alternate-port fallback.
        let details: serde_json::Value = serde_json::from_str(&dt(&["show", &self.id, "-j"]).await?)?;
        ensure!(!anonymous(&details), "devtunnel {} allows anonymous access", self.id);
        let ports = details
            .pointer("/tunnel/ports")
            .and_then(|p| p.as_array())
            .context("devtunnel did not report its ports")?;
        ensure!(
            ports.len() == 1 && ports[0]["portNumber"].as_u64() == Some(u64::from(self.port)),
            "use a dedicated devtunnel with only the requested relay port"
        );
        let probe = tokio::net::TcpSocket::new_v4()?;
        probe.set_reuseaddr(true)?;
        probe
            .bind((std::net::Ipv4Addr::LOCALHOST, self.port).into())
            .with_context(|| format!("devtunnel local port {} is in use", self.port))?;
        let probe = probe.listen(1).with_context(|| format!("devtunnel local port {} is in use", self.port))?;
        drop(probe);
        let mut child = tokio::process::Command::new("devtunnel")
            .args(["connect", &self.id])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("start devtunnel connect (install the CLI and log in first)")?;
        wait_ready(&mut child, Some(&format!("127.0.0.1:{}", self.port))).await?;
        Ok(child)
    }
}

async fn wait_ready(child: &mut tokio::process::Child, expected: Option<&str>) -> Result<()> {
    let mut out = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
    let mut err = BufReader::new(child.stderr.take().expect("piped stderr")).lines();
    let mut stdout_open = true;
    let mut stderr_open = true;
    let mut ready = Ready::default();
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let line = tokio::select! {
                line = out.next_line(), if stdout_open => {
                    let line = line?;
                    stdout_open = line.is_some();
                    line
                }
                line = err.next_line(), if stderr_open => {
                    let line = line?;
                    stderr_open = line.is_some();
                    line
                }
                status = child.wait() => bail!("devtunnel child exited: {}", status?),
            };
            let Some(line) = line else { continue };
            if match expected {
                Some(at) => ready.update(&line, at),
                None => line.contains("Ready to accept connections for tunnel:"),
            } {
                return Ok(());
            }
        }
    })
    .await
    .context("devtunnel did not become ready within 60 seconds")??;
    // Drain both streams for the child's lifetime so its output cannot block it.
    tokio::spawn(async move {
        while let Ok(Some(line)) = out.next_line().await {
            tracing::debug!(%line, "devtunnel stdout");
        }
    });
    tokio::spawn(async move {
        while let Ok(Some(line)) = err.next_line().await {
            tracing::debug!(%line, "devtunnel stderr");
        }
    });
    Ok(())
}

#[derive(Default)]
struct Ready {
    connected: bool,
    listening: bool,
}

impl Ready {
    fn update(&mut self, line: &str, expected: &str) -> bool {
        self.connected |= line.contains("Connected to tunnel:");
        self.listening |= line.contains(&format!("PortForwardingService listening on {expected}."));
        self.connected && self.listening
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn host_rejects_invalid_library_id_before_running_cli() {
        let url = watch::channel(None).0;
        let machine = iroh::SecretKey::generate().public();
        let error = devtunnel(Some("--help"), 3340, machine, &url).await.unwrap_err();
        assert_eq!(error.to_string(), "invalid devtunnel ID");
        assert!(url.borrow().is_none());
    }

    #[test]
    fn forwarding_rule_is_not_readiness() {
        let mut ready = Ready::default();
        let at = "127.0.0.1:3340";
        assert!(!ready.update("SSH: Forwarding from 127.0.0.1:3340 to host port 3340.", at));
        assert!(!ready.update("Connected to tunnel: test", at));
        assert!(!ready.update("SSH: PortForwardingService listening on 127.0.0.1:3341.", at));
        assert!(ready.update("SSH: PortForwardingService listening on 127.0.0.1:3340.", at));
        let mut ready = Ready::default();
        assert!(!ready.update("SSH: PortForwardingService listening on 127.0.0.1:3340.", at));
        assert!(ready.update("Connected to tunnel: test", at));
    }

    #[test]
    fn private_tunnel_inputs_and_access() {
        let c: Connect = "devtunnel:my-relay.use:3340".parse().unwrap();
        assert_eq!(c.relay_url().as_str(), "http://127.0.0.1:3340/");
        for bad in ["devtunnel:-v:3340", "devtunnel:a:0", "devtunnel:a/b:3340", "devtunnel:a:65536"] {
            assert!(bad.parse::<Connect>().is_err(), "{bad}");
        }
        assert!(anonymous(&serde_json::json!({"tunnel":{"accessControl":{"entries":[{"type":"Anonymous"}]}}})));
        assert!(!anonymous(&serde_json::json!({"tunnel":{"accessControl":{"entries":[{"type":"Users"}]}}})));
    }
}
