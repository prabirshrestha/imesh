//! Installing on Windows over SSH, for the SSH user: a scheduled task, as `imesh service install`
//! does there. Commands are fixed PowerShell scripts sent as `-EncodedCommand`; data goes on stdin
//! or base64-encoded, never as script text.

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

use iroh_mesh::client::Client;

use super::{
    Env, Service, format_env_for, parse_env,
    remote::{Mode, Target},
};
use crate::{connect, ssh::Ssh};

/// What the Windows host reports before an install.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Facts {
    arch: String,
    data: String,
    /// SHA-256 of `%LOCALAPPDATA%\imesh\bin\imesh.exe`, lowercase, if installed.
    exe_sha: Option<String>,
    /// SHA-256 of the kept bundle ZIP.
    bundle_sha: Option<String>,
    env: Option<String>,
    /// `imesh service status` of the installed exe.
    status: Option<String>,
    /// Whether the Windows service (a system install) exists.
    service: bool,
}

const FACTS_PS: &str = r#"$ErrorActionPreference = 'Stop'
$data = Join-Path $env:LOCALAPPDATA 'imesh'
$exe = Join-Path $data 'bin\imesh.exe'
$sha = $null; $status = $null
if (Test-Path $exe) {
  $sha = (Get-FileHash -Algorithm SHA256 $exe).Hash.ToLower()
  $status = (& $exe service status 2>$null | Out-String).Trim()
}
$b = Join-Path $data 'bundle\imesh.zip.sha256'
$bundle = if (Test-Path $b) { (Get-Content -Raw $b).Trim() } else { $null }
$e = Join-Path $data 'env'
$envText = if (Test-Path $e) { Get-Content -Raw $e } else { $null }
$arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
[pscustomobject]@{ arch = $arch; data = $data; exeSha = $sha; bundleSha = $bundle; env = $envText; status = $status;
  service = [bool](Get-Service imesh -ErrorAction SilentlyContinue) } | ConvertTo-Json -Compress
"#;

/// Writes stdin to a new private staging directory and expands the ZIP there. Prints the directory.
const UPLOAD_PS: &str = r#"$ErrorActionPreference = 'Stop'
$data = Join-Path $env:LOCALAPPDATA 'imesh'
New-Item -ItemType Directory -Force $data | Out-Null
$stage = Join-Path $data ('upload-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory $stage | Out-Null
$zip = Join-Path $stage 'imesh.zip'
$in = [Console]::OpenStandardInput()
$out = [IO.File]::Create($zip)
try { $in.CopyTo($out) } finally { $out.Close() }
Expand-Archive -LiteralPath $zip -DestinationPath (Join-Path $stage 'x')
$stage
"#;

/// Runs the install from the staged or installed exe. Its inputs come base64-encoded in the
/// prelude: the exe path, its SHA-256, the ZIP path or empty, and the settings, which go to the
/// installer on stdin.
const INSTALL_PS: &str = r#"$ErrorActionPreference = 'Stop'
function d($s) { [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($s)) }
$exe = d $p_exe; $sha = d $p_sha; $zip = d $p_zip
if ((Get-FileHash -Algorithm SHA256 $exe).Hash.ToLower() -ne $sha) { throw "checksum mismatch: $exe" }
$a = @('service', 'install', '--settings-stdin', '--sha256', $sha)
if ($zip) { $a += @('--bundle', $zip) }
$OutputEncoding = New-Object Text.UTF8Encoding $false
(d $p_env) | & $exe @a
if ($LASTEXITCODE) { exit $LASTEXITCODE }
"#;

const CLEANUP_PS: &str = r#"function d($s) { [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($s)) }
$stage = d $p_stage
$data = Join-Path $env:LOCALAPPDATA 'imesh'
if ($stage.StartsWith($data) -and (Split-Path -Leaf $stage).StartsWith('upload-')) { Remove-Item -Recurse -Force -LiteralPath $stage -ErrorAction SilentlyContinue }
"#;

fn b64(s: &str) -> String {
    data_encoding::BASE64.encode(s.as_bytes())
}

/// Sets `$p_<name>` to base64 text, which has no quotes.
fn prelude(vars: &[(&str, &str)]) -> String {
    vars.iter().map(|(k, v)| format!("$p_{k} = '{}'\n", b64(v))).collect()
}

#[derive(Debug, Deserialize)]
struct Status {
    running: bool,
    cluster: Option<String>,
    #[serde(default)]
    alone_and_empty: bool,
}

pub(super) async fn provision(target: &Target, ssh: &Ssh, mode: Mode) -> Result<Client> {
    ensure!(
        !target.system && target.run_as.is_none() && target.run_as_group.is_none() && !target.docker,
        "a remote Windows install is for the SSH user only: drop --system, --run-as, and --docker"
    );
    ensure!(target.service == Service::Auto, "--service is for Linux");
    let out = ssh.powershell(FACTS_PS, b"").await.context("inspect the Windows host")?;
    let f: Facts = serde_json::from_str(out.lines().rev().find(|l| l.starts_with('{')).unwrap_or_default())
        .with_context(|| format!("unexpected answer from {}: {out}", ssh.dest()))?;
    ensure!(
        !f.service,
        "{} has the imesh Windows service (a system install); a remote install is per user",
        ssh.dest()
    );
    let status: Option<Status> = f.status.as_deref().and_then(|s| serde_json::from_str(s).ok());
    match mode {
        Mode::New | Mode::NewCluster => {
            if let Some(s) = &status
                && let Some(c) = &s.cluster
                && (mode == Mode::NewCluster || !s.alone_and_empty)
            {
                bail!("{} is already in cluster {c}; run `imesh logout` there first", ssh.dest());
            }
            if f.exe_sha.is_some() && status.is_none() {
                bail!("{} has imesh installed, but its daemon does not answer; fix or update it first", ssh.dest());
            }
        }
        Mode::Update => {
            ensure!(f.exe_sha.is_some(), "{} has no imesh installed; use `imesh add`", ssh.dest());
        }
    }
    let current = parse_env(f.env.as_deref().unwrap_or_default());
    let env: Env = match mode {
        Mode::New | Mode::NewCluster => target.settings.to_env(),
        Mode::Update => {
            let mut env = current.clone();
            target.settings.net.clear_replaced_env(&mut env);
            env.extend(target.settings.to_env());
            env
        }
    };
    let env_text = format_env_for(&env, true)?;
    let p = target.load_binary(crate::bundle::triple("windows", &f.arch)?)?;
    let bundle = p.zip.as_ref().context("a Windows install needs a bundle; --binary is for Linux")?;
    let same_exe = f.exe_sha.as_deref() == Some(p.sha.as_str());
    let same_bundle = f.bundle_sha.as_deref() == Some(bundle.zip_sha256.as_str());
    if mode == Mode::Update && same_exe && same_bundle && env == current && status.as_ref().is_some_and(|s| s.running) {
        eprintln!("{} is up to date; nothing to change", ssh.dest());
        return open(target, ssh).await;
    }
    let installed = format!("{}\\bin\\imesh.exe", f.data);
    let (exe, zip, stage) = if same_exe && same_bundle {
        (installed, String::new(), None)
    } else {
        let bytes = std::fs::read(&bundle.zip)?;
        eprintln!("uploading the bundle {} ({} MB) to {}", bundle.zip.display(), bytes.len() >> 20, ssh.dest());
        let stage = ssh.powershell(UPLOAD_PS, &bytes).await.context("upload the bundle")?;
        let stage = stage.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or_default().to_string();
        ensure!(stage.contains("upload-"), "unexpected answer to the upload: {stage}");
        let (a, _) = bundle.artifact(crate::bundle::triple("windows", &f.arch)?)?;
        let exe = format!("{stage}\\x\\{}", a.path.replace('/', "\\"));
        (exe, format!("{stage}\\imesh.zip"), Some(stage))
    };
    eprintln!("installing imesh on {} for the SSH user, as a scheduled task", ssh.dest());
    let script = prelude(&[("exe", &exe), ("sha", &p.sha), ("zip", &zip)])
        + &format!("$p_env = '{}'\n", b64(&env_text))
        + INSTALL_PS;
    let result = ssh.powershell(&script, b"").await;
    if let Some(stage) = stage {
        let _ = ssh.powershell(&(prelude(&[("stage", &stage)]) + CLEANUP_PS), b"").await;
    }
    eprint!("{}", result.context("install on Windows")?);
    open(target, ssh).await
}

async fn open(target: &Target, ssh: &Ssh) -> Result<Client> {
    let mut conn = target.ssh();
    conn.platform = Some("windows".into());
    connect::open(&conn).await.with_context(|| format!("reach the imesh daemon on {}", ssh.dest()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prelude_has_no_quotes_from_data() {
        let p = prelude(&[("exe", "C:\\x'; Remove-Item C:\\ -Recurse #")]);
        assert_eq!(p.matches('\'').count(), 2);
    }
}
