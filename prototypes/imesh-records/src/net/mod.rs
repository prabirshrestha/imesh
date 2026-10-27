//! How a machine connects: network options, the endpoint builder, name resolution, the local API
//! socket, the relay a machine can serve, and how that relay is exposed.

pub(crate) mod devtunnel;
pub(crate) mod dns;
pub mod local;
pub(crate) mod relay;

use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use iroh::RelayUrl;

pub use devtunnel::Expose;

/// Network settings of one daemon: relays, address lookup, and the relay it serves.
#[derive(Debug, Clone, Default, clap::Args)]
#[group(skip)]
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
    /// Also publish and resolve addresses through this pkarr server (a self-hosted iroh-dns-server),
    /// over HTTP. For example `https://dns.example.com/pkarr`
    #[arg(long, env = "IMESH_PKARR")]
    pub pkarr: Option<url::Url>,
    /// Also find machines on the same LAN with mDNS.
    #[arg(long, env = "IMESH_MDNS")]
    pub mdns: bool,
    /// Publish the served relay through a tunnel: `devtunnel` (found or created by labels) or
    /// `devtunnel:<tunnel id>`.
    #[arg(long, env = "IMESH_EXPOSE_RELAY", requires = "serve_relay")]
    pub expose_relay: Option<Expose>,
}

impl Opts {
    /// The settings as environment variables for the daemon.
    pub fn to_env(&self) -> BTreeMap<String, String> {
        let mut e = BTreeMap::new();
        let mut set = |k: &str, v: Option<String>| {
            if let Some(v) = v {
                e.insert(k.to_string(), v);
            }
        };
        let path = |p: &Option<PathBuf>| p.as_ref().map(|p| p.display().to_string());
        let join = self.relays.iter().map(|r| r.to_string()).collect::<Vec<_>>().join(",");
        set("IMESH_RELAYS", (!self.relays.is_empty()).then_some(join));
        set("IMESH_RELAY_TOKEN", self.relay_token.clone());
        set("IMESH_RELAY_CA", path(&self.relay_ca));
        set("IMESH_NO_N0", self.no_n0.then(|| "true".into()));
        set("IMESH_PKARR", self.pkarr.as_ref().map(|u| u.to_string()));
        set("IMESH_MDNS", self.mdns.then(|| "true".into()));
        set("IMESH_SERVE_RELAY", self.serve_relay.map(|a| a.to_string()));
        set("IMESH_SERVE_RELAY_TOKEN", self.serve_relay_token.clone());
        set("IMESH_SERVE_RELAY_CERT", path(&self.serve_relay_cert));
        set("IMESH_SERVE_RELAY_KEY", path(&self.serve_relay_key));
        set("IMESH_EXPOSE_RELAY", self.expose_relay.as_ref().map(|x| x.to_string()));
        e
    }
}

/// Relay URLs with the bearer token to present to each.
pub type Relays = BTreeMap<RelayUrl, Option<String>>;

pub fn relays(urls: &[RelayUrl], token: &Option<String>) -> Relays {
    urls.iter().map(|u| (u.clone(), token.clone())).collect()
}

pub fn config(url: &RelayUrl, token: &Option<String>) -> iroh::RelayConfig {
    let c = iroh::RelayConfig::from(url.clone());
    match token {
        Some(t) => c.with_auth_token(t.clone()),
        None => c,
    }
}

/// The endpoint settings that the daemon and the CLI share: preset, relays, extra CA roots, the
/// HTTP proxy from the environment, and name resolution through the OS. `relays: None` keeps the
/// preset's relays. `Some`, even empty, is exact and keeps a relay transport, so relays can be
/// added later.
pub fn endpoint_builder(no_n0: bool, relays: Option<&Relays>, ca: Option<&Path>) -> Result<iroh::endpoint::Builder> {
    use iroh::endpoint::presets;
    let mut b = if no_n0 { iroh::Endpoint::builder(presets::Minimal) } else { iroh::Endpoint::builder(presets::N0) };
    if let Some(relays) = relays {
        let map: iroh::RelayMap = relays.iter().map(|(u, t)| config(u, t)).collect();
        b = b.relay_mode(iroh::RelayMode::Custom(map));
    }
    if let Some(ca) = ca {
        b = b.ca_tls_config(iroh_relay::tls::CaTlsConfig::default().with_extra_roots(load_certs(ca)?));
    }
    Ok(b.proxy_from_env().dns_resolver(crate::net::dns::OsResolver::resolver()))
}

pub(crate) fn load_certs(path: &Path) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    use rustls::pki_types::pem::PemObject;
    rustls::pki_types::CertificateDer::pem_file_iter(path)
        .with_context(|| format!("read {}", path.display()))?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("parse {}", path.display()))
}
