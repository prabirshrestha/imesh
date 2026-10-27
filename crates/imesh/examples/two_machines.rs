//! Two machines in one process, with no command line and no files outside a temporary directory:
//! one starts a cluster, admits the other, and a row written on one appears on the other.
//!
//! Runs on Linux and Windows. macOS cannot run a machine: Corrosion needs one loopback address
//! per machine in 127.0.0.0/8, and macOS answers only 127.0.0.1.
//!
//! ```bash
//! cargo run -p imesh --example two_machines
//! ```

use std::time::Duration;

use anyhow::{Result, bail};
use imesh::{
    daemon::{Builder, Config, Running},
    state::rusqlite::OptionalExtension,
};

const SCHEMA: &str = "CREATE TABLE notes (id TEXT NOT NULL PRIMARY KEY, body TEXT NOT NULL DEFAULT '');";

async fn start(dir: &std::path::Path, name: &str) -> Result<Running> {
    let mut config = Config::new(dir.join(name));
    config.name = Some(name.into());
    // Both machines are in this process: no relays and no n0 lookup are needed.
    config.net.no_n0 = true;
    Builder::new(config).schema(SCHEMA).spawn().await
}

#[tokio::main]
async fn main() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let a = start(dir.path(), "a").await?;
    let b = start(dir.path(), "b").await?;

    let cluster = a.machine.init_cluster(None).await?;
    let ticket = a.machine.admit(b.machine.id(), "b").await?;
    let joined = b.machine.join(&ticket).await?;
    println!("cluster {cluster}: b joined, {} members", joined.members);

    a.machine.state().write(|c| c.execute("INSERT INTO notes (id, body) VALUES ('hello', 'from a')", [])).await?;
    for _ in 0..50 {
        let body: Option<String> = b
            .machine
            .state()
            .query(|c| c.query_row("SELECT body FROM notes WHERE id = 'hello'", [], |r| r.get(0)).optional())
            .await?;
        if let Some(body) = body {
            println!("b reads {body:?}");
            a.shutdown().await?;
            b.shutdown().await?;
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("the row did not reach b")
}
