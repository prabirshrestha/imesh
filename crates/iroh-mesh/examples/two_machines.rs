//! Two machines in one process, with no command line and no files outside a temporary directory:
//! one starts a cluster, admits the other, and each calls an app method on the other.
//!
//! ```bash
//! cargo run -p iroh-mesh --example two_machines
//! ```

use anyhow::Result;
use iroh_mesh::{
    api::{API_ALPN, Request},
    daemon::{Builder, Caller, Config, Running},
};
use serde_json::json;

async fn start(dir: &std::path::Path, name: &str) -> Result<Running> {
    let mut config = Config::new(dir.join(name));
    config.name = Some(name.into());
    // Both machines are in this process: no relays and no n0 lookup are needed, the default.
    Builder::new(config)
        .method("hello", |m, caller, _| async move { Ok(json!(format!("{} greets {caller:?}", m.name()))) })
        .spawn()
        .await
}

#[tokio::main]
async fn main() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let a = start(dir.path(), "a").await?;
    let b = start(dir.path(), "b").await?;

    let init = a.machine.init_cluster(None).await?;
    println!("cluster {}; keep the recovery code {}", init.cluster, init.recovery_code);
    let ticket = a.machine.admit(Caller::Local, b.machine.id(), "b", None).await?;
    let joined = b.machine.join(&ticket).await?;
    println!("b joined, {} members", joined.members);

    let conn = b.machine.connect(a.machine.id(), API_ALPN).await?;
    let (send, recv) = conn.open_bi().await?;
    let req = Request { to: None, method: "hello".into(), params: json!(null) };
    let reply = iroh_mesh::api::call(iroh_mesh::api::framed(tokio::io::join(recv, send)), &req).await?;
    println!("a answers b: {reply}");
    a.shutdown().await?;
    b.shutdown().await?;
    Ok(())
}
