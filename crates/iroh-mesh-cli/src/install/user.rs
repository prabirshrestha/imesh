//! A user install: no root and no sudo. See the `install` module.
//!
//! On Linux, the daemon runs as a systemd user service when the user may keep one running after
//! logout (linger). Otherwise it runs as a detached supervisor process (`imesh service supervise`)
//! that restarts the daemon when it fails, and a `@reboot` line in the user's crontab starts it at
//! boot. On macOS, it runs as a launchd agent of the user, which starts at login and restarts it.

use std::{
    io::Write,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use tracing::warn;

use super::{Env, Service, env_file, format_env};
use iroh_mesh::os::{create_private_dir, write_private};

use crate::paths;

/// How long an install waits for the daemon's socket.
const START_TIMEOUT: Duration = Duration::from_secs(20);
/// The supervisor waits this long before it starts a failed daemon again, doubling to a minute.
const RESTART_BACKOFF: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// The crontab line that starts the daemon at boot contains this.
const CRON_MARK: &str = "service start # imesh";

fn home() -> Result<PathBuf> {
    dirs::home_dir().context("no home directory")
}

fn unit_file() -> Result<PathBuf> {
    Ok(dirs::config_dir().context("no config directory")?.join("systemd").join("user").join("imesh.service"))
}

fn pid_file() -> PathBuf {
    paths::data_dir().join("supervisor.pid")
}

fn log_file() -> PathBuf {
    paths::data_dir().join("imesh.log")
}

pub(super) fn install(exe: &Path, env: &Env, service: Service) -> Result<()> {
    let bin = paths::daemon_bin();
    stop_all()?;
    place(exe, &bin)?;
    write_private(&env_file(), format_env(env)?.as_bytes())?;
    if cfg!(target_os = "macos") && service != Service::Detached {
        launchd::install(&bin, &log_file())?;
        wait_for_socket(false)?;
        println!("imesh daemon running for {} as a launchd agent; it starts at login", whoami());
        println!("socket {}", paths::socket().display());
        return Ok(());
    }
    let systemd = match service {
        Service::Detached => false,
        Service::Systemd => {
            ensure!(systemd_user(), "no systemd user manager here; use --service detached");
            ensure!(linger(), "systemd would stop the daemon at logout: `loginctl enable-linger` was refused");
            true
        }
        Service::Auto => systemd_user() && linger(),
    };
    let how = if systemd {
        remove_cron_line();
        install_unit(&bin)?;
        "a systemd user service; it starts at boot and keeps running after logout"
    } else {
        remove_unit();
        if kills_user_processes() {
            eprintln!(
                "warning: logind stops this user's processes at logout, and linger is off, so the service stops \
                 when your last session ends. Ask an administrator to run `loginctl enable-linger {}`, then run \
                 the install again.",
                whoami()
            );
        }
        let at_boot = add_cron_line(&bin).inspect_err(|e| warn!("no start at boot: {e:#}")).is_ok();
        spawn_supervisor(&bin)?;
        if at_boot {
            "a background process that restarts itself; your crontab starts it at boot"
        } else {
            "a background process that restarts itself; run `imesh service start` after a reboot"
        }
    };
    wait_for_socket(systemd)?;
    println!("imesh daemon running for {} as {how}", whoami());
    println!("socket {}", paths::socket().display());
    Ok(())
}

pub(super) fn uninstall(purge: bool) -> Result<()> {
    // The data must not go while a daemon still runs on it.
    stop_all()?;
    remove_unit();
    remove_cron_line();
    launchd::remove();
    let _ = std::fs::remove_file(paths::daemon_bin());
    if purge {
        let _ = std::fs::remove_dir_all(paths::data_dir());
        let _ = std::fs::remove_file(env_file());
    }
    Ok(())
}

/// Copies the running binary to [`paths::daemon_bin`]. The rename replaces it at once, also while
/// the old one runs.
fn place(exe: &Path, bin: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if exe.canonicalize().ok() == bin.canonicalize().ok() {
        return Ok(());
    }
    std::fs::create_dir_all(bin.parent().expect("bin has a parent"))?;
    let new = bin.with_extension("new");
    std::fs::copy(exe, &new).with_context(|| format!("copy to {}", new.display()))?;
    std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&new, bin)?;
    Ok(())
}

fn run(cmd: &mut Command) -> Result<String> {
    let out = cmd.stdin(Stdio::null()).output().with_context(|| format!("run {cmd:?}"))?;
    ensure!(
        out.status.success(),
        "{cmd:?}: {}{}",
        String::from_utf8_lossy(&out.stdout).trim(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn systemctl(args: &[&str]) -> Result<String> {
    run(Command::new("systemctl").arg("--user").args(args))
}

/// Whether this login has a systemd user manager.
fn systemd_user() -> bool {
    systemctl(&["show-environment"]).is_ok()
}

/// Whether logind stops a user's processes when the user's last session ends
/// (`KillUserProcesses=yes`), which also stops a detached daemon.
fn kills_user_processes() -> bool {
    run(Command::new("busctl").args([
        "get-property",
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
        "KillUserProcesses",
    ]))
    .is_ok_and(|v| v.trim() == "b true")
}

/// Whether the user's services start at boot and outlive the login. Turns it on when it is off,
/// which logind allows a user for themselves on most systems.
fn linger() -> bool {
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() }.to_string();
    let on = || {
        run(Command::new("loginctl").args(["show-user", &uid, "-p", "Linger", "--value"]))
            .is_ok_and(|v| v.trim() == "yes")
    };
    on() || (run(Command::new("loginctl").arg("enable-linger")).is_ok() && on())
}

fn install_unit(bin: &Path) -> Result<()> {
    let unit = format!(
        "[Unit]\nDescription=imesh machine daemon\n\n\
         [Service]\nType=notify\nExecStart=\"{}\" service run{}\nRestart=always\nRestartSec=2\n\n\
         [Install]\nWantedBy=default.target\n",
        bin.display(),
        paths::home_args().iter().map(|a| format!(" \"{a}\"")).collect::<String>()
    );
    let path = unit_file()?;
    std::fs::create_dir_all(path.parent().expect("unit has a parent"))?;
    std::fs::write(&path, unit)?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "imesh.service"])?;
    systemctl(&["restart", "imesh.service"])?;
    Ok(())
}

fn remove_unit() {
    let Ok(path) = unit_file() else { return };
    if path.exists() {
        let _ = systemctl(&["disable", "--now", "imesh.service"]);
        let _ = std::fs::remove_file(path);
        let _ = systemctl(&["daemon-reload"]);
    }
}

/// Stops the daemon however it runs.
pub(super) fn stop_all() -> Result<()> {
    launchd::stop();
    if unit_file().is_ok_and(|p| p.exists()) {
        systemctl(&["stop", "imesh.service"])?;
    }
    stop()
}

fn crontab() -> Result<Vec<String>> {
    // No crontab yet is an error for `crontab -l`, and means no lines.
    let out = Command::new("crontab").arg("-l").stdin(Stdio::null()).output().context("run crontab")?;
    Ok(String::from_utf8_lossy(&out.stdout).lines().map(String::from).collect())
}

fn set_crontab(lines: &[String]) -> Result<()> {
    let mut child = Command::new("crontab").arg("-").stdin(Stdio::piped()).spawn().context("run crontab")?;
    let mut text = lines.join("\n");
    text.push('\n');
    child.stdin.take().expect("piped").write_all(text.as_bytes())?;
    ensure!(child.wait()?.success(), "crontab refused the new table");
    Ok(())
}

fn add_cron_line(bin: &Path) -> Result<()> {
    let mut lines = crontab()?;
    lines.retain(|l| !l.contains(CRON_MARK));
    // cron reads `%` as a line break; quote the path for the shell.
    let bin = bin.display().to_string();
    ensure!(!bin.contains(['%', '\'']), "cannot put {bin} in a crontab");
    let home: String = paths::home_args().iter().map(|a| format!("'{a}' ")).collect();
    ensure!(!paths::home_args().iter().any(|a| a.contains(['%', '\''])), "cannot put IMESH_HOME in a crontab");
    lines.push(format!("@reboot '{bin}' {home}{CRON_MARK}"));
    set_crontab(&lines)
}

fn remove_cron_line() {
    if let Ok(lines) = crontab()
        && lines.iter().any(|l| l.contains(CRON_MARK))
    {
        let kept: Vec<_> = lines.into_iter().filter(|l| !l.contains(CRON_MARK)).collect();
        let _ = set_crontab(&kept);
    }
}

fn whoami() -> String {
    std::env::var("USER").unwrap_or_else(|_| "this user".into())
}

fn wait_for_socket(systemd: bool) -> Result<()> {
    let socket = paths::socket();
    let start = Instant::now();
    while start.elapsed() < START_TIMEOUT {
        // A socket file can outlive a crashed daemon; only one that answers counts.
        if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let logs = if systemd { "journalctl --user -u imesh".to_string() } else { log_file().display().to_string() };
    bail!("the daemon did not open {} within {START_TIMEOUT:?}; see {logs}", socket.display())
}

/// The supervisor's PID, if one runs. A running supervisor holds a lock on its PID file, so a
/// stale file from before a reboot or crash names no process here.
fn supervisor() -> Option<i32> {
    let file = std::fs::File::open(pid_file()).ok()?;
    match file.try_lock_shared() {
        Err(std::fs::TryLockError::WouldBlock) => std::fs::read_to_string(pid_file()).ok()?.trim().parse().ok(),
        _ => None,
    }
}

fn spawn_supervisor(bin: &Path) -> Result<()> {
    if supervisor().is_some() {
        return Ok(());
    }
    create_private_dir(&paths::data_dir())?;
    let log = open_log()?;
    let mut cmd = Command::new(bin);
    cmd.args(["service", "supervise"])
        .args(paths::home_args())
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // SAFETY: setsid is async-signal-safe. A session of its own keeps the supervisor running
    // after the login that started it ends.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("start the supervisor")?;
    Ok(())
}

fn open_log() -> Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    Ok(std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(log_file())?)
}

/// Starts an installed daemon however it was installed. Waits for its socket.
pub(super) fn ensure_started() -> Result<()> {
    // On macOS the daemon runs under launchd; a binary that was only copied gets its agent here.
    if cfg!(target_os = "macos") && !launchd::installed() && supervisor().is_none() {
        launchd::install(&paths::daemon_bin(), &log_file())?;
        return wait_for_socket(false);
    }
    if launchd::installed() {
        launchd::start()?;
        return wait_for_socket(false);
    }
    if unit_file().is_ok_and(|p| p.exists()) {
        systemctl(&["start", "imesh.service"])?;
        return wait_for_socket(true);
    }
    spawn_supervisor(&paths::daemon_bin())?;
    wait_for_socket(false)
}

/// `imesh service stop`: stops the detached supervisor and its daemon.
pub fn stop() -> Result<()> {
    let Some(pid) = supervisor() else { return Ok(()) };
    // SAFETY: a plain signal to a process we checked.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let start = Instant::now();
    // The lock goes with the supervisor.
    while supervisor().is_some() {
        if start.elapsed() > START_TIMEOUT {
            return Err(anyhow!("the supervisor ({pid}) did not stop"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// `imesh service supervise`: runs the daemon and starts it again when it fails. A clean exit, or a
/// shutdown signal to the supervisor, stops both.
pub fn supervise() -> Result<()> {
    let exe = std::env::current_exe()?;
    create_private_dir(&paths::data_dir())?;
    // Hold the lock for the supervisor's life: a second supervisor stops here.
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(pid_file())?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => bail!("a supervisor already runs"),
        Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
    }
    lock.set_len(0)?;
    (&lock).write_all(std::process::id().to_string().as_bytes())?;
    let result = tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
        let signal = iroh_mesh::os::shutdown_signal();
        tokio::pin!(signal);
        let mut backoff = RESTART_BACKOFF;
        loop {
            let started = Instant::now();
            let mut child = tokio::process::Command::new(&exe)
                .args(["service", "run"])
                .args(paths::home_args())
                .spawn()
                .context("start the daemon")?;
            tokio::select! {
                status = child.wait() => {
                    let status = status?;
                    if status.success() {
                        return anyhow::Ok(());
                    }
                    if started.elapsed() > MAX_BACKOFF {
                        backoff = RESTART_BACKOFF;
                    }
                    warn!("the daemon exited ({status}); starting it again in {backoff:?}");
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = &mut signal => return Ok(()),
                    }
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
                _ = &mut signal => {
                    if let Some(pid) = child.id() {
                        // SAFETY: a plain signal to our own child.
                        unsafe { libc::kill(pid as i32, libc::SIGTERM) };
                    }
                    let _ = child.wait().await;
                    return Ok(());
                }
            }
        }
    });
    drop(lock);
    result
}

/// macOS: a launchd agent in `~/Library/LaunchAgents`, for the user. It needs no root. launchd
/// starts it at login and again when it exits.
mod launchd {
    use super::*;

    const LABEL: &str = "dev.imesh.daemon";

    fn plist() -> Result<PathBuf> {
        Ok(home()?.join("Library").join("LaunchAgents").join(format!("{LABEL}.plist")))
    }

    fn domain() -> String {
        // SAFETY: getuid has no preconditions.
        format!("gui/{}", unsafe { libc::getuid() })
    }

    fn launchctl(args: &[&str]) -> Result<String> {
        run(Command::new("launchctl").args(args))
    }

    fn xml(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
    }

    pub(super) fn installed() -> bool {
        cfg!(target_os = "macos") && plist().is_ok_and(|p| p.exists())
    }

    /// What the agent needs from the installing shell. launchd gives agents only the system
    /// PATH, so keep the shell's: the daemon then finds devtunnel from Homebrew. Keep the config
    /// directory too, so the daemon reads the settings file that the install wrote.
    fn agent_env() -> String {
        let mut env = vec![(
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin:/usr/sbin:/sbin".into()),
        )];
        for k in ["IMESH_CONFIG_DIR", "XDG_CONFIG_HOME", "HTTPS_PROXY", "HTTP_PROXY", "NO_PROXY"] {
            if let Ok(v) = std::env::var(k) {
                env.push((k.to_string(), v));
            }
        }
        env.iter().map(|(k, v)| format!("<key>{}</key><string>{}</string>", xml(k), xml(v))).collect()
    }

    pub(super) fn install(bin: &Path, log: &Path) -> Result<()> {
        create_private_dir(&paths::data_dir())?;
        let path = plist()?;
        std::fs::create_dir_all(path.parent().expect("plist has a parent"))?;
        let text = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{}</string><string>service</string><string>run</string>{home}</array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>2</integer>
  <key>EnvironmentVariables</key><dict>{env}</dict>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
            xml(&bin.display().to_string()),
            log = xml(&log.display().to_string()),
            env = agent_env(),
            home = paths::home_args().iter().map(|a| format!("<string>{}</string>", xml(a))).collect::<String>(),
        );
        std::fs::write(&path, text)?;
        let target = path.display().to_string();
        let _ = launchctl(&["bootout", &domain(), &target]);
        launchctl(&["bootstrap", &domain(), &target])?;
        Ok(())
    }

    pub(super) fn start() -> Result<()> {
        let target = plist()?.display().to_string();
        if launchctl(&["bootstrap", &domain(), &target]).is_err() {
            launchctl(&["kickstart", &format!("{}/{LABEL}", domain())])?;
        }
        Ok(())
    }

    pub(super) fn stop() {
        if installed()
            && let Ok(p) = plist()
        {
            let _ = launchctl(&["bootout", &domain(), &p.display().to_string()]);
        }
    }

    pub(super) fn remove() {
        stop();
        if let Ok(p) = plist() {
            let _ = std::fs::remove_file(p);
        }
    }
}
