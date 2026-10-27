//! Project tasks: `cargo x <task>`. Only std, so it builds before anything else.

use std::{
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

const WINDOWS: &str = "x86_64-pc-windows-gnu";

const HELP: &str = "\
cargo x <task>

  tools            install build tools, prepare the checkout, and check prerequisites
  install          build every target, make the npm packages, and install them with
                   `npm install -g`, so `imesh` is on PATH. Starts no daemon
  setup            once after clone: check out Corrosion and apply vendor/corrosion.patch
  test             unit tests of both crates
  lint             rustfmt check, and clippy with warnings as errors
  build [--target <triple>]... [--example app]
                   a bundle in dist/: every target (or the given ones), a manifest, and imesh.zip
  windows          clippy for Windows, and the Windows build of the imesh command and the example app
  example          two machines in one process, with the library only (Linux or Windows)
  e2e [VAR=v]...   the whole cluster in Docker; for example `cargo x e2e KEEP=1`
  e2e-mesh [VAR=v]...
                   three machines in the devtunnel mesh, with a stand-in devtunnel CLI
  e2e-windows [VAR=v]...
                   a Windows machine joins a Linux cluster (needs the Windows VM)
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().expect("xtask is in the repository").to_path_buf();
    let task = args.first().map(String::as_str).unwrap_or("help");
    let rest = args.get(1..).unwrap_or_default();
    let result = match task {
        "setup" => setup(&root),
        "tools" => install_prerequisites(&root),
        "install" => build(&root, &[]).and_then(|()| npm_install(&root)),
        "test" => cargo(&root, &["test", "--workspace"]),
        "lint" => cargo(&root, &["fmt", "--all", "--check"])
            .and_then(|()| cargo(&root, &["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"])),
        "windows" => windows_cargo(
            &root,
            &["clippy", "--workspace", "--all-targets", "--target", WINDOWS, "--", "-D", "warnings"],
        )
        .and_then(|()| {
            windows_cargo(&root, &["build", "--release", "-p", "iroh-mesh-cli", "--target", WINDOWS, "--bins", "--example", "app"])
        }),
        "build" => build(&root, rest),
        "example" => cargo(&root, &["run", "-p", "iroh-mesh", "--example", "two_machines"]),
        "e2e" => script(&root, "tests/e2e/linux.sh", rest),
        "e2e-mesh" => script(&root, "tests/e2e/mesh.sh", rest),
        "e2e-windows" => script(&root, "tests/e2e/windows.sh", rest),
        "help" | "-h" | "--help" => {
            print!("{HELP}");
            Ok(())
        }
        other => Err(format!("unknown task {other:?}\n\n{HELP}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cmd: &mut Command) -> Result<(), String> {
    let status = cmd.status().map_err(|e| format!("run {cmd:?}: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("{cmd:?} failed: {status}")) }
}

fn cargo(root: &Path, args: &[&str]) -> Result<(), String> {
    run(Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into())).current_dir(root).args(args))
}

/// Cargo for the Windows target, with the MinGW C compiler even when CC is set globally.
fn windows_cargo(root: &Path, args: &[&str]) -> Result<(), String> {
    run(Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .current_dir(root)
        .args(args)
        .env("CC_x86_64_pc_windows_gnu", "x86_64-w64-mingw32-gcc")
        .env("AR_x86_64_pc_windows_gnu", "x86_64-w64-mingw32-ar"))
}

/// Runs an e2e script with `VAR=value` arguments as its environment.
fn script(root: &Path, path: &str, vars: &[String]) -> Result<(), String> {
    let mut cmd = Command::new("bash");
    cmd.current_dir(root).arg(path);
    for v in vars {
        let (k, val) = v.split_once('=').ok_or_else(|| format!("expected VAR=value, got {v:?}"))?;
        cmd.env(k, val);
    }
    run(&mut cmd)
}

/// Checks out Corrosion at the pinned commit and applies imesh's patch. Safe to run twice.
fn setup(root: &Path) -> Result<(), String> {
    run(Command::new("git").current_dir(root).args(["submodule", "update", "--init", "vendor/corrosion"]))?;
    let corrosion = root.join("vendor/corrosion");
    let patch = std::fs::read_to_string(root.join("vendor/corrosion.patch")).map_err(|e| e.to_string())?;
    for part in patch.split("diff --git ").filter(|p| !p.trim().is_empty()) {
        let patch = format!("diff --git {part}");
        let check = |reverse: bool| -> Result<bool, String> {
            use std::io::Write;
            let mut cmd = Command::new("git");
            cmd.current_dir(&corrosion).args(["apply", "--check"]);
            if reverse { cmd.arg("--reverse"); }
            let mut child = cmd.stdin(std::process::Stdio::piped()).stderr(std::process::Stdio::null())
                .spawn().map_err(|e| e.to_string())?;
            child.stdin.take().expect("piped").write_all(patch.as_bytes()).map_err(|e| e.to_string())?;
            child.wait().map(|s| s.success()).map_err(|e| e.to_string())
        };
        if check(true)? { continue; }
        if !check(false)? { return Err(format!("vendor patch conflicts with local edits: {}", part.lines().next().unwrap_or_default())); }
        use std::io::Write;
        let mut child = Command::new("git").current_dir(&corrosion).arg("apply")
            .stdin(std::process::Stdio::piped()).spawn().map_err(|e| e.to_string())?;
        child.stdin.take().expect("piped").write_all(patch.as_bytes()).map_err(|e| e.to_string())?;
        if !child.wait().map_err(|e| e.to_string())?.success() { return Err("apply vendor patch failed".into()); }
    }
    println!("Corrosion patches are applied");
    Ok(())
}

/// The targets of a full bundle. The host's own triple is added when it is not one of them.
const TARGETS: &[&str] =
    &["x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl", "x86_64-pc-windows-gnu", "aarch64-apple-darwin"];

fn exe(triple: &str) -> &'static str {
    if triple.contains("windows") { "imesh.exe" } else { "imesh" }
}

fn host_triple() -> Result<String, String> {
    let out = Command::new("rustc").arg("-vV").output().map_err(|e| format!("run rustc: {e}"))?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("host: ").map(String::from))
        .ok_or_else(|| "rustc did not print its host".into())
}

fn has(cmd: &str, arg: &str) -> bool {
    Command::new(cmd).arg(arg).output().is_ok_and(|o| o.status.success())
}

/// `cargo x build`: builds imesh for every target from one source revision, then writes
/// `dist/imesh/` (the host executable, manifest.json, targets/<triple>/) and `dist/imesh.zip`.
/// `--example app` bundles the example app instead, for the e2e tests.
fn build(root: &Path, args: &[String]) -> Result<(), String> {
    let mut targets: Vec<String> = Vec::new();
    let mut example = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--target" => targets.push(it.next().ok_or("--target needs a triple")?.clone()),
            "--example" => example = Some(it.next().ok_or("--example needs a name")?.clone()),
            other => return Err(format!("unknown build option {other:?}")),
        }
    }
    let host = host_triple()?;
    let full = targets.is_empty();
    if full {
        targets = TARGETS.iter().map(|t| t.to_string()).collect();
        if !targets.contains(&host) {
            targets.push(host.clone());
        }
    }
    // Check every prerequisite first and report them together.
    let installed = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let mut missing = Vec::new();
    for t in &targets {
        if !installed.lines().any(|l| l.trim() == t) {
            missing.push(format!("rust target {t}: rustup target add {t}"));
        }
        if t.contains("linux-musl") && !(has("cargo-zigbuild", "--version") && has("zig", "version")) {
            missing.push(format!("{t} needs cargo-zigbuild and zig"));
        }
        if t.contains("windows-gnu") && !has("x86_64-w64-mingw32-gcc", "--version") {
            missing.push(format!("{t} needs MinGW (x86_64-w64-mingw32-gcc)"));
        }
        if t.contains("apple-darwin") && *t != host && !host.contains("apple-darwin") {
            missing.push(format!("{t} needs a Mac build host or an Apple SDK; build on a Mac"));
        }
    }
    missing.dedup();
    if !missing.is_empty() {
        return Err(format!("missing build prerequisites:\n  {}", missing.join("\n  ")));
    }
    let revision = Command::new("git")
        .current_dir(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let dirty = !Command::new("git")
        .current_dir(root)
        .args(["diff", "--quiet", "HEAD", "--", "crates", "Cargo.lock"])
        .status()
        .is_ok_and(|s| s.success());
    let revision = if dirty { format!("{revision}-dirty") } else { revision };
    let what: Vec<&str> = match &example {
        Some(e) => vec!["-p", "iroh-mesh-cli", "--example", e],
        None => vec!["-p", "iroh-mesh-cli", "--bin", "imesh"],
    };
    let out_dir = |t: &str| {
        let base = root.join("target").join(t).join("release");
        match &example {
            Some(e) => base.join("examples").join(if t.contains("windows") { format!("{e}.exe") } else { e.clone() }),
            None => base.join(exe(t)),
        }
    };
    let dist = root.join("dist");
    let stage = dist.join("imesh.new");
    let _ = std::fs::remove_dir_all(&stage);
    for t in &targets {
        println!("building {t}");
        let mut args = vec!["--release", "--target", t.as_str()];
        args.extend(&what);
        let sub = if t.contains("linux-musl") { "zigbuild" } else { "build" };
        let mut all = vec![sub];
        all.extend(args);
        let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
        cmd.current_dir(root).args(&all);
        if t.contains("windows-gnu") {
            // A global CC (for example a compiler cache) would compile Windows C code for the host.
            let mingw = t.replace("-pc-windows-gnu", "-w64-mingw32");
            cmd.env(format!("CC_{}", t.replace('-', "_")), format!("{mingw}-gcc"));
            cmd.env(format!("AR_{}", t.replace('-', "_")), format!("{mingw}-ar"));
        }
        if t.starts_with("aarch64") && t.contains("linux-musl") {
            // Rust passes --fix-cortex-a53-843419 for aarch64 Linux, which zig's linker rejects.
            // Link through cargo-zigbuild's cc with that one option removed.
            let wrapper = zig_linker(root, t)?;
            cmd.env(format!("CARGO_TARGET_{}_LINKER", t.to_uppercase().replace('-', "_")), wrapper);
        }
        run(&mut cmd)?;
        let dest = stage.join("targets").join(t);
        std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
        std::fs::copy(out_dir(t), dest.join(exe(t))).map_err(|e| format!("copy {t}: {e}"))?;
    }
    // The native CLI writes the manifest and ZIP, so build and bundle use the same code.
    if !targets.contains(&host) {
        let mut native_args = vec!["build", "--release", "--target", host.as_str()];
        native_args.extend(&what);
        cargo(root, &native_args)?;
    }
    let native = out_dir(&host);
    let zip = dist.join("imesh.zip.new");
    run(Command::new(&native)
        .args(["bundle-pack"])
        .arg(&stage)
        .arg(&zip)
        .args(["--revision", &revision])
        .stdout(std::process::Stdio::null()))?;
    std::fs::copy(&native, stage.join(exe(&host))).map_err(|e| e.to_string())?;
    let final_dir = dist.join("imesh");
    let _ = std::fs::remove_dir_all(&final_dir);
    std::fs::rename(&stage, &final_dir).map_err(|e| e.to_string())?;
    std::fs::rename(&zip, dist.join("imesh.zip")).map_err(|e| e.to_string())?;
    println!(
        "{} bundle of {} target(s) in {} and {}",
        if full { "full" } else { "partial" },
        targets.len(),
        final_dir.display(),
        dist.join("imesh.zip").display()
    );
    Ok(())
}

/// `cargo x install`: makes the npm packages from dist/ in target/npm, then installs this
/// computer's package, the bundle, and the main package with `npm install -g`, as a user of the
/// published package gets them. Starts no daemon and changes no settings.
fn npm_install(root: &Path) -> Result<(), String> {
    let host = host_triple()?;
    let platform = match host.as_str() {
        "aarch64-apple-darwin" => "darwin-arm64",
        "x86_64-unknown-linux-musl" | "x86_64-unknown-linux-gnu" => "linux-x64",
        "aarch64-unknown-linux-musl" | "aarch64-unknown-linux-gnu" => "linux-arm64",
        "x86_64-pc-windows-gnu" | "x86_64-pc-windows-msvc" => "windows-x64",
        other => return Err(format!("no npm package for {other}")),
    };
    let toml = std::fs::read_to_string(root.join("crates/iroh-mesh-cli/Cargo.toml")).map_err(|e| e.to_string())?;
    let version = toml
        .lines()
        .find_map(|l| l.strip_prefix("version = \"").and_then(|v| v.strip_suffix('"')))
        .ok_or("no version in crates/iroh-mesh-cli/Cargo.toml")?;
    let out = root.join("target").join("npm");
    let _ = std::fs::remove_dir_all(&out);
    let npm = if cfg!(windows) { "npm.cmd" } else { "npm" };
    run(Command::new("node").arg(root.join("npm/scripts/pack.mjs")).arg(version).arg(root.join("dist")).arg(&out))?;
    let mut tgz = Vec::new();
    for name in [format!("iroh-mesh-{platform}"), "iroh-mesh-bundle".into(), "iroh-mesh".into()] {
        run(Command::new(npm).current_dir(&out).args(["pack", "--silent"]).arg(out.join(&name)).stdout(std::process::Stdio::null()))?;
        tgz.push(out.join(format!("{name}-{version}.tgz")));
    }
    run(Command::new(npm).args(["install", "-g", "--no-fund", "--no-audit"]).args(&tgz))?;
    println!("installed imesh {version} with npm. No daemon was started; run `imesh up` to start one.");
    Ok(())
}

/// A linker script for `triple` that runs `cargo-zigbuild zig cc` without the linker options
/// zig rejects. Written to target/, so builds need no global setup.
fn zig_linker(root: &Path, triple: &str) -> Result<PathBuf, String> {
    let zt = triple.replace("-unknown", "");
    let path = root.join("target").join(format!("zigcc-{triple}.sh"));
    let script = format!(
        "#!/bin/sh\n# Written by cargo x build.\nn=$#\nwhile [ $n -gt 0 ]; do a=$1; shift; n=$((n-1)); case $a in -Wl,--fix-cortex-a53-843419) ;; *) set -- \"$@\" \"$a\" ;; esac; done\nexec cargo-zigbuild zig cc -- -target {zt} -g \"$@\"\n"
    );
    std::fs::create_dir_all(path.parent().expect("has a parent")).map_err(|e| e.to_string())?;
    std::fs::write(&path, script).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    Ok(path)
}

fn install_prerequisites(root: &Path) -> Result<(), String> {
    setup(root)?;
    let mut targets = vec!["target", "add"];
    targets.extend(TARGETS.iter().copied());
    run(Command::new("rustup").args(targets))?;
    run(Command::new("rustup").args(["toolchain", "install", "nightly-2023-10-05", "--profile", "minimal"]))?;
    if !has("cargo-zigbuild", "--version") {
        cargo(root, &["install", "cargo-zigbuild", "--locked"])?;
    }
    let host = host_triple()?;
    if host.contains("apple-darwin") {
        if !has("brew", "--version") { return Err("install Homebrew from https://brew.sh, then run cargo x tools".into()); }
        run(Command::new("brew").args(["install", "zig", "mingw-w64", "llvm@20"]))?;
    } else if host.contains("linux") {
        return Err("install Zig, MinGW-w64, LLVM 20 (libclang and llvm-objcopy), and an Apple SDK/build host; Rust tools and checkout are ready".into());
    } else {
        return Err("full bundles need a macOS build host with an Apple SDK; Rust tools and checkout are ready".into());
    }
    for (cmd, arg) in [("zig", "version"), ("cargo-zigbuild", "--version"), ("x86_64-w64-mingw32-gcc", "--version")] {
        if !has(cmd, arg) { return Err(format!("required tool is missing: {cmd}")); }
    }
    println!("Build tools and checkout are ready. Run cargo x build.");
    Ok(())
}
