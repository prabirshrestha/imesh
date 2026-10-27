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

pub use devtunnel::{Connect as DevTunnel, Expose};

/// How a machine connects: its relays, address lookup, and the relay it serves. The default
/// uses the n0 relays and n0 address lookup.
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// Relays to use instead of the n0 relays.
    pub relays: Vec<RelayUrl>,
    /// Bearer token to present to `relays`.
    pub relay_token: Option<String>,
    /// Extra CA certificates (PEM file) to trust for relay TLS, for relays with private certificates.
    pub relay_ca: Option<PathBuf>,
    /// No n0 relays and no n0 address lookup.
    pub no_n0: bool,
    /// Also publish and resolve addresses through this pkarr server (a self-hosted
    /// iroh-dns-server), for example `https://dns.example.com/pkarr`.
    pub pkarr: Option<url::Url>,
    /// Also find machines on the same LAN with mDNS.
    pub mdns: bool,
    /// Run a members-only iroh relay on this TCP address, for example `0.0.0.0:3340`. Members,
    /// authorized CLI keys, and this machine may use it.
    pub serve_relay: Option<SocketAddr>,
    /// Also require this bearer token on the served relay.
    pub serve_relay_token: Option<String>,
    /// Serve the relay over HTTPS with this certificate chain and key (PEM files).
    pub serve_relay_tls: Option<(PathBuf, PathBuf)>,
    /// Publish the served relay through a tunnel.
    pub expose_relay: Option<Expose>,
    /// Forward a private relay through the logged-in devtunnel CLI.
    pub devtunnel: Option<DevTunnel>,
    /// HTTP proxy for relay traffic.
    pub proxy: Option<url::Url>,
    /// Open no UDP port: every connection goes out to a relay over HTTPS, and nothing listens for
    /// iroh on this machine. Slower, and all traffic uses relay bandwidth.
    pub relay_only: bool,
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        if let Some(tunnel) = &self.devtunnel {
            anyhow::ensure!(self.relays == [tunnel.relay_url()], "devtunnel needs exactly its loopback relay URL");
            anyhow::ensure!(
                self.serve_relay.is_none() && self.proxy.is_none(),
                "a devtunnel client cannot also serve a relay or use an HTTP proxy"
            );
        }
        if self.expose_relay.is_some() {
            anyhow::ensure!(self.proxy.is_none(), "a private devtunnel host must reach its relay directly on loopback");
            anyhow::ensure!(
                self.serve_relay.is_some_and(|a| a.ip() == std::net::Ipv4Addr::LOCALHOST && a.port() != 0),
                "devtunnel exposure needs 127.0.0.1 and a fixed nonzero port"
            );
            anyhow::ensure!(self.serve_relay_tls.is_none(), "devtunnel forwards the plain HTTP loopback relay");
        }
        anyhow::ensure!(
            !(self.relay_only && self.no_n0 && self.relays.is_empty()),
            "relay_only with no_n0 needs relays: the machine would have no way to connect"
        );
        anyhow::ensure!(!(self.relay_only && self.mdns), "mDNS finds direct addresses, and relay_only has none");
        anyhow::ensure!(
            self.serve_relay.is_some()
                || (self.serve_relay_token.is_none() && self.serve_relay_tls.is_none() && self.expose_relay.is_none()),
            "a served relay token, certificate, or exposure needs serve_relay"
        );
        Ok(())
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

/// The endpoint settings that the daemon and a client share: preset, relays, extra CA roots, an
/// HTTP proxy, and name resolution through the OS. `relays: None` keeps the preset's relays.
/// `Some`, even empty, is exact and keeps a relay transport, so relays can be added later.
pub fn endpoint_builder(
    no_n0: bool,
    relays: Option<&Relays>,
    ca: Option<&Path>,
    proxy: Option<&url::Url>,
) -> Result<iroh::endpoint::Builder> {
    use iroh::endpoint::presets;
    let mut b = if no_n0 { iroh::Endpoint::builder(presets::Minimal) } else { iroh::Endpoint::builder(presets::N0) };
    if let Some(relays) = relays {
        let map: iroh::RelayMap = relays.iter().map(|(u, t)| config(u, t)).collect();
        b = b.relay_mode(iroh::RelayMode::Custom(map));
    }
    if let Some(ca) = ca {
        b = b.ca_tls_config(iroh_relay::tls::CaTlsConfig::default().with_extra_roots(load_certs(ca)?));
    }
    if let Some(proxy) = proxy {
        b = b.proxy_url(proxy.clone());
    }
    Ok(b.dns_resolver(crate::net::dns::OsResolver::resolver()))
}

pub(crate) fn load_certs(path: &Path) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    use rustls::pki_types::pem::PemObject;
    rustls::pki_types::CertificateDer::pem_file_iter(path)
        .with_context(|| format!("read {}", path.display()))?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("parse {}", path.display()))
}
