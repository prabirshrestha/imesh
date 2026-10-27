//! Where the `imesh` command keeps its daemon's files, and which local daemon a client uses.
//!
//! A service installed for a user (no root) keeps everything in the user's own directories. A
//! system daemon (a systemd unit, or a Windows service) listens at [`SYSTEM_SOCKET`], and
//! the members of [`SYSTEM_GROUP`] may use it. Clients on the machine try the user's daemon first.

use std::path::PathBuf;

use anyhow::Result;

/// Where a system daemon listens. The Linux path is also used by an install over SSH from any
/// computer.
#[cfg(unix)]
pub const SYSTEM_SOCKET: &str = "/run/imesh/imesh.sock";
#[cfg(windows)]
pub const SYSTEM_SOCKET: &str = r"\\.\pipe\imesh";
/// The variable that moves this user's imesh directory. See [`data_dir`].
pub const HOME_VAR: &str = "IMESH_HOME";

/// The arguments an installed service adds so that it uses the same imesh directory as the
/// install: `--imesh-home <dir>` when `IMESH_HOME` is set, else none.
pub fn home_args() -> Vec<String> {
    match std::env::var_os(HOME_VAR).filter(|d| !d.is_empty()) {
        Some(_) => vec!["--imesh-home".into(), data_dir().display().to_string()],
        None => vec![],
    }
}

/// Where imesh puts temporary files: `$IMESH_TMP` if it is set, else the system temporary
/// directory.
pub fn tmp_dir() -> PathBuf {
    std::env::var_os("IMESH_TMP").filter(|d| !d.is_empty()).map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

/// The group whose members may use a system daemon.
pub const SYSTEM_GROUP: &str = "imesh";

/// Where the Windows service keeps its key, database, settings, and log: `%ProgramData%\imesh`.
#[cfg(windows)]
pub fn system_data_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into())).join("imesh")
}

/// Where this user's daemon keeps its key, database, binary, and log: `$IMESH_HOME` if it is set,
/// else `~/.local/share/imesh` on Linux, `~/Library/Application Support/imesh` on macOS, and
/// `%LOCALAPPDATA%\imesh` on Windows. A system daemon gets `/var/lib/imesh` from its unit.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(HOME_VAR).filter(|d| !d.is_empty()) {
        return dir.into();
    }
    #[cfg(unix)]
    return dirs::data_dir().expect("home directory").join("imesh");
    #[cfg(windows)]
    return dirs::data_local_dir().expect("LOCALAPPDATA").join("imesh");
}

/// Where a user install puts the daemon executable: `bin/imesh` in the data directory, so the
/// install does not put a second `imesh` on the PATH. A system install uses `/usr/local/bin/imesh`
/// or `%ProgramFiles%\imesh\imesh.exe` instead, because its service user cannot read the data
/// directory of another user.
pub fn daemon_bin() -> PathBuf {
    data_dir().join("bin").join(if cfg!(windows) { "imesh.exe" } else { "imesh" })
}

/// Where an install keeps the bundle it came from, for onward installs: `bundle` in the data
/// directory. `imesh update` and remote installs find it there.
pub fn bundle_dir() -> PathBuf {
    data_dir().join("bundle")
}

/// Where this user's daemon listens: `imesh.sock` in the data directory, which only the user may
/// enter. Not `$XDG_RUNTIME_DIR`: logind deletes it when the user's last session ends, and a
/// service started at boot does not have it. On Windows, a named pipe per user,
/// `\\.\pipe\imesh-<user>`.
pub fn socket() -> PathBuf {
    #[cfg(unix)]
    return data_dir().join("imesh.sock");
    #[cfg(windows)]
    return PathBuf::from(format!(r"\\.\pipe\imesh-{}", std::env::var("USERNAME").unwrap_or_default().to_lowercase()));
}

/// The daemon a client on this machine talks to: this user's when it runs, else the system one.
pub fn find() -> PathBuf {
    let user = socket();
    if !user.exists() && std::path::Path::new(SYSTEM_SOCKET).exists() {
        return PathBuf::from(SYSTEM_SOCKET);
    }
    user
}

/// Bridges stdin and stdout to the local API. The CLI runs this over SSH.
pub async fn dial_stdio(socket: PathBuf) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let s = iroh_mesh::net::local::connect(&socket).await?;
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
