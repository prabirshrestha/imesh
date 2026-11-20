//! An example app on imesh: the whole imesh CLI and daemon, plus its own ALPN and API methods.
//! It is built the way any app is: `iroh_mesh_cli::run(register)`.
//! - `example.dial` opens the example ALPN to another machine and returns what it echoes.
//! - `example.whoami` returns who called, to show a method that decides by caller.

use anyhow::{Context, Result};
use iroh::{
    EndpointId,
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler},
};
use serde::Deserialize;
use serde_json::{Value, json};

use iroh_mesh::daemon::{Builder, Caller, Machine};

pub const ALPN: &[u8] = b"imesh-example/echo/0";

fn register(b: Builder) -> Builder {
    b.protocol(ALPN, Echo).method("example.dial", dial).method("example.whoami", whoami)
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

async fn dial(m: Machine, _caller: Caller, p: Value) -> Result<Value> {
    let p: DialParams = iroh_mesh::api::params(p)?;
    let conn = m.connect(p.to, ALPN).await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(p.text.as_bytes()).await?;
    send.finish()?;
    let back = recv.read_to_end(64 << 10).await.context("read echo")?;
    conn.close(0u32.into(), b"");
    Ok(json!({ "echo": String::from_utf8_lossy(&back), "from": p.to }))
}

async fn whoami(_: Machine, caller: Caller, _: Value) -> Result<Value> {
    let who = match caller {
        Caller::Local => "local".to_string(),
        Caller::Member(id) => format!("member-{}", id.fmt_short()),
    };
    Ok(json!({ "who": who }))
}

fn main() -> std::process::ExitCode {
    iroh_mesh_cli::run(register)
}
