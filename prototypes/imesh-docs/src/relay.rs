//! Relays: the relay this daemon can serve to members, and how it is exposed to the internet.

use std::{net::SocketAddr, path::PathBuf, process::Stdio, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use iroh::{EndpointId, RelayUrl};
use iroh_relay::server::{
    Access, AccessControl, CertConfig, ClientRequest, RelayConfig, Server, ServerConfig, TlsConfig,
};
use serde::Deserialize;
use tokio::sync::watch;
use tracing::{info, warn};

/// Relay settings of one daemon.
#[derive(Debug, Clone, clap::Args)]
pub struct Opts {
    /// Relay servers to use instead of the n0 relays. Repeat or separate with commas.
    #[arg(long = "relay", env = "IMESH_RELAYS", value_delimiter = ',')]
    pub relays: Vec<RelayUrl>,
    /// Bearer token to present to relays that require one.
    #[arg(long, env = "IMESH_RELAY_TOKEN", hide_env_values = true)]
    pub relay_token: Option<String>,
    /// Extra CA certificates (PEM) to trust for relay TLS, for relays with private certificates.
    #[arg(long, env = "IMESH_RELAY_CA")]
    pub relay_ca: Option<PathBuf>,
    /// Do not use n0 relays or n0 DNS lookup.
    #[arg(long, env = "IMESH_NO_N0")]
    pub no_n0: bool,
    /// Also run an iroh relay on this TCP address, for example 0.0.0.0:3340. Only members,
    /// authorized CLI keys, and this machine may use it.
    #[arg(long, env = "IMESH_SERVE_RELAY")]
    pub serve_relay: Option<SocketAddr>,
    /// Also require this bearer token on the served relay.
    #[arg(long, env = "IMESH_SERVE_RELAY_TOKEN", hide_env_values = true)]
    pub serve_relay_token: Option<String>,
    /// Certificate chain (PEM) to serve the relay over HTTPS. Needs --serve-relay-key.
    #[arg(long, env = "IMESH_SERVE_RELAY_CERT", requires = "serve_relay_key")]
    pub serve_relay_cert: Option<PathBuf>,
    /// Private key (PEM) for --serve-relay-cert.
    #[arg(long, env = "IMESH_SERVE_RELAY_KEY", requires = "serve_relay_cert")]
    pub serve_relay_key: Option<PathBuf>,
    /// Publish the served relay through a tunnel: `devtunnel` (found or created by labels) or
    /// `devtunnel:<tunnel id>`.
    #[arg(long, env = "IMESH_EXPOSE_RELAY", requires = "serve_relay")]
    pub expose_relay: Option<Expose>,
}

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

impl Opts {
    /// The settings as `VAR=value` lines for the daemon's environment file.
    pub fn to_env(&self) -> String {
        let mut e = String::new();
        let mut line = |k: &str, v: String| e.push_str(&format!("{k}={v}\n"));
        if !self.relays.is_empty() {
            line("IMESH_RELAYS", self.relays.iter().map(|r| r.to_string()).collect::<Vec<_>>().join(","));
        }
        if let Some(t) = &self.relay_token {
            line("IMESH_RELAY_TOKEN", t.clone());
        }
        if let Some(p) = &self.relay_ca {
            line("IMESH_RELAY_CA", p.display().to_string());
        }
        if self.no_n0 {
            line("IMESH_NO_N0", "true".into());
        }
        if let Some(a) = self.serve_relay {
            line("IMESH_SERVE_RELAY", a.to_string());
        }
        if let Some(t) = &self.serve_relay_token {
            line("IMESH_SERVE_RELAY_TOKEN", t.clone());
        }
        if let Some(p) = &self.serve_relay_cert {
            line("IMESH_SERVE_RELAY_CERT", p.display().to_string());
        }
        if let Some(p) = &self.serve_relay_key {
            line("IMESH_SERVE_RELAY_KEY", p.display().to_string());
        }
        if let Some(x) = &self.expose_relay {
            line("IMESH_EXPOSE_RELAY", x.to_string());
        }
        e
    }

    /// The relay map entry for a URL, with the token when one is set.
    pub fn relay_config(&self, url: RelayUrl) -> iroh::RelayConfig {
        let c = iroh::RelayConfig::from(url);
        match &self.relay_token {
            Some(t) => c.with_auth_token(t.clone()),
            None => c,
        }
    }

    pub fn ca_tls_config(&self) -> Result<Option<iroh_relay::tls::CaTlsConfig>> {
        let Some(path) = &self.relay_ca else { return Ok(None) };
        let roots = load_certs(path)?;
        Ok(Some(iroh_relay::tls::CaTlsConfig::default().with_extra_roots(roots)))
    }
}

/// Decides who may use the served relay. `allows` is the member check of the daemon.
pub struct Gate {
    pub me: EndpointId,
    pub token: Option<String>,
    pub allows: Box<dyn Fn(&EndpointId) -> bool + Send + Sync>,
}

impl std::fmt::Debug for Gate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Gate")
    }
}

impl AccessControl for Gate {
    async fn on_connect(&self, req: &ClientRequest) -> Access {
        let id = req.endpoint_id();
        if let Some(t) = &self.token
            && req.auth_token().as_deref() != Some(t.as_str())
        {
            return Access::Deny { reason: Some("bad or missing token".into()) };
        }
        if id == self.me || (self.allows)(&id) {
            Access::Allow
        } else {
            Access::Deny { reason: Some("not a member of this cluster".into()) }
        }
    }
}

/// Starts the relay server. With a certificate it serves HTTPS on `addr`, and the plain HTTP
/// probe port on localhost.
pub async fn serve(opts: &Opts, addr: SocketAddr, gate: Gate) -> Result<Server> {
    let mut relay;
    if let (Some(cert), Some(key)) = (&opts.serve_relay_cert, &opts.serve_relay_key) {
        let certs = load_certs(cert)?;
        let key = <rustls::pki_types::PrivateKeyDer as rustls::pki_types::pem::PemObject>::from_pem_file(key)
            .with_context(|| format!("read key {}", key.display()))?;
        let server_config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(certs, key)?;
        relay = RelayConfig::new(SocketAddr::from(([127, 0, 0, 1], 0)));
        relay.tls = Some(TlsConfig::new(addr, CertConfig::Manual { server_config }));
    } else {
        relay = RelayConfig::new(addr);
    }
    relay.access = Arc::new(gate);
    let mut config = ServerConfig::default();
    config.relay = Some(relay);
    let server = Server::spawn(config).await?;
    info!(http = ?server.http_addr(), https = ?server.https_addr(), "relay serving members");
    Ok(server)
}

fn load_certs(path: &PathBuf) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    use rustls::pki_types::pem::PemObject;
    rustls::pki_types::CertificateDer::pem_file_iter(path)
        .with_context(|| format!("read {}", path.display()))?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("parse {}", path.display()))
}

/// Keeps the relay reachable through a tunnel and reports its public URL.
pub fn expose(how: Expose, port: u16, machine: EndpointId, url: watch::Sender<Option<RelayUrl>>) {
    match how {
        Expose::DevTunnel { id } => {
            tokio::spawn(async move {
                let mut backoff = Duration::from_secs(1);
                loop {
                    match devtunnel(id.as_deref(), port, machine, &url).await {
                        Ok(()) => warn!("devtunnel host exited"),
                        Err(e) => warn!("devtunnel: {e:#}"),
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                }
            });
        }
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
async fn devtunnel(id: Option<&str>, port: u16, machine: EndpointId, url: &watch::Sender<Option<RelayUrl>>) -> Result<()> {
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
                    let c: Created = serde_json::from_str(&dt(&["create", "-a", "-l", "imesh", "-l", &label, "-j"]).await?)?;
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
