//! A system install: a systemd unit, a service user, and the socket group `imesh`. Needs root.

use std::{io::Write, path::Path, process::Stdio};

use anyhow::{Context, Result, bail, ensure};

use super::{Env, format_env, is_name, sha256};

const INSTALL_SH: &str = include_str!("install.sh");

pub(super) fn install(exe: &Path, env: &Env, run_as: Option<&str>, run_group: Option<&str>) -> Result<()> {
    // SAFETY: geteuid has no preconditions.
    ensure!(unsafe { libc::geteuid() } == 0, "--system needs root; without it, imesh installs for your user");
    // The user who asked: through sudo, or root itself.
    let asked = std::env::var("SUDO_USER").unwrap_or_else(|_| "root".into());
    let run_user =
        run_as.map(str::to_string).unwrap_or_else(|| if asked == "root" { "imesh".into() } else { asked.clone() });
    ensure!(is_name(&asked) && is_name(&run_user), "bad user name");
    if let Some(g) = run_group {
        ensure!(is_name(g), "bad group name {g:?}");
    }
    let sha = sha256(&std::fs::read(exe)?);
    // The settings go base64-encoded, so no value can break out of the script.
    let script = format!("IMESH_ENV_B64={}\n{INSTALL_SH}", data_encoding::BASE64.encode(format_env(env)?.as_bytes()));
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-s")
        .env("IMESH_SRC", exe)
        .env("IMESH_SHA256", sha)
        .env("IMESH_USER", asked)
        .env("IMESH_RUN_USER", run_user)
        .stdin(Stdio::piped());
    cmd.env("IMESH_RUN_GROUP", run_group.unwrap_or(crate::paths::SYSTEM_GROUP));
    let mut child = cmd.spawn().context("run sh")?;
    child.stdin.take().expect("piped").write_all(script.as_bytes())?;
    if !child.wait()?.success() {
        bail!("system install failed");
    }
    Ok(())
}

pub(super) fn uninstall(purge: bool) -> Result<()> {
    // SAFETY: geteuid has no preconditions.
    ensure!(unsafe { libc::geteuid() } == 0, "removing the system daemon needs root");
    let data = if purge { " /var/lib/imesh /etc/imesh" } else { "" };
    let script = format!(
        "systemctl disable --now imesh.service 2>/dev/null || true\n\
         rm -f /etc/systemd/system/imesh.service /usr/local/bin/imesh\n\
         systemctl daemon-reload\n\
         rm -rf{data}\n"
    );
    let status = std::process::Command::new("sh").arg("-c").arg(script).status()?;
    ensure!(status.success(), "uninstall failed");
    Ok(())
}
