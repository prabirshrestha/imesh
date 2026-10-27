//! Project tasks: `cargo x <task>`. Only std, so it builds before anything else.

use std::{
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

const WINDOWS: &str = "x86_64-pc-windows-gnu";

const HELP: &str = "\
cargo x <task>

  setup            once after clone: check out Corrosion and apply vendor/corrosion.patch
  test             unit tests of both crates
  lint             rustfmt check, and clippy with warnings as errors
  windows          clippy for Windows, and the Windows build of the imesh command and the example app
  example          two machines in one process, with the library only (Linux or Windows)
  e2e [VAR=v]...   the whole cluster in Docker; for example `cargo x e2e KEEP=1`
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
        "test" => cargo(&root, &["test", "--workspace"]),
        "lint" => cargo(&root, &["fmt", "--all", "--check"])
            .and_then(|()| cargo(&root, &["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"])),
        "windows" => cargo(
            &root,
            &["clippy", "--workspace", "--all-targets", "--target", WINDOWS, "--", "-D", "warnings"],
        )
        .and_then(|()| {
            cargo(&root, &["build", "--release", "-p", "imesh-cli", "--target", WINDOWS, "--bins", "--example", "app"])
        }),
        "example" => cargo(&root, &["run", "-p", "imesh", "--example", "two_machines"]),
        "e2e" => script(&root, "tests/e2e/linux.sh", rest),
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
    let applied = Command::new("git")
        .current_dir(&corrosion)
        .args(["apply", "--reverse", "--check", "../corrosion.patch"])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if applied {
        println!("corrosion.patch is already applied");
        return Ok(());
    }
    run(Command::new("git").current_dir(&corrosion).args(["apply", "../corrosion.patch"]))?;
    println!("applied corrosion.patch");
    Ok(())
}
