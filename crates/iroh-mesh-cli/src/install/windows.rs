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

pub(super) fn install(exe: &Path, env: &Env) -> Result<()> {
    install_task(exe, env, true)
}

/// Registers the scheduled task and starts it. With `copy`, also puts `exe` at the installed path.
fn install_task(exe: &Path, env: &Env, copy: bool) -> Result<()> {
    ensure!(
        !service_exists(),
        "this computer has the imesh Windows service; remove it with `imesh service uninstall --system` first"
    );
    let dir = paths::data_dir();
    let target = paths::daemon_bin();
    iroh_mesh::os::create_private_dir(&dir)?;
    std::fs::create_dir_all(target.parent().expect("exe has a parent"))?;
    // Stop the running daemon first, even when the binary does not change, so new settings apply.
    stop_daemon(&target);
    if copy && exe != target {
        let mut tries = 0;
        while let Err(e) = std::fs::copy(exe, &target) {
            tries += 1;
            // A running exe cannot be replaced, but it can be renamed. The CLI that started this
            // update may run from the installed path.
            if tries == 4 && target.exists() {
                let old = target.with_extension(format!("old-{}.exe", std::process::id()));
                if std::fs::rename(&target, &old).is_ok() {
                    continue;
                }
            }
            if tries > 20 {
                return Err(e).with_context(|| format!("copy to {}", target.display()));
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        remove_old_exes(&target);
    }
    if copy || !env_file().exists() {
        iroh_mesh::os::write_private(&env_file(), format_env(env)?.as_bytes())?;
    }
    let log = dir.join("imesh.log");
    let ps = |p: &Path| p.display().to_string().replace('\'', "''");
    let script = format!(
        r#"$ErrorActionPreference = 'Stop'
$exe = '{exe}'
$log = '{log}'
$a = New-ScheduledTaskAction -Execute 'conhost.exe' -Argument "--headless `"$exe`" service run --log-file `"$log`"{home}"
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
        log = ps(&log),
        home = paths::home_args().iter().map(|a| format!(" `\"{}`\"", a.replace('\'', "''"))).collect::<String>()
    );
    print!("{}", powershell(&script)?);
    wait_for_pipe()
}

/// Removes executables that earlier updates renamed. One that still runs stays until next time.
fn remove_old_exes(target: &Path) {
    let Some(dir) = target.parent() else { return };
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.file_name().to_string_lossy().starts_with("imesh.old-") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Starts the installed task and waits for its pipe. An executable that was only copied to the
/// installed path has no task yet: register it, with the settings the install keeps, if any.
pub(super) fn ensure_started() -> Result<()> {
    if service_exists() {
        powershell("Start-Service imesh")?;
        return wait_for_pipe();
    }
    if !task_exists() {
        return install_task(&paths::daemon_bin(), &Env::new(), false);
    }
    powershell("Start-ScheduledTask -TaskName imesh")?;
    wait_for_pipe()
}

fn task_exists() -> bool {
    powershell("if (-not (Get-ScheduledTask -TaskName imesh -ErrorAction SilentlyContinue)) { exit 1 }").is_ok()
}

/// Whether the Windows service (a system install) exists.
pub(super) fn system_installed() -> bool {
    service_exists()
}

pub(super) fn uninstall(purge: bool) -> Result<()> {
    let _ = powershell("Unregister-ScheduledTask -TaskName imesh -Confirm:$false -ErrorAction SilentlyContinue");
    stop_daemon(&paths::daemon_bin());
    // The running CLI may be the installed exe, which Windows does not let it delete.
    if std::env::current_exe().ok().as_deref() != Some(paths::daemon_bin().as_path()) {
        let _ = std::fs::remove_file(paths::daemon_bin());
    }
    if purge {
        let dir = paths::data_dir();
        // A stopped daemon can hold its files open for a short time.
        let mut last = None;
        for _ in 0..20 {
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => last = Some(e),
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        bail!(
            "could not delete {}: {}. Stop every imesh daemon (taskkill /im imesh.exe /f) and try again",
            dir.display(),
            last.map(|e| e.to_string()).unwrap_or_default()
        );
    }
    Ok(())
}

/// Stops the task and every `imesh service run` process of this user, from any path, and nothing
/// else: not this process, which may be the installed binary itself.
fn stop_daemon(exe: &Path) {
    let _ = exe;
    let _ = powershell(&format!(
        "Stop-ScheduledTask -TaskName imesh -ErrorAction SilentlyContinue; \
         Get-CimInstance Win32_Process -Filter \"Name='imesh.exe'\" -ErrorAction SilentlyContinue | Where-Object {{ $_.CommandLine -match '(?:^|\\s)service\\s+run(?:\\s|$)' -and $_.ProcessId -ne {} -and $_.ProcessId -ne {} -and (Invoke-CimMethod -InputObject $_ -MethodName GetOwner).User -eq $env:USERNAME }} | ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force }}; \
         Start-Sleep -Milliseconds 500",
        std::process::id(),
        // `imesh update` from the installed exe runs the new one; it must not stop its caller.
        std::env::var("IMESH_CALLER_PID").ok().and_then(|p| p.parse::<u32>().ok()).unwrap_or(0)
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
    ensure!(
        !paths::daemon_bin().exists(),
        "this user has an imesh install; remove it with `imesh service uninstall` first"
    );
    let data = paths::system_data_dir();
    let bin = system_exe();
    let log = data.join("imesh.log");
    let bin_path = format!("\"{}\" --log-file \"{}\" service windows-service", bin.display(), log.display());
    let script = format!(
        r#"$ErrorActionPreference = 'Stop'
$p = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $p.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {{ throw 'imesh service install --system needs an elevated shell' }}
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
    iroh_mesh::os::write_private(&data.join("env"), format_env(env)?.as_bytes())?;
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

pub(super) fn stop_service() -> Result<()> {
    if service_exists() {
        powershell("Stop-Service imesh -ErrorAction Stop")?;
    } else {
        stop_daemon(&paths::daemon_bin());
    }
    Ok(())
}
