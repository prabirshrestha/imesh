//! Operating system glue: hostname, service readiness, shutdown, and the SSH bridge.

use std::path::PathBuf;

use anyhow::Result;

pub fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| std::env::var("COMPUTERNAME").ok().map(|s| s.to_lowercase()))
        .unwrap_or_else(|| "machine".into())
}

/// Ctrl-C, or SIGTERM from systemd. The listeners exist from this call on, so a signal that
/// arrives before the future is polled is not lost.
pub fn shutdown_signal() -> impl std::future::Future<Output = ()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("listen for SIGTERM");
        let mut int = signal(SignalKind::interrupt()).expect("listen for SIGINT");
        async move {
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
            }
        }
    }
    #[cfg(windows)]
    {
        let mut ctrl_c = tokio::signal::windows::ctrl_c().expect("listen for Ctrl-C");
        async move {
            ctrl_c.recv().await;
        }
    }
}

#[cfg(not(unix))]
pub fn sd_notify_ready() {}

/// Tells systemd (`Type=notify`) that the daemon is ready.
#[cfg(unix)]
pub fn sd_notify_ready() {
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    let Ok(sock) = std::os::unix::net::UnixDatagram::unbound() else {
        return;
    };
    let path = path.to_string_lossy().into_owned();
    #[cfg(target_os = "linux")]
    if let Some(name) = path.strip_prefix('@') {
        use std::os::linux::net::SocketAddrExt;
        if let Ok(addr) = std::os::unix::net::SocketAddr::from_abstract_name(name) {
            let _ = sock.send_to_addr(b"READY=1", &addr);
        }
        return;
    }
    let _ = sock.send_to(b"READY=1", path);
}

/// Bridges stdin and stdout to the local API. The CLI runs this over SSH.
pub async fn dial_stdio(socket: PathBuf) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let s = crate::net::local::connect(&socket).await?;
    let (mut r, mut w) = tokio::io::split(s);
    let up = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut tokio::io::stdin(), &mut w).await;
        let _ = w.shutdown().await;
    });
    let mut stdout = tokio::io::stdout();
    tokio::io::copy(&mut r, &mut stdout).await?;
    stdout.flush().await?;
    up.abort();
    Ok(())
}
