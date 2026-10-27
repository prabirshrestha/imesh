use std::{
    collections::HashSet,
    sync::{Arc, RwLock},
    time::Duration,
};

use anyhow::{Result, ensure};
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayMode, SecretKey, Watcher,
    address_lookup::MemoryLookup,
    endpoint::{AfterHandshakeOutcome, Connection, EndpointHooks, presets},
    protocol::{AcceptError, ProtocolHandler, Router},
};
use iroh_gossip::{api::Event, net::Gossip, proto::TopicId};
use n0_future::StreamExt;

const ECHO_ALPN: &[u8] = b"poc/echo/0";

#[derive(Debug, Clone, Default)]
struct Members(Arc<RwLock<HashSet<EndpointId>>>);

impl EndpointHooks for Members {
    async fn after_handshake<'a>(&'a self, conn: &'a Connection) -> AfterHandshakeOutcome {
        if self.0.read().unwrap().contains(&conn.remote_id()) {
            AfterHandshakeOutcome::accept()
        } else {
            AfterHandshakeOutcome::Reject {
                error_code: 403u32.into(),
                reason: b"not a member".to_vec(),
            }
        }
    }
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

struct Node {
    router: Router,
    gossip: Gossip,
    members: Members,
    lookup: MemoryLookup,
}

async fn node(members: Members) -> Result<Node> {
    let lookup = MemoryLookup::new();
    let endpoint = Endpoint::builder(presets::Minimal)
        .secret_key(SecretKey::generate())
        .relay_mode(RelayMode::Disabled)
        .address_lookup(lookup.clone())
        .hooks(members.clone())
        .bind()
        .await?;
    let gossip = Gossip::builder().spawn(endpoint.clone());
    let router = Router::builder(endpoint)
        .accept(ECHO_ALPN, Echo)
        .accept(iroh_gossip::ALPN, gossip.clone())
        .spawn();
    Ok(Node { router, gossip, members, lookup })
}

async fn addr(n: &Node) -> EndpointAddr {
    let mut w = n.router.endpoint().watch_addr();
    loop {
        let a = w.get();
        if a.ip_addrs().next().is_some() {
            return a;
        }
        w.updated().await.unwrap();
    }
}

async fn echo(from: &Node, to: EndpointAddr, msg: &[u8]) -> Result<Vec<u8>> {
    let conn = from.router.endpoint().connect(to, ECHO_ALPN).await?;
    let (mut s, mut r) = conn.open_bi().await?;
    s.write_all(msg).await?;
    s.finish()?;
    let out = r.read_to_end(1024).await?;
    conn.close(0u32.into(), b"");
    Ok(out)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("warn").init();
    let nodes = [
        node(Members::default()).await?,
        node(Members::default()).await?,
        node(Members::default()).await?,
    ];
    let outsider = node(Members::default()).await?;
    let mut addrs = Vec::new();
    for n in &nodes {
        addrs.push(addr(n).await);
    }
    for n in nodes.iter().chain([&outsider]) {
        for a in &addrs {
            n.lookup.add_endpoint_info(a.clone());
        }
    }
    for n in &nodes {
        let mut m = n.members.0.write().unwrap();
        m.extend(addrs.iter().map(|a| a.id));
    }

    let got = echo(&nodes[0], addrs[1].clone(), b"hello").await?;
    ensure!(got == b"hello", "member echo over custom ALPN");
    println!("PASS member echo over custom ALPN, no relay, no n0 lookup");

    let denied = echo(&outsider, addrs[1].clone(), b"x").await;
    ensure!(denied.is_err(), "outsider must be rejected");
    println!("PASS outsider rejected: {:#}", denied.unwrap_err());

    let topic = TopicId::from_bytes([7; 32]);
    let ids: Vec<_> = addrs.iter().map(|a| a.id).collect();
    let mut subs = Vec::new();
    for (i, n) in nodes.iter().enumerate() {
        let peers = if i == 0 { vec![] } else { vec![ids[0]] };
        subs.push(n.gossip.subscribe(topic, peers).await?);
    }
    for s in subs.iter_mut().skip(1) {
        tokio::time::timeout(Duration::from_secs(10), s.joined()).await??;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    subs[2].broadcast("from-2".into()).await?;
    let mut got = 0;
    for s in subs.iter_mut().take(2) {
        let ev = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(Ok(Event::Received(m))) = s.next().await {
                    return m.content;
                }
            }
        })
        .await?;
        ensure!(&ev[..] == b"from-2");
        got += 1;
    }
    println!("PASS gossip broadcast reached {got} peers, node 1 joined only via node 0");

    for n in nodes.into_iter().chain([outsider]) {
        n.router.shutdown().await?;
    }
    Ok(())
}
