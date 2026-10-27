//! The relay a machine can serve to members, for networks that block UDP.

use std::{net::SocketAddr, sync::Arc};

use anyhow::{Context, Result};
use iroh::EndpointId;
use iroh_relay::server::{
    Access, AccessControl, CertConfig, ClientRequest, RelayConfig, Server, ServerConfig, TlsConfig,
};
use tracing::info;

use super::load_certs;

/// Decides who may use the served relay: this machine and members.
#[derive(Debug)]
pub(crate) struct Gate {
    pub me: EndpointId,
    pub token: Option<String>,
    pub admission: crate::daemon::Admission,
}

impl AccessControl for Gate {
    async fn on_connect(&self, req: &ClientRequest) -> Access {
        let id = req.endpoint_id();
        if let Some(t) = &self.token
            && req.auth_token().as_deref() != Some(t.as_str())
        {
            return Access::Deny { reason: Some("bad or missing token".into()) };
        }
        if id == self.me || self.admission.allows(&id) {
            Access::Allow
        } else {
            Access::Deny { reason: Some("not a member of this cluster".into()) }
        }
    }
}

/// Starts the relay server. With a certificate it serves HTTPS on `addr`, and the plain HTTP
/// probe port on localhost.
pub(crate) async fn serve(opts: &super::Config, addr: SocketAddr, gate: Gate) -> Result<Server> {
    let mut relay;
    if let Some((cert, key)) = &opts.serve_relay_tls {
        let certs = load_certs(cert)?;
        let key = <rustls::pki_types::PrivateKeyDer as rustls::pki_types::pem::PemObject>::from_pem_file(key)
            .with_context(|| format!("read key {}", key.display()))?;
        let server_config =
            rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
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
