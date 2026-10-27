//! Reaching one machine of a cluster through ssh, iroh, or the local socket.

pub mod config;
pub mod ssh;

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use iroh::{Endpoint, EndpointAddr, EndpointId};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use self::{config::Connection, ssh::Ssh};
use crate::{
    api::{self, API_ALPN, Request},
    net,
};

const OPEN_TIMEOUT: Duration = Duration::from_secs(20);

pub enum Client {
    Local(std::path::PathBuf),
    Ssh(Ssh),
    Iroh { endpoint: Endpoint, conn: iroh::endpoint::Connection },
}

impl Client {
    /// Opens a connection and probes it with `ping`. The whole attempt has a deadline, so a hung
    /// daemon fails here and the next connection is tried.
    pub async fn open(c: &Connection) -> Result<Self> {
        tokio::time::timeout(OPEN_TIMEOUT, Self::open_inner(c)).await.context("no answer within 20 s")?
    }

    async fn open_inner(c: &Connection) -> Result<Self> {
        c.validate()?;
        let client = if let Some(p) = &c.local {
            Client::Local(p.clone())
        } else if let Some(dest) = &c.ssh {
            Client::Ssh(Ssh::new(dest, c.ssh_key_file.clone()))
        } else {
            let relays = c.relay.as_ref().map(|r| net::relays(std::slice::from_ref(r), &c.relay_token));
            let endpoint = net::endpoint_builder(c.no_n0, relays.as_ref(), c.relay_ca.as_deref())?
                .secret_key(config::client_key()?)
                .bind()
                .await?;
            let mut addr = EndpointAddr::new(c.iroh.unwrap());
            if let Some(r) = &c.relay {
                addr = addr.with_relay_url(r.clone());
            }
            for a in &c.addrs {
                addr = addr.with_ip_addr(*a);
            }
            let conn = endpoint.connect(addr, API_ALPN).await?;
            Client::Iroh { endpoint, conn }
        };
        client.call::<Value>(None, "ping", ()).await?;
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
            Client::Local(p) => api::framed(crate::net::local::connect(p).await?),
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

    /// Calls a method on the machine this client reaches, or on member `to` through it.
    pub async fn call<T: DeserializeOwned>(
        &self,
        to: Option<EndpointId>,
        method: &str,
        params: impl Serialize,
    ) -> Result<T> {
        let req = Request { to, method: method.to_string(), params: serde_json::to_value(params)?, via: None };
        let v = api::call(self.stream().await?, &req).await?;
        serde_json::from_value(v).with_context(|| format!("unexpected reply from {method}"))
    }

    pub async fn close(self) {
        match self {
            Client::Local(_) => {}
            Client::Ssh(ssh) => ssh.close().await,
            Client::Iroh { endpoint, conn } => {
                conn.close(0u32.into(), b"");
                endpoint.close().await;
            }
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
