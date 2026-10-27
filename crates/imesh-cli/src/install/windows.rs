//! Windows installs.
//!
//! - **For the user** (the default): a scheduled task at logon, no administrator needed.
//! - **For the system** (`--system`, as an administrator): a Windows service that starts at boot,
//!   before anyone logs in. It runs as the virtual account `NT SERVICE\imesh`, not as SYSTEM.
//!   Its pipe is `\\.\pipe\imesh`, which the members of the local group `imesh` may open; the
//!   install creates the group and adds the user who runs it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

use super::{Env, env_file, format_env};
use crate::paths;

fn installed_exe() -> PathBuf {
    paths::data_dir().join("bin").join("imesh.exe")
}

pub(super) fn install(exe: &Path, env: &Env) -> Result<()> {
    ensure!(
        !service_exists(),
        "this computer has the imesh Windows service; remove it with `imesh daemon uninstall --system` first"
    );
    let dir = paths::data_dir();
    let target = installed_exe();
    imesh::os::create_private_dir(&dir)?;
    std::fs::create_dir_all(target.parent().expect("exe has a parent"))?;
    // Stop the running daemon first, even when the binary does not change, so new settings apply.
    stop_daemon(&target);
    if exe != target {
        let mut tries = 0;
        while let Err(e) = std::fs::copy(exe, &target) {
            tries += 1;
            if tries > 20 {
                return Err(e).with_context(|| format!("copy to {}", target.display()));
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }
    imesh::os::write_private(&env_file(), format_env(env)?.as_bytes())?;
    let log = dir.join("imesh.log");
    let ps = |p: &Path| p.display().to_string().replace('\'', "''");
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
        exe = ps(&target),
        log = ps(&log)
    );
    print!("{}", powershell(&script)?);
    wait_for_pipe()
}

pub(super) fn uninstall(purge: bool) -> Result<()> {
    let _ = powershell("Unregister-ScheduledTask -TaskName imesh -Confirm:$false -ErrorAction SilentlyContinue");
    stop_daemon(&installed_exe());
    let _ = std::fs::remove_file(installed_exe());
    if purge {
        std::fs::remove_dir_all(paths::data_dir()).ok();
    }
    Ok(())
}

/// Stops the task and the process that runs from the installed path, and nothing else: not
/// this process, which may be the installed binary itself.
fn stop_daemon(exe: &Path) {
    let _ = powershell(&format!(
        "Stop-ScheduledTask -TaskName imesh -ErrorAction SilentlyContinue; \
         Get-Process -ErrorAction SilentlyContinue | Where-Object {{ $_.Path -eq '{}' -and $_.Id -ne {} }} | Stop-Process -Force; \
         Start-Sleep -Milliseconds 500",
        exe.display().to_string().replace('\'', "''"),
        std::process::id()
    ));
}

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

fn wait_for_pipe() -> Result<()> {
    for _ in 0..100 {
        if std::fs::metadata(paths::socket()).is_ok() {
            println!("imesh daemon is running");
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    bail!("the daemon did not start; see {}", paths::data_dir().join("imesh.log").display())
}

const SERVICE: &str = "imesh";

fn service_exists() -> bool {
    powershell("if (-not (Get-Service imesh -ErrorAction SilentlyContinue)) { exit 1 }").is_ok()
}

fn system_exe() -> PathBuf {
    PathBuf::from(std::env::var_os("ProgramFiles").unwrap_or_else(|| r"C:\Program Files".into()))
        .join("imesh")
        .join("imesh.exe")
}

/// Quotes a value for a single-quoted PowerShell string.
fn ps(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

pub(super) fn install_system(exe: &Path, env: &Env) -> Result<()> {
    ensure!(!installed_exe().exists(), "this user has an imesh install; remove it with `imesh daemon uninstall` first");
    let data = paths::system_data_dir();
    let bin = system_exe();
    let log = data.join("imesh.log");
    let bin_path = format!("\"{}\" --log-file \"{}\" daemon windows-service", bin.display(), log.display());
    let script = format!(
        r#"$ErrorActionPreference = 'Stop'
$p = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $p.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {{ throw 'imesh daemon install --system needs an elevated shell' }}
$src = {src}; $bin = {bin}; $data = {data}; $binPath = {bin_path}
$svc = Get-Service {name} -ErrorAction SilentlyContinue
if ($svc) {{ Stop-Service {name} -Force -ErrorAction SilentlyContinue }}
New-Item -ItemType Directory -Force (Split-Path $bin), $data | Out-Null
if ($src -ne $bin) {{ Copy-Item -Force $src $bin }}
if (-not (Get-LocalGroup {group} -ErrorAction SilentlyContinue)) {{ New-LocalGroup {group} -Description 'May use the imesh daemon' | Out-Null }}
$me = [Security.Principal.WindowsIdentity]::GetCurrent().Name
if (-not (Get-LocalGroupMember {group} -Member $me -ErrorAction SilentlyContinue)) {{ Add-LocalGroupMember {group} -Member $me }}
if ($svc) {{ sc.exe config {name} binPath= $binPath start= auto obj= 'NT SERVICE\{name}' | Out-Null }}
else {{ sc.exe create {name} binPath= $binPath start= auto obj= 'NT SERVICE\{name}' DisplayName= 'imesh machine daemon' | Out-Null }}
if ($LASTEXITCODE -ne 0) {{ throw "sc.exe failed: $LASTEXITCODE" }}
sc.exe failure {name} reset= 86400 actions= restart/2000/restart/10000/restart/60000 | Out-Null
icacls $data /inheritance:r /grant:r 'SYSTEM:(OI)(CI)F' 'BUILTIN\Administrators:(OI)(CI)F' 'NT SERVICE\{name}:(OI)(CI)F' | Out-Null
if ($LASTEXITCODE -ne 0) {{ throw "icacls failed: $LASTEXITCODE" }}
"#,
        src = ps(&exe.display().to_string()),
        bin = ps(&bin.display().to_string()),
        data = ps(&data.display().to_string()),
        bin_path = ps(&bin_path),
        name = SERVICE,
        group = paths::SYSTEM_GROUP,
    );
    powershell(&script)?;
    imesh::os::write_private(&data.join("env"), format_env(env)?.as_bytes())?;
    powershell(&format!("Start-Service {SERVICE}"))?;
    for _ in 0..100 {
        if std::fs::metadata(paths::SYSTEM_SOCKET).is_ok() {
            println!("imesh Windows service running; it starts at boot. Pipe {}", paths::SYSTEM_SOCKET);
            println!("members of the local group {} may use it; log in again to join it", paths::SYSTEM_GROUP);
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    bail!("the service did not start; see {}", log.display())
}

pub(super) fn uninstall_system(purge: bool) -> Result<()> {
    let data = paths::system_data_dir();
    let purge = if purge {
        format!(
            "Remove-Item -Recurse -Force {} -ErrorAction SilentlyContinue; Remove-LocalGroup {} -ErrorAction SilentlyContinue",
            ps(&data.display().to_string()),
            paths::SYSTEM_GROUP
        )
    } else {
        String::new()
    };
    powershell(&format!(
        "$ErrorActionPreference = 'Stop'
Stop-Service {SERVICE} -Force -ErrorAction SilentlyContinue
sc.exe delete {SERVICE} | Out-Null
Remove-Item -Recurse -Force {} -ErrorAction SilentlyContinue
{purge}",
        ps(&system_exe().parent().expect("exe has a parent").display().to_string())
    ))?;
    Ok(())
}
