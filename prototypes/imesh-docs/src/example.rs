//! An example app on top of imesh: one API method and one ALPN.
//! `example.dial` opens the example ALPN to another machine and returns what it echoes.

use anyhow::{Context, Result};
use iroh::{
    EndpointId,
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::daemon::{Builder, Machine};

pub const ALPN: &[u8] = b"imesh-example/echo/0";

pub fn register(b: Builder) -> Builder {
    b.protocol(ALPN, Echo).method("example.dial", dial)
}

#[derive(Debug, Clone)]
struct Echo;

impl ProtocolHandler for Echo {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let (mut send, mut recv) = conn.accept_bi().await?;
        tokio::io::copy(&mut recv, &mut send).await?;
        send.finish()?;
        conn.closed().await;
        Ok(())
    }
}

#[derive(Deserialize)]
struct DialParams {
    to: EndpointId,
    text: String,
}

async fn dial(m: Machine, p: Value) -> Result<Value> {
    let p: DialParams = crate::api::params(p)?;
    let conn = m.connect(p.to, ALPN).await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(p.text.as_bytes()).await?;
    send.finish()?;
    let back = recv.read_to_end(64 << 10).await.context("read echo")?;
    conn.close(0u32.into(), b"");
    Ok(json!({ "echo": String::from_utf8_lossy(&back), "from": p.to }))
}
