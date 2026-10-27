//! `imesh daemon install`: installs the running binary as this machine's daemon, without SSH.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::relay;

#[derive(Debug, clap::Args)]
pub struct Opts {
    /// Labels for the machine, as key=value. Repeat for more.
    #[arg(long = "label")]
    pub labels: Vec<String>,
    #[command(flatten)]
    pub relay: relay::Opts,
}

pub fn env_lines(labels: &[String], relay: &relay::Opts) -> String {
    let mut env = relay.to_env();
    if !labels.is_empty() {
        env.push_str(&format!("IMESH_LABELS={}\n", labels.join(",")));
    }
    env
}

/// Where Windows keeps the daemon settings. The daemon reads it at start.
#[cfg(windows)]
pub fn env_file() -> PathBuf {
    crate::local::default_data_dir().join("env")
}

/// Loads `VAR=value` lines into the process environment, without overriding what is set.
#[cfg(windows)]
pub fn load_env_file(path: &std::path::Path) {
    let Ok(text) = std::fs::read_to_string(path) else { return };
    for line in text.lines() {
        if let Some((k, v)) = line.trim().split_once('=')
            && !k.starts_with('#')
            && std::env::var_os(k).is_none()
        {
            // SAFETY: called before the runtime starts any other thread.
            unsafe { std::env::set_var(k, v) };
        }
    }
}

pub fn run(opts: Opts) -> Result<()> {
    let env = env_lines(&opts.labels, &opts.relay);
    let exe = std::env::current_exe().context("find the running binary")?;
    platform(exe, env)
}

#[cfg(windows)]
fn platform(exe: PathBuf, env: String) -> Result<()> {
    let dir = crate::local::default_data_dir();
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin)?;
    let target = bin.join("imesh.exe");
    if exe != target {
        // A running daemon locks the exe. Stop the task, and the daemon process started from
        // this exact path. Nothing else is stopped.
        let _ = powershell(&format!(
            "Stop-ScheduledTask -TaskName imesh -ErrorAction SilentlyContinue; \
             Get-Process imesh -ErrorAction SilentlyContinue | Where-Object {{ $_.Path -eq '{}' }} | Stop-Process -Force; \
             Start-Sleep -Milliseconds 500",
            target.display()
        ));
        let mut tries = 0;
        while let Err(e) = std::fs::copy(&exe, &target) {
            tries += 1;
            if tries > 20 {
                return Err(e).with_context(|| format!("copy to {}", target.display()));
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }
    crate::config::write_private(&env_file(), env.as_bytes())?;
    let log = dir.join("imesh.log");
    let script = format!(
        r#"$ErrorActionPreference = 'Stop'
$exe = '{exe}'
$log = '{log}'
$a = New-ScheduledTaskAction -Execute 'conhost.exe' -Argument "--headless `"$exe`" daemon --log-file `"$log`""
$t = New-ScheduledTaskTrigger -AtLogOn -User "$env:USERDOMAIN\$env:USERNAME"
$s = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 9999 -RestartInterval (New-TimeSpan -Minutes 1) -MultipleInstances IgnoreNew
try {{
  $p = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -LogonType S4U -RunLevel Limited
  Register-ScheduledTask -TaskName imesh -Action $a -Trigger $t -Settings $s -Principal $p -Force | Out-Null
  'registered imesh task (runs without a logon session)'
}} catch {{
  $p = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -LogonType Interactive -RunLevel Limited
  Register-ScheduledTask -TaskName imesh -Action $a -Trigger $t -Settings $s -Principal $p -Force | Out-Null
  'registered imesh task (runs while the user is logged on)'
}}
Start-ScheduledTask -TaskName imesh
"#,
        exe = target.display(),
        log = log.display()
    );
    print!("{}", powershell(&script)?);
    wait_for_pipe()
}

#[cfg(windows)]
fn powershell(script: &str) -> Result<String> {
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .context("run powershell")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(windows)]
fn wait_for_pipe() -> Result<()> {
    for _ in 0..100 {
        if std::fs::metadata(crate::local::DEFAULT_PATH).is_ok() {
            println!("imesh daemon is running");
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    bail!("the daemon did not start; see {}", crate::local::default_data_dir().join("imesh.log").display())
}

/// Runs the same script as `machine init` over SSH, with this binary. Needs root.
#[cfg(unix)]
fn platform(exe: PathBuf, env: String) -> Result<()> {
    use std::io::Write;
    let bytes = std::fs::read(&exe)?;
    let sha = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&bytes));
    let tmp = std::env::temp_dir().join(format!("imesh-install-{}", std::process::id()));
    std::fs::write(&tmp, &bytes)?;
    let user = std::env::var("SUDO_USER").or_else(|_| std::env::var("USER")).unwrap_or_else(|_| "root".into());
    let script = format!("IMESH_ENV='{}'\n{}", env.trim(), crate::INSTALL_SH);
    let mut child = std::process::Command::new("sh")
        .arg("-s")
        .env("IMESH_SRC", &tmp)
        .env("IMESH_SHA256", sha)
        .env("IMESH_USER", user)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("run sh")?;
    child.stdin.take().unwrap().write_all(script.as_bytes())?;
    if !child.wait()?.success() {
        bail!("install failed; run it with sudo");
    }
    Ok(())
}
