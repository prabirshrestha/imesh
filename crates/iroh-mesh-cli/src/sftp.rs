//! The `sftp` subsystem of `imesh/ssh/0`: files of the daemon's user, for `scp` and `sftp`.
//! Relative paths start at the home directory, as with OpenSSH.

use std::{collections::HashMap, path::PathBuf};

use russh_sftp::protocol::{Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode, Version};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
};

enum Open {
    /// The file and its path, so attributes can be set on it later.
    File(fs::File, PathBuf),
    /// A directory listing, sent in parts, then EOF.
    Dir(Vec<File>),
}

#[derive(Default)]
pub struct Sftp {
    handles: HashMap<String, Open>,
    next: u64,
}

fn ok(id: u32) -> Status {
    Status { id, status_code: StatusCode::Ok, error_message: "Ok".into(), language_tag: "en-US".into() }
}

fn code(e: std::io::Error) -> StatusCode {
    match e.kind() {
        std::io::ErrorKind::NotFound => StatusCode::NoSuchFile,
        std::io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
        _ => StatusCode::Failure,
    }
}

/// SFTP paths use `/`. On Windows `/C:/x` and `C:/x` both name `C:\x`.
fn path(p: &str) -> PathBuf {
    let home = || dirs::home_dir().unwrap_or_default();
    let p = if p.is_empty() { "." } else { p };
    if cfg!(windows) {
        let b = p.as_bytes();
        // `/C:` and `/C:/x` name drive C. `/` alone has no drive; it means the home directory.
        if b.len() >= 3 && b[0] == b'/' && b[2] == b':' {
            let rest = &p[3..];
            return PathBuf::from(format!("{}:\\{}", &p[1..2], rest.trim_start_matches(['/', '\\'])));
        }
        if p == "/" {
            return home();
        }
    }
    let p = PathBuf::from(p);
    if p.is_absolute() { p } else { home().join(p) }
}

/// A path as SFTP shows it: with `/`, and `/C:/...` on Windows.
fn shown(p: &std::path::Path) -> String {
    let s = p.display().to_string();
    if cfg!(windows) { format!("/{}", s.replace('\\', "/")) } else { s }
}

impl Sftp {
    fn add(&mut self, o: Open) -> String {
        self.next += 1;
        let h = self.next.to_string();
        self.handles.insert(h.clone(), o);
        h
    }

    fn file(&mut self, h: &str) -> Result<&mut fs::File, StatusCode> {
        match self.handles.get_mut(h) {
            Some(Open::File(f, _)) => Ok(f),
            _ => Err(StatusCode::Failure),
        }
    }
}

async fn set_attrs(p: &std::path::Path, a: &FileAttributes) -> std::io::Result<()> {
    if let Some(size) = a.size {
        fs::OpenOptions::new().write(true).open(p).await?.set_len(size).await?;
    }
    #[cfg(unix)]
    if let Some(mode) = a.permissions {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(p, std::fs::Permissions::from_mode(mode & 0o7777)).await?;
    }
    if let (Some(at), Some(mt)) = (a.atime, a.mtime) {
        let t = |s: u32| std::time::UNIX_EPOCH + std::time::Duration::from_secs(s.into());
        let f = std::fs::File::options().write(true).open(p).or_else(|_| std::fs::File::open(p))?;
        f.set_times(std::fs::FileTimes::new().set_accessed(t(at)).set_modified(t(mt)))?;
    }
    Ok(())
}

impl russh_sftp::server::Handler for Sftp {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn init(&mut self, _v: u32, _ext: HashMap<String, String>) -> Result<Version, Self::Error> {
        Ok(Version::new())
    }

    async fn open(
        &mut self,
        id: u32,
        name: String,
        flags: OpenFlags,
        attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let p = path(&name);
        let mut o = fs::OpenOptions::new();
        o.read(flags.contains(OpenFlags::READ))
            .write(flags.contains(OpenFlags::WRITE) || flags.contains(OpenFlags::APPEND))
            .append(flags.contains(OpenFlags::APPEND))
            .truncate(flags.contains(OpenFlags::TRUNCATE));
        if flags.contains(OpenFlags::CREATE) {
            if flags.contains(OpenFlags::EXCLUDE) {
                o.create_new(true);
            } else {
                o.create(true);
            }
        }
        #[cfg(unix)]
        if let Some(mode) = attrs.permissions {
            o.mode(mode & 0o7777);
        }
        let _ = &attrs;
        let f = o.open(&p).await.map_err(code)?;
        Ok(Handle { id, handle: self.add(Open::File(f, p)) })
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        if let Some(Open::File(mut f, _)) = self.handles.remove(&handle) {
            f.flush().await.map_err(code)?;
        }
        Ok(ok(id))
    }

    async fn read(&mut self, id: u32, handle: String, offset: u64, len: u32) -> Result<Data, Self::Error> {
        let f = self.file(&handle)?;
        f.seek(std::io::SeekFrom::Start(offset)).await.map_err(code)?;
        let mut buf = vec![0u8; len.min(256 * 1024) as usize];
        let n = f.read(&mut buf).await.map_err(code)?;
        if n == 0 {
            return Err(StatusCode::Eof);
        }
        buf.truncate(n);
        Ok(Data { id, data: buf })
    }

    async fn write(&mut self, id: u32, handle: String, offset: u64, data: Vec<u8>) -> Result<Status, Self::Error> {
        let f = self.file(&handle)?;
        f.seek(std::io::SeekFrom::Start(offset)).await.map_err(code)?;
        f.write_all(&data).await.map_err(code)?;
        Ok(ok(id))
    }

    async fn lstat(&mut self, id: u32, p: String) -> Result<Attrs, Self::Error> {
        let m = fs::symlink_metadata(path(&p)).await.map_err(code)?;
        Ok(Attrs { id, attrs: (&m).into() })
    }

    async fn stat(&mut self, id: u32, p: String) -> Result<Attrs, Self::Error> {
        let m = fs::metadata(path(&p)).await.map_err(code)?;
        Ok(Attrs { id, attrs: (&m).into() })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        let m = self.file(&handle)?.metadata().await.map_err(code)?;
        Ok(Attrs { id, attrs: (&m).into() })
    }

    async fn setstat(&mut self, id: u32, p: String, attrs: FileAttributes) -> Result<Status, Self::Error> {
        set_attrs(&path(&p), &attrs).await.map_err(code)?;
        Ok(ok(id))
    }

    async fn fsetstat(&mut self, id: u32, handle: String, attrs: FileAttributes) -> Result<Status, Self::Error> {
        let path = match self.handles.get(&handle) {
            Some(Open::File(_, p)) => p.clone(),
            _ => return Err(StatusCode::Failure),
        };
        self.file(&handle)?.flush().await.map_err(code)?;
        set_attrs(&path, &attrs).await.map_err(code)?;
        Ok(ok(id))
    }

    async fn opendir(&mut self, id: u32, p: String) -> Result<Handle, Self::Error> {
        let mut rd = fs::read_dir(path(&p)).await.map_err(code)?;
        let mut files = Vec::new();
        while let Some(e) = rd.next_entry().await.map_err(code)? {
            if let Ok(m) = e.metadata().await {
                files.push(File::new(e.file_name().to_string_lossy(), (&m).into()));
            }
        }
        files.reverse();
        Ok(Handle { id, handle: self.add(Open::Dir(files)) })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        match self.handles.get_mut(&handle) {
            // At most 100 names per reply, as OpenSSH's server sends, so a reply stays small.
            Some(Open::Dir(files)) if !files.is_empty() => {
                let part = files.split_off(files.len().saturating_sub(100));
                Ok(Name { id, files: part.into_iter().rev().collect() })
            }
            Some(Open::Dir(_)) => Err(StatusCode::Eof),
            _ => Err(StatusCode::Failure),
        }
    }

    async fn remove(&mut self, id: u32, p: String) -> Result<Status, Self::Error> {
        fs::remove_file(path(&p)).await.map_err(code)?;
        Ok(ok(id))
    }

    async fn mkdir(&mut self, id: u32, p: String, _attrs: FileAttributes) -> Result<Status, Self::Error> {
        fs::create_dir(path(&p)).await.map_err(code)?;
        Ok(ok(id))
    }

    async fn rmdir(&mut self, id: u32, p: String) -> Result<Status, Self::Error> {
        fs::remove_dir(path(&p)).await.map_err(code)?;
        Ok(ok(id))
    }

    async fn realpath(&mut self, id: u32, p: String) -> Result<Name, Self::Error> {
        let full = path(&p);
        let full = fs::canonicalize(&full).await.unwrap_or(full);
        #[cfg(windows)]
        let full = PathBuf::from(full.display().to_string().trim_start_matches(r"\\?\"));
        Ok(Name { id, files: vec![File::dummy(shown(&full))] })
    }

    async fn rename(&mut self, id: u32, from: String, to: String) -> Result<Status, Self::Error> {
        fs::rename(path(&from), path(&to)).await.map_err(code)?;
        Ok(ok(id))
    }

    async fn readlink(&mut self, id: u32, p: String) -> Result<Name, Self::Error> {
        let t = fs::read_link(path(&p)).await.map_err(code)?;
        Ok(Name { id, files: vec![File::dummy(t.display().to_string())] })
    }

    async fn symlink(&mut self, id: u32, link: String, target: String) -> Result<Status, Self::Error> {
        #[cfg(unix)]
        {
            fs::symlink(&target, path(&link)).await.map_err(code)?;
            Ok(ok(id))
        }
        #[cfg(windows)]
        {
            let _ = (id, link, target);
            Err(StatusCode::OpUnsupported)
        }
    }
}
