//! Bundles: every imesh executable of one build, with a manifest, as a directory and one ZIP.
//!
//! ```text
//! imesh.zip              manifest.json and targets/<triple>/imesh[.exe]
//! imesh/                 the same files, extracted, plus the executable for this computer
//! ```
//!
//! An install sends the whole ZIP, and the destination keeps it, so it can install onward.
//! Executables are selected by target triple from the manifest and checked by SHA-256.

use std::{
    collections::BTreeSet,
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::install::sha256;

/// The manifest format this CLI reads and writes.
pub const FORMAT: u32 = 1;
pub const MANIFEST: &str = "manifest.json";
/// The ZIP's name inside a kept bundle directory.
pub const ZIP: &str = "imesh.zip";
/// The ZIP's SHA-256, beside it, so a probe need not hash it.
pub const ZIP_SHA: &str = "imesh.zip.sha256";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub format_version: u32,
    pub version: String,
    pub revision: String,
    pub targets: Vec<Artifact>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    pub triple: String,
    /// Relative to the bundle directory, with `/`.
    pub path: String,
    pub sha256: String,
}

/// The triple of the executable for a computer, from `uname -s`/`uname -m` or Windows names.
pub fn triple(os: &str, arch: &str) -> Result<&'static str> {
    let os = os.to_ascii_lowercase();
    let arch = arch.to_ascii_lowercase();
    Ok(match (os.as_str(), arch.as_str()) {
        ("linux", "x86_64" | "amd64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64" | "arm64") => "aarch64-unknown-linux-musl",
        // Windows on ARM64 runs the x86-64 build under emulation.
        ("windows", "x86_64" | "amd64" | "aarch64" | "arm64") => "x86_64-pc-windows-gnu",
        ("macos" | "darwin", "aarch64" | "arm64") => "aarch64-apple-darwin",
        _ => bail!("no imesh build for {os}/{arch}"),
    })
}

/// The executable's file name in a bundle for a triple.
pub fn exe_name(triple: &str) -> &'static str {
    if triple.contains("windows") { "imesh.exe" } else { "imesh" }
}

/// A verified bundle on this computer.
#[derive(Debug, Clone)]
pub struct Bundle {
    pub dir: PathBuf,
    pub manifest: Manifest,
    pub zip: PathBuf,
    pub zip_sha256: String,
}

impl Bundle {
    /// `path` (a directory or a ZIP), else the bundle beside this executable, else the one an
    /// install keeps.
    pub fn find(path: Option<&Path>) -> Result<Bundle> {
        if let Some(p) = path {
            return Bundle::open(p);
        }
        let exe = std::env::current_exe().context("find the running executable")?;
        let beside = exe.parent().context("executable has no directory")?;
        for dir in [beside.to_path_buf(), crate::paths::bundle_dir()] {
            if dir.join(MANIFEST).exists() {
                return Bundle::open(&dir);
            }
        }
        bail!(
            "no imesh bundle beside {} or in {}: build one with `cargo x build`, or pass --bundle",
            exe.display(),
            crate::paths::bundle_dir().display()
        )
    }

    pub fn open(path: &Path) -> Result<Bundle> {
        if path.is_file() {
            let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
            let dir = crate::config::config_dir().join("bundle-cache").join(sha256(&bytes));
            let _lock = crate::local_state::lock(&dir.with_extension("lock"))?;
            if !dir.join(MANIFEST).exists() {
                let stage = dir.with_extension(hex::encode(&iroh::SecretKey::generate().to_bytes()[..8]));
                extract(&bytes, &stage)?;
                if dir.exists() {
                    std::fs::remove_dir_all(&dir)?;
                }
                std::fs::rename(&stage, &dir)?;
            }
            std::fs::copy(path, dir.join(ZIP))?;
            return Bundle::open_dir(&dir);
        }
        Bundle::open_dir(path)
    }

    fn open_dir(dir: &Path) -> Result<Bundle> {
        let manifest = read_manifest(dir)?;
        // A build directory has its ZIP beside it (`dist/imesh.zip`); a kept bundle has it inside.
        let zip = [dir.join(ZIP), dir.with_extension("zip")]
            .into_iter()
            .find(|z| z.is_file())
            .with_context(|| format!("no {ZIP} for the bundle in {}", dir.display()))?;
        let bytes = std::fs::read(&zip)?;
        let zip_manifest = manifest_in_zip(&bytes)?;
        ensure!(zip_manifest == manifest, "{} does not hold the manifest of {}", zip.display(), dir.display());
        verify_dir(dir, &manifest)?;
        Ok(Bundle { dir: dir.to_path_buf(), manifest, zip, zip_sha256: sha256(&bytes) })
    }

    pub fn artifact(&self, triple: &str) -> Result<(&Artifact, PathBuf)> {
        let a = self.manifest.targets.iter().find(|a| a.triple == triple).with_context(|| {
            format!("the bundle in {} has no build for {triple}; run a full `cargo x build`", self.dir.display())
        })?;
        Ok((a, self.dir.join(&a.path)))
    }
}

fn read_manifest(dir: &Path) -> Result<Manifest> {
    let path = dir.join(MANIFEST);
    let text = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    parse_manifest(&text)
}

fn parse_manifest(text: &str) -> Result<Manifest> {
    let m: Manifest = serde_json::from_str(text).context("parse the bundle manifest")?;
    ensure!(m.format_version == FORMAT, "bundle manifest format {} is not {FORMAT}", m.format_version);
    let mut seen = BTreeSet::new();
    for a in &m.targets {
        ensure!(seen.insert(a.triple.as_str()), "the manifest lists {} twice", a.triple);
        ensure!(a.path == format!("targets/{}/{}", a.triple, exe_name(&a.triple)), "bad artifact path {:?}", a.path);
        ensure!(safe_name(&a.triple), "bad target triple {:?}", a.triple);
        ensure!(a.sha256.len() == 64 && a.sha256.chars().all(|c| c.is_ascii_hexdigit()), "bad checksum");
    }
    Ok(m)
}

fn safe_name(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.') && s != ".."
}

/// Checks every artifact in `dir` against the manifest.
pub fn verify_dir(dir: &Path, m: &Manifest) -> Result<()> {
    for a in &m.targets {
        let path = dir.join(&a.path);
        let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        ensure!(sha256(&bytes) == a.sha256, "checksum mismatch: {}", path.display());
        check_format(&a.triple, &bytes)?;
    }
    Ok(())
}

/// The ZIP's entries must be exactly the manifest and its artifacts.
fn check_entries(archive: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>, m: &Manifest) -> Result<()> {
    let mut want: BTreeSet<String> = m.targets.iter().map(|a| a.path.clone()).collect();
    want.insert(MANIFEST.into());
    let mut seen = BTreeSet::new();
    for i in 0..archive.len() {
        let f = archive.by_index(i)?;
        let name = f.name().to_string();
        if f.is_dir() {
            ensure!(m.targets.iter().any(|a| a.path.starts_with(&name)) || name == "targets/", "unexpected {name:?}");
            continue;
        }
        ensure!(!f.is_symlink(), "the bundle holds a link: {name:?}");
        ensure!(f.enclosed_name().is_some() && !name.contains('\\'), "unsafe path in the bundle: {name:?}");
        ensure!(want.contains(&name), "the bundle holds a file the manifest does not list: {name:?}");
        ensure!(seen.insert(name.clone()), "the bundle holds {name:?} twice");
    }
    ensure!(seen == want, "the bundle misses files of its manifest");
    Ok(())
}

fn manifest_in_zip(bytes: &[u8]) -> Result<Manifest> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).context("read the bundle ZIP")?;
    let mut text = String::new();
    archive.by_name(MANIFEST).context("the bundle ZIP has no manifest")?.read_to_string(&mut text)?;
    let m = parse_manifest(&text)?;
    check_entries(&mut archive, &m)?;
    for artifact in &m.targets {
        let mut bytes = Vec::new();
        archive.by_name(&artifact.path)?.read_to_end(&mut bytes)?;
        ensure!(sha256(&bytes) == artifact.sha256, "ZIP checksum mismatch for {}", artifact.triple);
        check_format(&artifact.triple, &bytes)?;
    }
    Ok(m)
}

/// Extracts and verifies a bundle ZIP into `dest`, a new private directory.
pub fn extract(bytes: &[u8], dest: &Path) -> Result<Manifest> {
    let m = manifest_in_zip(bytes)?;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    iroh_mesh::os::create_private_dir(dest)?;
    for i in 0..archive.len() {
        let mut f = archive.by_index(i)?;
        if f.is_dir() {
            continue;
        }
        let rel = f.enclosed_name().context("unsafe path")?;
        let out = dest.join(rel);
        std::fs::create_dir_all(out.parent().expect("has a parent"))?;
        let mut data = Vec::new();
        f.read_to_end(&mut data)?;
        write_file(&out, &data, f.name() != MANIFEST)?;
    }
    verify_dir(dest, &m)?;
    Ok(m)
}

fn write_file(path: &Path, data: &[u8], exe: bool) -> Result<()> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(if exe { 0o755 } else { 0o644 });
    }
    #[cfg(not(unix))]
    let _ = exe;
    o.open(path).with_context(|| format!("create {}", path.display()))?.write_all(data)?;
    Ok(())
}

/// `imesh bundle pack`: writes the manifest for the executables in `dir/targets`, and the ZIP.
pub fn pack(dir: &Path, zip_path: &Path, version: &str, revision: &str) -> Result<Manifest> {
    let mut targets = Vec::new();
    let tdir = dir.join("targets");
    let mut triples: Vec<String> = std::fs::read_dir(&tdir)
        .with_context(|| format!("read {}", tdir.display()))?
        .map(|e| Ok(e?.file_name().to_string_lossy().into_owned()))
        .collect::<Result<_>>()?;
    triples.sort();
    for triple in triples {
        ensure!(safe_name(&triple), "bad target directory {triple:?}");
        let path = format!("targets/{triple}/{}", exe_name(&triple));
        let bytes = std::fs::read(dir.join(&path)).with_context(|| format!("read {path}"))?;
        check_format(&triple, &bytes)?;
        targets.push(Artifact { triple, path, sha256: sha256(&bytes) });
    }
    ensure!(!targets.is_empty(), "no executables in {}", tdir.display());
    let m = Manifest { format_version: FORMAT, version: version.into(), revision: revision.into(), targets };
    let text = serde_json::to_string_pretty(&m)? + "\n";
    std::fs::write(dir.join(MANIFEST), &text)?;
    let tmp = zip_path.with_extension("zip.tmp");
    let mut w = zip::ZipWriter::new(std::fs::File::create(&tmp)?);
    let opts = |mode| {
        zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .unix_permissions(mode)
            .large_file(true)
    };
    w.start_file(MANIFEST, opts(0o644))?;
    w.write_all(text.as_bytes())?;
    for a in &m.targets {
        w.start_file(&a.path, opts(0o755))?;
        w.write_all(&std::fs::read(dir.join(&a.path))?)?;
    }
    w.finish()?;
    std::fs::rename(&tmp, zip_path)?;
    Ok(m)
}

/// Checks that an executable has the file format of its triple.
pub fn check_format(triple: &str, bytes: &[u8]) -> Result<()> {
    let elf_machine = |b: &[u8]| b.get(18..20).map(|m| u16::from_le_bytes([m[0], m[1]]));
    let ok = match triple {
        "x86_64-unknown-linux-musl" => bytes.starts_with(b"\x7fELF") && elf_machine(bytes) == Some(62),
        "aarch64-unknown-linux-musl" => bytes.starts_with(b"\x7fELF") && elf_machine(bytes) == Some(183),
        t if t.contains("windows") => {
            bytes.starts_with(b"MZ") && {
                let pe = bytes.get(0x3c..0x40).map(|o| u32::from_le_bytes([o[0], o[1], o[2], o[3]]) as usize);
                let machine = pe.and_then(|o| bytes.get(o + 4..o + 6)).map(|m| u16::from_le_bytes([m[0], m[1]]));
                bytes.get(pe.unwrap_or(0)..pe.unwrap_or(0) + 4) == Some(b"PE\0\0")
                    && machine == Some(if t.starts_with("x86_64") { 0x8664 } else { 0xaa64 })
            }
        }
        // Mach-O 64-bit, CPU type ARM64.
        "aarch64-apple-darwin" => bytes.starts_with(&[0xcf, 0xfa, 0xed, 0xfe, 0x0c, 0x00, 0x00, 0x01]),
        "x86_64-apple-darwin" => bytes.starts_with(&[0xcf, 0xfa, 0xed, 0xfe, 0x07, 0x00, 0x00, 0x01]),
        _ => true,
    };
    ensure!(ok, "the executable for {triple} has the wrong file format");
    Ok(())
}

/// Keeps a verified bundle in `dest` for onward installs: the ZIP and its extracted files. The
/// old bundle stays until the new one is complete.
pub fn keep(zip: &Path, dest: &Path) -> Result<()> {
    let _lock = crate::local_state::lock(&dest.with_extension("lock"))?;
    let bytes = std::fs::read(zip).with_context(|| format!("read {}", zip.display()))?;
    let sha = sha256(&bytes);
    if std::fs::read_to_string(dest.join(ZIP_SHA)).is_ok_and(|s| s.trim() == sha) && Bundle::open_dir(dest).is_ok() {
        return Ok(());
    }
    let parent = dest.parent().context("bundle directory has no parent")?;
    iroh_mesh::os::create_private_dir(parent)?;
    let stage = parent.join(format!(".bundle-new-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage);
    extract(&bytes, &stage)?;
    std::fs::write(stage.join(ZIP), &bytes)?;
    std::fs::write(stage.join(ZIP_SHA), format!("{sha}\n"))?;
    let old = parent.join(format!(".bundle-old-{}", std::process::id()));
    if dest.exists() {
        std::fs::rename(dest, &old)?;
    }
    if let Err(e) = std::fs::rename(&stage, dest) {
        let _ = std::fs::rename(&old, dest);
        return Err(e).context("publish the bundle");
    }
    let _ = std::fs::remove_dir_all(&old);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ELF_ARM: &[u8] = b"\x7fELF\x02\x01\x01\0\0\0\0\0\0\0\0\0\x02\0\xb7\0";

    fn build(dir: &Path) -> (PathBuf, Manifest) {
        let t = dir.join("b/targets/aarch64-unknown-linux-musl");
        std::fs::create_dir_all(&t).unwrap();
        std::fs::write(t.join("imesh"), ELF_ARM).unwrap();
        let zip = dir.join("b.zip");
        let m = pack(&dir.join("b"), &zip, "0.1.0", "abc").unwrap();
        (zip, m)
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("imesh-bundle-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn pack_open_extract_keep() {
        let d = tmp("pack");
        let (zip, m) = build(&d);
        let b = Bundle::open(&d.join("b")).unwrap();
        assert_eq!(b.manifest, m);
        let (a, path) = b.artifact("aarch64-unknown-linux-musl").unwrap();
        assert_eq!(std::fs::read(path).unwrap(), ELF_ARM);
        assert_eq!(a.sha256, sha256(ELF_ARM));
        assert!(b.artifact("x86_64-pc-windows-gnu").is_err());
        assert_eq!(Bundle::open(&zip).unwrap().manifest, m);
        keep(&zip, &d.join("kept")).unwrap();
        keep(&zip, &d.join("kept")).unwrap();
        assert_eq!(Bundle::open(&d.join("kept")).unwrap().manifest, m);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(d.join("kept/targets/aarch64-unknown-linux-musl/imesh")).unwrap();
            assert_eq!(mode.permissions().mode() & 0o777, 0o755);
        }
    }

    #[test]
    fn rejects_bad_bundles() {
        let d = tmp("bad");
        let (zip, m) = build(&d);
        // A changed executable.
        std::fs::write(d.join("b/targets/aarch64-unknown-linux-musl/imesh"), b"\x7fELFx").unwrap();
        assert!(Bundle::open(&d.join("b")).is_err());
        // Extra entries, traversal, and a missing artifact.
        let text = serde_json::to_string(&m).unwrap();
        for extra in ["../evil", "targets/other/imesh", "/abs"] {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            let o = zip::write::SimpleFileOptions::default();
            w.start_file(MANIFEST, o).unwrap();
            w.write_all(text.as_bytes()).unwrap();
            w.start_file(&m.targets[0].path, o).unwrap();
            w.write_all(ELF_ARM).unwrap();
            w.start_file(extra, o).unwrap();
            w.write_all(b"x").unwrap();
            let bytes = w.finish().unwrap().into_inner();
            assert!(extract(&bytes, &d.join(format!("x{}", extra.len()))).is_err(), "{extra}");
        }
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        w.start_file(MANIFEST, zip::write::SimpleFileOptions::default()).unwrap();
        w.write_all(text.as_bytes()).unwrap();
        assert!(extract(&w.finish().unwrap().into_inner(), &d.join("missing")).is_err());
        assert!(std::fs::read(zip).is_ok());
        // Wrong formats.
        assert!(check_format("x86_64-unknown-linux-musl", ELF_ARM).is_err());
        assert!(check_format("x86_64-pc-windows-gnu", ELF_ARM).is_err());
    }

    #[test]
    fn triples() {
        assert_eq!(triple("Linux", "x86_64").unwrap(), "x86_64-unknown-linux-musl");
        assert_eq!(triple("linux", "aarch64").unwrap(), "aarch64-unknown-linux-musl");
        assert_eq!(triple("windows", "ARM64").unwrap(), "x86_64-pc-windows-gnu");
        assert_eq!(triple("Darwin", "arm64").unwrap(), "aarch64-apple-darwin");
        assert!(triple("linux", "riscv64").is_err());
    }
}
