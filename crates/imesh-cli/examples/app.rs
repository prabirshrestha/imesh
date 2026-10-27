//! An example app on imesh: the whole imesh CLI and daemon, plus its own tables, an ALPN, and
//! API methods. It is built the way any app is: `imesh_cli::run(register)`.
//! - `example.dial` opens the example ALPN to another machine and returns what it echoes.
//! - `example.note` keeps a note in the app's table `example_notes`, one per caller kind.
//! - `kv.fill` and `kv.count` write and count many rows of `example_items` in one call, for the
//!   e2e suite.

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
    state::rusqlite::params,
};

pub const ALPN: &[u8] = b"imesh-example/echo/0";

/// The app's tables. A later version may add tables, columns, and indexes here.
const SCHEMA: &str = "
CREATE TABLE example_notes (
    who TEXT NOT NULL PRIMARY KEY,
    text TEXT NOT NULL DEFAULT ''
);
CREATE TABLE example_items (
    key TEXT NOT NULL PRIMARY KEY,
    value TEXT NOT NULL DEFAULT ''
);
";

fn register(b: Builder) -> Builder {
    b.schema(SCHEMA)
        .protocol(ALPN, Echo)
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

/// Writes through the in-process database. Only members and the local socket may write; a CLI
/// key over iroh may not, to show a method deciding by caller.
async fn note(m: Machine, caller: Caller, p: Value) -> Result<Value> {
    let p: NoteParams = imesh::api::params(p)?;
    let who = match caller {
        Caller::Local => "local".to_string(),
        Caller::Member(id) => format!("member-{}", id.fmt_short()),
        Caller::Client(_) => anyhow::bail!("CLI keys may not write notes"),
    };
    m.state()
        .write(|c| {
            c.execute(
                "INSERT INTO example_notes (who, text) VALUES (?1, ?2) ON CONFLICT (who) DO UPDATE SET text = excluded.text",
                params![who, p.text],
            )
        })
        .await?;
    Ok(json!({ "who": who }))
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
    m.state()
        .write(|c| {
            let mut st = c.prepare(
                "INSERT INTO example_items (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            )?;
            for i in 0..p.count {
                let value = format!("{i:06}{}", "x".repeat(p.bytes.saturating_sub(6)));
                st.execute(params![format!("{}/{i:06}", p.prefix), value])?;
            }
            Ok(())
        })
        .await?;
    Ok(json!(p.count))
}

async fn kv_count(m: Machine, _: Caller, p: Value) -> Result<Value> {
    let p: FillParams = imesh::api::params(p)?;
    let prefix = format!("{}/%", p.prefix);
    let n: i64 = m
        .state()
        .query(|c| c.query_row("SELECT count(*) FROM example_items WHERE key LIKE ?1", params![prefix], |r| r.get(0)))
        .await?;
    Ok(json!(n))
}

fn main() -> std::process::ExitCode {
    imesh_cli::run(register)
}
