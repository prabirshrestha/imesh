//! `imesh invite` and `imesh up --join <code>`: pair a machine that you can open a terminal on.
//! Nothing is copied: the new machine already runs a daemon (`imesh up`). The same three steps
//! (hello, answer, done) go over one of two channels:
//!
//! - iroh: a temporary iroh endpoint on the inviting computer, for machines that can reach each
//!   other (same LAN, a public address, or a shared relay). The code holds its ticket.
//! - devtunnel (`--devtunnel`): a private, temporary devtunnel. Each side uses its own devtunnel
//!   login, and the devtunnel service refuses a request without a valid connect token (HTTP 401)
//!   before it reaches this computer.
//!
//! Both sides print the new machine's short ID, and the operator compares them before it is
//! admitted. iroh then checks that key on every connection.

use std::{process::Stdio, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use iroh_mesh::{
    api::{self, Inspect, JoinTicket, Joined},
    client::Client,
    state::TunnelEndpoint,
};

use crate::{cli::At, config::Connection, output::Output};

const PREFIX: &str = "imeshpair1-dt-";
/// The prefix of a code from a cluster in the devtunnel mesh: the new machine joins the mesh too.
const MESH_PREFIX: &str = "imeshpair1-dtm-";
/// The prefix of a code for pairing over iroh: the base32 of an iroh endpoint ticket follows.
const IROH_PREFIX: &str = "imeshpair1-ir-";
/// The ALPN of the temporary pairing endpoint.
const PAIR_ALPN: &[u8] = b"imesh/pair/0";
/// How long `invite` waits for the new machine.
const PAIR_TIME: Duration = Duration::from_secs(15 * 60);

/// What the new machine sends: its address, name, platform, and mesh tunnel.
#[derive(Debug, Serialize, Deserialize)]
struct Hello {
    ticket: String,
    name: String,
    os: String,
    arch: String,
    #[serde(default)]
    tunnel: Option<TunnelEndpoint>,
    #[serde(default)]
    gossip_port_base: Option<u16>,
}

/// What the operator sends back.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
enum Answer {
    /// Join this cluster.
    Accepted {
        join: String,
    },
    Refused {
        reason: String,
    },
}

/// What the new machine reports after it joined or started the cluster.
#[derive(Debug, Serialize, Deserialize)]
struct Done {
    #[serde(default)]
    cluster: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

/// A pairing code: where the inviting computer waits, and whether the cluster uses the devtunnel
/// mesh. Only a devtunnel code can be a mesh code.
#[derive(Debug, PartialEq)]
pub struct Code {
    via: Via,
    pub mesh: bool,
}

#[derive(Debug, PartialEq)]
enum Via {
    /// The ID of a private devtunnel.
    Devtunnel(String),
    /// The address of a temporary iroh endpoint.
    Iroh(iroh::EndpointAddr),
}

impl std::fmt::Display for Code {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.via {
            Via::Devtunnel(tunnel) => write!(f, "{}{tunnel}", if self.mesh { MESH_PREFIX } else { PREFIX }),
            Via::Iroh(addr) => {
                let ticket = iroh_tickets::endpoint::EndpointTicket::new(addr.clone()).to_string();
                write!(f, "{IROH_PREFIX}{}", ticket.strip_prefix("endpoint").unwrap_or(&ticket))
            }
        }
    }
}

impl std::str::FromStr for Code {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        let s = s.trim();
        if let Some(t) = s.strip_prefix(IROH_PREFIX) {
            ensure!(t.bytes().all(|c| c.is_ascii_alphanumeric()), "bad ticket in the pairing code");
            let ticket: iroh_tickets::endpoint::EndpointTicket =
                format!("endpoint{t}").parse().context("bad ticket in the pairing code")?;
            return Ok(Code { via: Via::Iroh(ticket.endpoint_addr().clone()), mesh: false });
        }
        let (tunnel, mesh) = match s.strip_prefix(MESH_PREFIX) {
            Some(t) => (t, true),
            None => (s.strip_prefix(PREFIX).context("not an imesh pairing code")?, false),
        };
        ensure!(
            !tunnel.is_empty()
                && tunnel.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'.')
                && tunnel.as_bytes()[0].is_ascii_alphanumeric(),
            "bad tunnel ID in the pairing code"
        );
        Ok(Code { via: Via::Devtunnel(tunnel.into()), mesh })
    }
}

/// Runs `work` while a spinner on stderr shows `what` and the seconds so far. Without a terminal
/// it prints `what` once.
async fn spin<T>(what: &str, work: impl std::future::Future<Output = T>) -> T {
    use std::io::{IsTerminal, Write};
    if !std::io::stderr().is_terminal() {
        eprintln!("{what}...");
        return work.await;
    }
    let start = std::time::Instant::now();
    let ticker = async {
        let frames = ['|', '/', '-', '\\'];
        let mut i = 0;
        loop {
            eprint!("\r{} {what} ({}s) ", frames[i % frames.len()], start.elapsed().as_secs());
            let _ = std::io::stderr().flush();
            i += 1;
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    };
    let out = tokio::select! {
        out = work => out,
        _ = ticker => unreachable!("the ticker never ends"),
    };
    eprintln!("\r{what} ({}s), done", start.elapsed().as_secs());
    out
}

/// The short form of a machine ID that both sides print, for the operator to compare.
fn short(id: &iroh::EndpointId) -> String {
    let h = id.to_string();
    format!("{}-{}", &h[..5], &h[5..10])
}

async fn dt(args: &[&str]) -> Result<String> {
    let out = tokio::time::timeout(
        Duration::from_secs(45),
        tokio::process::Command::new("devtunnel").args(args).stdin(Stdio::null()).kill_on_drop(true).output(),
    )
    .await
    .context("devtunnel timed out")?
    .context("run devtunnel: install it and run `devtunnel user login`")?;
    ensure!(
        out.status.success(),
        "devtunnel {} failed: {}{}",
        args[0],
        String::from_utf8_lossy(&out.stdout).trim(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The JSON object in devtunnel's output. The CLI can print a welcome banner before it.
fn json_of(text: &str) -> Result<Value> {
    let start = text.find('{').context("devtunnel printed no JSON")?;
    let end = text.rfind('}').context("devtunnel printed no JSON")?;
    serde_json::from_str(&text[start..=end]).context("read devtunnel JSON")
}

async fn require_login() -> Result<()> {
    let v = json_of(&dt(&["user", "show", "--json"]).await?).unwrap_or(Value::Null);
    ensure!(v["status"] == "Logged in", "devtunnel is not logged in: run `devtunnel user login`");
    Ok(())
}

/// The temporary tunnel that `invite` owns. It is deleted on drop.
struct Tunnel {
    id: String,
    host: Option<tokio::process::Child>,
    deleted: bool,
}

impl Tunnel {
    async fn create(port: u16) -> Result<Self> {
        // devtunnel expires a tunnel after whole hours; invite itself stops after 15 minutes and
        // deletes the tunnel then, also after Ctrl-C.
        let v = json_of(&dt(&["create", "--expiration", "1h", "--description", "imesh pairing", "--json"]).await?)?;
        ensure!(!anonymous(&v), "devtunnel created a tunnel with anonymous access; refusing to pair through it");
        let id = v
            .pointer("/tunnel/tunnelId")
            .or_else(|| v.get("tunnelId"))
            .and_then(Value::as_str)
            .context("devtunnel create returned no tunnel ID")?
            .to_string();
        let mut t = Tunnel { id, host: None, deleted: false };
        dt(&["port", "create", &t.id, "--port-number", &port.to_string(), "--protocol", "http"]).await?;
        let mut child = tokio::process::Command::new("devtunnel")
            .args(["host", &t.id])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("start devtunnel host")?;
        let mut out = BufReader::new(child.stdout.take().expect("piped")).lines();
        let mut err = BufReader::new(child.stderr.take().expect("piped")).lines();
        let (mut o, mut e) = (true, true);
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let line = tokio::select! {
                    l = out.next_line(), if o => { let l = l?; o = l.is_some(); l }
                    l = err.next_line(), if e => { let l = l?; e = l.is_some(); l }
                    s = child.wait() => bail!("devtunnel host exited: {}", s?),
                };
                if line.is_some_and(|l| l.contains("Ready to accept connections for tunnel:")) {
                    return Ok(());
                }
            }
        })
        .await
        .context("devtunnel host did not become ready within 60 seconds")??;
        tokio::spawn(async move { while let Ok(Some(_)) = out.next_line().await {} });
        tokio::spawn(async move { while let Ok(Some(_)) = err.next_line().await {} });
        t.host = Some(child);
        Ok(t)
    }

    async fn close(&mut self) {
        if let Some(mut c) = self.host.take() {
            let _ = c.kill().await;
            let _ = c.wait().await;
        }
        self.deleted = dt(&["delete", &self.id, "--force"]).await.is_ok();
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        if !self.deleted {
            let _ = std::process::Command::new("devtunnel")
                .args(["delete", &self.id, "--force"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

/// Whether devtunnel output grants anonymous access anywhere.
fn anonymous(v: &Value) -> bool {
    match v {
        Value::Object(o) => {
            o.get("type").and_then(Value::as_str).is_some_and(|t| t.eq_ignore_ascii_case("anonymous"))
                || o.values().any(anonymous)
        }
        Value::Array(a) => a.iter().any(anonymous),
        _ => false,
    }
}

/// One request to the pairing server: method, path, and body. The devtunnel service already
/// refused every request without a valid connect token (HTTP 401) before it got here.
struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
}

/// Reads one HTTP request with a small JSON body.
async fn read_request(s: &mut tokio::net::TcpStream) -> Result<Request> {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        ensure!(head.len() < 16384, "request header too large");
        head.push(s.read_u8().await?);
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let mut words = head.split_whitespace();
    let method = words.next().unwrap_or_default().to_string();
    let path = words.next().unwrap_or_default().to_string();
    let len: usize = head
        .lines()
        .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("content-length")))
        .map(|(_, v)| v.trim().parse())
        .transpose()?
        .unwrap_or(0);
    ensure!(len <= 65536, "request body too large");
    let mut body = vec![0; len];
    s.read_exact(&mut body).await?;
    Ok(Request { method, path, body })
}

async fn respond(s: &mut tokio::net::TcpStream, status: &str, body: &[u8]) -> Result<()> {
    s.write_all(
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .as_bytes(),
    )
    .await?;
    s.write_all(body).await?;
    s.shutdown().await?;
    Ok(())
}

/// The result of `invite`.
#[derive(Debug, Serialize)]
pub struct Invited {
    pub transport: &'static str,
    pub id: iroh::EndpointId,
    pub name: String,
    pub os: String,
    pub arch: String,
    pub cluster: String,
}

/// Who admits the new machine.
enum Admitter {
    /// The daemon on this computer, a member.
    Local(Client),
    /// The context's entry machine, over SSH. This computer runs no daemon.
    Entry(Client),
}

impl Admitter {
    fn client(&self) -> &Client {
        match self {
            Admitter::Local(c) | Admitter::Entry(c) => c,
        }
    }
}

/// Decides who admits, as [`At::client`] does: the named context, else the local member daemon,
/// else the current context.
async fn admitter(at: &At) -> Result<Admitter> {
    if at.context.is_none()
        && let Ok(client) = crate::connect::open(&Connection::local()).await
    {
        match client.call::<Inspect>(None, "machine.inspect", ()).await {
            Ok(i) if i.cluster.is_some() => return Ok(Admitter::Local(client)),
            _ => client.close().await,
        }
    }
    Ok(Admitter::Entry(at.client().await?))
}

/// `imesh invite`: waits for one machine, over a temporary iroh endpoint or, with `devtunnel`, a
/// private devtunnel, and admits it.
pub async fn invite(at: &At, name: String, devtunnel: bool, yes: bool, out: &Output) -> Result<()> {
    if devtunnel {
        require_login().await?;
    }
    // Decide who admits first, so a broken daemon or context fails before any tunnel exists.
    let admitter = admitter(at).await?;
    let entry = admitter.client().call::<Inspect>(None, "machine.inspect", ()).await?;
    let result = async {
        let (server, code) = Server::open(devtunnel, &entry).await?;
        eprintln!(
            "\nPairing over {}. It ends when this command exits, or after 15 minutes.\n\
             On the new machine, run:\n\n  imesh up --join {code}\n\nWaiting for {name}...\n",
            server.what()
        );
        // Ctrl-C ends the wait; the tunnel is deleted below either way.
        let r = tokio::select! {
            r = serve(&server, &name, yes, &admitter) => r,
            _ = tokio::signal::ctrl_c() => Err(anyhow::anyhow!("pairing canceled")),
        };
        server.close().await;
        r
    }
    .await;
    match admitter {
        Admitter::Local(client) | Admitter::Entry(client) => client.close().await,
    }
    let r = result?;
    out.print(&r, |r| println!("{} ({}, {}/{}) is in cluster {}", r.name, r.id, r.os, r.arch, r.cluster))
}

/// A temporary endpoint for iroh pairing, reachable the way the machine `like` is: the same n0
/// setting and the same relays.
async fn temp_endpoint(like: &Inspect, alpns: Vec<Vec<u8>>) -> Result<iroh::Endpoint> {
    let relays = (!like.relays.is_empty()).then(|| iroh_mesh::net::relays(&like.relays, &None));
    let endpoint = iroh_mesh::net::endpoint_builder(like.n0, relays.as_ref(), None, None)?
        .secret_key(iroh::SecretKey::generate())
        .alpns(alpns)
        .bind()
        .await?;
    Ok(endpoint)
}

/// Where `invite` waits for the new machine.
#[allow(clippy::large_enum_variant)] // one per invite
enum Server {
    Devtunnel { listener: tokio::net::TcpListener, tunnel: Tunnel },
    Iroh(iroh::Endpoint),
}

/// One new machine that sent its hello, waiting for the answer.
#[allow(clippy::large_enum_variant)] // one per invite
enum Peer {
    /// The open HTTP request. The devtunnel service already refused every request without a
    /// valid connect token (HTTP 401) before it got here.
    Http(tokio::net::TcpStream),
    /// The pairing stream. The connection must stay open while the stream is in use.
    Iroh(#[allow(dead_code)] iroh::endpoint::Connection, api::Stream),
}

impl Server {
    async fn open(devtunnel: bool, entry: &Inspect) -> Result<(Server, Code)> {
        if devtunnel {
            let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
            eprintln!("creating a private devtunnel for pairing");
            let tunnel = Tunnel::create(listener.local_addr()?.port()).await?;
            let code = Code { via: Via::Devtunnel(tunnel.id.clone()), mesh: entry.devtunnel_mesh };
            return Ok((Server::Devtunnel { listener, tunnel }, code));
        }
        let endpoint = temp_endpoint(entry, vec![PAIR_ALPN.to_vec()]).await?;
        // Wait a little for a home relay, so the code holds it. Without relays, the direct
        // addresses are enough on a LAN.
        if entry.n0 || !entry.relays.is_empty() {
            let _ = tokio::time::timeout(Duration::from_secs(10), endpoint.online()).await;
        }
        let code = Code { via: Via::Iroh(endpoint.addr()), mesh: false };
        Ok((Server::Iroh(endpoint), code))
    }

    fn what(&self) -> String {
        match self {
            Server::Devtunnel { tunnel, .. } => format!("devtunnel {}", tunnel.id),
            Server::Iroh(e) => format!("iroh (temporary endpoint {})", short(&e.id())),
        }
    }

    fn transport(&self) -> &'static str {
        match self {
            Server::Devtunnel { .. } => "devtunnel",
            Server::Iroh(_) => "iroh",
        }
    }

    async fn close(self) {
        match self {
            Server::Devtunnel { mut tunnel, .. } => tunnel.close().await,
            Server::Iroh(e) => e.close().await,
        }
    }

    /// The next valid hello. A request that is broken or slow is dropped, and the server keeps
    /// waiting, one machine at a time.
    async fn hello(&self, deadline: tokio::time::Instant) -> Result<(Peer, Hello, iroh::EndpointAddr)> {
        loop {
            let (mut peer, body) = match self {
                Server::Devtunnel { listener, .. } => {
                    let (mut s, req) = next_request(listener, deadline).await?;
                    if req.path != "/pair" {
                        let _ = respond(&mut s, "404 Not Found", b"{}").await;
                        continue;
                    }
                    (Peer::Http(s), req.body)
                }
                Server::Iroh(endpoint) => {
                    let incoming = tokio::time::timeout_at(deadline, endpoint.accept())
                        .await
                        .context("the pairing code expired")?
                        .context("the pairing endpoint closed")?;
                    let Ok(Ok(Some(peer))) = tokio::time::timeout(Duration::from_secs(30), async {
                        let conn = incoming.await?;
                        let (send, recv) = conn.accept_bi().await?;
                        let mut s = api::framed(tokio::io::join(recv, send));
                        let body = api::recv::<Value>(&mut s).await?;
                        anyhow::Ok(body.map(|b| (Peer::Iroh(conn, s), serde_json::to_vec(&b).unwrap_or_default())))
                    })
                    .await
                    else {
                        continue;
                    };
                    peer
                }
            };
            match parse_hello(&body) {
                Some((hello, addr)) => return Ok((peer, hello, addr)),
                None => {
                    let _ = peer.answer(&Answer::Refused { reason: "bad hello".into() }).await;
                }
            }
        }
    }

    /// Waits for the new machine to report that it joined, or why not.
    async fn done(&self, peer: Peer, deadline: tokio::time::Instant) -> Result<Done> {
        match (self, peer) {
            (Server::Iroh(_), Peer::Iroh(_conn, mut s)) => tokio::time::timeout_at(deadline, api::recv(&mut s))
                .await
                .context("the pairing code expired")??
                .context("the new machine closed the pairing connection"),
            (Server::Devtunnel { listener, .. }, _) => loop {
                let (mut s, req) = next_request(listener, deadline).await?;
                if req.path != "/done" {
                    let _ = respond(&mut s, "404 Not Found", b"{}").await;
                    continue;
                }
                let _ = respond(&mut s, "200 OK", b"{}").await;
                return serde_json::from_slice(&req.body).context("bad report from the new machine");
            },
            (Server::Iroh(_), Peer::Http(_)) => unreachable!("an iroh server has iroh peers"),
        }
    }
}

impl Peer {
    async fn answer(&mut self, a: &Answer) -> Result<()> {
        match self {
            Peer::Http(s) => respond(s, "200 OK", &serde_json::to_vec(a)?).await,
            Peer::Iroh(_, s) => api::send(s, a).await,
        }
    }
}

/// Accepts the pairing requests until one machine is in the cluster.
async fn serve(server: &Server, name: &str, yes: bool, admitter: &Admitter) -> Result<Invited> {
    let deadline = tokio::time::Instant::now() + PAIR_TIME;
    let (mut peer, hello, addr) = server.hello(deadline).await?;
    eprintln!(
        "Machine asks to join:\n  name    {name} (calls itself {})\n  os      {}/{}\n  id      {}\n\n\
         Check that the new machine prints the same ID: {}",
        hello.name,
        hello.os,
        hello.arch,
        addr.id,
        short(&addr.id)
    );
    if !yes && !confirm()? {
        let _ = peer.answer(&Answer::Refused { reason: "the operator refused".into() }).await;
        bail!("refused {}", addr.id);
    }
    let answer = match admit(admitter, &addr, name, &hello).await {
        Ok(a) => a,
        Err(e) => {
            let _ = peer.answer(&Answer::Refused { reason: format!("{e:#}") }).await;
            return Err(e);
        }
    };
    peer.answer(&answer).await?;
    // The pairing counts only after the new machine joined.
    let done =
        spin("waiting for the new machine to join; this can take up to a minute", server.done(peer, deadline)).await?;
    if let Some(e) = done.error {
        bail!("{name} could not join: {e}");
    }
    let cluster = done.cluster.context("the new machine reported no cluster")?;
    Ok(Invited {
        transport: server.transport(),
        id: addr.id,
        name: name.into(),
        os: hello.os,
        arch: hello.arch,
        cluster,
    })
}

/// The next POST request. A request that is broken or slow is dropped, and the server keeps
/// waiting.
async fn next_request(
    listener: &tokio::net::TcpListener,
    deadline: tokio::time::Instant,
) -> Result<(tokio::net::TcpStream, Request)> {
    loop {
        let (mut s, _) =
            tokio::time::timeout_at(deadline, listener.accept()).await.context("the pairing code expired")??;
        let Ok(Ok(req)) = tokio::time::timeout(Duration::from_secs(30), read_request(&mut s)).await else {
            continue;
        };
        if req.method != "POST" {
            let _ = respond(&mut s, "403 Forbidden", b"{}").await;
            continue;
        }
        return Ok((s, req));
    }
}

fn parse_hello(body: &[u8]) -> Option<(Hello, iroh::EndpointAddr)> {
    let hello: Hello = serde_json::from_slice(body).ok()?;
    let ticket: iroh_tickets::endpoint::EndpointTicket = hello.ticket.parse().ok()?;
    let addr = ticket.endpoint_addr().clone();
    Some((hello, addr))
}

fn confirm() -> Result<bool> {
    use std::io::Write;
    eprint!("Admit this machine? [y/N] ");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

/// Admits the new machine and returns the answer for it.
async fn admit(admitter: &Admitter, addr: &iroh::EndpointAddr, name: &str, hello: &Hello) -> Result<Answer> {
    let client = admitter.client();
    let entry: Inspect = client.call(None, "machine.inspect", ()).await?;
    if let Some(base) = hello.gossip_port_base {
        let theirs = entry.gossip_port_base;
        ensure!(
            base == theirs,
            "the new machine uses gossip ports from {base}, the cluster from {theirs}; run `imesh up --gossip-port-base {theirs}` there"
        );
    }
    ensure!(
        hello.tunnel.is_some() || !entry.devtunnel_mesh,
        "the cluster uses the devtunnel mesh; run `imesh up --relay devtunnel` on the new machine first"
    );
    let ticket: JoinTicket =
        client.call(None, "cluster.add", json!({ "id": addr.id, "name": name, "tunnel": hello.tunnel })).await?;
    Ok(Answer::Accepted { join: ticket.to_string() })
}

/// `imesh up --join <code>` on the new machine. It acts on this computer's daemon only. A machine
/// alone in its own cluster with nothing in it leaves that cluster first. With `force`, also when
/// its cluster holds data, which is then lost. Returns false when the machine is in that cluster
/// already.
pub async fn join(code: &Code, force: bool) -> Result<bool> {
    if matches!(code.via, Via::Devtunnel(_)) {
        require_login().await?;
    }
    let mut c = crate::connect::open(&Connection::local()).await.context("no imesh daemon here")?;
    let i: Inspect = c.call(None, "machine.inspect", ()).await?;
    if i.cluster.is_some() {
        let alone = i.alone_and_empty || (force && alone(&c, i.id).await?);
        if !alone {
            c.close().await;
            bail!(if force {
                "this machine is in a cluster with other members; run `imesh logout` first".to_string()
            } else {
                format!(
                    "this machine is in cluster {} with other members or data; run `imesh logout` first, or pass --force to drop its data",
                    i.cluster.unwrap_or_default()
                )
            });
        }
        if i.alone_and_empty {
            eprintln!("leaving this machine's own empty cluster to join; the machine gets a new key");
        } else {
            eprintln!(
                "--force: leaving this machine's own cluster {} to join; its kv and app data are deleted, and the machine gets a new key",
                i.cluster.as_deref().unwrap_or_default()
            );
        }
        c.call::<Value>(None, "machine.reset", ()).await?;
        c.close().await;
        c = reconnect_after_reset(i.id).await?;
    }
    let r = join_with(&c, code).await;
    c.close().await;
    r.map(|()| true)
}

/// Whether this machine is the only member of its cluster.
async fn alone(c: &Client, me: iroh::EndpointId) -> Result<bool> {
    let rows: Vec<iroh_mesh::api::MachineRow> = c.call(None, "cluster.machines", ()).await?;
    Ok(rows.iter().all(|r| r.id == me))
}

/// Waits until this computer's daemon answers again as a new machine, out of any cluster.
async fn reconnect_after_reset(old: iroh::EndpointId) -> Result<Client> {
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let Ok(c) = crate::connect::open(&Connection::local()).await else { continue };
        match c.call::<Inspect>(None, "machine.inspect", ()).await {
            Ok(i) if i.id != old && i.cluster.is_none() => return Ok(c),
            _ => c.close().await,
        }
    }
    bail!("the daemon did not come back as a new machine within 60 seconds")
}

async fn join_with(c: &Client, code: &Code) -> Result<()> {
    let i: Inspect = c.call(None, "machine.inspect", ()).await?;
    if let Some(cluster) = &i.cluster {
        bail!("this machine is already in cluster {cluster}");
    }
    let tunnel = if i.devtunnel_mesh { Some(wait_tunnel(c).await?) } else { None };
    let hello = Hello {
        ticket: iroh_tickets::endpoint::EndpointTicket::new(i.info.addr.clone()).to_string(),
        name: i.info.name.clone(),
        os: i.info.os.clone(),
        arch: i.info.arch.clone(),
        tunnel,
        gossip_port_base: Some(i.gossip_port_base),
    };
    let mut channel = Channel::open(code, &i).await?;
    eprintln!(
        "This machine is {} ({}, {}/{}).\nCheck that the inviting computer shows the same ID: {}\nWaiting for it to admit this machine...",
        i.info.name,
        i.id,
        i.info.os,
        i.info.arch,
        short(&i.id)
    );
    let answer = channel.call("pair", &hello).await?.context("the inviting computer closed the pairing")?;
    let result = match serde_json::from_value::<Answer>(answer).context("bad answer from the inviting computer")? {
        Answer::Accepted { join } => {
            async {
                let j: Joined = spin(
                    "admitted; joining the cluster, this can take up to a minute",
                    c.call(None, "cluster.join", join.parse::<JoinTicket>()?),
                )
                .await?;
                let msg = format!("joined cluster {} with {} members", j.cluster, j.members);
                anyhow::Ok((j.cluster, msg))
            }
            .await
        }
        Answer::Refused { reason } => bail!("pairing refused: {reason}"),
    };
    let report = match &result {
        Ok((cluster, _)) => Done { cluster: Some(cluster.clone()), error: None },
        Err(e) => Done { cluster: None, error: Some(format!("{e:#}")) },
    };
    channel.call("done", &report).await?;
    channel.close().await;
    println!("{}", result?.1);
    Ok(())
}

/// How the new machine reaches the inviting computer.
enum Channel {
    /// HTTPS through the devtunnel service, with this computer's own connect token.
    Devtunnel { base: String, token: String },
    /// One stream on a connection from a temporary iroh endpoint. Its key is fresh, not the
    /// machine's key: the daemon holds that key, and the CLI cannot use it. So iroh does not
    /// prove who sends the hello. As with devtunnel, the operator compares the short ID that both
    /// sides print before the machine is admitted, and only that key can then connect.
    Iroh { endpoint: iroh::Endpoint, stream: api::Stream },
}

impl Channel {
    async fn open(code: &Code, i: &Inspect) -> Result<Channel> {
        match &code.via {
            Via::Devtunnel(tunnel) => {
                require_login().await?;
                eprintln!("connecting to devtunnel {tunnel} with this computer's devtunnel login");
                let token = json_of(&dt(&["token", tunnel, "--scopes", "connect", "--json"]).await?)?;
                let token = token["token"]
                    .as_str()
                    .context("no connect token: does your devtunnel account have access to this tunnel?")?
                    .to_string();
                let show = json_of(&dt(&["show", tunnel, "--json"]).await?)?;
                let uri =
                    find_uri(&show).context("the pairing tunnel has no port URL; is `imesh invite` still running?")?;
                Ok(Channel::Devtunnel { base: uri.trim_end_matches('/').to_string(), token })
            }
            Via::Iroh(addr) => {
                eprintln!("connecting to the inviting computer over iroh");
                let endpoint = temp_endpoint(i, vec![]).await?;
                let stream = async {
                    let conn = tokio::time::timeout(Duration::from_secs(60), endpoint.connect(addr.clone(), PAIR_ALPN))
                        .await
                        .context("the inviting computer did not answer within 60 seconds")??;
                    let (send, recv) = conn.open_bi().await?;
                    anyhow::Ok(api::framed(tokio::io::join(recv, send)))
                }
                .await;
                match stream {
                    Ok(stream) => Ok(Channel::Iroh { endpoint, stream }),
                    Err(e) => {
                        endpoint.close().await;
                        Err(e.context("is `imesh invite` still running, and can this machine reach it?"))
                    }
                }
            }
        }
    }

    /// Sends one step and returns the reply. The `done` step of the iroh channel has no reply.
    async fn call(&mut self, step: &str, body: &impl Serialize) -> Result<Option<Value>> {
        match self {
            Channel::Devtunnel { base, token } => {
                let reply = post(&format!("{base}/{step}"), token, &serde_json::to_vec(body)?).await?;
                Ok(Some(serde_json::from_slice(&reply)?))
            }
            Channel::Iroh { stream, .. } => {
                api::send(stream, body).await?;
                if step == "done" {
                    // Wait until the inviting computer has read it and closes the connection.
                    let _ = tokio::time::timeout(Duration::from_secs(10), api::recv::<Value>(stream)).await;
                    return Ok(None);
                }
                api::recv(stream).await
            }
        }
    }

    async fn close(self) {
        if let Channel::Iroh { endpoint, .. } = self {
            endpoint.close().await;
        }
    }
}

/// This machine's mesh tunnel. A mesh member publishes it once its host is ready, so wait up to a
/// minute for it.
async fn wait_tunnel(c: &Client) -> Result<TunnelEndpoint> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let i: Inspect = c.call(None, "machine.inspect", ()).await?;
        if let Some(t) = i.info.tunnel {
            return Ok(t);
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "this machine's devtunnel relay did not start within 60 seconds; see its log"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn find_uri(v: &Value) -> Option<String> {
    match v {
        Value::Object(o) => {
            for k in ["portUri", "portForwardingUris"] {
                match o.get(k) {
                    Some(Value::String(s)) if s.starts_with("https://") => return Some(s.clone()),
                    Some(Value::Array(a)) => {
                        if let Some(s) = a.iter().filter_map(Value::as_str).find(|s| s.starts_with("https://")) {
                            return Some(s.to_string());
                        }
                    }
                    _ => {}
                }
            }
            o.values().find_map(find_uri)
        }
        Value::Array(a) => a.iter().find_map(find_uri),
        _ => None,
    }
}

/// One HTTPS POST through the devtunnel service, with this machine's own connect token. `curl`
/// ships with Windows 10 and later, macOS, and most Linux systems. The token goes to curl on
/// stdin, never on its command line.
async fn post(url: &str, token: &str, body: &[u8]) -> Result<Vec<u8>> {
    let url = url::Url::parse(url)?;
    ensure!(
        url.scheme() == "https" && url.host_str().is_some_and(|h| h.ends_with(".devtunnels.ms")),
        "unexpected pairing URL {url}"
    );
    let dir = crate::paths::tmp_dir()
        .join(format!("imesh-pair-{}", hex::encode(&iroh::SecretKey::generate().to_bytes()[..8])));
    iroh_mesh::os::create_private_dir(&dir)?;
    let request = dir.join("request.json");
    std::fs::write(&request, body)?;
    let config = format!(
        "url = \"{url}\"\nrequest = \"POST\"\nheader = \"X-Tunnel-Authorization: tunnel {token}\"\n\
         header = \"X-Tunnel-Skip-AntiPhishing-Page: 1\"\nheader = \"Content-Type: application/json\"\n\
         data-binary = \"@{}\"\nmax-time = 900\nsilent\nshow-error\nfail-with-body\n",
        request.display().to_string().replace('\\', "/")
    );
    let result = async {
        let mut child = tokio::process::Command::new(if cfg!(windows) { "curl.exe" } else { "curl" })
            .args(["--config", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("run curl")?;
        child.stdin.take().expect("piped").write_all(config.as_bytes()).await?;
        let out = child.wait_with_output().await?;
        ensure!(
            out.status.success(),
            "pairing tunnel request failed: {}{}",
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
        Ok(out.stdout)
    }
    .await;
    let _ = std::fs::remove_dir_all(&dir);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_round_trip_and_reject_bad_input() {
        assert_eq!(json_of("Welcome to dev tunnels!\n{\"a\":{\"b\":1}}\n").unwrap(), json!({"a":{"b":1}}));
        let c = Code { via: Via::Devtunnel("spiffy-ocean-75063tx.usw2".into()), mesh: false };
        let s = c.to_string();
        assert_eq!(s, "imeshpair1-dt-spiffy-ocean-75063tx.usw2");
        assert_eq!(s.parse::<Code>().unwrap(), c);
        let m = Code { via: Via::Devtunnel("spiffy-ocean-75063tx.usw2".into()), mesh: true };
        assert_eq!(m.to_string(), "imeshpair1-dtm-spiffy-ocean-75063tx.usw2");
        assert_eq!(m.to_string().parse::<Code>().unwrap(), m);
        for bad in ["imeshjoinabc", "imeshpair1-dt-", "imeshpair1-dt--x", "imeshpair1-dt-a b", "imeshpair1-dt-a/b"] {
            assert!(bad.parse::<Code>().is_err(), "{bad}");
        }
    }

    #[test]
    fn iroh_codes_round_trip_and_reject_bad_input() {
        let addr = iroh::EndpointAddr::new(iroh::SecretKey::generate().public())
            .with_ip_addr("192.168.1.5:4433".parse().unwrap())
            .with_relay_url("https://relay.example.com".parse().unwrap());
        let c = Code { via: Via::Iroh(addr), mesh: false };
        let s = c.to_string();
        assert!(s.starts_with("imeshpair1-ir-") && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'), "{s}");
        assert_eq!(s.parse::<Code>().unwrap(), c);
        assert_eq!(format!("  {s}\n").parse::<Code>().unwrap(), c);
        let cut = &s[..s.len() - 4];
        for bad in ["imeshpair1-ir-", "imeshpair1-ir-abc", "imeshpair1-ir-a=b", cut, &format!("{s}-x")] {
            assert!(bad.parse::<Code>().is_err(), "{bad}");
        }
    }

    #[test]
    fn anonymous_access_is_found_anywhere() {
        assert!(anonymous(&json!({"tunnel":{"accessControl":{"entries":[{"type":"Anonymous"}]}}})));
        assert!(!anonymous(&json!({"tunnel":{"accessControl":[]}})));
    }

    #[test]
    fn finds_the_https_port_uri() {
        let v = json!({"tunnel":{"ports":[{"portNumber":5,"portForwardingUris":["https://x-5.usw2.devtunnels.ms/"]}]}});
        assert_eq!(find_uri(&v).as_deref(), Some("https://x-5.usw2.devtunnels.ms/"));
        assert_eq!(find_uri(&json!({"tunnel":{}})), None);
    }
}
