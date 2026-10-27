//! Operating system glue: hostname, service readiness, shutdown signals, and private files.

use std::path::Path;

use anyhow::{Context, Result};
use iroh::SecretKey;

/// The host name as a valid member name: lowercase, with characters that a name may not have
/// replaced by `-`. `machine` when the OS has none, or when it is `self`.
pub fn hostname() -> String {
    name_from_host(&gethostname::gethostname().to_string_lossy())
}

fn name_from_host(host: &str) -> String {
    let name = host.trim().to_lowercase();
    // macOS gives `name.local`; the domain only makes the machine name longer.
    let name: String = name
        .split('.')
        .next()
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    if name.is_empty() || name == "self" { "machine".into() } else { name }
}

/// Ctrl-C, or SIGTERM from systemd. The listeners exist from this call on, so a signal that
/// arrives before the future is polled is not lost.
pub fn shutdown_signal() -> impl std::future::Future<Output = ()> {
    crate::platform::shutdown_signal()
}

/// For a command line program: end quietly when stdout is a closed pipe (`imesh status | head`),
/// instead of a failed write. Not for a daemon or a process that relays streams. Call it before
/// any other thread starts.
pub fn exit_on_closed_pipe() {
    crate::platform::exit_on_closed_pipe()
}

/// Tells systemd (`Type=notify`) that the daemon is ready. Does nothing elsewhere.
/// On Windows, lets processes that this one starts leave its job, as they may under sshd.
pub fn allow_job_breakaway() {
    crate::platform::allow_job_breakaway()
}

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

#[cfg(test)]
mod tests {
    #[test]
    fn host_names_become_valid_member_names() {
        for (host, name) in [("Machine1.local", "machine1"), ("self", "machine"), ("", "machine"), ("my box", "my-box")]
        {
            assert_eq!(super::name_from_host(host), name);
            crate::daemon::valid_member_name(&super::name_from_host(host)).unwrap();
        }
    }
}
