//! On musl and Windows targets, links cr-sqlite statically: a static musl binary cannot dlopen
//! the glibc `crsqlite.so` that Corrosion embeds, and Corrosion has no Windows build.
//! `src/crsqlite.rs` serves it to SQLite instead.

use std::{path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=crsqlite.sh");
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap();
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap();
    if env != "musl" && os != "windows" {
        return;
    }
    let target = std::env::var("TARGET").unwrap();
    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("target/crsqlite").join(&target);
    let lib = dir.join("libimesh_crsqlite.a");
    let stamp = dir.join("build-script");
    let script = include_bytes!("crsqlite.sh");
    if !lib.exists() || std::fs::read(&stamp).ok().as_deref() != Some(script.as_slice()) {
        // The inner build uses another toolchain, so none of this build's cargo variables may leak in.
        let mut cmd = Command::new("bash");
        cmd.env_clear();
        for k in ["HOME", "PATH", "LIBCLANG_PATH", "TMPDIR"] {
            if let Some(v) = std::env::var_os(k) {
                cmd.env(k, v);
            }
        }
        let ok = cmd.arg("crsqlite.sh").arg(&target).arg(&lib).status().expect("run crsqlite.sh").success();
        assert!(ok, "crsqlite.sh failed");
        std::fs::write(&stamp, script).expect("record crsqlite build script");
    }
    // Rerun when the archive is missing or moved, for example after the package directory moves.
    println!("cargo:rerun-if-changed={}", lib.display());
    println!("cargo:rustc-link-search=native={}", dir.display());
    println!("cargo:rustc-link-lib=static=imesh_crsqlite");
}
