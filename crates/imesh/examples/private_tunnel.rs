//! Connect to an existing private dev tunnel with the current CLI login.
//! Usage: cargo run -p imesh --example private_tunnel -- devtunnel:<id>:<port>
//! The tunnel must serve an HTTP service. This checks forwarding and child cleanup.
use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::main]
async fn main() -> Result<()> {
    let tunnel: imesh::net::DevTunnel = std::env::args().nth(1).context("expected devtunnel:<id>:<port>")?.parse()?;
    let child = tunnel.start().await?;
    println!("forwarding started");
    let mut stream = tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, tunnel.port)).await?;
    println!("local TCP connected");
    stream.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").await?;
    let mut reply = String::new();
    let mut stream = BufReader::new(stream);
    tokio::time::timeout(std::time::Duration::from_secs(20), stream.read_line(&mut reply)).await??;
    println!("{}", reply.trim());
    anyhow::ensure!(reply.split_whitespace().nth(1) == Some("200"), "HTTP service did not return 200");
    drop(stream);
    drop(child);
    for _ in 0..50 {
        if std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, tunnel.port)).is_ok() {
            println!("PASS forwarding and child cleanup");
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    anyhow::bail!("forwarded port remained open after child stopped")
}
