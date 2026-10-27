//! Opening a [`Client`] for a [`Connection`] from the CLI config.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, ensure};
use imesh::{client::Client, net};
use iroh::EndpointAddr;

use crate::{
    config::{self, Connection},
    ssh::{Ssh, SshDial},
};

const OPEN_TIMEOUT: Duration = Duration::from_secs(20);

/// Opens a connection and probes it with `ping`. The whole attempt has a deadline, so a hung
/// daemon fails here and the next connection is tried.
pub async fn open(c: &Connection) -> Result<Client> {
    let timeout = if c.devtunnel.is_some() { Duration::from_secs(100) } else { OPEN_TIMEOUT };
    tokio::time::timeout(timeout, open_inner(c)).await.with_context(|| format!("no answer within {timeout:?}"))?
}

async fn open_inner(c: &Connection) -> Result<Client> {
    c.validate()?;
    let client = if let Some(p) = &c.local {
        Client::local(p)
    } else if let Some(dest) = &c.ssh {
        let ssh = Ssh::new(dest, c.ssh_key_file.clone());
        match &c.docker {
            Some(name) => {
                ensure!(crate::install::is_name(name), "bad container name {name:?}");
                Client::new(SshDial::container(ssh, name))
            }
            None => Client::new(SshDial::host(ssh)),
        }
    } else {
        let tunnel = c.devtunnel.as_deref().map(str::parse::<net::DevTunnel>).transpose()?;
        let forwarding = match &tunnel {
            Some(t) => Some(t.start().await?),
            None => None,
        };
        let relay = tunnel.as_ref().map(net::DevTunnel::relay_url).or_else(|| c.relay.clone());
        let relays = relay.as_ref().map(|r| net::relays(std::slice::from_ref(r), &c.relay_token));
        let proxy = if tunnel.is_some() { None } else { crate::args::proxy_from_env() };
        let endpoint = net::endpoint_builder(c.no_n0, relays.as_ref(), c.relay_ca.as_deref(), proxy.as_ref())?
            .secret_key(config::client_key()?)
            .bind()
            .await?;
        let mut addr = EndpointAddr::new(c.iroh.expect("validated"));
        if let Some(r) = &relay {
            addr = addr.with_relay_url(r.clone());
        }
        for a in &c.addrs {
            addr = addr.with_ip_addr(*a);
        }
        let client = Client::iroh(endpoint, addr).await?;
        match forwarding {
            Some(task) => client.with_task(task),
            None => client,
        }
    };
    if let Err(e) = client.ping().await {
        client.close().await;
        return Err(e);
    }
    Ok(client)
}

/// Tries each connection in order. Returns the first that answers.
pub async fn open_any(conns: &[Connection]) -> Result<(Client, &Connection)> {
    let mut errs = Vec::new();
    for c in conns {
        match open(c).await {
            Ok(client) => return Ok((client, c)),
            Err(e) => errs.push(format!("  {c}: {e:#}")),
        }
    }
    Err(anyhow!("no connection worked:\n{}", errs.join("\n")))
}
