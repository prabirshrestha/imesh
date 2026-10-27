//! Exposes the served relay through a Microsoft dev tunnel, found or created by labels.

use std::{process::Stdio, time::Duration};

use anyhow::{Context, Result, bail};
use iroh::{EndpointId, RelayUrl};
use n0_future::task::AbortOnDropHandle;
use serde::Deserialize;
use tokio::sync::watch;
use tracing::{info, warn};

#[derive(Debug, Clone)]
pub enum Expose {
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
            Some(("devtunnel", id)) if !id.is_empty() => Ok(Expose::DevTunnel { id: Some(id.to_string()) }),
            _ => Err(format!("unknown relay exposure {s:?}; supported: devtunnel, devtunnel:<id>")),
        }
    }
}

/// Keeps the relay reachable through a tunnel and reports its public URL. Dropping the handle
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
                    Ok(()) => warn!("devtunnel host exited"),
                    Err(e) => warn!("devtunnel: {e:#}"),
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        })),
    }
}

fn devtunnel_bin() -> String {
    std::env::var("IMESH_DEVTUNNEL").unwrap_or_else(|_| "devtunnel".into())
}

async fn dt(args: &[&str]) -> Result<String> {
    let out = tokio::process::Command::new(devtunnel_bin())
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
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
    #[serde(default)]
    port_uri: Option<String>,
}

/// Finds or creates this machine's tunnel, adds the relay port, publishes the URL, and runs
/// `devtunnel host` until it exits. Only the child started here is ever stopped.
async fn devtunnel(
    id: Option<&str>,
    port: u16,
    machine: EndpointId,
    url: &watch::Sender<Option<RelayUrl>>,
) -> Result<()> {
    let label = format!("imesh-{}", machine.fmt_short());
    let tunnel = match id {
        Some(id) => {
            if dt(&["show", id, "-j"]).await.is_err() {
                dt(&["create", id, "-a", "-l", "imesh", "-l", &label, "-j"]).await?;
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
                    let c: Created =
                        serde_json::from_str(&dt(&["create", "-a", "-l", "imesh", "-l", &label, "-j"]).await?)?;
                    c.tunnel.tunnel_id
                }
            }
        }
    };
    let p = port.to_string();
    #[derive(Deserialize)]
    struct Show {
        tunnel: Tunnel,
    }
    let show: Show = serde_json::from_str(&dt(&["show", &tunnel, "-j"]).await?)?;
    if !show.tunnel.ports.iter().any(|x| x.port_number == port) {
        dt(&["port", "create", &tunnel, "-p", &p, "--protocol", "http"]).await?;
    }
    let mut child = tokio::process::Command::new(devtunnel_bin())
        .args(["host", &tunnel])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("start devtunnel host")?;
    // The port URL appears once a host is connected.
    let mut uri = None;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if let Some(status) = child.try_wait()? {
            bail!("devtunnel host exited early: {status}");
        }
        let s: Show = serde_json::from_str(&dt(&["show", &tunnel, "-j"]).await?)?;
        uri = s.tunnel.ports.into_iter().find(|x| x.port_number == port).and_then(|x| x.port_uri);
        if uri.is_some() {
            break;
        }
    }
    let public: RelayUrl = uri.context("devtunnel did not report a port URL")?.parse().context("devtunnel port URL")?;
    info!(tunnel = %tunnel, url = %public, "relay exposed through devtunnel");
    url.send_replace(Some(public));
    let status = child.wait().await?;
    url.send_replace(None);
    info!(?status, "devtunnel host stopped");
    Ok(())
}
