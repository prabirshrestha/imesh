//! Operating system glue: hostname, service readiness, shutdown signals, and private files.

use std::path::Path;

use anyhow::{Context, Result};
use iroh::SecretKey;

/// The host name, lowercase, or `machine` when the OS has none.
pub fn hostname() -> String {
    let name = gethostname::gethostname().to_string_lossy().trim().to_lowercase();
    if name.is_empty() { "machine".into() } else { name }
}

/// Ctrl-C, or SIGTERM from systemd. The listeners exist from this call on, so a signal that
/// arrives before the future is polled is not lost.
pub fn shutdown_signal() -> impl std::future::Future<Output = ()> {
    crate::platform::shutdown_signal()
}

/// Tells systemd (`Type=notify`) that the daemon is ready. Does nothing elsewhere.
pub fn sd_notify_ready() {
    crate::platform::sd_notify_ready()
}

/// Reads an iroh secret key (hex) from `path`, or creates one there, mode 0600.
pub fn load_or_create_key(path: &Path) -> Result<SecretKey> {
    match std::fs::read_to_string(path) {
        Ok(s) => s.trim().parse().context("invalid key file"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let key = SecretKey::generate();
            write_private(path, hex::encode(key.to_bytes()).as_bytes())?;
            Ok(key)
        }
        Err(e) => Err(e.into()),
    }
}

/// Creates a directory, and its missing parents, that only this user may enter.
pub fn create_private_dir(dir: &Path) -> Result<()> {
    crate::platform::create_private_dir(dir)
}

/// Writes a file with mode 0600 through a temp file and rename.
pub fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    let dir = path.parent().context("path has no parent")?;
    create_private_dir(dir)?;
    let tmp = path.with_extension("tmp");
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    crate::platform::private_file(&mut o);
    let mut f = o.open(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    // Make the rename itself durable, so a written file survives a power loss.
    crate::platform::sync_dir(dir)?;
    Ok(())
}
