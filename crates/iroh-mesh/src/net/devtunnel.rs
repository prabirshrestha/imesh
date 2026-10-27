//! Exposes the served relay through a Microsoft dev tunnel, found or created by labels.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use iroh::{EndpointId, RelayUrl};
use n0_future::task::AbortOnDropHandle;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::watch;
use tracing::{info, warn};

/// How a machine publishes the relay it serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expose {
    /// Through a Microsoft dev tunnel, with the `devtunnel` CLI on the PATH. With no ID, the
    /// tunnel is found by the labels `imesh` and `imesh-<first 10 hex of the id>`, or created as
    /// [`tunnel_name`].
    DevTunnel { id: Option<String> },
}

impl std::fmt::Display for Expose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Expose::DevTunnel { id: None } => f.write_str("devtunnel"),
            Expose::DevTunnel { id: Some(id) } => write!(f, "devtunnel:{id}"),
        }
    }
}

impl std::str::FromStr for Expose {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.split_once(':') {
            None if s == "devtunnel" => Ok(Expose::DevTunnel { id: None }),
            Some(("devtunnel", id)) if !id.is_empty() => {
                Ok(Expose::DevTunnel { id: Some(valid_id(id).map_err(|e| e.to_string())?.to_string()) })
            }
            _ => Err(format!("unknown relay exposure {s:?}; supported: devtunnel, devtunnel:<id>")),
        }
    }
}

/// The relay a machine hosts through a private tunnel: its loopback URL and the tunnel ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Hosted {
    pub url: RelayUrl,
    pub tunnel: String,
}

/// Keeps the relay reachable through a private tunnel and reports it while it is ready. Dropping
/// the handle stops the tunnel host.
pub(crate) fn expose(
    how: Expose,
    port: u16,
    machine: EndpointId,
    name: String,
    cli: Option<PathBuf>,
    url: watch::Sender<Option<Hosted>>,
) -> AbortOnDropHandle<()> {
    match how {
        Expose::DevTunnel { id } => AbortOnDropHandle::new(tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            loop {
                match devtunnel(cli.as_deref(), id.as_deref(), port, machine, &name, &url).await {
                    Ok(stable) => {
                        if stable {
                            backoff = Duration::from_secs(1);
                        }
                        warn!("devtunnel host exited");
                    }
                    Err(e) => warn!("devtunnel: {e:#}"),
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        })),
    }
}

/// The devtunnel CLI: `cli`, else `devtunnel` from the PATH.
pub(crate) fn command(cli: Option<&Path>) -> tokio::process::Command {
    // A saved path goes stale when the CLI moves, for example after an upgrade.
    tokio::process::Command::new(cli.filter(|p| p.exists()).unwrap_or(Path::new("devtunnel")))
}

async fn dt(cli: Option<&Path>, args: &[&str]) -> Result<String> {
    let out = tokio::time::timeout(
        Duration::from_secs(30),
        command(cli).args(args).stdin(Stdio::null()).kill_on_drop(true).output(),
    )
    .await
    .context("devtunnel command timed out")?
    .context("run devtunnel (is it installed and logged in?)")?;
    if !out.status.success() {
        bail!(
            "devtunnel {}: {}{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Tunnel {
    tunnel_id: String,
    #[serde(default)]
    ports: Vec<Port>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Port {
    port_number: u16,
}

/// Finds or creates this machine's tunnel, adds the relay port, publishes the URL, and runs
/// `devtunnel host` until it exits. Only the child started here is ever stopped.
async fn devtunnel(
    cli: Option<&Path>,
    id: Option<&str>,
    port: u16,
    machine: EndpointId,
    name: &str,
    url: &watch::Sender<Option<Hosted>>,
) -> Result<bool> {
    let label = format!("imesh-{}", machine.fmt_short());
    let tunnel = match id {
        Some(id) => {
            valid_id(id)?;
            if dt(cli, &["show", id, "-j"]).await.is_err() {
                dt(cli, &["create", id, "-l", "imesh", "-l", &label, "-j"]).await?;
            }
            id.to_string()
        }
        None => {
            #[derive(Deserialize)]
            struct List {
                #[serde(default)]
                tunnels: Vec<Tunnel>,
            }
            let list: List = serde_json::from_value(json_of(
                &dt(cli, &["list", "--all-labels", "imesh", "--all-labels", &label, "-j"]).await?,
            )?)?;
            match list.tunnels.into_iter().next() {
                Some(t) => t.tunnel_id,
                None => {
                    #[derive(Deserialize)]
                    struct Created {
                        tunnel: Tunnel,
                    }
                    let c: Created = serde_json::from_value(json_of(
                        &dt(cli, &["create", &tunnel_name(name, machine), "-l", "imesh", "-l", &label, "-j"]).await?,
                    )?)?;
                    c.tunnel.tunnel_id
                }
            }
        }
    };
    valid_id(&tunnel)?;
    let p = port.to_string();
    #[derive(Deserialize)]
    struct Show {
        tunnel: Tunnel,
    }
    let details = json_of(&dt(cli, &["show", &tunnel, "-j"]).await?)?;
    let show: Show = serde_json::from_value(details.clone())?;
    ensure!(
        show.tunnel.ports.iter().all(|p| p.port_number == port),
        "use a dedicated devtunnel with only the relay port"
    );
    ensure!(!anonymous(&details), "devtunnel {tunnel} allows anonymous access; remove that access first");
    if !show.tunnel.ports.iter().any(|x| x.port_number == port) {
        dt(cli, &["port", "create", &tunnel, "-p", &p, "--protocol", "http"]).await?;
    }
    let mut child = command(cli)
        .args(["host", &tunnel])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("start devtunnel host")?;
    // Clients authenticate with `devtunnel connect` and forward this same port locally.
    // The HTTPS web-forwarding URL stays private and is not an iroh relay address.
    let local: RelayUrl = format!("http://127.0.0.1:{port}/").parse()?;
    wait_ready(&mut child, None).await?;
    info!(%tunnel, %local, "private relay hosted through devtunnel");
    url.send_replace(Some(Hosted { url: local, tunnel: tunnel.clone() }));
    let started = tokio::time::Instant::now();
    let status = supervise(&mut child, CHECK_EVERY, || hosted(cli, &tunnel)).await;
    url.send_replace(None);
    let status = status?;
    info!(%status, "devtunnel host stopped");
    Ok(started.elapsed() >= Duration::from_secs(60))
}

/// Deletes the tunnel that a machine hosted, after it left its cluster and its daemon stopped.
/// Only a tunnel that carries the machine's label is deleted, never a tunnel that the user named
/// and shares.
pub async fn delete_own(cli: Option<&Path>, tunnel: &str, machine: EndpointId) -> Result<bool> {
    let label = format!("imesh-{}", machine.fmt_short());
    let ours = dt(cli, &["show", tunnel, "-j"])
        .await
        .and_then(|out| json_of(&out))
        .map(|d| labels(&d).iter().any(|l| *l == label))
        .unwrap_or(false);
    if !ours {
        return Ok(false);
    }
    dt(cli, &["delete", tunnel, "--force"]).await?;
    Ok(true)
}

fn labels(details: &serde_json::Value) -> Vec<&str> {
    details
        .pointer("/tunnel/labels")
        .and_then(|l| l.as_array())
        .map(|l| l.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default()
}

/// The ID of a new tunnel for a machine: `imesh-<name>-<first 10 hex of the id>`, so
/// `devtunnel list` shows which machine owns it. The name keeps only lowercase letters, digits,
/// and single hyphens, and is shortened to fit the 49 characters that devtunnel allows.
pub(crate) fn tunnel_name(name: &str, machine: EndpointId) -> String {
    let mut clean = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            clean.push(c);
        } else if !clean.is_empty() && !clean.ends_with('-') {
            clean.push('-');
        }
    }
    clean.truncate(49 - "imesh--".len() - 10);
    let clean = clean.trim_end_matches('-');
    let id = machine.fmt_short();
    if clean.is_empty() { format!("imesh-{id}") } else { format!("imesh-{clean}-{id}") }
}

/// The JSON object in devtunnel's output. The CLI can print a banner or a notice around it.
pub(crate) fn json_of(text: &str) -> Result<serde_json::Value> {
    let start = text.find('{').context("devtunnel printed no JSON")?;
    let end = text.rfind('}').context("devtunnel printed no JSON")?;
    serde_json::from_str(&text[start..=end]).context("read devtunnel JSON")
}

fn valid_id(id: &str) -> Result<&str> {
    ensure!(
        id.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
            && id.len() <= 200
            && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'.'),
        "invalid devtunnel ID"
    );
    Ok(id)
}

fn anonymous(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(o) => {
            o.get("type").and_then(|v| v.as_str()).is_some_and(|t| t.eq_ignore_ascii_case("anonymous"))
                || o.values().any(anonymous)
        }
        serde_json::Value::Array(a) => a.iter().any(anonymous),
        _ => false,
    }
}

/// A private tunnel forwarded by the logged-in devtunnel CLI. The same port must be
/// free on each client. Use one tunnel per endpoint; loopback URLs do not name a tunnel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connect {
    pub id: String,
    pub port: u16,
}

/// The relay port of a machine in the devtunnel mesh, from its key. `devtunnel connect` forwards
/// a tunnel's port to the same local port number, so each member needs its own. 49152..65535.
pub fn mesh_port(id: &EndpointId) -> u16 {
    let b = id.as_bytes();
    49152 + (u16::from_le_bytes([b[0], b[1]]) % 16383)
}

impl std::str::FromStr for Connect {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        let (id, port) = s
            .strip_prefix("devtunnel:")
            .context("expected devtunnel:<id>:<port>")?
            .rsplit_once(':')
            .context("expected devtunnel:<id>:<port>")?;
        let port = port.parse::<u16>()?;
        ensure!(port != 0, "devtunnel port must not be zero");
        Ok(Self { id: valid_id(id)?.to_string(), port })
    }
}

impl std::fmt::Display for Connect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "devtunnel:{}:{}", self.id, self.port)
    }
}

impl Connect {
    pub fn relay_url(&self) -> RelayUrl {
        format!("http://127.0.0.1:{}/", self.port).parse().expect("loopback URL")
    }

    /// Starts the CLI and waits for its forwarding announcement. Dropping the handle
    /// stops only this child. No login credentials or tokens enter imesh's database.
    /// `cli` is the devtunnel CLI, else `devtunnel` from the PATH.
    pub async fn start(&self, cli: Option<PathBuf>) -> Result<AbortOnDropHandle<()>> {
        valid_id(&self.id)?;
        ensure!(self.port != 0, "devtunnel port must not be zero");
        let first = self.connect(cli.as_deref()).await?;
        let config = self.clone();
        Ok(AbortOnDropHandle::new(tokio::spawn(async move {
            let mut child = first;
            let mut backoff = Duration::from_secs(1);
            loop {
                let started = tokio::time::Instant::now();
                if let Ok(why) = supervise(&mut child, CHECK_EVERY, || listening(config.port)).await {
                    warn!(tunnel = %config.id, "devtunnel connect stopped: {why}");
                }
                if started.elapsed() >= Duration::from_secs(60) {
                    backoff = Duration::from_secs(1);
                }
                loop {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                    match config.connect(cli.as_deref()).await {
                        Ok(next) => {
                            child = next;
                            break;
                        }
                        Err(e) => warn!("devtunnel connect: {e:#}"),
                    }
                }
            }
        })))
    }

    /// Starts one forwarding child and waits until it listens on the exact port. The caller
    /// owns the child: dropping it stops the forward.
    pub(crate) async fn run_once(&self, cli: Option<&Path>) -> Result<tokio::process::Child> {
        self.connect(cli).await
    }

    /// Runs until the forwarding child exits, or stops it when its port stops accepting
    /// connections. Returns why it stopped.
    pub(crate) async fn watch(&self, child: &mut tokio::process::Child) -> Result<String> {
        supervise(child, CHECK_EVERY, || listening(self.port)).await
    }

    async fn connect(&self, cli: Option<&Path>) -> Result<tokio::process::Child> {
        // Do not attach to an unrelated listener or accept the CLI's alternate-port fallback.
        let details = json_of(&dt(cli, &["show", &self.id, "-j"]).await?)?;
        ensure!(!anonymous(&details), "devtunnel {} allows anonymous access", self.id);
        let ports = details
            .pointer("/tunnel/ports")
            .and_then(|p| p.as_array())
            .context("devtunnel did not report its ports")?;
        ensure!(
            ports.len() == 1 && ports[0]["portNumber"].as_u64() == Some(u64::from(self.port)),
            "use a dedicated devtunnel with only the requested relay port"
        );
        let probe = tokio::net::TcpSocket::new_v4()?;
        probe.set_reuseaddr(true)?;
        probe
            .bind((std::net::Ipv4Addr::LOCALHOST, self.port).into())
            .with_context(|| format!("devtunnel local port {} is in use", self.port))?;
        let probe = probe.listen(1).with_context(|| format!("devtunnel local port {} is in use", self.port))?;
        drop(probe);
        let mut child = command(cli)
            .args(["connect", &self.id])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("start devtunnel connect (install the CLI and log in first)")?;
        wait_ready(&mut child, Some(&format!("127.0.0.1:{}", self.port))).await?;
        Ok(child)
    }
}

/// How often a running devtunnel child is checked, and how many failed checks in a row stop it.
const CHECK_EVERY: Duration = Duration::from_secs(30);
const CHECK_FAILURES: u32 = 3;

/// The result of one liveness check of a running devtunnel child.
enum Check {
    Alive,
    Down(String),
    /// The check itself could not run, for example the service was slow. It does not count.
    Unknown(String),
}

/// Waits for `child` to exit. A devtunnel child can stay running after it loses its connection to
/// the service, and then nothing restarts it. So `check` runs every `every`, and after
/// [`CHECK_FAILURES`] `Down` results in a row the child is stopped. The caller then starts a new
/// one.
async fn supervise<F, Fut>(child: &mut tokio::process::Child, every: Duration, check: F) -> Result<String>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Check>,
{
    let mut failures = 0;
    loop {
        let result = tokio::select! {
            status = child.wait() => return Ok(format!("exited: {}", status?)),
            result = async { tokio::time::sleep(every).await; check().await } => result,
        };
        match result {
            Check::Alive => failures = 0,
            Check::Unknown(e) => tracing::debug!("devtunnel check could not run: {e}"),
            Check::Down(e) => {
                failures += 1;
                warn!(failures, "devtunnel check failed: {e}");
                if failures >= CHECK_FAILURES {
                    let _ = child.kill().await;
                    return Ok(format!("stopped after {failures} failed checks: {e}"));
                }
            }
        }
    }
}

/// The tunnel has a host connection: `devtunnel host` still reaches the service. Only a reply
/// that reports zero host connections counts as down.
async fn hosted(cli: Option<&Path>, tunnel: &str) -> Check {
    let details = match dt(cli, &["show", tunnel, "-j"]).await.and_then(|out| json_of(&out)) {
        Ok(d) => d,
        Err(e) => return Check::Unknown(format!("{e:#}")),
    };
    host_connections(&details, tunnel)
}

fn host_connections(details: &serde_json::Value, tunnel: &str) -> Check {
    match details.pointer("/tunnel/hostConnections").and_then(|v| v.as_u64()) {
        Some(0) => Check::Down(format!("devtunnel {tunnel} has no host connection")),
        Some(_) => Check::Alive,
        None => Check::Unknown("devtunnel show reported no hostConnections".into()),
    }
}

/// The forwarded port accepts connections: `devtunnel connect` still listens.
async fn listening(port: u16) -> Check {
    let connect = tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port));
    match tokio::time::timeout(Duration::from_secs(5), connect).await {
        Ok(Ok(_)) => Check::Alive,
        Ok(Err(e)) => Check::Down(format!("nothing listens on 127.0.0.1:{port}: {e}")),
        Err(_) => Check::Down(format!("127.0.0.1:{port} did not accept a connection within 5 seconds")),
    }
}

async fn wait_ready(child: &mut tokio::process::Child, expected: Option<&str>) -> Result<()> {
    let mut out = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
    let mut err = BufReader::new(child.stderr.take().expect("piped stderr")).lines();
    let mut stdout_open = true;
    let mut stderr_open = true;
    let mut ready = Ready::default();
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let line = tokio::select! {
                line = out.next_line(), if stdout_open => {
                    let line = line?;
                    stdout_open = line.is_some();
                    line
                }
                line = err.next_line(), if stderr_open => {
                    let line = line?;
                    stderr_open = line.is_some();
                    line
                }
                status = child.wait() => bail!("devtunnel child exited: {}", status?),
            };
            let Some(line) = line else { continue };
            if match expected {
                Some(at) => ready.update(&line, at),
                None => line.contains("Ready to accept connections for tunnel:"),
            } {
                return Ok(());
            }
        }
    })
    .await
    .context("devtunnel did not become ready within 60 seconds")??;
    // Drain both streams for the child's lifetime so its output cannot block it.
    tokio::spawn(async move {
        while let Ok(Some(line)) = out.next_line().await {
            tracing::debug!(%line, "devtunnel stdout");
        }
    });
    tokio::spawn(async move {
        while let Ok(Some(line)) = err.next_line().await {
            tracing::debug!(%line, "devtunnel stderr");
        }
    });
    Ok(())
}

#[derive(Default)]
struct Ready {
    connected: bool,
    listening: bool,
}

impl Ready {
    fn update(&mut self, line: &str, expected: &str) -> bool {
        self.connected |= line.contains("Connected to tunnel:");
        self.listening |= line.contains(&format!("PortForwardingService listening on {expected}."));
        self.connected && self.listening
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn host_rejects_invalid_library_id_before_running_cli() {
        let url = watch::channel(None).0;
        let machine = iroh::SecretKey::generate().public();
        assert!((49152..65535).contains(&mesh_port(&machine)));
        let error = devtunnel(None, Some("--help"), 3340, machine, "m", &url).await.unwrap_err();
        assert_eq!(error.to_string(), "invalid devtunnel ID");
        assert!(url.borrow().is_none());
    }

    #[test]
    #[ignore = "the child process of a_stuck_child_is_stopped_and_a_healthy_one_is_kept"]
    fn sleep_as_child() {
        if std::env::var_os("IMESH_TEST_CHILD").is_some() {
            std::thread::sleep(Duration::from_secs(600));
        }
    }

    #[tokio::test]
    async fn a_stuck_child_is_stopped_and_a_healthy_one_is_kept() {
        // This test binary, sleeping: a child that runs until it is stopped.
        let spawn = || {
            tokio::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "net::devtunnel::tests::sleep_as_child", "--ignored"])
                .env("IMESH_TEST_CHILD", "1")
                .stdout(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .expect("run the test binary")
        };
        let every = Duration::from_millis(10);
        let mut stuck = spawn();
        let why = supervise(&mut stuck, every, || async { Check::Down("down".into()) }).await.unwrap();
        assert!(why.starts_with("stopped after 3 failed checks"), "{why}");
        assert!(stuck.try_wait().unwrap().is_some(), "the stuck child was stopped");

        let mut healthy = spawn();
        let kept =
            tokio::time::timeout(Duration::from_millis(200), supervise(&mut healthy, every, || async { Check::Alive }));
        assert!(kept.await.is_err(), "a child that passes its checks keeps running");

        let mut unknown = spawn();
        let kept = supervise(&mut unknown, every, || async { Check::Unknown("slow".into()) });
        assert!(
            tokio::time::timeout(Duration::from_millis(200), kept).await.is_err(),
            "a check that cannot run does not stop the child"
        );
    }

    #[test]
    fn host_connections_from_devtunnel_show() {
        let show = |n: serde_json::Value| serde_json::json!({ "tunnel": { "tunnelId": "t", "hostConnections": n } });
        assert!(matches!(host_connections(&show(1.into()), "t"), Check::Alive));
        assert!(matches!(host_connections(&show(0.into()), "t"), Check::Down(_)));
        assert!(matches!(host_connections(&serde_json::json!({ "tunnel": {} }), "t"), Check::Unknown(_)));
    }

    #[test]
    fn tunnel_names_are_valid_and_readable() {
        let machine = iroh::SecretKey::generate().public();
        let id = machine.fmt_short();
        assert_eq!(tunnel_name("machine1", machine), format!("imesh-machine1-{id}"));
        assert_eq!(tunnel_name("My Mac.local", machine), format!("imesh-my-mac-local-{id}"));
        assert_eq!(tunnel_name("--", machine), format!("imesh-{id}"));
        let long = tunnel_name(&"a-".repeat(40), machine);
        assert!(long.len() <= 49, "{long}");
        for n in [tunnel_name("machine1", machine), long, tunnel_name("", machine)] {
            assert!(n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'), "{n}");
            assert!(!n.ends_with('-') && !n.contains("--"), "{n}");
        }
    }

    #[test]
    fn forwarding_rule_is_not_readiness() {
        let mut ready = Ready::default();
        let at = "127.0.0.1:3340";
        assert!(!ready.update("SSH: Forwarding from 127.0.0.1:3340 to host port 3340.", at));
        assert!(!ready.update("Connected to tunnel: test", at));
        assert!(!ready.update("SSH: PortForwardingService listening on 127.0.0.1:3341.", at));
        assert!(ready.update("SSH: PortForwardingService listening on 127.0.0.1:3340.", at));
        let mut ready = Ready::default();
        assert!(!ready.update("SSH: PortForwardingService listening on 127.0.0.1:3340.", at));
        assert!(ready.update("Connected to tunnel: test", at));
    }

    #[test]
    fn private_tunnel_inputs_and_access() {
        let c: Connect = "devtunnel:my-relay.use:3340".parse().unwrap();
        assert_eq!(c.relay_url().as_str(), "http://127.0.0.1:3340/");
        for bad in ["devtunnel:-v:3340", "devtunnel:a:0", "devtunnel:a/b:3340", "devtunnel:a:65536"] {
            assert!(bad.parse::<Connect>().is_err(), "{bad}");
        }
        assert!(anonymous(&serde_json::json!({"tunnel":{"accessControl":{"entries":[{"type":"Anonymous"}]}}})));
        assert!(!anonymous(&serde_json::json!({"tunnel":{"accessControl":{"entries":[{"type":"Users"}]}}})));
    }
}
