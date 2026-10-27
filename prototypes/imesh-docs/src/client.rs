//! Reaching one machine of a cluster through ssh, iroh, or the local socket.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, endpoint::presets};
use serde_json::Value;

use crate::{
    api::{self, API_ALPN, Request},
    config::{self, Connection},
    ssh::Ssh,
};

pub enum Client {
    Local(std::path::PathBuf),
    Ssh(Ssh),
    Iroh {
        endpoint: Endpoint,
        conn: iroh::endpoint::Connection,
    },
}

impl Client {
    pub async fn open(c: &Connection) -> Result<Self> {
        c.validate()?;
        let client = if let Some(p) = &c.local {
            Client::Local(p.clone())
        } else if let Some(dest) = &c.ssh {
            Client::Ssh(Ssh::new(dest, c.ssh_key_file.clone()))
        } else {
            let id = c.iroh.unwrap();
            let mut b = Endpoint::builder(presets::N0)
                .dns_resolver(crate::dns::OsResolver::resolver())
                .secret_key(config::client_key()?);
            if let Some(r) = &c.relay {
                b = b.relay_mode(RelayMode::custom([r.clone()]));
            }
            let endpoint = b.bind().await?;
            let mut addr = EndpointAddr::new(id);
            if let Some(r) = &c.relay {
                addr = addr.with_relay_url(r.clone());
            }
            for a in &c.addrs {
                addr = addr.with_ip_addr(*a);
            }
            let conn = tokio::time::timeout(Duration::from_secs(20), endpoint.connect(addr, API_ALPN))
                .await
                .context("timeout")??;
            Client::Iroh { endpoint, conn }
        };
        // A probe call, so a dead connection fails here and the next one is tried.
        client.call(None, "ping", Value::Null).await?;
        Ok(client)
    }

    /// Tries each connection in order. Returns the first that answers.
    pub async fn open_any(conns: &[Connection]) -> Result<(Self, &Connection)> {
        let mut errs = Vec::new();
        for c in conns {
            match Client::open(c).await {
                Ok(client) => return Ok((client, c)),
                Err(e) => errs.push(format!("  {c}: {e:#}")),
            }
        }
        Err(anyhow!("no connection worked:\n{}", errs.join("\n")))
    }

    async fn stream(&self) -> Result<api::Stream> {
        Ok(match self {
            Client::Local(p) => api::framed(crate::local::connect(p).await?),
            Client::Ssh(ssh) => {
                let mut child = ssh.stdio("imesh dial-stdio")?;
                let out = child.stdout.take().unwrap();
                let inp = child.stdin.take().unwrap();
                // The child is killed when the stream is dropped.
                api::framed(ChildIo { _child: child, io: tokio::io::join(out, inp) })
            }
            Client::Iroh { conn, .. } => {
                let (send, recv) = conn.open_bi().await?;
                api::framed(tokio::io::join(recv, send))
            }
        })
    }

    pub async fn call(&self, to: Option<EndpointId>, method: &str, params: Value) -> Result<Value> {
        let req = Request { to, method: method.to_string(), params };
        let (_items, v) = api::call(self.stream().await?, &req).await?;
        Ok(v)
    }

    pub async fn close(self) {
        if let Client::Iroh { endpoint, conn } = self {
            conn.close(0u32.into(), b"");
            endpoint.close().await;
        }
    }
}

struct ChildIo<T> {
    _child: tokio::process::Child,
    io: T,
}

impl<T: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for ChildIo<T> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl<T: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for ChildIo<T> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.io).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_shutdown(cx)
    }
}
