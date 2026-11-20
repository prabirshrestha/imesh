//! The devtunnel CLI: find it and check its login.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, ensure};
use serde_json::Value;

/// Checks the devtunnel CLI and its login, with deadlines. Never installs it or logs in.
pub async fn check_devtunnel() -> Result<()> {
    check_devtunnel_at(&devtunnel_cli()).await
}

/// The devtunnel CLI: from the PATH, else, on Unix, from the PATH of the user's login shell. A
/// command over SSH and a service have a shorter PATH than a login shell: Microsoft's Linux
/// installer puts the CLI in `~/bin`, which only `~/.profile` adds.
pub fn devtunnel_cli() -> PathBuf {
    let name = if cfg!(windows) { "devtunnel.exe" } else { "devtunnel" };
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|p| p.join(name))
        .find(|p| p.is_file())
        .or_else(from_login_shell)
        .and_then(|p| p.canonicalize().ok())
        .unwrap_or_else(|| name.into())
}

/// `devtunnel` as the user's shell finds it, with its profile and rc files, as in a terminal.
/// A shell that is not POSIX may reject `command -v`, so `/bin/sh` tries after it.
fn from_login_shell() -> Option<PathBuf> {
    if cfg!(windows) {
        return None;
    }
    let shell = std::env::var_os("SHELL").filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into());
    [(shell, "-ilc"), ("/bin/sh".into(), "-lc")].into_iter().find_map(|(shell, flags)| {
        let out = std::process::Command::new(shell)
            .args([flags, "command -v devtunnel"])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        // An rc file may print other lines; the path is the line that names a file.
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| PathBuf::from(l.trim()))
            .find(|p| p.is_absolute() && p.is_file())
    })
}

pub async fn check_devtunnel_at(cli: &Path) -> Result<()> {
    let run = |args: &'static [&'static str]| async move {
        let mut c = tokio::process::Command::new(cli);
        c.args(args).kill_on_drop(true);
        tokio::time::timeout(Duration::from_secs(30), c.output()).await
    };
    let v = run(&["--version"]).await.map_err(|_| anyhow!("`devtunnel --version` did not answer in 30 s"))?;
    v.ok().filter(|o| o.status.success()).context(
        "the devtunnel CLI is missing: install it (https://aka.ms/devtunnels/cli), then run `devtunnel user login`",
    )?;
    let u = run(&["user", "show", "--json"]).await.map_err(|_| anyhow!("`devtunnel user show` did not answer"))?;
    let u = u.context("run devtunnel")?;
    let status: Value = serde_json::from_slice(&u.stdout).unwrap_or(Value::Null);
    ensure!(
        u.status.success() && status["status"] == "Logged in",
        "devtunnel is not logged in: run `devtunnel user login`"
    );
    Ok(())
}
