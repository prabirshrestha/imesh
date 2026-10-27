//! Calling the API of one machine: over its local socket or pipe, over iroh with a key the cluster
//! authorized, or over any stream that reaches its local API, for example SSH.

use std::{future::Future, path::PathBuf, pin::Pin};

use anyhow::{Context, Result};
use iroh::{Endpoint, EndpointAddr, EndpointId, endpoint::Connection};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::api::{self, API_ALPN, Io, Request};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Opens a new stream to a machine's API for each call. Implement it to reach a machine another
/// way, for example through SSH to a helper that bridges to its local API.
pub trait Dial: Send + Sync + 'static {
    fn open(&self) -> BoxFuture<'_, Result<Box<dyn Io>>>;

    /// Releases what the dialer holds. Called by [`Client::close`].
    fn close(self: Box<Self>) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

/// A connection to the API of one machine.
pub struct Client {
    dial: Box<dyn Dial>,
    tasks: Vec<n0_future::task::AbortOnDropHandle<()>>,
}

impl Client {
    pub fn new(dial: impl Dial) -> Self {
        Client { dial: Box::new(dial), tasks: Vec::new() }
    }

    /// The local API at `path`: a unix socket, or a named pipe on Windows.
    pub fn local(path: impl Into<PathBuf>) -> Self {
        Client::new(Local(path.into()))
    }

    /// Connects to a machine over iroh on `endpoint`, whose key must be a member of the cluster.
    pub async fn iroh(endpoint: Endpoint, addr: impl Into<EndpointAddr>) -> Result<Self> {
        let conn = endpoint.connect(addr, API_ALPN).await?;
        Ok(Client::new(Iroh { endpoint, conn }))
    }

    /// Keeps a transport task alive until this client closes or is dropped.
    pub fn with_task(mut self, task: n0_future::task::AbortOnDropHandle<()>) -> Self {
        self.tasks.push(task);
        self
    }

    /// Calls a method on the machine this client reaches, or on member `to` through it.
    pub async fn call<T: DeserializeOwned>(
        &self,
        to: Option<EndpointId>,
        method: &str,
        params: impl Serialize,
    ) -> Result<T> {
        let req = Request { to, method: method.to_string(), params: serde_json::to_value(params)? };
        let v = api::call(api::framed(self.dial.open().await?), &req).await?;
        serde_json::from_value(v).with_context(|| format!("unexpected reply from {method}"))
    }

    /// Opens a raw stream to `alpn` on member `to`, through a local daemon, with the daemon's
    /// machine key. The member sees this machine as the caller.
    pub async fn open(&self, to: EndpointId, alpn: &[u8]) -> Result<Box<dyn Io>> {
        let req = Request {
            to: None,
            method: api::OPEN_METHOD.into(),
            params: serde_json::json!({ "to": to, "alpn": String::from_utf8(alpn.to_vec())? }),
        };
        let mut s = api::framed(self.dial.open().await?);
        api::send(&mut s, &req).await?;
        match api::recv::<api::Reply>(&mut s).await?.context("stream closed before reply")? {
            api::Reply::Ok(_) => {}
            api::Reply::Err(e) => anyhow::bail!(e),
        }
        let parts = s.into_parts();
        anyhow::ensure!(parts.read_buf.is_empty(), "unexpected bytes after the open reply");
        Ok(parts.io)
    }

    /// Checks that the machine answers.
    pub async fn ping(&self) -> Result<()> {
        self.call::<Value>(None, "ping", ()).await.map(drop)
    }

    pub async fn close(self) {
        self.dial.close().await
    }
}

struct Local(PathBuf);

impl Dial for Local {
    fn open(&self) -> BoxFuture<'_, Result<Box<dyn Io>>> {
        Box::pin(crate::net::local::connect(&self.0))
    }
}

struct Iroh {
    endpoint: Endpoint,
    conn: Connection,
}

impl Dial for Iroh {
    fn open(&self) -> BoxFuture<'_, Result<Box<dyn Io>>> {
        Box::pin(async {
            let (send, recv) = self.conn.open_bi().await?;
            Ok(Box::new(tokio::io::join(recv, send)) as Box<dyn Io>)
        })
    }

    fn close(self: Box<Self>) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            self.conn.close(0u32.into(), b"");
            self.endpoint.close().await;
        })
    }
}
