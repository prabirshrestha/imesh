//! Opening a [`Client`] for a [`Connection`] from the CLI config.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, ensure};
use iroh_mesh::{client::Client, net};

use crate::{
    config::Connection,
    ssh::{Ssh, SshDial},
};

const OPEN_TIMEOUT: Duration = Duration::from_secs(20);

/// Opens a connection and probes it with `ping`. The whole attempt has a deadline, so a hung
/// daemon fails here and the next connection is tried.
pub async fn open(c: &Connection) -> Result<Client> {
    let timeout = if c.ssh_tunnel.is_some() { Duration::from_secs(100) } else { OPEN_TIMEOUT };
    tokio::time::timeout(timeout, open_inner(c)).await.with_context(|| format!("no answer within {timeout:?}"))?
}

async fn open_inner(c: &Connection) -> Result<Client> {
    c.validate()?;
    let client = if let Some(p) = &c.local {
        Client::local(p)
    } else if let Some(dest) = &c.ssh {
        let tunnel = c.ssh_tunnel.as_deref().map(str::parse::<net::DevTunnel>).transpose()?;
        let ssh = Ssh::new(dest, c.ssh_key_file.clone()).with_tunnel(tunnel);
        if c.platform.as_deref() == Some("windows") {
            ensure!(c.docker.is_none(), "docker is for a Linux host");
            return finish(Client::new(SshDial::windows(ssh))).await;
        }
        match &c.docker {
            Some(name) => {
                ensure!(crate::install::is_name(name), "bad container name {name:?}");
                Client::new(SshDial::container(ssh, name))
            }
            None => Client::new(SshDial::host(ssh)),
        }
    } else {
        unreachable!("validated: ssh or local")
    };
    finish(client).await
}

async fn finish(client: Client) -> Result<Client> {
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
