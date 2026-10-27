//! What the SSH server does beyond the machine key: session recording.
//!
//! Recording writes each terminal session on the server, so a client cannot skip it, as an
//! asciinema v2 file (the format Tailscale SSH records): a header line, then `[time, "o", data]`
//! for output and `[time, "r", "COLSxROWS"]` for a resize.

use std::{
    io::Write,
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use iroh::EndpointId;

/// The SSH settings of a daemon.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    /// Where recordings go. `None`: no recording.
    pub record: Option<PathBuf>,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

pub fn recordings_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("ssh-recordings")
}

/// One session's recording.
pub struct Recorder {
    file: std::fs::File,
    start: Instant,
}

impl Recorder {
    pub fn start(dir: &Path, peer: &EndpointId, cols: u16, rows: u16, command: Option<&str>) -> Result<Self> {
        iroh_mesh::os::create_private_dir(dir)?;
        // Unique for sessions that start in the same second, also on one connection.
        let tag = hex::encode(&iroh::SecretKey::generate().to_bytes()[..4]);
        let path = dir.join(format!("{}-{}-{tag}.cast", now(), peer.fmt_short()));
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("create {}", path.display()))?;
        #[cfg(unix)]
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        let mut header = serde_json::json!({
            "version": 2,
            "width": cols,
            "height": rows,
            "timestamp": now(),
            "env": { "IMESH_PEER": peer.to_string() },
        });
        if let Some(c) = command {
            header["command"] = c.into();
        }
        writeln!(file, "{header}")?;
        Ok(Recorder { file, start: Instant::now() })
    }

    fn event(&mut self, kind: &str, data: &str) {
        let t = self.start.elapsed().as_secs_f64();
        let _ = writeln!(self.file, "{}", serde_json::json!([t, kind, data]));
    }

    pub fn output(&mut self, data: &[u8]) {
        self.event("o", &String::from_utf8_lossy(data));
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.event("r", &format!("{cols}x{rows}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_is_asciinema_v2() {
        let dir = std::env::temp_dir().join(format!("imesh-rec-{}", std::process::id()));
        let peer = iroh::SecretKey::generate().public();
        let mut r = Recorder::start(&dir, &peer, 80, 24, None).unwrap();
        r.output(b"hi\r\n");
        r.resize(100, 30);
        drop(r);
        let f = std::fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
        let lines: Vec<serde_json::Value> =
            std::fs::read_to_string(f).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines[0]["version"], 2);
        assert_eq!(lines[0]["width"], 80);
        assert_eq!(lines[1][1], "o");
        assert_eq!(lines[1][2], "hi\r\n");
        assert_eq!(lines[2][1], "r");
        assert_eq!(lines[2][2], "100x30");
    }
}
