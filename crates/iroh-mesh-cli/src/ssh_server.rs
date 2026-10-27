//! `imesh/ssh/0`: an SSH server inside the daemon, for members, as Tailscale SSH serves the
//! devices of its network. iroh has already proved the machine key, so SSH asks for no more
//! authentication. Sessions run as the daemon's user.
//! Shell and exec run in a PTY when the client asks for one, else with pipes. `sftp` serves files,
//! so `scp` and `sftp` work. `direct-tcpip` gives `ssh -L`.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use anyhow::Result;
use iroh::{
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler},
};
use iroh_mesh::daemon::Machine;
use russh::{
    Channel, ChannelId, Pty,
    server::{self, Auth, ChannelOpenHandle, Msg, Session},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::ssh_policy::{Policy, Recorder};

pub const ALPN: &[u8] = b"imesh/ssh/0";

/// The SSH host key, from the machine key, so it stays the same while the machine does.
pub fn host_key(secret: &iroh::SecretKey) -> russh::keys::PrivateKey {
    let pair = russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&secret.to_bytes());
    russh::keys::PrivateKey::from(pair)
}

/// The host key line for `known_hosts`, for the machine with endpoint ID `id`.
pub fn host_public_key(id: &iroh::EndpointId) -> Result<String> {
    let key = russh::keys::ssh_key::public::Ed25519PublicKey(*id.as_bytes());
    Ok(russh::keys::PublicKey::from(key).to_openssh()?)
}

#[derive(Debug, Clone)]
pub struct SshProtocol {
    config: Arc<server::Config>,
    policy: Arc<Policy>,
}

impl SshProtocol {
    pub fn new(m: &Machine, policy: Policy) -> Self {
        let config = server::Config {
            keys: vec![host_key(m.endpoint().secret_key())],
            methods: (&[russh::MethodKind::None][..]).into(),
            max_auth_attempts: 1,
            auth_rejection_time: std::time::Duration::from_secs(0),
            auth_rejection_time_initial: Some(std::time::Duration::from_secs(0)),
            inactivity_timeout: None,
            keepalive_interval: Some(std::time::Duration::from_secs(30)),
            ..Default::default()
        };
        SshProtocol { config: Arc::new(config), policy: Arc::new(policy) }
    }
}

impl ProtocolHandler for SshProtocol {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let peer = conn.remote_id();
        let (send, recv) = conn.accept_bi().await?;
        let stream = tokio::io::join(recv, send);
        let handler = Handler { peer: Some(peer), policy: self.policy.clone(), ..Default::default() };
        let session = server::run_stream(self.config.clone(), stream, handler)
            .await
            .map_err(|e| AcceptError::from_err(std::io::Error::other(format!("{e:#}"))))?;
        if let Err(e) = session.await {
            tracing::debug!("ssh session: {e:#}");
        }
        conn.close(0u32.into(), b"");
        Ok(())
    }
}

/// What a session channel runs, once the client asks for it.
enum Run {
    Shell,
    Exec(String),
}

/// A session channel before its program starts: the channel, the PTY request, and env values.
struct Pending {
    channel: Channel<Msg>,
    pty: Option<PtyRequest>,
    env: Vec<(String, String)>,
}

#[derive(Clone)]
struct PtyRequest {
    term: String,
    cols: u16,
    rows: u16,
}

/// Reads and drops the messages that russh also queues on `channel`. Input reaches the program
/// through [`Handler::data`]; an unread queue fills and stops the whole session.
fn drain(channel: Channel<Msg>) {
    let (mut read, _write) = channel.split();
    tokio::spawn(async move {
        let _write = _write;
        while read.wait().await.is_some() {}
    });
}

/// A running program's input side. Input goes through a queue to its own writer, so a program
/// that does not read never stops the session: its output and keepalives still flow.
enum Input {
    Pty(std::sync::mpsc::Sender<Vec<u8>>, Box<dyn portable_pty::MasterPty + Send>, Killer),
    /// The sender is `None` after end of input.
    Pipe(Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>, Killer),
}

/// Ends the program when the session's channel closes, as sshd does on disconnect.
enum Killer {
    Pty(Box<dyn portable_pty::ChildKiller + Send + Sync>),
    Pipe(Option<tokio::sync::oneshot::Sender<()>>),
}

impl Drop for Input {
    fn drop(&mut self) {
        match self {
            Input::Pty(_, _, Killer::Pty(k)) => {
                let _ = k.kill();
            }
            Input::Pipe(_, Killer::Pipe(k)) => {
                if let Some(k) = k.take() {
                    let _ = k.send(());
                }
            }
            _ => {}
        }
    }
}

#[derive(Default)]
struct Handler {
    peer: Option<iroh::EndpointId>,
    policy: Arc<Policy>,
    pending: HashMap<ChannelId, Pending>,
    running: Arc<Mutex<HashMap<ChannelId, Input>>>,
    /// The recording of each terminal channel, written as its output goes out.
    recordings: Arc<Mutex<HashMap<ChannelId, Recorder>>>,
}

impl Handler {
    /// Starts the program, then answers the request. A failure fails this channel only: the
    /// connection and its other channels stay.
    fn start(&mut self, id: ChannelId, run: Run, session: &mut Session) -> Result<()> {
        let Some(p) = self.pending.remove(&id) else {
            session.channel_failure(id)?;
            return Ok(());
        };
        match self.spawn(id, p, run, session.handle()) {
            Ok(input) => {
                self.running.lock().unwrap().insert(id, input);
                session.channel_success(id)?;
            }
            Err(e) => {
                tracing::warn!("ssh: start a session: {e:#}");
                self.recordings.lock().unwrap().remove(&id);
                session.channel_failure(id)?;
                session.close(id)?;
            }
        }
        Ok(())
    }

    fn spawn(&mut self, id: ChannelId, p: Pending, run: Run, handle: server::Handle) -> Result<Input> {
        let input = match p.pty {
            Some(pty) => {
                let rec = match (&self.policy.record, self.peer) {
                    (Some(dir), Some(peer)) => {
                        let cmd = match &run {
                            Run::Exec(c) => Some(c.as_str()),
                            Run::Shell => None,
                        };
                        Some(Recorder::start(dir, &peer, pty.cols, pty.rows, cmd)?)
                    }
                    _ => None,
                };
                if let Some(r) = rec {
                    self.recordings.lock().unwrap().insert(id, r);
                }
                spawn_pty(p.channel, run, pty, p.env, handle, self.recordings.clone())?
            }
            None => spawn_pipes(p.channel, run, p.env, handle)?,
        };
        Ok(input)
    }
}

impl server::Handler for Handler {
    type Error = anyhow::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.pending.insert(channel.id(), Pending { channel, pty: None, env: Vec::new() });
        reply.accept().await;
        Ok(())
    }

    async fn pty_request(
        &mut self,
        id: ChannelId,
        term: &str,
        cols: u32,
        rows: u32,
        _pw: u32,
        _ph: u32,
        _modes: &[(Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        match self.pending.get_mut(&id) {
            Some(p) => {
                let (cols, rows) = if cols == 0 || rows == 0 { (80, 24) } else { (cols as u16, rows as u16) };
                p.pty = Some(PtyRequest { term: term.into(), cols, rows });
                session.channel_success(id)?;
            }
            None => session.channel_failure(id)?,
        }
        Ok(())
    }

    async fn env_request(
        &mut self,
        id: ChannelId,
        name: &str,
        value: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        match self.pending.get_mut(&id) {
            Some(p) => {
                p.env.push((name.into(), value.into()));
                session.channel_success(id)?;
            }
            None => session.channel_failure(id)?,
        }
        Ok(())
    }

    async fn shell_request(&mut self, id: ChannelId, session: &mut Session) -> Result<(), Self::Error> {
        self.start(id, Run::Shell, session)
    }

    async fn exec_request(&mut self, id: ChannelId, data: &[u8], session: &mut Session) -> Result<(), Self::Error> {
        self.start(id, Run::Exec(String::from_utf8_lossy(data).into_owned()), session)
    }

    async fn subsystem_request(&mut self, id: ChannelId, name: &str, session: &mut Session) -> Result<(), Self::Error> {
        match (name, self.pending.remove(&id)) {
            ("sftp", Some(p)) => {
                session.channel_success(id)?;
                // The SFTP server runs on one end of a pipe; this task copies the other end to
                // the channel, then reports exit 0 as sshd does, so `scp` exits 0.
                let (ours, theirs) = tokio::io::duplex(256 * 1024);
                russh_sftp::server::run(theirs, crate::sftp::Sftp::default()).await;
                let handle = session.handle();
                tokio::spawn(async move {
                    let mut ch = p.channel.into_stream();
                    let mut ours = ours;
                    let _ = tokio::io::copy_bidirectional(&mut ch, &mut ours).await;
                    drop(ch);
                    finish(&handle, id, 0).await;
                });
            }
            _ => session.channel_failure(id)?,
        }
        Ok(())
    }

    async fn window_change_request(
        &mut self,
        id: ChannelId,
        cols: u32,
        rows: u32,
        _pw: u32,
        _ph: u32,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let (cols, rows) = (cols.max(1) as u16, rows.max(1) as u16);
        if let Some(Input::Pty(_, master, _)) = self.running.lock().unwrap().get(&id) {
            let _ = master.resize(size(cols, rows));
        }
        if let Some(r) = self.recordings.lock().unwrap().get_mut(&id) {
            r.resize(cols, rows);
        }
        Ok(())
    }

    async fn data(&mut self, id: ChannelId, data: &[u8], _session: &mut Session) -> Result<(), Self::Error> {
        match self.running.lock().unwrap().get(&id) {
            Some(Input::Pty(tx, _, _)) => {
                let _ = tx.send(data.to_vec());
            }
            Some(Input::Pipe(Some(tx), _)) => {
                let _ = tx.send(data.to_vec());
            }
            _ => {}
        }
        Ok(())
    }

    async fn channel_eof(&mut self, id: ChannelId, _session: &mut Session) -> Result<(), Self::Error> {
        let mut running = self.running.lock().unwrap();
        // A PTY has no end of input; the program ends when the user ends it, as with sshd.
        if let Some(Input::Pipe(stdin, _)) = running.get_mut(&id) {
            drop(stdin.take());
        }
        Ok(())
    }

    async fn channel_close(&mut self, id: ChannelId, _session: &mut Session) -> Result<(), Self::Error> {
        self.running.lock().unwrap().remove(&id);
        self.pending.remove(&id);
        Ok(())
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host: &str,
        port: u32,
        _from: &str,
        _from_port: u32,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let Ok(port) = u16::try_from(port) else {
            reply.reject(russh::ChannelOpenFailure::ConnectFailed).await;
            return Ok(());
        };
        // Connect in a task: the session handles one message at a time, so a slow connect here
        // would stop every other channel.
        let host = host.to_string();
        tokio::spawn(async move {
            let connect = tokio::net::TcpStream::connect((host.as_str(), port));
            match tokio::time::timeout(std::time::Duration::from_secs(15), connect).await {
                Ok(Ok(mut tcp)) => {
                    reply.accept().await;
                    let mut ch = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut ch, &mut tcp).await;
                }
                _ => reply.reject(russh::ChannelOpenFailure::ConnectFailed).await,
            }
        });
        Ok(())
    }
}

fn size(cols: u16, rows: u16) -> portable_pty::PtySize {
    portable_pty::PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }
}

/// The user's shell, and how it runs one command.
fn shell() -> (String, Vec<String>) {
    if cfg!(windows) {
        let ps = "powershell.exe".to_string();
        return (ps, vec!["-NoLogo".into(), "-NoProfile".into(), "-Command".into()]);
    }
    let sh = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into());
    (sh, vec!["-c".into()])
}

fn home() -> std::path::PathBuf {
    dirs::home_dir().unwrap_or_else(|| ".".into())
}

fn spawn_pty(
    channel: Channel<Msg>,
    run: Run,
    req: PtyRequest,
    env: Vec<(String, String)>,
    handle: server::Handle,
    recordings: Arc<Mutex<HashMap<ChannelId, Recorder>>>,
) -> Result<Input> {
    let pair = portable_pty::native_pty_system().openpty(size(req.cols, req.rows))?;
    let (sh, flag) = shell();
    let mut cmd = portable_pty::CommandBuilder::new(&sh);
    match run {
        // A login shell, as sshd starts it.
        Run::Shell if !cfg!(windows) => cmd.arg("-l"),
        Run::Shell => cmd.arg("-NoLogo"),
        Run::Exec(c) => {
            cmd.args(&flag);
            cmd.arg(c);
        }
    }
    cmd.cwd(home());
    cmd.env("TERM", &req.term);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = pair.slave.spawn_command(cmd)?;
    let killer = Killer::Pty(child.clone_killer());
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;
    let (input, keys) = std::sync::mpsc::channel::<Vec<u8>>();
    // ConPTY asks the terminal for the cursor position (`ESC[6n`) before it shows anything, and
    // waits. A client that does not answer (Zed's terminal, for one) then shows a blank screen.
    // So the first request on Windows gets an answer here; the client's own answer, if it sends
    // one, ConPTY ignores.
    let mut answer = cfg!(windows).then(|| input.clone());
    std::thread::spawn(move || {
        while let Ok(b) = keys.recv() {
            if writer.write_all(&b).and_then(|()| writer.flush()).is_err() {
                break;
            }
        }
    });
    let id = channel.id();
    drain(channel);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        // The end of the previous read, so a request split across two reads is found.
        let mut tail: Vec<u8> = Vec::new();
        loop {
            match std::io::Read::read(&mut reader, &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Some(a) = &answer {
                        tail.extend_from_slice(&buf[..n]);
                        if tail.windows(4).any(|w| w == b"\x1b[6n") {
                            let _ = a.send(b"\x1b[1;1R".to_vec());
                            answer = None;
                        }
                        tail.drain(..tail.len().saturating_sub(3));
                    }
                    if tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let wait = tokio::task::spawn_blocking(move || child.wait().map(|s| s.exit_code()).unwrap_or(255));
    tokio::spawn(async move {
        tokio::pin!(wait);
        let code = loop {
            tokio::select! {
                Some(b) = rx.recv() => {
                    if let Some(r) = recordings.lock().unwrap().get_mut(&id) {
                        r.output(&b);
                    }
                    if handle.data(id, b).await.is_err() {
                        break 255;
                    }
                }
                code = &mut wait => {
                    // The program ended; send what its PTY still holds.
                    while let Ok(Some(b)) = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv()).await {
                        if let Some(r) = recordings.lock().unwrap().get_mut(&id) {
                            r.output(&b);
                        }
                        let _ = handle.data(id, b).await;
                    }
                    break code.unwrap_or(255);
                }
            }
        };
        recordings.lock().unwrap().remove(&id);
        finish(&handle, id, code).await;
    });
    Ok(Input::Pty(input, pair.master, killer))
}

fn spawn_pipes(channel: Channel<Msg>, run: Run, env: Vec<(String, String)>, handle: server::Handle) -> Result<Input> {
    let (sh, flag) = shell();
    let mut cmd = tokio::process::Command::new(&sh);
    match run {
        Run::Shell => {}
        Run::Exec(c) => {
            cmd.args(&flag).arg(c);
        }
    }
    // No console window for the program, as under Windows OpenSSH. Without this flag, a console
    // program started from the daemon, which runs headless, opens a visible window.
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    cmd.current_dir(home())
        .envs(env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn()?;
    let (input, mut queued) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    if let Some(mut stdin) = child.stdin.take() {
        // Ends, and closes the program's stdin, when the client sends EOF.
        tokio::spawn(async move {
            while let Some(b) = queued.recv().await {
                if stdin.write_all(&b).await.is_err() {
                    break;
                }
            }
        });
    }
    let mut out = child.stdout.take().expect("piped");
    let mut err = child.stderr.take().expect("piped");
    let id = channel.id();
    drain(channel);
    let h = handle.clone();
    let errs = tokio::spawn(async move {
        let mut buf = vec![0u8; 32 * 1024];
        while let Ok(n) = err.read(&mut buf).await {
            if n == 0 || h.extended_data(id, 1, buf[..n].to_vec()).await.is_err() {
                break;
            }
        }
    });
    let (kill, killed) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let run = async {
            let mut buf = vec![0u8; 32 * 1024];
            while let Ok(n) = out.read(&mut buf).await {
                if n == 0 || handle.data(id, buf[..n].to_vec()).await.is_err() {
                    break;
                }
            }
            let _ = errs.await;
        };
        tokio::select! {
            () = run => {
                let code = child.wait().await.ok().and_then(|s| s.code()).unwrap_or(255) as u32;
                finish(&handle, id, code).await;
            }
            // The channel closed first: end the program, as sshd does.
            _ = killed => {
                let _ = child.kill().await;
            }
        }
    });
    Ok(Input::Pipe(Some(input), Killer::Pipe(Some(kill))))
}

async fn finish(handle: &server::Handle, id: ChannelId, code: u32) {
    let _ = handle.exit_status_request(id, code).await;
    let _ = handle.eof(id).await;
    let _ = handle.close(id).await;
}
