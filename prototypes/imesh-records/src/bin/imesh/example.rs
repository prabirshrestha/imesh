//! An example app on top of imesh: an ALPN and two API methods.
//! - `example.dial` opens the example ALPN to another machine and returns what it echoes.
//! - `kv.fill` and `kv.count` write and count many keys in one call, for the e2e suite.
//! - `example.note` keeps a note in the replicated state under `app/example/`, one per caller kind.

use anyhow::{Context, Result};
use iroh::{
    EndpointId,
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler},
};
use serde::Deserialize;
use serde_json::{Value, json};

use imesh::{
    daemon::{Builder, Caller, Machine},
    state::keys,
};

pub const ALPN: &[u8] = b"imesh-example/echo/0";

pub fn register(b: Builder) -> Builder {
    b.protocol(ALPN, Echo)
        .method("example.dial", dial)
        .method("example.note", note)
        .method("kv.fill", kv_fill)
        .method("kv.count", kv_count)
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
    let p: DialParams = imesh::api::params(p)?;
    let conn = m.connect(p.to, ALPN).await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(p.text.as_bytes()).await?;
    send.finish()?;
    let back = recv.read_to_end(64 << 10).await.context("read echo")?;
    conn.close(0u32.into(), b"");
    Ok(json!({ "echo": String::from_utf8_lossy(&back), "from": p.to }))
}

#[derive(Deserialize)]
struct NoteParams {
    text: String,
}

/// Writes through the in-process state API. Only members and the local socket may write; a CLI
/// key over iroh may not, to show a method deciding by caller.
async fn note(m: Machine, caller: Caller, p: Value) -> Result<Value> {
    let p: NoteParams = imesh::api::params(p)?;
    let who = match caller {
        Caller::Local => "local".to_string(),
        Caller::Member(id) => format!("member-{}", id.fmt_short()),
        Caller::Client(_) => anyhow::bail!("CLI keys may not write notes"),
    };
    let key = format!("app/example/{who}");
    m.state().put(&key, Some(&p.text))?;
    Ok(json!({ "key": key }))
}

// Test helpers for the e2e suite: fill and count many keys in one call.

#[derive(Deserialize)]
struct FillParams {
    prefix: String,
    #[serde(default)]
    count: usize,
    #[serde(default)]
    bytes: usize,
}

async fn kv_fill(m: Machine, _: Caller, p: Value) -> Result<Value> {
    let p: FillParams = imesh::api::params(p)?;
    for i in 0..p.count {
        let value = format!("{i:06}{}", "x".repeat(p.bytes.saturating_sub(6)));
        m.state().put(&keys::app(&format!("{}/{i:06}", p.prefix)), Some(&value))?;
    }
    Ok(json!(p.count))
}

async fn kv_count(m: Machine, _: Caller, p: Value) -> Result<Value> {
    let p: FillParams = imesh::api::params(p)?;
    Ok(json!(m.state().live(&keys::app(&format!("{}/", p.prefix))).len()))
}
