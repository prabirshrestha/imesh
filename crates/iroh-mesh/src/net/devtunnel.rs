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
    cli: Cli,
    url: watch::Sender<Option<Hosted>>,
    mut wake: Wake,
) -> AbortOnDropHandle<()> {
    match how {
        Expose::DevTunnel { id } => AbortOnDropHandle::new(tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            loop {
                match devtunnel(&cli, id.as_deref(), port, machine, &name, &url, &mut wake).await {
                    Ok(stable) => {
                        if stable {
                            backoff = Duration::from_secs(1);
                        }
                        warn!("devtunnel host exited");
                    }
                    Err(e) => warn!("devtunnel: {e:#}"),
                }
                backoff = nap(backoff, &mut wake).await;
            }
        })),
    }
}

/// The devtunnel CLI: `cli`, else `devtunnel` from the PATH.
pub(crate) fn command(cli: Option<&Path>) -> tokio::process::Command {
    // A saved path goes stale when the CLI moves, for example after an upgrade.
    tokio::process::Command::new(cli.filter(|p| p.exists()).unwrap_or(Path::new("devtunnel")))
}

/// The environment variable that marks a long-running devtunnel child (`host`, `connect`) with
/// the machine that started it: a hash of its data directory.
const OWNER_ENV: &str = "IMESH_DEVTUNNEL_OWNER";

/// The devtunnel CLI, and the machine whose children it starts. A machine that stops by a crash
/// or a kill cannot stop its children, and they keep running: a `devtunnel host` of the old run
/// then serves the machine's tunnel too, with no relay behind it. So every long-running child
/// carries its machine's mark ([`OWNER_ENV`]), and [`reap`] stops the marked ones of an earlier
/// run when the machine starts again. Where the system can do more, a child also ends with the
/// machine: on Linux by `PR_SET_PDEATHSIG`, on Windows in a job that closes with this process.
#[derive(Debug, Clone, Default)]
pub(crate) struct Cli {
    path: Option<PathBuf>,
    owner: Option<String>,
}

impl Cli {
    /// The CLI at `path`, for the machine whose data directory is `data_dir`.
    pub(crate) fn owned(path: Option<PathBuf>, data_dir: &Path) -> Self {
        Cli { path, owner: Some(owner_of(data_dir)) }
    }

    /// The CLI at `path`, with no machine: its children are stopped only when dropped.
    pub(crate) fn plain(path: Option<PathBuf>) -> Self {
        Cli { path, owner: None }
    }

    pub(crate) fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Starts a long-running child: `devtunnel <args>`, marked with the machine, piped, and
    /// stopped when dropped.
    fn spawn(&self, args: &[&str], what: &str) -> Result<tokio::process::Child> {
        let mut c = command(self.path());
        c.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        if let Some(owner) = &self.owner {
            c.env(OWNER_ENV, owner);
        }
        crate::os::end_with_parent(&mut c);
        let child = c.spawn().with_context(|| what.to_string())?;
        crate::os::adopt_child(&child);
        Ok(child)
    }
}

/// The mark of the machine whose data directory is `dir`: the first 16 hex of the BLAKE3 hash of
/// its absolute path.
fn owner_of(dir: &Path) -> String {
    let dir = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
    blake3::hash(dir.as_os_str().as_encoded_bytes()).to_hex()[..16].to_string()
}

/// Stops the devtunnel children of an earlier run of `cli`'s machine: every process that carries
/// its mark, other than this one. Call it after the machine holds its data directory, before it
/// starts new children. Returns how many it stopped.
pub(crate) async fn reap(cli: &Cli) -> usize {
    let Some(owner) = cli.owner.clone() else { return 0 };
    let n = tokio::task::spawn_blocking(move || reap_marked(&format!("{OWNER_ENV}={owner}"))).await.unwrap_or(0);
    if n > 0 {
        warn!(stopped = n, "stopped devtunnel children that an earlier run left behind");
    }
    n
}

fn reap_marked(mark: &str) -> usize {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let Ok(mut sys) = System::new() else { return 0 };
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_environ(UpdateKind::Always),
    );
    let me = sysinfo::Pid::from_u32(std::process::id());
    sys.processes()
        .iter()
        .filter(|(pid, p)| **pid != me && p.environ().iter().any(|e| e == mark))
        .filter(|(_, p)| p.kill())
        .count()
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
    owned: &Cli,
    id: Option<&str>,
    port: u16,
    machine: EndpointId,
    name: &str,
    url: &watch::Sender<Option<Hosted>>,
    wake: &mut Wake,
) -> Result<bool> {
    let cli = owned.path();
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
    // A port other than the relay port: on this machine's own tunnel, from an earlier mesh port.
    let stale: Vec<String> =
        show.tunnel.ports.iter().filter(|x| x.port_number != port).map(|x| x.port_number.to_string()).collect();
    ensure!(id.is_none() || stale.is_empty(), "use a dedicated devtunnel with only the relay port");
    ensure!(!anonymous(&details), "devtunnel {tunnel} allows anonymous access; remove that access first");
    for old in &stale {
        dt(cli, &["port", "delete", &tunnel, "-p", old]).await?;
    }
    if !show.tunnel.ports.iter().any(|x| x.port_number == port) {
        dt(cli, &["port", "create", &tunnel, "-p", &p, "--protocol", "http"]).await?;
    }
    let mut child = owned.spawn(&["host", &tunnel], "start devtunnel host")?;
    // Clients authenticate with `devtunnel connect` and forward this same port locally.
    // The HTTPS web-forwarding URL stays private and is not an iroh relay address.
    let local: RelayUrl = format!("http://127.0.0.1:{port}/").parse()?;
    wait_ready(&mut child, false).await?;
    info!(%tunnel, %local, "private relay hosted through devtunnel");
    url.send_replace(Some(Hosted { url: local, tunnel: tunnel.clone() }));
    let started = tokio::time::Instant::now();
    let status = supervise(&mut child, CHECK_EVERY, wake, || hosted(cli, &tunnel)).await;
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

/// A private tunnel forwarded by the logged-in devtunnel CLI. Use one tunnel per endpoint;
/// loopback URLs do not name a tunnel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connect {
    pub id: String,
    pub port: u16,
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
    pub async fn start(&self, cli: Option<PathBuf>, wake: Wake) -> Result<AbortOnDropHandle<()>> {
        self.start_owned(Cli::plain(cli), wake).await
    }

    /// As [`Connect::start`], with children marked as `cli`'s machine's.
    pub(crate) async fn start_owned(&self, cli: Cli, mut wake: Wake) -> Result<AbortOnDropHandle<()>> {
        valid_id(&self.id)?;
        ensure!(self.port != 0, "devtunnel port must not be zero");
        let (first, _) = self.connect(&cli, true).await?;
        let config = self.clone();
        Ok(AbortOnDropHandle::new(tokio::spawn(async move {
            let mut child = first;
            let mut backoff = Duration::from_secs(1);
            loop {
                let started = tokio::time::Instant::now();
                if let Ok(why) = supervise(&mut child, CHECK_EVERY, &mut wake, || relay_answers(config.port)).await {
                    warn!(tunnel = %config.id, "devtunnel connect stopped: {why}");
                }
                if started.elapsed() >= Duration::from_secs(60) {
                    backoff = Duration::from_secs(1);
                }
                loop {
                    backoff = nap(backoff, &mut wake).await;
                    match config.connect(&cli, true).await {
                        Ok((next, _)) => {
                            child = next;
                            break;
                        }
                        Err(e) => warn!("devtunnel connect: {e:#}"),
                    }
                }
            }
        })))
    }

    /// Starts one forwarding child and waits until it listens. Returns the child and its local
    /// port: the tunnel's port, or another one when that is in use. The caller owns the child:
    /// dropping it stops the forward.
    pub(crate) async fn run_once(&self, cli: &Cli) -> Result<(tokio::process::Child, u16)> {
        self.connect(cli, false).await
    }

    /// Runs until the forwarding child exits, or stops it when the member's relay stops answering
    /// through its local `port`. Returns why it stopped.
    pub(crate) async fn watch(&self, child: &mut tokio::process::Child, port: u16, wake: &mut Wake) -> Result<String> {
        supervise(child, CHECK_EVERY, wake, || relay_answers(port)).await
    }

    /// With `exact`, the local port must be the tunnel's port, because every member names the
    /// relay by that URL. Else the CLI may pick another free port, which it prints.
    async fn connect(&self, owned: &Cli, exact: bool) -> Result<(tokio::process::Child, u16)> {
        let details = json_of(&dt(owned.path(), &["show", &self.id, "-j"]).await?)?;
        ensure!(!anonymous(&details), "devtunnel {} allows anonymous access", self.id);
        let ports = details
            .pointer("/tunnel/ports")
            .and_then(|p| p.as_array())
            .context("devtunnel did not report its ports")?;
        ensure!(
            ports.len() == 1 && ports[0]["portNumber"].as_u64() == Some(u64::from(self.port)),
            "use a dedicated devtunnel with only the requested relay port"
        );
        if exact {
            let probe = tokio::net::TcpSocket::new_v4()?;
            probe.set_reuseaddr(true)?;
            probe
                .bind((std::net::Ipv4Addr::LOCALHOST, self.port).into())
                .with_context(|| format!("devtunnel local port {} is in use", self.port))?;
            let probe = probe.listen(1).with_context(|| format!("devtunnel local port {} is in use", self.port))?;
            drop(probe);
        }
        let mut child =
            owned.spawn(&["connect", &self.id], "start devtunnel connect (install the CLI and log in first)")?;
        let port = wait_ready(&mut child, true).await?.expect("a forward reports its port");
        ensure!(!exact || port == self.port, "devtunnel local port {} is in use", self.port);
        Ok((child, port))
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

/// Fires when the computer woke from sleep or its network changed (see `daemon::wake`): the
/// devtunnel children are checked at once then, and a child that waits to start again starts now.
pub type Wake = watch::Receiver<u64>;

/// Resolves on the next wake. Never, when no one sends wakes any more.
async fn woken(wake: &mut Wake) {
    if wake.changed().await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Sleeps `backoff`, or less when a wake comes. Gives the next backoff: doubled up to a minute,
/// or one second again after a wake.
pub(crate) async fn nap(backoff: Duration, wake: &mut Wake) -> Duration {
    tokio::select! {
        _ = tokio::time::sleep(backoff) => (backoff * 2).min(Duration::from_secs(60)),
        _ = woken(wake) => Duration::from_secs(1),
    }
}

/// Waits for `child` to exit. A devtunnel child can stay running after it loses its connection to
/// the service, and then nothing restarts it. So `check` runs every `every`, and after
/// [`CHECK_FAILURES`] `Down` results in a row the child is stopped. A wake runs `check` at once,
/// and one `Down` then stops the child: after sleep or a network change, its connection is most
/// likely gone. The caller then starts a new one.
async fn supervise<F, Fut>(
    child: &mut tokio::process::Child,
    every: Duration,
    wake: &mut Wake,
    check: F,
) -> Result<String>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Check>,
{
    let mut failures = 0;
    loop {
        let (result, woke) = tokio::select! {
            status = child.wait() => return Ok(format!("exited: {}", status?)),
            result = async { tokio::time::sleep(every).await; check().await } => (result, false),
            result = async { woken(wake).await; check().await } => (result, true),
        };
        match result {
            Check::Alive => failures = 0,
            Check::Unknown(e) => tracing::debug!("devtunnel check could not run: {e}"),
            Check::Down(e) => {
                failures += 1;
                warn!(failures, woke, "devtunnel check failed: {e}");
                if woke || failures >= CHECK_FAILURES {
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

/// The member's relay answers through the forward: `GET /healthz` on the forwarded port goes
/// through `devtunnel connect`, the service and the member's `devtunnel host` to its relay. A
/// forward can listen and still carry nothing, after sleep or a network change.
async fn relay_answers(port: u16) -> Check {
    match tokio::time::timeout(RELAY_CHECK_TIMEOUT, healthz(port)).await {
        Ok(Ok(200)) => Check::Alive,
        Ok(Ok(status)) => Check::Down(format!("the relay through 127.0.0.1:{port} answered {status}")),
        Ok(Err(e)) => Check::Down(format!("no relay answers through 127.0.0.1:{port}: {e:#}")),
        Err(_) => Check::Down(format!(
            "the relay through 127.0.0.1:{port} did not answer within {} seconds",
            RELAY_CHECK_TIMEOUT.as_secs()
        )),
    }
}

const RELAY_CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// The status of `GET /healthz` on the loopback `port`.
async fn healthz(port: u16) -> Result<u16> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    stream
        .write_all(format!("GET /healthz HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").as_bytes())
        .await?;
    let mut head = Vec::new();
    let mut buf = [0u8; 256];
    while !head.windows(2).any(|w| w == b"\r\n") {
        let n = stream.read(&mut buf).await?;
        ensure!(n != 0, "the connection closed before an answer");
        head.extend_from_slice(&buf[..n]);
        ensure!(head.len() < 4096, "the answer has no status line");
    }
    status_of(&head).context("the answer is not HTTP")
}

/// The status code of an HTTP status line: `HTTP/1.1 200 OK`.
fn status_of(head: &[u8]) -> Option<u16> {
    let line = std::str::from_utf8(head).ok()?.lines().next()?;
    let mut words = line.split_whitespace();
    words.next().filter(|v| v.starts_with("HTTP/"))?;
    words.next()?.parse().ok()
}

/// Waits until a host is ready, or with `forward` until a forward listens. Returns the forward's
/// local port.
async fn wait_ready(child: &mut tokio::process::Child, forward: bool) -> Result<Option<u16>> {
    let mut out = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
    let mut err = BufReader::new(child.stderr.take().expect("piped stderr")).lines();
    let mut stdout_open = true;
    let mut stderr_open = true;
    let mut ready = Ready::default();
    let port = tokio::time::timeout(Duration::from_secs(60), async {
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
            if !forward && line.contains("Ready to accept connections for tunnel:") {
                return Ok(None);
            }
            if forward && let Some(port) = ready.update(&line) {
                return Ok(Some(port));
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
    Ok(port)
}

#[derive(Default)]
struct Ready {
    connected: bool,
    listening: Option<u16>,
}

impl Ready {
    /// The local port, once the child is connected and listens on IPv4 loopback.
    fn update(&mut self, line: &str) -> Option<u16> {
        self.connected |= line.contains("Connected to tunnel:");
        if let Some(port) = line
            .split_once("PortForwardingService listening on 127.0.0.1:")
            .and_then(|(_, rest)| rest.strip_suffix('.'))
            .and_then(|p| p.parse().ok())
        {
            self.listening = Some(port);
        }
        self.listening.filter(|_| self.connected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn host_rejects_invalid_library_id_before_running_cli() {
        let url = watch::channel(None).0;
        let machine = iroh::SecretKey::generate().public();
        let error = devtunnel(&Cli::default(), Some("--help"), 3340, machine, "m", &url, &mut watch::channel(0).1)
            .await
            .unwrap_err();
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
    async fn a_start_stops_only_the_children_its_machine_left_behind() {
        let mine = Cli::owned(None, Path::new("/tmp/imesh-reap-test-a"));
        let theirs = Cli::owned(None, Path::new("/tmp/imesh-reap-test-b"));
        assert_ne!(mine.owner, theirs.owner, "each data directory has its own mark");
        assert_eq!(mine.owner, Cli::owned(None, Path::new("/tmp/imesh-reap-test-a")).owner);
        let sleeper = |cli: &Cli| {
            let mut c = tokio::process::Command::new(std::env::current_exe().unwrap());
            c.args(["--exact", "net::devtunnel::tests::sleep_as_child", "--ignored"])
                .env("IMESH_TEST_CHILD", "1")
                .env(OWNER_ENV, cli.owner.as_deref().unwrap())
                .stdout(Stdio::null())
                .kill_on_drop(true);
            c.spawn().expect("run the test binary")
        };
        // A child of an earlier run, and one of another machine on this computer.
        let mut left = sleeper(&mine);
        let mut other = sleeper(&theirs);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(reap(&mine).await, 1);
        let gone = tokio::time::timeout(Duration::from_secs(10), left.wait()).await;
        assert!(gone.is_ok(), "the left child ends");
        assert!(other.try_wait().unwrap().is_none(), "the other machine's child keeps running");
        assert_eq!(reap(&Cli::plain(None)).await, 0, "a CLI with no machine stops nothing");
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
        let (_tx, wake) = watch::channel(0);
        let mut stuck = spawn();
        let why =
            supervise(&mut stuck, every, &mut wake.clone(), || async { Check::Down("down".into()) }).await.unwrap();
        assert!(why.starts_with("stopped after 3 failed checks"), "{why}");
        assert!(stuck.try_wait().unwrap().is_some(), "the stuck child was stopped");

        let mut healthy = spawn();
        let mut w = wake.clone();
        let kept = tokio::time::timeout(
            Duration::from_millis(200),
            supervise(&mut healthy, every, &mut w, || async { Check::Alive }),
        );
        assert!(kept.await.is_err(), "a child that passes its checks keeps running");

        let mut unknown = spawn();
        let mut w = wake.clone();
        let kept = supervise(&mut unknown, every, &mut w, || async { Check::Unknown("slow".into()) });
        assert!(
            tokio::time::timeout(Duration::from_millis(200), kept).await.is_err(),
            "a check that cannot run does not stop the child"
        );
    }

    #[tokio::test]
    async fn a_wake_checks_at_once_and_one_failure_then_stops_the_child() {
        let spawn = || {
            tokio::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "net::devtunnel::tests::sleep_as_child", "--ignored"])
                .env("IMESH_TEST_CHILD", "1")
                .stdout(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .expect("run the test binary")
        };
        let (tx, mut wake) = watch::channel(0);
        let mut child = spawn();
        // Checks are an hour apart, so only the wake can run one.
        let run = supervise(&mut child, Duration::from_secs(3600), &mut wake, || async { Check::Down("gone".into()) });
        let started = tokio::time::Instant::now();
        let (why, ()) = tokio::join!(run, async { tx.send_modify(|n| *n += 1) });
        assert!(why.unwrap().starts_with("stopped after 1 failed checks"));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(child.try_wait().unwrap().is_some());

        // A wake cuts the wait before a new start short, and the backoff starts again.
        let (tx, mut wake) = watch::channel(0);
        let napping = nap(Duration::from_secs(3600), &mut wake);
        let (next, ()) = tokio::join!(napping, async { tx.send_modify(|n| *n += 1) });
        assert_eq!(next, Duration::from_secs(1));
        let (_tx, mut quiet) = watch::channel(0);
        assert_eq!(nap(Duration::from_millis(5), &mut quiet).await, Duration::from_millis(10));
    }

    #[tokio::test]
    async fn the_relay_check_reads_the_status_through_the_port() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let serve = |status: &'static str| {
            let listener = &listener;
            async move {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 512];
                let n = s.read(&mut buf).await.unwrap();
                assert!(buf[..n].starts_with(b"GET /healthz HTTP/1.1"));
                s.write_all(format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\n\r\n").as_bytes()).await.unwrap();
            }
        };
        let (check, ()) = tokio::join!(relay_answers(port), serve("200 OK"));
        assert!(matches!(check, Check::Alive));
        let (check, ()) = tokio::join!(relay_answers(port), serve("502 Bad Gateway"));
        assert!(matches!(check, Check::Down(e) if e.contains("502")));
        drop(listener);
        assert!(matches!(relay_answers(port).await, Check::Down(_)));
        assert_eq!(status_of(b"HTTP/1.1 204 No Content\r\n"), Some(204));
        assert_eq!(status_of(b"SSH-2.0-x\r\n"), None);
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
    fn a_forward_is_ready_on_the_port_it_listens_on() {
        let mut ready = Ready::default();
        assert_eq!(ready.update("SSH: Forwarding from 127.0.0.1:3341 to host port 3340."), None);
        assert_eq!(ready.update("Connected to tunnel: test"), None);
        assert_eq!(ready.update("SSH: PortForwardingService listening on ::1:3341."), None);
        assert_eq!(ready.update("SSH: PortForwardingService listening on 127.0.0.1:3341."), Some(3341));
        let mut ready = Ready::default();
        assert_eq!(ready.update("SSH: PortForwardingService listening on 127.0.0.1:3340."), None);
        assert_eq!(ready.update("Connected to tunnel: test"), Some(3340));
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
